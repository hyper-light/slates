//! A repository's state logs shipped to their holders a log at a time (§4.17.1 shared piece 2, the fleet half's live
//! plane; A-636 part 2b; §4.8 "records are sent to **all** candidates; committed at `f + 1`").
//!
//! **What it is.** A repository keeps its own state in four logs of its own (A-642): its names, Mercurial's phase roots
//! and markers, Git LFS's locks. Each is a register of the repository's class (A-636 part 2a,
//! `slates_db::register::RegisterClass::State`), placed on the repository's candidates and taken over by its successor.
//! The partition hands its owner one record of each state log an operation moved, once the operation is durable
//! (`slates_db::replay::Db::take_state_log_records`): the operation's changes of that log, from the position it found the
//! log at to the one it left it at, with the operation's identity and every span it moved, its greens' included
//! (`slates_db::catalog::StateLogRecord`). This module keeps each record as a **part** owed to the log's candidates and
//! answers what each candidate still lacks; the record plane ships each part through the owner's acceptor to the
//! candidates missing it, committed at `f + 1`, as it ships a head (`crate::fleet::ship_head`), and every
//! acknowledgement is kept here.
//!
//! **A log at a time.** Each log is its own register: a part is held by `(register, position)` in its holders' acceptors,
//! and the logs of one operation ship on their own, so a push that moved the names and the locks never waits on one
//! log's holders for the other's. What keeps an operation whole across its logs is in the records themselves: each
//! names the operation's identity and every span it moved, so a takeover keeps the operation where every log it moved
//! holds it and drops it otherwise (A-636 part 1's rule; part 2c applies it).
//!
//! **A ledger, held whole.** A part is a ledger position, never compacted (`crate::catalog::is_single_value`): a
//! successor rebuilds the repository's state from the whole log. A holder holds a part only where it continues its own
//! copy, the part's predecessor held or the log's start ([`check_held`]), so a holder's copy of a log is always a
//! prefix of it, and an owner ships each candidate its parts in order.
//!
//! **Bounded.** A part stays until every candidate holds it. The parts held are bounded by the partition's merge budget
//! ([`bound`]): a repository's change is admitted only while the parts held are under it, refused `Overloaded`
//! otherwise ([`admit`]), so the parts never pass it by more than one operation's records. A laptop (`f = 0`, no remote
//! candidate) keeps none.
//!
//! **A push's records ride the names log** (A-724 piece 8c, found by `crates/cli/tests/v3_takeover.rs`): before, they
//! were kept only in the owner's own partition, so a successor rebuilt a taken-over repository with no refs at all.
//! Each push's record is now a change of the names log (`LoggedChange::Objects`), shipped here with the operation's
//! identity and spans, and a push is answered only once its names log is held at `f + 1` ([`names_committed_within`],
//! the barrier, §4.8's commit rule): the HTTP and SSH edges wait on it before the report goes out.
//!
//! **Owed** (A-636's entry): a candidate
//! that joins a log's placement past parts every earlier candidate held, which needs the log's state installed before
//! its next part (a state-log checkpoint, which also lets holders drop the records below it); the parts rebuilt after
//! the owner restarts; parts batched a shipment; a repository's destroy forgetting its four state logs' registers; and
//! the successor's adoption of the logs (part 2c).

use std::collections::BTreeMap;

use slates_db::catalog::{StateLog, StateLogRecord};
use slates_db::register::{HostId, ObjectId, Record, RegisterError};
use slates_ipc::protocol::Refusal;
use slates_wire::Wire;

use crate::catalog::{STATE_LOG_CLASS, tagged, untagged};
use crate::state::ShardState;

/// The state logs' parts an owner shard still owes some candidate (one field on [`ShardState`]).
#[derive(Debug, Default)]
pub(crate) struct StateLogs {
  /// Each state log's register with parts owed, by register.
  logs: BTreeMap<ObjectId, OwedLog>,
  /// The bytes of every part held: what [`admit`] holds to [`bound`].
  bytes: usize,
}

