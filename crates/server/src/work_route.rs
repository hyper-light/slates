//! A work's route ends once nothing can use it (A-288, the end A-255 owed; §4.16 "Commit", §4.8 "Lookup"; CLAUDE.md §2
//! item 8).
//!
//! **What it is.** After its owner's takeover a work has no owner (its head is never placed), and its catalog record, which
//! names its green, is the route a resend of one of its submits takes to the green's owner, where the submit's completion
//! is (A-255). The survivor that adopted the record keeps it placed (`fleet::owed_adopted_work_catalogs`). Before this,
//! nothing retired it: only a volume's owner writes its tombstone, and a work never comes back to have one, so every work
//! alive at an owner's death was held, by its adopter and every holder, for as long as the fleet ran.
//!
//! **The rule.** A route is kept exactly while the green's owner retains a completion of one of the work's submits that a
//! resend can reach only through the route, since only such a completion can answer a resend the route carries. RIFL
//! keeps a completion record until its client acknowledges it and moves it with its object (Lee et al., SOSP 2015, §3),
//! so the route's life is the life of those records. A resend reaches its completion through the route only when its
//! client is on another node than the work's owner: a client of the owner's own node resends to that node, which answers
//! from the records it made (its partition still holds the work), and if the node is gone, so is the client. So the
//! submit's merge record names its work only for a forwarded submit (`merge_service::SubmitCompletion::work`), which
//! matters most for the commonest work, one its own owner's clients submitted from. The green's successor records each
//! such completion it adopts under that work, durably (`CompletionRecord::work`), and its partition counts what each
//! work still has retained (`Partition::completions_routed_by`); the acknowledgement that releases the last ends the
//! count. The route's adopter asks the green's owner every record period, for each work whose route it keeps, whether it
//! still holds one ([`Question`] on [`STREAM`]). A work answered "none" is retired: moved from the adopter's pending
//! catalogs to its pending tombstones, so the two stages AUD-29-43 ships for a destroyed volume (`crate::tombstone`) drop
//! every holder's copy of the work's registers, and the adopter's own copies go when the tombstone retires.
//!
//! **What is never answered "none".** Only the green's shard answers, and only once its count is final: once it serves
//! the green, since the takeover recorded every adopted completion in the same turn the green began to be served
//! (`verbs::adopt_taken_over_green`), or once the green is `unrecovered`: a takeover given up after the failover
//! SLO's attempts (`fleet::give_up_taken_over`) recorded none and never will, and a recovery that could not rebuild the
//! green left its partition's records as they were. A green pending materialization, owned elsewhere or not found gives
//! no answer, and the route stays to be asked again the next period; so does a question refused or unanswered.
//!
//! **Bounds.** A question names at most [`WORKS_PER_QUESTION`] works, so its encoding and the answering shard's work are
//! bounded; a node keeps at most the work catalogs it adopted, each asked once a period. An owner that leaves a question
//! unanswered is not asked its green's other questions that period, so a pass spends at most one liveness budget on it.
//! The keeper runs on its own task, parked while this node keeps no route ([`nudge`]), so a slow question never holds the
//! record plane's coordinator, and an idle node takes no wakeup for it.
//!
//! **Not yet.** A green destroyed, or lost to a second takeover it cannot survive (GAP-A9-7), while a route to it is
//! kept leaves that route held: the green has no owner, so no answer comes; so does a given-up green that is then
//! destroyed, which leaves it neither served nor `unrecovered`. A completion its client never acknowledges (a client
//! of another node that died, or one that never writes to the green again) keeps its route as it keeps the record
//! itself: only an acknowledgement or a client lease ends either, and RIFL's client leases are not built. And an
//! adopter that restarts warm gets its holder copies back but not the routes it kept (`pending_catalogs` starts
//! empty), so it neither re-places nor retires them: A-255's own gap, found by reading (GAPS, A-288's entry).

use std::collections::BTreeMap;

