//! The repositories a partition holds and their namespaces (§4.17.1 "The repository and its greens"; D-28; A-515).
//!
//! A repository is the record its tool's clients reach by name; its namespace binds each branch name to the green
//! whose versions are that branch's values. The rules the guard keeps are the design's:
//! - a repository's name is unique on its partition, which a stable hash of the name chooses, so it is unique
//!   everywhere;
//! - a branch binds a green that lives on its repository's partition (the green's id names the repository's
//!   partition), so one register can commit any set of a repository's greens together (§4.17.1 item 2);
//! - a green is bound to at most one name, of one repository;
//! - a name is not empty and holds no NUL byte (each tool narrows that further, as git's `check-ref-format` does,
//!   §4.17.2);
//! - retiring a name leaves its green alone, so binding the name to the green again restores the branch;
//! - destroying a repository drops its namespace and leaves its greens; destroying a green drops its name.
//!
//! The records are the partition's: logged as operations, applied unconditionally once the guard has passed them,
//! carried whole by a partition snapshot and rebuilt from it ([`Repositories::restore`]). Two indexes are derived and
//! rebuilt rather than carried: names to ids, and greens to the names that bind them. The tables are ordered maps, so a
//! repository's branches are read in name order and a name prefix is a range (git's `ls-refs ref-prefix`, §4.17.2).
//!
//! Bounds: repositories by the partition's volume capacity, with a typed `Capacity` refusal at it; branches by the
//! greens, since each binds a distinct green of the partition and the greens are bounded by that same capacity.

use std::collections::BTreeMap;

use crate::catalog::{BranchRecord, RepositoryId, RepositoryRecord, VolumeId};
use crate::error::DbError;
use crate::op::Op;

/// What the partition's catalog holds under a volume id, as the guard needs it (§4.17.1: a namespace binds greens).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
  /// No volume by that id on this partition.
  Absent,
  /// A volume that is not a green.
  NotGreen,
  /// A green.
  Green,
}

/// The repositories of one partition and their namespaces.
#[derive(Debug, Default)]
pub struct Repositories {
  /// The records, by id.
  records: BTreeMap<RepositoryId, RepositoryRecord>,
  /// Derived: each repository's id by its name.
  by_name: BTreeMap<String, RepositoryId>,
  /// Every namespace: (repository, branch name) to the green it binds, so one repository's names are one range.
  branches: BTreeMap<(RepositoryId, String), VolumeId>,
  /// Derived: each bound green to the name that binds it.
  bound: BTreeMap<VolumeId, (RepositoryId, String)>,
}

/// Refuses an empty name or one holding a NUL byte (§4.17.1, A-515).
fn valid_name(name: &str) -> Result<(), DbError> {
  if name.is_empty() || name.contains('\0') {
    return Err(DbError::InvalidName);
  }
  Ok(())
}

impl Repositories {
  /// No repositories.
  pub fn new() -> Repositories {
    Repositories::default()
  }

  /// A repository by id.
  pub fn repository(&self, id: RepositoryId) -> Option<&RepositoryRecord> {
    self.records.get(&id)
  }

  /// A repository by name.
  pub fn repository_named(&self, name: &str) -> Option<&RepositoryRecord> {
    self.by_name.get(name).and_then(|id| self.records.get(id))
  }

  /// Every repository, in id order.
  pub fn repositories(&self) -> impl Iterator<Item = &RepositoryRecord> {
    self.records.values()
  }

  /// How many repositories the partition holds.
  pub fn len(&self) -> usize {
    self.records.len()
  }

  /// Whether the partition holds none.
  pub fn is_empty(&self) -> bool {
    self.records.is_empty()
  }

  /// The green a repository's branch name binds.
  pub fn branch(&self, repository: RepositoryId, name: &str) -> Option<VolumeId> {
    self.branches.get(&(repository, name.to_owned())).copied()
  }

