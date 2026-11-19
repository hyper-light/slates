//! An entry of a repository's logs: its guard and its byte measure (§4.17.1 item 2, shared piece 2; A-603, rebuilt by
//! A-642).
//!
//! A repository's greens share one register, and one entry of it commits any set of the repository's changes as one
//! durable step: several greens' versions (git's atomic push: "Either all refs are updated or none" [B:
//! gitprotocol-capabilities `atomic`]; a stacked change landed whole is several versions of one green), greens joining
//! its lineage, its namespace's changes (§4.17.1 item 3), and its own state (Mercurial's phase roots and obsolescence
//! markers, §4.17.3 decisions 3 and 4), moved with the push that moves them or alone.
//!
//! **The rule the guard keeps.** An entry is judged against each log's own position, never a position of the whole
//! repository (A-642): its checks read each log before any of its own changes, so a check holds at exactly the positions
//! the entry appends at (Mercurial's `check:heads` and `unbundle`'s `heads`, every head unchanged, read in the push's own
//! step, §4.17.3 decision 6); each version appends to its green's log only while the green is at the position the push
//! read, the version before it, and a green that moved is refused `Moved`, named; then its members join, its names
//! change and its state moves, each item judged against what the items before it left, as the apply will apply them.
//! Anything that does not hold refuses the whole entry, typed, before the log records any of it; the apply is then
//! unconditional, so a replay of the log rebuilds the same logs (guard then apply, `crate::partition`).
//!
//! **No copy of the tables.** The guard keeps what the entry's earlier items changed in an `Overlay` of small maps
//! over the register's state, so judging an entry costs its own items, each a lookup, never a copy of the namespace or
//! the state. Rejected on its cost, not measured: applying the entry to a clone of the repository's tables and
//! keeping the clone on success, which costs the repository's size for every entry.
//!
//! **What each change costs the budget.** A version's chain entry, a phase root's node and phase, a marker's bytes and a
//! push's objects (A-662) are held in the partition's RAM for as long as the record lives, so each is charged to the
//! partition's merge budget
//! (`PartitionCaps::green_chain_bytes`) with the chains, origins, checkpoints and work logs: an entry past it is refused
//! `Capacity`, typed, and a removed root or a destroyed repository gives its bytes back.

use std::collections::{BTreeMap, BTreeSet};

use super::{Held, Register, Repositories, valid_name};
use crate::catalog::{
  GreenVersion, MemberOrigin, MemberRecord, NameChange, RepositoryCheck, RepositoryEntry,
  RepositoryId, RepositoryRecord, StateChange, VolumeId,
};
use crate::error::DbError;

#[cfg(test)]
mod tests;

/// What an apply changed of the repositories' own state, in bytes (A-603): what the partition charges to its merge
/// budget and what it gives back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateBytes {
  /// Bytes the change added.
  pub added: usize,
  /// Bytes the change released.
  pub released: usize,
}

impl StateBytes {
  /// Both counts added to.
  pub(super) fn add(&mut self, other: StateBytes) {
    self.added = self.added.saturating_add(other.added);
    self.released = self.released.saturating_add(other.released);
  }
}

/// The bytes a phase root holds: its node and its phase.
pub(super) fn root_bytes(node: &[u8]) -> usize {
  node.len().saturating_add(size_of::<u8>())
}

/// The bytes a lock holds: its path and its record.
pub(super) fn lock_bytes(path: &str, lock: &[u8]) -> usize {
  path.len().saturating_add(lock.len())
}

/// A marker's identity: BLAKE3 of its bytes, so a held marker is found by a lookup, and two markers with the same bytes
/// are one marker, as Mercurial's obsstore keeps them [C: `obsolete.py` `obsstore.add`, which skips a known marker].
pub(super) fn marker_id(marker: &[u8]) -> [u8; 32] {
  *blake3::hash(marker).as_bytes()
}

/// An item's index among its part of the entry, as a refusal names it.
fn index_of(index: usize) -> u32 {
  u32::try_from(index).unwrap_or(u32::MAX)
}

