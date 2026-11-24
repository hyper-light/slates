//! A repository's store lent to git's upload-pack from its owner shard's map (A-682; A-625 part 3's [`StoreLender`] over
//! A-662's map; §4.17.2 "A fetch" and "Transports" items 1 and 2; §4.17.1 "Policy").
//!
//! **What is lent.** upload-pack (`slates_git::upload`, A-640) answers from a [`Served`]: A-614's store, the content it
//! is kept in, the refs a client may see and the branch `HEAD` names. [`ShardStores`] lends them from the shard's map
//! ([`super::VcsStores`]) afresh for each step of an answer, so a shard's state is never held across a wait (A-640 part
//! 2). The content store is lent the room the shard's budget admits for the step's reads and set back after it, as every
//! read of the map is (`super::lent_to_read`): upload-pack lists a tree, a tag or a commit within that room. A store the
//! map does not hold here (another shard's, one a recovery could not make again, another tool's) is not lent, and each
//! edge answers as it answers a service not served.
//!
//! **The refs, read by the asker's rights.** The step that answers a request ([`StoreLender::lend`]) reads the
//! repository's names from the partition's register as it runs. Each name binds a green, which is a git ref (§4.17.1's
//! terms: "each ref (`refs/heads/*`, `refs/tags/*` and every other ref)"), by git's push's rule (`crate::git_push`'s
//! `ref_of`, A-654), so both services see one ref a name: a short name `N` is `refs/heads/N`, a name under `refs/` is
//! itself, and a name beginning `refs/heads/` is no ref, since its ref is another name's, so names and refs map one to
//! one. A name that makes no ref git can carry (git's `check_refname_format`: a space, a `..`, a control byte) is no
//! ref either, since protocol v2's lines refuse one and a single such name would refuse every `ls-refs`. A ref is lent
//! only to a reader the repository's policy lets read it, hidden as git's push hides one from its pusher
//! (`crate::git_push`'s `PolicyHidden`: A-622's `Read`, git's `transfer.hideRefs` precedence, a ref no rule speaks to
//! the owner's alone), so a hidden ref is neither advertised nor wanted by name (`want-ref`, `deepen-not`). `HEAD` names
//! the repository's default branch by the same rule, and reads as unborn to a reader that may not read that branch,
//! since its target is looked up among the refs lent. The refs are lent in name order, as upload-pack reads them.
//!
//! **A pack's steps read no refs.** After the request's step, an answer's steps hand the store to the pack's source
//! alone, which names what it sends by position (`slates_git::upload::Answer::write`), so they are lent the objects
//! alone ([`StoreLender::lend_objects`]): a step costs its room and one packet whatever the repository's refs (A-640's
//! bound), never a walk of the register.
//!
//! **The pack** ([`ShardPacks`]) is A-639's stream from the store's fragments (`slates_git::upload::Fragments`, A-653),
//! each turn on the shard's clock (§4.3), within the step quantum the shard keeps now (`DaemonConfig::step_quantum_ns`),
//! at the owner's exchange ([`exchange`]).
//!
//! **Owed** (GAPS' §4.17 entry). The objects a hidden ref alone reaches stay fetchable by their ids: slates-git's
//! negotiation answers a want of any object the store holds (`negotiation::Refusal::NotOurs` names one it lacks), so
//! hiding them is slates-git's hidden refs, owed there. A repository with no default branch, or one whose default
//! branch is no ref, is not lent, since slates-git's answers always name `HEAD`'s branch (no verb records none today;
//! `Op::RepositoryDefaultSet` can). The exchange is §4.11's at neutral until the live memory-pressure and load signals
//! move it, as the codec policy's are.

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord};
use slates_git::Admission;
use slates_git::receive::HiddenRefs;
use slates_git::store::learn::{Clock, Exchange};
use slates_git::store::{NamedRef, UploadCounters};
use slates_git::upload::{Fragments, PackPlan, PackRefused, PackSource, Served, Streamed};
use slates_vcs::Work;

use crate::git_push::{PolicyHidden, ref_of};
use crate::state::ShardState;
use crate::vcs_http::git::StoreLender;

