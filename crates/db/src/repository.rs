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
//!
//! **Lineages (A-575).** A repository's greens share the nodes of their maps in one lineage on its partition
//! (`slates_merge::engine::Lineage`), and each green's membership is a record here: the lineage's first green its root,
//! every other made at a version of a member before it. A recovery rebuilds a lineage's members in the order they
//! joined, each the way it joined, so the rules keep that order rebuildable: one root a lineage; a member made from a
//! member of the same lineage that joined before it; a green that live members were made from cannot be destroyed
//! while they live ([`Repositories::branched_from`]); and the oldest version a live member was made at is the floor its
//! source's durable checkpoint may not pass ([`Repositories::branch_floor`]), so that version is still there to branch
//! from. A membership leaves with its green, and outlives its repository's record, since a destroyed repository leaves
//! its greens.
//!
//! **The logs (A-603, rebuilt by A-642).** Each green keeps its own log, its chain of versions, and the repository's own
//! state is kept in logs of its own: the names log (the namespace and the lineage's joins), the phases log, the markers
//! log and the locks log, each at its own position and moved only by its own changes; the policy's version is its own
//! log's position (A-622). Nothing counts every change of a repository, so a push to one green never waits on another's.
//! An `Op::RepositoryAdvanced` entry commits several greens' versions, greens joining, namespace changes and state
//! changes as one step, all or none: each version appended only while its green is at the position the push read,
//! refused `Moved` naming the green otherwise, and its checks read against each log's position first (`entry`). An entry
//! that moves two logs or more, a green's among them, is kept as an atomic record ([`AtomicRecord`]): each position it
//! took on each green's log points to it, a holder applies it only at its join, and a cut that splits it is refused
//! ([`Repositories::split_by`]). Records go once no green's log points to them (its checkpoint, or its destroy). The
//! state is created with its repository, carried whole by a partition snapshot, and goes with its repository.
//!
//! **The policy (A-622).** A repository's policy, its rules by name pattern and its hooks (§4.17.1 "Policy"), is part
//! of its record: empty at creation, set whole by `Op::RepositoryPolicySet` from the version its setter read (`policy`
//! keeps what the log must never record of one), an entry of its register of that one change. Its rules' and hooks'
//! encoded bytes are charged to the partition's merge budget, as the register's state is, and given back when it is
//! replaced or its repository leaves.

use std::collections::{BTreeMap, BTreeSet};

use crate::catalog::{
  AtomicHeld, AtomicRecord, BranchRecord, GreenPosition, LockRecord, LogName, LogSpan,
  MemberOrigin, MemberRecord, NameChange, PhaseRoot, RegisterRecord, RepositoryId,
  RepositoryPolicy, RepositoryRecord, StateChange, StateLog, VolumeId,
};
use crate::error::DbError;
use crate::op::Op;

mod entry;
mod policy;

pub use entry::StateBytes;

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
  /// Every lineage member, by green (A-575).
  members: BTreeMap<VolumeId, MemberRecord>,
  /// Each lineage's members in the order they joined, the order a recovery rebuilds them in (A-575).
  lineages: BTreeMap<RepositoryId, Vec<VolumeId>>,
  /// Each repository's own state, in logs of its own (A-603, A-642).
  registers: BTreeMap<RepositoryId, Register>,
  /// The atomic records kept (A-642), by key: the first log a record moved and the position it took it to; each with how
  /// many positions of greens' logs point to it.
  atomic: BTreeMap<(LogName, u64), (AtomicRecord, u64)>,
  /// Derived: each position an atomic record took a green's log to, to the record's key: each green's log's pointers.
  atomic_at: BTreeMap<(VolumeId, u64), (LogName, u64)>,
}

/// A repository's own state (§4.17.1 shared piece 2; A-603, A-642): its logs of its own, each at its own position,
/// moved only by changes of it. A push that moves none of them passes none of their positions.
#[derive(Debug, Default)]
struct Register {
  /// The names log's position: the namespace's changes and the lineage's joins.
  names: u64,
  /// The phases log's position.
  phases: u64,
  /// The locks log's position.
  locks_position: u64,
  /// The phase roots: each phase's root nodes.
  phase_roots: BTreeMap<u8, BTreeSet<Vec<u8>>>,
  /// The obsolescence markers, in the order the log received them: the markers log's position is their count.
  markers: Vec<Vec<u8>>,
  /// Derived: each held marker's identity (`entry::marker_id`).
  marker_ids: BTreeSet<[u8; 32]>,
  /// The locks, by path.
  locks: BTreeMap<String, Vec<u8>>,
  /// Each entry's objects (A-662), in the order the register received them: what a recovery replays through the
  /// repository's tool store. Not a log of its own, so no entry joins on it (A-642).
  objects: Vec<Vec<u8>>,
}

impl Register {
  /// Whether `node` is a root of `phase`.
  fn holds_root(&self, phase: u8, node: &[u8]) -> bool {
    self
      .phase_roots
      .get(&phase)
      .is_some_and(|nodes| nodes.contains(node))
  }

  /// The lock `path` holds.
  fn lock(&self, path: &str) -> Option<&[u8]> {
    self.locks.get(path).map(Vec::as_slice)
  }

  /// Every state log's position, in log order.
  fn positions(&self) -> [(StateLog, u64); 4] {
    [
      StateLog::Names,
      StateLog::Phases,
      StateLog::Markers,
      StateLog::Locks,
    ]
    .map(|log| (log, self.position(log)))
  }

  /// `log`'s position.
  fn position(&self, log: StateLog) -> u64 {
    match log {
      StateLog::Names => self.names,
      StateLog::Phases => self.phases,
      StateLog::Markers => u64::try_from(self.markers.len()).unwrap_or(u64::MAX),
      StateLog::Locks => self.locks_position,
    }
  }

  /// Applies one state change, unconditionally, as a replay must: a root added that is one, a root removed that is not,
  /// a marker held, or a lock change its path does not allow changes nothing and counts nothing. Each change applied
  /// takes its log's next position. What it added and released.
  fn apply(&mut self, change: &StateChange) -> StateBytes {
    let bytes = match change {
      StateChange::PhaseRootAdded { phase, node } => self.root_added(*phase, node),
      StateChange::PhaseRootRemoved { phase, node } => self.root_removed(*phase, node),
      StateChange::MarkerAdded { marker } => {
        if !self.marker_ids.insert(entry::marker_id(marker)) {
          return StateBytes::default();
        }
        self.markers.push(marker.clone());
        return StateBytes {
          added: marker.len(),
          released: 0,
        };
      }
      StateChange::LockTaken { path, lock } => {
        if self.locks.contains_key(path) {
          return StateBytes::default();
        }
        self.locks.insert(path.clone(), lock.clone());
        Some(StateBytes {
          added: entry::lock_bytes(path, lock),
          released: 0,
        })
      }
      StateChange::LockReleased { path, lock } => {
        if self.lock(path) != Some(lock.as_slice()) {
          return StateBytes::default();
        }
        self.locks.remove(path);
        Some(StateBytes {
          added: 0,
          released: entry::lock_bytes(path, lock),
        })
      }
    };
    let Some(bytes) = bytes else {
      return StateBytes::default();
    };
    match change {
      StateChange::LockTaken { .. } | StateChange::LockReleased { .. } => {
        self.locks_position = self.locks_position.saturating_add(1);
      }
      _ => self.phases = self.phases.saturating_add(1),
    }
    bytes
  }

