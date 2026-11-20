//! The one room a repository's work is admitted on its owner shard: the shard's admittable room, narrowed to the
//! repository's share of it while other repositories' intakes are under way (A-724 piece 3; §4.17.5 "Shares" and
//! "Admission and backpressure"; A-704, A-713, A-711).
//!
//! **One path.** Every admission of a version-control work asks [`room`] and nothing else: git's push as it starts
//! (`crate::git_push`), git's fetch (`crate::vcs_store::upload`'s lender), Mercurial's push (`crate::vcs_store::hg_view`).
//! It replaces A-704's `git_push::push_room`, which every one of them read before; A-722's reservation of a push's decode
//! window and post-state checks against this room too (main's ruling of 2026-11-20: one admission path).
//!
//! **The room.** A repository alone in taking new room gets all the shard can admit (`Store::admittable`): the fast
//! path, one map look-up and no division, counted ([`FAST`]). While other repositories have intakes under way, the
//! room they compete for (what the shard can admit, plus what each of them holds already: A-662's and A-714's charged
//! objects) is divided among them by weighted dominant-resource fairness with demands as caps
//! (`slates_vcs::share::divide`, A-724 piece 2), and the asker's room is its grant less what it holds, never more than
//! the shard can admit ([`Bound::Share`], counted [`NARROWED`] when it binds). So a repository holding much has less new
//! room while others push, and one asking little is given all it asks: a monorepo never starves a toy repository.
//!
//! **Demand, learned.** The asker claims what it holds and all the shard can admit, so its room is what the others'
//! claims leave. Each other repository with intakes under way claims what it holds plus, for each intake, the bytes its
//! answered intakes have brought on average (its usage row, `ShardState::vcs_usage`): its own measured traffic. One
//! with no answered intake yet claims everything the shard can admit, its first value taken from the machine, never a
//! literal. Weights are equal: the repository is the unit of fairness (§4.17.5).
//!
//! **The cold trigger** (A-724 piece 4). What a repository holds counts its rebuildable values too (a git repository's
//! kept fragments). Each claimant found holding more than its grant gives back, by §4.11's cost model
//! (`crate::vcs_store::git_encode::relieve`), the values worth more as room than as time saved, before the asker's room
//! is read.
//!
//! **Live intakes.** An intake is live from its start until its [`Live`] guard drops (the push answered, refused or
//! abandoned however its edge ended), kept in a map of this shard's thread apart from its state, so a guard dropped
//! while the state is borrowed still counts its end. One entry a repository with an intake under way, so bounded by
//! the partition's repositories.

use std::cell::RefCell;
use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_vcs::{Claim, Work, amount_of, divide, whole_units};

use crate::state::ShardState;

/// Counter: rooms given by the fast path, no other repository taking new room. Format: a counter name.
pub(crate) const FAST: &str = "vcs_share.fast";
/// Counter: rooms divided among repositories with intakes under way. Format: a counter name.
pub(crate) const DIVIDED: &str = "vcs_share.divided";
/// Counter: rooms the repository's share narrowed below what the shard could admit. Format: a counter name.
pub(crate) const NARROWED: &str = "vcs_share.narrowed";

/// What bounds a room ([`room`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bound {
  /// What the shard can admit (A-704's bound): a work past it is refused in the tool's words for a pack too large.
  Shard,
  /// The repository's share among repositories with intakes under way: a work past it is refused typed, retryably,
  /// naming the share and what the repository holds.
  Share {
    /// The repository's grant of the shard's contested room, in bytes.
    share: u64,
    /// What the repository holds already, in bytes.
    held: u64,
  },
}

/// A work's room and what bounds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Room {
  /// The bytes it may be admitted.
  pub(crate) bytes: u64,
  /// What bounds them.
  pub(crate) bound: Bound,
}

std::thread_local! {
  /// Each repository's intakes under way on this shard's thread (module docs, "Live intakes").
  static LIVE: RefCell<BTreeMap<RepositoryId, u64>> = const { RefCell::new(BTreeMap::new()) };
}

/// An intake under way, counted live until it drops (module docs).
#[derive(Debug)]
pub(crate) struct Live {
  /// Its repository.
  repository: RepositoryId,
}

impl Live {
  /// Counts an intake of `repository`'s live.
  pub(crate) fn begin(repository: RepositoryId) -> Live {
    LIVE.with(|live| {
      if let Ok(mut live) = live.try_borrow_mut() {
        let count = live.entry(repository).or_insert(0);
        *count = count.saturating_add(1);
      }
    });
    Live { repository }
  }
}

