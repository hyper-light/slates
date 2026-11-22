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
//! next intake is learned to bring (its usage row, `ShardState::vcs_usage`, A-724 piece 2's `slates_vcs::Rates`: the
//! mean bytes of its answered intakes at the horizon that best predicted each of them, so a repository whose pushes grew
//! claims their new size, not its lifetime's mean). One
//! with no answered intake yet claims everything the shard can admit, its first value taken from the machine, never a
//! literal. Weights are equal: the repository is the unit of fairness (§4.17.5).
//!
//! **The cold trigger** (A-724 piece 4). What a repository holds counts its rebuildable values too (a git repository's
//! kept fragments). Each claimant found holding more than its grant gives back, by §4.11's cost model
//! (`crate::vcs_store::git_encode::relieve`), the values worth more as room than as time saved, before the asker's room
//! is read.
//!
//! **Two resources.** A push's record grows its partition's snapshot (A-724 piece 8a), so the slot's room left
//! (`slates_db::replay::Db::snapshot_room`) is divided with memory by dominant-resource fairness: each repository holds
//! its records' bytes of it, claims them and its traffic's mean, and its room is the lesser headroom; alone, the lesser
//! of what the shard admits and what the slot has left ([`Bound::Snapshot`]).
//!
//! **Live works.** An intake is live from its start until its [`Live`] guard drops (the push or commit answered,
//! refused or abandoned however its edge ended); a fetch's answer is live while it holds its repository's generation
//! ([`Live::fetch`], A-724 piece 3d: a fetch's room is a claim of its own). Every repository with a live work of either
//! kind is a claimant in another's room, claiming what it holds plus, for each live work, what a work of that kind is
//! learned to bring or use. Kept in a map of this shard's thread apart from its state, so a guard dropped while the state
//! is borrowed still counts its end. One entry a repository with a work under way, so bounded by the partition's
//! repositories.

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
  /// The partition's snapshot slot's room left (A-724 piece 8a): a work past it is refused typed, naming the room, since
  /// its record would not fit the snapshot until the slot is laid out larger.
  Snapshot {
    /// The slot's room left, in bytes.
    room: u64,
  },
  /// The repository's share among repositories with intakes under way, of memory or of the snapshot slot, whichever
  /// binds: a work past it is refused typed, retryably, naming the share and what the repository holds of it.
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
  /// Each repository's works under way on this shard's thread, by kind (module docs, "Live intakes"): `[intakes,
  /// fetches]`.
  static LIVE: RefCell<BTreeMap<RepositoryId, [u64; 2]>> = const { RefCell::new(BTreeMap::new()) };
}

/// The slot of `work` in a repository's live counts.
fn slot(work: Work) -> usize {
  match work {
    Work::Intake => 0,
    Work::Fetch => 1,
  }
}

/// A work under way, counted live until it drops (module docs): an intake ([`Live::begin`]) or a fetch's answer
/// ([`Live::fetch`], A-724 piece 3d).
#[derive(Debug)]
pub(crate) struct Live {
  /// Its repository.
  repository: RepositoryId,
  /// Its kind.
  work: Work,
}

impl Live {
  /// Counts an intake of `repository`'s live.
  pub(crate) fn begin(repository: RepositoryId) -> Live {
    Live::of(repository, Work::Intake)
  }

  /// Counts a fetch's answer of `repository`'s live (A-724 piece 3d: a fetch's room is a claim of its own).
  pub(crate) fn fetch(repository: RepositoryId) -> Live {
    Live::of(repository, Work::Fetch)
  }

  fn of(repository: RepositoryId, work: Work) -> Live {
    LIVE.with(|live| {
      if let Ok(mut live) = live.try_borrow_mut()
        && let Some(count) = live.entry(repository).or_insert([0, 0]).get_mut(slot(work))
      {
        *count = count.saturating_add(1);
      }
    });
    Live { repository, work }
  }
}

impl Drop for Live {
  fn drop(&mut self) {
    LIVE.with(|live| {
      if let Ok(mut live) = live.try_borrow_mut()
        && let Some(counts) = live.get_mut(&self.repository)
      {
        if let Some(count) = counts.get_mut(slot(self.work)) {
          *count = count.saturating_sub(1);
        }
        if counts.iter().all(|count| *count == 0) {
          live.remove(&self.repository);
        }
      }
    });
  }
}

/// Each repository with works under way on this shard, and how many of each kind (`[intakes, fetches]`).
fn live_works() -> Vec<(RepositoryId, [u64; 2])> {
  LIVE.with(|live| {
    live
      .try_borrow()
      .map(|live| live.iter().map(|(id, counts)| (*id, *counts)).collect())
      .unwrap_or_default()
  })
}

