//! Locating a remotely homed object (§4.8 Lookup, D-14). The creator is the initial route;
//! after takeover, only a peer that holds the object's routing record can name its owner.
//! A read-only exchange asks the admitted home-region peers, bounded by the existing session
//! table and the record plane's measured round budget over the slowest path it asks (A-437). It never executes a
//! client verb or grants authority. One cached route per client avoids repeating discovery for ordinary use, without a
//! global catalog. A query is bound to its asker's root view; one bound to a newer view than a peer holds is refused and
//! sends that peer's root learner to fetch from the asker (A-489), and a refusal names the root its peer holds, so an
//! asker behind it fetches from that peer and asks again under the root it then holds (A-504). Evidence: the five-node
//! history in tests/fleet.rs and the dated remote-lookup bug reports.

use slates_cluster::membership::Liveness;
use slates_cluster::{CommitBudget, broadcast};
use slates_db::register::{HostId, ObjectId, RegionId};
use slates_transport::connection::Priority;
use slates_wire::Wire;

use crate::fleet::{return_sessions, take_sessions};
use crate::state::{self, ShardState};

/// Format: read-only owner location follows the enrollment stream (13).
pub(crate) const STREAM: u64 = 14;

/// A location request and reply bind the same object and committed root view. Fixed-size fields
/// keep decoding bounded even when an authenticated peer supplies hostile bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub(crate) struct Query {
  object: [u8; 16],
  region: u64,
  root_version: u64,
}

#[derive(Debug, Wire)]
struct Reply {
  query: Query,
  generation: u64,
  serves: bool,
  /// For a work no member owns, the green its catalog record names, from a peer that holds that record (A-255): the
  /// route a resend of its submit takes, a hint that grants nothing. `None` from every other peer and for every other
  /// object. Fixed-size, so decoding stays bounded.
  work_of: Option<[u8; 16]>,
}

/// A peer's refusal of a location query (A-504): the root configuration version the peer holds, so an asker under an
/// older root view learns that it is behind and from whom to fetch the newer root ([`learn_from_refusal`]). Sent where a
/// refusal was an empty reply that named nothing. Its encoding is never a [`Reply`]'s (a fixed 8 bytes against a reply's
/// 42 or more, each decoded exactly), and an asker of an earlier build reads it as malformed, which it folds as no
/// claim, as it did the empty reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
struct Refused {
  root_version: u64,
}

/// A routing hint owned by one live client slot. A hint permits a single forward, never an
/// operation by itself; the receiving owner still checks the principal and its authority.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CachedRoute {
  query: Query,
  owner: HostId,
}

/// Closed failures of the read-only location exchange. The client receives HomedElsewhere and
/// the node counts the precise failure, so an unavailable view is never reported as NotFound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocationError {
  /// No peer claimed the object at the newest generation answered.
  Unavailable,
  /// A query or reply that does not decode.
  Malformed,
  /// A reply bound to another query: another object, region or root view.
  ForeignView,
  /// Two peers claimed the object at the same generation.
  ConflictingOwners,
  /// Asked while this node's consensus groups are not ready to answer (joining, or not initialized).
  NotReady,
  /// Asked under a root view other than the one this node holds.
  RootViewDiffers,
  /// Asked about a region this node is not in, or an object homed elsewhere.
  OutsideHome,
}

impl LocationError {
  pub(crate) fn counter(self) -> &'static str {
    match self {
      Self::Unavailable => "fleet.owner_location.unavailable",
      Self::Malformed => "fleet.owner_location.malformed",
      Self::ForeignView => "fleet.owner_location.foreign_view",
      Self::ConflictingOwners => "fleet.owner_location.conflicting_owners",
      Self::NotReady => "fleet.owner_location.not_ready",
      Self::RootViewDiffers => "fleet.owner_location.root_view_differs",
      Self::OutsideHome => "fleet.owner_location.outside_home",
    }
  }
}

impl Query {
  pub(crate) fn new(state: &ShardState, object: ObjectId, region: u64) -> Self {
    Self {
      object: object.0,
      region,
      root_version: state.root.configuration().version,
    }
  }

  pub(crate) fn route(self, owner: HostId) -> CachedRoute {
    CachedRoute { query: self, owner }
  }

  /// This query bound to the root view this node holds now (A-504): each look of a lookup asks under it
  /// (`verbs::locate_within_horizon`), so a newer root this node fetched since the lookup began, after a peer's refusal
  /// named it ([`learn_from_refusal`]), is the view its peers answer under. Before, every look asked under the root the
  /// lookup began with, and an asker that had caught up was still refused for the old view.
  pub(crate) fn under_held_root(self, state: &ShardState) -> Self {
    Self {
      root_version: state.root.configuration().version,
      ..self
    }
  }

