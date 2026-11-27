//! A candidate a death brought in catches up on a register's ledger it holds only part of, or none of, by reading it
//! from its cohort (A-919, main 01a20182's rulings of 2026-11-25; closes state_log's owed transfer to a candidate that
//! joins past parts every earlier candidate held).
//!
//! **Why.** After a death at three members every member is a candidate of every register, so a node that never held a
//! repository's state logs joined their placement. Each part its owner shipped continues the log from a position the
//! newcomer lacks, so it refused every one (`PartRefusal::Discontinuous`), and nothing shipped it the prefix: the
//! owner let each part go once every candidate held it, and a part's acknowledgement from the newcomer never came, so
//! the owner kept every part from then on, toward the partition's bound past which every change to the repository is
//! refused `Overloaded`.
//!
//! **The read.** The ledger question is split into a read and a promise (`slates_db::takeover::read_ledger`; a
//! takeover's `answer_ledger` is the read after its promise). A current candidate of a register, under the
//! configuration generation both have installed, asks the register's other candidates for its positions in a range;
//! each answers what its acceptor holds there, paged, with the position its holding begins at, moving nothing. The
//! owner answers too, from its own hold (`ShardState::owner_hold`), since it counts among `f + 1`. The newcomer folds
//! the answers at its cohort's quorum under the start rule (`slates_db::takeover::ledger_positions_chained`: an
//! answer names nothing only from where its acceptor's holding begins; below it, it counts toward the cohort's size,
//! not its promises), adopting a part only where it continues the last ([`continues`]: its `from` the last part's
//! position, the first the newcomer's own), and holds each adopted position in order (`Acceptor::hold_read`), checked
//! again against its copy (`state_log::judge`). The owner's next shipment then continues it.
//!
//! **Bounded.** One read in flight per register per node, one page a period (`takeover::page_budget`), its range what a
//! refused part named; wanted registers at most this node's held registers. Counted: reads asked, answers, positions
//! and bytes held, and reads past tolerance.
//!
//! **The open edge.** A newcomer whose fold is past tolerance (the owner's hold forgotten at its restart and the
//! departed gone) adopts nothing, refused typed (counted [`PAST_TOLERANCE`]): what committed is the owner's applied
//! state, never the newest surviving older value. It then pins the owner's let-go of the register's parts. The answer
//! is the configuration's (a candidate that cannot hold a register leaving its candidacy), owed.
//!
//! **A successor's missing join** (A-927 part 2, main 01a20182's ruling of 2026-11-25). A takeover adopts a state log's
//! newest part alone; a successor that adopts a family green whose join no names-log part it holds records
//! ([`join_read`]) notes the parts it holds of that log again, then reads the positions it lacks below the log's adopted
//! position from the log's cohort, this same read, and waits for it. It refuses the green (`Unjoined`) only where the
//! read went past tolerance, where it holds the log whole through its adopted position with the join in none of it, or
//! where it holds no names log of the family, each counted by why. Before, the join a takeover adopted into the
//! successor's acceptor, or held only by the other survivor, was never seen, and the green was given up (a894, 3 of 41).

use std::collections::BTreeMap;

use slates_cluster::{CommitBudget, broadcast};
use slates_db::register::{Acceptor, Authority, HostId, LedgerPromise, LedgerRead, ObjectId};
use slates_db::takeover::{LedgerAnswered, ledger_positions_chained, read_ledger};
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;

use crate::fleet::{Dispatch, LateReplies, return_sessions, take_sessions};
use crate::state::{self, ShardState};

/// The stream a ledger read rides (A-919). Format: a stream id past every other fleet stream (home 30, credentials 31).
pub(crate) const LEDGER_READ_STREAM: u64 = 32;

