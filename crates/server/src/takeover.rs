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
//!   confirming carries the object into its own takeover. A copy whose line ends in a takeover the configuration
//!   keeps a completion record of is stale and reclaimed ([`resolve_held_objects`]); a line that ends anywhere else
//!   (no record kept) is unresolved, and its copy is kept (A-272, A-309).
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
//!   the successor's own placement at the round's epoch, and the successor then serves it. Past tolerance, when a
//!   cohort has fewer members left than that quorum, it is adopted once every surviving member of its cohorts, settled
//!   and current, has promised, with the newest record any of them holds (A-309): the members that exist now recover
//!   it, where before it was lost and recovered by no one.
//! - **The wait's bound (A-304).** A member that never answers the successor while every other member hears it is never
//!   retired, so an object waiting for its promise would wait without end. Each learned object counts the periods it
//!   waits with no member promising it anew and no page advancing ([`PromiseWait`]); past the failover SLO's worth it is
//!   given up by name ([`give_up_unpromised`]), and the share stays unconfirmed, so its holders keep their copies
//!   (A-272's rule). A wait past tolerance, for every surviving member, is bounded the same way.
//! - **Confirmation.** A successor confirms its share once every member it asked has answered in full and every
//!   object it learned is adopted, none given up (A-272, A-304); the council records it ([`crate::fleet`]'s report stream), and a
//!   retirement becomes a completion record once every survivor has confirmed or carried its share into a takeover
//!   that completed (A-309). Only that record settles a line, and only for a holder the takeover asked.
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
  CountingRule, Heard, Readiness, answer_by_name, copy_falls_to, falls_to as falls_by_authority,
  promise_object, readiness,
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
/// Of [`TAKEOVER_UNPROMISED`]: those that had waited past the round's bound ([`PromiseWait`]), each period until it gave
/// them up. Zero is the healthy reading. Format: a refusal name in the daemon's status report.
const TAKEOVER_UNPROMISED_PAST_BOUND: &str = "fleet.takeover.unpromised.past_bound";
/// The status count of learned objects a round gave up on ([`give_up_unpromised`]): every verb on one answers
/// `NotRecovered { cause: TakeoverIncomplete { waited_on: "promises" } }`. Zero is the healthy reading. Format: a refusal
/// name in the daemon's status report.
const TAKEOVER_GIVEN_UP: &str = "fleet.takeover.given_up";
/// Format: the wait a given-up learned object names in its refusal's cause (`RecoveryCause::TakeoverIncomplete`): its
/// recovery cohorts' promises.
const UNPROMISED_WAIT: &str = "promises";
/// Format: the wait a learned object names in its refusal's cause (`RecoveryCause::TakeoverIncomplete`) when the
/// retirement bound let its takeover go before it completed (A-340): the retirement its round recovered it through.
const RETIREMENT_WAIT: &str = "retirement";
/// The status count of learned objects given up because the retirement bound let their takeover go before it completed
/// ([`give_up_dropped`], A-340): each answers `NotRecovered { cause: TakeoverIncomplete { waited_on: "retirement" } }`
/// on this node, the successor its round ranked. Also counted [`TAKEOVER_GIVEN_UP`]. Zero is the healthy reading.
/// Format: a refusal name in the daemon's status report.
const TAKEOVER_GIVEN_UP_DROPPED: &str = "fleet.takeover.given_up.retirement_dropped";
/// The status count under which a holder records a copy it keeps on a line no kept retirement or completion record
/// resolves ([`Lineage::Unresolved`]): a retirement the bound let go before it completed, a completion record let go, or
/// a host retired with nothing to recover through. Unresolved never reclaims (A-272), so such a copy stays; counted at
/// each install that judges it, so a holder shows what it keeps that no takeover will recover (A-340). Format: a refusal
/// name in the daemon's status report.
const TAKEOVER_KEPT_UNRESOLVED: &str = "fleet.takeover.kept_unresolved";
/// The status count under which a holder records a copy it keeps on a line whose retirement had no survivor to rank
/// ([`Lineage::Lost`]): nothing can recover it. Counted at each install that judges it (A-340). Format: a refusal name in
/// the daemon's status report.
const TAKEOVER_KEPT_LOST: &str = "fleet.takeover.kept_lost";

/// Counter: takeover pages and questions by name a test's injected silence left unanswered
/// (`Daemon::inject_takeover_silence`); zero in production. Format: a counter name in the daemon's status report.
const TAKEOVER_SILENCED_BY_FAULT: &str = "fleet.takeover.silenced_by_fault";

/// The status refusal count under which a holder records a takeover page it deferred because its own answers
/// to the retired owner's probes may still feed that owner's lease (§4.8 "Leases and reads", AUD-08); the
/// successor asks again next period. Format: a refusal name in the daemon's status report.
pub(crate) const PROMOTION_DEFERRED: &str = "fleet.promotion.deferred";

/// The status count under which a holder records a stale copy it dropped: its object's line ends in a
/// takeover already done, or its successor has confirmed and this holder is outside the successor's placement.
/// Format: a refusal name in the daemon's status report.
const TAKEOVER_RECLAIMED: &str = "fleet.takeover.reclaimed";

/// The status count under which a holder records a copy it kept though its line ended at a completed takeover, or its
/// successor confirmed with this holder outside its placement: the takeover never asked this holder (it was no survivor
/// of it), so nothing shows its copy adopted (A-309). Format: a refusal name in the daemon's status report.
const TAKEOVER_KEPT_UNASKED: &str = "fleet.takeover.kept_unasked";

/// The status count under which a holder records a stale copy it kept because it still holds the copy's content
/// (A-335): content its records name it for, or content placed for a sequence ahead of them, which its retention rule
/// keeps (`crate::content_retention`). The successor adopts the newest record its round learned, and that head may name
/// this holder among the content's holders; it confirms once the record is adopted, before its materialization has
/// fetched the content, and it fetches from the head's holders alone (`fleet::fetch_into_hold`). Let go here, with the
/// dead owner the head's only other holder, the successor could fetch from nobody, and one failure lost the volume at
/// `f = 1`. Kept until a newer record reaches this holder and the retention rule releases the content; a holder outside
/// the successor's placement receives none, so a release from the successor is owed (GAPS). Counted at each install it
/// is judged at. Format: a refusal name in the daemon's status report.
const TAKEOVER_KEPT_HOLDING_CONTENT: &str = "fleet.takeover.kept_holding_content";