  /// Reuse this client's most recent route, else the owner this node's own routing names (A-338), else the id's live
  /// first owner when it is still in the object's home: the owner a moved object's exception names (A-291; §4.8
  /// "Lookup": "an ownership or home move is a configuration exception naming the affected volume"), else its creator. A
  /// dead one never turns into a rendezvous guess over unrelated members. Never this node: a verb is forwarded only when
  /// this node does not serve the object, and the first owner is this node itself only for an object it lost to a
  /// successor (A-334, A-338), which only the location round can find. Asked on the control shard, where the council's
  /// configuration is. A holder of a moved object's copy follows the exception in its routing
  /// (`crate::moves::follow_moved_object`), so the two agree wherever both are known.
  pub(crate) fn known_owner(
    self,
    state: &ShardState,
    cached: Option<CachedRoute>,
  ) -> Option<HostId> {
    // This node's own routing names the owner of an object it holds a copy of, from the records it accepted and the
    // retirements it installed, so it is asked before the creator, which a takeover may have succeeded (A-338): routed
    // to a creator its routing knew had lost the volume, every forward was redirected there again, for good. And never
    // this node (A-334): a verb is forwarded only when this node does not serve the object, so a creator or a route that
    // names this node leaves the owner to the location round.
    let local = state.fleet.host();
    let object = ObjectId(self.object);
    cached
      .filter(|route| self.reuses(state, *route).is_none())
      .map(|route| route.owner)
      .or_else(|| {
        state
          .fleet
          .object_owner(object)
          .filter(|owner| *owner != local && eligible(state, *owner, RegionId(self.region)))
      })
      .or_else(|| {
        let first = state
          .council
          .configuration()
          .moved
          .get(&object.placement_key())
          .map_or_else(|| object.creator(), |moved| moved.owner);
        eligible(state, first, RegionId(self.region)).then_some(first)
      })
      .filter(|owner| *owner != local)
  }
}

impl Query {
  /// Why `route` cannot be reused for this query, as the counter the miss is recorded under, or `None` when it
  /// can: it was found under another query (another object, region or root view), or its owner is no longer an
  /// eligible member of the region (another region, or held dead).
  pub(crate) fn reuses(self, state: &ShardState, route: CachedRoute) -> Option<&'static str> {
    if route.query != self {
      Some("fleet.owner_location.route_dropped.query_changed")
    } else {
      ineligibility(state, route.owner, RegionId(self.region))
    }
  }
}

/// Why `host` is not an eligible owner in `region`, as the counter a dropped route is recorded under, or `None` when
/// it is: its region is another (or unknown), membership does not know it, or membership holds it dead. A suspected
/// member stays eligible: SWIM's suspicion is refutable, and only a death removes it (Das, Gupta & Motivala, DSN 2002,
/// §4.2). The three are counted apart because each has a different source: the root configuration, the member list,
/// and a death this node's detector judged or a peer gossiped.
fn ineligibility(state: &ShardState, host: HostId, region: RegionId) -> Option<&'static str> {
  if state.node_regions.get(&host) != Some(&region) {
    return Some("fleet.owner_location.route_dropped.owner_ineligible.region");
  }
  match state.fleet.membership().state(host) {
    None => Some("fleet.owner_location.route_dropped.owner_ineligible.unknown"),
    Some(member) if member.liveness == Liveness::Dead => {
      Some("fleet.owner_location.route_dropped.owner_ineligible.dead")
    }
    Some(_) => None,
  }
}

fn eligible(state: &ShardState, host: HostId, region: RegionId) -> bool {
  ineligibility(state, host, region).is_none()
}

/// Answer from the held-object route, under the council's configuration. A node claims an object only
/// while that configuration holds it as a member and its route names itself; ownership moves only when the
/// owner is retired, so a placement still installing the council's newest configuration cannot make a claim
/// false, and the node answers meanwhile. (Refusing then — the placement a version behind its council for most
/// of a long coordinator period right after a takeover — refused the one owner a lookup could find.) A pending
/// takeover does not advertise itself until adoption finishes. Peers without this object report no owner;
/// they never infer one from live membership. The session has already authenticated enrollment. Returns the reply
/// and, when this node does not claim the object, why ([`declined`]), for the caller to count.
///
/// A query bound to a root view other than this node's is refused, since a reply binds the view it was asked under.
/// One bound to a newer view than this node holds also says that `peer`, the asker, holds that view and this node is
/// behind ([`note_newer_root`]): its root learner fetches from `peer` at its next period, and the asker's next look is
/// answered (A-489). A root learner fetched only when its regions or the voters it knew looked wrong
/// (`fleet::root_diverges`), and a seat change moves the root's version with neither (A-406), so a learner left behind at
/// formation refused every round a foreign voter asked, until the lookup's budget ran out (`OwnerUnreachable`).
pub(crate) fn serve(
  state: &mut ShardState,
  peer: HostId,
  bytes: &[u8],
) -> Result<(Vec<u8>, Option<&'static str>), LocationError> {
  let query = Query::from_bytes(bytes).map_err(|_| LocationError::Malformed)?;
  if !state.consensus_ready {
    return Err(LocationError::NotReady);
  }
  let held_root = state.root.configuration().version;
  if held_root != query.root_version {
    if query.root_version > held_root {
      note_newer_root(state, query.root_version, peer);
    }
    return Err(LocationError::RootViewDiffers);
  }
  let local = state.fleet.host();
  let root = state.root.configuration();
  let creator_region = state
    .node_regions
    .get(&ObjectId(query.object).creator())
    .copied();
  if state.node_regions.get(&local) != Some(&RegionId(query.region))
    || creator_region
      .is_none_or(|region| root.home_of(ObjectId(query.object), region) != RegionId(query.region))
  {
    return Err(LocationError::OutsideHome);
  }
  let regional = state.council.configuration();
  let declined = declined(
    state,
    ObjectId(query.object),
    regional.members.contains(&local),
  );
  let work_of = declined.and_then(|_| held_work_of(state, ObjectId(query.object)));
  Ok((
    Reply {
      query,
      generation: regional.version,
      serves: declined.is_none(),
      work_of,
    }
    .to_bytes(),
    declined,
  ))
}