/// One state log's parts and what its candidates acknowledged.
#[derive(Debug, Default)]
struct OwedLog {
  /// The parts not yet held by every candidate, by the position each leaves the log at (its sequence): each the
  /// register value its record ships as.
  parts: BTreeMap<u64, Vec<u8>>,
  /// Each candidate's highest position acknowledged: a holder holds a prefix of the log ([`check_held`]), so its
  /// highest acknowledgement says every part at or below it is held there.
  acked: BTreeMap<HostId, u64>,
}

impl StateLogs {
  /// Keeps `record` as a part of its log's register, owed to the log's candidates.
  pub(crate) fn hold(&mut self, record: &StateLogRecord) {
    let value = value_of(record);
    self.bytes = self.bytes.saturating_add(value.len());
    let log = self.logs.entry(record.log_name().register()).or_default();
    if let Some(replaced) = log.parts.insert(record.to, value) {
      self.bytes = self.bytes.saturating_sub(replaced.len());
    }
  }

  /// Notes `hosts` holding `register`'s log through `sequence`.
  pub(crate) fn acknowledged(&mut self, register: ObjectId, sequence: u64, hosts: &[HostId]) {
    let Some(log) = self.logs.get_mut(&register) else {
      return;
    };
    for host in hosts {
      let held = log.acked.entry(*host).or_insert(sequence);
      *held = (*held).max(sequence);
    }
  }

  /// Lets go of `register`'s parts every one of `remote`, the log's remote candidates, holds; all of them when there is
  /// none (a laptop). A log with no part left is forgotten with its acknowledgements: a later part ships to every
  /// candidate, each holding the parts before it.
  pub(crate) fn let_go_held(&mut self, register: ObjectId, remote: &[HostId]) {
    let Some(log) = self.logs.get_mut(&register) else {
      return;
    };
    let held_everywhere = remote
      .iter()
      .map(|host| log.acked.get(host).copied().unwrap_or(0))
      .min()
      .unwrap_or(u64::MAX);
    let kept = log.parts.split_off(&held_everywhere.saturating_add(1));
    let released: usize = log.parts.values().map(Vec::len).sum();
    log.parts = kept;
    self.bytes = self.bytes.saturating_sub(released);
    if log.parts.is_empty() {
      self.logs.remove(&register);
    }
  }

  /// Every register with parts owed, each with its parts in position order and the highest position each candidate
  /// acknowledged.
  pub(crate) fn owed(
    &self,
  ) -> impl Iterator<Item = (ObjectId, &BTreeMap<u64, Vec<u8>>, &BTreeMap<HostId, u64>)> + '_ {
    self
      .logs
      .iter()
      .map(|(register, log)| (*register, &log.parts, &log.acked))
  }

  /// The bytes of every part held; only the tests read it.
  #[cfg(test)]
  pub(crate) fn bytes(&self) -> usize {
    self.bytes
  }

  /// Whether the parts held leave room under `bound` for another operation's ([`admit`]).
  pub(crate) fn within(&self, bound: usize) -> bool {
    self.bytes < bound
  }
}

/// A state log's record as its register holds it: behind [`STATE_LOG_CLASS`].
pub(crate) fn value_of(record: &StateLogRecord) -> Vec<u8> {
  tagged(STATE_LOG_CLASS, record.to_bytes())
}

/// The record a state log's register value holds, `None` for a value of another class or one that does not decode.
pub(crate) fn record_of(value: &[u8]) -> Option<StateLogRecord> {
  StateLogRecord::from_bytes(untagged(STATE_LOG_CLASS, value)?).ok()
}

/// Takes the records the partition handed on since the last take into the parts this owner shard owes, and lets go at
/// once of those no remote candidate needs (a laptop's, every one).
pub(crate) fn take_made(state: &mut ShardState) {
  let records = state.db.take_state_log_records();
  if records.is_empty() {
    return;
  }
  let local = state.fleet.host();
  let config = state.fleet.configuration().clone();
  for record in &records {
    state.state_logs.hold(record);
  }
  for record in &records {
    let register = record.log_name().register();
    let remote = remote_candidates(&config, register, local);
    state.state_logs.let_go_held(register, &remote);
  }
}