  /// `node` added as a root of `phase`, unless it is one.
  fn root_added(&mut self, phase: u8, node: &[u8]) -> Option<StateBytes> {
    if !self
      .phase_roots
      .entry(phase)
      .or_default()
      .insert(node.to_vec())
    {
      return None;
    }
    Some(StateBytes {
      added: entry::root_bytes(node),
      released: 0,
    })
  }

  /// `node` removed as a root of `phase`, if it is one.
  fn root_removed(&mut self, phase: u8, node: &[u8]) -> Option<StateBytes> {
    let nodes = self.phase_roots.get_mut(&phase)?;
    let removed = nodes.remove(node);
    if nodes.is_empty() {
      self.phase_roots.remove(&phase);
    }
    removed.then(|| StateBytes {
      added: 0,
      released: entry::root_bytes(node),
    })
  }

  /// The bytes its state holds, as the merge budget charged them.
  fn bytes(&self) -> usize {
    let roots = self
      .phase_roots
      .values()
      .flatten()
      .map(|node| entry::root_bytes(node))
      .fold(0, usize::saturating_add);
    let markers = self
      .markers
      .iter()
      .map(Vec::len)
      .fold(0, usize::saturating_add);
    let locks = self
      .locks
      .iter()
      .map(|(path, lock)| entry::lock_bytes(path, lock))
      .fold(0, usize::saturating_add);
    let objects = self
      .objects
      .iter()
      .map(Vec::len)
      .fold(0, usize::saturating_add);
    roots
      .saturating_add(markers)
      .saturating_add(locks)
      .saturating_add(objects)
  }

  /// The state as a snapshot carries it.
  fn record(&self, repository: RepositoryId) -> RegisterRecord {
    RegisterRecord {
      repository,
      names_position: self.names,
      phases_position: self.phases,
      locks_position: self.locks_position,
      phase_roots: self
        .phase_roots
        .iter()
        .flat_map(|(phase, nodes)| {
          nodes.iter().map(|node| PhaseRoot {
            phase: *phase,
            node: node.clone(),
          })
        })
        .collect(),
      markers: self.markers.clone(),
      locks: self
        .locks
        .iter()
        .map(|(path, lock)| LockRecord {
          path: path.clone(),
          lock: lock.clone(),
        })
        .collect(),
      objects: self.objects.clone(),
    }
  }

  /// The state a snapshot carried. A root, a marker or a lock's path listed twice is a corrupt snapshot, refused rather
  /// than half restored.
  fn restored(record: &RegisterRecord) -> Result<Register, DbError> {
    let corrupt = DbError::Corrupt {
      seq: 0,
      reason: "a snapshot's repository state lists a phase root, a marker or a lock twice",
    };
    let mut register = Register {
      names: record.names_position,
      phases: record.phases_position,
      locks_position: record.locks_position,
      objects: record.objects.clone(),
      ..Register::default()
    };
    for root in &record.phase_roots {
      if !register
        .phase_roots
        .entry(root.phase)
        .or_default()
        .insert(root.node.clone())
      {
        return Err(corrupt);
      }
    }
    for marker in &record.markers {
      if !register.marker_ids.insert(entry::marker_id(marker)) {
        return Err(corrupt);
      }
      register.markers.push(marker.clone());
    }
    for held in &record.locks {
      if register
        .locks
        .insert(held.path.clone(), held.lock.clone())
        .is_some()
      {
        return Err(corrupt);
      }
    }
    Ok(register)
  }
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

  /// `green`'s membership of a lineage, when it is a member (A-575).
  pub fn member(&self, green: VolumeId) -> Option<&MemberRecord> {
    self.members.get(&green)
  }