/// Counter: reads this node asked of its cohort. Format: a counter name.
pub(crate) const ASKED: &str = "fleet.ledger_read.asked";
/// Counter: answers it folded. Format: a counter name.
pub(crate) const ANSWERED: &str = "fleet.ledger_read.answered";
/// Counter: positions it held from its reads. Format: a counter name.
pub(crate) const POSITIONS_HELD: &str = "fleet.ledger_read.positions_held";
/// Counter: the bytes of those positions. Format: a counter name.
pub(crate) const BYTES_HELD: &str = "fleet.ledger_read.bytes_held";
/// Counter: reads whose fold was past tolerance, adopting nothing (the open edge). Format: a counter name.
pub(crate) const PAST_TOLERANCE: &str = "fleet.ledger_read.past_tolerance";
/// Counter: a read refused as it was served (not a current candidate, or another generation). Format: a counter name.
pub(crate) const REFUSED: &str = "fleet.ledger_read.refused";
/// Counter: a register a read held positions of that the routing view refused to learn (its owner not a member), so no
/// install raises its acceptor. Format: a counter name.
pub(crate) const UNTRACKED: &str = "fleet.ledger_read.untracked";
/// Counter: an adopted position that did not continue this node's copy, held no further. Format: a counter name.
pub(crate) const DISCONTINUOUS: &str = "fleet.ledger_read.discontinuous";
/// Counter: a successor's adoption of a family green whose join it did not hold, reading its names log's lacking
/// positions from the log's cohort (A-927 part 2, [`join_read`]). Format: a counter name.
pub(crate) const JOIN_WANTED: &str = "fleet.ledger_read.join_wanted";
/// Counter: such an adoption refused (`Unjoined`) because no names log of the green's family is held or adopted here
/// (A-927 part 2). Format: a counter name.
pub(crate) const UNJOINED_NO_LOG: &str = "merge.takeover.adopted_unjoined.no_names_log";
/// Counter: such an adoption refused because its names log's read went past tolerance (A-927 part 2). Format: a counter
/// name.
pub(crate) const UNJOINED_PAST_TOLERANCE: &str = "merge.takeover.adopted_unjoined.past_tolerance";
/// Counter: such an adoption refused because this node holds the names log whole through its adopted position and no
/// part of it records the join: never acknowledged as a member (A-915's case, A-927 part 2). Format: a counter name.
pub(crate) const UNJOINED_LOG_WHOLE: &str = "merge.takeover.adopted_unjoined.log_whole";

/// The registers this node reads a ledger range of (one field of [`ShardState`]): each wanted once its holder refused a
/// part past its copy, at most one per register.
#[derive(Debug, Default)]
pub(crate) struct LedgerReads {
  wanted: BTreeMap<ObjectId, Wanted>,
  /// The registers whose last read went past tolerance (A-927 part 2: a successor's adoption refuses on it, never reads
  /// again for it). Bounded by the registers this node holds an acceptor of: pruned to them as one is added.
  past_tolerance: std::collections::BTreeSet<ObjectId>,
}

/// What a successor's adoption of a family green whose join it does not know does next (A-927 part 2, [`join_read`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinRead {
  /// A part this node holds records the join after all: the adoption goes on.
  Joined,
  /// The names log's lacking positions are being read from its cohort: the adoption waits.
  Reading,
  /// No read can bring the join: the adoption is refused `Unjoined`, counted by this counter's why.
  Unjoined(&'static str),
}

/// One register's read: the range still lacked, and the owner its parts come from.
#[derive(Clone, Copy, Debug)]
struct Wanted {
  owner: HostId,
  from: u64,
  to: u64,
}

/// Notes that this node lacks `register`'s positions from past `held` (none: from its start) through `through`, its
/// parts coming from `owner`: a read of them is wanted, widened where one already is (A-919).
pub(crate) fn want(
  state: &mut ShardState,
  (register, owner): (ObjectId, HostId),
  held: Option<u64>,
  through: u64,
) {
  let from = held.map_or(first_position(register), |held| held.saturating_add(1));
  let to = through.saturating_add(1);
  if from >= to {
    return;
  }
  let wanted = state
    .ledger_reads
    .wanted
    .entry(register)
    .or_insert(Wanted { owner, from, to });
  wanted.owner = owner;
  wanted.from = wanted.from.min(from);
  wanted.to = wanted.to.max(to);
}

