//! A git repository's versions encoded as their fragments once each, on the owner shard, before a fetch streams them
//! (A-718; §4.17.2 "A push" item 8, "the new versions' pack fragments encoded off the answer's path"; A-614 part 2's
//! fragments; A-653 continued (2)'s turns).
//!
//! **Why.** upload-pack's stream (A-639) copies a version's kept fragments, deltas and all, and searches deltas only for
//! the objects of a version not yet encoded (`slates_git::store::Upload`, "What it sends" 3). Nothing encoded a pushed
//! version, so every clone searched every object again: on the Mac Studio a clone of git/git cut at v1.0.0 (12,233
//! objects) spent about 72% of the daemon's CPU in `slates_git::store::deltas::Search::step` and took 2.816 s against
//! `git daemon`'s 0.300 s (BENCHMARKS.md, A-699's git row). git's own server sends the deltas its pack already holds.
//!
//! **What it does.** Before a fetch's answer is made, the request drives [`turn`] until it answers [`Turned::Idle`]:
//! each turn takes the repository's first version whose fragments are not kept (`Repository::first_unencoded`, by green
//! and then version), begins its encoding if none is under way, and advances it one turn (`Encoding::advance`): units of
//! work until the shard's quantum (§4.3's step budget) is spent on the shard's clock, at least one, lent every byte the
//! shard could admit now, then the arena grown by what the turn kept and the charge trued up, as the map's every step is
//! (`super::lend`, `super::settle`). The request yields between turns. A version is encoded once; every fetch after it
//! copies its fragments. So the first fetch after a push pays the search once, and the result is kept with the objects
//! as content, charged to the shard as everything the content store holds is.
//!
//! **Bounds.** One encoding a repository at a time, kept in the map between turns, so the encodings under way are
//! bounded by the partition's repositories; each holds what `Encoding` holds between its turns (its order, the walk's
//! bases, the part's bytes so far and its encoder's recent entries), a version's objects at most. Two fetches of one
//! repository drive the same encoding, whichever turn comes first.
//!
//! **Cancellation.** The encoding lives in the map, keyed by its repository, and a request that drives it holds the key
//! alone, so a request dropped between turns leaks nothing: the next fetch of the repository carries the encoding on,
//! and freeing the repository drops it ([`abandon`]).
//!
//! **Refused.** An encoding refused (the content store's room, a damaged record) is dropped, the version left
//! unencoded and counted ([`REFUSED`]); the fetch goes on and its stream searches that version's objects as before, so
//! a refusal costs time, never an answer. A refused version is offered again by the next fetch.
//!
//! **Owed** (GAPS' §4.17 entry). The parts' bytes an encoding holds between turns are charged to no budget until the
//! last unit keeps them as content. A push's own intake could record each object's delta as it arrives, so a pushed
//! version needs no search at all (git's pack reuse; w-git's sliced intake). The base pack is not made again here: a
//! clone is every version's fragments, in order, until the rebuild rule makes one. Kept fragments are not durable: a
//! recovery makes the store again from the log without them, and the next fetch encodes again.

use std::collections::BTreeMap;
use std::fmt;

use slates_db::catalog::RepositoryId;
use slates_git::store::learn::{Clock, Exchange};
use slates_git::store::{Encoding, Progress, StoreRefused, Turn};

use slates_vcs::{Prices, Rebuildable, amount_of, choose, whole_units};

use super::upload::{ShardClock, exchange};
use crate::state::ShardState;

/// Counter: a version's encoding begun. Format: a counter name.
pub(crate) const BEGUN: &str = "vcs_store.git_encode.begun";
/// Counter: a version's fragments kept. Format: a counter name.
pub(crate) const ENCODED: &str = "vcs_store.git_encode.encoded";
/// Counter: an encoding's turns. Format: a counter name.
pub(crate) const TURNS: &str = "vcs_store.git_encode.turns";
/// Counter: an encoding refused, its version left unencoded. Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.git_encode.refused";

/// The encodings under way, one a repository (the module doc's bounds), each with the green and version it encodes.
#[derive(Default)]
pub(crate) struct Encodings(BTreeMap<RepositoryId, (Encoding, (u64, usize))>);

impl fmt::Debug for Encodings {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_set().entries(self.0.keys()).finish()
  }
}

