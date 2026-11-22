//! Each repository's memory share on its owner shard, divided by marginal utility at the learning cadence and kept as
//! an immutable value between divisions (A-724 piece 2d; §4.17.5 "Shares": "published ... as an immutable snapshot at
//! the learning cadence (D-8), never asked for per request").
//!
//! **What it holds.** [`Grants`]: for each repository with a usage row on this shard, the bytes of the shard's room
//! its curve wins (`slates_vcs::partition`: each first guaranteed the lesser of A-188's floor and what its curve can
//! use, the rest by lookahead), and the room no curve would use (the spare, lent). A division reads every row's curve,
//! aged first to now at its repository's read horizon (`Ledger::age_curves`), each repository's misses turned into
//! misses a second by that horizon (a curve's counts are its reads over about one horizon).
//!
//! **When it is divided** ([`current_at`]). At the learning cadence: once the shortest read horizon among the
//! repositories served has passed since the last division, or at once when the set of repositories served changed
//! (one came or went, so every floor moved). With no horizon learned yet, only such a change divides again. So a
//! division costs `O(n · e · log n)` (n repositories, e ≤ 65 edges a curve) once a cadence, never a request's.
//!
//! **What it is not.** It divides; admission reads it (`crate::vcs_share::room`, A-724 piece 3's side). One value a
//! shard, replaced whole, read where it is kept: nothing shares it across threads.

use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_vcs::{Curve, Want, partition};

use crate::state::ShardState;

/// Counter: divisions of the shard's room by the repositories' curves. Format: a counter name.
pub(crate) const DIVIDED: &str = "vcs_share.grants_divided";

/// Format: nanoseconds in a second: a curve's counts over its horizon in nanoseconds, read as misses a second.
const NANOS_PER_SECOND: f64 = 1e9;

/// One division of the shard's room (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Grants {
  /// The divisions made on this shard: one more each time.
  pub(crate) generation: u64,
  /// The shard clock's nanoseconds it was divided at.
  pub(crate) at_ns: u64,
  /// The room divided, in bytes.
  pub(crate) room: u64,
  /// The repositories it was divided among, in id order.
  served: Vec<RepositoryId>,
  /// The cadence it is good for: the shortest read horizon among them, in nanoseconds; `None` before any is learned.
  pub(crate) cadence_ns: Option<u64>,
  /// Each repository's bytes.
  given: BTreeMap<RepositoryId, u64>,
  /// The room no curve would use.
  pub(crate) spare: u64,
}

impl Grants {
  /// Repository `id`'s bytes, when it was among those divided.
  pub(crate) fn of(&self, id: RepositoryId) -> Option<u64> {
    self.given.get(&id).copied()
  }
}

/// The shard's grants as of `now_ns`, divided again first when they are due (module docs, "When it is divided").
pub(crate) fn current_at(state: &mut ShardState, now_ns: u64) -> &Grants {
  if due(state, now_ns) {
    let divided = divide(state, now_ns);
    state.vcs_grants = divided;
    state.count(DIVIDED, 1);
  }
  &state.vcs_grants
}

/// Whether the grants kept are due to be divided again at `now_ns`.
fn due(state: &ShardState, now_ns: u64) -> bool {
  let kept = &state.vcs_grants;
  if kept.generation == 0 {
    return true;
  }
  let served = state.vcs_usage.rows().map(|(id, _)| *id);
  if !served.eq(kept.served.iter().copied()) {
    return true;
  }
  kept
    .cadence_ns
    .is_some_and(|cadence| now_ns.saturating_sub(kept.at_ns) >= cadence)
}