/// What this node answers a peer's location query on [`STREAM`] (§4.8 Lookup): the reply ([`serve`]), with why it did
/// not claim the object counted; or, refused, a refusal naming the root version this node holds ([`Refused`], A-504),
/// with the refusal's reason counted. Before A-504 a refusal was an empty reply, from which an asker behind this node's
/// root learned nothing.
pub(crate) fn answer(state: &mut ShardState, peer: HostId, request: &[u8]) -> Vec<u8> {
  match serve(state, peer, request) {
    Ok((reply, declined)) => {
      if let Some(declined) = declined {
        state.count(declined, 1);
      }
      reply
    }
    Err(error) => {
      state.count(error.counter(), 1);
      Refused {
        root_version: state.root.configuration().version,
      }
      .to_bytes()
    }
  }
}

/// Counter: a location query bound to a newer root view than this node holds, which says this node is behind (A-489).
/// Format: a counter name in the daemon's status report.
const ROOT_BEHIND: &str = "fleet.owner_location.root_behind";

/// Counter: a peer's refusal of this node's location query named a newer root than this node holds, which says this
/// asker is behind (A-504).
/// Format: a counter name in the daemon's status report.
const ASKER_BEHIND: &str = "fleet.owner_location.asker_behind";

/// Counter: a peer refused this node's location query, by an empty reply (a peer of an earlier build) or one naming its
/// root ([`Refused`]); the peer counted the reason.
/// Format: a counter name in the daemon's status report.
const REFUSED_BY_PEER: &str = "fleet.owner_location.refused_by_peer";

/// Records that `peer` holds root configuration `version`, newer than the one this node holds ([`serve`], A-489): the
/// newest version so shown and the peer that showed it, which the root learner's next period fetches from
/// (`fleet::root_learner_fetch`), counted.
fn note_newer_root(state: &mut ShardState, version: u64, peer: HostId) {
  state.count(ROOT_BEHIND, 1);
  record_newer_root(state, version, peer);
}

/// Reads a peer's refusal of this node's location query (A-504): when the root it names is newer than the one this node
/// holds, this asker is behind, so its root learner's next period fetches from `peer`, which holds that root
/// (`fleet::root_learner_fetch`), counted. Whether it was behind. Before, a refusal named nothing, and a behind asker's
/// rounds were refused for its lookup's whole budget (A-489's converse).
fn learn_from_refusal(state: &mut ShardState, peer: HostId, refused: Refused) -> bool {
  if refused.root_version <= state.root.configuration().version {
    return false;
  }
  state.count(ASKER_BEHIND, 1);
  record_newer_root(state, refused.root_version, peer);
  true
}

/// Keeps `version`, held by `peer`, as the newest root this node has been shown past its own, with the peer that showed
/// it (`ShardState::root_newer_seen`).
fn record_newer_root(state: &mut ShardState, version: u64, peer: HostId) {
  if state.root_newer_seen.is_none_or(|(seen, _)| version > seen) {
    state.root_newer_seen = Some((version, peer));
  }
}

/// The green `object`'s catalog record names, if `object` is a work whose catalog record this node holds (A-255).
pub(crate) fn held_work_of(state: &ShardState, object: ObjectId) -> Option<[u8; 16]> {
  let catalog = object.catalog();
  let (_, bytes) = state.holder_records.get(&catalog)?.highest(catalog)?;
  crate::catalog::CatalogValue::from_record_bytes(bytes)?
    .work_of
    .map(|green| green.bytes)
}

/// Why this node does not claim `object` (`serve`), as the counter it is recorded under on this node, or `None` when
/// it claims it: not a member of the council's configuration, the object's route naming another owner or none, or the
/// object retired here as a departed owner's. A lookup that missed the owner then names, on the node that should have
/// claimed, the rule that refused.
pub(crate) fn declined(state: &ShardState, object: ObjectId, member: bool) -> Option<&'static str> {
  let local = state.fleet.host();
  if !member {
    Some("fleet.owner_location.declined.not_member")
  } else if state.fleet.object_owner(object) != Some(local) {
    Some("fleet.owner_location.declined.not_owner")
  } else if state.departed_owners.contains_key(&object) {
    Some("fleet.owner_location.declined.departed")
  } else {
    None
  }
}

/// Only claims supply a route: the claim at the newest regional generation among them, since an owner
/// retired and succeeded claims under an older configuration than its successor adopted under. Conflicting
/// claims at that generation refuse regardless of arrival order. A peer's answer that the object is not its own
/// carries no ownership information and never takes a claim away (AC-8.14: after one configuration change a
/// stale lookup refreshes once) — an unrelated settlement a survivor installed a moment earlier used to discard
/// the successor's claim. This is location, not authority: the claimed owner's lease and fence decide whether it
/// serves. The peer identity supplies the owner, never a payload field.
#[derive(Default)]
struct Answers {
  generation: Option<u64>,
  owner: Option<HostId>,
  conflicting: bool,
  /// Claims a newer claim dropped, counted when the round ends (`fleet.owner_location.claim_superseded`).
  superseded_claims: u64,
  /// The green a peer named for a work no member owns (`Reply::work_of`), the first named.
  work_of: Option<[u8; 16]>,
  /// Peers that named another green than the first: a hint is dropped when its holders disagree.
  work_of_conflicting: bool,
}

/// What folding one reply did to a round's answer, counted by the round, so a refused lookup says which
/// answers it had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Folded {
  /// A peer claimed the object, at the newest generation claimed so far.
  Claim,
  /// A peer answered that the object is not its own.
  NotOwner,
  /// A claim at an older generation than one already folded: it changes nothing.
  OlderClaim,
}