/// What a successor's adoption of family green `green`, whose join no membership noted here records, does next (A-927
/// part 2; module docs, "A successor's missing join"): the parts of its family's names log this node holds noted again
/// (a takeover adopts the log's newest part into the acceptor without noting it); then, the join still unknown, a read of
/// the positions this node lacks below the log's adopted position from its cohort ([`want`]), or the refusal's why.
pub(crate) fn join_read(state: &mut ShardState, green: ObjectId) -> JoinRead {
  let Some(register) = names_register_of(state, green) else {
    return JoinRead::Unjoined(UNJOINED_NO_LOG);
  };
  crate::merge_service::note_names_register(state, register);
  if crate::merge_service::membership(state, crate::merge_service::replica_key(green)).is_some() {
    return JoinRead::Joined;
  }
  if state.ledger_reads.past_tolerance.contains(&register) {
    return JoinRead::Unjoined(UNJOINED_PAST_TOLERANCE);
  }
  if state.ledger_reads.wanted.contains_key(&register) {
    return JoinRead::Reading;
  }
  let adopted = state.placed_heads.get(&register).map(|head| head.sequence);
  let highest = state
    .holder_records
    .get(&register)
    .and_then(|acceptor| acceptor.highest(register))
    .map(|(position, _)| position);
  let Some(through) = adopted.max(highest) else {
    return JoinRead::Unjoined(UNJOINED_NO_LOG);
  };
  let held = chained_end(state, register, through);
  if held.is_some_and(|held| held >= through) {
    return JoinRead::Unjoined(UNJOINED_LOG_WHOLE);
  }
  let owner = state
    .fleet
    .object_owner(register)
    .unwrap_or_else(|| state.fleet.host());
  want(state, (register, owner), held, through);
  state.count(JOIN_WANTED, 1);
  JoinRead::Reading
}

/// The names log of `green`'s family this node holds or adopted: the state-log register of class `Names` whose line key is
/// the green's (a repository, its registers and every green minted to join it share one, A-636 part 2b-2).
pub(crate) fn names_register_of(state: &ShardState, green: ObjectId) -> Option<ObjectId> {
  let names = slates_db::register::RegisterClass::State(slates_db::catalog::StateLog::Names);
  let family = green.line_key();
  let is_names = |object: &ObjectId| object.class() == names && object.line_key() == family;
  state
    .holder_records
    .iter()
    .map(|(object, _)| *object)
    .find(is_names)
    .or_else(|| state.placed_heads.keys().copied().find(is_names))
}

/// The position through which this node holds `register`'s parts chained from the log's start (each part's `from` the
/// position of the one before, the first's the log's start), looked at through `through`; `None` where it holds no part
/// from the start.
fn chained_end(state: &ShardState, register: ObjectId, through: u64) -> Option<u64> {
  let acceptor = state.holder_records.get(&register)?;
  let held = acceptor.positions_between(
    register,
    first_position(register),
    through.saturating_add(1),
  );
  let mut last = None;
  for accepted in held {
    let from = crate::state_log::record_of(&accepted.value).map(|part| part.from);
    if from != Some(last.unwrap_or(0)) {
      break;
    }
    last = Some(accepted.sequence);
  }
  last
}

/// A candidate's answer to `peer_host`'s ledger read (A-919): admitted only from a current candidate of the register
/// under the generation this node has installed, and answered from what it holds of it, its holding's start beside
/// (`read_ledger`); as the register's owner, from its own hold. Moves nothing. A refused read is nothing, counted.
pub(crate) fn serve_ledger_read(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Ok(read) = LedgerRead::decode(request) else {
    state.count(REFUSED, 1);
    return Vec::new();
  };
  let configuration = state.fleet.configuration();
  if read.asker != peer_host
    || read.generation != configuration.version
    || !configuration
      .place(read.object)
      .candidates
      .contains(&peer_host)
  {
    state.count(REFUSED, 1);
    return Vec::new();
  }
  let acceptor = state.holder_records.get(&read.object).or_else(|| {
    state
      .owner_hold
      .as_ref()
      .filter(|hold| hold.held_from(read.object).is_some())
  });
  let mut answer = read_ledger(local, acceptor, &read, crate::takeover::page_budget());
  answer.held_from = acceptor.and_then(|acceptor| knowledge_start(read.object, acceptor));
  answer.encode()
}

