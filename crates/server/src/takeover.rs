//! The takeover of a retired host's objects (§4.8 "Promotion and takeover"): "each new owner runs phase one in
//! one batched round per register class across the neighbourhood: every holder raises its fence for that host
//! to the new epoch and reports the highest record it holds for each object; the new owner adopts the newest
//! reported record per object … completes safe adoption under the new epoch …, and only then serves under
//! confirmed authority." This is Vertical Paxos II with the owner as leader-acceptor (§4.8 mechanism 1), and
//! everything a takeover decides comes from the committed regional configuration, so every node decides alike:
//!
//! - **Who owns what.** When an install retires a host, each holder resolves the owner of every object it
//!   holds from the configuration's lineage ([`RegionalConfiguration::lineage`]): the retirement ranks each
//!   object's successor among the members it left (its survivors), and a successor that itself retired before
//!   confirming carries the object into its own takeover. A copy whose line ends in a takeover already done is
//!   stale and reclaimed ([`resolve_held_objects`]).
//! - **Phase one, one round per retired host.** Every survivor that owes a confirmation of the retired host's
//!   takeover asks every other member of its recovery neighbourhoods for the records it holds of that host's
//!   objects whose successor is the asker, in pages of one fresh session's first credit ([`page_budget`]). A
//!   holder raises its fence and installs the asker's authority on each object it lists before answering
//!   ([`serve_host_prepare`]). Pages list what the holder's routing says falls to the asker, so a holder's silence
//!   about an object says nothing (A-251): a holder that lost a copy's routing leaves it out of a complete page. So once
//!   a member's pages are complete the successor asks it **by name** about each object it learned that the member did
//!   not list, and the member answers each alone from its own records: its highest record, or that it holds nothing of
//!   it ([`slates_db::takeover::answer_by_name`]). That named "nothing" is the empty promise a takeover needs when a
//!   survivor never received the object
//!   (`docs/bugs/2026-09-29-a-takeover-stalled-when-a-survivor-never-received-the-head.md`): the retired host is no
//!   member, so no holder accepts its records, and the successor is the only member the configuration names for it.
//! - **Adoption.** An object is adopted once `f + 1` members of each of its recovery cohorts have promised, so
//!   the adoption meets every record any of them committed; the newest reported record is re-committed under
//!   the successor's own placement at the round's epoch, and the successor then serves it.
//! - **Confirmation.** A successor confirms its share once every member it asked has answered in full and every
//!   object it learned is adopted or cannot be recovered; the council records it ([`crate::fleet`]'s report
//!   stream), and a retirement is dropped once every survivor has confirmed.
//!
//! The holder side also applies the owner lease's gate (AUD-08): it promises nothing while its own answers may
//! still feed the retired owner's lease ([`crate::lease::AnswersGiven::promotion_open`]).

use std::collections::{BTreeMap, BTreeSet};

use slates_cluster::{ClusterError, CommitBudget, broadcast, commit_record};
use slates_db::register::{
  Accepted, Acceptor, Authority, HostEpoch, HostId, HostPrepare, HostPromise, Lineage, ObjectId,
  Placement, Record, RegionalConfiguration, RegisterError, encode_refusal,
};
use slates_db::takeover::{
  CountingRule, Heard, Readiness, answer_by_name, promise_object, readiness,
};
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;

use crate::fleet::{Dispatch, FLEET_FRAME_CAP, LateReplies, return_sessions, take_sessions};
use crate::head::{HeadValue, PlacedHead};
use crate::state::{self, ShardState};

/// Format: the stream id a successor's takeover page rides on a record session — distinct from every other
/// stream the session multiplexes (1–15), so `serve_peer_records` dispatches it by its stream.
pub(crate) const TAKEOVER_STREAM: u64 = 16;

/// The status refusal count under which a holder records a takeover page it refused: bytes that are no
/// request, a request whose asker is not the session's peer, or one naming a host this holder has no kept
/// retirement for. Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const TAKEOVER_REFUSED: &str = "fleet.takeover.refused";
/// The status count of takeover pages and adopted records a holder refused `ConfigurationStale`, each of which made this
/// node fetch the committed configuration. Format: a refusal name in the daemon's status report.
const TAKEOVER_STALE_CONFIGURATION: &str = "fleet.takeover.stale_configuration";
/// The status count of takeover rounds refused because no epoch could supersede what they must fence — a
/// holder's fence or a held record at `u64::MAX` (AUD-29-26's sibling): the round never reuses an epoch.
const TAKEOVER_EPOCH_EXHAUSTED: &str = "fleet.takeover.epoch_exhausted";
/// The status count of takeover pages a holder did not answer because the round's configuration differs from the
/// one it installed: refused `ConfigurationStale` when the round's is older, unanswered (the holder fetches) when it
/// is newer, or when the holder's council has not installed its own placement's version yet. Format: a refusal name in
/// the daemon's status report.
const TAKEOVER_PAGE_UNMATCHED: &str = "fleet.takeover.page_unmatched";
/// The status count of takeover periods whose page requests went unsent: every member asked was lent elsewhere or had
/// no session. Format: a refusal name in the daemon's status report.
const TAKEOVER_PAGES_UNSENT: &str = "fleet.takeover.pages_unsent";
/// The status count of page answers a successor could not bind to its request (no promise, another holder's, or
/// another round's), which count as no promise. Format: a refusal name in the daemon's status report.
const TAKEOVER_PAGE_UNBOUND: &str = "fleet.takeover.page_unbound";
/// The status count of takeover periods skipped because this node's placement is behind its council's configuration.
/// Format: a refusal name in the daemon's status report.
const TAKEOVER_ROUND_BEHIND: &str = "fleet.takeover.round_behind";
/// The status count of adoptions this period that did not place: the re-committed record reached too few candidates.
/// The object stays learned and is adopted again next period. Format: a refusal name in the daemon's status report.
const TAKEOVER_ADOPT_UNPLACED: &str = "fleet.takeover.adopt_unplaced";
/// Of [`TAKEOVER_ADOPT_UNPLACED`]: this node's own hold of the adopted record was refused or is not a candidate.
const TAKEOVER_ADOPT_LOCAL_REFUSED: &str = "fleet.takeover.adopt_unplaced.local";
/// Of [`TAKEOVER_ADOPT_UNPLACED`]: no other candidate's session could be taken for the commit.
const TAKEOVER_ADOPT_NO_HOLDERS: &str = "fleet.takeover.adopt_unplaced.no_holders";
/// Of [`TAKEOVER_ADOPT_UNPLACED`]: the candidates taken answered without a binding acknowledgement.
const TAKEOVER_ADOPT_UNACKED: &str = "fleet.takeover.adopt_unplaced.unacked";
/// The status count of learned objects a period left unadopted because a recovery cohort had not promised them yet.
/// Format: a refusal name in the daemon's status report.
const TAKEOVER_UNPROMISED: &str = "fleet.takeover.unpromised";