/// The room repository `repository`'s next work is admitted on `state`'s shard (module docs).
pub(crate) fn room(state: &mut ShardState, repository: RepositoryId) -> Room {
  let admittable = state.store.admittable();
  // A-724 piece 8a: a push's record grows its partition's snapshot (today by about its pack), so the slot's room left
  // is a resource as memory is (`slates_db::replay::Db::snapshot_room`).
  let slot_room = state.db.snapshot_room(&state.segment);
  let live = live_works();
  let others: Vec<(RepositoryId, [u64; 2])> = live
    .iter()
    .copied()
    .filter(|(id, _)| *id != repository)
    .collect();
  if others.is_empty() {
    state.count(FAST, 1);
    return alone(admittable, slot_room);
  }
  state.count(DIVIDED, 1);
  let own_live = live
    .iter()
    .find(|(id, _)| *id == repository)
    .map_or([0, 0], |(_, counts)| *counts);
  let mut claimants = vec![(repository, own_live)];
  claimants.extend(others);
  let held: Vec<[u64; 2]> = claimants
    .iter()
    .map(|(id, _)| [held_by(state, *id), snapshot_held_by(state, *id)])
    .collect();
  let capacity = held.iter().fold([admittable, slot_room], |total, held| {
    [
      total[0].saturating_add(held[0]),
      total[1].saturating_add(held[1]),
    ]
  });
  // The asker claims all it holds and all of each resource that is free, so its room is whatever the others' claims
  // leave it; the others claim what their own traffic says they will use, of memory and of the slot alike.
  let claims: Vec<Claim<2>> = claimants
    .iter()
    .zip(&held)
    .enumerate()
    .map(|(at, ((id, works), held))| {
      let demand = if at == 0 {
        [
          held[0].saturating_add(admittable),
          held[1].saturating_add(slot_room),
        ]
      } else {
        [
          demand_of(state, *id, (held[0], *works), admittable),
          demand_of(state, *id, (held[1], *works), slot_room),
        ]
      };
      Claim {
        demand: demand.map(amount_of),
        weight: 1.0,
      }
    })
    .collect();
  let grants = divide(&capacity.map(amount_of), &claims);
  // The cold trigger (A-724 piece 4): each claimant holding more memory than its grant gives back what its rebuildable
  // values are worth giving back, by §4.11's cost model at its pressure (what it holds over its grant).
  for ((id, _), (grant, holding)) in claimants.iter().zip(
    grants
      .iter()
      .filter_map(|grant| grant.granted.first())
      .zip(held.iter().map(|held| held[0])),
  ) {
    let grant = whole_units(*grant);
    if holding > grant {
      let pressure = amount_of(holding) / amount_of(grant.max(1));
      crate::vcs_store::git_encode::relieve(state, *id, holding.saturating_sub(grant), pressure);
    }
  }
  let own = grants
    .first()
    .map_or([0, 0], |grant| grant.granted.map(whole_units));
  let own_held = [
    held_by(state, repository),
    snapshot_held_by(state, repository),
  ];
  let headroom = [
    own[0].saturating_sub(own_held[0]),
    own[1].saturating_sub(own_held[1]),
  ];
  let shared = headroom[0].min(headroom[1]);
  if shared >= admittable.min(slot_room) {
    return alone(admittable, slot_room);
  }
  state.count(NARROWED, 1);
  // The resource whose share binds names the refusal: its grant and what the repository holds of it.
  let binding = usize::from(headroom[1] < headroom[0]);
  Room {
    bytes: shared,
    bound: Bound::Share {
      share: own.get(binding).copied().unwrap_or(0),
      held: own_held.get(binding).copied().unwrap_or(0),
    },
  }
}

/// A repository's room with no other taking new room: all the shard can admit, or the snapshot slot's room left where
/// that is less ([`Bound::Snapshot`]).
fn alone(admittable: u64, slot_room: u64) -> Room {
  if slot_room < admittable {
    Room {
      bytes: slot_room,
      bound: Bound::Snapshot { room: slot_room },
    }
  } else {
    Room {
      bytes: admittable,
      bound: Bound::Shard,
    }
  }
}

/// What repository `id`'s records hold of its partition's snapshot (A-724 piece 8a): its entries' bytes in the
/// partition's register, which every snapshot carries (`slates_db` `Repositories::objects_bytes`).
fn snapshot_held_by(state: &ShardState, id: RepositoryId) -> u64 {
  u64::try_from(state.db.partition().repositories().objects_bytes(id)).unwrap_or(u64::MAX)
}

/// What repository `id` holds of its shard: its entries' objects (A-662, A-714) and its derived values that can be made
/// again (A-724 piece 4: a git repository's kept fragments).
fn held_by(state: &ShardState, id: RepositoryId) -> u64 {
  state
    .vcs_stores
    .objects_charged(id)
    .saturating_add(crate::vcs_store::git_encode::kept_bytes(state, id))
}

/// What repository `id` claims with `works` under way (`[intakes, fetches]`) while it holds `held` (module docs, "Demand,
/// learned"): what it holds plus, for each live work, the bytes a work of its kind is learned to bring or use
/// (`slates_vcs::Rates`, A-724 pieces 2 and 3d), or all the shard can admit for a kind none of whose works was answered
/// yet.
fn demand_of(
  state: &ShardState,
  id: RepositoryId,
  (held, works): (u64, [u64; 2]),
  admittable: u64,
) -> u64 {
  let learned = |work: Work| {
    state
      .vcs_usage
      .usage(&id)
      .and_then(|row| row.learned(work).bytes_per_work())
      .unwrap_or(admittable)
  };
  [Work::Intake, Work::Fetch]
    .into_iter()
    .zip(works)
    .filter(|(_, live)| *live > 0)
    .fold(held, |claim, (work, live)| {
      claim.saturating_add(learned(work).saturating_mul(live))
    })
}

pub(crate) mod place;
pub(crate) mod turns;

#[cfg(test)]
mod tests;
