//! A holder's copy of a repository's state logs, built from the logs' records alone (A-636 part 2b): the oracle the
//! model test holds the partition's records to. A successor rebuilds a repository's state from what its holders hold,
//! so the records must continue each log exactly where the copy stands, carry one change a position, name each
//! operation alike on every log it moved, and rebuild every state log whole: positions, the namespace and the lineage,
//! the default, the phase roots, the markers in the order received, and the locks.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::{Model, index_of};
use crate::catalog::{
  LogSpan, LoggedChange, MemberOrigin, MemberRecord, NameChange, StateChange, StateLog,
  StateLogRecord, VolumeId,
};

/// What a copy of the state logs holds, in the model's terms: what [`Replica::view`] and [`StateView::of`] compare.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct StateView {
  /// The lineage in join order: each member and, for a branch, the member and version it was made at.
  members: Vec<(u8, Option<(u8, u64)>)>,
  /// The namespace.
  names: BTreeMap<String, u8>,
  /// The default branch.
  default: Option<String>,
  /// Each state log's position, in log order.
  positions: [u64; 4],
  /// The phase roots.
  roots: BTreeSet<(u8, Vec<u8>)>,
  /// The markers, in the order received.
  markers: Vec<Vec<u8>>,
  /// The locks, by path.
  locks: BTreeMap<String, Vec<u8>>,
}

impl StateView {
  /// The state logs of the model the partition is held to.
  pub(super) fn of(model: &Model) -> StateView {
    StateView {
      members: model.members.clone(),
      names: model.names.clone(),
      default: model.default.clone(),
      positions: [
        model.names_position,
        model.phases_position,
        model.markers.len() as u64,
        model.locks_position,
      ],
      roots: model.roots.clone(),
      markers: model.markers.clone(),
      locks: model.locks.clone(),
    }
  }
}

/// What one take of records met: the model test's census counts them.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Taken {
  /// Records taken.
  pub(super) records: u64,
  /// Operations that moved two logs or more, a state log among them: records a takeover keeps whole or drops.
  pub(super) several_logs: u64,
  /// Members that left, destroyed.
  pub(super) left: u64,
}

/// The copy: its logs' positions and what they hold, and every operation identity it met.
#[derive(Debug, Default)]
pub(super) struct Replica {
  /// Each state log's position.
  positions: BTreeMap<StateLog, u64>,
  /// The lineage in join order.
  members: Vec<MemberRecord>,
  /// The namespace.
  names: BTreeMap<String, VolumeId>,
  /// The default branch.
  default: Option<String>,
  /// The phase roots.
  roots: BTreeSet<(u8, Vec<u8>)>,
  /// The markers, in the order received.
  markers: Vec<Vec<u8>>,
  /// The locks, by path.
  locks: BTreeMap<String, Vec<u8>>,
  /// Every operation's identity met: no two operations name one.
  identities: BTreeSet<[u8; 32]>,
}

impl Replica {
  /// Takes `records`, in the order the database handed them on: each operation's records together, each continuing its
  /// log, its spans naming every log it moved, each record's own among them, and its identity new.
  pub(super) fn take(&mut self, records: Vec<StateLogRecord>) -> Result<Taken, TestCaseError> {
    let mut taken = Taken::default();
    let mut operation: Option<([u8; 32], Vec<LogSpan>)> = None;
    for record in records {
      let same = operation
        .as_ref()
        .is_some_and(|(identity, _)| *identity == record.identity);
      if same {
        prop_assert_eq!(
          operation.as_ref().map(|(_, spans)| spans),
          Some(&record.spans),
          "one operation's records name its spans alike"
        );
      } else {
        prop_assert!(
          self.identities.insert(record.identity),
          "an operation's identity met again: {:?}",
          record
        );
        let logs = record.spans.len();
        taken.several_logs += u64::from(logs > 1);
        operation = Some((record.identity, record.spans.clone()));
      }
      taken.records += 1;
      taken.left += self.apply(&record)?;
    }
    Ok(taken)
  }