/// What a turn of a repository's encoding came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Turned {
  /// A version is under way, or another is still unencoded: the caller yields and turns again.
  More,
  /// Every version's fragments are kept (or the store is not a git store held here): nothing to do.
  Idle,
}

/// One turn of repository `id`'s encoding (the module doc), within the shard's quantum on its clock.
pub(crate) fn turn(state: &mut ShardState, id: RepositoryId) -> Turned {
  let began = slates_rt::futures::now_ns();
  let (turned, kept) = turn_uncounted(state, id);
  // A-724 piece 4: the turn's time and, once a version's fragments are kept, their bytes, on the repository's usage row,
  // so what making a byte of fragments again costs is learned from this repository's own encodings.
  let now = slates_rt::futures::now_ns();
  if turned == Turned::More || kept > 0 {
    state
      .vcs_usage
      .derived(&id, (now.saturating_sub(began), kept), now);
  }
  turned
}

/// [`turn`]'s work: what it came to, and the bytes of the fragments it kept (zero unless a version was finished).
fn turn_uncounted(state: &mut ShardState, id: RepositoryId) -> (Turned, u64) {
  let quantum_ns = state.config.step_quantum_ns();
  let exchange = exchange(&state.config);
  let mut clock = ShardClock;
  let clock: &mut dyn Clock = &mut clock;
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  super::lent_state(state, |state| {
    turn_lent(state, id, clock, quantum_ns, exchange)
  })
}

/// [`turn_uncounted`]'s work inside its lend.
fn turn_lent(
  state: &mut ShardState,
  id: RepositoryId,
  clock: &mut dyn Clock,
  quantum_ns: u64,
  exchange: Exchange,
) -> (Turned, u64) {
  let stores = &mut state.vcs_stores;
  let (Some(content), Some(super::RepositoryStore::Git(store))) =
    (stores.content.as_mut(), stores.entries.get_mut(&id))
  else {
    super::settle(state);
    return (Turned::Idle, 0);
  };
  let mut begun = false;
  if let std::collections::btree_map::Entry::Vacant(unencoded) = stores.encodings.0.entry(id) {
    let Some((green, version)) = store.repository.first_unencoded() else {
      super::settle(state);
      return (Turned::Idle, 0);
    };
    match Encoding::fragments(&store.repository, content, green, version) {
      Ok(encoding) => {
        unencoded.insert((encoding, (green, version)));
        begun = true;
      }
      Err(_) => {
        super::settle(state);
        state.count(REFUSED, 1);
        return (Turned::Idle, 0);
      }
    }
  }
  let encoding_at = stores.encodings.0.get(&id).map(|(_, at)| *at);
  let mut kept = 0;
  let advanced: Option<Result<Progress, StoreRefused>> =
    stores.encodings.0.get_mut(&id).map(|(encoding, _)| {
      encoding.advance(
        &mut store.repository,
        content,
        Turn {
          clock,
          exchange,
          quantum_ns,
        },
      )
    });
  let turned = match advanced {
    Some(Ok(Progress::More)) => Turned::More,
    Some(Ok(Progress::Done)) => {
      stores.encodings.0.remove(&id);
      kept = encoded_bytes(stores, id, encoding_at);
      state.count(ENCODED, 1);
      Turned::More
    }
    Some(Err(_)) | None => {
      stores.encodings.0.remove(&id);
      state.count(REFUSED, 1);
      Turned::Idle
    }
  };
  if begun {
    state.count(BEGUN, 1);
  }
  state.count(TURNS, 1);
  super::settle(state);
  (turned, kept)
}

/// The bytes repository `id`'s version at `at` keeps as its fragments (`Repository::kept_fragments`), once encoded.
fn encoded_bytes(stores: &super::VcsStores, id: RepositoryId, at: Option<(u64, usize)>) -> u64 {
  let (Some(at), Some(content), Some(super::RepositoryStore::Git(store))) =
    (at, stores.content.as_ref(), stores.entries.get(&id))
  else {
    return 0;
  };
  store
    .repository
    .kept_fragments(content)
    .into_iter()
    .find(|(version, _)| *version == at)
    .map_or(0, |(_, bytes)| bytes)
}