/// Whether `next` continues the chain of `register` the read adopted (`last`, or before it what this node holds:
/// through the position before `from`), as main 01a20182's ruling makes continuity the fold's one rule (A-919): a state
/// log's part continues where its `from` is the last part's position; another ledger's position is the next one.
fn continues(
  register: ObjectId,
  from: u64,
  last: Option<&slates_db::register::Accepted>,
  next: &slates_db::register::Accepted,
) -> bool {
  match register.class() {
    slates_db::register::RegisterClass::State(_) => {
      let previous = last.map_or(from.saturating_sub(1), |last| last.sequence);
      crate::state_log::record_of(&next.value).is_some_and(|part| part.from == previous)
    }
    _ => next.sequence == last.map_or(from, |last| last.sequence.saturating_add(1)),
  }
}

/// A ledger's first position: a state log's parts are held at the position each leaves the log at, so its first is 1
/// (a part from the log's start, 0); a green's merge chain begins at version 0.
fn first_position(register: ObjectId) -> u64 {
  match register.class() {
    slates_db::register::RegisterClass::State(_) => 1,
    _ => 0,
  }
}

/// Where `acceptor`'s knowledge of `register` begins (the start rule's start, A-919): for a state log, past the `from`
/// of the first part it holds (a part names the position it continues from, so a holder from the log's start knows it
/// from the first position, and its silence there names nothing); for another ledger, its first position held. `None`
/// where it holds none of it.
fn knowledge_start(register: ObjectId, acceptor: &Acceptor) -> Option<u64> {
  let first = acceptor.held_from(register)?;
  match register.class() {
    slates_db::register::RegisterClass::State(_) => acceptor
      .value_at(register, first)
      .and_then(crate::state_log::record_of)
      .map(|part| part.from.saturating_add(1)),
    _ => Some(first),
  }
}

/// One read a period of each register this node wants (A-919): asked of the register's other current candidates, folded
/// with this node's own answer under the start rule, each adopted position held in order. A read past tolerance adopts
/// nothing and ends, counted (the open edge); one that held its whole range ends; any other is asked again next period.
/// Each dispatch kept for the caller.
pub(crate) async fn drive_ledger_reads(local: HostId, budget: CommitBudget) -> Vec<Dispatch> {
  let wanted: Vec<(ObjectId, Wanted)> = state::with_state(|s| {
    s.ledger_reads
      .wanted
      .iter()
      .map(|(register, wanted)| (*register, *wanted))
      .collect()
  })
  .unwrap_or_default();
  let mut dispatches = Vec::new();
  for (register, wanted) in wanted {
    if let Some(dispatch) = read_once(register, wanted, local, budget).await {
      dispatches.push(dispatch);
    }
  }
  dispatches
}

/// One read of `register`'s range `wanted` names: see [`drive_ledger_reads`].
async fn read_once(
  register: ObjectId,
  wanted: Wanted,
  local: HostId,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let (read, candidates, members, quorum, own) = state::with_state(|s| {
    let configuration = s.fleet.configuration();
    let candidates = configuration.place(register).candidates;
    let read = LedgerRead {
      asker: local,
      generation: configuration.version,
      object: register,
      from: wanted.from,
      to: wanted.to,
    };
    let held = s.holder_records.get(&register);
    let mut own = read_ledger(local, held, &read, usize::MAX);
    own.held_from = held.and_then(|acceptor| knowledge_start(register, acceptor));
    (
      read,
      candidates,
      s.council.configuration().members.clone(),
      configuration.quorum,
      own,
    )
  })?;
  let mut answers = BTreeMap::from([(local, answered(own, wanted.to))]);
  let sessions = take_sessions("fleet.lent_to.ledger_read", |host| {
    host != local && candidates.contains(&host)
  });
  if sessions.is_empty() {
    return None;
  }
  let sent: Vec<HostId> = sessions.iter().map(|(host, _)| *host).collect();
  let request = read.encode();
  let outgoing: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.clone(), endpoint))
    .collect();
  let (replied, stragglers) =
    broadcast(outgoing, LEDGER_READ_STREAM, Priority::Control, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    if let Ok(answer) = LedgerPromise::decode(&reply.bytes)
      && answer.holder == host
      && read.answers(&answer)
    {
      answers.insert(host, answered(answer, wanted.to));
    }
    recovered.push((host, endpoint));
  }
  let dispatch = Dispatch::new(sent, &recovered, stragglers, LateReplies::Discard);
  return_sessions(recovered);
  let adoption = ledger_positions_chained(
    (&[candidates], &[]),
    |host| members.contains(&host),
    quorum,
    (wanted.from, wanted.to),
    &answers,
    |last, next| continues(register, wanted.from, last, next),
  );
  let _ = state::with_state(|s| {
    s.count(ASKED, 1);
    s.count(
      ANSWERED,
      u64::try_from(answers.len().saturating_sub(1)).unwrap_or(u64::MAX),
    );
    if adoption.past_tolerance {
      // The open edge: what committed is the owner's applied state, not the newest surviving older value.
      s.count(PAST_TOLERANCE, 1);
      slates_machine::log_line!(
        "slates-server: a read of {register:?} from {} was past tolerance: nothing held, the register's candidacy \
         owed",
        wanted.from
      );
      s.ledger_reads.wanted.remove(&register);
      // Kept for a successor's adoption to refuse on (A-927 part 2), bounded by the registers held here.
      let held = &s.holder_records;
      s.ledger_reads
        .past_tolerance
        .retain(|kept| held.get(kept).is_some());
      s.ledger_reads.past_tolerance.insert(register);
      return;
    }
    let held = hold_adopted(s, (register, wanted.owner), local, &adoption.adopted);
    if adoption.stopped_at.is_none() && held == adoption.adopted.len() {
      s.ledger_reads.wanted.remove(&register);
    }
  });
  Some(dispatch)
}