/// What the entry's items before the one judged changed, over the register's state at its position. Keyed by borrows
/// of the entry, so it holds no copy of a name, a node or a marker.
#[derive(Default)]
struct Overlay<'entry> {
  /// Members joined earlier in the entry.
  joined: BTreeMap<VolumeId, &'entry MemberRecord>,
  /// The root joined earlier in the entry, if one did.
  root: Option<VolumeId>,
  /// Each green's version committed last in the entry.
  heads: BTreeMap<VolumeId, u64>,
  /// Names bound (`Some`) or retired (`None`) earlier in the entry.
  names: BTreeMap<&'entry str, Option<VolumeId>>,
  /// Greens bound (`true`) or let go by a retire (`false`) earlier in the entry.
  bound: BTreeMap<VolumeId, bool>,
  /// Phase roots added (`true`) or removed (`false`) earlier in the entry.
  roots: BTreeMap<(u8, &'entry [u8]), bool>,
  /// Markers added earlier in the entry, by identity.
  markers: BTreeSet<[u8; 32]>,
  /// Locks taken (`Some`) or released (`None`) earlier in the entry, by path.
  locks: BTreeMap<&'entry str, Option<&'entry [u8]>>,
}

/// The guard for `entry` (`Partition::check` for `Op::RepositoryAdvanced`): the module docs' rule. `held` says what the
/// partition holds under a volume id; `head` is a green's head as its durable records hold it.
pub(super) fn check(
  repositories: &Repositories,
  entry: &RepositoryEntry,
  held: impl Fn(VolumeId) -> Held,
  head: impl Fn(VolumeId) -> u64,
) -> Result<(), DbError> {
  let record = repositories
    .repository(entry.repository)
    .ok_or(DbError::NotFound)?;
  let register = repositories
    .registers
    .get(&entry.repository)
    .ok_or(DbError::NotFound)?;
  for (index, check) in entry.checks.iter().enumerate() {
    holds(repositories, (entry.repository, register), check, &head).map_err(|failed| {
      failed.unwrap_or(DbError::CheckFailed {
        index: index_of(index),
      })
    })?;
  }
  let mut overlay = Overlay::default();
  for member in &entry.joined {
    overlay.join(repositories, record, member, &held)?;
  }
  for version in &entry.versions {
    overlay.version(repositories, entry.repository, version, &head)?;
  }
  for change in &entry.names {
    overlay.name(repositories, record, change, &held)?;
  }
  for (index, change) in entry.state.iter().enumerate() {
    if !overlay.state(register, change) {
      return Err(DbError::StateChanged {
        index: index_of(index),
      });
    }
  }
  Ok(())
}

/// Whether `check` holds in `repository`, each log at its own position: `Err(Some(_))` naming a green that moved,
/// `Err(None)` for any other check that does not hold.
fn holds(
  repositories: &Repositories,
  (repository, register): (RepositoryId, &Register),
  check: &RepositoryCheck,
  head: &impl Fn(VolumeId) -> u64,
) -> Result<(), Option<DbError>> {
  let held = match check {
    // A head the check names is a head of this repository: a green of another, or none, holds no head here; one that
    // moved is named, as a version's green that moved is (A-642).
    RepositoryCheck::Head { green, version } => {
      let member = repositories
        .members
        .get(green)
        .is_some_and(|member| member.repository == repository);
      let at = head(*green);
      if member && at != *version {
        return Err(Some(DbError::Moved {
          green: green.bytes,
          read: *version,
          at,
        }));
      }
      member
    }
    RepositoryCheck::Names { prefix, bound } => {
      let mut held = repositories.branches(repository, prefix);
      bound.iter().all(|binding| {
        held
          .next()
          .is_some_and(|(name, green)| name == binding.name && green == binding.green)
      }) && held.next().is_none()
    }
    RepositoryCheck::Log { log, position } => register.position(*log) == *position,
  };
  if held { Ok(()) } else { Err(None) }
}

