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
//! **What it does.** Before a fetch's answer is made, the request drives [`turn`] until it answers [`Turned::Idle`]
//! (or its wait ends, "A bounded wait" below): each turn takes the repository's first version whose fragments are not kept (`Repository::first_unencoded`, by green
//! and then version), begins its encoding if none is under way, and advances it one turn (`Encoding::advance`, paced
//! by A-783: to the quantum while someone waits on the shard, on to its ceiling while no one does): units of work, each
//! sized by its measured cost within the shard's quantum (§4.3's step budget; A-718, main's ruling of 2026-11-21 07:30),
//! at least one, lent every byte the
//! shard could admit now, then the arena grown by what the turn kept and the charge trued up, as the map's every step is
//! (`super::lend`, `super::settle`). The request yields between turns. A version is encoded once; every fetch after it
//! copies its fragments. So the first fetch after a push pays the search once, and the result is kept with the objects
//! as content, charged to the shard as everything the content store holds is.
//!
//! **Bounds.** One encoding a repository at a time, kept in the map between turns, so the encodings under way are
//! bounded by the partition's repositories; each holds what `Encoding` holds between its turns (its order, the walk's
//! bases, the part's bytes so far, charged, and its encoder's recent entries), a version's objects at most. Two fetches of one
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
//! **A bounded wait** (A-718 continued). A fetch waits for the encoding at most one liveness window (`encode_for_fetch`);
//! past it the answer begins, its stream searching what is still unencoded, and a task finishes the encoding behind it
//! (`encode_behind`), one a repository, so a long history pushed whole never holds a fetch's first byte past the window.
//!
//! **Made ahead only for a client, within a budget** (A-856, main's ruling of 2026-11-22 00:30: derived data made ahead
//! is charged to the repository's learned budget, and nothing grows while no client is connected; clients measured
//! git/git's clone at 917 MiB as it ended and 1,695 MiB 90 s later with no client). The task behind a fetch turns the
//! encoding only while a fetch of the repository is answered and what the repository holds is within its learned grant
//! (`crate::vcs_share::budget`: its working-set curve's share of the shard, aged); past either it stops, counted
//! [`AHEAD_STOPPED`], and nothing is made ahead until a fetch asks again (that fetch's wait encodes, its stream searching
//! the rest). As a repository goes idle (its last fetch ending, or the task stopping with none answered) its encoding
//! under way is dropped and its kept fragments past its grant are given back by §4.11's cost model ([`give_back_idle`],
//! counted [`GIVEN_BACK_IDLE`]). An encoding's parts are charged to the content store as they grow
//! (`slates_git::store::Encoding::held_bytes`) and given back as they are kept or the encoding is dropped.
//!
//! **Owed** (GAPS' §4.17 entry). Each object's deflate makes a fresh state (clients' table: 97 GB made over one git/git
//! clone), owed next: one a shard, reset. A push's own intake could record each object's delta as it arrives, so a pushed
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
/// Counter: fetches that stopped waiting for their repository's encoding and answered, a task finishing it behind them.
pub(crate) const BEHIND: &str = "vcs_store.git_encode.behind";