/// The status refusal count under which a holder records a takeover page it deferred because its own answers
/// to the retired owner's probes may still feed that owner's lease (§4.8 "Leases and reads", AUD-08); the
/// successor asks again next period. Format: a refusal name in the daemon's status report.
pub(crate) const PROMOTION_DEFERRED: &str = "fleet.promotion.deferred";

/// The status count under which a holder records a stale copy it dropped: its object's line ends in a
/// takeover already done, or its successor has confirmed and this holder is outside the successor's placement.
/// Format: a refusal name in the daemon's status report.
const TAKEOVER_RECLAIMED: &str = "fleet.takeover.reclaimed";

/// The status count under which a successor records an object it cannot recover: one of its recovery cohorts
/// has fewer than `f + 1` members left. Format: a refusal name in the daemon's status report.
const TAKEOVER_LOST: &str = "fleet.takeover.lost";

/// The status count under which a successor records an object it adopted and placed under itself.
/// Format: a refusal name in the daemon's status report.
const TAKEOVER_ADOPTED: &str = "fleet.takeover.adopted";

/// Derived: the bytes one takeover page may carry — the credit a fresh fleet session grants before any window
/// update (`slates_transport::connection::initial_receive_window` at the fleet frame cap: the reorder
/// threshold plus one packets of stream data), so a page reaches the successor in one flight on any session,
/// however new; the session's window grows past it on its own. A single entry larger than it still goes, alone.
pub(crate) fn page_budget() -> usize {
  usize::try_from(slates_transport::connection::initial_receive_window(
    FLEET_FRAME_CAP,
  ))
  .unwrap_or(usize::MAX)
}

/// One survivor's takeover of one retired host's objects, kept on the control shard while it owes the council
/// a confirmation of its share. Bounded: one per kept retirement; its pages by the members asked; its learned
/// objects by the retired host's objects whose successor this node is, each dropped once adopted.
pub struct HostTakeover {
  /// The configuration version this round runs under; an install of another starts the round over.
  generation: u64,
  /// The epoch this node writes the adopted objects under — the ballot the holders promise.
  epoch: HostEpoch,
  /// Each asked member's progress through its answer.
  pages: BTreeMap<HostId, Page>,
  /// The objects learned and not yet adopted, each with the records its holders reported (this node's among
  /// them).
  learned: BTreeMap<ObjectId, BTreeMap<HostId, Accepted>>,
  /// The objects adopted and placed under this node.
  adopted: BTreeSet<ObjectId>,
  /// The objects no survivor can recover.
  lost: BTreeSet<ObjectId>,
  /// The highest fence a holder reported above the epoch; the next round runs above it.
  fenced: Option<HostEpoch>,
  /// Per learned object, the members that answered for it by name holding nothing of it (A-251 (b)): their promise of
  /// that object alone. A member's complete pages promise nothing of an object they did not list.
  named_nothing: BTreeMap<ObjectId, BTreeSet<HostId>>,
}

/// One asked member's progress through its answer: the object its next page starts after, and whether its
/// answer is complete.
#[derive(Clone, Copy, Default)]
struct Page {
  after: Option<ObjectId>,
  complete: bool,
}

impl HostTakeover {
  /// A round under `generation` at `epoch`, having asked nothing yet.
  fn new(generation: u64, epoch: HostEpoch) -> HostTakeover {
    HostTakeover {
      generation,
      epoch,
      pages: BTreeMap::new(),
      learned: BTreeMap::new(),
      adopted: BTreeSet::new(),
      lost: BTreeSet::new(),
      fenced: None,
      named_nothing: BTreeMap::new(),
    }
  }

  /// Starts the round over at `generation` and `epoch`: every promise was for the old round, so every page is
  /// asked again. What was adopted stays adopted.
  fn restart(&mut self, generation: u64, epoch: HostEpoch) {
    self.generation = generation;
    self.epoch = epoch;
    self.pages.clear();
    self.learned.clear();
    self.lost.clear();
    self.fenced = None;
    self.named_nothing.clear();
  }

  /// The objects learned and not yet adopted.
  pub fn outstanding(&self) -> usize {
    self.learned.len()
  }
}

/// Records `owner` as `object`'s owner in this shard's routing and in every other shard's (§4.8 "Lookup"). A takeover
/// runs on the control shard, and every shard decides from its own routing whether a volume is this node's: a client
/// whose request was served on another shard was refused its own node's adopted volume `OwnedByAnotherHost`, the
/// shard's routing still naming the dead creator (main's macOS fleet-deploy test, run 42206043007; counted
/// `attach.foreign_refused.*`). A shard the message does not reach (its admission bound) is counted
/// (`xshard.run_refused`) and learns the owner as before, from a lookup.
pub(crate) fn track_owner_on_every_shard(state: &mut ShardState, object: ObjectId, owner: HostId) {
  state.fleet.track_object_owner(object, owner);
  let origin = state.shard;
  for shard in state
    .shards
    .iter()
    .copied()
    .filter(|shard| *shard != origin)
  {
    crate::xshard::run_on_counted(origin, shard, move |s| {
      s.fleet.track_object_owner(object, owner);
    });
  }
}

/// Brings every object this holder holds to the owner the committed configuration names for it (§4.8), run on
/// the control shard at each install, before the held acceptors' authority is reconciled to their owners:
///
/// - an object whose owner a kept retirement names is moved to the successor its lineage ranks, and remembers
///   the retired host whose takeover it is recovered in (`departed_owners`, the lease gate's record);
/// - a copy whose line ends in a takeover already done is dropped, as is one whose successor has confirmed its
///   share while this holder is outside the successor's placement (the successor's adoption never reaches it);
/// - an object no survivor can recover is left as it is, counted by the successor that learns it.
pub(crate) fn resolve_held_objects(state: &mut ShardState, local: HostId) {
  let regional = state.council.configuration().clone();
  let held: Vec<ObjectId> = state.holder_records.keys().copied().collect();
  for object in held {
    let Some(owner) = state.fleet.object_owner(object) else {
      continue;
    };
    if regional.members.contains(&owner) && !regional.retired.contains_key(&owner) {
      if stale_after_confirmation(state, &regional, object, owner, local) {
        reclaim(state, object);
      }
      continue;
    }
    match regional.lineage(owner, object) {
      Lineage::Successor {
        departed,
        successor,
      } => {
        track_owner_on_every_shard(state, object, successor);
        let since_version = regional
          .retired
          .get(&departed)
          .map_or(regional.version, |retirement| retirement.version);
        state.departed_owners.insert(
          object,
          crate::lease::DepartedOwner {
            owner: departed,
            since_version,
          },
        );
      }
      Lineage::Lost => {}
      Lineage::Settled => reclaim(state, object),
    }
  }
}