// Its clone over protocol v2 serves the made commits' tests too (`super::commits`, A-664 part 3).
#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
std::thread_local! {
  /// Tests only (A-855): the rooms [`ShardStores::room`] answers on this shard's thread before the shard's own, first
  /// first, so a test makes a fetch's room short and then grow as other works ending would.
  pub(crate) static ROOMS: std::cell::RefCell<std::collections::VecDeque<u64>> =
    const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

/// The lender of this shard's stores to upload-pack (the module doc): the HTTP edge's, and the SSH site's once the
/// daemon starts its edge. It holds nothing; each step borrows the shard's state for its own length.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ShardStores;

impl StoreLender for ShardStores {
  type Packs = ShardPacks;

  fn holds(&self, id: RepositoryId) -> bool {
    crate::state::with_state_counted(|state| lendable(state, id).is_some()).unwrap_or(false)
  }

  /// Derived: the room repository `id`'s works are admitted on this shard (`crate::vcs_share::room`, A-724), as a
  /// push's.
  fn room(&mut self, id: RepositoryId) -> Option<u64> {
    #[cfg(test)]
    if let Some(room) = ROOMS.with(|rooms| rooms.borrow_mut().pop_front()) {
      return Some(room);
    }
    crate::state::with_state_counted(|state| crate::vcs_share::room(state, id).bytes)
  }

  fn lend<R>(
    &mut self,
    (id, reader): (RepositoryId, &Principal),
    step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
  ) -> Option<R> {
    crate::state::with_state_counted(|state| lend_on(state, (id, Some(reader)), step)).flatten()
  }

  fn lend_objects<R>(
    &mut self,
    id: RepositoryId,
    step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
  ) -> Option<R> {
    crate::state::with_state_counted(|state| lend_on(state, (id, None), step)).flatten()
  }

  type Hold = GitGeneration;

  fn hold(&mut self, id: RepositoryId, reader: &Principal) -> Option<GitGeneration> {
    crate::state::with_state_counted(|state| hold_in(state, id, reader)).flatten()
  }

  fn lend_held<R>(
    &mut self,
    hold: &GitGeneration,
    step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
  ) -> Option<R> {
    crate::state::with_state_counted(|state| lend_held_in(state, hold, step)).flatten()
  }

  fn wrote(&mut self, hold: &GitGeneration, bytes: u64) {
    hold.written.set(hold.written.get().saturating_add(bytes));
    if bytes > 0 {
      let _ = crate::state::with_state_counted(|state| state.count(WRITTEN, bytes));
    }
  }

  fn pack_reads(pack: &Streamed) -> crate::vcs_http::git::upload::PackReads {
    let (next, stopped) = pack.pack_reads();
    crate::vcs_http::git::upload::PackReads { next, stopped }
  }

  async fn read_ahead(
    &mut self,
    reads: crate::vcs_http::git::upload::PackReads,
    resident: &mut crate::vcs_http::git::upload::Resident,
  ) {
    read_ahead(reads, resident).await;
  }

  async fn bring_back(&mut self, waited: slates_git::store::packed::Waited) -> bool {
    bring_back_within(waited).await.is_ok()
  }

  fn let_go_behind(
    &mut self,
    resident: &mut crate::vcs_http::git::upload::Resident,
    next: Option<slates_git::store::packed::Waited>,
  ) {
    let _ = crate::state::with_state_counted(|state| let_go_behind(state, resident, next));
  }

  fn paced(&mut self, taught: (u64, u64)) {
    let _ = crate::state::with_state_counted(|state| learn_link(state, taught));
  }

  /// A-718: every version of repository `id` encoded as its fragments and kept, a turn at a time, before a fetch's answer
  /// is made, so the answer copies them rather than searching its objects for deltas ([`super::git_encode`]); waited for
  /// at most one liveness window, past which the answer begins and a task finishes the encoding behind it. First (A-614
  /// amended), every version admitted with no walk has its sets derived, a turn at a time ([`super::git_derive`]), waited
  /// for whole: the answer is made from them.
  async fn encode(&mut self, id: RepositoryId) {
    // A-864 piece 2: the answer's own reads are brought back a turn ahead (`read_ahead`); its encoding's packs, where
    // one is due, by the encoding (`git_encode::encode_within`).
    // A-614 amended: the sets of versions admitted with no walk derived first, which the answer and the encoding read.
    super::git_derive::derive_for_read(id).await;
    super::git_encode::encode_for_fetch(id).await;
  }
}

/// Counter: a let-go window of a git repository's pack that could not be brought back before a fetch or an encoding
/// read it (goal 93 item 2a; A-864 piece 2's look-ahead): its read then refuses typed, as an unreadable object's does.
/// Format: a counter name.
pub(crate) const RESIDENT_REFUSED: &str = "vcs_store.git_resident.refused";

/// The let-go windows of `packs` (each a pack and its length) in `content`, each as the read that would meet it waits
/// for it: one an away window, none for a pack held whole.
pub(crate) fn away_in(
  content: &slates_vcs::ContentStore,
  packs: impl Iterator<Item = (slates_vcs::ContentId, u64)>,
) -> Vec<slates_git::store::packed::Waited> {
  let window = to_u64(content.window()).max(1);
  packs
    .flat_map(|(pack, len)| {
      (0..len.div_ceil(window))
        .map(move |index| index.saturating_mul(window))
        .filter(|offset| content.away_window(&pack, len, *offset).is_some())
        .map(move |offset| slates_git::store::packed::Waited { pack, len, offset })
        .collect::<Vec<_>>()
    })
    .collect()
}

/// Brings back each of `away`'s windows from the node's store: the bytes put back; each one refused counted
/// [`RESIDENT_REFUSED`].
pub(crate) async fn brought_back_all(away: Vec<slates_git::store::packed::Waited>) -> u64 {
  let mut brought = 0_u64;
  for waited in away {
    match bring_back_within(waited).await {
      Ok(bytes) => brought = brought.saturating_add(bytes),
      Err(_) => {
        let _ = crate::state::with_state_counted(|state| state.count(RESIDENT_REFUSED, 1));
      }
    }
  }
  brought
}

/// Format: the windows a fetch's look-ahead brings back ahead of its next turn (A-864 piece 2): the one it begins to read
/// in and the next, so a turn reading on into the next window finds it held; one reading past them stops cleanly there.
const LOOK_AHEAD_WINDOWS: u64 = 2;

/// Counter: windows a fetch's look-ahead brought back ahead of its turn (A-864 piece 2). Format: a counter name.
pub(crate) const AHEAD_BROUGHT: &str = "vcs_store.git_resident.ahead_brought";
/// Counter: windows a fetch's turn stopped at, let go, and brought back before its next (A-864 piece 2). Format: a
/// counter name.
pub(crate) const STOPPED_BROUGHT: &str = "vcs_store.git_resident.stopped_brought";

/// A fetch's look-ahead (A-864 piece 2; main 01a20182's ruling of 2026-11-22): before its next turn, off the turn, the
/// let-go window its last turn stopped at, and those of the [`LOOK_AHEAD_WINDOWS`] its next turn reads first, brought
/// back from the node's store; the turn then reads on without meeting one, and one reading past them stops cleanly at a
/// unit's boundary, nothing of it written (`slates_git::store::Upload::waiting`).
pub(crate) async fn read_ahead(
  reads: crate::vcs_http::git::upload::PackReads,
  resident: &mut crate::vcs_http::git::upload::Resident,
) {
  // The window a read stopped at is pinned to the object under way (`Brought::pinned`): its read may need it wherever
  // it lies.
  if let Some(stopped) = reads.stopped
    && brought_one(stopped, resident, reads.next).await
  {
    let _ = crate::state::with_state_counted(|state| state.count(STOPPED_BROUGHT, 1));
  }
  let Some(next) = reads.next else {
    note_peak(resident);
    return;
  };
  let away = crate::state::with_state(|state| {
    let content = state.vcs_stores.content()?;
    let window = to_u64(content.window()).max(1);
    let first = next.offset.checked_div(window).unwrap_or(0);
    Some(
      (first..first.saturating_add(LOOK_AHEAD_WINDOWS))
        .map(|index| index.saturating_mul(window).max(next.offset))
        .filter(|offset| *offset < next.len)
        .filter(|offset| content.away_window(&next.pack, next.len, *offset).is_some())
        .map(|offset| slates_git::store::packed::Waited { offset, ..next })
        .collect::<Vec<_>>(),
    )
  })
  .flatten()
  .unwrap_or_default();
  let mut windows = 0_u64;
  for waited in away {
    if brought_one(waited, resident, None).await {
      windows = windows.saturating_add(1);
    }
  }
  if windows > 0 {
    let _ = crate::state::with_state_counted(|state| state.count(AHEAD_BROUGHT, windows));
  }
  note_peak(resident);
}

/// Format: the most windows one fetch's answer held brought back at once (A-864 piece 3: a turn's look-ahead and the
/// window it stopped at, never its pack's). Format: a counter name, a peak, not a sum.
pub(crate) const PEAK_WINDOWS: &str = "vcs_store.git_resident.peak_windows";
/// Counter: windows a fetch's read stopped at, held past a let-go behind its cursor because the stream still reads the
/// object they were brought back for (`Brought::pinned`; goal 93, r130). Format: a counter name.
pub(crate) const PINNED_HELD: &str = "vcs_store.git_resident.pinned_held";
/// Counter: windows a fetch brought back and let go again once its cursor passed them (A-864 piece 3). Format: a counter
/// name.
pub(crate) const LET_GO_BEHIND: &str = "vcs_store.git_resident.let_go_behind";

/// Brings back `waited`'s let-go window for an answer, kept in its `resident` windows to be let go again behind its
/// cursor: whether it was brought back.
async fn brought_one(
  waited: slates_git::store::packed::Waited,
  resident: &mut crate::vcs_http::git::upload::Resident,
  pinned: Option<slates_git::store::packed::Waited>,
) -> bool {
  let window = crate::state::with_state(|state| {
    state
      .vcs_stores
      .content()?
      .away_window(&waited.pack, waited.len, waited.offset)
  })
  .flatten();
  let Some((window, start, len)) = window else {
    return false;
  };
  match bring_back_within(waited).await {
    Ok(bytes) if bytes > 0 => {
      resident.push(crate::vcs_http::git::upload::Brought {
        pack: waited.pack,
        end: start.saturating_add(len),
        window,
        pinned,
      });
      true
    }
    Ok(_) => false,
    Err(_) => {
      let _ = crate::state::with_state_counted(|state| state.count(RESIDENT_REFUSED, 1));
      false
    }
  }
}

/// Counts the most windows `resident` holds ([`PEAK_WINDOWS`]).
fn note_peak(resident: &crate::vcs_http::git::upload::Resident) {
  let held = to_u64(resident.len());
  let _ = crate::state::with_state_counted(|state| state.count_max(PEAK_WINDOWS, held));
}

/// Lets go again the windows of `resident` an answer's cursor has passed (A-864 piece 3): those of the pack `next` reads
/// that end at or before where it reads, and every one of another pack (a pack is read front to back, each in turn);
/// every one with no `next`. A window a home holds here is kept (`vcs_store::let_go_read`'s `Pinned`); one met again
/// later is brought back again, its stop and look-ahead as before.
pub(crate) fn let_go_behind(
  state: &mut ShardState,
  resident: &mut crate::vcs_http::git::upload::Resident,
  next: Option<slates_git::store::packed::Waited>,
) {
  resident.retain(|brought| {
    // A window a read stopped at stays while the stream still reads the object it stopped for.
    let pinned = next.is_some() && brought.pinned == next;
    let behind =
      !pinned && next.is_none_or(|next| next.pack != brought.pack || brought.end <= next.offset);
    if pinned {
      state.count(PINNED_HELD, 1);
    }
    if behind && super::let_go_read(state, &brought.window).is_ok() {
      state.count(LET_GO_BEHIND, 1);
    }
    !behind
  });
}

/// Counter: a let-go window's bring-back that did not end within its patience, given up (A-864: main 01a20182's r29 and
/// c9bb36dc5 runs stalled a mirror clone at 183,511 bytes; a bring-back awaits the node's store, which may not answer).
/// Format: a counter name.
pub(crate) const BRING_BACK_PAST_PATIENCE: &str = "vcs_store.git_resident.bring_back_past_patience";

/// `read_through::bring_back(waited)`, given up once it has run past the patience every wait for room keeps
/// (`vcs_share::wait::PATIENCE_NS`), counted [`BRING_BACK_PAST_PATIENCE`]: a read that waits for it then refuses
/// typed, never a fetch that waits forever.
pub(crate) async fn bring_back_within(
  waited: slates_git::store::packed::Waited,
) -> Result<u64, super::read_through::ReadThroughRefused> {
  match slates_rt::futures::within(
    crate::vcs_share::wait::PATIENCE_NS,
    super::read_through::bring_back(waited),
  )
  .await
  {
    Ok(Some(brought)) => brought,
    Ok(None) | Err(_) => {
      let _ = crate::state::with_state_counted(|state| state.count(BRING_BACK_PAST_PATIENCE, 1));
      Err(super::read_through::ReadThroughRefused::NoStore)
    }
  }
}

/// Brings back the let-go window a read of git repository `id` met and noted (`Repository::waiting`), where one did;
/// whether one did (goal 93 item 2a: what a driver checks after every turn, so the next turn reads it).
pub(crate) async fn brought_back_waited(id: RepositoryId) -> bool {
  // A-864 piece 2: the window an encoding unit stopped at, else one a read met and noted.
  let waited = crate::state::with_state(|state| {
    state.vcs_stores.stopped_encoding(id).or_else(|| {
      state
        .vcs_stores
        .git(id)
        .and_then(|(_, store)| store.repository.waiting())
    })
  })
  .flatten();
  let Some(waited) = waited else {
    return false;
  };
  if bring_back_within(waited).await.is_err() {
    let _ = crate::state::with_state_counted(|state| state.count(RESIDENT_REFUSED, 1));
  }
  true
}

/// `len` as a word.
fn to_u64(len: usize) -> u64 {
  u64::try_from(len).unwrap_or(u64::MAX)
}

/// Runs `step` over repository `id`'s git store on `state`'s shard (the module doc): as upload-pack answers a request of
/// `reader`, with the refs `reader` may read and `HEAD`'s branch; or, with no reader, for an answer's next bytes, no ref
/// read. None when the store is not lendable here ([`lendable`]), or the repository's policy cannot be read for a
/// reader (A-622's guard keeps such rules out: a damaged record is never read without its policy).
pub(crate) fn lend_on<R>(
  state: &mut ShardState,
  (id, reader): (RepositoryId, Option<&Principal>),
  step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
) -> Option<R> {
  let began = slates_rt::futures::now_ns();
  let lent = super::observed(state, id, |state| lend_uncounted(state, (id, reader), step));
  // A-724 piece 1: a request's step (one with a reader) begins a fetch on the repository's usage row; every step that
  // was lent is counted with its time. Its answer is counted, and its bytes learned, as its generation is given back
  // (`GitGeneration`'s drop, A-724 piece 3d).
  if lent.is_some() {
    let now = slates_rt::futures::now_ns();
    if let Some(reader) = reader {
      state.vcs_usage.begun(&id, reader, Work::Fetch, began);
    }
    state
      .vcs_usage
      .stepped(&id, Work::Fetch, (now.saturating_sub(began), 0), now);
    // The step's lookups over the store's own index, reported to its merge rule (`super::index_upkeep`).
    super::index_upkeep::looked_up(state, id, now.saturating_sub(began));
  }
  lent
}

/// [`lend_on`]'s step, uncounted.
fn lend_uncounted<R>(
  state: &mut ShardState,
  (id, reader): (RepositoryId, Option<&Principal>),
  step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
) -> Option<R> {
  let (head, named) = match reader {
    Some(reader) => {
      let (record, head) = lendable(state, id)?;
      (head, readable_refs(state, record, reader)?)
    }
    None => (Vec::new(), Vec::new()),
  };
  let refs: Vec<NamedRef<'_>> = named
    .iter()
    .map(|(name, green)| NamedRef {
      name,
      green: *green,
    })
    .collect();
  let mut packs = ShardPacks::of(state);
  let lent = super::lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
    let (content, store) = stores.git_ref(id)?;
    let served = Served {
      repository: &store.repository,
      store: content,
      refs: &refs,
      head: &head,
    };
    Some(step(&served, &mut packs))
  });
  packs.count_on(state);
  lent
}