  /// A repository's branches whose names start with `prefix`, in name order, each with its green: a range of the
  /// namespace, so its cost is the names it answers (an empty prefix answers every branch).
  pub fn branches<'a>(
    &'a self,
    repository: RepositoryId,
    prefix: &'a str,
  ) -> impl Iterator<Item = (&'a str, VolumeId)> + 'a {
    self
      .branches
      .range((repository, prefix.to_owned())..)
      .take_while(move |((owner, name), _)| *owner == repository && name.starts_with(prefix))
      .map(|((_, name), green)| (name.as_str(), *green))
  }

  /// Every binding, in (repository, name) order: what a snapshot carries.
  pub fn bindings(&self) -> impl Iterator<Item = BranchRecord> + '_ {
    self
      .branches
      .iter()
      .map(|((repository, name), green)| BranchRecord {
        repository: *repository,
        name: name.clone(),
        green: *green,
      })
  }

  /// The repository and name that bind `green`, if any.
  pub fn binding_of(&self, green: VolumeId) -> Option<(RepositoryId, &str)> {
    self
      .bound
      .get(&green)
      .map(|(repository, name)| (*repository, name.as_str()))
  }

  /// The guard for a repository operation (`Partition::check`): refuses what the log must never record. `capacity`
  /// bounds the repositories; `held` says what the partition's catalog holds under a volume id.
  pub fn check(
    &self,
    op: &Op,
    capacity: usize,
    held: impl Fn(VolumeId) -> Held,
  ) -> Result<(), DbError> {
    match op {
      Op::RepositoryCreated { record } => self.check_created(record, capacity),
      Op::RepositoryBranchAdded { record } => self.check_branch_added(record, held),
      Op::RepositoryBranchRetired { repository, name } => self
        .branch(*repository, name)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::RepositoryDefaultSet { repository, branch } => {
        self.repository(*repository).ok_or(DbError::NotFound)?;
        branch.as_deref().map_or(Ok(()), valid_name)
      }
      Op::RepositoryDestroyed { repository } => self
        .repository(*repository)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      _ => Ok(()),
    }
  }

  /// A repository's name and id are its own; the partition holds no more repositories than its volume capacity.
  fn check_created(&self, record: &RepositoryRecord, capacity: usize) -> Result<(), DbError> {
    valid_name(&record.name)?;
    if let Some(existing) = self.repository_named(&record.name) {
      return Err(DbError::AlreadyExists {
        existing: existing.id.bytes,
      });
    }
    if self.records.contains_key(&record.id) {
      return Err(DbError::AlreadyExists {
        existing: record.id.bytes,
      });
    }
    if let Some(branch) = &record.default_branch {
      valid_name(branch)?;
    }
    if self.records.len() >= capacity {
      return Err(DbError::Capacity {
        table: "repositories",
      });
    }
    Ok(())
  }

  /// A branch binds a green of its repository's partition that no name binds yet, under a name the repository does not
  /// hold yet.
  fn check_branch_added(
    &self,
    record: &BranchRecord,
    held: impl Fn(VolumeId) -> Held,
  ) -> Result<(), DbError> {
    let repository = self
      .repository(record.repository)
      .ok_or(DbError::NotFound)?;
    valid_name(&record.name)?;
    if let Some(existing) = self.branch(record.repository, &record.name) {
      return Err(DbError::AlreadyExists {
        existing: existing.bytes,
      });
    }
    match held(record.green) {
      Held::Absent => return Err(DbError::NotFound),
      Held::NotGreen => return Err(DbError::NotGreen),
      Held::Green => {}
    }
    // A green another partition owns is not this repository's to bind, wherever its record was found: the namespace
    // keeps every green of a repository on the repository's partition (§4.17.1 item 2).
    if record.green.partition() != repository.id.partition() {
      return Err(DbError::NotFound);
    }
    if self.bound.contains_key(&record.green) {
      return Err(DbError::AlreadyExists {
        existing: record.green.bytes,
      });
    }
    Ok(())
  }

  /// Applies a guarded repository operation; unconditional, so replay is deterministic. A record the operation names
  /// that is missing (a corrupted history) is a typed refusal. Other operations are not the repositories' and change
  /// nothing.
  pub fn apply(&mut self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::RepositoryCreated { record } => {
        self.by_name.insert(record.name.clone(), record.id);
        self.records.insert(record.id, record.clone());
        Ok(())
      }
      Op::RepositoryBranchAdded { record } => {
        self.bind(record);
        Ok(())
      }
      Op::RepositoryBranchRetired { repository, name } => {
        let green = self
          .branches
          .remove(&(*repository, name.clone()))
          .ok_or(DbError::NotFound)?;
        self.bound.remove(&green);
        Ok(())
      }
      Op::RepositoryDefaultSet { repository, branch } => {
        let record = self.records.get_mut(repository).ok_or(DbError::NotFound)?;
        record.default_branch = branch.clone();
        Ok(())
      }
      Op::RepositoryDestroyed { repository } => {
        let record = self.records.remove(repository).ok_or(DbError::NotFound)?;
        self.by_name.remove(&record.name);
        let names: Vec<(RepositoryId, String)> = self
          .branches
          .range((*repository, String::new())..)
          .take_while(|((owner, _), _)| owner == repository)
          .map(|(key, _)| key.clone())
          .collect();
        for key in names {
          if let Some(green) = self.branches.remove(&key) {
            self.bound.remove(&green);
          }
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  /// Binds a name to a green in both tables.
  fn bind(&mut self, record: &BranchRecord) {
    self
      .branches
      .insert((record.repository, record.name.clone()), record.green);
    self
      .bound
      .insert(record.green, (record.repository, record.name.clone()));
  }

  /// A destroyed green leaves the namespace that bound it (§4.17.1: deleting a branch's green deletes the branch).
  pub fn green_removed(&mut self, green: VolumeId) {
    if let Some(key) = self.bound.remove(&green) {
      self.branches.remove(&key);
    }
  }

  /// The repositories a snapshot carries, rebuilt with their derived indexes. A binding naming no carried repository,
  /// or a second binding of one green, is a corrupt snapshot, refused rather than half restored.
  pub fn restore(
    records: &[RepositoryRecord],
    bindings: &[BranchRecord],
  ) -> Result<Repositories, DbError> {
    let mut repositories = Repositories::new();
    for record in records {
      repositories.by_name.insert(record.name.clone(), record.id);
      repositories.records.insert(record.id, record.clone());
    }
    for binding in bindings {
      if !repositories.records.contains_key(&binding.repository)
        || repositories.bound.contains_key(&binding.green)
      {
        return Err(DbError::Corrupt {
          seq: 0,
          reason: "a snapshot's repository binding names no repository, or binds a green twice",
        });
      }
      repositories.bind(binding);
    }
    Ok(repositories)
  }
}

#[cfg(test)]
// proptest's strategy types carry `Arc` (D-8's harness exception).
#[allow(clippy::disallowed_types)]
mod tests {
  use std::collections::{BTreeMap, BTreeSet};

  use proptest::prelude::*;

  use super::*;
  use crate::catalog::{
    BaseRecord, NamePolicy, ObjectFormat, PolicyRecord, Principal, Role, SizeClass, SnapshotId,
    VcsTool, VolumeRecord, VolumeState,
  };
  use crate::partition::{Partition, PartitionCaps};

  /// Shape: the partition the tests' repositories live on.
  const HOME: u16 = 3;
  /// Shape: another partition, for a green the namespace must refuse.
  const ELSEWHERE: u16 = 4;

  /// An id on `partition` with `serial` in its low byte, as the server mints them.
  fn id_bytes(partition: u16, serial: u8) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[8..10].copy_from_slice(&partition.to_be_bytes());
    bytes[15] = serial;
    bytes
  }

  fn repository_id(partition: u16, serial: u8) -> RepositoryId {
    RepositoryId {
      bytes: id_bytes(partition, serial),
    }
  }

  fn volume_id(partition: u16, serial: u8) -> VolumeId {
    VolumeId {
      bytes: id_bytes(partition, serial),
    }
  }

  fn owner() -> Principal {
    Principal::Certificate { hash: [7; 32] }
  }

  fn repository(serial: u8, name: &str) -> RepositoryRecord {
    RepositoryRecord {
      id: repository_id(HOME, serial),
      name: name.to_owned(),
      tool: VcsTool::Git,
      format: ObjectFormat::Sha1,
      default_branch: None,
      owner: owner(),
    }
  }

  fn caps() -> PartitionCaps {
    PartitionCaps {
      volumes: 1 << 4,
      snapshots: 1 << 4,
      attachments: 1 << 4,
      segment_slots: 4,
      timers: 1 << 4,
      tick_ns: 1_000,
      green_chain_bytes: 1 << 20,
      audit_records: 1 << 4,
    }
  }

  /// A volume record on its id's partition: a green, or a plain volume.
  fn volume(id: VolumeId, green: bool) -> VolumeRecord {
    VolumeRecord {
      id,
      name: format!("volume-{}", id.bytes[15]),
      owner_shard: id.partition(),
      policy: PolicyRecord {
        size: SizeClass::Dynamic { max: 0 },
        names: NamePolicy::Exact,
        require_locked: false,
        role: if green {
          Role::Green {
            require_evidence: false,
            head_version: 0,
          }
        } else {
          Role::Plain
        },
        live_shipped: false,
        live_holders: None,
        write: None,
        write_version: 0,
        signers: Vec::new(),
      },
      base: BaseRecord::Scratch,
      head: SnapshotId::default(),
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: owner(),
      access: Vec::new(),
      created_ns: 0,
      catalog_version: 0,
      taken_over: None,
    }
  }

  /// Runs `op` as the live path does: the guard, then the apply.
  fn run(partition: &mut Partition, op: &Op) -> Result<(), DbError> {
    partition.check(op, 0)?;
    partition.apply(op)
  }

  /// What a partition answers about repositories, compared whole across a snapshot.
  fn answers(partition: &Partition) -> (Vec<RepositoryRecord>, Vec<BranchRecord>) {
    let repositories = partition.repositories();
    (
      repositories.repositories().cloned().collect(),
      repositories.bindings().collect(),
    )
  }

  /// A-515 (§4.17.1). Do: create a repository, make two greens and a plain volume on its partition and a green on
  /// another, then bind names. Expect: the greens of its partition bind; the plain volume is refused `NotGreen`, the
  /// other partition's green and an unknown id `NotFound`, a taken name and a bound green `AlreadyExists`, an empty
  /// name or one with a NUL `InvalidName`; the names read back in order and by prefix; a partition rebuilt from its
  /// snapshot answers the same.
  #[test]
  fn a_repository_binds_only_unbound_greens_of_its_own_partition() {
    let mut partition = Partition::new(caps(), 0);
    let volumes = greens_a_plain_volume_and_a_foreign_green(&mut partition);
    let [main_green, topic_green, ..] = volumes;
    let record = repository(9, "linux");
    let repo = record.id;
    run(&mut partition, &Op::RepositoryCreated { record }).unwrap();
    run(
      &mut partition,
      &branch_added(repo, "refs/heads/main", main_green),
    )
    .unwrap();
    run(
      &mut partition,
      &branch_added(repo, "refs/heads/topic", topic_green),
    )
    .unwrap();
    assert_each_wrong_binding_refused_by_kind(&mut partition, repo, volumes);
    let names: Vec<(&str, VolumeId)> = partition.repositories().branches(repo, "").collect();
    assert_eq!(
      names,
      vec![
        ("refs/heads/main", main_green),
        ("refs/heads/topic", topic_green)
      ]
    );
    let prefixed: Vec<(&str, VolumeId)> = partition
      .repositories()
      .branches(repo, "refs/heads/t")
      .collect();
    assert_eq!(prefixed, vec![("refs/heads/topic", topic_green)]);
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(answers(&rebuilt), answers(&partition));
  }

  /// A-515's volumes, in order: two greens and a plain volume on the repository's partition (`HOME`), and a green on
  /// another.
  fn greens_a_plain_volume_and_a_foreign_green(partition: &mut Partition) -> [VolumeId; 4] {
    let ids = [
      volume_id(HOME, 1),
      volume_id(HOME, 2),
      volume_id(HOME, 3),
      volume_id(ELSEWHERE, 4),
    ];
    for (id, green) in ids.into_iter().zip([true, true, false, true]) {
      run(
        partition,
        &Op::VolumeCreated {
          record: volume(id, green),
        },
      )
      .unwrap();
    }
    ids
  }

  /// A-515's binding of `name` in `repo` to `green`.
  fn branch_added(repo: RepositoryId, name: &str, green: VolumeId) -> Op {
    Op::RepositoryBranchAdded {
      record: BranchRecord {
        repository: repo,
        name: name.to_owned(),
        green,
      },
    }
  }

  /// A-515's refusals: the plain volume `NotGreen`; the other partition's green and an unknown id `NotFound`; a taken
  /// name and a bound green `AlreadyExists`; an empty name and one with a NUL `InvalidName`.
  fn assert_each_wrong_binding_refused_by_kind(
    partition: &mut Partition,
    repo: RepositoryId,
    [main_green, topic_green, plain, foreign]: [VolumeId; 4],
  ) {
    let taken = DbError::AlreadyExists {
      existing: main_green.bytes,
    };
    for (name, green, refusal) in [
      ("refs/heads/plain", plain, DbError::NotGreen),
      ("refs/heads/foreign", foreign, DbError::NotFound),
      ("refs/heads/ghost", volume_id(HOME, 99), DbError::NotFound),
      ("refs/heads/main", topic_green, taken.clone()),
      ("refs/heads/again", main_green, taken),
      ("", plain, DbError::InvalidName),
      ("refs/heads/a\0b", plain, DbError::InvalidName),
    ] {
      assert_eq!(
        run(partition, &branch_added(repo, name, green)),
        Err(refusal),
        "binding {name:?}"
      );
    }
  }

  /// A-515 (§4.17.1 "Deleting a branch"). Do: bind a branch, destroy its green, and separately retire a branch and
  /// destroy its repository. Expect: the destroyed green's name leaves the namespace, so the name binds again; the
  /// retired name's green stays and binds again; the destroyed repository's names are gone and its greens remain.
  #[test]
  fn a_destroyed_green_leaves_its_name_and_a_destroyed_repository_leaves_its_greens() {
    let mut partition = Partition::new(caps(), 0);
    let first = volume_id(HOME, 1);
    let second = volume_id(HOME, 2);
    for id in [first, second] {
      run(
        &mut partition,
        &Op::VolumeCreated {
          record: volume(id, true),
        },
      )
      .unwrap();
    }
    let record = repository(9, "linux");
    let repo = record.id;
    run(&mut partition, &Op::RepositoryCreated { record }).unwrap();
    let bind = |name: &str, green: VolumeId| Op::RepositoryBranchAdded {
      record: BranchRecord {
        repository: repo,
        name: name.to_owned(),
        green,
      },
    };
    run(&mut partition, &bind("main", first)).unwrap();
    run(&mut partition, &Op::VolumeDestroyed { id: first }).unwrap();
    assert_eq!(partition.repositories().branch(repo, "main"), None);
    assert_eq!(partition.repositories().binding_of(first), None);
    run(&mut partition, &bind("main", second)).unwrap();
    run(
      &mut partition,
      &Op::RepositoryBranchRetired {
        repository: repo,
        name: "main".to_owned(),
      },
    )
    .unwrap();
    assert!(partition.volume(second).is_some());
    run(&mut partition, &bind("main", second)).unwrap();
    run(
      &mut partition,
      &Op::RepositoryDestroyed { repository: repo },
    )
    .unwrap();
    assert!(partition.repositories().repository(repo).is_none());
    assert_eq!(partition.repositories().binding_of(second), None);
    assert!(partition.volume(second).is_some());
    assert_eq!(
      run(
        &mut partition,
        &Op::RepositoryBranchRetired {
          repository: repo,
          name: "main".to_owned(),
        },
      ),
      Err(DbError::NotFound)
    );
  }

  /// One step of a generated history over a repository namespace.
  #[derive(Clone, Debug)]
  enum Step {
    Create { serial: u8, name: u8 },
    Bind { serial: u8, name: u8, green: u8 },
    Retire { serial: u8, name: u8 },
    Default { serial: u8, name: Option<u8> },
    Destroy { serial: u8 },
    DestroyGreen { green: u8 },
  }

  /// Shape: few repositories, names and greens, so histories collide on each rule often.
  const SERIALS: u8 = 3;
  /// Shape: see [`SERIALS`].
  const NAMES: u8 = 4;
  /// Shape: see [`SERIALS`].
  const GREENS: u8 = 5;

  fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
      (0..SERIALS, 0..NAMES).prop_map(|(serial, name)| Step::Create { serial, name }),
      (0..SERIALS, 0..NAMES, 0..GREENS).prop_map(|(serial, name, green)| Step::Bind {
        serial,
        name,
        green
      }),
      (0..SERIALS, 0..NAMES).prop_map(|(serial, name)| Step::Retire { serial, name }),
      (0..SERIALS, proptest::option::of(0..NAMES))
        .prop_map(|(serial, name)| Step::Default { serial, name }),
      (0..SERIALS).prop_map(|serial| Step::Destroy { serial }),
      (0..GREENS).prop_map(|green| Step::DestroyGreen { green }),
    ]
  }

  /// The design's rules (§4.17.1, A-515), stated over plain maps: the serial reference the partition must equal.
  #[derive(Default)]
  struct Model {
    /// Repository serial to (name, default branch).
    repositories: BTreeMap<u8, (String, Option<String>)>,
    /// (repository serial, branch name) to green serial.
    names: BTreeMap<(u8, String), u8>,
    /// The greens alive.
    greens: BTreeSet<u8>,
  }

  /// What the model answers: the refusal's kind, so the comparison is of rules, not of the existing ids named.
  #[derive(Debug, PartialEq, Eq)]
  enum Answer {
    Ok,
    NotFound,
    AlreadyExists,
  }

  fn answer_of(result: &Result<(), DbError>) -> Answer {
    match result {
      Ok(()) => Answer::Ok,
      Err(DbError::NotFound) => Answer::NotFound,
      Err(DbError::AlreadyExists { .. }) => Answer::AlreadyExists,
      Err(other) => panic!("a refusal the model has no rule for: {other:?}"),
    }
  }

  impl Model {
    fn step(&mut self, step: &Step) -> Answer {
      match *step {
        Step::Create { serial, name } => {
          let name = format!("repo-{name}");
          if self.repositories.contains_key(&serial)
            || self.repositories.values().any(|(held, _)| *held == name)
          {
            return Answer::AlreadyExists;
          }
          self.repositories.insert(serial, (name, None));
          Answer::Ok
        }
        Step::Bind {
          serial,
          name,
          green,
        } => {
          let name = format!("branch-{name}");
          if !self.repositories.contains_key(&serial) {
            return Answer::NotFound;
          }
          if self.names.contains_key(&(serial, name.clone())) {
            return Answer::AlreadyExists;
          }
          if !self.greens.contains(&green) {
            return Answer::NotFound;
          }
          if self.names.values().any(|bound| *bound == green) {
            return Answer::AlreadyExists;
          }
          self.names.insert((serial, name), green);
          Answer::Ok
        }
        Step::Retire { serial, name } => {
          match self.names.remove(&(serial, format!("branch-{name}"))) {
            Some(_) => Answer::Ok,
            None => Answer::NotFound,
          }
        }
        Step::Default { serial, name } => match self.repositories.get_mut(&serial) {
          Some((_, default)) => {
            *default = name.map(|name| format!("branch-{name}"));
            Answer::Ok
          }
          None => Answer::NotFound,
        },
        Step::Destroy { serial } => {
          if self.repositories.remove(&serial).is_none() {
            return Answer::NotFound;
          }
          self.names.retain(|(owner, _), _| *owner != serial);
          Answer::Ok
        }
        Step::DestroyGreen { green } => {
          if !self.greens.remove(&green) {
            return Answer::NotFound;
          }
          self.names.retain(|_, bound| *bound != green);
          Answer::Ok
        }
      }
    }
  }

  /// The operation a step is on the real partition.
  fn op_of(step: &Step) -> Op {
    match *step {
      Step::Create { serial, name } => Op::RepositoryCreated {
        record: repository(serial, &format!("repo-{name}")),
      },
      Step::Bind {
        serial,
        name,
        green,
      } => Op::RepositoryBranchAdded {
        record: BranchRecord {
          repository: repository_id(HOME, serial),
          name: format!("branch-{name}"),
          green: volume_id(HOME, green),
        },
      },
      Step::Retire { serial, name } => Op::RepositoryBranchRetired {
        repository: repository_id(HOME, serial),
        name: format!("branch-{name}"),
      },
      Step::Default { serial, name } => Op::RepositoryDefaultSet {
        repository: repository_id(HOME, serial),
        branch: name.map(|name| format!("branch-{name}")),
      },
      Step::Destroy { serial } => Op::RepositoryDestroyed {
        repository: repository_id(HOME, serial),
      },
      Step::DestroyGreen { green } => Op::VolumeDestroyed {
        id: volume_id(HOME, green),
      },
    }
  }

  /// The model's state in the partition's terms, to compare whole.
  fn expected(model: &Model) -> (Vec<RepositoryRecord>, Vec<BranchRecord>) {
    let records = model
      .repositories
      .iter()
      .map(|(serial, (name, default))| {
        let mut record = repository(*serial, name);
        record.default_branch = default.clone();
        record
      })
      .collect();
    let bindings = model
      .names
      .iter()
      .map(|((serial, name), green)| BranchRecord {
        repository: repository_id(HOME, *serial),
        name: name.clone(),
        green: volume_id(HOME, *green),
      })
      .collect();
    (records, bindings)
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(400), ..ProptestConfig::default() }))]

    /// A-515 (§4.17.1). Do: run a generated history of repository creates, binds, retires, default changes, repository
    /// destroys and green destroys on a partition holding a few greens. Expect: after every step the partition's
    /// answer (accepted, or which refusal) and its whole state equal the design's rules kept over plain maps, and a
    /// partition rebuilt from its snapshot answers the same.
    #[test]
    fn the_namespace_equals_the_rules_over_every_history(
      steps in proptest::collection::vec(step(), 0..120),
    ) {
      let mut partition = Partition::new(caps(), 0);
      let mut model = Model::default();
      for green in 0..GREENS {
        run(&mut partition, &Op::VolumeCreated { record: volume(volume_id(HOME, green), true) }).unwrap();
        model.greens.insert(green);
      }
      for step in &steps {
        let real = answer_of(&run(&mut partition, &op_of(step)));
        prop_assert_eq!(real, model.step(step), "step {:?}", step);
        prop_assert_eq!(answers(&partition), expected(&model));
        let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
        prop_assert_eq!(answers(&rebuilt), answers(&partition));
      }
    }
  }
}
