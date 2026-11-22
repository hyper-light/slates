//! A Mercurial push landed as a run of segment changesets (main 01a20182's ruling (b1) of 2026-11-22 04:44), under its
//! repository's landing turn (ruling (i), modelled on `svn_commit::landing::Turn` and the same turn: Mercurial's made
//! changesets hold it too, so a push's run and a made changeset never interleave on the log).
//!
//! **Why.** A push held every revision's kept text until its one changeset landed after its intake, so a whole-history
//! push's bookkeeping grew with the history until it passed what its repository could ever be lent (main's measure of
//! 2026-11-22 04:59: 1.37 GB held on a 16 GB runner). It now lands as the import does: its intake cuts its kept texts
//! into segments of at least [`segment_bytes`] of its changegroup (`Intake::cutting`), and each lands as it is cut,
//! between the intake's slices, as an ordinary bounded changeset: its `Record` the segment's rows by identity (record
//! v8 of a push's source, `intake::segment_record`), its `Payload` its slice of the changegroup, its `Texts` each
//! revision's text, base and place in that slice. What the segment's texts charged the push's admission is given back
//! once it lands. The push's entry names the run (record v9, `record::write_push_run_reference`); a replay reads each
//! segment's record back and appends its rows by identity, as an import's run replays.
//!
//! **The turn.** A run's rows are numbered after every row the repository's index holds, and a replay reads the run's
//! sequences first to last, so nothing else may land on the repository's log or replace its index from the run's
//! first segment to its entry. A push takes its repository's landing turn at its first segment, or at its intake's end
//! where it cut none, and holds it to its commit or give-back ([`Run`]); a push of the same repository that needs it
//! meanwhile parks with its intake held, for at most [`PATIENCE_NS`], and is refused typed past it
//! ([`TURN_PAST_PATIENCE`]). Other repositories never wait. A fork made before the turn was taken and stale once it is
//! (another landing replaced the index) is taken again inside the turn, from its changegroup: a stale fork's rows
//! could not be numbered on the log, and inside the turn its new fork stays fresh.
//!
//! **Given back.** Every terminal path ends the turn: the push's landing settles its run ([`Run::settle`]), and a run
//! dropped unsettled (its connection ended, its task cancelled) settles itself as it drops. A run not named by an entry
//! is abandoned: its repository's next sequence is counted again from the last the register named before it, so the
//! next landing takes its sequences, each store row replaced by the newest landing at it, as an import's refused run
//! is (decision 2).

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
/// Counter: a push's run of segments given back unnamed, its sequences counted again from the last named. Format: a
/// counter name.
pub(crate) const RUNS_ABANDONED: &str = "vcs.hg.push_runs_abandoned";
/// Counter: a push that found its repository's landing turn held and parked for it, counted as it parks. Format: a
/// counter name.
pub(crate) const TURN_WAITS: &str = "vcs.hg.push_turn_waits";
/// Counter: the nanoseconds pushes parked for their repository's landing turn, summed (main's ruling (i): the wait a
/// small push takes behind a whole-history push). Format: a counter name.
pub(crate) const TURN_WAITED_NS: &str = "vcs.hg.push_turn_waited_ns";
/// Counter: a push refused for waiting past [`PATIENCE_NS`] for its repository's landing turn. Format: a counter name.
pub(crate) const TURN_GIVEN_UP: &str = "vcs.hg.push_turn_given_up";
/// Counter: a push's intake taken again inside the turn, its fork made stale before it took it. Format: a counter name.
pub(crate) const RETAKEN: &str = "vcs.hg.push_retaken_in_turn";

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
  /// A test's crash point on this shard's thread where more than none: a push parks for good once its run has landed
  /// this many segments, its turn held, as a push the daemon's crash stops there.
  pub(crate) static TEST_PARK_AFTER_SEGMENTS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Parks for good where a test's crash point is `run`'s segments landed so far ([`TEST_PARK_AFTER_SEGMENTS`]).