/// A fork of a git repository's store as it stood at `stamp` (A-745, §4.17.1 "A fetch's answer" rule 4;
/// `slates_git::store::Repository::fork`), and how many fetches hold it.
#[derive(Debug)]
pub(crate) struct GitFork {
  /// Where the repository stood when it was forked ([`slates_git::store::Repository::stamp`]).
  stamp: (u64, u64),
  /// The fork.
  repository: slates_git::store::Repository,
  /// The fetches holding it.
  holders: u64,
}

/// A fetch's hold on the generation of a git repository it began on (A-745, §4.17.1 "A fetch's answer" rule 4): the fork
/// its shard keeps at the stamp the repository stood at ([`GitFork`], shared by every fetch that began there), and the
/// refs its reader could read and `HEAD`'s branch as they stood, so every step of its answer reads what its first step
/// read whatever lands meanwhile. Given back by its drop, on the shard that keeps the fork; the fork with its last
/// holder.
pub(crate) struct GitGeneration {
  /// The repository.
  id: RepositoryId,
  /// The stamp of the fork it holds.
  stamp: (u64, u64),
  /// The refs its reader could read, by full name and green, in name order.
  refs: Vec<(Vec<u8>, u64)>,
  /// The full name of the branch `HEAD` named.
  head: Vec<u8>,
  /// The fetch counted live while its answer holds this generation (A-724 piece 3d: a fetch's room is a claim of its own).
  _fetching: crate::vcs_share::Live,
  /// The bytes the answer has written so far, as its edge reports them ([`StoreLender::wrote`]): learned as the
  /// repository's fetch demand once the answer ends.
  written: core::cell::Cell<u64>,
}