/// Divides the shard's room among its repositories' curves at `now_ns` (module docs).
fn divide(state: &mut ShardState, now_ns: u64) -> Grants {
  state.vcs_usage.age_curves(now_ns);
  let room = state.store.admittable();
  let empty = Curve::new(1, 1);
  let rows: Vec<(RepositoryId, &Curve, f64, Option<f64>)> = state
    .vcs_usage
    .rows()
    .map(|(id, row)| {
      let horizon = row.read_horizon_ns();
      let per_s = horizon.map_or(0.0, |tau| NANOS_PER_SECOND / tau);
      (*id, row.curve().unwrap_or(&empty), per_s, horizon)
    })
    .collect();
  let wants: Vec<Want<'_>> = rows
    .iter()
    .map(|(_, curve, per_s, _)| Want {
      curve,
      per_s: *per_s,
    })
    .collect();
  let division = partition(room, &wants);
  let cadence_ns = rows
    .iter()
    .filter_map(|(_, _, _, horizon)| *horizon)
    .fold(None, |least: Option<f64>, tau| {
      Some(least.map_or(tau, |least| least.min(tau)))
    })
    .map(slates_vcs::whole_units);
  Grants {
    generation: state.vcs_grants.generation.saturating_add(1),
    at_ns: now_ns,
    room,
    served: rows.iter().map(|(id, ..)| *id).collect(),
    cadence_ns,
    given: rows
      .iter()
      .map(|(id, ..)| *id)
      .zip(division.given)
      .collect(),
    spare: division.spare,
  }
}

#[cfg(test)]
mod tests {
  use slates_db::catalog::RepositoryId;
  use slates_vcs::Work;

  use super::current_at;
  use crate::state::ShardState;

  /// Shape: a millisecond in the shard clock's nanoseconds, the tests' rhythm.
  const MILLISECOND: u64 = 1_000_000;

  fn id(byte: u8) -> RepositoryId {
    RepositoryId { bytes: [byte; 16] }
  }

  /// Repository `id` given `fetches` answered fetches a millisecond apart from `from`, and its curve `reads` reads of
  /// `objects` one-window objects in turn: its usage row, its read horizon and its working set.
  fn used(
    state: &mut ShardState,
    repository: RepositoryId,
    (fetches, from): (u64, u64),
    (objects, reads): (u64, u64),
  ) {
    for fetch in 0..fetches {
      state
        .vcs_usage
        .answered(&repository, Work::Fetch, 100, from + fetch * MILLISECOND);
    }
    let window = 1 << 16;
    let mut curve = state
      .vcs_usage
      .take_curve(&repository, (1 << 16, window), from);
    for read in 0..reads {
      let object = (u64::from(repository.bytes[0]) << 32) | (read % objects);
      let key = u64::from_le_bytes(
        blake3::hash(&object.to_le_bytes()).as_bytes()[..8]
          .try_into()
          .unwrap_or_default(),
      );
      curve.read(key, window);
    }
    state.vcs_usage.put_curve(&repository, curve);
  }

  /// A-724 piece 2d (§4.17.5 "Shares ... published as an immutable snapshot at the learning cadence"). Do: give two
  /// repositories fetches a millisecond apart and curves of 100 and 300 one-window objects read again and again; ask
  /// the grants, again a nanosecond later, after their shortest read horizon has passed, and after a third repository
  /// appears. Expect: the first division gives each its working set (all of the room is far past both) and the rest
  /// spare; the second answer is the same division, not divided again; past the cadence a new division; a new
  /// repository divides again at once.
  #[test]
  fn t_a724_the_grants_are_divided_at_the_learning_cadence_and_kept_between() {
    crate::test_store::audit_on_stored_shard("vcs_share-grants", |state| {
      let (first, second) = (id(1), id(2));
      used(state, first, (50, 1), (100, 2_000));
      used(state, second, (50, 1), (300, 6_000));
      let now = 1 + 50 * MILLISECOND;
      let window = 1u64 << 16;
      let grants = current_at(state, now).clone();
      assert_eq!(grants.generation, 1);
      assert!(
        grants.of(first).is_some_and(|bytes| bytes >= 100 * window),
        "{grants:?}"
      );
      assert!(
        grants.of(second).is_some_and(|bytes| bytes >= 300 * window),
        "{grants:?}"
      );
      assert!(grants.spare > 0, "{grants:?}");
      let cadence = grants.cadence_ns.expect("a learned cadence");
      assert_eq!(
        current_at(state, now + 1).generation,
        1,
        "divided again within its cadence"
      );
      assert_eq!(
        current_at(state, now + cadence).generation,
        2,
        "not divided at its cadence"
      );
      used(state, id(3), (1, now + cadence), (10, 10));
      assert_eq!(
        current_at(state, now + cadence + 1).generation,
        3,
        "a new repository did not divide again"
      );
    });
  }

