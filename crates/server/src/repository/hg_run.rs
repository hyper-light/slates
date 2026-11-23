//! A Mercurial push landed as a run of segment changesets (main 01a20182's ruling (b1) of 2026-11-22 04:44), its entry
//! committed under its repository's landing turn (rulings (i) and of 07:32 and 12:00; the turn `svn_commit::landing`
//! keeps, which Mercurial's made changesets hold too).
//!
//! **Why.** A push held every revision's kept text until its one changeset landed after its intake, so a whole-history
//! push's bookkeeping grew with the history until it passed what its repository could ever be lent (main's measure of
//! 2026-11-22 04:59: 1.37 GB held on a 16 GB runner). It lands as the import does: its intake cuts its kept texts into
//! segments of at least [`segment_bytes`] of its changegroup (`Intake::cutting`), and each lands as it is cut, between
//! the intake's slices, as an ordinary bounded changeset: its `Record` the segment's rows by node and identity (record
//! v10, `intake::segment_record`), its `Payload` its slice of the changegroup, its `Texts` each revision's text, base and
//! place in that slice. What the segment's texts charged the push's admission is given back once it lands.
//!
//! **Apart from the turn.** A segment lands at its repository's next sequence, which its owner shard allocates
//! (`VcsStores::next_changeset`): the shard runs one step at a time, so sequences are serialized with no lock, and other
//! landings (another push's segments, its entry, a made changeset) may take the sequences between a push's. The push's
//! entry names its segments' sequences as ranges (record v11, `record::write_push_runs_reference`, at most one range a
//! segment); a replay reads exactly those, each record's rows by node over the index as it stands then, so a segment
//! cut before another push's entry replays where that push left the index. Readers see only what an entry names: the
//! served view is the index the entries made, and a landed segment's texts are reached only through it. The push takes
//! its repository's landing turn only for its entry's commit (`hg_push`'s landing), so a small push waits for another's
//! commit, never for its segments.
//!
//! **Let go.** Once a segment is durable on the node's changeset store, which serves its texts from then on, its file
//! revisions' windows leave the shard's content store ([`let_go_landed`]), their entries kept, so the push's fork and
//! the index it becomes still name them by identity, and a read that meets one brings it back from the store (the
//! intake's base reads, the works' read-through, the served view's). Until its entry commits, a window a home holds stays,
//! as a landed git pack's does (`vcs_store::landed_pin`): a home reads by identity alone, and the revision is not yet in
//! the index its read-through reads. Once it commits, each window a home holds (one the shard holds for the region) is
//! noted with its revision for the home (`homes::note_landed`, `Landed::Hg`; goal 93 item 2b(b), w-shared's seam 1) and
//! let go, pinned by nothing: a home's read brings it back by its path and node (`hg_push::bring_back`). A window no home
//! holds needs no note, since only the index reads it (w-shared's 1ecb57cf4), so a history notes about none. That pass is a
//! whole push's file revisions, so the
//! push's task takes it a turn at a time within half the shard's step quantum, a revision its unit ([`let_go_committed`];
//! in one poll it spun CI's x86 shard 0 for 1,045 ms, run 45110563116), as the import's answer does once its landing is
//! durable. So a whole-history push leaves its rows held, never
//! its history's texts (main 01a20182's order of 2026-11-22 14:00: 4.7 GB of arena held after Mercurial's history
//! landed; 150,880 texts pinned on shard 0 after the first let-go, CI 45101827617).
//!
//! **Abandoned.** A run no entry names (the push refused, its connection ended, its task cancelled) is forgotten as it
//! ends: each of its sequences' rows dropped from the store (`StoreHandle::forget`, counted), its log bytes the owed
//! cleaner of unnamed log space's. So a client that keeps starting and dropping long pushes grows no rows. After a
//! crash a recovery applies only the sequences the register names, and the next landing takes the last named plus one,
//! its row the newest landing there.

use slates_db::catalog::RepositoryId;
use slates_hg::filerev::FileContents;
use slates_hg::import::record;
use slates_hg::push::intake::{KeptText, PushSegment};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::owner::{Part, StoreHandle};
use slates_vcs::ContentStore;