impl Folded {
  fn counter(self) -> &'static str {
    match self {
      Self::Claim => "fleet.owner_location.claim",
      Self::NotOwner => "fleet.owner_location.not_owner",
      Self::OlderClaim => "fleet.owner_location.older_claim",
    }
  }
}

impl Answers {
  fn fold(&mut self, query: Query, peer: HostId, bytes: &[u8]) -> Result<Folded, LocationError> {
    let reply = Reply::from_bytes(bytes).map_err(|_| LocationError::Malformed)?;
    if reply.query != query {
      return Err(LocationError::ForeignView);
    }
    if let Some(green) = reply.work_of {
      self.work_of_conflicting |= self.work_of.is_some_and(|named| named != green);
      self.work_of.get_or_insert(green);
    }
    if !reply.serves {
      return Ok(Folded::NotOwner);
    }
    if self
      .generation
      .is_some_and(|generation| generation > reply.generation)
    {
      return Ok(Folded::OlderClaim);
    }
    if self.generation != Some(reply.generation) {
      if self.owner.is_some() {
        self.superseded_claims = self.superseded_claims.saturating_add(1);
      }
      self.generation = Some(reply.generation);
      self.owner = None;
      self.conflicting = false;
    }
    self.conflicting |= self.owner.is_some_and(|owner| owner != peer);
    self.owner = Some(peer);
    Ok(Folded::Claim)
  }

  /// The round's owner and the work's green a peer named, each counted: superseded claims, conflicting hints (dropped)
  /// and a hint named, and the refusal when no owner was found.
  fn conclude(self) -> (Result<HostId, LocationError>, Option<[u8; 16]>) {
    for _ in 0..self.superseded_claims {
      count("fleet.owner_location.claim_superseded");
    }
    let work_of = if self.work_of_conflicting {
      count("fleet.owner_location.work_of_conflicting");
      None
    } else {
      self.work_of
    };
    if work_of.is_some() {
      count("fleet.owner_location.work_of_named");
    }
    let result = self.finish();
    if let Err(error) = result {
      count(error.counter());
    }
    (result, work_of)
  }

  fn finish(self) -> Result<HostId, LocationError> {
    if self.conflicting {
      Err(LocationError::ConflictingOwners)
    } else {
      self.owner.ok_or(LocationError::Unavailable)
    }
  }
}

fn count(counter: &'static str) {
  state::with_state(|state| state.count(counter, 1));
}

/// Derived: the budget one location round asks its peers under (A-437, R3's class B): the record plane's round budget
/// (`fleet::consensus_budget`) over the slowest measured round-trip tail among the peers it asks (`PathRtt`, RFC
/// 6298's estimator per path). Its base deadline is the longer of a coordinator period and that tail, so a live peer
/// whose path is slow is heard before the round can be judged stalled; it extends while replies keep arriving and ends
/// once none has arrived for the stall window, as every record-plane exchange does; and its whole span
/// (`max_deadline_ns`) is the longest one record-plane exchange runs, so a session lent out when the round begins comes
/// back within it ([`wait_for_returning_sessions`]). Before A-437 a round asked within one liveness window, hard, and
/// waited for lent sessions the same window, whatever the paths measured.
fn round_budget_for(slowest_tail_ns: Option<u64>) -> slates_cluster::CommitBudget {
  crate::fleet::consensus_budget(slowest_tail_ns)
}

/// Whether a round may look again for the sessions still `out`, having waited for one of them to come back within
/// what is left of the round's span (`span_ns`, its budget's longest, [`round_budget_for`]): no once the span has passed,
/// or when no wait can be made (off a shard — never a spin, AUD-29-39); either way counted as sessions that never
/// returned.
async fn wait_for_returning_sessions(began: u64, span_ns: u64, out: &[HostId]) -> bool {
  let left = span_ns.saturating_sub(slates_rt::futures::now_ns().saturating_sub(began));
  // Woken by one of the sessions coming back, or its link changing (`crate::fleet::session_returned`); before
  // 2026-11-20 it slept a tenth of a period between looks.
  if left == 0 || crate::fleet::session_returned(out, left).await.is_none() {
    count("fleet.owner_location.session_never_returned");
    return false;
  }
  true
}

/// One bounded read-only round, with no retries. Every dispatched child has the liveness-budget
/// deadline; the round owns their straggler receiver until every session is returned. Concurrent
/// requests can borrow only disjoint sessions, so aggregate fanout stays at the admitted peer bound.
pub(crate) async fn locate(query: Query) -> Result<HostId, LocationError> {
  locate_round(query).await.0
}

/// The green a work's catalog holders name for `query`'s object, from one bounded location round ([`locate`]'s), for
/// a work no member owns that this node holds no catalog record of (A-255). `None` when no peer named one, or peers
/// named different ones (counted). A hint: the green's owner answers only a completion under the request's own key.
pub(crate) async fn locate_work_of(query: Query) -> Option<[u8; 16]> {
  let (_, work_of) = locate_round(query).await;
  work_of
}