/// Whether `repository`'s names log is held at `f + 1` through its current position (§4.8's commit rule; A-724 piece
/// 8c, the takeover test's fix): the records the partition handed on taken first ([`take_made`]), so a part not yet
/// taken never reads as held; then `f` of the log's remote candidates acknowledged at or past the position (the owner
/// the `+ 1`), or every remote candidate where there are fewer. A log with no part owed is held: every remote candidate
/// holds it, or it has none (a laptop's).
pub(crate) fn names_committed(
  state: &mut ShardState,
  repository: slates_db::catalog::RepositoryId,
) -> bool {
  take_made(state);
  let Some(position) = state
    .db
    .partition()
    .repositories()
    .log_position(repository, StateLog::Names)
  else {
    return true;
  };
  let register = slates_db::catalog::LogName::State {
    repository,
    log: StateLog::Names,
  }
  .register();
  let local = state.fleet.host();
  let config = state.fleet.configuration();
  let remote = remote_candidates(config, register, local);
  let need = usize::try_from(config.quorum.f)
    .unwrap_or(usize::MAX)
    .min(remote.len());
  state.state_logs.logs.get(&register).is_none_or(|log| {
    log
      .acked
      .iter()
      .filter(|(host, at)| remote.contains(host) && **at >= position)
      .count()
      >= need
  })
}

/// Waits until `repository`'s names log is held at `f + 1` through its position now ([`names_committed`]), re-asking at
/// the record plane's poll within its exchange deadline (`crate::fleet::live_budget`): whether it was. A push is
/// answered only then (A-724 piece 8c: a push acknowledged before its records reached `f + 1` was lost with its owner).
pub(crate) async fn names_committed_within(repository: slates_db::catalog::RepositoryId) -> bool {
  let budget = crate::fleet::live_budget();
  let began = slates_rt::futures::now_ns();
  loop {
    if crate::state::with_state(|state| names_committed(state, repository)) != Some(false) {
      return true;
    }
    if slates_rt::futures::now_ns().saturating_sub(began) >= budget.deadline_ns {
      crate::state::with_state(|state| state.count(PUSH_UNCOMMITTED, 1));
      return false;
    }
    // A refused timer (no shard) ends the wait unheld: the push is answered refused, never acknowledged unheld.
    if slates_rt::futures::sleep(budget.poll_interval_ns.max(1))
      .await
      .is_err()
    {
      return false;
    }
  }
}

/// Counter: pushes answered refused because their records did not reach `f + 1` within the wait (A-724 piece 8c).
/// Format: a counter name.
pub(crate) const PUSH_UNCOMMITTED: &str = "state_log.push_uncommitted";

/// Notes `placement`'s acknowledgements of `register`'s part at `sequence` (the record plane's commit round,
/// `crate::fleet::record_acks_in`), and lets go of the parts every remote candidate then holds.
pub(crate) fn acknowledged(
  state: &mut ShardState,
  register: ObjectId,
  sequence: u64,
  placement: &slates_db::register::Placement,
) {
  let local = state.fleet.host();
  state
    .state_logs
    .acknowledged(register, sequence, &placement.acked);
  let remote = remote_candidates(state.fleet.configuration(), register, local);
  state.state_logs.let_go_held(register, &remote);
}

/// The candidates of `register`'s placement other than `local`.
fn remote_candidates(
  config: &slates_db::register::Configuration,
  register: ObjectId,
  local: HostId,
) -> Vec<HostId> {
  config
    .place(register)
    .candidates
    .into_iter()
    .filter(|candidate| *candidate != local)
    .collect()
}

/// The most bytes of parts an owner shard holds before it refuses a repository's next change.
/// Derived: the partition's merge budget (`PartitionCaps::green_chain_bytes`), the most repository state and chains its
/// partition holds at once: a part is a copy of state changes that budget charged when they were made, so the parts
/// owed past it would be more than a second copy of everything the partition may hold, which only holders gone silent
/// for long can leave unacknowledged.
pub(crate) fn bound(state: &ShardState) -> usize {
  state.db.partition().caps().green_chain_bytes
}

/// Counter: a repository's change refused `Overloaded` because the state logs' parts its owner shard holds reached
/// [`bound`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const STATE_LOG_PARTS_FULL: &str = "repository.state_log.parts_full";