use crate::repository::svn_commit::landing::{self, Turn};
use crate::state::ShardState;

/// Derived: how long a push waits for its repository's landing turn before it is refused, the patience a push waits
/// for room (`vcs_share::wait::PATIENCE_NS`): a push parked past it would be parked past a client's patience too.
pub(crate) const PATIENCE_NS: u64 = crate::vcs_share::wait::PATIENCE_NS;

/// Counter: a segment of a push's changegroup landed on the changeset store as a changeset. Format: a counter name in
/// the status report.
pub(crate) const SEGMENTS_LANDED: &str = "vcs.hg.push_segments_landed";
/// Counter: a segment the store refused to land, its push refused. Format: a counter name.
pub(crate) const SEGMENTS_UNLANDED: &str = "vcs.hg.push_segments_unlanded";
/// Counter: a push's run of segments no entry named, forgotten. Format: a counter name.
pub(crate) const RUNS_ABANDONED: &str = "vcs.hg.push_runs_abandoned";
/// Counter: the store rows an abandoned run's forgetting dropped or moved (`StoreHandle::forget`). Format: a counter
/// name.
pub(crate) const RUN_ROWS_FORGOTTEN: &str = "vcs.hg.push_run_rows_forgotten";
/// Counter: an abandoned run's sequence the store refused to forget, its rows left to the cleaner. Format: a counter
/// name.
pub(crate) const RUN_ROWS_UNFORGOTTEN: &str = "vcs.hg.push_run_rows_unforgotten";
/// Counter: a push that found its repository's landing turn held and parked for it, counted as it parks. Format: a
/// counter name.
pub(crate) const TURN_WAITS: &str = "vcs.hg.push_turn_waits";
/// Counter: the nanoseconds pushes parked for their repository's landing turn, summed (main's ruling (i): the wait a
/// small push takes behind a whole-history push). Format: a counter name.
pub(crate) const TURN_WAITED_NS: &str = "vcs.hg.push_turn_waited_ns";
/// Counter: a push refused for waiting past [`PATIENCE_NS`] for its repository's landing turn. Format: a counter name.
pub(crate) const TURN_GIVEN_UP: &str = "vcs.hg.push_turn_given_up";
/// Counter: a push's fork re-based before its entry, another landing having replaced the index it forked. Format: a
/// counter name.
pub(crate) const REBASED: &str = "vcs.hg.push_rebased";
/// Counter: a re-base taken holding the landing turn, the fork stale again after one re-base off it (main's bound: never
/// an unbounded retry). Format: a counter name.
pub(crate) const REBASED_IN_TURN: &str = "vcs.hg.push_rebased_in_turn";

/// Format: slates' words for a push that waited past [`PATIENCE_NS`] for its repository's landing turn: another
/// landing (a long push's run of segments) held it; nothing of the push is kept, and a retry may pass.
pub(crate) const TURN_PAST_PATIENCE: &[u8] =
  b"push refused: another landing on this repository held its turn past the server's patience; push again";