use slates_db::catalog::VolumeId as DbVolumeId;
use slates_db::register::{HostId, ObjectId};
use slates_wire::Wire;

use crate::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS};
use crate::fleet::FLEET_FRAME_CAP;
use crate::owner_location::{CachedRoute, Query};
use crate::state::{self, ShardState};

/// Format: the fleet stream a route's question rides, after the death reports (24).
pub(crate) const STREAM: u64 = 25;

/// Format: a volume id's bytes, as a question names a green and its works.
const VOLUME_ID_BYTES: usize = 16;
/// Format: the wire's length prefix of a list (a `u32`).
const COUNT_BYTES: usize = 4;
/// Derived: the works one question names, at most: as many as fit one fleet frame beside the green and the list's count
/// (`FLEET_FRAME_CAP`, the packet budget every fleet message is sized by), so a question is one frame and the answering
/// shard does a bounded turn of lookups for it. A green whose adopter keeps more routes is asked in several questions.
pub(crate) const WORKS_PER_QUESTION: usize =
  (FLEET_FRAME_CAP - VOLUME_ID_BYTES - COUNT_BYTES) / VOLUME_ID_BYTES;

/// The register sequence a retired work's tombstone is written at (AUD-29-43): one past the head register's creation
/// sequence. A work's head register is never written (its head is never placed), so the tombstone is its first and
/// newest value: the sequence the work's own destroy would have closed its registers at
/// (`Partition::tombstone_sequence`, its epoch, 0 for a work, plus one).
const WORK_TOMBSTONE_SEQUENCE: u64 = crate::verbs::CREATION_HEAD_SEQUENCE + 1;

/// Counter: a work's route retired, its owner holding no completion of the work's submits (the non-vacuity count of
/// this module). Format: a counter name in the daemon's status report.
pub(crate) const RETIRED: &str = "fleet.work_route.retired";
/// Counter: a question asked of a green's owner (one per green and chunk of its works, each period).
const ASKED: &str = "fleet.work_route.asked";
/// Counter: a work answered still held: its route stays.
const HELD: &str = "fleet.work_route.held";
/// Counter: a question the green's owner was not found for this period; asked again the next.
const OWNER_UNKNOWN: &str = "fleet.work_route.owner_unknown";
/// Counter: a question that came back with no answer, or with one that does not bind it; asked again the next period.
const UNANSWERED: &str = "fleet.work_route.unanswered";
/// Counter: on the asked node, a question it did not answer: it does not serve the green (pending, elsewhere, gone).
const NOT_SERVED_HERE: &str = "fleet.work_route.not_served_here";
/// Counter: on the asked node, a question that does not decode or names more than [`WORKS_PER_QUESTION`] works.
const MALFORMED: &str = "fleet.work_route.malformed";

/// The question a work route's adopter asks its green's owner: which of these works of the green it still holds a
/// completion of. A fixed-size green and a list checked against the bytes before anything is allocated, so a hostile
/// question costs at most its own length.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct Question {
  /// The green.
  green: [u8; VOLUME_ID_BYTES],
  /// The works asked about, at most [`WORKS_PER_QUESTION`].
  works: Vec<[u8; VOLUME_ID_BYTES]>,
}

/// The green's owner's answer: the question it answers, so an answer is never taken for another, and whether it holds a
/// completion of each of its works, in order.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct Answer {
  /// The question answered.
  question: Question,
  /// Per work of the question, in order: whether a completion of its submits is held.
  held: Vec<bool>,
}

/// The keeper's waker and whether an adoption gave it work since it last looked (on the control shard).
#[derive(Default)]
pub(crate) struct WorkRoutes {
  /// Whether a work's route was adopted since the keeper last looked.
  nudged: bool,
  /// The keeper's waker while it is parked.
  waker: Option<std::task::Waker>,
}