/// Whether this holder's copy of `object`, now owned by the member `owner` after a takeover, is stale: its
/// retired host's takeover names `owner` among the confirmed, so `owner` adopted it, and this holder is
/// outside `owner`'s placement of it, so the adoption never reaches this copy.
fn stale_after_confirmation(
  state: &ShardState,
  regional: &RegionalConfiguration,
  object: ObjectId,
  owner: HostId,
  local: HostId,
) -> bool {
  let Some(departed) = state.departed_owners.get(&object) else {
    return false;
  };
  let confirmed = regional
    .retired
    .get(&departed.owner)
    .is_some_and(|retirement| retirement.confirmed.contains(&owner));
  confirmed
    && owner != local
    && regional
      .configuration_for(owner)
      .is_some_and(|placement| !placement.place(object).candidates.contains(&local))
}

/// Drops this holder's copy of `object`: its acceptor, its routing entry, its lease-gate record, a green's
/// replica, and the content it held for the object (AUD-29-43: a stale copy's bytes are reclaimed with its
/// records, never kept for a head this holder no longer backs). Counted [`TAKEOVER_RECLAIMED`].
fn reclaim(state: &mut ShardState, object: ObjectId) {
  state.holder_records.remove(&object);
  state.held_content.forget_object(
    &mut crate::content_holder::hold_space(&mut state.store),
    object,
  );
  state.fleet.forget_object(object);
  state.departed_owners.remove(&object);
  state.merge.replicas.remove(&object);
  let count = state.refusals.entry(TAKEOVER_RECLAIMED).or_insert(0);
  *count = count.saturating_add(1);
}

/// Answers one page of a successor's takeover round on [`TAKEOVER_STREAM`] (§4.8): after checking the asker is
/// the session's peer, the round runs under the configuration this holder installed, and the retired host's
/// retirement is kept here, and — first — the owner lease's gate for the retired host, lists the records this
/// holder holds of the host's objects whose successor is the asker, from the page's start in object order,
/// each after installing the asker's authority on its acceptor and raising its fence to the round's epoch
/// (the prepare), until the page budget. An object whose fence is already above the epoch is not listed; its
/// fence is reported so the successor runs its next round above it. A holder under an older configuration
/// than the round's asks for the newer one; a round under an older one than this holder's is told the
/// current version.
pub(crate) fn serve_host_prepare(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Ok(prepare) = HostPrepare::decode(request) else {
    return refuse(state);
  };
  if prepare.owner != peer_host {
    return refuse(state);
  }
  let installed = state.fleet.configuration().version;
  if prepare.generation != installed || state.council.configuration().version != installed {
    state.count(TAKEOVER_PAGE_UNMATCHED, 1);
    if prepare.generation > installed {
      state.config_refresh_wanted = true;
      return Vec::new();
    }
    return encode_refusal(&RegisterError::ConfigurationStale { version: installed });
  }
  if !state
    .council
    .configuration()
    .retired
    .contains_key(&prepare.departed)
  {
    return refuse(state);
  }
  if !gate_open(state, prepare.departed) {
    let count = state.refusals.entry(PROMOTION_DEFERRED).or_insert(0);
    *count = count.saturating_add(1);
    return Vec::new();
  }
  let page = if prepare.named.is_empty() {
    list_page(state, local, &prepare)
  } else {
    answer_named(state, local, &prepare)
  };
  // The promise stands behind the shard's publish (§4.8 persistence before reply): the page raised each listed
  // object's fence and installed the asker's authority, and a warm restart keeps this node's member id, so that
  // fence must hold after it. A refused publish promises nothing; the asker asks again.
  if crate::verbs::publish_shard(state).is_err() {
    let count = state.refusals.entry(PROMISE_UNPUBLISHED).or_insert(0);
    *count = count.saturating_add(1);
    return Vec::new();
  }
  page.encode()
}

/// Counter: objects a holder refused to answer by name (A-251 (b)): the configuration names another successor, or the
/// copy's acceptor serves an owner outside the retired host's recovery chain. Such an object stays unanswered.
const TAKEOVER_NAMED_REFUSED: &str = "fleet.takeover.named_refused";

/// Answers a by-name question from this holder's own records, with no routing in the way
/// ([`slates_db::takeover::answer_by_name`]): its highest record of each named object, or that it holds nothing of it.
fn answer_named(state: &mut ShardState, local: HostId, prepare: &HostPrepare) -> HostPromise {
  let regional = state.council.configuration().clone();
  let answer = answer_by_name(
    local,
    prepare,
    &regional,
    &mut state.holder_records,
    page_budget(),
  );
  if answer.refused > 0 {
    state.count(TAKEOVER_NAMED_REFUSED, u64::from(answer.refused));
  }
  if answer.promise.unpromised > 0 {
    state.count(
      TAKEOVER_PAGE_UNPROMISED,
      u64::from(answer.promise.unpromised),
    );
  }
  answer.promise
}

/// The status count of takeover promises not answered because the shard's publish carrying their raised fences was
/// refused (§4.8 persistence before reply). Format: a refusal name in the daemon's status report.
const PROMISE_UNPUBLISHED: &str = "takeover.promise_unpublished";

/// Whether this node may promise `departed`'s objects now (§4.8 "Leases and reads", AUD-08): it has not answered
/// the retired owner's probes for the membership horizon, or the owner has announced the configuration that
/// retired it — so no lease the owner holds can still rest on this node's answers. Every promiser applies it:
/// a holder answering a page, and the successor counting its own copies.
fn gate_open(state: &ShardState, departed: HostId) -> bool {
  let Some(since_version) = state
    .council
    .configuration()
    .retired
    .get(&departed)
    .map(|retirement| retirement.version)
  else {
    return false;
  };
  state.answers_given.promotion_open(
    departed,
    since_version,
    slates_machine::clock::monotonic_ns(),
  )
}

/// Counts a refused takeover page and answers nothing, which the successor counts as no promise.
fn refuse(state: &mut ShardState) -> Vec<u8> {
  let count = state.refusals.entry(TAKEOVER_REFUSED).or_insert(0);
  *count = count.saturating_add(1);
  Vec::new()
}