/// One round ([`locate`]): the owner it found and the work's green a peer named, if any.
async fn locate_round(query: Query) -> (Result<HostId, LocationError>, Option<[u8; 16]>) {
  let peers = state::with_state(|state| {
    state
      .node_regions
      .keys()
      .copied()
      .filter(|peer| eligible(state, *peer, RegionId(query.region)))
      .collect::<std::collections::BTreeSet<_>>()
  })
  .unwrap_or_default();
  // The round's budget: the record plane's, over the slowest path among the peers it may ask.
  let budget = round_budget_for(
    state::with_state(|state| {
      peers
        .iter()
        .filter_map(|peer| {
          state
            .peer_paths
            .get(peer)
            .and_then(slates_cluster::timing::PathRtt::tail_ns)
        })
        .max()
    })
    .flatten(),
  );
  let request = query.to_bytes();
  count("fleet.owner_location.round");
  // One span for the whole round, its budget's longest. A peer whose record session is out when the round starts (a
  // coordinator dispatch or a discovery page has it, `fleet::take_sessions`) is asked once the session
  // comes back, polled at the fleet's interval; a round that skipped it could miss the owner and refuse
  // a lookup the owner would serve (docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md,
  // found 1). A peer with no session at all is not waited for: the bounded neighbourhood keeps no session
  // to most peers by design.
  let began = slates_rt::futures::now_ns();
  let mut unasked = peers;
  let mut answers = Answers::default();
  let mut met_out = false;
  loop {
    let taken = take_sessions("fleet.lent_to.locate", |peer| unasked.contains(&peer));
    for (peer, _) in &taken {
      unasked.remove(peer);
    }
    if !taken.is_empty() {
      ask(&mut answers, query, &request, taken, budget).await;
    }
    let out = sessions_out(&unasked);
    if out.is_empty() {
      count_unsessioned(&unasked);
      break;
    }
    if !met_out {
      met_out = true;
      count("fleet.owner_location.session_out");
    }
    let waiting_for: Vec<HostId> = out.iter().copied().collect();
    unasked = out;
    if !wait_for_returning_sessions(began, budget.max_deadline_ns(), &waiting_for).await {
      break;
    }
  }
  answers.conclude()
}

/// Counts the peers a round left unasked because they hold no record session at all: not waited for (the bounded
/// neighbourhood keeps none to most peers), but counted (`fleet.owner_location.no_session`), so a round that missed
/// a claim because its claimant had no session says so rather than reading as a peer that answered "not mine".
fn count_unsessioned(unasked: &std::collections::BTreeSet<HostId>) {
  for _ in unasked {
    count("fleet.owner_location.no_session");
  }
}

/// Asks the peers whose sessions were `taken`, under the round's `budget` ([`round_budget_for`]), folding every reply
/// that arrives in time and returning each session.
async fn ask(
  answers: &mut Answers,
  query: Query,
  request: &[u8],
  taken: Vec<(HostId, slates_transport::endpoint::Endpoint)>,
  budget: CommitBudget,
) {
  let mut unanswered: std::collections::BTreeSet<HostId> =
    taken.iter().map(|(peer, _)| *peer).collect();
  let requests = taken
    .into_iter()
    .map(|(peer, endpoint)| (peer, request.to_vec(), endpoint))
    .collect();
  let (replies, mut stragglers) = broadcast(requests, STREAM, Priority::Metadata, budget).await;
  fold_replies(answers, query, replies, &mut unanswered);
  loop {
    let (replies, done) = stragglers.recover_replies();
    fold_replies(answers, query, replies, &mut unanswered);
    if done {
      break;
    }
    if slates_rt::futures::sleep(budget.poll_interval_ns)
      .await
      .is_err()
    {
      break;
    }
  }
  for _ in &unanswered {
    count("fleet.owner_location.no_reply");
  }
}

/// The peers of `peers` whose record session exists but is out on loan now; a peer without a session is
/// not among them.
fn sessions_out(peers: &std::collections::BTreeSet<HostId>) -> std::collections::BTreeSet<HostId> {
  state::with_state(|state| {
    peers
      .iter()
      .copied()
      .filter(|peer| {
        state
          .record_sessions
          .get(peer)
          .is_some_and(|link| link.endpoint.is_none())
      })
      .collect()
  })
  .unwrap_or_default()
}

/// Folds the replies that arrived into the round's answer, counting what each did — a refusal is the peer's, which it
/// counted itself by reason, and one naming a newer root than this node holds sends this node's root learner to fetch
/// from that peer ([`learn_from_refusal`], A-504) — and strikes each replying peer from `unanswered`.
fn fold_replies(
  answers: &mut Answers,
  query: Query,
  replies: Vec<(
    HostId,
    slates_cluster::TimedReply,
    slates_transport::endpoint::Endpoint,
  )>,
  unanswered: &mut std::collections::BTreeSet<HostId>,
) {
  let mut sessions = Vec::with_capacity(replies.len());
  for (peer, reply, endpoint) in replies {
    unanswered.remove(&peer);
    let counter = if reply.bytes.is_empty() {
      REFUSED_BY_PEER
    } else if let Ok(refused) = Refused::from_bytes(&reply.bytes) {
      let _ = state::with_state_counted(|state| learn_from_refusal(state, peer, refused));
      REFUSED_BY_PEER
    } else {
      match answers.fold(query, peer, &reply.bytes) {
        Ok(folded) => folded.counter(),
        Err(error) => error.counter(),
      }
    };
    count(counter);
    sessions.push((peer, endpoint));
  }
  return_sessions(sessions);
}

#[cfg(test)]
mod tests {
  use super::*;

  fn query() -> Query {
    Query {
      object: ObjectId::new(HostId(1), 7).0,
      region: 2,
      root_version: 3,
    }
  }

  /// A reply to `query()` under `generation`, claiming the object when `serves`.
  fn reply(generation: u64, serves: bool) -> Vec<u8> {
    Reply {
      query: query(),
      generation,
      serves,
      work_of: None,
    }
    .to_bytes()
  }

