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
use slates_git::store::learn::Clock;
use slates_git::store::{Encoding, Progress, StoreRefused, Turn};

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

/// The encodings under way, one a repository (the module doc's bounds).
#[derive(Default)]
pub(crate) struct Encodings(BTreeMap<RepositoryId, Encoding>);

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
  let quantum_ns = state.config.step_quantum_ns();
  let exchange = exchange(&state.config);
  let mut clock = ShardClock;
  let clock: &mut dyn Clock = &mut clock;
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let stores = &mut state.vcs_stores;
  let (Some(content), Some(super::RepositoryStore::Git(store))) =
    (stores.content.as_mut(), stores.entries.get_mut(&id))
  else {
    super::settle(state);
    return Turned::Idle;
  };
  let mut begun = false;
  if let std::collections::btree_map::Entry::Vacant(unencoded) = stores.encodings.0.entry(id) {
    let Some((green, version)) = store.repository.first_unencoded() else {
      super::settle(state);
      return Turned::Idle;
    };
    match Encoding::fragments(&store.repository, content, green, version) {
      Ok(encoding) => {
        unencoded.insert(encoding);
        begun = true;
      }
      Err(_) => {
        super::settle(state);
        state.count(REFUSED, 1);
        return Turned::Idle;
      }
    }
  }
  let advanced: Option<Result<Progress, StoreRefused>> =
    stores.encodings.0.get_mut(&id).map(|encoding| {
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
  turned
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