#[cfg(test)]
pub(crate) async fn parked_for_a_crash(run: &Run) {
  let at = TEST_PARK_AFTER_SEGMENTS.with(core::cell::Cell::get);
  let landed = run.landed.map_or(0, |(first, last)| {
    last.saturating_sub(first).saturating_add(1)
  });
  if at > 0 && landed == at {
    std::future::pending::<()>().await;
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

/// A push's run of segment changesets under its repository's landing turn (module docs): the turn, the last sequence
/// the register named when it was taken, and the run's first and last sequence once a segment landed.
#[derive(Debug)]
#[must_use = "a run unsettled holds its repository's landing turn until it drops"]
pub(crate) struct Run {
  /// The repository.
  repository: RepositoryId,
  /// Its landing turn, until settled.
  turn: Option<Turn>,
  /// The last sequence its repository counted landed when the turn was taken: what an abandoned run counts from again.
  named: u64,
  /// The first and the last sequence landed.
  landed: Option<(u64, u64)>,
}

impl Run {
  /// Takes `repository`'s landing turn (module docs), parking for it where it is held, at most [`PATIENCE_NS`]: the run,
  /// or slates' words past the patience; none where the shard's state is gone.
  pub(crate) async fn begin(repository: RepositoryId) -> Option<Result<Run, Vec<u8>>> {
    let taken = crate::state::with_state(|state| Turn::take_now(state, repository))?;
    let turn = match taken {
      Some(turn) => turn,
      None => match parked(repository).await? {
        Ok(turn) => turn,
        Err(words) => return Some(Err(words)),
      },
    };
    let named =
      crate::state::with_state(|state| state.vcs_stores.landed_of(repository).unwrap_or(0))?;
    Some(Ok(Run {
      repository,
      turn: Some(turn),
      named,
      landed: None,
    }))
  }

  /// Takes `repository`'s landing turn in this step where it is free: a test's driver, which holds the shard's state and
  /// cannot park.
  #[cfg(test)]
  pub(crate) fn take_now(state: &mut ShardState, repository: RepositoryId) -> Option<Run> {
    let turn = Turn::take_now(state, repository)?;
    Some(Run {
      repository,
      turn: Some(turn),
      named: state.vcs_stores.landed_of(repository).unwrap_or(0),
      landed: None,
    })
  }

  /// Notes segment changeset `sequence` landed.
  pub(crate) fn landed(&mut self, sequence: u64) {
    self.landed = Some(match self.landed {
      Some((first, _)) => (first, sequence),
      None => (sequence, sequence),
    });
  }

  /// The register's record naming the run (record v9), its push's intake begun within `room`; none before a segment
  /// landed.
  pub(crate) fn reference(&self, room: u64) -> Option<Vec<u8>> {
    self
      .landed
      .map(|run| record::write_push_run_reference(room, run))
  }

  /// Ends the run (module docs, "Given back"): `committed` where an entry named it, else abandoned, its repository's
  /// next sequence counted again from the last named before it; the turn given back either way.
  pub(crate) fn settle(mut self, state: &mut ShardState, committed: bool) {
    let turn = self.turn.take();
    ended(state, (self.repository, self.named), committed);
    if let Some(turn) = turn {
      turn.give_back(state);
    }
  }
}

impl Drop for Run {
  fn drop(&mut self) {
    // Dropped unsettled (its push's task ended early): abandoned, where the shard's state is not borrowed then.
    if let Some(turn) = self.turn.take() {
      let run = (self.repository, self.named);
      let _ = crate::state::with_state_counted(move |state| {
        ended(state, run, false);
        turn.give_back(state);
      });
    }
  }
}

/// A run of `repository` ended (module docs, "Given back"): abandoned where no entry named it and it took a sequence,
/// landed or refused by the store, its repository's next sequence counted from `named` again; nothing else took one
/// while it held the turn.
fn ended(state: &mut ShardState, (repository, named): (RepositoryId, u64), committed: bool) {
  if committed
    || state
      .vcs_stores
      .landed_of(repository)
      .is_none_or(|landed| landed == named)
  {
    return;
  }
  state.vcs_stores.set_landed(repository, named);
  state.count(RUNS_ABANDONED, 1);
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

/// `segment` of a push to `repository` as a changeset at the repository's next sequence (module docs): its `Record`
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