  /// AC-8.14: an owner that claims under a newer generation than an old one — the successor that adopted what
  /// a retired owner still claims — is the route, regardless of reply order, and a peer that is not the owner
  /// changes nothing. Unrelated members never become owners by their ranking.
  #[test]
  fn the_newest_claim_supplies_the_route() {
    let query = query();
    let (old, absent, current) = (reply(1, true), reply(2, false), reply(2, true));
    for order in [[0, 1, 2], [2, 1, 0], [1, 0, 2]] {
      let replies = [
        (HostId(1), &old),
        (HostId(2), &absent),
        (HostId(3), &current),
      ];
      let mut answers = Answers::default();
      for index in order {
        let (peer, bytes) = replies[index];
        answers.fold(query, peer, bytes).unwrap();
      }
      assert_eq!(answers.finish(), Ok(HostId(3)));
    }
    let mut none = Answers::default();
    assert_eq!(none.fold(query, HostId(2), &absent), Ok(Folded::NotOwner));
    assert_eq!(
      none.finish(),
      Err(LocationError::Unavailable),
      "with no claim there is no route"
    );
  }

  /// What each reply did is what the round counts: a peer that is not the owner drops no claim, a newer claim
  /// drops an older one (counted `claim_superseded`), and a claim older than the one held changes nothing.
  #[test]
  fn each_reply_says_what_it_did_to_the_round() {
    let query = query();
    let mut answers = Answers::default();
    assert_eq!(
      answers.fold(query, HostId(1), &reply(1, true)),
      Ok(Folded::Claim)
    );
    assert_eq!(
      answers.fold(query, HostId(2), &reply(2, false)),
      Ok(Folded::NotOwner)
    );
    assert_eq!(
      answers.superseded_claims, 0,
      "not the owner: no claim dropped"
    );
    assert_eq!(
      answers.fold(query, HostId(3), &reply(2, true)),
      Ok(Folded::Claim)
    );
    assert_eq!(answers.superseded_claims, 1, "the older claim was dropped");
    assert_eq!(
      answers.fold(query, HostId(1), &reply(1, true)),
      Ok(Folded::OlderClaim)
    );
    assert_eq!(answers.finish(), Ok(HostId(3)));
  }

  /// AC-8.14 ("after one stable configuration change a stale lookup refreshes once"): a peer's answer that it
  /// is not the owner carries no ownership information — peers "never infer [an owner] from live membership" —
  /// so a newer configuration it answers under cannot take away the owner's claim. Do: the successor claims
  /// under configuration 8, a survivor that installed an unrelated settlement since answers "not mine" under 9.
  /// Expect: the successor is the route. The Linux io_uring loop failed the location test on this: after a
  /// takeover each survivor's `Settle` and the successor's `Confirm` advance the version within a few periods.
  #[test]
  fn a_peer_that_is_not_the_owner_never_takes_away_a_claim() {
    let query = query();
    let claim = Reply {
      query,
      generation: 8,
      serves: true,
      work_of: None,
    }
    .to_bytes();
    let not_mine = Reply {
      query,
      generation: 9,
      serves: false,
      work_of: None,
    }
    .to_bytes();
    for order in [[0, 1], [1, 0]] {
      let replies = [(HostId(3), &claim), (HostId(4), &not_mine)];
      let mut answers = Answers::default();
      for index in order {
        let (peer, bytes) = replies[index];
        answers.fold(query, peer, bytes).unwrap();
      }
      assert_eq!(answers.finish(), Ok(HostId(3)), "order {order:?}");
    }
  }

  /// A-255: a work no member owns has its route in its catalog record, which its holders name in their "not mine"
  /// replies. Do: fold no claim, one peer with no record, and two holders naming the same green, in every order; then
  /// a round where two holders name different greens. Expect: the first round finds no owner and names the green; the
  /// second names none (a hint its holders disagree on is dropped, counted), and neither round ever selects an owner.
  #[test]
  fn a_works_holders_name_its_green_and_a_disagreement_names_none() {
    let query = query();
    let named = |green: u8| {
      Reply {
        query,
        generation: 4,
        serves: false,
        work_of: Some([green; 16]),
      }
      .to_bytes()
    };
    let unknowing = Reply {
      query,
      generation: 4,
      serves: false,
      work_of: None,
    }
    .to_bytes();
    let (first, second) = (named(7), named(7));
    for order in [[0, 1, 2], [2, 1, 0], [1, 0, 2]] {
      let replies = [
        (HostId(3), &first),
        (HostId(4), &unknowing),
        (HostId(5), &second),
      ];
      let mut answers = Answers::default();
      for index in order {
        let (peer, bytes) = replies[index];
        answers.fold(query, peer, bytes).unwrap();
      }
      assert_eq!(
        answers.conclude(),
        (Err(LocationError::Unavailable), Some([7; 16])),
        "order {order:?}"
      );
    }
    let other = named(8);
    let mut answers = Answers::default();
    answers.fold(query, HostId(3), &first).unwrap();
    answers.fold(query, HostId(5), &other).unwrap();
    assert_eq!(
      answers.conclude(),
      (Err(LocationError::Unavailable), None),
      "holders that disagree name no green"
    );
  }

