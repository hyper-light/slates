//! A git repository's versions admitted with no walk derived on the owner shard before a fetch reads them (A-614
//! amended, main's ruling (A) of 2026-11-21; `slates_git::store::Deriving`).
//!
//! **Why.** A create, a rewrite and a fast-forward over sets never walked are admitted with no walk, so git/git's whole
//! history, 1,027 refs mostly tags at old commits, is decided in time about its new objects rather than refs times
//! history. Their versions' sets (what each reaches, D(k)) are the closures of their heads, derived when a reader first
//! needs them. Upload-pack reads them for every fetch (its negotiation, its gather, its fragments), from the generation
//! a fetch holds, which copies the repository's memo of derived sets when it is made.
//!
//! **What it does.** Before a fetch holds its generation (beside the encoding of fragments, `super::git_encode`, which
//! reads D(k) too), the request drives [`turn`] until it answers [`Turned::Idle`]: each turn takes the repository's
//! derivation under way, or begins one of every version not yet known (`Repository::deriving_due`, the oldest heads
//! first, so each derivation's search back finds the one before as its base), and runs it for the shard's quantum on its
//! clock, at least one unit, keeping each version's sets as it comes (`Repository::keep_derived`). The request yields
//! between turns. Unlike an encoding, a derivation is never left behind a fetch: the fetch's answer is made from these
//! sets, so it waits for them, a unit a turn, the shard free between.
//!
//! **Bounds.** One derivation a repository, kept between turns, so the derivations under way are bounded by the
//! partition's repositories; each holds its queue of versions (the versions' own count bounds it) and its walk (a
//! history's objects at most). The derived values are content, charged to the shard as the content store's puts are,
//! and refused past its room, typed: a refusal drops the derivation, counted ([`REFUSED`]), and the fetch answers what
//! it can (a version still unknown refuses it, typed).
//!
//! **Cancellation.** The derivation lives in the map, keyed by its repository; a request dropped between turns leaks
//! nothing, the next fetch carries it on, and freeing the repository drops it ([`abandon`]).

use std::collections::BTreeMap;
use std::fmt;

use slates_db::catalog::RepositoryId;
use slates_git::store::hook_view::Quantum;
use slates_git::store::{Derived, Deriving};

use super::upload::ShardClock;
use crate::state::ShardState;

/// Counter: a version's sets derived and kept. Format: a counter name.
pub(crate) const DERIVED: &str = "vcs_store.git_derive.derived";
/// Counter: a derivation's turns. Format: a counter name.
pub(crate) const TURNS: &str = "vcs_store.git_derive.turns";
/// Counter: a derivation refused (the content store's room, a damaged record), dropped. Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.git_derive.refused";

/// The derivations under way, one a repository (the module doc's bounds).
#[derive(Default)]
pub(crate) struct Derivings(BTreeMap<RepositoryId, Deriving>);

impl fmt::Debug for Derivings {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_set().entries(self.0.keys()).finish()
  }
}

/// What a turn of a repository's derivation came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Turned {
  /// A derivation is under way, or versions are still unknown: the caller yields and turns again.
  More,
  /// Every version's sets are known (or the store is not a git store held here, or its derivation was refused).
  Idle,
}

/// One turn of repository `id`'s derivation (the module doc), within the shard's quantum on its clock.
pub(crate) fn turn(state: &mut ShardState, id: RepositoryId) -> Turned {
  let quantum_ns = state.config.step_quantum_ns();
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let (turned, derived, refused) =
    super::lent_state(state, |state| turn_lent(state, id, quantum_ns));
  state.count(TURNS, 1);
  if derived > 0 {
    state.count(DERIVED, derived);
  }
  if refused {
    state.count(REFUSED, 1);
  }
  turned
}

/// [`turn`]'s work inside its lend: what it came to, the versions it kept, and whether it was refused.
fn turn_lent(state: &mut ShardState, id: RepositoryId, quantum_ns: u64) -> (Turned, u64, bool) {
  let stores = &mut state.vcs_stores;
  let (Some(content), Some(super::RepositoryStore::Git(store))) =
    (stores.content.as_mut(), stores.entries.get_mut(&id))
  else {
    super::settle(state);
    return (Turned::Idle, 0, false);
  };
  let repository = &store.repository;
  let deriving = match stores.derivings.0.remove(&id) {
    Some(deriving) => deriving,
    None if repository.derivations_due() => repository.deriving_due(content),
    None => {
      super::settle(state);
      return (Turned::Idle, 0, false);
    }
  };
  let mut deriving = deriving;
  let mut clock = ShardClock;
  let mut derived = 0u64;
  let outcome = loop {
    let quantum = Quantum {
      clock: &mut clock,
      ns: quantum_ns,
    };
    match repository.derive_turn(content, &mut deriving, quantum) {
      Ok(Derived::More) => break Ok(Turned::More),
      Ok(Derived::Sets {
        green,
        version,
        sets,
      }) => {
        if let Err(refused) = repository.keep_derived(content, (green, version), &sets) {
          break Err(refused);
        }
        derived = derived.saturating_add(1);
        // A version kept ends the turn: its put was the turn's last unit.
        break Ok(Turned::More);
      }
      // Done with the versions it queued; a push meanwhile may have left more, which the next turn queues.
      Ok(Derived::Done) => {
        break Ok(if repository.derivations_due() {
          Turned::More
        } else {
          Turned::Idle
        });
      }
      Err(refused) => break Err(refused),
    }
  };
  let answer = match outcome {
    Ok(Turned::More) => {
      // Kept for the next turn unless it is done.
      if !deriving.is_done() {
        stores.derivings.0.insert(id, deriving);
      }
      (Turned::More, derived, false)
    }
    Ok(Turned::Idle) => (Turned::Idle, derived, false),
    Err(_) => (Turned::Idle, derived, true),
  };
  super::settle(state);
  answer
}

/// Drops repository `id`'s derivation under way, if any: its repository is freed.
pub(crate) fn abandon(state: &mut ShardState, id: RepositoryId) {
  state.vcs_stores.derivings.0.remove(&id);
}

/// Turns repository `id`'s derivation until every version's sets are known, yielding between turns (the module doc):
/// what a fetch's request runs before it holds its generation. Returns at once where the shard's state is not lent.
pub(crate) async fn derive_for_read(id: RepositoryId) {
  loop {
    match crate::state::with_state_counted(|state| turn(state, id)) {
      Some(Turned::More) => slates_rt::futures::yield_now().await,
      Some(Turned::Idle) | None => return,
    }
  }
}
