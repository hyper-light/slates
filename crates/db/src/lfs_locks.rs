//! A repository's Git LFS locks (§4.17.2 "Large files"; A-629 part 2; A-642).
//!
//! **A log of its own.** A repository that has taken a lock has a lock log: the place its next
//! lock takes, and the locks that stand, each at the place it took. A-642 keeps a repository's own
//! state in logs of its own, never on a position every push passes, so a lock is not an entry of
//! the repository's register (A-603) and taking one never waits behind a push. A lock is taken
//! only at the place its taker read: `Op::LfsLockTaken` names the place, which must be the one the
//! log stands at, so a taker that read a log another taker has moved since is refused (`Moved`)
//! and reads again. One shard holds a repository and writes its log, so that refusal names a race
//! between two of its own tasks. A place is never taken twice: the log outlives its last lock and
//! leaves only with its repository, so a lock's place is its id for good, and a cursor naming a
//! released lock still names a place in the log.
//!
//! **The rules the guard keeps** ([`LfsLocks::check`]), each refused by its own kind
//! ([`LfsLockRefusal`]), in this order, a request's form before its timing:
//! - a lock names a repository this partition holds (`NotFound` otherwise);
//! - its path is not empty and holds no NUL byte (`Path`);
//! - it takes the place the log stands at (`Moved`, naming it), the first lock place 0;
//! - the log has a place after it (`Exhausted` at `u64::MAX`);
//! - no lock stands on its path (`Held`, naming the lock): git-lfs locks per path \[B: git-lfs
//!   `docs/api/locking.md`\];
//! - a release names a standing lock (`NotHeld`). Who may release which lock is the server's
//!   judgment (its owner, or another with force where the repository's policy allows it), made
//!   before the release is recorded.
//!
//! **Logged, applied, snapshotted.** Logged as operations and applied unconditionally once the
//! guard has passed them; carried whole by a partition snapshot (each log's place and every
//! standing lock) and rebuilt from it ([`LfsLocks::restore`]). The index of paths to places is
//! derived and rebuilt rather than carried. A repository's locks and its log leave with it.
//!
//! **Bounds.** A lock's bytes (its record, its path and its owner's key) and a log's place are
//! charged to the partition's merge byte budget (`PartitionCaps::green_chain_bytes`, which the
//! daemon derives from memory), as the review records are (A-596), so a partition holds no more
//! locks than it has room for, refused with the merge plane's typed `Capacity` at it.

use std::collections::BTreeMap;

use crate::catalog::{LfsLockLogRecord, LfsLockRecord, RepositoryId};
use crate::error::{DbError, LfsLockRefusal};
use crate::op::Op;

/// The bytes a lock charges: its record, its path and its owner's key.
fn lock_bytes(record: &LfsLockRecord) -> usize {
  size_of::<LfsLockRecord>()
    .saturating_add(record.path.len())
    .saturating_add(record.owner.key().len())
}

/// The bytes a lock log's place charges.
fn log_bytes() -> usize {
  size_of::<LfsLockLogRecord>()
}

/// The refusal of `rule`.
fn refused(rule: LfsLockRefusal) -> Result<(), DbError> {
  Err(DbError::LfsLock(rule))
}

/// The refusal of a snapshot that cannot be restored whole.
fn corrupt(reason: &'static str) -> DbError {
  DbError::Corrupt { seq: 0, reason }
}

/// The Git LFS lock logs of one partition's repositories.
#[derive(Debug, Default)]
pub struct LfsLocks {
  /// Each repository's log: the place its next lock takes.
  logs: BTreeMap<RepositoryId, u64>,
  /// The standing locks, by repository and place, so one repository's are one range in its log's
  /// order.
  locks: BTreeMap<(RepositoryId, u64), LfsLockRecord>,
  /// Derived: each repository's paths, each to the place of the lock standing on it.
  paths: BTreeMap<RepositoryId, BTreeMap<String, u64>>,
  /// The bytes the logs and locks hold, charged to the partition's merge byte budget.
  bytes: usize,
}