/// A read's answer as the fold counts it: its positions, the range it covered, its holding's start.
fn answered(answer: LedgerPromise, to: u64) -> LedgerAnswered {
  LedgerAnswered {
    covered_to: answer.next.unwrap_or(to),
    held_from: answer.held_from,
    positions: answer
      .positions
      .into_iter()
      .map(|accepted| (accepted.sequence, accepted))
      .collect(),
  }
}

/// Holds each of `adopted` in order in this node's acceptor of `register` (made under `owner` where it has none, as its
/// first shipped record would make it), each a part continuing the copy before it; stops at the first that does not
/// (counted). How many it held.
fn hold_adopted(
  state: &mut ShardState,
  (register, owner): (ObjectId, HostId),
  local: HostId,
  adopted: &[slates_db::register::Accepted],
) -> usize {
  let generation = state.fleet.configuration().version;
  let log = match register.class() {
    slates_db::register::RegisterClass::State(log) => Some(log),
    _ => None,
  };
  let mut held = 0usize;
  for accepted in adopted {
    if let Some(log) = log {
      let part = crate::state_log::record_of(&accepted.value);
      let continues = part.as_ref().is_some_and(|part| {
        part.from == 0
          || state
            .holder_records
            .get(&register)
            .is_some_and(|acceptor| acceptor.value_at(register, part.from).is_some())
      });
      if crate::state_log::judge((register, accepted.sequence), part.as_ref(), log, continues)
        .is_err()
      {
        state.count(DISCONTINUOUS, 1);
        break;
      }
    }
    if !state.holder_records.contains_key(&register) {
      state.holder_records.insert(
        register,
        Acceptor::new(local, Authority { generation, owner }).tracking_changes(),
      );
    }
    let Some(acceptor) = state.holder_records.get_mut(&register) else {
      break;
    };
    if acceptor.hold_read(register, accepted) {
      state.count(POSITIONS_HELD, 1);
      state.count(
        BYTES_HELD,
        u64::try_from(accepted.value.len()).unwrap_or(u64::MAX),
      );
      // A names-log part's lineage memberships noted for the greens' replicas and adoptions, as a shipped part's are
      // (`fleet::after_held_record`, A-927 part 2).
      crate::merge_service::note_names_part(state, &accepted.value);
    }
    held = held.saturating_add(1);
  }
  // The routing view learns the register as an accepted record makes it learn one (`fleet::hold_checked_record`), so
  // each install raises this acceptor's authority with the others'. Unlearned, it stayed at the generation of the read,
  // and once the takeover's next install moved it, refused every part its owner shipped `ForeignGeneration` for good
  // (1 run in 6 of the A-919 fleet test, the owner's parts kept at 933 bytes).
  if held > 0 {
    let configuration = state.council.configuration().clone();
    if state
      .fleet
      .track_object(register, owner, &configuration)
      .is_err()
    {
      state.count(UNTRACKED, 1);
    }
  }
  held
}