impl Drop for Live {
  fn drop(&mut self) {
    LIVE.with(|live| {
      if let Ok(mut live) = live.try_borrow_mut()
        && let Some(count) = live.get_mut(&self.repository)
      {
        *count = count.saturating_sub(1);
        if *count == 0 {
          live.remove(&self.repository);
        }
      }
    });
  }
}

/// Each repository with intakes under way on this shard, and how many.
fn live_intakes() -> Vec<(RepositoryId, u64)> {
  LIVE.with(|live| {
    live
      .try_borrow()
      .map(|live| live.iter().map(|(id, count)| (*id, *count)).collect())
      .unwrap_or_default()
  })
}

/// The room repository `repository`'s next work is admitted on `state`'s shard (module docs).
pub(crate) fn room(state: &mut ShardState, repository: RepositoryId) -> Room {
  let admittable = state.store.admittable();
  let live = live_intakes();
  let others: Vec<(RepositoryId, u64)> = live
    .iter()
    .copied()
    .filter(|(id, _)| *id != repository)
    .collect();
  if others.is_empty() {
    state.count(FAST, 1);
    return Room {
      bytes: admittable,
      bound: Bound::Shard,
    };
  }
  state.count(DIVIDED, 1);
  let own_live = live
    .iter()
    .find(|(id, _)| *id == repository)
    .map_or(0, |(_, count)| *count);
  let mut claimants = vec![(repository, own_live.saturating_add(1))];
  claimants.extend(others);
  let held: Vec<u64> = claimants
    .iter()
    .map(|(id, _)| held_by(state, *id))
    .collect();
  let capacity = held
    .iter()
    .fold(admittable, |total, bytes| total.saturating_add(*bytes));
  // The asker claims all it holds and all the shard can admit, so its room is whatever the others' claims leave it;
  // the others claim what their own traffic says they will use.
  let claims: Vec<Claim<1>> = claimants
    .iter()
    .zip(&held)
    .enumerate()
    .map(|(at, ((id, intakes), held))| {
      let demand = if at == 0 {
        held.saturating_add(admittable)
      } else {
        demand_of(state, *id, (*held, *intakes), admittable)
      };
      Claim {
        demand: [amount_of(demand)],
        weight: 1.0,
      }
    })
    .collect();
  let grants = divide(&[amount_of(capacity)], &claims);
  let share = grants
    .first()
    .and_then(|grant| grant.granted.first())
    .map_or(0, |granted| whole_units(*granted));
  // The cold trigger (A-724 piece 4): each claimant holding more than its grant gives back what its rebuildable values
  // are worth giving back, by §4.11's cost model at its pressure (what it holds over its grant).
  for ((id, _), (grant, holding)) in claimants.iter().zip(
    grants
      .iter()
      .filter_map(|grant| grant.granted.first())
      .zip(&held),
  ) {
    let grant = whole_units(*grant);
    if *holding > grant {
      let pressure = amount_of(*holding) / amount_of(grant.max(1));
      crate::vcs_store::git_encode::relieve(state, *id, holding.saturating_sub(grant), pressure);
    }
  }
  let own_held = held_by(state, repository);
  let headroom = share.saturating_sub(own_held);
  if headroom >= admittable {
    return Room {
      bytes: admittable,
      bound: Bound::Shard,
    };
  }
  state.count(NARROWED, 1);
  Room {
    bytes: headroom,
    bound: Bound::Share {
      share,
      held: own_held,
    },
  }
}

/// What repository `id` holds of its shard: its entries' objects (A-662, A-714) and its derived values that can be made
/// again (A-724 piece 4: a git repository's kept fragments).
fn held_by(state: &ShardState, id: RepositoryId) -> u64 {
  state
    .vcs_stores
    .objects_charged(id)
    .saturating_add(crate::vcs_store::git_encode::kept_bytes(state, id))
}

/// What repository `id` claims with `intakes` under way while it holds `held` (module docs, "Demand, learned"): what it
/// holds plus its answered intakes' mean bytes for each, or plus all the shard can admit before any was answered.
fn demand_of(
  state: &ShardState,
  id: RepositoryId,
  (held, intakes): (u64, u64),
  admittable: u64,
) -> u64 {
  let per_intake = state
    .vcs_usage
    .usage(&id)
    .and_then(|row| {
      let counts = row.work(Work::Intake);
      counts.bytes_in.checked_div(counts.answered)
    })
    .unwrap_or(admittable);
  held.saturating_add(per_intake.saturating_mul(intakes))
}

#[cfg(test)]
mod tests;