/// The status count under which a successor records an object it adopted past tolerance (A-309): one of its recovery
/// cohorts had fewer members left than its recovery quorum, so every surviving member of its cohorts, settled and
/// current, answered first, and the newest record any of them held was adopted. Before A-309 such an object was counted
/// `fleet.takeover.lost` and recovered by no one. Format: a refusal name in the daemon's status report.
const TAKEOVER_RECOVERED_PAST_TOLERANCE: &str = "fleet.takeover.recovered_past_tolerance";

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
  /// The highest fence a holder reported above the epoch; the next round runs above it.
  fenced: Option<HostEpoch>,
  /// Per learned object, the members that answered for it by name holding nothing of it (A-251 (b)): their promise of
  /// that object alone. A member's complete pages promise nothing of an object they did not list.
  named_nothing: BTreeMap<ObjectId, BTreeSet<HostId>>,
  /// The page answers that advanced a member's page (its next page starts later, or its answer completed), counted
  /// since the round began and never cleared by a restart: the round's progress, which a taken-over object waiting on a
  /// register this round may still bring reads ([`rounds_still_paging`]).
  page_advances: u64,
  /// Per learned object still waiting for its recovery cohorts' promises, its count toward the round's bound
  /// ([`PromiseWait`]). Bounded by `learned`: an entry leaves with its object.
  waits: BTreeMap<ObjectId, PromiseWait>,
  /// The objects this round gave up on: each waited the failover SLO's worth of periods with no member promising it
  /// anew and no page of the round advancing, and every verb on it answers `NotRecovered` naming the promises it waited
  /// for ([`give_up_unpromised`]). Never adopted or learned again in this round, and the share is not confirmed while
  /// any is here, so their holders keep their copies (A-272's rule). Bounded by the retired host's objects.
  given_up: BTreeSet<ObjectId>,
  /// Per learned object judged ready past tolerance ([`ready_objects`]), the first of its cohorts short of the recovery
  /// quorum: its adoption records it with a volume's head, whose loss report names it once served (A-332). Bounded by
  /// `learned`: an entry leaves with its object.
  past_tolerance: BTreeMap<ObjectId, slates_db::catalog::RecoveryShortfall>,
}

/// One learned object's wait for its recovery cohorts' promises (§4.8 phase one), as the round's bound counts it: the
/// periods it waited since it last made progress, and the marks its progress is judged against. Without a bound a member
/// that never promised it (one cut off from this node alone, which the council therefore never retires) left the object
/// waiting every period for as long as the daemon ran, and every verb on it answered a retryable `Overloaded`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PromiseWait {
  /// The periods judged waiting since the object last made progress.
  attempts: u32,
  /// The most members that had promised the object at any period of this round.
  promised: usize,
  /// The round's page advances ([`HostTakeover::page_advances`]) at the object's last progress.
  advances: u64,
}

impl PromiseWait {
  /// Counts one period in which the object was judged waiting, `promised` members having promised it and the round at
  /// `advances` page advances, and returns its attempts since its last progress. More members promising it than ever
  /// before, or the round paging further, is progress and starts the count again: a page still to come may list it, and
  /// a member answers by name only once its pages are complete. A period with neither counts one attempt.
  fn record(&mut self, promised: usize, advances: u64) -> u32 {
    if promised > self.promised || advances > self.advances {
      self.promised = self.promised.max(promised);
      self.advances = self.advances.max(advances);
      self.attempts = 0;
    } else {
      self.attempts = self.attempts.saturating_add(1);
    }
    self.attempts
  }
}

/// One asked member's progress through its answer: the object its next page starts after, and whether its
/// answer is complete.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
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
      fenced: None,
      named_nothing: BTreeMap::new(),
      page_advances: 0,
      waits: BTreeMap::new(),
      given_up: BTreeSet::new(),
      past_tolerance: BTreeMap::new(),
    }
  }

  /// Starts the round over at `generation` and `epoch`: every promise was for the old round, so every page is
  /// asked again. What was adopted stays adopted, and the page advances stay counted. What was given up is judged
  /// afresh: the new configuration may have retired the member it waited for, and an adoption then clears its record
  /// on the owner shard (`verbs::materialize_taken_over`).
  fn restart(&mut self, generation: u64, epoch: HostEpoch) {
    self.generation = generation;
    self.epoch = epoch;
    self.pages.clear();
    self.learned.clear();
    self.fenced = None;
    self.named_nothing.clear();
    self.waits.clear();
    self.given_up.clear();
    self.past_tolerance.clear();
  }

  /// Whether `object` is settled in this round, so no answer makes it learned again: adopted, or given up on.
  fn settled(&self, object: &ObjectId) -> bool {
    self.adopted.contains(object) || self.given_up.contains(object)
  }

  /// Records `host`'s answer to a page as its progress `page`, counting it in [`HostTakeover::page_advances`] when it
  /// moved the member on: a later next page, or its answer complete. A member that answers the same page again (a copy
  /// it could not promise keeps the page open where it was) has not advanced the round.
  fn advance_page(&mut self, host: HostId, page: Page) {
    if self.pages.insert(host, page) != Some(page) {
      self.page_advances = self.page_advances.saturating_add(1);
    }
  }

  /// The objects learned and not yet adopted.
  pub fn outstanding(&self) -> usize {
    self.learned.len()
  }
}

/// The retired hosts whose takeover round on this node is still paging (§4.8 phase one), each with its page advances
/// so far ([`HostTakeover::page_advances`]): its configuration moved on or a fence sends it higher, so it starts over,
/// or a member it asks has not answered every page. While a round pages, an object it adopted may still be joined by
/// another register of the same object from a later page. A catalog register's class byte sorts it after every object
/// of its creator's partition (`ObjectId::catalog`), and a round reads one page a member a period ([`page_budget`],
/// about 55 objects), so a large host's catalog registers come many periods after their heads. The takeover bound
/// (`fleet::materialize_pending`, `fleet::materialize_pending_greens`) takes an advance of the round that brings such a
/// register as progress of every object waiting on it, and counts the wait only while the round stands still.
pub(crate) fn rounds_still_paging(state: &ShardState) -> BTreeMap<HostId, u64> {
  let regional = state.council.configuration();
  let local = state.fleet.host();
  state
    .host_takeovers
    .iter()
    .filter(|(departed, take)| {
      take.generation != regional.version
        || take.fenced.is_some()
        || pages_outstanding(
          &take.pages,
          regional
            .recovery_hosts(**departed)
            .into_iter()
            .filter(|host| *host != local && regional.members.contains(host)),
        )
    })
    .map(|(departed, take)| (*departed, take.page_advances))
    .collect()
}

