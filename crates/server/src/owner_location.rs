//! Locating a remotely homed object (§4.8 Lookup, D-14). The creator is the initial route;
//! after takeover, only a peer that holds the object's routing record can name its owner.
//! A read-only exchange asks the admitted home-region peers, bounded by the existing session
//! table and liveness budget. It never executes a client verb or grants authority. One cached
//! route per client avoids repeating discovery for ordinary use, without a global catalog.
//! Evidence: the five-node history in tests/fleet.rs and the dated remote-lookup bug report.

use slates_cluster::membership::Liveness;
use slates_cluster::{CommitBudget, broadcast};
use slates_db::register::{HostId, ObjectId, RegionId};
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
  Unavailable,
  Malformed,
  ForeignView,
  ConflictingOwners,
}

impl LocationError {
  pub(crate) fn counter(self) -> &'static str {
    match self {
      Self::Unavailable => "fleet.owner_location.unavailable",
      Self::Malformed => "fleet.owner_location.malformed",
      Self::ForeignView => "fleet.owner_location.foreign_view",
      Self::ConflictingOwners => "fleet.owner_location.conflicting_owners",
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

/// Answer from the held-object route after configuration application. A pending takeover does
/// not advertise itself until adoption finishes. Peers without this object report no owner;
/// they never infer one from live membership. The session has already authenticated enrollment.
pub(crate) fn serve(state: &ShardState, bytes: &[u8]) -> Result<Vec<u8>, LocationError> {
  let query = Query::from_bytes(bytes).map_err(|_| LocationError::Malformed)?;
  let local = state.fleet.host();
  let root = state.root.configuration();
  let creator_region = state
    .node_regions
    .get(&ObjectId(query.object).creator())
    .copied();
  if !state.consensus_ready
    || root.version != query.root_version
    || state.node_regions.get(&local) != Some(&RegionId(query.region))
    || creator_region
      .is_none_or(|region| root.home_of(ObjectId(query.object), region) != RegionId(query.region))
  {
    return Err(LocationError::ForeignView);
  }
  let regional = state.council.configuration();
  if state.fleet.configuration().version != regional.version {
    return Err(LocationError::ForeignView);
  }
  Ok(
    Reply {
      query,
      generation: regional.version,
      serves: regional.members.contains(&local)
        && state.fleet.object_owner(ObjectId(query.object)) == Some(local)
        && !state.pending_takeovers.contains(&ObjectId(query.object)),
    }
    .to_bytes(),
  )
}

/// Only replies at the newest observed regional generation can supply a route. A conflicting
/// claim at that generation refuses regardless of arrival order; newer negative replies also
/// invalidate an older positive. The peer identity supplies the owner, never a payload field.
#[derive(Default)]
struct Answers {
  generation: Option<u64>,
  owner: Option<HostId>,
  conflicting: bool,
}

impl Answers {
  fn fold(&mut self, query: Query, peer: HostId, bytes: &[u8]) -> Result<(), LocationError> {
    let reply = Reply::from_bytes(bytes).map_err(|_| LocationError::Malformed)?;
    if reply.query != query {
      return Err(LocationError::ForeignView);
    }
    if self
      .generation
      .is_some_and(|generation| generation > reply.generation)
    {
      return Ok(());
    }
    if self.generation != Some(reply.generation) {
      self.generation = Some(reply.generation);
      self.owner = None;
      self.conflicting = false;
    }
    if reply.serves {
      self.conflicting |= self.owner.is_some_and(|owner| owner != peer);
      self.owner = Some(peer);
    }
    Ok(())
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
  state::with_state(|state| *state.refusals.entry(counter).or_insert(0) += 1);
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
  let requests = take_sessions(|peer| peers.contains(&peer))
    .into_iter()
    .map(|(peer, endpoint)| (peer, request.clone(), endpoint))
    .collect();
  count("fleet.owner_location.round");
  let budget = CommitBudget::hard(LIVENESS_BUDGET_NS, HEARTBEAT_NS / POLL_PER_PERIOD);
  let (replies, mut stragglers) = broadcast(requests, STREAM, budget).await;
  let mut answers = Answers::default();
  fold_replies(&mut answers, query, replies);
  loop {
    let (replies, done) = stragglers.recover_replies();
    fold_replies(&mut answers, query, replies);
    if done {
      break;
    }
    slates_rt::futures::sleep(budget.poll_interval_ns).await;
  }
  let result = answers.finish();
  if let Err(error) = result {
    count(error.counter());
  }
  result
}

fn fold_replies(
  answers: &mut Answers,
  query: Query,
  replies: Vec<(
    HostId,
    slates_cluster::TimedReply,
    slates_transport::endpoint::Endpoint,
  )>,
) {
  let mut sessions = Vec::with_capacity(replies.len());
  for (peer, reply, endpoint) in replies {
    if !reply.bytes.is_empty()
      && let Err(error) = answers.fold(query, peer, &reply.bytes)
    {
      count(error.counter());
    }
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

  /// AC-8.14: a new negative view invalidates an old owner; an actual new owner is the route,
  /// regardless of reply order. Unrelated members never become owners by their ranking.
  #[test]
  fn only_the_owner_at_the_newest_observed_generation_supplies_a_route() {
    let query = query();
    let old = Reply {
      query,
      generation: 1,
      serves: true,
    }
    .to_bytes();
    let absent = Reply {
      query,
      generation: 2,
      serves: false,
    }
    .to_bytes();
    let current = Reply {
      query,
      generation: 2,
      serves: true,
    }
    .to_bytes();
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
    let mut answers = Answers::default();
    answers.fold(query, HostId(1), &old).unwrap();
    answers.fold(query, HostId(2), &absent).unwrap();
    assert_eq!(answers.finish(), Err(LocationError::Unavailable));
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