  /// A lineage's members, in the order they joined: the order a recovery rebuilds them in, each member's source before
  /// it.
  pub fn members_of(&self, repository: RepositoryId) -> impl Iterator<Item = &MemberRecord> + '_ {
    self
      .lineages
      .get(&repository)
      .into_iter()
      .flatten()
      .filter_map(|green| self.members.get(green))
  }

  /// Every lineage that has a member, in id order.
  pub fn lineages(&self) -> impl Iterator<Item = RepositoryId> + '_ {
    self.lineages.keys().copied()
  }

  /// Every membership, lineage by lineage in id order and each lineage's in the order it joined: what a snapshot
  /// carries.
  pub fn memberships(&self) -> impl Iterator<Item = MemberRecord> + '_ {
    self
      .lineages
      .keys()
      .flat_map(|repository| self.members_of(*repository))
      .cloned()
  }

  /// The live members made from `green`: while there are any, `green` cannot be destroyed (A-575).
  pub fn branched_from(&self, green: VolumeId) -> u64 {
    let made = self.branches_of(green).count();
    u64::try_from(made).unwrap_or(u64::MAX)
  }

  /// The oldest version of `green` a live member was made at: the floor `green`'s durable checkpoint may not pass, so
  /// a recovery can branch it at that version again (A-575). `None` when no live member was made from it.
  pub fn branch_floor(&self, green: VolumeId) -> Option<u64> {
    self.branches_of(green).min()
  }

  /// The versions of `green` its lineage's live members were made at, one a member.
  fn branches_of(&self, green: VolumeId) -> impl Iterator<Item = u64> + '_ {
    let repository = self.members.get(&green).map(|member| member.repository);
    repository
      .into_iter()
      .flat_map(move |repository| self.members_of(repository))
      .filter_map(move |member| match member.origin {
        MemberOrigin::Branch { from, version } if from == green => Some(version),
        _ => None,
      })
  }

  /// The position of one of `repository`'s own state logs (A-642); `None` for no such repository.
  pub fn log_position(&self, repository: RepositoryId, log: StateLog) -> Option<u64> {
    self
      .registers
      .get(&repository)
      .map(|register| register.position(log))
  }

  /// `repository`'s locks, each `(path, lock)`, in path order (A-642).
  pub fn locks(&self, repository: RepositoryId) -> impl Iterator<Item = (&str, &[u8])> + '_ {
    self
      .registers
      .get(&repository)
      .into_iter()
      .flat_map(|register| register.locks.iter())
      .map(|(path, lock)| (path.as_str(), lock.as_slice()))
  }

  /// The lock on `path` of `repository`, when one stands (A-629 part 2b-ii: an unlock finds its
  /// lock by the path its id names).
  pub fn lock_on(&self, repository: RepositoryId, path: &str) -> Option<&[u8]> {
    self.registers.get(&repository)?.lock(path)
  }

  /// `repository`'s locks from `from` on, each `(path, lock)`, in path order (A-629 part 2b-ii: a
  /// page of locks goes on at its cursor's path, whether or not a lock still stands there).
  pub fn locks_from(
    &self,
    repository: RepositoryId,
    from: String,
  ) -> impl Iterator<Item = (&str, &[u8])> + '_ {
    let bounds = (std::ops::Bound::Included(from), std::ops::Bound::Unbounded);
    self
      .registers
      .get(&repository)
      .into_iter()
      .flat_map(move |register| register.locks.range::<String, _>(bounds.clone()))
      .map(|(path, lock)| (path.as_str(), lock.as_slice()))
  }

  /// The atomic record version `version` of `green` belongs to, if one does (A-642: each green's log points to it).
  pub fn atomic_at(&self, green: VolumeId, version: u64) -> Option<&AtomicRecord> {
    let key = self.atomic_at.get(&(green, version))?;
    self.atomic.get(key).map(|(record, _)| record)
  }

  /// Every atomic record kept, in key order (A-642).
  pub fn atomic_records(&self) -> impl Iterator<Item = &AtomicRecord> + '_ {
    self.atomic.values().map(|(record, _)| record)
  }

  /// Every atomic record kept with the positions pointing to it still, in key order: what a snapshot carries (A-642),
  /// so a rebuilt partition points exactly where the live one did.
  pub fn atomic_held(&self) -> Vec<AtomicHeld> {
    let mut held: BTreeMap<(LogName, u64), Vec<GreenPosition>> = BTreeMap::new();
    for ((green, version), key) in &self.atomic_at {
      held.entry(*key).or_default().push(GreenPosition {
        green: *green,
        version: *version,
      });
    }
    self
      .atomic
      .iter()
      .map(|(key, (record, _))| AtomicHeld {
        record: record.clone(),
        pointers: held.remove(key).unwrap_or_default(),
      })
      .collect()
  }

  /// The first atomic record kept that `cut` splits (A-642): a cut of logs at positions that holds part of the record
  /// and not the rest, which a replica's read refuses. `None` when the cut is consistent with every record kept.
  pub fn split_by(&self, cut: &[(LogName, u64)]) -> Option<&AtomicRecord> {
    let at = |log: &LogName| {
      cut
        .iter()
        .find(|(named, _)| named == log)
        .map(|(_, position)| *position)
    };
    self
      .atomic
      .values()
      .map(|(record, _)| record)
      .find(|record| record.split_by(at))
  }

  /// `repository`'s phase roots, each `(phase, node)`, in that order (A-603).
  pub fn phase_roots(&self, repository: RepositoryId) -> impl Iterator<Item = (u8, &[u8])> + '_ {
    self
      .registers
      .get(&repository)
      .into_iter()
      .flat_map(|register| register.phase_roots.iter())
      .flat_map(|(phase, nodes)| nodes.iter().map(move |node| (*phase, node.as_slice())))
  }

  /// `repository`'s obsolescence markers, in the order its register received them (A-603).
  pub fn markers(&self, repository: RepositoryId) -> impl Iterator<Item = &[u8]> + '_ {
    self
      .registers
      .get(&repository)
      .into_iter()
      .flat_map(|register| register.markers.iter().map(Vec::as_slice))
  }

  /// `repository`'s entries' objects (A-662), in the order its register received them: what a recovery replays through
  /// its tool store.
  pub fn objects(&self, repository: RepositoryId) -> impl Iterator<Item = &[u8]> + '_ {
    self
      .registers
      .get(&repository)
      .into_iter()
      .flat_map(|register| register.objects.iter().map(Vec::as_slice))
  }

  /// `repository`'s entry objects at `index` in the order its register received them (A-662): what a recovery replays a
  /// record at a time.
  pub fn object_record(&self, repository: RepositoryId, index: usize) -> Option<&[u8]> {
    self
      .registers
      .get(&repository)?
      .objects
      .get(index)
      .map(Vec::as_slice)
  }

  /// Every register, in repository order: what a snapshot carries (A-603).
  pub fn registers(&self) -> impl Iterator<Item = RegisterRecord> + '_ {
    self
      .registers
      .iter()
      .map(|(repository, register)| register.record(*repository))
  }

  /// The bytes the repositories' own state holds, as the partition's merge budget charged them: their registers' state
  /// (A-603) and their policies' rules and hooks (A-622).
  pub fn state_bytes(&self) -> usize {
    let registers = self
      .registers
      .values()
      .map(Register::bytes)
      .fold(0, usize::saturating_add);
    self
      .records
      .values()
      .map(|record| policy::bytes(&record.policy))
      .fold(registers, usize::saturating_add)
  }

  /// What an entry the guard admitted adds to the merge budget and gives back of it: its versions' chain entries and
  /// the state it adds, against the state it removes (A-603).
  pub fn entry_bytes(entry: &crate::catalog::RepositoryEntry) -> StateBytes {
    entry::bytes(entry)
  }

  /// What each green an entry moves goes from and to (A-642): from its head before the entry, `head`, to the last
  /// version the entry names for it, in green order.
  pub fn green_spans(
    entry: &crate::catalog::RepositoryEntry,
    head: impl Fn(VolumeId) -> u64,
  ) -> Vec<LogSpan> {
    let mut spans: BTreeMap<VolumeId, LogSpan> = BTreeMap::new();
    for version in &entry.versions {
      spans
        .entry(version.green)
        .and_modify(|span| span.to = span.to.max(version.version))
        .or_insert(LogSpan {
          log: LogName::Green {
            green: version.green,
          },
          from: head(version.green),
          to: version.version,
        });
    }
    spans.into_values().collect()
  }

  /// The guard for an entry of a repository's register (A-603), `held` and `head` as [`Repositories::check`] takes
  /// them: what a partition judging an entry beside volumes it is about to record asks (A-635).
  pub fn check_entry(
    &self,
    entry: &crate::catalog::RepositoryEntry,
    held: impl Fn(VolumeId) -> Held,
    head: impl Fn(VolumeId) -> u64,
  ) -> Result<(), DbError> {
    entry::check(self, entry, held, head)
  }

  /// The guard for a repository operation (`Partition::check`): refuses what the log must never record. `capacity`
  /// bounds the repositories; `held` says what the partition's catalog holds under a volume id; `head` is a green's
  /// head as its durable records hold it, which an entry's versions follow and its checks read (A-603).
  pub fn check(
    &self,
    op: &Op,
    capacity: usize,
    held: impl Fn(VolumeId) -> Held,
    head: impl Fn(VolumeId) -> u64,
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
      Op::RepositoryGreenJoined { record } => self.check_joined(record, held),
      Op::RepositoryAdvanced { entry } => entry::check(self, entry, held, head),
      Op::RepositoryPolicySet { repository, policy } => self.check_policy(*repository, policy),
      _ => Ok(()),
    }
  }

  /// A green joins the lineage of a repository that exists, on that repository's partition, once; the lineage's first
  /// member is its root and every other was made from a member of the same lineage (A-575).
  fn check_joined(
    &self,
    record: &MemberRecord,
    held: impl Fn(VolumeId) -> Held,
  ) -> Result<(), DbError> {
    let repository = self
      .repository(record.repository)
      .ok_or(DbError::NotFound)?;
    match held(record.green) {
      Held::Absent => return Err(DbError::NotFound),
      Held::NotGreen => return Err(DbError::NotGreen),
      Held::Green => {}
    }
    if record.green.partition() != repository.id.partition() {
      return Err(DbError::NotFound);
    }
    if self.members.contains_key(&record.green) {
      return Err(DbError::AlreadyExists {
        existing: record.green.bytes,
      });
    }
    match record.origin {
      // One root a lineage: every other member is made from one of its members, so the lineage is one arena's.
      MemberOrigin::Root => match self
        .lineages
        .get(&record.repository)
        .and_then(|order| order.first())
      {
        Some(root) => Err(DbError::AlreadyExists {
          existing: root.bytes,
        }),
        None => Ok(()),
      },
      MemberOrigin::Branch { from, .. } => {
        let joined = self.members.get(&from).map(|member| member.repository);
        if joined == Some(record.repository) {
          Ok(())
        } else {
          Err(DbError::NotFound)
        }
      }
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
    // A repository starts with no policy, version 0: only its own setting gives it one (A-622).
    if record.policy != RepositoryPolicy::default() {
      return Err(DbError::PolicyChanged { current: 0 });
    }
    Ok(())
  }

  /// A policy set whole on a repository that exists, as the version after its current one, each rule naming what the
  /// repository's tool has and each hook at one of the tool's points (A-622, `policy`).
  fn check_policy(
    &self,
    repository: RepositoryId,
    policy: &RepositoryPolicy,
  ) -> Result<(), DbError> {
    let record = self.repository(repository).ok_or(DbError::NotFound)?;
    let current = record.policy.version;
    if current.checked_add(1) != Some(policy.version) {
      return Err(DbError::PolicyChanged { current });
    }
    policy::check(record.tool, policy)
  }

  /// What setting `policy` on `repository` adds to the merge budget and gives back of it: its rules' and hooks' bytes,
  /// against those of the policy it replaces (A-622). Nothing for a repository that is not here.
  pub fn policy_bytes(&self, repository: RepositoryId, policy: &RepositoryPolicy) -> StateBytes {
    StateBytes {
      added: policy::bytes(policy),
      released: self
        .repository(repository)
        .map_or(0, |record| policy::bytes(&record.policy)),
    }
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
  /// that is missing (a corrupted history) is a typed refusal, before anything changes. A change of the namespace or the
  /// lineage takes the names log's next position (A-642); an entry is applied by [`Repositories::apply_advanced`], with
  /// its greens' spans. Returns what the repositories' own state gained and released, which the partition charges to
  /// its merge budget (an entry's versions are the partition's chains, charged there). Other operations are not the
  /// repositories' and change nothing.
  pub fn apply(&mut self, op: &Op) -> Result<StateBytes, DbError> {
    match op {
      Op::RepositoryCreated { record } => {
        self.by_name.insert(record.name.clone(), record.id);
        self.records.insert(record.id, record.clone());
        self.registers.insert(record.id, Register::default());
        Ok(StateBytes::default())
      }
      Op::RepositoryBranchAdded { record } => {
        self.bind(record);
        self.names_moved(record.repository, 1);
        Ok(StateBytes::default())
      }
      Op::RepositoryBranchRetired { repository, name } => {
        let green = self
          .branches
          .remove(&(*repository, name.clone()))
          .ok_or(DbError::NotFound)?;
        self.bound.remove(&green);
        self.names_moved(*repository, 1);
        Ok(StateBytes::default())
      }
      Op::RepositoryDefaultSet { repository, branch } => {
        let record = self.records.get_mut(repository).ok_or(DbError::NotFound)?;
        record.default_branch = branch.clone();
        self.names_moved(*repository, 1);
        Ok(StateBytes::default())
      }
      Op::RepositoryGreenJoined { record } => {
        self.join(record);
        self.names_moved(record.repository, 1);
        Ok(StateBytes::default())
      }
      // An entry needs its greens' spans, which only the partition knows: `apply_advanced` takes them.
      Op::RepositoryAdvanced { .. } => Err(DbError::Corrupt {
        seq: 0,
        reason: "an entry is applied with its greens' spans",
      }),
      Op::RepositoryPolicySet { repository, policy } => {
        let record = self.records.get_mut(repository).ok_or(DbError::NotFound)?;
        let bytes = StateBytes {
          added: policy::bytes(policy),
          released: policy::bytes(&record.policy),
        };
        // The policy's own version is its log's position (A-622): nothing else moves.
        record.policy = policy.clone();
        Ok(bytes)
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
        // The register goes with its repository, and gives back what its state held, as its policy does (A-622).
        let released = self
          .registers
          .remove(repository)
          .map_or(0, |register| register.bytes())
          .saturating_add(policy::bytes(&record.policy));
        Ok(StateBytes { added: 0, released })
      }
      _ => Ok(StateBytes::default()),
    }
  }

  /// Applies an entry the guard admitted (A-603, A-642), unconditionally: its members join, its names change and its
  /// state moves, in order, each change taking its own log's next position; its versions are the partition's chains,
  /// which moved each green in `greens` (`Repositories::green_spans`). An entry that moved two logs or more, a green's
  /// among them, is kept as an atomic record, each position it took on each green's log pointing to it. An entry of a
  /// repository this partition does not hold is a corrupted history, refused before anything changes.
  pub fn apply_advanced(
    &mut self,
    entry: &crate::catalog::RepositoryEntry,
    greens: Vec<LogSpan>,
  ) -> Result<StateBytes, DbError> {
    let Some(before) = self
      .registers
      .get(&entry.repository)
      .map(Register::positions)
    else {
      return Err(DbError::NotFound);
    };
    if !self.records.contains_key(&entry.repository) {
      return Err(DbError::NotFound);
    }
    for member in &entry.joined {
      self.join(member);
    }
    for change in &entry.names {
      match change {
        NameChange::Bind { name, green } => self.bind(&BranchRecord {
          repository: entry.repository,
          name: name.clone(),
          green: *green,
        }),
        NameChange::Retire { name } => {
          if let Some(green) = self.branches.remove(&(entry.repository, name.clone())) {
            self.bound.remove(&green);
          }
        }
        NameChange::Default { branch } => {
          if let Some(record) = self.records.get_mut(&entry.repository) {
            record.default_branch = branch.clone();
          }
        }
      }
    }
    let namespace = entry.joined.len().saturating_add(entry.names.len());
    self.names_moved(
      entry.repository,
      u64::try_from(namespace).unwrap_or(u64::MAX),
    );
    let mut bytes = StateBytes::default();
    let mut spans = greens;
    if let Some(register) = self.registers.get_mut(&entry.repository) {
      for change in &entry.state {
        bytes.add(register.apply(change));
      }
      // The objects ride with the entry and take no position (A-662), so they never make it an atomic record.
      if !entry.objects.is_empty() {
        bytes.added = bytes.added.saturating_add(entry.objects.len());
        register.objects.push(entry.objects.clone());
      }
      let after = register.positions();
      for ((log, from), (_, to)) in before.into_iter().zip(after) {
        if to > from {
          spans.push(LogSpan {
            log: LogName::State {
              repository: entry.repository,
              log,
            },
            from,
            to,
          });
        }
      }
    }
    let moves_a_green = spans
      .iter()
      .any(|span| matches!(span.log, LogName::Green { .. }));
    if spans.len() > 1 && moves_a_green {
      self.keep_atomic(AtomicRecord { spans });
    }
    Ok(bytes)
  }

  /// The names log of `repository` takes `changes` positions (A-642). A repository that is gone has no log to move.
  fn names_moved(&mut self, repository: RepositoryId, changes: u64) {
    if let Some(register) = self.registers.get_mut(&repository) {
      register.names = register.names.saturating_add(changes);
    }
  }

  /// Keeps `record`, its spans in log order, and points each position it took on each green's log to it (A-642).
  fn keep_atomic(&mut self, mut record: AtomicRecord) {
    record.spans.sort_by_key(|span| span.log);
    let Some(first) = record.spans.first() else {
      return;
    };
    let key = (first.log, first.to);
    let mut pointers = 0u64;
    for span in &record.spans {
      if let LogName::Green { green } = span.log {
        for position in span.from.saturating_add(1)..=span.to {
          self.atomic_at.insert((green, position), key);
          pointers = pointers.saturating_add(1);
        }
      }
    }
    self.atomic.insert(key, (record, pointers));
  }

  /// An atomic record a snapshot carried, with exactly the pointers it had (A-642). A record listed twice, one no
  /// position points to, or a pointer outside its green's span or pointing twice, is a corrupt snapshot.
  fn restore_atomic(&mut self, held: &AtomicHeld) -> Result<(), DbError> {
    let corrupt = DbError::Corrupt {
      seq: 0,
      reason: "a snapshot's atomic record is listed twice, points nowhere, or is pointed to from outside its spans",
    };
    let first = held.record.spans.first().ok_or_else(|| corrupt.clone())?;
    let key = (first.log, first.to);
    if held.pointers.is_empty() || self.atomic.contains_key(&key) {
      return Err(corrupt);
    }
    for pointer in &held.pointers {
      let within = held.record.spans.iter().any(|span| {
        span.log
          == LogName::Green {
            green: pointer.green,
          }
          && pointer.version > span.from
          && pointer.version <= span.to
      });
      let at = (pointer.green, pointer.version);
      if !within || self.atomic_at.insert(at, key).is_some() {
        return Err(corrupt);
      }
    }
    let pointers = u64::try_from(held.pointers.len()).unwrap_or(u64::MAX);
    self.atomic.insert(key, (held.record.clone(), pointers));
    Ok(())
  }

  /// `green`'s log folded through `floor` (its checkpoint, or everything at its destroy): its pointers there go, and so
  /// does each atomic record no green's log points to any more (A-642).
  pub fn fold_atomic(&mut self, green: VolumeId, floor: u64) {
    let folded: Vec<((VolumeId, u64), (LogName, u64))> = self
      .atomic_at
      .range((green, 0)..=(green, floor))
      .map(|(at, key)| (*at, *key))
      .collect();
    for (at, key) in folded {
      self.atomic_at.remove(&at);
      let gone = self.atomic.get_mut(&key).is_some_and(|(_, pointers)| {
        *pointers = pointers.saturating_sub(1);
        *pointers == 0
      });
      if gone {
        self.atomic.remove(&key);
      }
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

  /// Adds a member at the end of its lineage's order.
  fn join(&mut self, record: &MemberRecord) {
    self
      .lineages
      .entry(record.repository)
      .or_default()
      .push(record.green);
    self.members.insert(record.green, record.clone());
  }

  /// A destroyed green leaves the namespace that bound it (§4.17.1: deleting a branch's green deletes the branch), and
  /// its lineage (A-575): each repository whose names it leaves takes a position on its names log, and its log's
  /// pointers to atomic records go (A-642).
  pub fn green_removed(&mut self, green: VolumeId) {
    self.fold_atomic(green, u64::MAX);
    let mut changed = BTreeSet::new();
    if let Some(key) = self.bound.remove(&green) {
      changed.insert(key.0);
      self.branches.remove(&key);
    }
    if let Some(member) = self.members.remove(&green) {
      changed.insert(member.repository);
      if let Some(order) = self.lineages.get_mut(&member.repository) {
        order.retain(|joined| *joined != green);
        if order.is_empty() {
          self.lineages.remove(&member.repository);
        }
      }
    }
    for repository in changed {
      self.names_moved(repository, 1);
    }
  }

  /// The repositories a snapshot carries, rebuilt with their derived indexes. A binding naming no carried repository,
  /// a second binding of one green, a member joining twice or before the member it was made from, a register naming no
  /// carried repository or a second for one, or a repository without its register (A-603), is a corrupt snapshot,
  /// refused rather than half restored.
  pub fn restore(
    records: &[RepositoryRecord],
    bindings: &[BranchRecord],
    members: &[MemberRecord],
    registers: &[RegisterRecord],
    atomic: &[AtomicHeld],
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
    for member in members {
      let source_joined = match member.origin {
        MemberOrigin::Root => true,
        MemberOrigin::Branch { from, .. } => repositories
          .members
          .get(&from)
          .is_some_and(|source| source.repository == member.repository),
      };
      if repositories.members.contains_key(&member.green) || !source_joined {
        return Err(DbError::Corrupt {
          seq: 0,
          reason: "a snapshot's lineage member joins twice, or before the member it was made from",
        });
      }
      repositories.join(member);
    }
    for register in registers {
      if !repositories.records.contains_key(&register.repository)
        || repositories.registers.contains_key(&register.repository)
      {
        return Err(DbError::Corrupt {
          seq: 0,
          reason: "a snapshot's repository register names no repository, or a second register for one",
        });
      }
      repositories
        .registers
        .insert(register.repository, Register::restored(register)?);
    }
    if repositories.registers.len() != repositories.records.len() {
      return Err(DbError::Corrupt {
        seq: 0,
        reason: "a snapshot's repository carries no register",
      });
    }
    for held in atomic {
      repositories.restore_atomic(held)?;
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
      policy: crate::catalog::RepositoryPolicy::default(),
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

  /// What a partition answers about repositories and their lineages, compared whole across a snapshot.
  type Answers = (
    Vec<RepositoryRecord>,
    Vec<BranchRecord>,
    Vec<MemberRecord>,
    Vec<Option<u64>>,
  );

  /// What a partition answers about repositories, their lineages and each test green's branch floor.
  fn answers(partition: &Partition) -> Answers {
    let repositories = partition.repositories();
    (
      repositories.repositories().cloned().collect(),
      repositories.bindings().collect(),
      repositories.memberships().collect(),
      (0..GREENS)
        .map(|green| repositories.branch_floor(volume_id(HOME, green)))
        .collect(),
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

  /// A green joining `repo`'s lineage as its root, or made from `from` at `version`.
  fn joined(repo: RepositoryId, green: VolumeId, origin: MemberOrigin) -> Op {
    Op::RepositoryGreenJoined {
      record: MemberRecord {
        repository: repo,
        green,
        origin,
      },
    }
  }

  /// A-575 (§4.17.1 item 4). Do: make greens on the repository's partition, a plain volume and a foreign green; join a
  /// root, branch two members from it and one from a branch, and try each wrong join; destroy greens in either order;
  /// rebuild from a snapshot, and from one listing a member before the member it was made from. Expect: a second root
  /// and a member joining twice are `AlreadyExists`; a branch of a non-member or another lineage's member, an unknown
  /// repository and the foreign green `NotFound`; the plain volume `NotGreen`; a green members were made from refuses
  /// its destroy `BranchedFrom` until they go, and its branch floor is the oldest version they were made at; the
  /// snapshot rebuilds the members in the order they joined; the disordered snapshot is refused `Corrupt`.
  #[test]
  fn a_lineage_takes_one_root_and_branches_of_its_own_members() {
    let mut partition = Partition::new(caps(), 0);
    let [root, first, second, deep] = [1, 2, 3, 4].map(|serial| volume_id(HOME, serial));
    let plain = volume_id(HOME, 5);
    let foreign = volume_id(ELSEWHERE, 6);
    for (id, green) in [
      (root, true),
      (first, true),
      (second, true),
      (deep, true),
      (plain, false),
      (foreign, true),
    ] {
      run(
        &mut partition,
        &Op::VolumeCreated {
          record: volume(id, green),
        },
      )
      .unwrap();
    }
    let record = repository(9, "linux");
    let repo = record.id;
    run(&mut partition, &Op::RepositoryCreated { record }).unwrap();
    let branch = |from: VolumeId, version: u64| MemberOrigin::Branch { from, version };
    run(&mut partition, &joined(repo, root, MemberOrigin::Root)).unwrap();
    run(&mut partition, &joined(repo, first, branch(root, 7))).unwrap();
    run(&mut partition, &joined(repo, second, branch(root, 3))).unwrap();
    run(&mut partition, &joined(repo, deep, branch(first, 1))).unwrap();
    let refused = |partition: &mut Partition, op: &Op| run(partition, op).unwrap_err();
    assert!(matches!(
      refused(&mut partition, &joined(repo, plain, MemberOrigin::Root)),
      DbError::NotGreen
    ));
    assert!(matches!(
      refused(&mut partition, &joined(repo, root, branch(first, 0))),
      DbError::AlreadyExists { .. }
    ));
    assert!(matches!(
      refused(
        &mut partition,
        &joined(repository_id(HOME, 8), plain, MemberOrigin::Root)
      ),
      DbError::NotFound
    ));
    assert!(matches!(
      refused(&mut partition, &joined(repo, foreign, MemberOrigin::Root)),
      DbError::NotFound
    ));
    let other = repository(10, "other");
    let other_repo = other.id;
    run(&mut partition, &Op::RepositoryCreated { record: other }).unwrap();
    let spare = volume_id(HOME, 7);
    run(
      &mut partition,
      &Op::VolumeCreated {
        record: volume(spare, true),
      },
    )
    .unwrap();
    assert!(matches!(
      refused(&mut partition, &joined(other_repo, spare, branch(root, 0))),
      DbError::NotFound
    ));
    run(
      &mut partition,
      &joined(other_repo, spare, MemberOrigin::Root),
    )
    .unwrap();
    let late = volume_id(HOME, 11);
    run(
      &mut partition,
      &Op::VolumeCreated {
        record: volume(late, true),
      },
    )
    .unwrap();
    assert!(matches!(
      refused(
        &mut partition,
        &joined(other_repo, late, MemberOrigin::Root)
      ),
      DbError::AlreadyExists { .. }
    ));
    assert_eq!(partition.repositories().branch_floor(root), Some(3));
    assert_eq!(partition.repositories().branch_floor(first), Some(1));
    assert_eq!(partition.repositories().branch_floor(second), None);
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(answers(&rebuilt), answers(&partition));
    let order: Vec<VolumeId> = partition
      .repositories()
      .members_of(repo)
      .map(|member| member.green)
      .collect();
    assert_eq!(order, vec![root, first, second, deep]);
    assert_eq!(
      refused(&mut partition, &Op::VolumeDestroyed { id: root }),
      DbError::BranchedFrom { members: 2 }
    );
    assert_eq!(
      refused(&mut partition, &Op::VolumeDestroyed { id: first }),
      DbError::BranchedFrom { members: 1 }
    );
    run(&mut partition, &Op::VolumeDestroyed { id: deep }).unwrap();
    run(&mut partition, &Op::VolumeDestroyed { id: first }).unwrap();
    assert_eq!(partition.repositories().branch_floor(root), Some(3));
    run(&mut partition, &Op::VolumeDestroyed { id: second }).unwrap();
    assert_eq!(partition.repositories().branch_floor(root), None);
    run(&mut partition, &Op::VolumeDestroyed { id: root }).unwrap();
    assert_eq!(partition.repositories().members_of(repo).count(), 0);
    let mut disordered = partition.to_snapshot(0);
    disordered.repository_members = vec![
      MemberRecord {
        repository: repo,
        green: first,
        origin: branch(root, 1),
      },
      MemberRecord {
        repository: repo,
        green: root,
        origin: MemberOrigin::Root,
      },
    ];
    assert!(matches!(
      Partition::from_snapshot(&disordered, caps(), 0),
      Err(DbError::Corrupt { .. })
    ));
  }

  /// One step of a generated history over a repository namespace and its lineage.
  #[derive(Clone, Debug)]
  enum Step {
    Create {
      serial: u8,
      name: u8,
    },
    Bind {
      serial: u8,
      name: u8,
      green: u8,
    },
    Retire {
      serial: u8,
      name: u8,
    },
    Default {
      serial: u8,
      name: Option<u8>,
    },
    Destroy {
      serial: u8,
    },
    DestroyGreen {
      green: u8,
    },
    JoinRoot {
      serial: u8,
      green: u8,
    },
    JoinBranch {
      serial: u8,
      green: u8,
      from: u8,
      version: u8,
    },
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
      (0..SERIALS, 0..GREENS).prop_map(|(serial, green)| Step::JoinRoot { serial, green }),
      (0..SERIALS, 0..GREENS, 0..GREENS, any::<u8>()).prop_map(|(serial, green, from, version)| {
        Step::JoinBranch {
          serial,
          green,
          from,
          version,
        }
      }),
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
    /// Each lineage's members in the order they joined, by repository serial: each green serial with the member it was
    /// made from and the version, `None` for the root (A-575).
    lineages: BTreeMap<u8, Vec<(u8, Option<(u8, u8)>)>>,
  }

  /// What the model answers: the refusal's kind, so the comparison is of rules, not of the existing ids named.
  #[derive(Debug, PartialEq, Eq)]
  enum Answer {
    Ok,
    NotFound,
    AlreadyExists,
    BranchedFrom,
  }

  fn answer_of(result: &Result<(), DbError>) -> Answer {
    match result {
      Ok(()) => Answer::Ok,
      Err(DbError::NotFound) => Answer::NotFound,
      Err(DbError::AlreadyExists { .. }) => Answer::AlreadyExists,
      Err(DbError::BranchedFrom { .. }) => Answer::BranchedFrom,
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
        Step::DestroyGreen { green } => self.destroy_green(green),
        Step::JoinRoot { serial, green } => self.join(serial, green, None),
        Step::JoinBranch {
          serial,
          green,
          from,
          version,
        } => self.join(serial, green, Some((from, version))),
      }
    }

    /// A green's destroy: refused while a live member was made from it; its name and its membership leave with it.
    fn destroy_green(&mut self, green: u8) -> Answer {
      if !self.greens.contains(&green) {
        return Answer::NotFound;
      }
      let made_from = self
        .lineages
        .values()
        .flatten()
        .any(|(_, origin)| origin.is_some_and(|(from, _)| from == green));
      if made_from {
        return Answer::BranchedFrom;
      }
      self.greens.remove(&green);
      self.names.retain(|_, bound| *bound != green);
      for members in self.lineages.values_mut() {
        members.retain(|(member, _)| *member != green);
      }
      self.lineages.retain(|_, members| !members.is_empty());
      Answer::Ok
    }

    /// A green joining a repository's lineage, the rules in the guard's order (A-575).
    fn join(&mut self, serial: u8, green: u8, origin: Option<(u8, u8)>) -> Answer {
      if !self.repositories.contains_key(&serial) || !self.greens.contains(&green) {
        return Answer::NotFound;
      }
      if self
        .lineages
        .values()
        .flatten()
        .any(|(member, _)| *member == green)
      {
        return Answer::AlreadyExists;
      }
      let members = self.lineages.get(&serial);
      match origin {
        None if members.is_some_and(|members| !members.is_empty()) => return Answer::AlreadyExists,
        Some((from, _))
          if !members.is_some_and(|members| members.iter().any(|(member, _)| *member == from)) =>
        {
          return Answer::NotFound;
        }
        _ => {}
      }
      self
        .lineages
        .entry(serial)
        .or_default()
        .push((green, origin));
      Answer::Ok
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
      Step::JoinRoot { serial, green } => Op::RepositoryGreenJoined {
        record: MemberRecord {
          repository: repository_id(HOME, serial),
          green: volume_id(HOME, green),
          origin: MemberOrigin::Root,
        },
      },
      Step::JoinBranch {
        serial,
        green,
        from,
        version,
      } => Op::RepositoryGreenJoined {
        record: MemberRecord {
          repository: repository_id(HOME, serial),
          green: volume_id(HOME, green),
          origin: MemberOrigin::Branch {
            from: volume_id(HOME, from),
            version: u64::from(version),
          },
        },
      },
    }
  }

  /// The model's state in the partition's terms, to compare whole.
  fn expected(model: &Model) -> Answers {
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
    let members = model
      .lineages
      .iter()
      .flat_map(|(serial, members)| {
        members.iter().map(|(green, origin)| MemberRecord {
          repository: repository_id(HOME, *serial),
          green: volume_id(HOME, *green),
          origin: match origin {
            None => MemberOrigin::Root,
            Some((from, version)) => MemberOrigin::Branch {
              from: volume_id(HOME, *from),
              version: u64::from(*version),
            },
          },
        })
      })
      .collect();
    let floors = (0..GREENS)
      .map(|green| {
        model
          .lineages
          .values()
          .flatten()
          .filter_map(|(_, origin)| match origin {
            Some((from, version)) if *from == green => Some(u64::from(*version)),
            _ => None,
          })
          .min()
      })
      .collect();
    (records, bindings, members, floors)
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(400), ..ProptestConfig::default() }))]

    /// A-515, A-575 (§4.17.1). Do: run a generated history of repository creates, binds, retires, default changes,
    /// repository destroys, green destroys, and greens joining lineages as roots or as branches of members at a
    /// version, on a partition holding a few greens. Expect: after every step the partition's answer (accepted, or
    /// which refusal) and its whole state (records, names, every lineage's members in the order they joined, each
    /// green's branch floor) equal the design's rules kept over plain maps, and a partition rebuilt from its snapshot
    /// answers the same.
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

  /// A rule allowing everyone to submit to refs matching `pattern`.
  fn ref_rule(pattern: &str) -> crate::catalog::PolicyRule {
    crate::catalog::PolicyRule {
      names: crate::catalog::PolicyNames::Refs,
      pattern: pattern.to_owned(),
      right: crate::catalog::PolicyRight::Submit,
      effect: crate::catalog::PolicyEffect::Allow,
      who: crate::catalog::PolicyWho::Anyone,
    }
  }

  /// A policy of `rules`, version `version`, no hooks.
  fn rules_policy(version: u64, rules: Vec<crate::catalog::PolicyRule>) -> RepositoryPolicy {
    RepositoryPolicy {
      version,
      rules,
      hooks: Vec::new(),
      hooks_env: Vec::new(),
      signers: Vec::new(),
    }
  }

  /// A-648 (§4.17.4 item 9). Do: create a Subversion repository; set its policy with a hook at start-commit, pre-lock
  /// and post-unlock and a hooks-env of a default section and a pre-lock one; rebuild the partition from its snapshot
  /// and, apart from it, replay its operations. Expect: the record holds the policy whole, and the rebuilt and the
  /// replayed partitions hold it alike.
  #[test]
  fn a_subversion_repositorys_hooks_and_hooks_env_are_kept_by_a_snapshot_and_a_replay() {
    use crate::catalog::{HookConfig, HookPointName, HooksEnvSection, HooksEnvVariable};
    let mut partition = Partition::new(caps(), 0);
    let mut record = repository(11, "svn");
    record.tool = VcsTool::Subversion;
    let repo = record.id;
    let created = Op::RepositoryCreated { record };
    run(&mut partition, &created).unwrap();
    let program = std::env::current_exe()
      .unwrap()
      .to_string_lossy()
      .into_owned();
    let hook = |point: HookPointName| HookConfig {
      point,
      name: point.name().to_owned(),
      program: program.clone(),
    };
    let section = |name: &str, variable: &str, value: &[u8]| HooksEnvSection {
      name: name.to_owned(),
      variables: vec![HooksEnvVariable {
        name: variable.to_owned(),
        value: value.to_vec(),
      }],
    };
    let policy = RepositoryPolicy {
      version: 1,
      rules: Vec::new(),
      hooks: vec![
        hook(HookPointName::SvnStartCommit),
        hook(HookPointName::SvnPreLock),
        hook(HookPointName::SvnPostUnlock),
      ],
      hooks_env: vec![
        section("default", "LANG", b"en_US.UTF-8"),
        section("pre-lock", "TOKENS", b"made by the hook"),
      ],
      signers: Vec::new(),
    };
    let set = Op::RepositoryPolicySet {
      repository: repo,
      policy: policy.clone(),
    };
    run(&mut partition, &set).unwrap();
    let held = |partition: &Partition| {
      partition
        .repositories()
        .repository(repo)
        .map(|record| record.policy.clone())
    };
    assert_eq!(held(&partition), Some(policy.clone()));
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(held(&rebuilt), Some(policy.clone()));
    let mut replayed = Partition::new(caps(), 0);
    replayed.apply(&created).unwrap();
    replayed.apply(&set).unwrap();
    assert_eq!(held(&replayed), Some(policy));
  }

  /// A-622 (§4.17.1 "Policy"). Do: create a git repository; set its policy from version 0 with a rule and a hook; set
  /// it again from the stale version 0; set one holding a branch rule, which a git repository has not, and one running
  /// a relative program; create a repository already holding a policy; then rebuild the partition from its snapshot,
  /// and apart from it replay its recorded operations. Expect: the record holds the policy at version 1, its own log's
  /// position, and no other log moved (A-642); the stale setting is refused naming version 1, the malformed ones by kind and index, the create
  /// naming version 0, none of them changing anything; the rebuilt and the replayed partitions answer as the live one.
  #[test]
  fn a_policy_is_set_whole_from_the_version_read_and_kept_by_a_snapshot_and_a_replay() {
    let mut partition = Partition::new(caps(), 0);
    let mut log = Vec::new();
    let record = repository(9, "linux");
    let repo = record.id;
    let created = Op::RepositoryCreated { record };
    run(&mut partition, &created).unwrap();
    log.push(created);
    let names = partition
      .repositories()
      .log_position(repo, crate::catalog::StateLog::Names);
    let mut policy = rules_policy(1, vec![ref_rule("refs/heads/release/*")]);
    policy.hooks.push(crate::catalog::HookConfig {
      point: crate::catalog::HookPointName::GitPreReceive,
      name: "pre-receive".to_owned(),
      program: std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned(),
    });
    let set = Op::RepositoryPolicySet {
      repository: repo,
      policy: policy.clone(),
    };
    run(&mut partition, &set).unwrap();
    log.push(set);
    assert_eq!(
      partition.repositories().repository(repo).unwrap().policy,
      policy
    );
    assert_eq!(
      partition
        .repositories()
        .log_position(repo, crate::catalog::StateLog::Names),
      names
    );
    let before = answers(&partition);
    let stale = Op::RepositoryPolicySet {
      repository: repo,
      policy: rules_policy(1, Vec::new()),
    };
    assert_eq!(
      run(&mut partition, &stale),
      Err(DbError::PolicyChanged { current: 1 })
    );
    let mut branches = ref_rule("main");
    branches.names = crate::catalog::PolicyNames::Branches;
    let wrong_kind = Op::RepositoryPolicySet {
      repository: repo,
      policy: rules_policy(2, vec![ref_rule("refs/tags/*"), branches]),
    };
    assert_eq!(
      run(&mut partition, &wrong_kind),
      Err(DbError::PolicyRuleMalformed { index: 1 })
    );
    let mut relative = rules_policy(2, Vec::new());
    relative.hooks.push(crate::catalog::HookConfig {
      point: crate::catalog::HookPointName::GitUpdate,
      name: "update".to_owned(),
      program: "hooks/update".to_owned(),
    });
    assert_eq!(
      run(
        &mut partition,
        &Op::RepositoryPolicySet {
          repository: repo,
          policy: relative,
        }
      ),
      Err(DbError::PolicyHookMalformed { index: 0 })
    );
    let mut holding = repository(10, "holding");
    holding.policy = rules_policy(1, Vec::new());
    assert_eq!(
      run(&mut partition, &Op::RepositoryCreated { record: holding }),
      Err(DbError::PolicyChanged { current: 0 })
    );
    assert_eq!(
      answers(&partition),
      before,
      "a refused setting changes nothing"
    );
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(answers(&rebuilt), answers(&partition));
    assert_eq!(
      rebuilt
        .repositories()
        .log_position(repo, crate::catalog::StateLog::Names),
      names
    );
    let mut replayed = Partition::new(caps(), 0);
    for op in &log {
      replayed.apply(op).unwrap();
    }
    assert_eq!(answers(&replayed), answers(&partition));
  }

  /// A-622 (the merge budget, by use). Do: under a merge budget of four rules' bytes, set a three-rule policy on one
  /// repository; replace it with another three-rule policy; set a two-rule policy on a second repository, on the live
  /// partition and on one rebuilt from its snapshot; destroy the first repository and set it again on the second.
  /// Expect: the replacement fits, since the policy it replaces is given back; the second repository's policy is
  /// refused `Capacity` on both partitions, so the rebuilt one charges the first repository's policy as the live one
  /// does; and once the first repository has left, its policy's bytes are back and the second's fits.
  #[test]
  fn a_policys_bytes_are_charged_given_back_and_charged_again_after_a_snapshot() {
    let one = slates_wire::Wire::to_bytes(&ref_rule("refs/heads/r0")).len();
    let budget = PartitionCaps {
      green_chain_bytes: 4 * one,
      ..caps()
    };
    let rules = |names: &[&str]| names.iter().map(|name| ref_rule(name)).collect::<Vec<_>>();
    let mut partition = Partition::new(budget, 0);
    let first = repository(9, "first");
    let second = repository(10, "second");
    let (first_id, second_id) = (first.id, second.id);
    run(&mut partition, &Op::RepositoryCreated { record: first }).unwrap();
    run(&mut partition, &Op::RepositoryCreated { record: second }).unwrap();
    let set = |repository, policy| Op::RepositoryPolicySet { repository, policy };
    run(
      &mut partition,
      &set(
        first_id,
        rules_policy(
          1,
          rules(&["refs/heads/r0", "refs/heads/r1", "refs/heads/r2"]),
        ),
      ),
    )
    .unwrap();
    run(
      &mut partition,
      &set(
        first_id,
        rules_policy(
          2,
          rules(&["refs/heads/r3", "refs/heads/r4", "refs/heads/r5"]),
        ),
      ),
    )
    .expect("a replacement gives back the policy it replaces");
    let crowded = set(
      second_id,
      rules_policy(1, rules(&["refs/heads/r6", "refs/heads/r7"])),
    );
    let full = Err(DbError::Capacity {
      table: "green_chains",
    });
    assert_eq!(run(&mut partition, &crowded), full);
    let mut rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), budget, 0).unwrap();
    assert_eq!(
      run(&mut rebuilt, &crowded),
      full,
      "the rebuilt partition charges the first policy"
    );
    run(
      &mut rebuilt,
      &Op::RepositoryDestroyed {
        repository: first_id,
      },
    )
    .unwrap();
    run(&mut rebuilt, &crowded).expect("a destroyed repository gives its policy's bytes back");
  }
}