impl Drop for GitGeneration {
  /// Gives the fork back and answers the fetch on its repository's usage row with the bytes its answer wrote (A-724
  /// piece 3d).
  fn drop(&mut self) {
    let (id, stamp, written) = (self.id, self.stamp, self.written.get());
    crate::state::with_state(|state| {
      release_fork(state, id, stamp);
      let now = slates_rt::futures::now_ns();
      state.vcs_usage.answered(&id, Work::Fetch, written, now);
      // A-856: the repository's last fetch ending (this one is still counted live until its guard drops after this),
      // with no task making its encoding ahead (which gives back as it stops), its derived values past its learned
      // grant are given back.
      if crate::vcs_share::fetches_live(id) <= 1 && !super::git_encode::ahead_under_way(state, id) {
        super::git_encode::give_back_idle(state, id);
      }
    })
    .unwrap_or_default();
  }
}

/// Holds repository `id`'s generation as `state` holds it now for `reader` ([`GitGeneration`]): the fork at the stamp it
/// stands at, made where none is kept; none when it is not lendable here or cannot be forked.
pub(crate) fn hold_in(
  state: &mut ShardState,
  id: RepositoryId,
  reader: &Principal,
) -> Option<GitGeneration> {
  let (record, head) = lendable(state, id)?;
  let refs = readable_refs(state, record, reader)?;
  let stamp = state.vcs_stores.git_ref(id)?.1.repository.stamp();
  let kept = state
    .vcs_stores
    .git_forks
    .get_mut(&id)
    .and_then(|forks| forks.iter_mut().find(|fork| fork.stamp == stamp));
  match kept {
    Some(fork) => fork.holders = fork.holders.saturating_add(1),
    None => {
      // The fork's records charge the shard's budget, as the repository's do, lent for the fork alone.
      let repository = super::within_budget(state, 0, |stores| {
        let (content, store) = stores.git(id)?;
        store.repository.fork(content).ok()
      })?;
      state
        .vcs_stores
        .git_forks
        .entry(id)
        .or_default()
        .push(GitFork {
          stamp,
          repository,
          holders: 1,
        });
    }
  }
  Some(GitGeneration {
    id,
    stamp,
    refs,
    head,
    _fetching: crate::vcs_share::Live::fetch(id),
    written: core::cell::Cell::new(0),
  })
}