  /// Applies one record to its log: it must start where the log stands, name its own span among its operation's, and
  /// carry one change of its log a position. How many members left.
  fn apply(&mut self, record: &StateLogRecord) -> Result<u64, TestCaseError> {
    let position = self.positions.entry(record.log).or_insert(0);
    prop_assert_eq!(
      *position,
      record.from,
      "a record continues its log: {:?}",
      record
    );
    prop_assert!(
      record.from < record.to,
      "a record moves its log: {:?}",
      record
    );
    let own = LogSpan {
      log: record.log_name(),
      from: record.from,
      to: record.to,
    };
    prop_assert!(
      record.spans.contains(&own),
      "a record's spans name its own: {:?}",
      record
    );
    prop_assert_eq!(record.changes.len() as u64, record.to - record.from);
    *position = record.to;
    let mut left = 0;
    for change in &record.changes {
      prop_assert_eq!(
        log_of(change),
        record.log,
        "a change of another log: {:?}",
        record
      );
      left += self.change(change)?;
    }
    Ok(left)
  }

  /// One change, as the database's rules apply it: each took a position, so each changes what it names.
  fn change(&mut self, change: &LoggedChange) -> Result<u64, TestCaseError> {
    match change {
      LoggedChange::Joined { member } => self.members.push(member.clone()),
      LoggedChange::Named { change } => self.named(change),
      LoggedChange::Left { green } => {
        self.names.retain(|_, bound| bound != green);
        self.members.retain(|member| member.green != *green);
        return Ok(1);
      }
      LoggedChange::Changed { change } => self.changed(change)?,
    }
    Ok(0)
  }

  fn named(&mut self, change: &NameChange) {
    match change {
      NameChange::Bind { name, green } => {
        self.names.insert(name.clone(), *green);
      }
      NameChange::Retire { name } => {
        self.names.remove(name);
      }
      NameChange::Default { branch } => self.default.clone_from(branch),
    }
  }

  fn changed(&mut self, change: &StateChange) -> Result<(), TestCaseError> {
    match change {
      StateChange::PhaseRootAdded { phase, node } => {
        prop_assert!(self.roots.insert((*phase, node.clone())), "{:?}", change);
      }
      StateChange::PhaseRootRemoved { phase, node } => {
        prop_assert!(self.roots.remove(&(*phase, node.clone())), "{:?}", change);
      }
      StateChange::MarkerAdded { marker } => self.markers.push(marker.clone()),
      StateChange::LockTaken { path, lock } => {
        prop_assert!(
          self.locks.insert(path.clone(), lock.clone()).is_none(),
          "{:?}",
          change
        );
      }
      StateChange::LockReleased { path, lock } => {
        prop_assert_eq!(self.locks.remove(path).as_ref(), Some(lock), "{:?}", change);
      }
    }
    Ok(())
  }

  /// What the copy holds, in the model's terms.
  pub(super) fn view(&self) -> StateView {
    let position = |log| self.positions.get(&log).copied().unwrap_or(0);
    StateView {
      members: self
        .members
        .iter()
        .map(|member| {
          let origin = match member.origin {
            MemberOrigin::Root => None,
            MemberOrigin::Branch { from, version } => Some((index_of(from), version)),
          };
          (index_of(member.green), origin)
        })
        .collect(),
      names: self
        .names
        .iter()
        .map(|(name, green)| (name.clone(), index_of(*green)))
        .collect(),
      default: self.default.clone(),
      positions: [
        position(StateLog::Names),
        position(StateLog::Phases),
        position(StateLog::Markers),
        position(StateLog::Locks),
      ],
      roots: self.roots.clone(),
      markers: self.markers.clone(),
      locks: self.locks.clone(),
    }
  }
}

/// The log a change of a record moves.
fn log_of(change: &LoggedChange) -> StateLog {
  match change {
    LoggedChange::Joined { .. } | LoggedChange::Named { .. } | LoggedChange::Left { .. } => {
      StateLog::Names
    }
    LoggedChange::Changed { change } => match change {
      StateChange::PhaseRootAdded { .. } | StateChange::PhaseRootRemoved { .. } => StateLog::Phases,
      StateChange::MarkerAdded { .. } => StateLog::Markers,
      StateChange::LockTaken { .. } | StateChange::LockReleased { .. } => StateLog::Locks,
    },
  }
}