/// Wakes the keeper: a takeover adopted a work's catalog record, a route this node now keeps (`takeover::record_adoption`).
/// Called on the control shard, where the keeper runs; a test that plants a route there calls it as the adoption does.
pub fn nudge(state: &mut ShardState) {
  state.work_routes.nudged = true;
  if let Some(waker) = state.work_routes.waker.take() {
    waker.wake();
  }
}

/// Ready once a route was adopted since the keeper last looked; the nudge is consumed. A look the state refused (a nested
/// borrow, or a retention check that discarded the look after it took the nudge; counted) is ready too: parked then, the
/// keeper would hold no waker and never look again, and the routes it keeps would stay held for good. A pass that
/// cannot reach the state keeps looking each period ([`keep_routes`]).
async fn nudged() {
  std::future::poll_fn(|cx| {
    state::with_state_counted(|s| {
      if std::mem::take(&mut s.work_routes.nudged) {
        std::task::Poll::Ready(())
      } else {
        s.work_routes.waker = Some(cx.waker().clone());
        std::task::Poll::Pending
      }
    })
    .unwrap_or(std::task::Poll::Ready(()))
  })
  .await;
}

/// The route keeper (module docs): parked until this node adopts a work's route, then each record period asks the
/// greens' owners about every route it keeps and retires the ones nothing can use, until it keeps none.
pub(crate) async fn run(local: HostId) {
  let mut routes: BTreeMap<ObjectId, CachedRoute> = BTreeMap::new();
  loop {
    nudged().await;
    while keep_routes(local, &mut routes).await > 0 {
      crate::daemon::pace(HEARTBEAT_NS).await;
    }
    routes.clear();
  }
}

/// One pass: every work route this node keeps, grouped by its green, asked about and retired where the green's owner
/// holds none of its completions. Returns how many routes it still keeps. `routes` keeps each green's located owner
/// across passes, so a pass runs a location round only for a green whose owner it has not found or no longer reaches.
/// A green whose owner leaves one question unanswered is not asked its other chunks this pass, so a pass spends at most
/// one liveness budget on an owner that does not answer, however many routes lead to it.
async fn keep_routes(local: HostId, routes: &mut BTreeMap<ObjectId, CachedRoute>) -> usize {
  // A state out of reach this pass (a lost borrow, counted) is no evidence that nothing is kept: look again next period.
  let Some(kept) = state::with_state_counted(|s| kept_routes(s, local)) else {
    return 1;
  };
  routes.retain(|green, _| kept.contains_key(green));
  let mut keeping = 0usize;
  for (green, works) in kept {
    let mut answering = true;
    for chunk in works.chunks(WORKS_PER_QUESTION.max(1)) {
      let held = if answering {
        let question = Question {
          green: green.0,
          works: chunk.iter().map(|work| work.0).collect(),
        };
        ask(local, green, &question, routes).await
      } else {
        None
      };
      let still = match held {
        Some(held) => retire_unheld(chunk, &held),
        None => {
          answering = false;
          chunk.len()
        }
      };
      keeping = keeping.saturating_add(still);
    }
  }
  keeping
}

/// The work routes this node keeps, by green: the work catalogs it adopted and owns, each naming its green.
fn kept_routes(state: &ShardState, local: HostId) -> BTreeMap<ObjectId, Vec<ObjectId>> {
  let mut kept: BTreeMap<ObjectId, Vec<ObjectId>> = BTreeMap::new();
  for (work, (_, catalog)) in &state.pending_catalogs {
    let Some(green) = catalog.work_of else {
      continue;
    };
    if state.fleet.object_owner(work.catalog()) == Some(local) {
      kept.entry(ObjectId(green.bytes)).or_default().push(*work);
    }
  }
  kept
}