/// Runs `step` over the generation `hold` holds, counted as [`lend_on`] counts a step; none when its fork is gone.
fn lend_held_in<R>(
  state: &mut ShardState,
  hold: &GitGeneration,
  step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
) -> Option<R> {
  let began = slates_rt::futures::now_ns();
  let refs: Vec<NamedRef<'_>> = hold
    .refs
    .iter()
    .map(|(name, green)| NamedRef {
      name,
      green: *green,
    })
    .collect();
  let mut packs = ShardPacks::of(state);
  let lent = super::observed(state, hold.id, |state| {
    super::lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
      let fork = stores
        .git_forks
        .get(&hold.id)?
        .iter()
        .find(|fork| fork.stamp == hold.stamp)?;
      let served = Served {
        repository: &fork.repository,
        store: stores.content.as_ref()?,
        refs: &refs,
        head: &hold.head,
      };
      Some(step(&served, &mut packs))
    })
  });
  packs.count_on(state);
  if lent.is_some() {
    let now = slates_rt::futures::now_ns();
    state
      .vcs_usage
      .stepped(&hold.id, Work::Fetch, (now.saturating_sub(began), 0), now);
  }
  lent
}

/// Lets go of one hold on repository `id`'s fork at `stamp`, giving the fork back with its last holder; a refused
/// release counted.
fn release_fork(state: &mut ShardState, id: RepositoryId, stamp: (u64, u64)) {
  let Some(forks) = state.vcs_stores.git_forks.get_mut(&id) else {
    return;
  };
  let Some(at) = forks.iter().position(|fork| fork.stamp == stamp) else {
    return;
  };
  let last = forks.get_mut(at).is_some_and(|fork| {
    fork.holders = fork.holders.saturating_sub(1);
    fork.holders == 0
  });
  if !last {
    return;
  }
  let fork = forks.remove(at);
  if forks.is_empty() {
    state.vcs_stores.git_forks.remove(&id);
  }
  let released = super::within_budget(state, 0, |stores| {
    stores
      .content
      .as_mut()
      .map_or(Ok(()), |content| fork.repository.release(content))
  });
  if released.is_err() {
    state.count(FORK_RELEASE_REFUSED, 1);
  }
}

