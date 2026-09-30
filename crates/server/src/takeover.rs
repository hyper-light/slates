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
//!   ([`serve_host_prepare`]). A holder's complete answer promises every object of the host whose successor is
//!   the asker — those it lists, and, by listing nothing more, those it holds nothing of: the retired host is
//!   no member, so no holder accepts its records, and the successor is the only member the configuration names
//!   for the object. That empty promise is what the per-object round could not give, and why a takeover
//!   stalled for good when one survivor never received the object
//!   (`docs/bugs/2026-09-29-a-takeover-stalled-when-a-survivor-never-received-the-head.md`).
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
  Placement, Prepare, Record, RegionalConfiguration, RegisterError, encode_refusal,
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
/// The status count of takeover rounds refused because no epoch could supersede what they must fence — a
/// holder's fence or a held record at `u64::MAX` (AUD-29-26's sibling): the round never reuses an epoch.
const TAKEOVER_EPOCH_EXHAUSTED: &str = "fleet.takeover.epoch_exhausted";

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
  }

  /// The objects learned and not yet adopted.
  pub fn outstanding(&self) -> usize {
    self.learned.len()
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
        state.fleet.track_object_owner(object, successor);
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

/// Drops this holder's copy of `object`: its acceptor, its routing entry, its lease-gate record, and a green's
/// replica. Counted [`TAKEOVER_RECLAIMED`].
fn reclaim(state: &mut ShardState, object: ObjectId) {
  state.holder_records.remove(&object);
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
  list_page(state, local, &prepare).encode()
}

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
      Ok(None) => {}
      Err(RegisterError::StaleEpoch { current }) => {
        let fence = HostEpoch(current);
        promise.fenced = Some(promise.fenced.map_or(fence, |seen| seen.max(fence)));
      }
      Err(_) => {}
    }
    last = Some(object);
  }
  promise
}

/// Promises `object` to the asker: installs the asker's authority under the round's generation and runs the
/// prepare through the object's acceptor, raising its fence to the epoch. The highest record held, `None` when
/// the acceptor holds nothing, or the refusal (a fence already above the epoch).
fn promise_object(
  acceptor: &mut Acceptor,
  object: ObjectId,
  prepare: &HostPrepare,
) -> Result<Option<Accepted>, RegisterError> {
  acceptor.install_authority(Authority {
    generation: prepare.generation,
    owner: prepare.owner,
  })?;
  let promise = acceptor.prepare(&Prepare {
    owner: prepare.owner,
    object,
    epoch: prepare.epoch,
    generation: prepare.generation,
  })?;
  Ok(promise.highest)
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
/// returns the page request for each member that has not answered in full. `None` while this node's placement
/// is behind the council's (its round must run under the configuration the holders install).
pub(crate) fn begin_round(
  state: &mut ShardState,
  departed: HostId,
  local: HostId,
) -> Option<Vec<(HostId, HostPrepare)>> {
  let regional = state.council.configuration().clone();
  if state.fleet.configuration().version != regional.version {
    return None;
  }
  let Some(first_epoch) = initial_epoch(state, &regional, departed, local) else {
    *state.refusals.entry(TAKEOVER_EPOCH_EXHAUSTED).or_insert(0) += 1;
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
      *state.refusals.entry(TAKEOVER_EPOCH_EXHAUSTED).or_insert(0) += 1;
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
      (!page.complete).then_some((
        host,
        HostPrepare {
          departed,
          owner: local,
          epoch,
          generation,
          after: page.after,
        },
      ))
    })
    .collect();
  Some(requests)
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
  let sessions = take_sessions(|host| wanted.contains_key(&host));
  if sessions.is_empty() {
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
  for (host, reply, endpoint) in replied {
    if let Ok(promise) = HostPromise::decode(&reply.bytes)
      && promise.holder == host
      && wanted
        .get(&host)
        .is_some_and(|prepare| promise.binds(prepare))
    {
      answers.push((host, promise));
    }
    recovered.push((host, endpoint));
  }
  let dispatch = Dispatch::new(sent, &recovered, stragglers, LateReplies::Discard);
  return_sessions(recovered);
  state::with_state(|s| fold_answers(s, departed, answers));
  Some(dispatch)
}

/// Folds members' page answers into `departed`'s round: each listed record learned under its holder, each
/// page's start advanced (complete once a page names no next), and the highest fence above the epoch noted.
fn fold_answers(state: &mut ShardState, departed: HostId, answers: Vec<(HostId, HostPromise)>) {
  let Some(take) = state.host_takeovers.get_mut(&departed) else {
    return;
  };
  for (host, promise) in answers {
    if let Some(fence) = promise.fenced
      && fence >= take.epoch
    {
      take.fenced = Some(take.fenced.map_or(fence, |seen| seen.max(fence)));
      continue;
    }
    for (object, highest) in promise.entries {
      if !take.adopted.contains(&object) {
        take
          .learned
          .entry(object)
          .or_default()
          .insert(host, highest);
      }
    }
    take.pages.insert(
      host,
      Page {
        after: promise.next,
        complete: promise.next.is_none(),
      },
    );
  }
}

/// The objects of `departed`'s takeover ready to adopt, each with the newest record reported for it: every
/// one of its recovery cohorts has its recovery quorum of members that promised it — `cohort − f`, `f + 1` at
/// the floor, at least one ([`slates_db::register::Quorum::recovery`]) — this node, a member whose answer is
/// complete, or one that listed it. An object with a cohort left with fewer members than that quorum is lost,
/// counted and never adopted: more of that cohort failed than the region tolerates.
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
  for (object, reports) in &take.learned {
    let mut promised_everywhere = true;
    for cohort in regional.recovery_cohorts(departed, *object) {
      let needed = regional.quorum.recovery(cohort.len());
      let members: Vec<HostId> = cohort
        .into_iter()
        .filter(|host| regional.members.contains(host))
        .collect();
      if members.len() < needed {
        lost.push(*object);
        promised_everywhere = false;
        break;
      }
      let promised = members
        .iter()
        .filter(|host| {
          if **host == local {
            return local_promises;
          }
          reports.contains_key(host) || take.pages.get(host).is_some_and(|page| page.complete)
        })
        .count();
      promised_everywhere &= promised >= needed;
    }
    if promised_everywhere
      && let Some(newest) = reports
        .values()
        .cloned()
        .reduce(|best, next| if next.newer_than(&best) { next } else { best })
    {
      ready.push((*object, newest));
    }
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
  let holders = take_sessions(|host| host != local && shape.candidates.contains(&host));
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
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
  let dispatch = Dispatch::new(
    taken,
    &committed.reusable,
    committed.stragglers,
    LateReplies::Discard,
  );
  return_sessions(committed.reusable);
  let placement = match committed.outcome {
    Ok(placement) if placement.placed(quorum) => Some(placement),
    Ok(_) | Err(ClusterError::Uncertain { .. } | ClusterError::NotPlaced { .. }) => None,
    Err(_) => None,
  };
  state::with_state(|s| {
    s.holder_records.insert(object, acceptor);
    if let Some(placement) = placement {
      record_adoption(s, departed, object, &record, placement, local);
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
  state.fleet.track_object_owner(object, local);
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
  if let Some(head) = HeadValue::from_record_bytes(&record.value) {
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