/// The page of `prepare`'s answer this holder gives: see [`serve_host_prepare`].
fn list_page(state: &mut ShardState, local: HostId, prepare: &HostPrepare) -> HostPromise {
  let objects: Vec<ObjectId> = state
    .holder_records
    .keys()
    .copied()
    .filter(|object| prepare.after.is_none_or(|after| *object > after))
    .filter(|object| {
      state
        .departed_owners
        .get(object)
        .is_some_and(|departed| departed.owner == prepare.departed)
        && state.fleet.object_owner(*object) == Some(prepare.owner)
    })
    .collect();
  let budget = page_budget();
  let mut promise = HostPromise {
    holder: local,
    departed: prepare.departed,
    epoch: prepare.epoch,
    generation: prepare.generation,
    entries: Vec::new(),
    next: None,
    fenced: None,
    unpromised: 0,
    nothing: Vec::new(),
  };
  let mut used = slates_db::register::HOST_PROMISE_HEADER_BYTES;
  let mut last = prepare.after;
  for object in objects {
    let Some(acceptor) = state.holder_records.get_mut(&object) else {
      continue;
    };
    match promise_object(acceptor, object, prepare) {
      Ok(Some(highest)) => {
        let size = HostPromise::entry_bytes(&highest);
        if !promise.entries.is_empty() && used.saturating_add(size) > budget {
          promise.next = last;
          return promise;
        }
        used = used.saturating_add(size);
        promise.entries.push((object, highest));
      }
      // The acceptor holds nothing for this object, and promised that by raising its fence.
      Ok(None) => {}
      Err(refusal) => note_refusal(&mut promise, refusal, &mut state.refusals),
    }
    last = Some(object);
  }
  promise
}

/// Counter: a copy a holder could not promise for a reason other than a higher fence (a generation newer than the
/// asker's), so its page is no answer for it.
const TAKEOVER_PAGE_UNPROMISED: &str = "fleet.takeover.page_unpromised";

/// Records why `promise` could not cover one held copy. A fence above the epoch is the asker's to retry above; any
/// other refusal leaves the page short of an answer ([`HostPromise::unpromised`]), since a copy left out would otherwise
/// read as a copy not held, and a successor could adopt an older record over it.
fn note_refusal(
  promise: &mut HostPromise,
  refusal: RegisterError,
  counters: &mut BTreeMap<&'static str, u64>,
) {
  if let RegisterError::StaleEpoch { current } = refusal {
    let fence = HostEpoch(current);
    promise.fenced = Some(promise.fenced.map_or(fence, |seen| seen.max(fence)));
  } else {
    promise.unpromised = promise.unpromised.saturating_add(1);
    let count = counters.entry(TAKEOVER_PAGE_UNPROMISED).or_insert(0);
    *count = count.saturating_add(1);
  }
}

/// How far a holder's answer has come: where its next page starts, and whether it has answered in full. A page that
/// could not promise a copy it holds is not a full answer, however little was left to list.
fn page_of(promise: &HostPromise) -> Page {
  Page {
    after: promise.next,
    complete: promise.next.is_none() && promise.unpromised == 0,
  }
}

/// One period of this node's takeovers (§4.8), run by the record-plane coordinator: for every kept retirement
/// this node owes a confirmation of, the round asks the next page of every member that has not answered in
/// full, adopts every object whose recovery cohorts have promised, and confirms the share once done. A
/// takeover this node no longer owes (confirmed, or its retirement dropped) is forgotten. Returns the dispatches
/// still in flight, whose sessions the coordinator recovers.
pub(crate) async fn drive_takeovers(local: HostId, budget: CommitBudget) -> Vec<Dispatch> {
  let owed: Vec<HostId> = state::with_state(|s| {
    let regional = s.council.configuration();
    let owed: Vec<HostId> = regional
      .retired
      .keys()
      .copied()
      .filter(|departed| regional.owes_confirmation(*departed, local))
      .collect();
    s.host_takeovers
      .retain(|departed, _| owed.contains(departed));
    owed
  })
  .unwrap_or_default();
  let mut dispatches = Vec::new();
  for departed in owed {
    dispatches.extend(drive_host_takeover(departed, local, budget).await);
  }
  dispatches
}

/// One period of the takeover of `departed`'s objects: see [`drive_takeovers`].
async fn drive_host_takeover(
  departed: HostId,
  local: HostId,
  budget: CommitBudget,
) -> Vec<Dispatch> {
  let mut dispatches = Vec::new();
  let Some(requests) = state::with_state(|s| begin_round(s, departed, local)).flatten() else {
    return dispatches;
  };
  if !requests.is_empty() {
    dispatches.extend(ask_pages(departed, requests, budget).await);
  }
  let ready = state::with_state(|s| ready_objects(s, departed, local)).unwrap_or_default();
  for (object, adopted) in ready {
    dispatches.extend(adopt(departed, object, adopted, local, budget).await);
  }
  let done = state::with_state(|s| round_done(s, departed, local)).unwrap_or(false);
  if done {
    crate::fleet::send_council_report(
      slates_cluster::config_group::ConfigCommand::Confirm {
        departed,
        successor: local,
      },
      local,
      budget,
      &mut dispatches,
    )
    .await;
  }
  dispatches
}

/// Readies this period's round of `departed`'s takeover: starts it (or starts it over, on a new configuration
/// or above a fence a holder reported), promises this node's own copies of the objects that fall to it, and
/// returns each member's next question: its next page while its pages are incomplete, then the objects it has not
/// answered for by name, if any. `None` while this node's placement
/// is behind the council's (its round must run under the configuration the holders install).
pub(crate) fn begin_round(
  state: &mut ShardState,
  departed: HostId,
  local: HostId,
) -> Option<Vec<(HostId, HostPrepare)>> {
  let regional = state.council.configuration().clone();
  if state.fleet.configuration().version != regional.version {
    state.count(TAKEOVER_ROUND_BEHIND, 1);
    return None;
  }
  let Some(first_epoch) = initial_epoch(state, &regional, departed, local) else {
    state.count(TAKEOVER_EPOCH_EXHAUSTED, 1);
    return None;
  };
  let take = state
    .host_takeovers
    .entry(departed)
    .or_insert_with(|| HostTakeover::new(regional.version, first_epoch));
  if take.generation != regional.version {
    let epoch = take.epoch.max(first_epoch);
    take.restart(regional.version, epoch);
  }
  if let Some(fence) = take.fenced {
    let Some(above) = fence.0.checked_add(1) else {
      state.count(TAKEOVER_EPOCH_EXHAUSTED, 1);
      return None;
    };
    let epoch = HostEpoch(above).max(take.epoch);
    take.restart(regional.version, epoch);
  }
  let (epoch, generation) = (take.epoch, take.generation);
  // This node's own copies count toward each cohort's `f + 1` only once it may promise them, as a holder's
  // answer does ([`gate_open`]); until then it still asks the other members, and counts their answers.
  if gate_open(state, departed) {
    promise_local_copies(state, departed, local, epoch, generation);
  }
  let take = state.host_takeovers.get(&departed)?;
  let requests = regional
    .recovery_hosts(departed)
    .into_iter()
    .filter(|host| *host != local && regional.members.contains(host))
    .filter_map(|host| {
      let page = take.pages.get(&host).copied().unwrap_or_default();
      let named = if page.complete {
        unanswered_by_name(take, &regional, departed, host)
      } else {
        Vec::new()
      };
      (!page.complete || !named.is_empty()).then_some((
        host,
        HostPrepare {
          departed,
          owner: local,
          epoch,
          generation,
          after: if page.complete { None } else { page.after },
          named,
        },
      ))
    })
    .collect();
  Some(requests)
}