/// Format: the count of a git fetch's fork its content store refused to take back (A-745 rule 4).
const FORK_RELEASE_REFUSED: &str = "vcs_store.git_fork_release";

/// Repository `id`'s record and the full name of the branch its `HEAD` names, when its git store is held on `state`'s
/// shard and can be lent (the module doc): none for a store not held here, one of another tool, or a repository with no
/// default branch or one whose default branch is no ref.
fn lendable(state: &ShardState, id: RepositoryId) -> Option<(&RepositoryRecord, Vec<u8>)> {
  state.vcs_stores.git_ref(id)?;
  let record = state.db.partition().repositories().repository(id)?;
  let head = ref_of(record.default_branch.as_deref()?)?;
  Some((record, head))
}

/// The refs of `record`'s repository that `reader` may read (the module doc): each ref's full name and its green's key
/// in the store, in name order, one ref a name; none when the repository's policy cannot be read.
fn readable_refs(
  state: &ShardState,
  record: &RepositoryRecord,
  reader: &Principal,
) -> Option<Vec<(Vec<u8>, u64)>> {
  let hidden = PolicyHidden::of(record, reader).ok()?;
  let mut refs: Vec<(Vec<u8>, u64)> = state
    .db
    .partition()
    .repositories()
    .branches(record.id, "")
    .filter_map(|(name, green)| Some((ref_of(name)?, super::green_key(green))))
    .filter(|(name, _)| !hidden.is_hidden(name))
    .collect();
  refs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
  Some(refs)
}

/// Derived: the owner's exchange, what a byte the fragments' encoder saves is worth in its time (§4.11's value of a byte
/// over its value of CPU; A-859 part A, main 01a20182's rulings of 2026-11-22 04:30 and 06:32): the price of a byte on
/// the links this shard's fetches are sent over, learned from the time their writes waited for the peer
/// ([`LinkPrice`]); until it is learned, the price at which deflating at git's default level just pays on this machine
/// (`Exchange::cold`, from `DaemonConfig::git_rates`). Never the time a copy in memory takes: a fetched byte crosses a
/// link, and pricing it at memcpy's 0.14 ns sent git/git's clone uncompressed.
pub(super) fn exchange(state: &ShardState) -> Exchange {
  state
    .vcs_stores
    .git_link
    .exchange(state.config.git_rates)
    .unwrap_or_else(|| Exchange::cold(state.config.git_rates))
}

/// Format: the bytes sent past a link's first wait before its price is learned (A-859 part A): a MiB, past a socket
/// send buffer's few hundred KiB (macOS's 128 KiB default, Linux's autotuned up to 4 MiB), so the price rests on
/// writes the peer paced rather than on a few that happened to meet a full buffer.
const LINK_LEARNED_FROM: u64 = 1 << 20;