/// Retires each work of `works` its green's owner holds no completion of, and counts the rest held: how many it keeps.
/// A state out of reach retires none and keeps them all, to be asked again (a lost borrow is counted).
fn retire_unheld(works: &[ObjectId], held: &[bool]) -> usize {
  state::with_state_counted(|s| {
    let mut keeping = 0usize;
    for (work, held) in works.iter().zip(held) {
      if *held {
        s.count(HELD, 1);
        keeping = keeping.saturating_add(1);
      } else {
        retire(s, *work);
      }
    }
    keeping
  })
  .unwrap_or(works.len())
}

/// Retires `work`'s route on the adopter (module docs): no longer kept placed as a catalog (`pending_catalogs`, and the
/// control shard's placement of the catalog), it is owed as a destroyed work's tombstone instead, which the record plane
/// records on the work's own partition and ships in its two stages (`fleet::adopt_pending_tombstones`, which bounds the
/// attempts to record it as it bounds an adopted tombstone's).
fn retire(state: &mut ShardState, work: ObjectId) {
  if state.pending_catalogs.remove(&work).is_none() {
    return;
  }
  state.placed_heads.remove(&work.catalog());
  state.pending_tombstones.insert(
    work,
    crate::state::PendingTombstone::adopted(WORK_TOMBSTONE_SEQUENCE),
  );
  state.count(RETIRED, 1);
}

/// Asks `green`'s owner which of `question`'s works it holds a completion of: answered on this node's shard that serves
/// the green when this node owns it, else over the owner's record session within a liveness budget. `None` when the
/// owner was not found, did not answer, or answered something that does not bind the question; counted.
async fn ask(
  local: HostId,
  green: ObjectId,
  question: &Question,
  routes: &mut BTreeMap<ObjectId, CachedRoute>,
) -> Option<Vec<bool>> {
  state::with_state(|s| s.count(ASKED, 1));
  let Some(owner) = green_owner(local, green, routes).await else {
    state::with_state(|s| s.count(OWNER_UNKNOWN, 1));
    return None;
  };
  let answered = if owner == local {
    let control = state::with_state(|s| s.shard)?;
    answer_on_green_shard(control, question.clone()).await
  } else {
    crate::fleet::request_peer(
      owner,
      STREAM,
      &question.to_bytes(),
      crate::fleet::live_budget(),
    )
    .await
    .and_then(|bytes| Answer::from_bytes(&bytes).ok())
    .filter(|answer| answer.question == *question && answer.held.len() == question.works.len())
    .map(|answer| answer.held)
  };
  if answered.is_none() {
    // The owner may have moved: the next pass locates it again.
    routes.remove(&green);
    state::with_state(|s| s.count(UNANSWERED, 1));
  }
  answered
}

/// Where `green`'s owner is: this node, or a member found from the route `routes` kept, the green's live creator, or one
/// location round (`owner_location`, its own bound and counters). `None` when none is found this pass.
async fn green_owner(
  local: HostId,
  green: ObjectId,
  routes: &mut BTreeMap<ObjectId, CachedRoute>,
) -> Option<HostId> {
  let cached = routes.get(&green).copied();
  let (here, known, query) = state::with_state(|s| {
    let region = s.node_regions.get(&local)?.0;
    let query = Query::new(s, green, region);
    Some((
      s.fleet.object_owner(green) == Some(local),
      query.known_owner(s, cached),
      query,
    ))
  })
  .flatten()?;
  if here {
    return Some(local);
  }
  let owner = match known {
    Some(owner) => owner,
    None => crate::owner_location::locate(query).await.ok()?,
  };
  routes.insert(green, query.route(owner));
  Some(owner)
}

/// Serves a question from a route's adopter (on this node's control shard, as a child task of the peer's serve loop, so
/// the call to the green's shard holds no other request of the peer): the answer's bytes, or empty when this node does
/// not serve the green or the question is malformed, each counted.
pub(crate) async fn serve(control: u16, request: &[u8]) -> Vec<u8> {
  let question = match Question::from_bytes(request) {
    Ok(question) if question.works.len() <= WORKS_PER_QUESTION => question,
    _ => {
      state::with_state(|s| s.count(MALFORMED, 1));
      return Vec::new();
    }
  };
  match answer_on_green_shard(control, question.clone()).await {
    Some(held) => Answer { question, held }.to_bytes(),
    None => Vec::new(),
  }
}