/// Derived: the most objects one by-name question names: the ids that fit one page's bytes after a request's fields
/// ([`page_budget`]), at least one. The answer is bounded by the holder as a page is; what it leaves out is asked
/// again next period.
fn named_per_question() -> usize {
  page_budget()
    .saturating_sub(slates_db::register::HOST_PREPARE_BYTES)
    .saturating_sub(slates_db::register::OBJECT_LIST_HEADER_BYTES)
    .checked_div(slates_db::register::OBJECT_BYTES)
    .unwrap_or(0)
    .max(1)
}

/// The learned objects `host`, whose pages are complete, has not answered for (A-251 (b)): those of whose recovery
/// cohorts it is a member that it neither listed nor named as holding nothing, at most [`named_per_question`].
fn unanswered_by_name(
  take: &HostTakeover,
  regional: &RegionalConfiguration,
  departed: HostId,
  host: HostId,
) -> Vec<ObjectId> {
  take
    .learned
    .iter()
    .filter(|(object, reports)| {
      !reports.contains_key(&host)
        && !take
          .named_nothing
          .get(*object)
          .is_some_and(|hosts| hosts.contains(&host))
        && regional
          .recovery_cohorts(departed, **object)
          .iter()
          .any(|cohort| cohort.contains(&host))
    })
    .map(|(object, _)| *object)
    .take(named_per_question())
    .collect()
}

/// The epoch a new round of `departed`'s takeover starts at: above every epoch this node's copies of the
/// objects that fall to it were written at, and at least the retired host's bumped fencing epoch — the fence
/// every holder raised for it at the install. A holder's fence above it sends the round higher. `None` when a
/// copy was written at `u64::MAX`, which no epoch can exceed.
fn initial_epoch(
  state: &ShardState,
  regional: &RegionalConfiguration,
  departed: HostId,
  local: HostId,
) -> Option<HostEpoch> {
  let bumped = regional
    .epochs
    .get(&departed)
    .copied()
    .unwrap_or(slates_db::register::FIRST_EPOCH);
  state
    .holder_records
    .iter()
    .filter(|(object, _)| falls_to(state, **object, departed, local))
    .flat_map(|(_, acceptor)| acceptor.persisted().1)
    .try_fold(bumped, |highest, (_, _, epoch, _)| {
      Some(highest.max(HostEpoch(epoch.0.checked_add(1)?)))
    })
}

/// Whether `object`, held here, is one of `departed`'s objects that falls to `local` in its takeover.
fn falls_to(state: &ShardState, object: ObjectId, departed: HostId, local: HostId) -> bool {
  state
    .departed_owners
    .get(&object)
    .is_some_and(|record| record.owner == departed)
    && state.fleet.object_owner(object) == Some(local)
}

/// Promises this node's own copies of `departed`'s objects that fall to it, as a holder answers a page: the
/// highest record of each is learned from this node itself, and a fence above the epoch sends the round higher.
fn promise_local_copies(
  state: &mut ShardState,
  departed: HostId,
  local: HostId,
  epoch: HostEpoch,
  generation: u64,
) {
  let objects: Vec<ObjectId> = state
    .holder_records
    .keys()
    .copied()
    .filter(|object| falls_to(state, *object, departed, local))
    .collect();
  let prepare = HostPrepare {
    departed,
    owner: local,
    epoch,
    generation,
    after: None,
    named: Vec::new(),
  };
  let mut learned = Vec::new();
  let mut fenced = None;
  for object in objects {
    let Some(acceptor) = state.holder_records.get_mut(&object) else {
      continue;
    };
    match promise_object(acceptor, object, &prepare) {
      Ok(Some(highest)) => learned.push((object, highest)),
      Ok(None) => {}
      Err(RegisterError::StaleEpoch { current }) => {
        fenced = Some(fenced.map_or(HostEpoch(current), |seen: HostEpoch| {
          seen.max(HostEpoch(current))
        }));
      }
      Err(_) => {}
    }
  }
  let Some(take) = state.host_takeovers.get_mut(&departed) else {
    return;
  };
  for (object, highest) in learned {
    if !take.adopted.contains(&object) {
      take
        .learned
        .entry(object)
        .or_default()
        .insert(local, highest);
    }
  }
  take.fenced = fenced;
}