/// Format: the bytes a link's price remembers before both its sums are halved (A-859 part A): 64 MiB, a few clones of
/// a mid-sized repository, so the price follows the links the shard sends over now and an old one fades.
const LINK_MEMORY: u64 = 64 << 20;

/// Counter: the bytes git fetches' answers wrote, counted as each step writes them, so a clone cut short says what it
/// was sent (main 01a20182's r17, 2026-11-22 07:08). Format: a counter name.
pub(crate) const WRITTEN: &str = "vcs_store.git_upload.written_bytes";

/// Counter: bytes a git fetch sent past its link's first wait, which the shard's link price was learned from (A-859
/// part A; its non-vacuity: the price moved). Format: a counter name.
pub(crate) const LINK_LEARNED: &str = "vcs_store.git_link.learned_bytes";

/// The price of a byte on this shard's links (A-859 part A, main 01a20182's ruling of 2026-11-22 06:32): the
/// nanoseconds the edges' writes of git's fetch answers waited for the peer to take their bytes, over those bytes, each
/// answer's counted from its first write that found the socket full ([`Sent::waited`]), since writes into a buffer with
/// room take no time whatever the link and would read a slow link as free. Time slates spends making the answer is not
/// in it, so a clone slow for its encoding never prices its link dearer (the loop a whole answer's wall time would make).
/// Both sums are halved past [`LINK_MEMORY`], so the price is the recent links'.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LinkPrice {
  /// The nanoseconds the writes waited.
  ns: u64,
  /// The bytes they wrote.
  bytes: u64,
}

impl LinkPrice {
  /// Learns a write of `bytes` that took `ns` to be taken, past its answer's first wait.
  pub(crate) fn learn(&mut self, (ns, bytes): (u64, u64)) {
    self.ns = self.ns.saturating_add(ns);
    self.bytes = self.bytes.saturating_add(bytes);
    while self.bytes > LINK_MEMORY {
      self.ns /= 2;
      self.bytes /= 2;
    }
  }

  /// The exchange it prices a byte at, with the machine's `rates`: none until [`LINK_LEARNED_FROM`] bytes have been
  /// learned.
  pub(crate) fn exchange(&self, rates: slates_git::store::learn::Rates) -> Option<Exchange> {
    (self.bytes >= LINK_LEARNED_FROM).then_some(Exchange {
      ns: self.ns,
      bytes: self.bytes,
      rates,
    })
  }
}

/// One write of a fetch's answer to its peer (A-859 part A), as its edge saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sent {
  /// The bytes written.
  pub(crate) bytes: u64,
  /// The nanoseconds the write took, to the peer taking its last byte.
  pub(crate) ns: u64,
  /// Whether it found the socket full and waited for the peer.
  pub(crate) waited: bool,
}

/// What a fetch's answer has learned of its link (A-859 part A): nothing until a write waits for the peer; from it on,
/// each write's bytes and time, learned into the shard's [`LinkPrice`] as they are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pacing {
  /// Whether a write has waited.
  waited: bool,
}

impl Pacing {
  /// What `sent` teaches the shard's price: its bytes and time once this or an earlier write has waited; none before.
  pub(crate) fn sent(&mut self, sent: Sent) -> Option<(u64, u64)> {
    self.waited |= sent.waited;
    self.waited.then_some((sent.ns, sent.bytes))
  }
}

/// Learns `taught` into `state`'s link price, counted [`LINK_LEARNED`].
pub(crate) fn learn_link(state: &mut ShardState, taught: (u64, u64)) {
  state.vcs_stores.git_link.learn(taught);
  state.count(LINK_LEARNED, taught.1);
}