/// What an entry adds to the partition's merge budget and gives back of it (module docs): every version's chain entry
/// and every root and marker it adds, against every root it removes. Exact for an entry the guard admitted, whose adds
/// are new and whose removes are held.
pub(super) fn bytes(entry: &RepositoryEntry) -> StateBytes {
  let mut bytes = StateBytes {
    added: entry
      .versions
      .iter()
      .map(|version| version.entry.len())
      .fold(0, usize::saturating_add),
    released: 0,
  };
  for change in &entry.state {
    bytes.add(change_bytes(change));
  }
  // The push's objects are held by the register for as long as the repository lives (A-662).
  bytes.added = bytes.added.saturating_add(entry.objects.len());
  bytes
}

/// What one state change adds or releases.
pub(super) fn change_bytes(change: &StateChange) -> StateBytes {
  match change {
    StateChange::PhaseRootAdded { node, .. } => StateBytes {
      added: root_bytes(node),
      released: 0,
    },
    StateChange::PhaseRootRemoved { node, .. } => StateBytes {
      added: 0,
      released: root_bytes(node),
    },
    StateChange::MarkerAdded { marker } => StateBytes {
      added: marker.len(),
      released: 0,
    },
    StateChange::LockTaken { path, lock } => StateBytes {
      added: lock_bytes(path, lock),
      released: 0,
    },
    StateChange::LockReleased { path, lock } => StateBytes {
      added: 0,
      released: lock_bytes(path, lock),
    },
  }
}

impl<'entry> Overlay<'entry> {
  /// Whether `green` is a member of `repository` at this point of the entry.
  fn member_of(
    &self,
    repositories: &Repositories,
    repository: RepositoryId,
    green: VolumeId,
  ) -> bool {
    self
      .joined
      .get(&green)
      .map(|member| member.repository)
      .or_else(|| {
        repositories
          .members
          .get(&green)
          .map(|member| member.repository)
      })
      == Some(repository)
  }

  /// The green `name` binds at this point of the entry.
  fn binding(
    &self,
    repositories: &Repositories,
    repository: RepositoryId,
    name: &str,
  ) -> Option<VolumeId> {
    match self.names.get(name) {
      Some(bound) => *bound,
      None => repositories.branch(repository, name),
    }
  }

  /// Whether a name binds `green` at this point of the entry.
  fn is_bound(&self, repositories: &Repositories, green: VolumeId) -> bool {
    match self.bound.get(&green) {
      Some(bound) => *bound,
      None => repositories.bound.contains_key(&green),
    }
  }

  /// A green joins the lineage (A-575's rules, at this point of the entry): of the entry's repository, a green of its
  /// partition, not a member yet; the lineage's first member is its root and every other is made from a member of the
  /// same lineage.
  fn join(
    &mut self,
    repositories: &Repositories,
    record: &RepositoryRecord,
    member: &'entry MemberRecord,
    held: &impl Fn(VolumeId) -> Held,
  ) -> Result<(), DbError> {
    if member.repository != record.id {
      return Err(DbError::NotFound);
    }
    match held(member.green) {
      Held::Absent => return Err(DbError::NotFound),
      Held::NotGreen => return Err(DbError::NotGreen),
      Held::Green => {}
    }
    if member.green.partition() != record.id.partition() {
      return Err(DbError::NotFound);
    }
    if self.joined.contains_key(&member.green) || repositories.members.contains_key(&member.green) {
      return Err(DbError::AlreadyExists {
        existing: member.green.bytes,
      });
    }
    match member.origin {
      MemberOrigin::Root => {
        let root = repositories
          .lineages
          .get(&record.id)
          .and_then(|order| order.first())
          .copied()
          .or(self.root);
        if let Some(root) = root {
          return Err(DbError::AlreadyExists {
            existing: root.bytes,
          });
        }
        self.root = Some(member.green);
      }
      MemberOrigin::Branch { from, .. } => {
        if !self.member_of(repositories, record.id, from) {
          return Err(DbError::NotFound);
        }
      }
    }
    self.joined.insert(member.green, member);
    Ok(())
  }