/// Whether a member among `asked` has not answered every page: `pages` holds each asked member's progress, and a member
/// never asked yet has none.
fn pages_outstanding(
  pages: &BTreeMap<HostId, Page>,
  asked: impl IntoIterator<Item = HostId>,
) -> bool {
  asked
    .into_iter()
    .any(|host| !pages.get(&host).is_some_and(|page| page.complete))
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
/// - a copy whose line ends in a takeover the configuration keeps a completion record of is stale when this holder
///   took part in that takeover (`RegionalConfiguration::settled_for`), as is one whose successor has confirmed its
///   share while this holder, asked in that round, is outside the successor's placement (the successor's adoption never
///   reaches it); a stale copy is dropped unless this holder still holds its content, which the successor may still
///   fetch from it ([`let_go_stale`], A-335); a holder the round never asked keeps its copy, counted (A-309);
/// - an object no survivor can recover is left as it is, counted by the successor that learns it.
pub(crate) fn resolve_held_objects(state: &mut ShardState, local: HostId) {
  let regional = state.council.configuration().clone();
  let held: Vec<ObjectId> = state.holder_records.keys().copied().collect();
  for object in held {
    let Some(owner) = state.fleet.object_owner(object) else {
      continue;
    };
    if regional.members.contains(&owner) && !regional.retired.contains_key(&owner) {
      match stale_after_confirmation(state, &regional, object, owner, local) {
        Confirmation::Stale => let_go_stale(state, object),
        Confirmation::Unasked => state.count(TAKEOVER_KEPT_UNASKED, 1),
        Confirmation::None => {}
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
      // Lost or unresolved: nothing shows the object adopted anywhere, so this holder keeps its copy (A-272), counted so
      // it shows what it keeps that no takeover will recover (A-340).
      Lineage::Lost => state.count(TAKEOVER_KEPT_LOST, 1),
      Lineage::Unresolved => state.count(TAKEOVER_KEPT_UNRESOLVED, 1),
      Lineage::Settled { takeover } => {
        if regional.settled_for(takeover, local) {
          let_go_stale(state, object);
        } else {
          state.count(TAKEOVER_KEPT_UNASKED, 1);
        }
      }
    }
  }
}

/// Lets a stale copy of `object` go ([`reclaim`]) unless this holder still holds content for it once its retention
/// rule has run (`crate::content_retention::retain_object`): content its newest record names it for, or content placed
/// for a sequence ahead of its records. Such a copy is kept whole, counted [`TAKEOVER_KEPT_HOLDING_CONTENT`] (A-335). The
/// record is stale, but the successor's adopted head may name this holder among the content's holders: it confirmed
/// once the record was adopted, before its materialization fetched the content from the head's holders, and with the
/// dead owner the only other holder this copy is the one it fetches. AUD-29-43's rule is the same one: a holder keeps
/// content for a head it still backs, and the adopted head still names it. Before, the reclaim dropped the content with
/// the record (the bug record of 2026-11-18).
fn let_go_stale(state: &mut ShardState, object: ObjectId) {
  crate::content_retention::retain_object(state, object);
  if state.held_content.newest_placed(object).is_some() {
    state.count(TAKEOVER_KEPT_HOLDING_CONTENT, 1);
    return;
  }
  reclaim(state, object);
}

/// Whether this holder's copy of `object`, now owned by the member `owner` after a takeover, is stale: its
/// retired host's takeover names `owner` among the confirmed, so `owner` adopted every object it learned, this holder
/// was asked in that round (one of its survivors, A-309), so `owner` learned this copy and adopted the object, and this
/// holder is outside `owner`'s placement of it, so the adoption never reaches this copy. A holder the round never asked
/// (one an earlier neighbourhood of the retired host reached) keeps its copy: `owner` may have confirmed without ever
/// learning the object (`crates/db/tests/lineage_model.rs` found it). The confirmation is read from the kept retirement
/// or, once the takeover completed, from its completion record (A-309): the last confirmation drops the retirement in
/// the very version that records it, and judged by the kept retirement alone a holder outside the placement never saw
/// that confirmation and kept its stale copy for good.
fn stale_after_confirmation(
  state: &ShardState,
  regional: &RegionalConfiguration,
  object: ObjectId,
  owner: HostId,
  local: HostId,
) -> Confirmation {
  let Some(departed) = state.departed_owners.get(&object) else {
    return Confirmation::None;
  };
  let (confirmed, asked) = match (
    regional.retired.get(&departed.owner),
    regional.completed.get(&departed.owner),
  ) {
    (Some(retirement), _) => (
      retirement.confirmed.contains(&owner),
      retirement.survivors.contains(&local),
    ),
    (None, Some(record)) => (
      record.confirmed.contains(&owner),
      record.confirmed.contains(&local),
    ),
    (None, None) => (false, false),
  };
  // The placement is computed only for a confirmed successor: every install resolves every held copy, and an owner's
  // placement view is built anew each time (its domains and neighbourhood copied, its copysets laid out).
  if !confirmed {
    return Confirmation::None;
  }
  let outside = owner != local
    && regional
      .configuration_for(owner)
      .is_some_and(|placement| !placement.place(object).candidates.contains(&local));
  match (outside, asked) {
    (true, true) => Confirmation::Stale,
    (true, false) => Confirmation::Unasked,
    (false, _) => Confirmation::None,
  }
}

/// What a successor's confirmation says of this holder's copy routed to it ([`stale_after_confirmation`]).
enum Confirmation {
  /// The successor confirmed its share and this holder, outside its placement, was asked in that round: stale.
  Stale,
  /// The successor confirmed and this holder is outside its placement, but the round never asked it: kept, counted
  /// [`TAKEOVER_KEPT_UNASKED`] (A-309).
  Unasked,
  /// No confirmation that judges the copy: none yet, or this holder is within the successor's placement.
  None,
}

/// Forgets `object`'s owner in this shard's routing and in every other shard's: the end [`track_owner_on_every_shard`]
/// owes once the object's registers are retired (AUD-29-43), or every shard but this one kept the entry for good. A shard
/// the message does not reach is counted (`xshard.run_refused`).
pub(crate) fn forget_owner_on_every_shard(state: &mut ShardState, object: ObjectId) {
  state.fleet.forget_object(object);
  let origin = state.shard;
  for shard in state
    .shards
    .iter()
    .copied()
    .filter(|shard| *shard != origin)
  {
    crate::xshard::run_on_counted(origin, shard, move |s| {
      s.fleet.forget_object(object);
    });
  }
}

/// Drops this holder's copy of `object`: its acceptor, its routing entry, its lease-gate record, a green's
/// replica, and whatever content it held for the object, a partial stage included (AUD-29-43: a stale copy's bytes are
/// reclaimed with its records, never kept for a head this holder no longer backs). Reached only through
/// [`let_go_stale`], once the retention rule left no whole content for the object here (A-335). Counted
/// [`TAKEOVER_RECLAIMED`].
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
  if state.injected_takeover_silence.contains(&peer_host) {
    state.count(TAKEOVER_SILENCED_BY_FAULT, 1);
    return Vec::new();
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

/// Counter: copies a page listed, or a successor learned of its own, by the owner an install moved their authority from
/// (A-251 (b), shape (i)): the authority alone named the successor, and only the moved-from owner placed the copy in this
/// round. The non-vacuity count of that half of the rule. Format: a counter name in the daemon's status report.
const TAKEOVER_LISTED_BY_MOVED_FROM: &str = "fleet.takeover.listed_by_moved_from";

/// Takes the copies of `departed`'s objects this holder holds that fall to `asker` in its round (A-251 (b), shape (i),
/// one rule with no routing in it: [`copy_falls_to`]), counting those only the moved-from owner placed there
/// ([`TAKEOVER_LISTED_BY_MOVED_FROM`]).
fn copies_falling_to(
  state: &mut ShardState,
  departed: HostId,
  asker: HostId,
  after: Option<ObjectId>,
) -> Vec<ObjectId> {
  let regional = state.council.configuration().clone();
  let mut by_moved_from: u64 = 0;
  let objects: Vec<ObjectId> = state
    .holder_records
    .iter()
    .filter(|(object, _)| after.is_none_or(|after| **object > after))
    .filter(|(object, acceptor)| {
      let falls = copy_falls_to(&regional, acceptor, **object, departed, asker);
      let by_authority = falls_by_authority(&regional, acceptor.owner(), **object, departed, asker);
      by_moved_from = by_moved_from.saturating_add(u64::from(falls && !by_authority));
      falls
    })
    .map(|(object, _)| *object)
    .collect();
  if by_moved_from > 0 {
    state.count(TAKEOVER_LISTED_BY_MOVED_FROM, by_moved_from);
  }
  objects
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
  let objects = copies_falling_to(state, prepare.departed, prepare.owner, prepare.after);
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
  let retired: BTreeSet<HostId> = state
    .council
    .configuration()
    .retired
    .keys()
    .copied()
    .collect();
  for object in objects {
    let Some(acceptor) = state.holder_records.get_mut(&object) else {
      continue;
    };
    let from_retired = retired.contains(&acceptor.owner());
    match promise_object(acceptor, object, prepare, from_retired) {
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
/// takeover this node no longer owes (confirmed, or its retirement dropped) is forgotten, and a round the retirement
/// bound ended before its takeover completed first gives up by name every object it had learned
/// ([`rounds_the_bound_ended`], [`give_up_dropped`], A-340). Returns the dispatches still in flight, whose sessions the
/// coordinator recovers.
pub(crate) async fn drive_takeovers(local: HostId, budget: CommitBudget) -> Vec<Dispatch> {
  let (owed, ended) = state::with_state(|s| owed_rounds(s, local)).unwrap_or_default();
  for (departed, learned) in ended {
    give_up_dropped(departed, learned, local).await;
  }
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
  let past = state::with_state(|s| unpromised_past_the_bound(s, departed)).unwrap_or_default();
  give_up_unpromised(departed, past, local).await;
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
/// cohorts it is a member, settled or current (the neighbourhood a change in flight reached, whose every surviving
/// member must answer for an object past tolerance, A-309), that it neither listed nor named as holding nothing, at most
/// [`named_per_question`].
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
          .chain(regional.current_cohorts(departed, **object).iter())
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
    .filter(|(object, acceptor)| copy_falls_to(regional, acceptor, **object, departed, local))
    .flat_map(|(_, acceptor)| acceptor.persisted().1)
    .try_fold(bumped, |highest, (_, _, epoch, _)| {
      Some(highest.max(HostEpoch(epoch.0.checked_add(1)?)))
    })
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
  let objects = copies_falling_to(state, departed, local, None);
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
  let retired: BTreeSet<HostId> = state
    .council
    .configuration()
    .retired
    .keys()
    .copied()
    .collect();
  for object in objects {
    let Some(acceptor) = state.holder_records.get_mut(&object) else {
      continue;
    };
    let from_retired = retired.contains(&acceptor.owner());
    match promise_object(acceptor, object, &prepare, from_retired) {
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
    if !take.settled(&object) {
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
      if !take.settled(&object) {
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
      take.advance_page(host, page);
    }
  }
}

/// The objects of `departed`'s takeover ready to adopt, each with the newest record reported for it: every
/// one of its recovery cohorts has its recovery quorum of members that promised it — `cohort − f`, `f + 1` at
/// the floor, at least one ([`slates_db::register::Quorum::recovery`]) — this node, a member that listed it, or one
/// that answered for it by name. An object with a cohort left with fewer members than that quorum (more of it failed
/// than the region tolerates) is ready once every surviving member of its cohorts, settled and current, has promised
/// it, and is adopted with the newest record any of them holds, counted [`TAKEOVER_RECOVERED_PAST_TOLERANCE`] (A-309):
/// the members that exist now recover it, the replacement holding a live log among them. Before A-309 it was lost,
/// counted and never adopted. The rule is [`slates_db::takeover::readiness`] under [`CountingRule::NamedAnswer`]
/// (A-251); the model in `crates/db/tests/takeover_model.rs` finds where counting a complete page instead adopts an
/// older record than a committed one, and holds the past-tolerance adoption to the newest surviving record.
pub(crate) fn ready_objects(
  state: &mut ShardState,
  departed: HostId,
  local: HostId,
) -> Vec<(ObjectId, Accepted)> {
  let regional = state.council.configuration().clone();
  let local_promises = gate_open(state, departed);
  if local_promises {
    answer_own_copies_by_name(state, departed, local, &regional);
  }
  let bound = crate::fleet::takeover_attempts(state.config.failover_slo_ns);
  let Some(take) = state.host_takeovers.get_mut(&departed) else {
    return Vec::new();
  };
  let mut ready = Vec::new();
  let mut past_tolerance: u64 = 0;
  let mut unpromised: u64 = 0;
  let mut past_the_bound: u64 = 0;
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
    let current = regional.current_cohorts(departed, *object);
    match readiness(
      &cohorts,
      &current,
      |host| regional.members.contains(&host),
      regional.quorum,
      &heard,
      CountingRule::NamedAnswer,
    ) {
      Readiness::Ready => {
        // Its promises are in: an adoption short of placement is tried again, never counted toward the bound.
        take.waits.remove(object);
        if let Some(newest) = reports.values().cloned().reduce(newest_of) {
          ready.push((*object, newest));
        }
      }
      Readiness::ReadyPastTolerance {
        needed, members, ..
      } => {
        // Every surviving member of its cohorts has promised (A-309): adopted as a ready one is, counted, and the cohort
        // it fell short in kept for the volume's loss report (A-332).
        take.waits.remove(object);
        if let Some(newest) = reports.values().cloned().reduce(newest_of) {
          past_tolerance = past_tolerance.saturating_add(1);
          take
            .past_tolerance
            .insert(*object, shortfall(needed, members));
          ready.push((*object, newest));
        }
      }
      Readiness::Waiting => {
        unpromised = unpromised.saturating_add(1);
        let promised = promisers(reports, take.named_nothing.get(object));
        let attempts = take
          .waits
          .entry(*object)
          .or_default()
          .record(promised, take.page_advances);
        if attempts >= bound {
          past_the_bound = past_the_bound.saturating_add(1);
        }
      }
    }
  }
  if unpromised > 0 {
    let count = state.refusals.entry(TAKEOVER_UNPROMISED).or_insert(0);
    *count = count.saturating_add(unpromised);
  }
  if past_the_bound > 0 {
    let count = state
      .refusals
      .entry(TAKEOVER_UNPROMISED_PAST_BOUND)
      .or_insert(0);
    *count = count.saturating_add(past_the_bound);
  }
  // An object that left the learned (adopted or given up) waits no more.
  take
    .waits
    .retain(|object, _| take.learned.contains_key(object));
  take
    .past_tolerance
    .retain(|object, _| take.learned.contains_key(object));
  if past_tolerance > 0 {
    state.count(TAKEOVER_RECOVERED_PAST_TOLERANCE, past_tolerance);
  }
  ready
}

/// The shortfall a readiness past tolerance judged (A-332): a cohort's recovery quorum `needed` and the `members` of it
/// left, as the loss report counts them.
fn shortfall(needed: usize, members: usize) -> slates_db::catalog::RecoveryShortfall {
  slates_db::catalog::RecoveryShortfall {
    needed: u64::try_from(needed).unwrap_or(u64::MAX),
    remaining: u64::try_from(members).unwrap_or(u64::MAX),
  }
}

/// The members that promised one learned object: those that listed it, with the highest record each holds, and those
/// that answered for it by name holding nothing of it. A member is counted once, however it promised.
fn promisers(
  reports: &BTreeMap<HostId, Accepted>,
  named_nothing: Option<&BTreeSet<HostId>>,
) -> usize {
  let named = named_nothing
    .into_iter()
    .flatten()
    .filter(|host| !reports.contains_key(host))
    .count();
  reports.len().saturating_add(named)
}

/// The learned objects of `departed`'s round that waited for their cohorts' promises past the bound, each with its
/// attempts: as many periods as the failover SLO holds ([`crate::fleet::takeover_attempts`]) with no member promising it
/// anew and no page of the round advancing. [`ready_objects`] counts the periods; [`drive_host_takeover`] gives these up
/// ([`give_up_unpromised`]).
pub(crate) fn unpromised_past_the_bound(
  state: &ShardState,
  departed: HostId,
) -> Vec<(ObjectId, u32)> {
  let bound = crate::fleet::takeover_attempts(state.config.failover_slo_ns);
  state
    .host_takeovers
    .get(&departed)
    .map(|take| {
      take
        .waits
        .iter()
        .filter(|(object, wait)| wait.attempts >= bound && take.learned.contains_key(*object))
        .map(|(object, wait)| (*object, wait.attempts))
        .collect()
    })
    .unwrap_or_default()
}

/// Gives up on `departed`'s learned objects in `past` ([`unpromised_past_the_bound`], each with its attempts), which no
/// member cut off from this node alone will ever promise, since the council never retires it, so nothing else ends their
/// wait. Each object's owner shard records it unrecovered, `TakeoverIncomplete { waited_on: "promises" }`, so every verb
/// on it answers `NotRecovered` where it answered a retryable `Overloaded`; this node claims it to lookups, so a read from
/// another node meets that answer; and the round learns it no more, asks no more about it, and confirms no share while it
/// is given up, so its holders keep their copies (A-272's rule). A catalog register has no verbs of its own: the volume it
/// belongs to waits for it in its materialization and is given up there, naming `catalog`. With no shard for its partition
/// here nothing is recorded or claimed, as for a taken-over volume ([`crate::fleet`]'s give-up). Bounded work: one call
/// per owner shard, whatever the objects' number, and one log line; an owner shard that does not answer within the period
/// leaves its objects learned, given up again the next.
async fn give_up_unpromised(departed: HostId, past: Vec<(ObjectId, u32)>, local: HostId) {
  if past.is_empty() {
    return;
  }
  let most_attempts = past
    .iter()
    .map(|(_, attempts)| *attempts)
    .max()
    .unwrap_or(0);
  let objects: Vec<ObjectId> = past.iter().map(|(object, _)| *object).collect();
  let settled = record_given_up(&objects, UNPROMISED_WAIT).await;
  let Some(first) = settled.first().map(|(object, _)| *object) else {
    return;
  };
  let given_up = u64::try_from(settled.len()).unwrap_or(u64::MAX);
  let claimed = settled.iter().filter(|(_, claimed)| *claimed).count();
  state::with_state_counted(|s| {
    if let Some(take) = s.host_takeovers.get_mut(&departed) {
      for (object, _) in &settled {
        take.learned.remove(object);
        take.waits.remove(object);
        take.named_nothing.remove(object);
        take.given_up.insert(*object);
      }
    }
    claim_given_up(s, &settled, local);
    s.count(TAKEOVER_GIVEN_UP, given_up);
  });
  slates_machine::log_line!(
    "slates daemon: the takeover of {}'s objects gave up {given_up} of them (the first {}) after up to {most_attempts} \
     periods, the failover SLO's worth, with no member of their recovery cohorts promising them anew and no page of the \
     round advancing; {claimed} recorded unrecovered on their shards, every verb on one answering NotRecovered (a \
     catalog register is given up with its volume, and an object whose partition no shard here serves is dropped)",
    departed.0,
    crate::verbs::hex_of(&first.0)
  );
}

/// The takeovers this node owes a confirmation of under the configuration it holds, with the rounds among those it held
/// that the retirement bound ended before their takeover completed, each with what it had learned
/// ([`rounds_the_bound_ended`], A-340); every round it no longer owes is forgotten. The first step of
/// [`drive_takeovers`], on the control shard.
fn owed_rounds(
  state: &mut ShardState,
  local: HostId,
) -> (Vec<HostId>, Vec<(HostId, Vec<ObjectId>)>) {
  let regional = state.council.configuration();
  let owed: Vec<HostId> = regional
    .retired
    .keys()
    .copied()
    .filter(|departed| regional.owes_confirmation(*departed, local))
    .collect();
  let ended = rounds_the_bound_ended(state, &owed);
  state
    .host_takeovers
    .retain(|departed, _| owed.contains(departed));
  (owed, ended)
}

/// The rounds of this node's that the retirement bound ended before their takeover completed (A-340), each with the
/// objects it had learned and not adopted: every takeover this node held a round of (`ShardState::host_takeovers`) that
/// it no longer owes (`owed`), whose retirement the configuration no longer keeps and whose completion it does not
/// record. A takeover whose share this node confirmed had nothing learned left (a share confirms only once every object
/// it learned is adopted, [`share_done`]), and one that completed is recorded, so neither names an object here. Run on
/// the control shard before [`drive_takeovers`] forgets the rounds it no longer owes, which otherwise dropped what the
/// round had learned without a word.
pub(crate) fn rounds_the_bound_ended(
  state: &ShardState,
  owed: &[HostId],
) -> Vec<(HostId, Vec<ObjectId>)> {
  let regional = state.council.configuration();
  state
    .host_takeovers
    .iter()
    .filter(|(departed, _)| {
      !owed.contains(departed)
        && !regional.retired.contains_key(departed)
        && !regional.completed.contains_key(departed)
    })
    .map(|(departed, take)| {
      (
        *departed,
        take.learned.keys().copied().collect::<Vec<ObjectId>>(),
      )
    })
    .filter(|(_, learned)| !learned.is_empty())
    .collect()
}

/// Gives up by name every object `learned` in this node's round of `departed`'s takeover, which the retirement bound let
/// go before it completed ([`rounds_the_bound_ended`], A-340): with the retirement gone no round will ever recover them
/// (its successors were ranked through it, and the configuration keeps nothing that ranks them again), so each owner
/// shard records the object unrecovered, `TakeoverIncomplete { waited_on: "retirement" }`, every verb on it answers
/// `NotRecovered` rather than a retryable `OwnerUnreachable`, and this node, the successor the round ranked, claims it to
/// lookups. Its holders keep their copies, the line now unresolved (counted `fleet.takeover.kept_unresolved` on each).
/// Counted [`TAKEOVER_GIVEN_UP`] and [`TAKEOVER_GIVEN_UP_DROPPED`], with one log line. An owner shard that does not
/// answer within the period leaves its objects unrecorded: the round is forgotten here, so they are not given up again
/// (one attempt, as the bound's drop is one event), and are answered as any volume no member serves.
async fn give_up_dropped(departed: HostId, learned: Vec<ObjectId>, local: HostId) {
  let settled = record_given_up(&learned, RETIREMENT_WAIT).await;
  let Some(first) = settled.first().map(|(object, _)| *object) else {
    return;
  };
  let given_up = u64::try_from(settled.len()).unwrap_or(u64::MAX);
  let claimed = settled.iter().filter(|(_, claimed)| *claimed).count();
  state::with_state_counted(|s| {
    claim_given_up(s, &settled, local);
    s.count(TAKEOVER_GIVEN_UP, given_up);
    s.count(TAKEOVER_GIVEN_UP_DROPPED, given_up);
  });
  slates_machine::log_line!(
    "slates daemon: the retirement bound let {}'s takeover go before it completed, while this node's round of it had \
     learned {given_up} objects it had not adopted (the first {}); {claimed} recorded unrecovered on their shards, every \
     verb on one answering NotRecovered naming the retirement; their holders keep their copies (a catalog register is \
     given up with its volume, and an object whose partition no shard here serves is dropped)",
    departed.0,
    crate::verbs::hex_of(&first.0)
  );
}

/// Records each of `objects` unrecovered on its owner shard, `RecoveryCause::TakeoverIncomplete { waited_on }`, so every
/// verb on it answers `NotRecovered`: the give-up a takeover round makes by name ([`give_up_unpromised`],
/// [`give_up_dropped`]). A catalog register has no verbs of its own (its volume waits for it in its materialization and
/// is given up there, naming `catalog`), and an object whose partition no shard here serves has nowhere to be recorded:
/// each is settled unrecorded. Returns every object settled, each with whether its owner shard recorded it; an owner
/// shard that does not answer within the period settles none of its objects. Bounded work: one call per owner shard,
/// whatever the objects' number.
async fn record_given_up(objects: &[ObjectId], waited_on: &'static str) -> Vec<(ObjectId, bool)> {
  let Some(origin) = state::with_state(|s| s.shard) else {
    return Vec::new();
  };
  // Each object's owner shard, `None` for a catalog register (nothing to record) or a partition with no shard here.
  let mut by_shard: BTreeMap<u16, Vec<ObjectId>> = BTreeMap::new();
  let mut settled: Vec<(ObjectId, bool)> = Vec::new();
  for object in objects {
    let target = (!object.is_catalog())
      .then(|| {
        let partition = crate::verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
        state::with_state(|s| s.shards.get(usize::from(partition)).copied()).flatten()
      })
      .flatten();
    match target {
      Some(shard) => by_shard.entry(shard).or_default().push(*object),
      None => settled.push((*object, false)),
    }
  }
  for (shard, objects) in by_shard {
    let ids: Vec<slates_db::catalog::VolumeId> = objects
      .iter()
      .map(|object| slates_db::catalog::VolumeId { bytes: object.0 })
      .collect();
    let recorded = crate::xshard::call_within(
      origin,
      shard,
      move |s| {
        for id in ids {
          s.unrecovered.insert(
            id,
            slates_ipc::protocol::RecoveryCause::TakeoverIncomplete {
              waited_on: waited_on.to_owned(),
            },
          );
        }
      },
      crate::daemon::HEARTBEAT_NS,
    )
    .await
    .is_some();
    if recorded {
      settled.extend(objects.into_iter().map(|object| (object, true)));
    }
  }
  settled
}

/// Claims every recorded object of `settled` ([`record_given_up`]) as this node's in every shard's routing, so a read
/// from another node meets its `NotRecovered` answer here.
fn claim_given_up(state: &mut ShardState, settled: &[(ObjectId, bool)], local: HostId) {
  for (object, _) in settled.iter().filter(|(_, claimed)| *claimed) {
    track_owner_on_every_shard(state, *object, local);
  }
}

/// The successor answers itself by name (A-251 (b)), as it asks any member: for each learned object it has neither
/// reported nor named, a direct look-up of its own records ([`answer_by_name`]). Its own copies are learned through its
/// routing (`promise_local_copies`), which can lose one as any holder's can, so its silence is no promise. Run only
/// while its lease gate is open, as every promise of its own is.
fn answer_own_copies_by_name(
  state: &mut ShardState,
  departed: HostId,
  local: HostId,
  regional: &RegionalConfiguration,
) {
  let Some(take) = state.host_takeovers.get(&departed) else {
    return;
  };
  let named: Vec<ObjectId> = take
    .learned
    .iter()
    .filter(|(object, reports)| {
      !reports.contains_key(&local)
        && !take
          .named_nothing
          .get(*object)
          .is_some_and(|hosts| hosts.contains(&local))
    })
    .map(|(object, _)| *object)
    .collect();
  if named.is_empty() {
    return;
  }
  let prepare = HostPrepare {
    departed,
    owner: local,
    epoch: take.epoch,
    generation: take.generation,
    after: None,
    named,
  };
  let answer = answer_by_name(
    local,
    &prepare,
    regional,
    &mut state.holder_records,
    usize::MAX,
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
  fold_answers(state, departed, vec![(local, answer.promise, true)]);
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
  // The members other than this node that reported the adopted position: a green's successor fetches any input it
  // lacks from them (`fleet::fetch_green_inputs`), since each accepted that version holding its inputs.
  let mut reported_adopted = Vec::new();
  let mut registers_short = None;
  if let Some(take) = state.host_takeovers.get_mut(&departed) {
    registers_short = take.past_tolerance.remove(&object);
    if let Some(reports) = take.learned.remove(&object) {
      reported_adopted = reports
        .into_iter()
        .filter(|(host, accepted)| *host != local && accepted.sequence == record.sequence)
        .map(|(host, _)| host)
        .collect();
    }
    take.adopted.insert(object);
  }
  if object.is_catalog() {
    // A volume's catalog register: its successor rebuilds the volume under it (AUD-29-17) — unless the
    // volume's own register was adopted as a tombstone (AUD-29-43): a destroyed volume is not rebuilt. A work's catalog
    // is the route a resend of its submits takes (A-255), which the route keeper ends once nothing can use it (A-288).
    if let Some(catalog) = crate::catalog::CatalogValue::from_record_bytes(&record.value)
      && !state
        .pending_tombstones
        .contains_key(&object.placement_key())
    {
      let route = catalog.work_of.is_some();
      state
        .pending_catalogs
        .insert(object.placement_key(), (record.sequence, catalog));
      if route {
        crate::work_route::nudge(state);
      }
    }
  } else if let Some(tombstone) = crate::tombstone::TombstoneValue::from_record_bytes(&record.value)
  {
    // A destroyed volume (AUD-29-43): the successor owes its tombstone from the adopted stage on, and never
    // materializes it.
    state.pending_catalogs.remove(&object);
    state.pending_tombstones.insert(
      object,
      crate::state::PendingTombstone::adopted(tombstone.tombstone_sequence(record.sequence)),
    );
  } else if let Some(head) = HeadValue::from_record_bytes(&record.value) {
    // Where the retired owner placed the volume, its settled and current cohorts in this round: a live takeover past
    // tolerance asks every surviving one of them (A-309), and the retirement may be let go before the log is fetched.
    let regional = state.council.configuration();
    let mut cohort_hosts: Vec<HostId> = regional
      .recovery_cohorts(departed, object)
      .into_iter()
      .chain(regional.current_cohorts(departed, object))
      .flatten()
      .collect();
    cohort_hosts.sort_unstable_by_key(|host| host.0);
    cohort_hosts.dedup();
    let mut pending = crate::state::PendingVolume::adopted(head, Some(departed), cohort_hosts);
    pending.registers_short = registers_short;
    state.pending_materializations.insert(object, pending);
  } else {
    match crate::merge_service::MergeRecordValue::decode_record(&record.value) {
      // A green: its newest merge record was adopted; the owned green is rebuilt from this node's accepted chain
      // and held inputs on the shard its id routes to (AUD-14), fetching an input it lacks from the members that
      // reported the adopted version.
      Ok(merge) => {
        state.pending_green_materializations.insert(
          object,
          crate::state::PendingGreen::adopted(merge, Some(departed), reported_adopted),
        );
      }
      // A merge record of another format: adopted as bytes, never materialized from, and counted.
      Err(refused) => {
        if let Some(counter) = refused.counter() {
          state.count(counter, 1);
        }
      }
    }
  }
  let count = state.refusals.entry(TAKEOVER_ADOPTED).or_insert(0);
  *count = count.saturating_add(1);
}

/// Whether this node's share of `departed`'s takeover is done: every member of the recovery neighbourhoods it
/// asked has answered in full, and every object it learned is adopted, none given up ([`share_done`]).
fn round_done(state: &ShardState, departed: HostId, local: HostId) -> bool {
  let regional = state.council.configuration();
  let Some(take) = state.host_takeovers.get(&departed) else {
    return false;
  };
  share_done(
    take.generation == regional.version,
    take.fenced.is_none(),
    take.learned.is_empty(),
    take.given_up.is_empty(),
    regional
      .recovery_hosts(departed)
      .into_iter()
      .filter(|host| *host != local && regional.members.contains(host))
      .all(|host| take.pages.get(&host).is_some_and(|page| page.complete)),
  )
}

/// Whether a share is done, from its round's facts: under the current configuration, unfenced, every member's page
/// complete, and every object it learned adopted, none given up (A-272, A-304). A confirmation is what lets the takeover
/// complete, and a completed takeover settles its objects' lines for every holder it asked (A-309), so a round with an
/// object still learned or given up must not confirm: its retirement stays kept, and its holders keep their copies.
/// Before A-272 an object declared lost was dropped from those learned and the round confirmed as though it had been
/// adopted; since A-309 no object is declared lost (past tolerance the members that exist now recover it), and an
/// object given up on waiting for its cohorts' promises ([`give_up_unpromised`]) keeps the share open the same way.
fn share_done(
  current: bool,
  unfenced: bool,
  nothing_learned: bool,
  nothing_given_up: bool,
  every_page: bool,
) -> bool {
  current && unfenced && nothing_learned && nothing_given_up && every_page
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The takeover bound's paging gate (`rounds_still_paging`): a round still pages while any member it asks has not
  /// answered every page, a member never asked counting as not answered. Do: judge two asked members with one page
  /// complete and the other part-way, never asked, and complete. Expect paging until both are complete, and none once
  /// no member is asked.
  #[test]
  fn a_round_pages_until_every_asked_member_has_answered_every_page() {
    let (first, second) = (HostId(2), HostId(3));
    let complete = Page {
      after: None,
      complete: true,
    };
    let part_way = Page {
      after: Some(ObjectId::new(HostId(1), 7)),
      complete: false,
    };
    let asked = [first, second];
    let pages = BTreeMap::from([(first, complete), (second, part_way)]);
    assert!(
      pages_outstanding(&pages, asked),
      "one member part-way: paging"
    );
    let pages = BTreeMap::from([(first, complete)]);
    assert!(
      pages_outstanding(&pages, asked),
      "one member never asked: paging"
    );
    let pages = BTreeMap::from([(first, complete), (second, complete)]);
    assert!(
      !pages_outstanding(&pages, asked),
      "every member complete: done paging"
    );
    assert!(
      !pages_outstanding(&BTreeMap::new(), []),
      "no member asked: nothing to page"
    );
  }

  /// The takeover bound's progress of a paging round (`HostTakeover::page_advances`): a page answer advances the round
  /// only when it moves its member on, and a restart keeps the count. Do: fold a member's first page, the same page
  /// again (a copy it could not promise keeps the page open where it was), its next page, its completion, the
  /// completion again; restart the round; fold the first page again. Expect the count to move at the first page, the
  /// next, the completion and the first page after the restart, and nowhere else, so a member that answers without
  /// moving on reads as a round standing still and the bound counts the waits on it.
  #[test]
  fn a_round_advances_only_when_a_member_moves_on_and_a_restart_keeps_the_count() {
    let member = HostId(2);
    let mut take = HostTakeover::new(5, HostEpoch(3));
    let page = |after: u64, complete: bool| Page {
      after: (!complete).then_some(ObjectId::new(HostId(1), after)),
      complete,
    };
    let mut seen = Vec::new();
    for answer in [
      page(55, false),
      page(55, false),
      page(110, false),
      page(0, true),
      page(0, true),
    ] {
      take.advance_page(member, answer);
      seen.push(take.page_advances);
    }
    assert_eq!(seen, vec![1, 1, 2, 3, 3]);
    take.restart(6, HostEpoch(4));
    assert_eq!(
      take.page_advances, 3,
      "a restart keeps the round's progress counted"
    );
    take.advance_page(member, page(55, false));
    assert_eq!(
      take.page_advances, 4,
      "the first page asked again after the restart moves the member on"
    );
  }

  /// The bound on a learned object's wait for its cohorts' promises (`PromiseWait`): a period counts only when no member
  /// promised the object anew and no page of the round advanced. Do: record a first period with one member promising,
  /// three still periods, a second member's promise, two still periods, a page advance, then still periods to the
  /// failover SLO's attempts at a one-second SLO. Expect the count to start again at the first period, the promise and
  /// the advance, to count one a still period otherwise, and to reach the bound only after that many still periods in a
  /// row, so a round that still pages or still hears promises never gives an object up.
  #[test]
  fn a_promise_wait_counts_only_periods_with_no_new_promise_and_no_page_advance() {
    let mut wait = PromiseWait::default();
    let mut seen = Vec::new();
    for (promised, advances) in [
      (1, 4),
      (1, 4),
      (1, 4),
      (1, 4),
      (2, 4),
      (2, 4),
      (2, 4),
      (2, 5),
    ] {
      seen.push(wait.record(promised, advances));
    }
    assert_eq!(seen, vec![0, 1, 2, 3, 0, 1, 2, 0]);
    let bound = crate::fleet::takeover_attempts(crate::daemon::HEARTBEAT_NS.saturating_mul(10));
    let mut attempts = 0;
    for _ in 0..bound {
      attempts = wait.record(2, 5);
    }
    assert_eq!(
      attempts, bound,
      "the bound's worth of still periods in a row"
    );
    assert_eq!(
      wait.record(1, 5),
      bound.saturating_add(1),
      "fewer promisers than before are no progress"
    );
  }

  /// A-272, A-304, A-309: a round confirms its share only once every object it learned is adopted, none given up. Do ask
  /// whether a share is done with every page complete, with and without an object still learned or given up, and with a
  /// page missing. Expect done only with nothing learned, nothing given up and every page in. Before A-272 a lost object
  /// counted as done; since A-309 none is lost, and an object past tolerance stays learned until every surviving holder
  /// has answered and it is adopted, or is given up at A-304's bound.
  #[test]
  fn a_round_confirms_only_once_every_object_it_learned_is_adopted() {
    assert!(
      !share_done(true, true, false, true, true),
      "an object still learned: not done, the takeover stays kept"
    );
    assert!(
      !share_done(true, true, true, false, true),
      "an object given up: not done, the takeover stays kept"
    );
    assert!(
      share_done(true, true, true, true, true),
      "every object adopted: done"
    );
    assert!(
      !share_done(true, true, true, true, false),
      "a member's pages incomplete: not done"
    );
  }
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
    let refusal = promise_object(&mut acceptor, object, &prepare, false).unwrap_err();
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
    assert_eq!(
      promise_object(&mut empty, object, &prepare, false),
      Ok(None)
    );
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
      promise_object(&mut holding, object, &prepare, false),
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

  /// Installs the audit node's council configuration into its placement view, as the coordinator's council sync does.
  fn install_council(state: &mut ShardState) {
    let local = state.fleet.host();
    let regional = state.council.configuration().clone();
    if let Some(configuration) = regional.configuration_for(local) {
      state
        .fleet
        .install_configuration(configuration, &regional.members);
    }
  }

  /// A-340's region: seven members at `f = 1` whose every neighbourhood is the whole region, the audit node the
  /// council's sole voter, so each proposal commits as it is made and the audit node survives every takeover. Returns
  /// the six other members.
  fn whole_region(state: &mut ShardState) -> Vec<HostId> {
    use slates_cluster::config_group::RegionalCouncil;
    let local = state.fleet.host();
    let others: Vec<HostId> = (1..7).map(|key| HostId(local.0 ^ key)).collect();
    let mut members = vec![local];
    members.extend(others.iter().copied());
    // Shape: a neighbourhood of every member.
    let scatter = 7;
    state.council = RegionalCouncil::new(
      local,
      members,
      vec![local],
      slates_db::register::Quorum { f: 1 },
      BTreeMap::new(),
      scatter,
      false,
    );
    install_council(state);
    others
  }

  /// A round of the audit node's for `departed`'s takeover that has learned `object` and adopted nothing.
  fn round_that_learned(state: &mut ShardState, departed: HostId, object: ObjectId) {
    let mut take = HostTakeover::new(state.council.configuration().version, HostEpoch(2));
    take.learned.insert(object, BTreeMap::new());
    state.host_takeovers.insert(departed, take);
  }

  /// A-340: the retirement bound ends a takeover round before its takeover completes, and the round names what it had
  /// learned while the holders of its objects keep their copies, counted. Do: in a seven-member region, take a member
  /// over and have every survivor confirm, so it completes, with a round of the audit node's still recorded for it; take
  /// a second member over with the audit node's round having learned one of its objects and the audit node holding a
  /// copy of another, and have three other survivors confirm it, the audit node and one more still owing their shares;
  /// then take those three over, so four retirements are kept with two members left and the bound (one per member) lets
  /// the oldest outstanding one go, and with the next oldest every retirement that carries it; then take the step that
  /// opens each period's takeovers (`owed_rounds`), install and resolve the held copy. Expect the second member's
  /// takeover neither kept nor completed; the step to have forgotten both rounds and named exactly the one the bound
  /// ended, with its learned object (a completed takeover's round names nothing); and the held copy kept on its now
  /// unresolved line, counted `fleet.takeover.kept_unresolved`. Before, the round was forgotten without a word, and the
  /// copy kept uncounted.
  #[test]
  fn a_round_the_retirement_bound_ended_names_what_it_had_learned() {
    let (dropped, ended, kept, held) = crate::daemon::audit_on_shard(|state| {
      use slates_cluster::config_group::Reconfiguration;
      let local = state.fleet.host();
      let others = whole_region(state);
      let (completed, departed) = (others[5], others[0]);
      assert!(state.council.propose(Reconfiguration::TakeOver(completed)));
      for survivor in state.council.configuration().survivors_of(completed) {
        state.council.propose(Reconfiguration::Confirm {
          departed: completed,
          successor: survivor,
        });
      }
      assert!(
        state
          .council
          .configuration()
          .completed
          .contains_key(&completed)
      );
      round_that_learned(state, completed, ObjectId::new(completed, 0));
      assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
      let learned = ObjectId::new(departed, 0);
      round_that_learned(state, departed, learned);
      let held_object = ObjectId::new(departed, 1);
      let generation = state.fleet.configuration().version;
      let mut acceptor = Acceptor::new(
        local,
        Authority {
          generation,
          owner: departed,
        },
      );
      acceptor
        .accept(&Record {
          owner: departed,
          object: held_object,
          sequence: 0,
          epoch: HostEpoch(1),
          generation,
          value: b"head".to_vec(),
        })
        .unwrap();
      state.holder_records.insert(held_object, acceptor);
      state.fleet.track_object_owner(held_object, departed);
      // Three survivors finish their shares and leave, so their own takeovers carry nothing of it.
      for survivor in &others[1..4] {
        assert!(state.council.propose(Reconfiguration::Confirm {
          departed,
          successor: *survivor,
        }));
      }
      for gone in &others[1..4] {
        assert!(state.council.propose(Reconfiguration::TakeOver(*gone)));
      }
      let regional = state.council.configuration().clone();
      let dropped =
        !regional.retired.contains_key(&departed) && !regional.completed.contains_key(&departed);
      let (_, ended) = owed_rounds(state, local);
      assert!(
        !state.host_takeovers.contains_key(&departed)
          && !state.host_takeovers.contains_key(&completed),
        "both rounds are forgotten, the takeover owed no more"
      );
      install_council(state);
      resolve_held_objects(state, local);
      (
        dropped,
        ended,
        state
          .refusals
          .get(TAKEOVER_KEPT_UNRESOLVED)
          .copied()
          .unwrap_or(0),
        state.holder_records.contains_key(&held_object),
      )
    });
    assert!(dropped, "the bound let the takeover go before it completed");
    assert_eq!(
      ended.len(),
      1,
      "the bound ended exactly the audit node's round of it: {ended:?}"
    );
    assert_eq!(
      ended.first().map(|(_, learned)| learned.len()),
      Some(1),
      "naming the one object it had learned: {ended:?}"
    );
    assert!(held, "the copy on the unresolved line is kept");
    assert_eq!(kept, 1, "and counted");
  }

  /// A-340: a round the retirement bound ended gives every volume it had learned up by name. Do: on an audit daemon,
  /// give a departed member's volume and its catalog register up as a round the bound ended does, and let the shard run
  /// it. Expect the volume recorded unrecovered on its shard, `TakeoverIncomplete { waited_on: "retirement" }`, so every
  /// verb on it answers `NotRecovered`; claimed by the audit node in its routing; and both registers counted given up,
  /// `fleet.takeover.given_up` and `fleet.takeover.given_up.retirement_dropped` (the catalog register has no verbs of
  /// its own and is settled unrecorded, as A-304's give-up settles it).
  #[test]
  fn a_round_the_retirement_bound_ended_gives_its_learned_volumes_up_by_name() {
    let daemon = crate::daemon::audit_daemon();
    let (volume, local) = crate::daemon::observe_first(&daemon, |state| {
      let local = state.fleet.host();
      let departed = HostId(local.0 ^ 1);
      let volume = ObjectId::new(departed, 0);
      let task = slates_rt::futures::spawn(give_up_dropped(
        departed,
        vec![volume, volume.catalog()],
        local,
      ))
      .expect("the give-up is spawned on the shard");
      slates_rt::futures::detach(task).expect("and detached");
      (volume, local)
    });
    /// Shape: the observations the shard is given to run the spawned give-up, which takes one poll.
    const OBSERVATIONS: usize = 64;
    let mut seen = None;
    for _ in 0..OBSERVATIONS {
      seen = crate::daemon::observe_first(&daemon, move |state| {
        let cause = state
          .unrecovered
          .get(&slates_db::catalog::VolumeId { bytes: volume.0 })
          .cloned()?;
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        Some((
          cause,
          state.fleet.object_owner(volume),
          count(TAKEOVER_GIVEN_UP),
          count(TAKEOVER_GIVEN_UP_DROPPED),
        ))
      });
      if seen.is_some() {
        break;
      }
    }
    daemon.stop();
    let (cause, owner, given_up, dropped) = seen.expect("the volume was recorded unrecovered");
    assert_eq!(
      cause,
      slates_ipc::protocol::RecoveryCause::TakeoverIncomplete {
        waited_on: RETIREMENT_WAIT.to_owned()
      },
      "every verb on it answers NotRecovered naming the retirement"
    );
    assert_eq!(owner, Some(local), "claimed by the round's node");
    assert_eq!((given_up, dropped), (2, 2), "both registers counted");
  }
}