/// Counter: a version's kept fragments dropped for its repository's overrun (A-724 piece 4). Format: a counter name.
pub(crate) const DROPPED: &str = "vcs_store.git_encode.dropped";

/// The bytes repository `id`'s kept fragments give back if dropped (`Repository::kept_fragments`): what of its holding
/// is derived and can be made again (A-724 piece 4). Zero for a store not a git store held here.
pub(crate) fn kept_bytes(state: &ShardState, id: RepositoryId) -> u64 {
  let stores = &state.vcs_stores;
  let (Some(content), Some(super::RepositoryStore::Git(store))) =
    (stores.content.as_ref(), stores.entries.get(&id))
  else {
    return 0;
  };
  store
    .repository
    .kept_fragments(content)
    .into_iter()
    .fold(0u64, |total, (_, bytes)| total.saturating_add(bytes))
}

/// Gives back up to `overrun` bytes of repository `id`'s kept fragments, chosen by §4.11's cost model at `pressure`
/// (`slates_vcs::cold::choose`, A-724 piece 4): each version's fragments priced at their bytes against their making
/// again, at the time a byte of this repository's fragments took to make (its usage row's `derive_ns / derive_bytes`)
/// on each read its row says to expect before the next push (its fetches over its answered intakes). The bytes given
/// back. Nothing is dropped while the repository's encoding is under way (it may read a dropped version's entries as a
/// base) or before any of its encodings was measured (nothing to price a rebuild by).
pub(crate) fn relieve(
  state: &mut ShardState,
  id: RepositoryId,
  overrun: u64,
  pressure: f64,
) -> u64 {
  if overrun == 0 || state.vcs_stores.encodings.0.contains_key(&id) {
    return 0;
  }
  let Some(row) = state.vcs_usage.usage(&id) else {
    return 0;
  };
  if row.derive_bytes == 0 {
    return 0;
  }
  let per_byte_ns = amount_of(row.derive_ns) / amount_of(row.derive_bytes);
  let reads = amount_of(row.fetch.begun) / amount_of(row.intake.answered.max(1));
  let price = exchange(&state.config);
  let prices = Prices {
    ns_per_byte: amount_of(price.ns) / amount_of(price.bytes.max(1)),
    pressure,
  };
  let kept: Vec<((u64, usize), u64)> = {
    let stores = &state.vcs_stores;
    match (stores.content.as_ref(), stores.entries.get(&id)) {
      (Some(content), Some(super::RepositoryStore::Git(store))) => {
        store.repository.kept_fragments(content)
      }
      _ => return 0,
    }
  };
  let candidates: Vec<Rebuildable> = kept
    .iter()
    .map(|(_, bytes)| Rebuildable {
      bytes: *bytes,
      rebuild_ns: whole_units(amount_of(*bytes) * per_byte_ns),
      reads,
    })
    .collect();
  let chosen = choose(overrun, &candidates, prices);
  if chosen.is_empty() {
    return 0;
  }
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let mut given = 0u64;
  let mut dropped = 0u64;
  {
    let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
    let stores = &mut *lent.stores;
    if let (Some(content), Some(super::RepositoryStore::Git(store))) =
      (stores.content.as_mut(), stores.entries.get_mut(&id))
    {
      for (at, bytes) in chosen.iter().filter_map(|index| kept.get(*index)) {
        if let Ok(true) = store.repository.drop_fragments(content, *at) {
          given = given.saturating_add(*bytes);
          dropped = dropped.saturating_add(1);
        }
      }
    }
  }
  super::settle(state);
  state.count(DROPPED, dropped);
  given
}

/// Drops repository `id`'s encoding under way, if any: its repository is freed.
pub(crate) fn abandon(state: &mut ShardState, id: RepositoryId) {
  state.vcs_stores.encodings.0.remove(&id);
}

/// Turns repository `id`'s encoding until every version's fragments are kept, yielding between turns (the module doc):
/// what a fetch's request runs before its answer is made. Returns at once where the shard's state is not lent.
pub(crate) async fn settle_encodings(id: RepositoryId) {
  loop {
    match crate::state::with_state_counted(|state| turn(state, id)) {
      Some(Turned::More) => slates_rt::futures::yield_now().await,
      Some(Turned::Idle) | None => return,
    }
  }
}