  /// A version commits: of a member of the repository, the one after its head or after its version earlier in the
  /// entry.
  fn version(
    &mut self,
    repositories: &Repositories,
    repository: RepositoryId,
    version: &GreenVersion,
    head: &impl Fn(VolumeId) -> u64,
  ) -> Result<(), DbError> {
    if !self.member_of(repositories, repository, version.green) {
      return Err(DbError::NotFound);
    }
    let at = self
      .heads
      .get(&version.green)
      .copied()
      .unwrap_or_else(|| head(version.green));
    if version.version != at.saturating_add(1) {
      return Err(DbError::Moved {
        green: version.green.bytes,
        read: version.version.saturating_sub(1),
        at,
      });
    }
    self.heads.insert(version.green, version.version);
    Ok(())
  }

  /// A name changes (A-515's rules, at this point of the entry): a bind takes a valid name the repository does not
  /// hold, for a green of its partition and lineage that no name binds; a retire names a bound name; a default is a
  /// valid name or none.
  fn name(
    &mut self,
    repositories: &Repositories,
    record: &RepositoryRecord,
    change: &'entry NameChange,
    held: &impl Fn(VolumeId) -> Held,
  ) -> Result<(), DbError> {
    match change {
      NameChange::Bind { name, green } => {
        valid_name(name)?;
        if let Some(existing) = self.binding(repositories, record.id, name) {
          return Err(DbError::AlreadyExists {
            existing: existing.bytes,
          });
        }
        match held(*green) {
          Held::Absent => return Err(DbError::NotFound),
          Held::NotGreen => return Err(DbError::NotGreen),
          Held::Green => {}
        }
        if green.partition() != record.id.partition()
          || !self.member_of(repositories, record.id, *green)
        {
          return Err(DbError::NotFound);
        }
        if self.is_bound(repositories, *green) {
          return Err(DbError::AlreadyExists {
            existing: green.bytes,
          });
        }
        self.names.insert(name.as_str(), Some(*green));
        self.bound.insert(*green, true);
        Ok(())
      }
      NameChange::Retire { name } => {
        let green = self
          .binding(repositories, record.id, name)
          .ok_or(DbError::NotFound)?;
        self.names.insert(name.as_str(), None);
        self.bound.insert(green, false);
        Ok(())
      }
      NameChange::Default { branch } => branch.as_deref().map_or(Ok(()), valid_name),
    }
  }

  /// A state change applies to the state at this point of the entry: a root added is not one, a root removed is one, a
  /// marker added is not held. False when it does not.
  fn state(&mut self, register: &Register, change: &'entry StateChange) -> bool {
    match change {
      StateChange::PhaseRootAdded { phase, node } => {
        let key = (*phase, node.as_slice());
        if self.root_held(register, key) {
          return false;
        }
        self.roots.insert(key, true);
        true
      }
      StateChange::PhaseRootRemoved { phase, node } => {
        let key = (*phase, node.as_slice());
        if !self.root_held(register, key) {
          return false;
        }
        self.roots.insert(key, false);
        true
      }
      StateChange::MarkerAdded { marker } => {
        let id = marker_id(marker);
        if self.markers.contains(&id) || register.marker_ids.contains(&id) {
          return false;
        }
        self.markers.insert(id);
        true
      }
      StateChange::LockTaken { path, lock } => {
        if self.lock(register, path).is_some() {
          return false;
        }
        self.locks.insert(path.as_str(), Some(lock.as_slice()));
        true
      }
      StateChange::LockReleased { path, lock } => {
        if self.lock(register, path) != Some(lock.as_slice()) {
          return false;
        }
        self.locks.insert(path.as_str(), None);
        true
      }
    }
  }

  /// The lock `path` holds at this point of the entry.
  fn lock<'held>(&self, register: &'held Register, path: &str) -> Option<&'held [u8]>
  where
    'entry: 'held,
  {
    match self.locks.get(path) {
      Some(held) => *held,
      None => register.lock(path),
    }
  }

  /// Whether `(phase, node)` is a phase root at this point of the entry.
  fn root_held(&self, register: &Register, key: (u8, &'entry [u8])) -> bool {
    match self.roots.get(&key) {
      Some(held) => *held,
      None => register.holds_root(key.0, key.1),
    }
  }
}