  /// AC-8.14 / §4.9: conflicting, truncated or wrongly bound replies cannot select an owner.
  #[test]
  fn conflicts_and_foreign_or_malformed_replies_refuse() {
    let query = query();
    let reply = Reply {
      query,
      generation: 2,
      serves: true,
      work_of: None,
    }
    .to_bytes();
    let mut answers = Answers::default();
    for length in 0..reply.len() {
      assert_eq!(
        answers.fold(query, HostId(1), &reply[..length]),
        Err(LocationError::Malformed)
      );
    }
    let mut trailing = reply.clone();
    trailing.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      answers.fold(query, HostId(1), &trailing),
      Err(LocationError::Malformed)
    );
    let mut invalid_boolean = reply.clone();
    *invalid_boolean.last_mut().unwrap() = u8::MAX;
    assert_eq!(
      answers.fold(query, HostId(1), &invalid_boolean),
      Err(LocationError::Malformed)
    );
    for foreign in [
      Query {
        object: ObjectId::new(HostId(9), 7).0,
        ..query
      },
      Query { region: 8, ..query },
      Query {
        root_version: 9,
        ..query
      },
    ] {
      assert_eq!(
        answers.fold(foreign, HostId(1), &reply),
        Err(LocationError::ForeignView)
      );
    }
    answers.fold(query, HostId(1), &reply).unwrap();
    answers.fold(query, HostId(2), &reply).unwrap();
    assert_eq!(answers.finish(), Err(LocationError::ConflictingOwners));
  }

  /// R3's class B (B-a, A-437). Do: derive the budget a location round asks its peers under, with no path measured and
  /// with the slowest asked path's round-trip tail at two liveness windows. Expect: with none measured, a round that
  /// extends while replies keep arriving past its base deadline; with the slow path, a base deadline past that tail, so a
  /// live peer two windows away is heard before the round can stall; and a span past the tail, which a lent session's
  /// return is waited for within. Before, every round asked within one liveness window, hard, whatever the paths
  /// measured, and gave its lent sessions the same window.
  #[test]
  fn a_location_round_waits_for_its_slowest_asked_peers_measured_round_trip() {
    let quiet = round_budget_for(None);
    assert!(
      quiet.max_deadline_ns() > quiet.deadline_ns,
      "a round extends while replies keep arriving"
    );
    let slow_tail = 2 * crate::daemon::LIVENESS_BUDGET_NS;
    let slow = round_budget_for(Some(slow_tail));
    assert!(
      slow.deadline_ns >= slow_tail,
      "the slowest asked path's tail is waited for before the round can stall: {} ns",
      slow.deadline_ns
    );
    assert!(
      slow.max_deadline_ns() > slow_tail,
      "a lent session is waited for past the slowest path's tail"
    );
  }

  /// The peer a root learner's period fetches from beside the voters it knows, if any (`fleet::root_learner_fetch`).
  fn shown_by(fetch: crate::fleet::RootLearnerFetch) -> Option<HostId> {
    match fetch {
      crate::fleet::RootLearnerFetch::Due { shown_by } => shown_by,
      crate::fleet::RootLearnerFetch::Idle => None,
    }
  }

  /// A-489 (§4.8 Lookup; D-14 root learners): a location query bound to a newer root view than this node holds is
  /// refused, since a reply binds the view it was asked under, and it tells this node that it is behind and that the
  /// asker holds the newer root: the root learner's next period fetches from the asker. Main's runs of the fleet test
  /// `a_location_round_asks_a_peer_whose_session_was_out_once_it_returns` (the quiet Mac Studio, main 3022c6894 and
  /// 2a5bf7264) failed 2 of 3 with `OwnerUnreachable`: the successor, a root learner, counted `root_view_differs` once
  /// for each of the foreign voter's 58 and 68 rounds until the lookup's budget ran out, since nothing it compared told
  /// it that a seat change (A-406) had moved the root on. Do: on a shard whose root has moved past one version (a region
  /// admitted), run the learner's period, then serve location queries bound to the older version (from Q), to its own
  /// (from Q), to the next (from P), to the one after (from R) and to the next again (from P), and run the period twice.
  /// Expect: the older and both newer views refused `RootViewDiffers` and its own view served; three newer views
  /// counted; no fetch from a shown peer before the queries, a fetch from R (which showed the newest) at the first period
  /// after them, and none from a shown peer at the second.
  #[test]
  fn a_location_query_bound_to_a_newer_root_sends_the_learner_to_fetch_from_its_asker() {
    use slates_cluster::root_group::RootCommand;
    let (refusals, periods, behind, (p, q, r)) = crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let object = ObjectId::new(local, 1);
      state.node_regions.insert(local, RegionId(0));
      let older = state.root.configuration().version;
      assert!(state.root.propose(RootCommand::AdmitRegion(RegionId(2))));
      let held = state.root.configuration().version;
      assert!(held > older, "the root moved past one version");
      let (p, q, r) = (
        HostId(local.0 ^ 1),
        HostId(local.0 ^ 2),
        HostId(local.0 ^ 3),
      );
      let bound = |root_version| {
        Query {
          object: object.0,
          region: 0,
          root_version,
        }
        .to_bytes()
      };
      let now = slates_rt::futures::now_ns();
      let before = crate::fleet::root_learner_fetch(state, now);
      let refusals = [
        serve(state, q, &bound(older)).err(),
        serve(state, q, &bound(held)).err(),
        serve(state, p, &bound(held.saturating_add(1))).err(),
        serve(state, r, &bound(held.saturating_add(2))).err(),
        serve(state, p, &bound(held.saturating_add(1))).err(),
      ];
      let first = crate::fleet::root_learner_fetch(state, now);
      let second = crate::fleet::root_learner_fetch(state, now);
      let behind = state.refusals.get(ROOT_BEHIND).copied();
      (refusals, [before, first, second], behind, (p, q, r))
    });
    let view = Some(LocationError::RootViewDiffers);
    assert_eq!(
      refusals,
      [view, None, view, view, view],
      "the older and the newer views refused, its own served (from {q:?}, {p:?} and {r:?})"
    );
    assert_eq!(
      periods.map(shown_by),
      [None, Some(r), None],
      "fetched from {r:?}, which showed the newest root, at the first period after the queries alone: {periods:?}"
    );
    assert_eq!(behind, Some(3), "each newer view counted");
  }

  /// A-504 (§4.8 Lookup; D-14 root learners): a behind asker's round is answered after one fetch. A peer's refusal names
  /// the root the peer holds, so an asker under an older root learns that it is behind and its root learner's next period
  /// fetches from that peer; once the asker holds that root, its next look, asked under the root it holds, is answered.
  /// Before, a refusal was an empty reply that named nothing, and every look asked under the root the lookup began with,
  /// so a behind asker was refused for its lookup's whole budget (A-489's converse). Do: two nodes, the peer's root a
  /// version past the asker's (a region admitted) and the peer owning the object; the asker asks under its root, reads
  /// the peer's answer and runs its learner's period; then the asker reaches the peer's root (the fetch, as the same
  /// region admitted) and asks again under the root it holds. Expect: the first answer a refusal naming the peer's root,
  /// the asker counted behind and its learner fetching from the peer; the second answer the peer's claim.
  #[test]
  fn a_behind_askers_round_is_answered_after_one_fetch() {
    use slates_cluster::root_group::RootCommand;
    let peer = crate::daemon::audit_daemon();
    let asker = crate::daemon::audit_daemon();
    let (peer_host, peer_root) = crate::daemon::observe_first(&peer, |state| {
      assert!(state.root.propose(RootCommand::AdmitRegion(RegionId(2))));
      (state.fleet.host(), state.root.configuration().version)
    });
    let (asker_host, asked) = crate::daemon::observe_first(&asker, |state| {
      let local = state.fleet.host();
      (local, Query::new(state, ObjectId::new(local, 1), 0))
    });
    let first = crate::daemon::observe_first(&peer, move |state| {
      let local = state.fleet.host();
      state.node_regions.insert(local, RegionId(0));
      state.node_regions.insert(asker_host, RegionId(0));
      state
        .fleet
        .track_object_owner(ObjectId::new(asker_host, 1), local);
      answer(state, asker_host, &asked.to_bytes())
    });
    let (read, behind, fetch, counted) = crate::daemon::observe_first(&asker, move |state| {
      let read = Refused::from_bytes(&first).ok();
      let behind = read.is_some_and(|refused| learn_from_refusal(state, peer_host, refused));
      let fetch = crate::fleet::root_learner_fetch(state, slates_rt::futures::now_ns());
      (
        read,
        behind,
        fetch,
        state.refusals.get(ASKER_BEHIND).copied(),
      )
    });
    let (caught_up, again) = crate::daemon::observe_first(&asker, move |state| {
      assert!(state.root.propose(RootCommand::AdmitRegion(RegionId(2))));
      (
        state.root.configuration().version,
        asked.under_held_root(state),
      )
    });
    let second = crate::daemon::observe_first(&peer, move |state| {
      answer(state, asker_host, &again.to_bytes())
    });
    peer.stop();
    asker.stop();
    assert!(
      asked.root_version < peer_root,
      "the asker began a root behind the peer: {} < {peer_root}",
      asked.root_version
    );
    assert_eq!(
      read,
      Some(Refused {
        root_version: peer_root
      }),
      "the peer's refusal names the root it holds"
    );
    assert!(behind, "the asker read itself behind");
    assert_eq!(counted, Some(1), "counted once");
    assert_eq!(
      fetch,
      crate::fleet::RootLearnerFetch::Due {
        shown_by: Some(peer_host)
      },
      "the asker's root learner fetches from the peer at its next period"
    );
    assert_eq!(caught_up, peer_root, "one fetch reached the peer's root");
    assert_eq!(again.root_version, peer_root, "the next look asks under it");
    let mut answers = Answers::default();
    assert_eq!(
      answers.fold(again, peer_host, &second),
      Ok(Folded::Claim),
      "the peer answers the look under the root they share"
    );
    assert_eq!(answers.finish(), Ok(peer_host), "the round finds the owner");
  }

  /// A-504, the refusal's bytes (§4.9's bounded decoding; hostile input): a refusal is exactly the root version it names,
  /// and a reply is never read as a refusal nor a refusal as a reply. Do: encode a refusal and decode it; cut it short at
  /// every length and give it a byte more; decode a reply as a refusal and the refusal as a reply. Expect: the refusal
  /// back whole; every cut and the longer one refused; neither read as the other.
  #[test]
  fn a_refusal_names_the_peers_root_and_no_reply_reads_as_one() {
    let refused = Refused {
      root_version: u64::MAX,
    };
    let bytes = refused.to_bytes();
    assert_eq!(Refused::from_bytes(&bytes).ok(), Some(refused));
    for length in 0..bytes.len() {
      assert!(
        Refused::from_bytes(&bytes[..length]).is_err(),
        "cut at {length}"
      );
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Refused::from_bytes(&longer).is_err(), "a byte more");
    for serves in [false, true] {
      assert!(
        Refused::from_bytes(&reply(4, serves)).is_err(),
        "a reply is no refusal"
      );
    }
    let mut answers = Answers::default();
    assert_eq!(
      answers.fold(query(), HostId(1), &bytes),
      Err(LocationError::Malformed),
      "a refusal is no reply"
    );
  }
}