impl LfsLocks {
  /// No log and no lock.
  pub fn new() -> LfsLocks {
    LfsLocks::default()
  }

  /// The bytes the logs and locks hold.
  pub fn bytes(&self) -> usize {
    self.bytes
  }

  /// The place `repository`'s next lock takes: 0 before its first.
  pub fn next(&self, repository: RepositoryId) -> u64 {
    self.logs.get(&repository).copied().unwrap_or(0)
  }

  /// The lock standing on `path` of `repository`.
  pub fn on_path(&self, repository: RepositoryId, path: &str) -> Option<&LfsLockRecord> {
    let place = self.paths.get(&repository)?.get(path)?;
    self.locks.get(&(repository, *place))
  }

  /// The lock standing at `place` of `repository`'s log.
  pub fn at(&self, repository: RepositoryId, place: u64) -> Option<&LfsLockRecord> {
    self.locks.get(&(repository, place))
  }

  /// `repository`'s standing locks from `place` on, in its log's order.
  pub fn from_place(
    &self,
    repository: RepositoryId,
    place: u64,
  ) -> impl Iterator<Item = &LfsLockRecord> + '_ {
    self
      .locks
      .range((repository, place)..=(repository, u64::MAX))
      .map(|(_, record)| record)
  }

  /// Every log's place, for a snapshot, in repository order.
  pub fn log_records(&self) -> impl Iterator<Item = LfsLockLogRecord> + '_ {
    self.logs.iter().map(|(repository, next)| LfsLockLogRecord {
      repository: *repository,
      next: *next,
    })
  }

  /// Every standing lock, for a snapshot, by repository and place.
  pub fn lock_records(&self) -> impl Iterator<Item = &LfsLockRecord> + '_ {
    self.locks.values()
  }

  /// The bytes `op` releases and adds: what the partition's merge budget must have room for
  /// before it is applied (`Partition::check`), and what the budget moves by when it is.
  pub fn charge(&self, op: &Op) -> (usize, usize) {
    match op {
      Op::LfsLockTaken { lock } => {
        let log = if self.logs.contains_key(&lock.repository) {
          0
        } else {
          log_bytes()
        };
        (0, lock_bytes(lock).saturating_add(log))
      }
      Op::LfsLockReleased { repository, place } => {
        (self.at(*repository, *place).map_or(0, lock_bytes), 0)
      }
      _ => (0, 0),
    }
  }

  /// The guard for a lock operation (`Partition::check`): refuses what the log must never record
  /// (the module's rules). `holds` says whether this partition holds a repository.
  pub fn check(&self, op: &Op, holds: impl Fn(RepositoryId) -> bool) -> Result<(), DbError> {
    match op {
      Op::LfsLockTaken { lock } => self.check_taken(lock, holds),
      Op::LfsLockReleased { repository, place } => {
        if !holds(*repository) {
          return Err(DbError::NotFound);
        }
        if self.at(*repository, *place).is_none() {
          return refused(LfsLockRefusal::NotHeld);
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  /// The guard for a lock taken (the module's rules, in their order).
  fn check_taken(
    &self,
    lock: &LfsLockRecord,
    holds: impl Fn(RepositoryId) -> bool,
  ) -> Result<(), DbError> {
    if !holds(lock.repository) {
      return Err(DbError::NotFound);
    }
    if lock.path.is_empty() || lock.path.contains('\0') {
      return refused(LfsLockRefusal::Path);
    }
    let next = self.next(lock.repository);
    if lock.place != next {
      return refused(LfsLockRefusal::Moved { next });
    }
    if next == u64::MAX {
      return refused(LfsLockRefusal::Exhausted);
    }
    if let Some(holder) = self.on_path(lock.repository, &lock.path) {
      return refused(LfsLockRefusal::Held {
        place: holder.place,
      });
    }
    Ok(())
  }

  /// Applies a lock operation the guard passed, unconditionally (the module doc). A release of no
  /// lock, which the guard refuses, is a corrupt log, refused rather than half applied.
  pub fn apply(&mut self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::LfsLockTaken { lock } => {
        self.insert(lock.clone());
        Ok(())
      }
      Op::LfsLockReleased { repository, place } => {
        let record = self
          .locks
          .remove(&(*repository, *place))
          .ok_or_else(|| corrupt("a lock released at a place where no lock stands"))?;
        if let Some(paths) = self.paths.get_mut(repository) {
          paths.remove(&record.path);
        }
        self.bytes = self.bytes.saturating_sub(lock_bytes(&record));
        Ok(())
      }
      _ => Ok(()),
    }
  }

  /// Puts `lock` in its log, the log made with its first lock and moved past the lock's place.
  fn insert(&mut self, lock: LfsLockRecord) {
    let next = lock.place.saturating_add(1);
    if self.logs.insert(lock.repository, next).is_none() {
      self.bytes = self.bytes.saturating_add(log_bytes());
    }
    self
      .paths
      .entry(lock.repository)
      .or_default()
      .insert(lock.path.clone(), lock.place);
    self.bytes = self.bytes.saturating_add(lock_bytes(&lock));
    self.locks.insert((lock.repository, lock.place), lock);
  }

  /// A destroyed repository's locks and log leave with it: the bytes they held, which the
  /// partition's merge budget releases.
  pub fn repository_removed(&mut self, repository: RepositoryId) -> usize {
    let mut released = 0usize;
    if self.logs.remove(&repository).is_some() {
      released = released.saturating_add(log_bytes());
    }
    self.paths.remove(&repository);
    let places: Vec<u64> = self
      .from_place(repository, 0)
      .map(|record| record.place)
      .collect();
    for place in places {
      if let Some(record) = self.locks.remove(&(repository, place)) {
        released = released.saturating_add(lock_bytes(&record));
      }
    }
    self.bytes = self.bytes.saturating_sub(released);
    released
  }

  /// The lock logs a snapshot carries, rebuilt with their byte count. A log or lock of a repository
  /// `holds` says the partition does not hold, two logs of one repository, a lock with no log or at
  /// a place its log has not reached, or two locks on one path, is a corrupt snapshot, refused
  /// rather than half restored.
  pub fn restore(
    logs: &[LfsLockLogRecord],
    locks: &[LfsLockRecord],
    holds: impl Fn(RepositoryId) -> bool,
  ) -> Result<LfsLocks, DbError> {
    let mut restored = LfsLocks::new();
    for log in logs {
      if !holds(log.repository) || restored.logs.insert(log.repository, log.next).is_some() {
        return Err(corrupt(
          "a snapshot's lock log names no repository, or a repository has two",
        ));
      }
      restored.bytes = restored.bytes.saturating_add(log_bytes());
    }
    for lock in locks {
      let reached = restored
        .logs
        .get(&lock.repository)
        .is_some_and(|next| lock.place < *next);
      let free = restored.on_path(lock.repository, &lock.path).is_none()
        && restored.at(lock.repository, lock.place).is_none();
      if !reached || !free {
        return Err(corrupt(
          "a snapshot's lock stands past its log, at a place or on a path taken twice",
        ));
      }
      restored
        .paths
        .entry(lock.repository)
        .or_default()
        .insert(lock.path.clone(), lock.place);
      restored.bytes = restored.bytes.saturating_add(lock_bytes(lock));
      restored
        .locks
        .insert((lock.repository, lock.place), lock.clone());
    }
    Ok(restored)
  }
}

#[cfg(test)]
// proptest's strategy types carry `Arc` (D-8's harness exception).
#[allow(clippy::disallowed_types)]
#[path = "tests/lfs_locks.rs"]
mod tests;
