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
//! clock, at least one unit, keeping each version's sets as it comes (`Repository::keep_derived`), and goes on so while
//! nobody else waits on the shard, to the step's ceiling (A-783's pace, as a push's build runs): a clone of git/git's
//! branches derived 8 versions in 3,024,021 turns, a few units each, each paying the shard's lend, its settle and a
//! yield (r52, 465 s). The request yields between turns. Unlike an encoding, a derivation is never left behind a fetch: the fetch's answer is made from these
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
/// Counter: a derivation's turns that ended early because someone else waited on the shard (A-783's pace: it runs to the
/// step's ceiling only while nobody waits). Read beside [`TURNS`], what tells turns the work's time took from turns its
/// shard's other work asked for. Format: a counter name.
pub(crate) const TURNS_YIELDED: &str = "vcs_store.git_derive.turns_yielded";

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
  /// A read met a window let go (goal 93 item 2b): the caller brings it back ([`super::read_through::bring_back`]) and
  /// turns again. The derivation under way is dropped and begun afresh from the versions still due: every version it
  /// derived is kept, so only the version it was on is walked again.
  Waiting(slates_git::store::packed::Waited),
}

/// One turn of repository `id`'s derivation (the module doc), within the shard's quantum on its clock.
pub(crate) fn turn(state: &mut ShardState, id: RepositoryId) -> Turned {
  let quantum_ns = state.config.step_quantum_ns();
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let (turned, derived, refused) =
    super::lent_state(state, |state| turn_lent(state, id, quantum_ns));
  state.count(TURNS, 1);
  if YIELDED.with(|yielded| yielded.replace(false)) {
    state.count(TURNS_YIELDED, 1);
  }
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
  // A miss an earlier step left noted is not this turn's: cleared, so only a read of this turn names what it waits for.
  let _ = repository.waiting();
  let (outcome, derived) = paced(repository, content, &mut deriving, quantum_ns);
  let answer = match outcome {
    Ok(Turned::More) => {
      // Kept for the next turn unless it is done.
      if !deriving.is_done() {
        stores.derivings.0.insert(id, deriving);
      }
      (Turned::More, derived, false)
    }
    Ok(Turned::Idle) => (Turned::Idle, derived, false),
    Ok(Turned::Waiting(waited)) => (Turned::Waiting(waited), derived, false),
    // A read that met a window let go: waited for, never refused (the unit it was on cannot be taken again, so the
    // derivation is begun afresh once the window is back).
    Err(_) => match repository.waiting() {
      Some(waited) => (Turned::Waiting(waited), derived, false),
      None => (Turned::Idle, derived, true),
    },
  };
  super::settle(state);
  answer
}

thread_local! {
  /// Whether this shard's turn under way ended early because someone else waited ([`TURNS_YIELDED`]), read and cleared
  /// by [`turn`] once its lend ends.
  static YIELDED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Turns of `deriving` while nobody else waits on the shard, to the step's ceiling (A-783's pace, the module doc), each
/// version kept as it comes: what came of it, and the versions kept.
fn paced(
  repository: &slates_git::store::Repository,
  content: &mut slates_vcs::ContentStore,
  deriving: &mut Deriving,
  quantum_ns: u64,
) -> (Result<Turned, slates_git::store::StoreRefused>, u64) {
  let began = slates_rt::futures::now_ns();
  let ceiling = slates_rt::futures::step_ceiling_ns()
    .unwrap_or(quantum_ns)
    .max(quantum_ns);
  let go_on = || {
    if slates_rt::futures::others_waiting() {
      YIELDED.with(|yielded| yielded.set(true));
      return false;
    }
    slates_rt::futures::now_ns().saturating_sub(began) < ceiling.saturating_sub(quantum_ns)
  };
  let mut clock = ShardClock;
  let mut derived = 0u64;
  loop {
    let quantum = Quantum {
      clock: &mut clock,
      ns: quantum_ns,
    };
    match repository.derive_turn(content, deriving, quantum) {
      Ok(Derived::More) if go_on() => {}
      Ok(Derived::More) => return (Ok(Turned::More), derived),
      Ok(Derived::Sets {
        green,
        version,
        sets,
      }) => {
        if let Err(refused) = repository.keep_derived(content, (green, version), &sets) {
          return (Err(refused), derived);
        }
        derived = derived.saturating_add(1);
        if !go_on() {
          return (Ok(Turned::More), derived);
        }
      }
      // Done with the versions it queued; a push meanwhile may have left more, which the next turn queues.
      Ok(Derived::Done) => {
        let due = repository.derivations_due();
        return (Ok(if due { Turned::More } else { Turned::Idle }), derived);
      }
      Err(refused) => return (Err(refused), derived),
    }
  }
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
      // A window brought back, turned again; one that brought nothing back (back already, or refused) would only meet the
      // same miss again, so the derivation stops there as a refused one does.
      Some(Turned::Waiting(waited)) => {
        if !matches!(super::read_through::bring_back(waited).await, Ok(brought) if brought > 0) {
          return;
        }
      }
      Some(Turned::Idle) | None => return,
    }
  }
}
