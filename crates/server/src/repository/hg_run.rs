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
//! **Abandoned.** A run no entry names (the push refused, its connection ended, its task cancelled) is forgotten as it
//! ends: each of its sequences' rows dropped from the store (`StoreHandle::forget`, counted), its log bytes the owed
//! cleaner of unnamed log space's. So a client that keeps starting and dropping long pushes grows no rows. After a
//! crash a recovery applies only the sequences the register names, and the next landing takes the last named plus one,
//! its row the newest landing there.

use slates_db::catalog::RepositoryId;
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
  /// A test's patience for a landing turn on this shard's thread, in place of [`PATIENCE_NS`] where more than none: a
  /// test's push refused past it in milliseconds.
  pub(crate) static TEST_TURN_PATIENCE_NS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
  /// A test's crash point on this shard's thread where more than none: a push parks once its run has landed this many
  /// segments, as a push the daemon's crash stops there, until [`TEST_RELEASE`] is set.
  pub(crate) static TEST_PARK_AFTER_SEGMENTS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
  /// Set by a test on this shard's thread: a push parked at [`TEST_PARK_AFTER_SEGMENTS`] goes on.
  pub(crate) static TEST_RELEASE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
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
    while !TEST_RELEASE.with(core::cell::Cell::get) {
      let _ = slates_rt::futures::sleep(1_000_000).await;
    }
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
  /// Whether it is settled.
  settled: bool,
}

impl Run {
  /// A run of `repository` with no segment landed yet.
  pub(crate) fn new(repository: RepositoryId) -> Run {
    Run {
      repository,
      ranges: Vec::new(),
      settled: false,
    }
  }

  /// Notes segment changeset `sequence` landed, after every one before.
  pub(crate) fn landed(&mut self, sequence: u64) {
    match self.ranges.last_mut() {
      Some((_, last)) if last.checked_add(1) == Some(sequence) => *last = sequence,
      _ => self.ranges.push((sequence, sequence)),
    }
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

  /// Ends the run (module docs, "Abandoned"): kept where an entry named it (`committed`), else forgotten.
  pub(crate) fn settle(mut self, state: &mut ShardState, committed: bool) {
    self.settled = true;
    if !committed {
      abandoned(state, self.repository, core::mem::take(&mut self.ranges));
    }
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