/// A counter of a fetch's stream's paths ([`STREAM_COUNTED`]): its name and the stream's count it adds.
pub(crate) type StreamCount = (&'static str, fn(&UploadCounters) -> u64);

/// Counters: the paths a fetch's stream sent its objects by (`slates_git::store::UploadCounters`, A-859; main 01a20182's
/// order of 2026-11-22 04:53), counted on its shard as each step of its answer writes, so the daemon's status says which
/// path a clone's objects took, a clone cut short too. Format: each a counter name and the stream's count it adds.
pub(crate) const STREAM_COUNTED: [StreamCount; 18] = [
  ("vcs_store.git_upload.parts_copied", |c| c.parts_copied),
  ("vcs_store.git_upload.parts_by_entry", |c| c.parts_by_entry),
  ("vcs_store.git_upload.entries_judged", |c| c.entries_judged),
  ("vcs_store.git_upload.entries_copied", |c| c.entries_copied),
  ("vcs_store.git_upload.headers_rewritten", |c| {
    c.headers_rewritten
  }),
  ("vcs_store.git_upload.made_whole", |c| c.made_whole),
  ("vcs_store.git_upload.loose_whole", |c| c.loose_whole),
  ("vcs_store.git_upload.loose_deltas", |c| c.loose_deltas),
  ("vcs_store.git_upload.loose_unsearched", |c| {
    c.loose_unsearched
  }),
  ("vcs_store.git_upload.reused_whole", |c| c.reused_whole),
  ("vcs_store.git_upload.reused_deltas", |c| c.reused_deltas),
  ("vcs_store.git_upload.reuse_refused", |c| c.reuse_refused),
  ("vcs_store.git_upload.reuse_no_entry", |c| c.reuse_no_entry),
  ("vcs_store.git_upload.reuse_unread", |c| c.reuse_unread),
  ("vcs_store.git_upload.reuse_crc_missed", |c| {
    c.reuse_crc_missed
  }),
  ("vcs_store.git_upload.reuse_rebuilt", |c| c.reuse_rebuilt),
  ("vcs_store.git_upload.backward_reads", |c| c.backward_reads),
  ("vcs_store.git_upload.stops", |c| c.stops),
];

/// Counter: deflate states the shard's thread made, none kept to take (A-868, `slates_git::store::deflaters`): O(the
/// deflates under way at once on the shard), never O(objects × trials). Format: a counter name.
pub(crate) const DEFLATE_STATES_MADE: &str = "vcs_store.git_deflate.states_made";
/// Counter: deflate states taken again from those the shard's thread keeps (A-868). Format: a counter name.
pub(crate) const DEFLATE_STATES_REUSED: &str = "vcs_store.git_deflate.states_reused";
/// Counter: deflate states dropped, given back past what a thread keeps or let go idle (A-868). Format: a counter name.
pub(crate) const DEFLATE_STATES_DROPPED: &str = "vcs_store.git_deflate.states_dropped";

/// Counts on `state` the deflate states its thread made, took again and dropped since this was last called (A-868):
/// the git crate counts them on the thread, a shard's.
pub(crate) fn count_deflaters(state: &mut ShardState) {
  let counts = slates_git::store::deflaters::take_counts();
  for (name, by) in [
    (DEFLATE_STATES_MADE, counts.made),
    (DEFLATE_STATES_REUSED, counts.reused),
    (DEFLATE_STATES_DROPPED, counts.dropped),
  ] {
    if by > 0 {
      state.count(name, by);
    }
  }
}

/// A-639's stream as upload-pack's pack source on the owner shard (the module doc): each turn on the shard's clock, at
/// the owner's exchange, within the step quantum the shard keeps when the step was lent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShardPacks {
  /// What a saved byte is worth in time.
  exchange: Exchange,
  /// The nanoseconds a turn may spend.
  quantum_ns: u64,
  /// The stream's counters before this step's first write and after its last, for the shard to count what the step's
  /// writes added once it ends ([`STREAM_COUNTED`]); none where the step wrote none.
  counted: Option<(UploadCounters, UploadCounters)>,
}

impl ShardPacks {
  /// The source on `state`'s shard, at its link's price and its step quantum now.
  fn of(state: &ShardState) -> ShardPacks {
    ShardPacks {
      exchange: exchange(state),
      quantum_ns: state.config.step_quantum_ns(),
      counted: None,
    }
  }

  /// Counts on `state` what the stream's writes counted in this step ([`STREAM_COUNTED`]), and the deflate states its
  /// shard thread made and took again ([`count_deflaters`]).
  fn count_on(&self, state: &mut ShardState) {
    count_deflaters(state);
    let Some((before, after)) = self.counted else {
      return;
    };
    for (name, of) in STREAM_COUNTED {
      let by = of(&after).saturating_sub(of(&before));
      if by > 0 {
        state.count(name, by);
      }
    }
  }

  /// A-639's stream's source for one call, its turns on `clock`.
  fn fragments<'c>(&self, clock: &'c mut ShardClock) -> Fragments<'c> {
    Fragments {
      clock,
      exchange: self.exchange,
      quantum_ns: self.quantum_ns,
    }
  }
}

impl PackSource for ShardPacks {
  type Pack = Streamed;

  /// The shard's step quantum, begun now, on the shard's clock (§4.3; A-745): a fetch's choice of its pack's objects
  /// stops between units once it is spent.
  fn turn(&self) -> impl FnMut() -> bool + 'static {
    let began = slates_rt::futures::now_ns();
    let quantum = self.quantum_ns;
    move || slates_rt::futures::now_ns().saturating_sub(began) >= quantum
  }

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<Streamed, PackRefused> {
    self
      .fragments(&mut ShardClock)
      .begin(served, plan, admission)
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut Streamed,
    admission: &mut Admission,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    let before = self
      .counted
      .map_or_else(|| pack.counters(), |(before, _)| before);
    let written = self
      .fragments(&mut ShardClock)
      .write(served, pack, admission, out, room);
    self.counted = Some((before, pack.counters()));
    written
  }
}

/// The shard's clock (§4.3), which a turn's quantum is spent on.
pub(super) struct ShardClock;

impl Clock for ShardClock {
  fn now_ns(&mut self) -> u64 {
    slates_rt::futures::now_ns()
  }
}
