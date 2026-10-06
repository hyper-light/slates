//! Locating a remotely homed object (§4.8 Lookup, D-14). The creator is the initial route;
//! after takeover, only a peer that holds the object's routing record can name its owner.
//! A read-only exchange asks the admitted home-region peers, bounded by the existing session
//! table and liveness budget. It never executes a client verb or grants authority. One cached
//! route per client avoids repeating discovery for ordinary use, without a global catalog.
//! Evidence: the five-node history in tests/fleet.rs and the dated remote-lookup bug report.

use slates_cluster::membership::Liveness;
use slates_cluster::{CommitBudget, broadcast};
use slates_db::register::{HostId, ObjectId, RegionId};
use slates_transport::connection::Priority;
use slates_wire::Wire;

use crate::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS};
use crate::fleet::{POLL_PER_PERIOD, return_sessions, take_sessions};
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

  /// Reuse this client's most recent route, or the id's live creator when it is still in the
  /// object's home. A dead creator never turns into a rendezvous guess over unrelated members.
  pub(crate) fn known_owner(
    self,
    state: &ShardState,
    cached: Option<CachedRoute>,
  ) -> Option<HostId> {
    cached
      .filter(|route| route.query == self && eligible(state, route.owner, RegionId(self.region)))
      .map(|route| route.owner)
      .or_else(|| {
        eligible(
          state,
          ObjectId(self.object).creator(),
          RegionId(self.region),
        )
        .then_some(ObjectId(self.object).creator())
      })
  }
}

fn eligible(state: &ShardState, host: HostId, region: RegionId) -> bool {
  state.node_regions.get(&host) == Some(&region)
    && state
      .fleet
      .membership()
      .state(host)
      .is_some_and(|member| member.liveness != Liveness::Dead)
}

/// Answer from the held-object route, under the council's configuration. A node claims an object only
/// while that configuration holds it as a member and its route names itself; ownership moves only when the
/// owner is retired, so a placement still installing the council's newest configuration cannot make a claim
/// false, and the node answers meanwhile. (Refusing then — the placement a version behind its council for most
/// of a long coordinator period right after a takeover — refused the one owner a lookup could find.) A pending
/// takeover does not advertise itself until adoption finishes. Peers without this object report no owner;
/// they never infer one from live membership. The session has already authenticated enrollment.
pub(crate) fn serve(state: &ShardState, bytes: &[u8]) -> Result<Vec<u8>, LocationError> {
  let query = Query::from_bytes(bytes).map_err(|_| LocationError::Malformed)?;
  let local = state.fleet.host();
  let root = state.root.configuration();
  let creator_region = state
    .node_regions
    .get(&ObjectId(query.object).creator())
    .copied();
  if !state.consensus_ready {
    return Err(LocationError::NotReady);
  }
  if root.version != query.root_version {
    return Err(LocationError::RootViewDiffers);
  }
  if state.node_regions.get(&local) != Some(&RegionId(query.region))
    || creator_region
      .is_none_or(|region| root.home_of(ObjectId(query.object), region) != RegionId(query.region))
  {
    return Err(LocationError::OutsideHome);
  }
  let regional = state.council.configuration();
  Ok(
    Reply {
      query,
      generation: regional.version,
      serves: regional.members.contains(&local)
        && state.fleet.object_owner(ObjectId(query.object)) == Some(local)
        && !state.departed_owners.contains_key(&ObjectId(query.object)),
    }
    .to_bytes(),
  )
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

/// Whether a round may wait one more poll for sessions still out: no once the liveness budget is spent, or
/// when no poll can be timed (a refused sleep, off a shard — never a spin, AUD-29-39); either way counted as
/// sessions that never returned.
async fn wait_for_returning_sessions(began: u64, poll_ns: u64) -> bool {
  let spent = slates_rt::futures::now_ns().saturating_sub(began) >= LIVENESS_BUDGET_NS;
  if spent || slates_rt::futures::sleep(poll_ns).await.is_err() {
    count("fleet.owner_location.session_never_returned");
    return false;
  }
  true
}

/// One bounded read-only round, with no retries. Every dispatched child has the liveness-budget
/// deadline; the round owns their straggler receiver until every session is returned. Concurrent
/// requests can borrow only disjoint sessions, so aggregate fanout stays at the admitted peer bound.
pub(crate) async fn locate(query: Query) -> Result<HostId, LocationError> {
  let peers = state::with_state(|state| {
    state
      .node_regions
      .keys()
      .copied()
      .filter(|peer| eligible(state, *peer, RegionId(query.region)))
      .collect::<std::collections::BTreeSet<_>>()
  })
  .unwrap_or_default();
  let request = query.to_bytes();
  count("fleet.owner_location.round");
  // One deadline for the whole round. A peer whose record session is out when the round starts (a
  // coordinator dispatch or a discovery page has it, `fleet::take_sessions`) is asked once the session
  // comes back, polled at the fleet's interval; a round that skipped it could miss the owner and refuse
  // a lookup the owner would serve (docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md,
  // found 1). A peer with no session at all is not waited for: the bounded neighbourhood keeps no session
  // to most peers by design.
  let began = slates_rt::futures::now_ns();
  let poll_ns = HEARTBEAT_NS / POLL_PER_PERIOD;
  let mut unasked = peers;
  let mut answers = Answers::default();
  let mut met_out = false;
  loop {
    let taken = take_sessions(|peer| unasked.contains(&peer));
    for (peer, _) in &taken {
      unasked.remove(peer);
    }
    if !taken.is_empty() {
      let remaining =
        LIVENESS_BUDGET_NS.saturating_sub(slates_rt::futures::now_ns().saturating_sub(began));
      ask(&mut answers, query, &request, taken, remaining, poll_ns).await;
    }
    let out = sessions_out(&unasked);
    if out.is_empty() {
      break;
    }
    if !met_out {
      met_out = true;
      count("fleet.owner_location.session_out");
    }
    unasked = out;
    if !wait_for_returning_sessions(began, poll_ns).await {
      break;
    }
  }
  for _ in 0..answers.superseded_claims {
    count("fleet.owner_location.claim_superseded");
  }
  let result = answers.finish();
  if let Err(error) = result {
    count(error.counter());
  }
  result
}

/// Asks the peers whose sessions were `taken`, within `budget_ns`, folding every reply that arrives in
/// time and returning each session.
async fn ask(
  answers: &mut Answers,
  query: Query,
  request: &[u8],
  taken: Vec<(HostId, slates_transport::endpoint::Endpoint)>,
  budget_ns: u64,
  poll_ns: u64,
) {
  let mut unanswered: std::collections::BTreeSet<HostId> =
    taken.iter().map(|(peer, _)| *peer).collect();
  let requests = taken
    .into_iter()
    .map(|(peer, endpoint)| (peer, request.to_vec(), endpoint))
    .collect();
  let budget = CommitBudget::hard(budget_ns, poll_ns);
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

/// Folds the replies that arrived into the round's answer, counting what each did — an empty reply is the
/// peer's refusal, which it counted itself by reason — and strikes each replying peer from `unanswered`.
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
      "fleet.owner_location.refused_by_peer"
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
    }
    .to_bytes();
    let not_mine = Reply {
      query,
      generation: 9,
      serves: false,
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

  /// AC-8.14 / §4.9: conflicting, truncated or wrongly bound replies cannot select an owner.
  #[test]
  fn conflicts_and_foreign_or_malformed_replies_refuse() {
    let query = query();
    let reply = Reply {
      query,
      generation: 2,
      serves: true,
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
}