/// How long a fetch waits for its repository's encoding before it answers (A-718 continued): one liveness window, the
/// time the daemon allows any step's work before it is presumed stuck, so a client sees its answer begin within it
/// however many versions a push left unencoded. Past it the fetch's stream searches what is still unencoded, a turn's
/// object at a time as before A-718, and a task finishes the encoding for the fetches after it.
const FETCH_WAIT_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
/// Counter: a unit of an encoding (a pair of trees walked, a window of rows read, an object encoded, the parts kept)
/// that alone took longer than its turn's quantum, the step budget (A-718: a turn may overrun its quantum by its last
/// unit, and no unit may outgrow the quantum). Format: a counter name.
pub(crate) const UNITS_PAST_BUDGET: &str = "vcs_store.git_encode.units_past_budget";
/// Counters: [`UNITS_PAST_BUDGET`] by the stage the unit was in (`slates_git::store::EncodingCounters`'s order: the
/// walk, the gather, the encoding, the keeping), so the unit to split is named. Format: counter names.
pub(crate) const UNITS_PAST_BUDGET_BY_STAGE: [&str; 4] = [
  "vcs_store.git_encode.units_past_budget.walk",
  "vcs_store.git_encode.units_past_budget.gather",
  "vcs_store.git_encode.units_past_budget.encode",
  "vcs_store.git_encode.units_past_budget.keep",
];
/// Counter: objects a finished encoding sent on as their pushed entry lies, whole (A-859, git's pack reuse). Format: a
/// counter name.
pub(crate) const REUSED_WHOLE: &str = "vcs_store.git_encode.reused_whole";
/// Counter: objects a finished encoding sent on as their pushed delta (A-859). Format: a counter name.
pub(crate) const REUSED_DELTAS: &str = "vcs_store.git_encode.reused_deltas";
/// Counter: objects whose pushed entry failed its check and were encoded instead (A-859). Format: a counter name.
pub(crate) const REUSE_REFUSED: &str = "vcs_store.git_encode.reuse_refused";
/// Counter: objects a finished encoding found in no pack the store holds, encoded with no entry to reuse (A-859).
/// Format: a counter name.
pub(crate) const REUSE_NO_ENTRY: &str = "vcs_store.git_encode.reuse_no_entry";
/// Counter: objects whose pushed entry a finished encoding could not read, encoded instead (A-859). Format: a counter
/// name.
pub(crate) const REUSE_UNREAD: &str = "vcs_store.git_encode.reuse_unread";
/// Counter: an encoding refused, its version left unencoded. Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.git_encode.refused";