  /// A-724 piece 2d (§4.17.5 "Admission": a work admitted against its repository's share, else the room nobody's share
  /// is using). Do: give two repositories curves (100 and 300 one-window objects) and fetch horizons, make both live,
  /// and ask each one's room. Expect: each room read from the published grants (counted `vcs_share.by_grants`), at most
  /// its grant plus the spare (the snapshot slot, divided as before, may bind sooner), so the hot one's room leaves out
  /// the small one's grant.
  #[test]
  fn t_a724_a_contested_room_reads_the_published_grants() {
    crate::test_store::audit_on_stored_shard("vcs_share-grants", |state| {
      let (small, hot) = (id(5), id(6));
      used(state, small, (50, 1), (100, 2_000));
      used(state, hot, (50, 1), (300, 6_000));
      let _live = (
        crate::vcs_share::Live::begin(small),
        crate::vcs_share::Live::begin(hot),
      );
      let before = state
        .refusals
        .get(crate::vcs_share::BY_GRANTS)
        .copied()
        .unwrap_or(0);
      let small_room = crate::vcs_share::room(state, small);
      let hot_room = crate::vcs_share::room(state, hot);
      assert_eq!(
        state
          .refusals
          .get(crate::vcs_share::BY_GRANTS)
          .copied()
          .unwrap_or(0),
        before + 2
      );
      let grants = state.vcs_grants.clone();
      let (small_grant, hot_grant) = (grants.of(small).unwrap_or(0), grants.of(hot).unwrap_or(0));
      // Memory binds at most at the grant and the spare; the snapshot slot, divided as before, may bind sooner.
      assert!(
        small_room.bytes <= small_grant + grants.spare,
        "{small_room:?} {grants:?}"
      );
      assert!(
        hot_room.bytes <= hot_grant + grants.spare,
        "{hot_room:?} {grants:?}"
      );
      assert!(
        hot_room.bytes + small_grant <= grants.room,
        "the hot repository took the small one's grant"
      );
    });
  }

  /// A-724 piece 2d (no overcommit at a newcomer, w-balance's condition). Do: give one repository a curve and a
  /// second, live beside it, none (its first work). Expect: the room not read from the grants (`vcs_share.by_grants`
  /// unmoved) but divided per request as before, so the newcomer is not given all the shard admits.
  #[test]
  fn t_a724_a_live_repository_with_no_curve_leaves_the_room_divided_as_before() {
    crate::test_store::audit_on_stored_shard("vcs_share-grants", |state| {
      let (curved, newcomer) = (id(7), id(8));
      used(state, curved, (50, 1), (100, 2_000));
      state.vcs_usage.answered(&newcomer, Work::Intake, 100, 1);
      let _live = (
        crate::vcs_share::Live::begin(curved),
        crate::vcs_share::Live::begin(newcomer),
      );
      let before = state
        .refusals
        .get(crate::vcs_share::BY_GRANTS)
        .copied()
        .unwrap_or(0);
      let room = crate::vcs_share::room(state, newcomer);
      assert_eq!(
        state
          .refusals
          .get(crate::vcs_share::BY_GRANTS)
          .copied()
          .unwrap_or(0),
        before
      );
      assert!(room.bytes < state.store.admittable(), "{room:?}");
    });
  }
}