/// Sends each member its next page request over [`TAKEOVER_STREAM`] and folds every binding answer into the
/// round: its listed records learned, its page advanced, a fence above the epoch noted. A member that does
/// not answer this period is asked again next period.
async fn ask_pages(
  departed: HostId,
  requests: Vec<(HostId, HostPrepare)>,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let wanted: BTreeMap<HostId, HostPrepare> = requests.into_iter().collect();
  let sessions = take_sessions("fleet.lent_to.ask_pages", |host| wanted.contains_key(&host));
  if sessions.is_empty() {
    let _ = state::with_state(|s| s.count(TAKEOVER_PAGES_UNSENT, 1));
    return None;
  }
  let sent: Vec<HostId> = sessions.iter().map(|(host, _)| *host).collect();
  let outgoing: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .filter_map(|(host, endpoint)| {
      wanted
        .get(&host)
        .map(|prepare| (host, prepare.encode(), endpoint))
    })
    .collect();
  let (replied, stragglers) = broadcast(outgoing, TAKEOVER_STREAM, Priority::Control, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut answers = Vec::new();
  let mut stale = None;
  let mut unbound: u64 = 0;
  for (host, reply, endpoint) in replied {
    if let Some(slates_db::register::Refusal::ConfigurationStale { version }) =
      slates_db::register::decode_refusal(&reply.bytes)
    {
      stale = stale.max(Some(version));
    }
    if let Ok(promise) = HostPromise::decode(&reply.bytes)
      && promise.holder == host
      && let Some(prepare) = wanted.get(&host)
      && promise.binds(prepare)
    {
      answers.push((host, promise, !prepare.named.is_empty()));
    } else {
      unbound = unbound.saturating_add(1);
    }
    recovered.push((host, endpoint));
  }
  let dispatch = Dispatch::new(sent, &recovered, stragglers, LateReplies::Discard);
  return_sessions(recovered);
  state::with_state(|s| {
    // A holder on a newer configuration refused the page `ConfigurationStale` (`serve_host_prepare`): this node is
    // behind, so it fetches the committed configuration next period and its round starts over under it, as a
    // record's sender does (`fleet::ship_head`). Dropped before 2026-11-05, a learner successor one version behind,
    // whose own membership view matched its configuration and so gave it no cause to fetch, asked every period and
    // was refused every period (2026-11-05, the forming-fleet takeover flows in `crates/cli/tests/live_fleet.rs`).
    if stale.is_some_and(|version| version > s.council.configuration().version) {
      s.config_refresh_wanted = true;
      s.count(TAKEOVER_STALE_CONFIGURATION, 1);
    }
    if unbound > 0 {
      s.count(TAKEOVER_PAGE_UNBOUND, unbound);
    }
    fold_answers(s, departed, answers);
  });
  Some(dispatch)
}

/// Folds members' answers into `departed`'s round: each listed record learned under its holder, and the highest fence
/// above the epoch noted. A page answer advances its member's page (complete once a page names no next); an answer by
/// name (A-251 (b)) leaves the page as it was and records each object its member holds nothing of.
pub(crate) fn fold_answers(
  state: &mut ShardState,
  departed: HostId,
  answers: Vec<(HostId, HostPromise, bool)>,
) {
  let Some(take) = state.host_takeovers.get_mut(&departed) else {
    return;
  };
  for (host, promise, by_name) in answers {
    if let Some(fence) = promise.fenced
      && fence >= take.epoch
    {
      take.fenced = Some(take.fenced.map_or(fence, |seen| seen.max(fence)));
      continue;
    }
    let page = page_of(&promise);
    for (object, highest) in promise.entries {
      if !take.adopted.contains(&object) {
        take
          .learned
          .entry(object)
          .or_default()
          .insert(host, highest);
      }
    }
    if by_name {
      for object in promise.nothing {
        if take.learned.contains_key(&object) {
          take.named_nothing.entry(object).or_default().insert(host);
        }
      }
    } else {
      take.pages.insert(host, page);
    }
  }
}

/// The objects of `departed`'s takeover ready to adopt, each with the newest record reported for it: every
/// one of its recovery cohorts has its recovery quorum of members that promised it — `cohort − f`, `f + 1` at
/// the floor, at least one ([`slates_db::register::Quorum::recovery`]) — this node, a member that listed it, or one
/// that answered for it by name. An object with a cohort left with fewer members than that quorum is lost,
/// counted and never adopted: more of that cohort failed than the region tolerates. The rule is
/// [`slates_db::takeover::readiness`] under [`CountingRule::NamedAnswer`] (A-251); the model in
/// `crates/db/tests/takeover_model.rs` finds where counting a complete page instead adopts an older record than a
/// committed one.
pub(crate) fn ready_objects(
  state: &mut ShardState,
  departed: HostId,
  local: HostId,
) -> Vec<(ObjectId, Accepted)> {
  let regional = state.council.configuration().clone();
  let local_promises = gate_open(state, departed);
  let Some(take) = state.host_takeovers.get_mut(&departed) else {
    return Vec::new();
  };
  let mut ready = Vec::new();
  let mut lost = Vec::new();
  let mut unpromised: u64 = 0;
  let complete: BTreeSet<HostId> = take
    .pages
    .iter()
    .filter(|(_, page)| page.complete)
    .map(|(host, _)| *host)
    .collect();
  let none_named = BTreeSet::new();
  for (object, reports) in &take.learned {
    let heard = Heard {
      reports,
      complete: &complete,
      named_nothing: take.named_nothing.get(object).unwrap_or(&none_named),
      local: local_promises.then_some(local),
    };
    let cohorts = regional.recovery_cohorts(departed, *object);
    match readiness(
      &cohorts,
      |host| regional.members.contains(&host),
      regional.quorum,
      &heard,
      CountingRule::NamedAnswer,
    ) {
      Readiness::Ready => {
        if let Some(newest) = reports.values().cloned().reduce(newest_of) {
          ready.push((*object, newest));
        }
      }
      Readiness::Lost { .. } => lost.push(*object),
      Readiness::Waiting => unpromised = unpromised.saturating_add(1),
    }
  }
  if unpromised > 0 {
    let count = state.refusals.entry(TAKEOVER_UNPROMISED).or_insert(0);
    *count = count.saturating_add(unpromised);
  }
  for object in lost {
    take.learned.remove(&object);
    if take.lost.insert(object) {
      let count = state.refusals.entry(TAKEOVER_LOST).or_insert(0);
      *count = count.saturating_add(1);
    }
  }
  ready
}

/// The newer of two reported records by phase one's order (sequence, then epoch), with tied sealed heads merged: two
/// head copies at one position that name the same manifest under the same lineage key differ only in which holders'
/// key entries they carry (an owner adds an entry for a member that joined after the head first shipped, A-92 piece
/// 4c), and each entry is valid on its own. The merge keeps every entry, by anchor, so a successor that holds its own
/// entry in any copy can open the envelope; keeping one copy by host order could adopt one without it, and the
/// successor would refuse the volume for good (`docs/bugs/2026-10-08-a-sealed-head-shipped-without-its-successors-key-entry.md`).
/// Any other tie keeps the first, as before.
fn newest_of(best: Accepted, next: Accepted) -> Accepted {
  if next.newer_than(&best) {
    return next;
  }
  if best.newer_than(&next) || best.value == next.value {
    return best;
  }
  let (Some(mut merged), Some(other)) = (
    HeadValue::from_record_bytes(&best.value),
    HeadValue::from_record_bytes(&next.value),
  ) else {
    return best;
  };
  let (Some(sealing), Some(other_sealing)) = (merged.sealing.as_mut(), other.sealing) else {
    return best;
  };
  if merged.manifest != other.manifest
    || sealing.lineage != other_sealing.lineage
    || sealing.owner_anchor != other_sealing.owner_anchor
    || sealing.partition != other_sealing.partition
  {
    return best;
  }
  for key in other_sealing.keys {
    if !sealing.keys.iter().any(|held| held.anchor == key.anchor) {
      sealing.keys.push(key);
    }
  }
  // Anchor order, as the owner writes the entries, so equal sets encode equally.
  sealing.keys.sort_by_key(|key| key.anchor);
  Accepted {
    value: merged.to_record_bytes(),
    ..best
  }
}

/// Adopts `object` (§4.8 "completes safe adoption under the new epoch"): re-commits the newest reported record
/// under this node's own placement of it at the round's epoch, through the object's acceptor here (created
/// under this node's authority when it held nothing). Once placed, this node owns the object: it is tracked as
/// its own, its placement recorded, its lease-gate record dropped, and its content or green queued to be
/// served. Short of placement it stays learned and is adopted again next period.
async fn adopt(
  departed: HostId,
  object: ObjectId,
  adopted: Accepted,
  local: HostId,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let (mut acceptor, shape, quorum, record) = state::with_state(|s| {
    let config = s.fleet.configuration();
    let generation = config.version;
    let quorum = config.quorum;
    let shape = Placement {
      acked: Vec::new(),
      ..config.place(object)
    };
    let epoch = s.host_takeovers.get(&departed)?.epoch;
    let mut acceptor = s.holder_records.remove(&object).unwrap_or_else(|| {
      Acceptor::new(
        local,
        Authority {
          generation,
          owner: local,
        },
      )
    });
    let _ = acceptor.install_authority(Authority {
      generation,
      owner: local,
    });
    let record = Record {
      owner: local,
      object,
      sequence: adopted.sequence,
      epoch,
      generation,
      value: adopted.value,
    };
    Some((acceptor, shape, quorum, record))
  })
  .flatten()?;
  let holders = take_sessions("fleet.lent_to.adopt", |host| {
    host != local && shape.candidates.contains(&host)
  });
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let no_holders = taken.is_empty();
  let committed = commit_record(
    local,
    &mut acceptor,
    &shape,
    &record,
    quorum,
    holders,
    budget,
  )
  .await;
  // A holder on a newer configuration refused the adopted record `ConfigurationStale`: this node fetches the committed
  // configuration next period and adopts under it, as an owner shipping a head does (`fleet::ship_head`). Dropped before
  // 2026-11-05, a learner successor one version behind the council's sole voter had its adoption refused every period
  // and never fetched, since its membership view matched its configuration (`NotPlaced` each period, the volume never
  // served; `a_live_volume_made_while_its_successor_was_stopped_is_served_by_it` under the forming-fleet soak).
  if let Some(version) = committed.stale_version {
    let _ = state::with_state(|s| {
      if version > s.council.configuration().version {
        s.config_refresh_wanted = true;
        s.count(TAKEOVER_STALE_CONFIGURATION, 1);
      }
    });
  }
  let dispatch = Dispatch::new(
    taken,
    &committed.reusable,
    committed.stragglers,
    LateReplies::Discard,
  );
  return_sessions(committed.reusable);
  let acked = match &committed.outcome {
    Ok(placement)
    | Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => {
      placement.acked.clone()
    }
    Err(_) => Vec::new(),
  };
  let unplaced_cause = if !acked.contains(&local) {
    TAKEOVER_ADOPT_LOCAL_REFUSED
  } else if no_holders {
    TAKEOVER_ADOPT_NO_HOLDERS
  } else {
    TAKEOVER_ADOPT_UNACKED
  };
  let placement = match committed.outcome {
    Ok(placement) if placement.placed(quorum) => Some(placement),
    Ok(_) | Err(ClusterError::Uncertain { .. } | ClusterError::NotPlaced { .. }) => None,
    Err(_) => None,
  };
  state::with_state(|s| {
    s.holder_records.insert(object, acceptor);
    if let Some(placement) = placement {
      record_adoption(s, departed, object, &record, placement, local);
    } else {
      s.count(TAKEOVER_ADOPT_UNPLACED, 1);
      s.count(unplaced_cause, 1);
    }
  });
  Some(dispatch)
}

/// Records `object` adopted by this node: tracked as its own, its placement kept at the adoption's sequence
/// and epoch, its lease-gate record dropped, the round's learned reports for it dropped, and its content — a
/// volume's manifest or a green's chain — queued to be served on the shard its id routes to.
fn record_adoption(
  state: &mut ShardState,
  departed: HostId,
  object: ObjectId,
  record: &Record,
  placement: Placement,
  local: HostId,
) {
  track_owner_on_every_shard(state, object, local);
  state.placed_heads.insert(
    object,
    PlacedHead {
      sequence: record.sequence,
      epoch: record.epoch,
      placement,
    },
  );
  state.departed_owners.remove(&object);
  if let Some(take) = state.host_takeovers.get_mut(&departed) {
    take.learned.remove(&object);
    take.adopted.insert(object);
  }
  if object.is_catalog() {
    // A volume's catalog register: its successor rebuilds the volume under it (AUD-29-17) — unless the
    // volume's own register was adopted as a tombstone (AUD-29-43): a destroyed volume is not rebuilt.
    if let Some(catalog) = crate::catalog::CatalogValue::from_record_bytes(&record.value)
      && !state
        .pending_tombstones
        .contains_key(&object.placement_key())
    {
      state
        .pending_catalogs
        .insert(object.placement_key(), (record.sequence, catalog));
    }
  } else if let Some(tombstone) = crate::tombstone::TombstoneValue::from_record_bytes(&record.value)
  {
    // A destroyed volume (AUD-29-43): the successor owes its tombstone from the adopted stage on, and never
    // materializes it.
    state.pending_catalogs.remove(&object);
    state
      .pending_tombstones
      .insert(object, tombstone.tombstone_sequence(record.sequence));
  } else if let Some(head) = HeadValue::from_record_bytes(&record.value) {
    state.pending_materializations.insert(object, head);
  } else if let Some(merge) =
    crate::merge_service::MergeRecordValue::from_record_bytes(&record.value)
  {
    // A green: its newest merge record was adopted; the owned green is rebuilt from this node's accepted chain
    // and held inputs on the shard its id routes to (AUD-14).
    state.pending_green_materializations.insert(object, merge);
  }
  let count = state.refusals.entry(TAKEOVER_ADOPTED).or_insert(0);
  *count = count.saturating_add(1);
}

/// Whether this node's share of `departed`'s takeover is done: every member of the recovery neighbourhoods it
/// asked has answered in full, and every object it learned is adopted or cannot be recovered.
fn round_done(state: &ShardState, departed: HostId, local: HostId) -> bool {
  let regional = state.council.configuration();
  let Some(take) = state.host_takeovers.get(&departed) else {
    return false;
  };
  take.generation == regional.version
    && take.fenced.is_none()
    && take.learned.is_empty()
    && regional
      .recovery_hosts(departed)
      .into_iter()
      .filter(|host| *host != local && regional.members.contains(host))
      .all(|host| take.pages.get(&host).is_some_and(|page| page.complete))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::head::{HeadKey, HeadSealing};
  use slates_db::register::{Authority, HostEpoch, HostId as RegisterHost, Record};

  /// §4.8 phase one: a holder's page is a promise only for what it could actually promise. Do: have a real acceptor,
  /// holding a record under a newer generation, refuse a prepare from an older one, and record that refusal in a page;
  /// also record a higher fence. Expect: the generation refusal leaves the page not complete and counted, so the holder is
  /// not read as holding nothing; the fence is left for the asker to retry above and does not.
  #[test]
  fn a_holder_that_could_not_promise_a_copy_has_not_answered_for_it() {
    let host = RegisterHost(7);
    let object = ObjectId::new(host, 1);
    let mut acceptor = Acceptor::new(
      RegisterHost(2),
      Authority {
        generation: 9,
        owner: host,
      },
    );
    acceptor
      .accept(&Record {
        owner: host,
        object,
        sequence: 1,
        epoch: HostEpoch(1),
        generation: 9,
        value: b"newer".to_vec(),
      })
      .unwrap();
    let prepare = HostPrepare {
      departed: host,
      owner: RegisterHost(3),
      epoch: HostEpoch(4),
      generation: 5,
      after: None,
      named: Vec::new(),
    };
    let refusal = promise_object(&mut acceptor, object, &prepare).unwrap_err();
    let mut promise = HostPromise {
      holder: RegisterHost(2),
      departed: host,
      epoch: prepare.epoch,
      generation: prepare.generation,
      entries: Vec::new(),
      next: None,
      fenced: None,
      unpromised: 0,
      nothing: Vec::new(),
    };
    let mut counters = BTreeMap::new();
    assert!(page_of(&promise).complete, "an empty answer is a full one");
    note_refusal(&mut promise, refusal, &mut counters);
    assert_eq!(promise.unpromised, 1);
    assert!(
      !page_of(&promise).complete,
      "a copy left out is not answered for"
    );
    assert_eq!(counters.get(TAKEOVER_PAGE_UNPROMISED), Some(&1));

    let mut fenced = HostPromise {
      unpromised: 0,
      ..promise.clone()
    };
    note_refusal(
      &mut fenced,
      RegisterError::StaleEpoch { current: 8 },
      &mut counters,
    );
    assert_eq!(fenced.fenced, Some(HostEpoch(8)));
    assert_eq!(
      fenced.unpromised, 0,
      "the fence is the asker's to retry above"
    );
  }

  /// §4.8 phase one: `None` from a holder means it holds nothing, so the empty promise stands. Do: prepare on a fresh
  /// acceptor and on one holding a record. Expect: `None` and the record.
  #[test]
  fn a_holder_lists_every_copy_it_holds_and_only_those() {
    let host = RegisterHost(7);
    let object = ObjectId::new(host, 1);
    let authority = Authority {
      generation: 5,
      owner: host,
    };
    let prepare = HostPrepare {
      departed: host,
      owner: host,
      epoch: HostEpoch(4),
      generation: 5,
      after: None,
      named: Vec::new(),
    };
    let mut empty = Acceptor::new(RegisterHost(2), authority);
    assert_eq!(promise_object(&mut empty, object, &prepare), Ok(None));
    let mut holding = Acceptor::new(RegisterHost(2), authority);
    holding
      .accept(&Record {
        owner: host,
        object,
        sequence: 1,
        epoch: HostEpoch(1),
        generation: 5,
        value: b"held".to_vec(),
      })
      .unwrap();
    assert!(matches!(
      promise_object(&mut holding, object, &prepare),
      Ok(Some(_))
    ));
  }

  fn sealed(keys: &[u64], manifest: u8) -> HeadValue {
    HeadValue {
      manifest: Some([manifest; 32]),
      content_holders: vec![1, 2],
      sealing: Some(HeadSealing {
        owner_anchor: 9,
        partition: 0,
        lineage: [7; 16],
        keys: keys
          .iter()
          .map(|anchor| HeadKey {
            anchor: *anchor,
            wrapped: vec![u8::try_from(*anchor).unwrap(); 4],
          })
          .collect(),
        naming: None,
      }),
    }
  }

  fn accepted(sequence: u64, epoch: u64, value: &HeadValue) -> Accepted {
    Accepted {
      sequence,
      epoch: HostEpoch(epoch),
      value: value.to_record_bytes(),
    }
  }

  fn anchors(record: &Accepted) -> Vec<u64> {
    HeadValue::from_record_bytes(&record.value)
      .and_then(|value| value.sealing)
      .map(|sealing| sealing.keys.iter().map(|key| key.anchor).collect())
      .unwrap_or_default()
  }

  /// A-92 piece 4c: two tied copies of a sealed head, one written before a member joined (entries 1, 2) and one after
  /// (entries 1, 2, 3). Do: fold them in both orders. Expect: the adopted value carries all three entries either way,
  /// so the member that joined can open the envelope as successor.
  #[test]
  fn tied_copies_of_a_sealed_head_adopt_every_holders_key_entry() {
    let older = accepted(4, 2, &sealed(&[1, 2], 5));
    let newer = accepted(4, 2, &sealed(&[1, 2, 3], 5));
    assert_eq!(
      anchors(&newest_of(older.clone(), newer.clone())),
      vec![1, 2, 3]
    );
    assert_eq!(anchors(&newest_of(newer, older)), vec![1, 2, 3]);
  }

  /// Phase one's order still decides: a newer position wins whole, and a tie of two heads naming different content is
  /// not merged (the first is kept). Do: fold a newer sequence over an older one with more entries, and two tied heads
  /// with different manifests. Expect: the newer record as it was, and the first tied record as it was.
  #[test]
  fn a_newer_record_wins_and_different_content_is_never_merged() {
    let old_many = accepted(4, 2, &sealed(&[1, 2, 3], 5));
    let new_few = accepted(5, 2, &sealed(&[1], 6));
    assert_eq!(newest_of(old_many.clone(), new_few.clone()), new_few);
    let first = accepted(4, 2, &sealed(&[1], 5));
    let other_content = accepted(4, 2, &sealed(&[2], 6));
    assert_eq!(newest_of(first.clone(), other_content), first);
  }
}