/// Answers `question` on the shard the green's id routes to, within a liveness budget of that shard's own time.
async fn answer_on_green_shard(control: u16, question: Question) -> Option<Vec<bool>> {
  let partition = crate::verbs::owner_of(slates_ipc::protocol::VolumeId {
    bytes: question.green,
  });
  let shard = state::with_state(|s| crate::verbs::shard_of_partition(s, partition)).flatten()?;
  crate::xshard::call_in_shard_time(
    control,
    shard,
    move |s| held_completions(s, &question),
    LIVENESS_BUDGET_NS,
  )
  .await
  .flatten()
}

/// On the green's shard: for each work of `question`, whether this partition retains a completion of its submits that
/// only its route reaches, or `None` (counted) when this shard does not answer for the green as its owner, since then
/// what it retains says nothing of what the green's owner holds (module docs). It answers for a green it serves, and for
/// one its takeover gave up on or its recovery could not rebuild (`unrecovered`): neither records an adopted completion
/// from then on, so the partition's count is final. A green whose takeover is still pending is neither, and is refused.
fn held_completions(state: &mut ShardState, question: &Question) -> Option<Vec<bool>> {
  let green = DbVolumeId {
    bytes: question.green,
  };
  let owner = state.fleet.object_owner(ObjectId(question.green));
  let answers = state.greens.contains_key(&green) || state.unrecovered.contains_key(&green);
  if !answers || owner.is_some_and(|owner| owner != state.fleet.host()) {
    state.count(NOT_SERVED_HERE, 1);
    return None;
  }
  let partition = state.db.partition();
  Some(
    question
      .works
      .iter()
      .map(|work| partition.completions_routed_by(DbVolumeId { bytes: *work }) > 0)
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  fn question(works: usize) -> Question {
    Question {
      green: [3u8; VOLUME_ID_BYTES],
      works: (0..works)
        .map(|index| [u8::try_from(index % 251).unwrap_or(0); VOLUME_ID_BYTES])
        .collect(),
    }
  }

  /// A-288: do: encode the largest question and its answer. Expect: the question fits one fleet frame and encodes the
  /// same bytes twice, and each decodes back to itself.
  #[test]
  fn the_largest_question_fits_one_frame_and_it_and_its_answer_round_trip() {
    let asked = question(WORKS_PER_QUESTION);
    let bytes = asked.to_bytes();
    assert!(bytes.len() <= FLEET_FRAME_CAP, "{} bytes", bytes.len());
    assert_eq!(bytes, asked.to_bytes());
    assert_eq!(Question::from_bytes(&bytes), Ok(asked.clone()));
    let answer = Answer {
      held: vec![true; asked.works.len()],
      question: asked,
    };
    assert_eq!(Answer::from_bytes(&answer.to_bytes()), Ok(answer));
  }

  /// A-288, hostile input (§4.9): do: decode every truncation of a question, one with a trailing byte, and one whose
  /// count claims more works than its bytes hold. Expect: each refused, never a panic, nothing allocated for the claim.
  #[test]
  fn a_truncated_padded_or_overclaiming_question_is_refused() {
    let bytes = question(5).to_bytes();
    for cut in 0..bytes.len() {
      assert!(
        Question::from_bytes(bytes.get(..cut).unwrap_or_default()).is_err(),
        "cut {cut}"
      );
    }
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(Question::from_bytes(&padded).is_err());
    let mut overclaiming = bytes;
    if let Some(count) = overclaiming.get_mut(VOLUME_ID_BYTES..VOLUME_ID_BYTES + COUNT_BYTES) {
      count.copy_from_slice(&u32::MAX.to_le_bytes());
    }
    assert!(Question::from_bytes(&overclaiming).is_err());
  }
}