/// Admits a repository's next change only while the parts held are under [`bound`], after taking what the partition
/// handed on since the last take: `Overloaded` naming this shard otherwise, counted, until the record plane's
/// acknowledgements let parts go.
pub(crate) fn admit(state: &mut ShardState) -> Result<(), Refusal> {
  take_made(state);
  if state.state_logs.within(bound(state)) {
    return Ok(());
  }
  state.count(STATE_LOG_PARTS_FULL, 1);
  Err(Refusal::Overloaded { shard: state.shard })
}

/// Whether `op` moves one of a repository's own state logs (A-642): a change of its namespace or lineage, or an entry
/// that joins greens, changes names or moves the repository's state. An entry of greens' versions alone moves none, so
/// it waits on no state log's holders ([`admit`]).
pub(crate) fn moves_state_logs(op: &slates_db::op::Op) -> bool {
  use slates_db::op::Op;
  match op {
    Op::RepositoryBranchAdded { .. }
    | Op::RepositoryBranchRetired { .. }
    | Op::RepositoryDefaultSet { .. }
    | Op::RepositoryGreenJoined { .. } => true,
    Op::RepositoryAdvanced { entry } => {
      !entry.joined.is_empty() || !entry.names.is_empty() || !entry.state.is_empty()
    }
    _ => false,
  }
}

/// Why a holder refused a state log's part ([`check_held`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartRefusal {
  /// Its value is not a state log's record.
  Malformed,
  /// Its record is of another register: another log, or another repository's.
  Misnamed,
  /// Its position is not the one its record leaves its log at, or its record moves nothing.
  Misplaced,
  /// This holder lacks its predecessor: its copy would not be a prefix of the log.
  Discontinuous,
}

impl PartRefusal {
  /// The counter naming it.
  /// Format: refusal names in the daemon's status report.
  pub(crate) fn counter(self) -> &'static str {
    match self {
      PartRefusal::Malformed => "fleet.record.refused.state_log.malformed",
      PartRefusal::Misnamed => "fleet.record.refused.state_log.misnamed",
      PartRefusal::Misplaced => "fleet.record.refused.state_log.misplaced",
      PartRefusal::Discontinuous => "fleet.record.refused.state_log.discontinuous",
    }
  }
}

/// Whether this holder may hold `record`, a part of the state log `log` (its register's class): it must decode as a
/// record of that log's register, sit at the position the record leaves the log at, name that span among its
/// operation's, carry one change a position, and continue this holder's copy, its predecessor held here or the log's
/// start. Refused otherwise, counted by kind, as a malformed record: an empty reply, no acknowledgement, and the owner
/// ships the part again.
pub(crate) fn check_held(
  state: &mut ShardState,
  record: &Record,
  log: StateLog,
) -> Result<(), RegisterError> {
  let part = record_of(&record.value);
  let held = part.as_ref().is_some_and(|part| {
    state
      .holder_records
      .get(&record.object)
      .is_some_and(|acceptor| acceptor.value_at(record.object, part.from).is_some())
  });
  match judge((record.object, record.sequence), part.as_ref(), log, held) {
    Ok(()) => Ok(()),
    Err(refusal) => {
      state.count(refusal.counter(), 1);
      Err(RegisterError::MalformedRecord)
    }
  }
}

/// [`check_held`]'s rule over the part a register holds at `(register, sequence)`, `held` saying whether this holder
/// holds the position the part found its log at.
pub(crate) fn judge(
  (register, sequence): (ObjectId, u64),
  part: Option<&StateLogRecord>,
  log: StateLog,
  held: bool,
) -> Result<(), PartRefusal> {
  let part = part.ok_or(PartRefusal::Malformed)?;
  if part.log != log || part.log_name().register() != register {
    return Err(PartRefusal::Misnamed);
  }
  if part.to != sequence || part.from >= part.to {
    return Err(PartRefusal::Misplaced);
  }
  let own = slates_db::catalog::LogSpan {
    log: part.log_name(),
    from: part.from,
    to: part.to,
  };
  let moved = u64::try_from(part.changes.len()).unwrap_or(u64::MAX);
  if !part.spans.contains(&own) || part.to.checked_sub(part.from) != Some(moved) {
    return Err(PartRefusal::Malformed);
  }
  if part.from > 0 && !held {
    return Err(PartRefusal::Discontinuous);
  }
  Ok(())
}

#[cfg(test)]
mod tests;