#[cfg(test)]
std::thread_local! {
  /// A test's segment bound on this shard's thread, in place of [`segment_bytes`]'s derivation where more than none: a
  /// test's push of a few hundred kilobytes cut into several segments.
  pub(crate) static TEST_SEGMENT_BYTES: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
  /// A test's budget for a turn of a committed run's let-go on this shard's thread, in place of [`let_go_turn_ns`]'s
  /// derivation where more than none: a test's push of a hundred files let go over a turn each.
  pub(crate) static TEST_LET_GO_TURN_NS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
  /// A test's patience for a landing turn on this shard's thread, in place of [`PATIENCE_NS`] where more than none: a
  /// test's push refused past it in milliseconds.
  pub(crate) static TEST_TURN_PATIENCE_NS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
  /// A test's crash point on this shard's thread where more than none: a push parks once its run has landed this many
  /// segments, as a push the daemon's crash stops there, until [`TEST_RELEASE`] is set.
  pub(crate) static TEST_PARK_AFTER_SEGMENTS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
  /// Set by a test on this shard's thread ([`release_parked`]): a push parked at [`TEST_PARK_AFTER_SEGMENTS`] goes on.
  pub(crate) static TEST_RELEASE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
  /// The parked push's task, woken when the test releases it ([`release_parked`]).
  static TEST_PARKED: core::cell::RefCell<Option<core::task::Waker>> = const { core::cell::RefCell::new(None) };
  /// Set by a test on this shard's thread: once, after a push's fork is re-based off the landing turn, its repository's
  /// index is counted changed, as another push's landing between the re-base and the turn would change it.
  pub(crate) static TEST_STALE_AFTER_REBASE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Parks where a test's crash point is `run`'s segments landed so far ([`TEST_PARK_AFTER_SEGMENTS`]), until the test
/// releases it ([`TEST_RELEASE`]).
#[cfg(test)]
pub(crate) async fn parked_for_a_crash(run: &Run) {
  let at = TEST_PARK_AFTER_SEGMENTS.with(core::cell::Cell::get);
  if at > 0 && run.segments() == at {
    // Woken by the release itself, no timed poll (R3): the task keeps its waker where the release finds it.
    core::future::poll_fn(|context| {
      if TEST_RELEASE.with(core::cell::Cell::get) {
        return core::task::Poll::Ready(());
      }
      TEST_PARKED.with(|parked| *parked.borrow_mut() = Some(context.waker().clone()));
      core::task::Poll::Pending
    })
    .await;
  }
}

/// Releases a push parked at a test's crash point on this shard's thread ([`parked_for_a_crash`]): its flag set and its
/// task woken.
#[cfg(test)]
pub(crate) fn release_parked() {
  TEST_RELEASE.with(|release| release.set(true));
  if let Some(waker) = TEST_PARKED.with(|parked| parked.borrow_mut().take()) {
    waker.wake();
  }
}

/// Derived: a segment of a push's changegroup, the least bytes one is cut at: an import's segment
/// (`vcs_store::hg_import::segment_bytes`, one trunk node of the changeset store's memtable), so a push and an import
/// land changesets of one size.
pub(crate) fn segment_bytes(state: &ShardState) -> u64 {
  #[cfg(test)]
  {
    let set = TEST_SEGMENT_BYTES.with(core::cell::Cell::get);
    if set > 0 {
      return set;
    }
  }
  u64::try_from(crate::vcs_store::hg_import::segment_bytes(state)).unwrap_or(u64::MAX)
}

/// A push's run of segment changesets (module docs): its repository, the store it lands on, and the sequences its
/// segments landed at, as ranges. Settled committed where an entry named it, else forgotten, and a run dropped
/// unsettled forgets itself.
#[derive(Debug)]
#[must_use = "a run unsettled forgets its segments as it drops"]
pub(crate) struct Run {
  /// The repository.
  repository: RepositoryId,
  /// The sequences landed, ascending and apart.
  ranges: Vec<(u64, u64)>,
  /// Its landed segments' file revisions, let go again and noted once its entry is committed (module docs, "Let go": a
  /// push's works bring back the texts they read between its segments and its entry).
  contents: Vec<FileContents>,
  /// The bytes of its landed segments' file texts still in the content store (kept, less what their let-go freed): what
  /// `hg_push::PUSH_HELD_HIGH_BYTES` adds to the push's admission held.
  texts_resident: u64,
  /// The push's admission held as its latest segment landed (`hg_push::segments_landed`).
  admission_held: u64,
  /// Whether it is settled.
  settled: bool,
}

impl Run {
  /// A run of `repository` with no segment landed yet.
  pub(crate) fn new(repository: RepositoryId) -> Run {
    Run {
      repository,
      ranges: Vec::new(),
      contents: Vec::new(),
      texts_resident: 0,
      admission_held: 0,
      settled: false,
    }
  }

  /// Notes segment changeset `sequence` landed, after every one before, its file revisions kept as `contents`.
  pub(crate) fn landed(&mut self, sequence: u64, contents: &[FileContents]) {
    self.contents.extend_from_slice(contents);
    match self.ranges.last_mut() {
      Some((_, last)) if last.checked_add(1) == Some(sequence) => *last = sequence,
      _ => self.ranges.push((sequence, sequence)),
    }
  }

  /// Notes the push's admission held as its next segment lands: what the high-water its landing samples reads.
  pub(crate) fn admission_held(&mut self, held: u64) {
    self.admission_held = held;
  }

  /// A segment's file texts landed, `kept` bytes of them in the content store before their let-go and `freed` bytes let
  /// go: what the push held then (its admission and its texts resident, these with them), and the texts resident after.
  /// The two figures: all it held, and its texts resident alone.
  pub(crate) fn texts_landed(&mut self, (kept, freed): (u64, u64)) -> (u64, u64) {
    let texts = self.texts_resident.saturating_add(kept);
    let held = self.admission_held.saturating_add(texts);
    self.texts_resident = texts.saturating_sub(freed);
    (held, texts)
  }

  /// The segments landed.
  #[cfg(test)]
  pub(crate) fn segments(&self) -> u64 {
    self.ranges.iter().fold(0, |count, (first, last)| {
      count.saturating_add(last.saturating_sub(*first).saturating_add(1))
    })
  }

  /// The register's record naming the run (record v11), its push's intake begun within `room`; none before a segment
  /// landed.
  pub(crate) fn reference(&self, room: u64) -> Option<Vec<u8>> {
    record::write_push_runs_reference(room, &self.ranges)
  }

  /// Ends a run no entry named (module docs, "Abandoned"): forgotten.
  pub(crate) fn abandon(mut self, state: &mut ShardState) {
    self.settled = true;
    abandoned(state, self.repository, core::mem::take(&mut self.ranges));
  }

  /// Ends a run its entry named (module docs, "Let go"): its repository and its file revisions, for their noting for a
  /// home and their let-go once more, whatever its works brought back, a turn at a time ([`let_go_committed`]).
  pub(crate) fn committed(mut self) -> (RepositoryId, Vec<FileContents>) {
    self.settled = true;
    (self.repository, core::mem::take(&mut self.contents))
  }
}

impl Drop for Run {
  fn drop(&mut self) {
    // Dropped unsettled (its push's task ended early): forgotten, where the shard's state is not borrowed then.
    if !self.settled && !self.ranges.is_empty() {
      let (repository, ranges) = (self.repository, core::mem::take(&mut self.ranges));
      let _ = crate::state::with_state_counted(move |state| abandoned(state, repository, ranges));
    }
  }
}

/// An abandoned run of `repository`, its segments at `ranges` (module docs, "Abandoned"): counted, and each sequence's
/// rows forgotten by a task of its own, each forgetting counted.
fn abandoned(state: &mut ShardState, repository: RepositoryId, ranges: Vec<(u64, u64)>) {
  if ranges.is_empty() {
    return;
  }
  state.count(RUNS_ABANDONED, 1);
  let Some(disk) = state.vcs_disk.clone() else {
    return;
  };
  let key = u128::from_be_bytes(repository.bytes);
  let forgetting = async move {
    for sequence in ranges.iter().flat_map(|(first, last)| *first..=*last) {
      let forgot = disk.forget(key, sequence).await;
      let _ = crate::state::with_state(|state| match forgot {
        Ok(rows) => state.count(RUN_ROWS_FORGOTTEN, rows),
        Err(_) => state.count(RUN_ROWS_UNFORGOTTEN, 1),
      });
    }
  };
  if slates_rt::futures::spawn_detached(forgetting).is_err() {
    state.count(RUN_ROWS_UNFORGOTTEN, 1);
  }
}

/// Counter: the bytes of pushes' file texts let go of the content store once the segment carrying them was durable
/// (module docs, "Let go"). Format: a counter name in the status report.
pub(crate) const TEXT_BYTES_LET_GO: &str = "vcs.hg.push_text_bytes_let_go";
/// Counter: a landed segment's window kept because a home holds it here (`vcs_store::landed_pin`: one this shard holds
/// for the region, which a home reads by identity alone), until its entry commits and notes it. Format: a counter name.
pub(crate) const TEXTS_PINNED: &str = "vcs.hg.push_texts_pinned";

/// Lets go of `contents`, a landed segment's file revisions' windows (module docs, "Let go"), each noted for a home as
/// [`let_go_windows`] notes it where `noting` names its committed repository; counted with its bytes, and the windows
/// kept pinned.
pub(crate) fn let_go_landed(
  state: &mut ShardState,
  noting: Option<RepositoryId>,
  contents: &[FileContents],
) -> u64 {
  let (freed, pinned) = let_go_windows(state, noting, contents);
  if freed > 0 {
    state.count(TEXT_BYTES_LET_GO, freed);
  }
  if pinned > 0 {
    state.count(TEXTS_PINNED, pinned);
  }
  freed
}

/// The bytes `contents`' objects hold in the shard's content store, their windows let go or not.
pub(crate) fn contents_bytes(state: &mut ShardState, contents: &[FileContents]) -> u64 {
  crate::vcs_store::lent_state(state, |state| {
    state.vcs_stores.content().map_or(0, |content| {
      contents
        .iter()
        .flat_map(FileContents::identities)
        .filter_map(|identity| content.len_of(identity))
        .fold(0, u64::saturating_add)
    })
  })
}

/// Lets go of `contents`' windows, each object's every window marked landed: all but those a home holds here
/// (`vcs_store::landed_pin`, as a landed git pack's), within the chunk store's lend. Where `noting` names the
/// repository whose index now holds them (its entry committed, or its import landed), each window is first noted with
/// its revision on this shard (`homes::note_landed`, `Landed::Hg`), so no home pins it and a home's read brings it back
/// by its path and node (`hg_push::bring_back`; goal 93 item 2b(b)); a window noted already (a landed pack's, another
/// revision's of the same bytes) keeps its note, which brings back the same bytes. The bytes freed and the windows kept
/// pinned: a push's ([`let_go_landed`]) and an import's (`hg_import::segment_landed`) one path.
pub(crate) fn let_go_windows(
  state: &mut ShardState,
  noting: Option<RepositoryId>,
  contents: &[FileContents],
) -> (u64, u64) {
  let (freed, pinned, _) = let_go_windows_until(state, noting, contents, u64::MAX);
  (freed, pinned)
}

/// [`let_go_windows`] over `contents` a revision at a time until the clock passes `deadline_ns`, one revision at least:
/// the bytes freed, the windows kept pinned, and how many revisions were taken.
fn let_go_windows_until(
  state: &mut ShardState,
  noting: Option<RepositoryId>,
  contents: &[FileContents],
  deadline_ns: u64,
) -> (u64, u64, usize) {
  // The repository's windows are the shard's chunk store's, lent to its content store for the step.
  crate::vcs_store::lent_state(state, |state| {
    crate::vcs_store::within_budget(state, 0, |stores| {
      let Some(content) = stores.content_mut() else {
        return (0, 0, contents.len());
      };
      let (mut freed, mut pinned, mut taken) = (0_u64, 0_u64, 0_usize);
      for revision in contents {
        for identity in revision.identities() {
          if let Some(repository) = noting {
            note_windows(content, repository, revision, identity);
          }
          let (more, kept) = content.let_go_object_landed(identity, &crate::vcs_store::landed_pin);
          freed = freed.saturating_add(more);
          pinned = pinned.saturating_add(kept);
        }
        taken = taken.saturating_add(1);
        if slates_rt::futures::now_ns() >= deadline_ns {
          break;
        }
      }
      (freed, pinned, taken)
    })
  })
}

/// Counter: the turns a committed run's or a landed import's file revisions were noted and let go over
/// ([`let_go_committed`]). Format: a counter name in the status report.
pub(crate) const LET_GO_TURNS: &str = "vcs.hg.landed_let_go_turns";
/// Counter: the longest of those turns, nanoseconds: held under the shard's step quantum (goal 93: a whole-history
/// push's let-go in one poll spun shard 0 for 1,045 ms of CPU on CI's x86 runner, run 45110563116). Format: a counter
/// name in the status report.
pub(crate) const LET_GO_LONGEST_TURN_NS: &str = "vcs.hg.landed_let_go_longest_turn_ns";

/// The budget of one turn of [`let_go_committed`]: half the shard's step quantum, as a push's work builds a turn
/// (`hg_push::built_works`), so the turn leaves the step room for what else the shard serves.
fn let_go_turn_ns(state: &ShardState) -> u64 {
  #[cfg(test)]
  {
    let set = TEST_LET_GO_TURN_NS.with(core::cell::Cell::get);
    if set > 0 {
      return set;
    }
  }
  (state.config.step_quantum_ns() / 2).max(1)
}

/// Notes `contents`, the file revisions of `repository` its index now holds (a push's entry committed, an import
/// landed), for a home and lets their windows go ([`let_go_windows`]), a turn at a time within [`let_go_turn_ns`],
/// yielding between: a whole history's revisions are hundreds of thousands, never one poll's work. Counted into
/// `freed_pinned` (the push's or the import's counters), with the turns and the longest. Ends where the shard's state
/// is gone; what was not let go then stays held, read as before.
pub(crate) async fn let_go_committed(
  repository: RepositoryId,
  contents: Vec<FileContents>,
  freed_pinned: (&'static str, &'static str),
) {
  let mut at = 0_usize;
  while let Some(rest) = contents.get(at..).filter(|rest| !rest.is_empty()) {
    // Each turn waits while a publication of the shard's image is owed past its bound (A-870 piece 2, main's progress
    // rule), woken by the publication that carries it up.
    crate::content_holder::image_resumed().await;
    let taken = crate::state::with_state(|state| {
      let began = slates_rt::futures::now_ns();
      let deadline = began.saturating_add(let_go_turn_ns(state));
      let (freed, pinned, taken) = let_go_windows_until(state, Some(repository), rest, deadline);
      state.count(freed_pinned.0, freed);
      state.count(freed_pinned.1, pinned);
      state.count(LET_GO_TURNS, 1);
      state.count_max(
        LET_GO_LONGEST_TURN_NS,
        slates_rt::futures::now_ns().saturating_sub(began),
      );
      taken
    });
    let Some(taken) = taken else {
      return;
    };
    at = at.saturating_add(taken.max(1));
    slates_rt::futures::yield_now().await;
  }
}

/// Notes each window of `identity`, one of `revision`'s objects in `repository`, with the revision for a home on this
/// shard (`homes::note_landed`) where a home would pin it (`vcs_store::landed_pin`: the shard holds it for the region,
/// itself or as a window of an object), so the note brings it back in the pin's place (w-shared's 1ecb57cf4, main's
/// ruling): a window no home holds is read only through the repository's index, which brings it back by itself, and needs
/// none. A window noted already keeps its note.
fn note_windows(
  content: &slates_vcs::ContentStore,
  repository: RepositoryId,
  revision: &FileContents,
  identity: &slates_vcs::ContentId,
) {
  use crate::vcs_store::homes::{self, HgLanded, Landed};
  for (window, _) in content.object_layout(identity) {
    if crate::vcs_store::landed_pin(&window).is_some() {
      homes::note_landed(
        window,
        Landed::Hg(HgLanded {
          repository,
          path: revision.path.clone(),
          node: revision.node,
        }),
      );
    }
  }
}

/// `repository`'s landing turn for a push's entry's commit (module docs, "Apart from the turn"), parking for it where
/// it is held, at most [`PATIENCE_NS`]: the turn, or slates' words past the patience; none where the shard's state is
/// gone.
pub(crate) async fn commit_turn(repository: RepositoryId) -> Option<Result<Turn, Vec<u8>>> {
  match crate::state::with_state(|state| Turn::take_now(state, repository))? {
    Some(turn) => Some(Ok(turn)),
    None => parked(repository).await,
  }
}

/// [`PATIENCE_NS`], or a test's on this shard's thread ([`TEST_TURN_PATIENCE_NS`]).
fn patience_ns() -> u64 {
  #[cfg(test)]
  {
    let set = TEST_TURN_PATIENCE_NS.with(core::cell::Cell::get);
    if set > 0 {
      return set;
    }
  }
  PATIENCE_NS
}

/// `repository`'s landing turn, parked for at most [`PATIENCE_NS`], the wait counted; slates' words past it; none where
/// the shard's state is gone.
async fn parked(repository: RepositoryId) -> Option<Result<Turn, Vec<u8>>> {
  crate::state::with_state(|state| state.count(TURN_WAITS, 1))?;
  let began = slates_rt::futures::now_ns();
  let waited = slates_rt::futures::within(patience_ns(), landing::turn(repository)).await;
  let spent = slates_rt::futures::now_ns().saturating_sub(began);
  let turn = crate::state::with_state(|state| {
    state.count(TURN_WAITED_NS, spent);
    match waited {
      Ok(Some(turn)) => turn.map(Ok),
      Ok(None) => {
        state.count(TURN_GIVEN_UP, 1);
        Some(Err(TURN_PAST_PATIENCE.to_vec()))
      }
      Err(_) => None,
    }
  });
  turn.flatten()
}

/// A segment changeset to land: the store, its header, and its sections.
pub(crate) type SegmentChangeset = (StoreHandle, Header, Vec<Part>);

/// `segment` of a push to `repository` as a changeset at the repository's next sequence, its owner shard's to allocate
/// (module docs): its `Record`
/// written by `record` over the intake's fork, its `Payload` its slice of `payload`, its `Texts` its texts. None where
/// the node holds no store; refused where the record could not be written or the slice is not the payload's.
pub(crate) fn segment_changeset(
  state: &mut ShardState,
  repository: RepositoryId,
  (segment, payload): (&PushSegment, &[u8]),
  record: impl FnOnce(&ContentStore) -> Result<Vec<u8>, record::WriteRefused>,
) -> Option<Result<SegmentChangeset, record::WriteRefused>> {
  let disk = state.vcs_disk.clone()?;
  // The fork's rows read the repository's windows: the shard's chunk store is lent to it.
  let written =
    crate::vcs_store::lent_state(state, |state| state.vcs_stores.content().map(record))?;
  let written = match written {
    Ok(written) => written,
    Err(refused) => return Some(Err(refused)),
  };
  let start = usize::try_from(segment.slice.start).unwrap_or(usize::MAX);
  let end = usize::try_from(segment.slice.end).unwrap_or(usize::MAX);
  let Some(slice) = payload.get(start..end) else {
    return Some(Err(record::WriteRefused::Allocation));
  };
  let slice = slice.to_vec();
  let identity = *blake3::hash(&slice).as_bytes();
  let sequence = state.vcs_stores.next_changeset(repository)?;
  let header = Header {
    tool: Tool::Mercurial,
    repository: u128::from_be_bytes(repository.bytes),
    sequence,
    judged_over: sequence.saturating_sub(1),
    grant: 0,
    work: 0,
    time_ns: slates_rt::futures::now_ns(),
    sections: 0,
  };
  let parts = vec![
    (SectionKind::Record, written),
    (SectionKind::Payload, slice),
    (SectionKind::Texts, texts_section(&segment.texts, identity)),
  ];
  Some(Ok((disk, header, parts)))
}

/// A changeset's `Texts` section: each kept revision's text, base and place in the changeset's payload, named by
/// `payload`, its identity (w-perf's Texts section, A-645 on A-767).
pub(crate) fn texts_section(texts: &[KeptText], payload: [u8; 32]) -> Vec<u8> {
  let entries: Vec<slates_store::texts::TextEntry> = texts
    .iter()
    .map(|kept| slates_store::texts::TextEntry {
      path: kept.path.clone(),
      tool_id: kept.node.to_vec(),
      text: kept.identity,
      len: kept.len,
      base: kept.base,
      place: slates_store::texts::Place {
        payload,
        offset: kept.place.start,
        len: kept.place.end.saturating_sub(kept.place.start),
      },
    })
    .collect();
  slates_store::texts::encode(&entries)
}