/// The encodings under way, one a repository (the module doc's bounds), each with the green and version it encodes.
#[derive(Default)]
pub(crate) struct Encodings(
  BTreeMap<RepositoryId, (Encoding, (u64, usize))>,
  /// The repositories whose encoding a task finishes behind a fetch that stopped waiting for it ([`encode_for_fetch`]).
  std::collections::BTreeSet<RepositoryId>,
);

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
  // A-614 amended: a version's D(k), which its fragments are encoded from, derived first where it is not known, so an
  // encoding turned by any path (a fetch's wait, the task behind it) never meets a version left for its first reader.
  if super::git_derive::turn(state, id) == super::git_derive::Turned::More {
    return Turned::More;
  }
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
  let mut past = 0;
  let mut reused = [0_u64; 5];
  let mut past_by_stage = [0_u64; UNITS_PAST_BUDGET_BY_STAGE.len()];
  let advanced: Option<Result<Progress, StoreRefused>> =
    stores.encodings.0.get_mut(&id).map(|(encoding, _)| {
      let before = encoding.counters().units_past_quantum;
      let before_by_stage = encoding.counters().units_past_quantum_by_stage;
      // A-783: advances of a quantum each, to the quantum while someone waits on the shard, on to its ceiling while
      // no one does, so units sized within the quantum (A-718) are not each a turn of their own.
      let mut costs = super::TurnCosts::ZERO;
      let mut paced = super::paced_turn(quantum_ns, slates_rt::futures::now_ns());
      let advanced = loop {
        if !paced.more(super::ANSWER_STEP, &mut costs) {
          break Ok(Progress::More);
        }
        match encoding.advance(
          &mut store.repository,
          content,
          Turn {
            clock: &mut *clock,
            exchange,
            quantum_ns,
          },
        ) {
          Ok(Progress::More) => {}
          other => break other,
        }
      };
      paced.close(slates_rt::futures::now_ns(), &mut costs);
      past = encoding
        .counters()
        .units_past_quantum
        .saturating_sub(before);
      for ((now, was), into) in encoding
        .counters()
        .units_past_quantum_by_stage
        .iter()
        .zip(before_by_stage)
        .zip(&mut past_by_stage)
      {
        *into = now.saturating_sub(was);
      }
      advanced
    });
  let turned = match advanced {
    Some(Ok(Progress::More)) => Turned::More,
    Some(Ok(Progress::Done)) => {
      if let Some((encoding, _)) = stores.encodings.0.remove(&id) {
        let counted = encoding.counters();
        reused = [
          counted.reused_whole,
          counted.reused_deltas,
          counted.reuse_refused,
          counted.reuse_no_entry,
          counted.reuse_unread,
        ];
      }
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
  for (name, by) in [
    REUSED_WHOLE,
    REUSED_DELTAS,
    REUSE_REFUSED,
    REUSE_NO_ENTRY,
    REUSE_UNREAD,
  ]
  .iter()
  .zip(reused)
  {
    if by > 0 {
      state.count(name, by);
    }
  }
  if past > 0 {
    state.count(UNITS_PAST_BUDGET, past);
    for (name, by) in UNITS_PAST_BUDGET_BY_STAGE.iter().zip(past_by_stage) {
      state.count(name, by);
    }
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
  drop_under_way(state, id);
  state.vcs_stores.encodings.1.remove(&id);
}

/// Drops repository `id`'s encoding under way, if any, what it charged the content store given back first (A-856).
fn drop_under_way(state: &mut ShardState, id: RepositoryId) {
  let stores = &mut state.vcs_stores;
  if let Some((mut encoding, _)) = stores.encodings.0.remove(&id)
    && let Some(content) = stores.content.as_mut()
  {
    encoding.release(content);
  }
  super::settle(state);
}

/// Counter: a turn of encoding ahead (the task behind a fetch that stopped waiting) not taken, its repository having no
/// fetch answered now or holding its learned grant already (A-856). Format: a counter name.
pub(crate) const AHEAD_STOPPED: &str = "vcs_store.git_encode.ahead_stopped";

/// Counter: bytes of a repository's derived values given back as it went idle (A-856): its encoding under way's parts
/// and its kept fragments past its learned grant. Format: a counter name.
pub(crate) const GIVEN_BACK_IDLE: &str = "vcs_store.git_encode.given_back_idle";

/// Whether repository `id`'s encoding may be turned ahead of any fetch waiting on it (A-856, main's ruling of
/// 2026-11-22 00:30: derived data made ahead is charged to the repository's learned budget, and nothing grows while no
/// client is connected): only while a fetch of it is answered now, and only while what it holds is within its learned
/// grant (`crate::vcs_share::budget`); none before a grant is learned, so nothing is made ahead without a budget.
fn ahead_allowed(state: &mut ShardState, id: RepositoryId) -> bool {
  if crate::vcs_share::fetches_live(id) == 0 {
    return false;
  }
  let held = crate::vcs_share::held(state, id);
  crate::vcs_share::budget(state, id).is_some_and(|grant| held < grant)
}

/// Gives back what repository `id` holds of derived values as it goes idle (A-856, "given back when idle"): its
/// encoding under way dropped (its parts' charge with it; the next fetch begins that version again), the objects its
/// reads keep let go (A-860), and its kept
/// fragments past its learned grant given back by §4.11's cost model ([`relieve`]); counted [`GIVEN_BACK_IDLE`]. A
/// repository with no grant learned yet keeps its fragments: nothing tells what they are worth.
pub(crate) fn give_back_idle(state: &mut ShardState, id: RepositoryId) {
  let under_way = state
    .vcs_stores
    .encodings
    .0
    .get(&id)
    .map_or(0, |(encoding, _)| encoding.held_bytes());
  drop_under_way(state, id);
  // A-860: what its reads keep (the objects read lately, git's delta base cache) is given back too, its budget kept.
  let recent = state.vcs_stores.git(id).map_or(0, |(content, store)| {
    store.repository.let_go_recent(content)
  });
  let mut given = under_way.saturating_add(recent);
  if let Some(grant) = crate::vcs_share::budget(state, id) {
    let held = crate::vcs_share::held(state, id);
    if held > grant {
      let pressure = slates_vcs::amount_of(held) / slates_vcs::amount_of(grant.max(1));
      given = given.saturating_add(relieve(state, id, held.saturating_sub(grant), pressure));
    }
  }
  if given > 0 {
    state.count(GIVEN_BACK_IDLE, given);
  }
}

/// Whether a task is making repository `id`'s encoding ahead now ([`encode_behind`]).
pub(crate) fn ahead_under_way(state: &ShardState, id: RepositoryId) -> bool {
  state.vcs_stores.encodings.1.contains(&id)
}

/// Turns repository `id`'s encoding ahead, behind a fetch that stopped waiting (A-718 continued, A-856), yielding
/// between turns: while [`ahead_allowed`] allows it, until every version's fragments are kept. Where it stops allowed,
/// counted [`AHEAD_STOPPED`], and its repository has no fetch answered now, what it holds is given back
/// ([`give_back_idle`]). Returns at once where the shard's state is not lent.
async fn encode_ahead(id: RepositoryId) {
  loop {
    let turned = crate::state::with_state_counted(|state| {
      if !ahead_allowed(state, id) {
        state.count(AHEAD_STOPPED, 1);
        if crate::vcs_share::fetches_live(id) == 0 {
          give_back_idle(state, id);
        }
        return Turned::Idle;
      }
      turn(state, id)
    });
    match turned {
      Some(Turned::More) => slates_rt::futures::yield_now().await,
      Some(Turned::Idle) | None => return,
    }
  }
}

/// A fetch's encoding (A-718 continued): repository `id`'s encoding turned a turn at a time, yielding between, for at most
/// [`FETCH_WAIT_NS`]; past it the fetch answers, and a task finishes the encoding behind it ([`encode_behind`]).
pub(crate) async fn encode_for_fetch(id: RepositoryId) {
  encode_within(id, FETCH_WAIT_NS).await;
}

/// [`encode_for_fetch`] with the wait `wait_ns` long: whether the encoding was idle when it returned.
pub(crate) async fn encode_within(id: RepositoryId, wait_ns: u64) -> bool {
  let began = slates_rt::futures::now_ns();
  loop {
    match crate::state::with_state_counted(|state| turn(state, id)) {
      Some(Turned::More) if slates_rt::futures::now_ns().saturating_sub(began) >= wait_ns => {
        let _ = crate::state::with_state_counted(|state| encode_behind(state, id));
        return false;
      }
      Some(Turned::More) => slates_rt::futures::yield_now().await,
      Some(Turned::Idle) | None => return true,
    }
  }
}

/// Starts a task finishing repository `id`'s encoding behind a fetch that stopped waiting, unless one is at it already;
/// counted [`BEHIND`]. The task ends when the encoding is idle, the repository freed or the shard's state gone, or when
/// it may no longer be made ahead ([`encode_ahead`], A-856).
fn encode_behind(state: &mut ShardState, id: RepositoryId) {
  state.count(BEHIND, 1);
  if !state.vcs_stores.encodings.1.insert(id) {
    return;
  }
  let behind = async move {
    encode_ahead(id).await;
    let _ = crate::state::with_state_counted(|state| state.vcs_stores.encodings.1.remove(&id));
  };
  match slates_rt::futures::spawn(behind) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
    }
    // No task to spare: the next fetch turns the encoding on, as before.
    Err(_) => {
      state.vcs_stores.encodings.1.remove(&id);
    }
  }
}

/// The units repository `id`'s encoding under way has done (none when none is under way): a test's sign that its turns
/// progress (`vcs_store::upload::tests`'s judge of a drive).
#[cfg(test)]
pub(crate) fn units_done(state: &ShardState, id: RepositoryId) -> Option<u64> {
  state
    .vcs_stores
    .encodings
    .0
    .get(&id)
    .map(|(encoding, _)| encoding.counters().units)
}

#[cfg(test)]
#[path = "git_encode/tests.rs"]
mod tests;
