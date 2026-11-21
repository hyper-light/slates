//! The deltify job: a Subversion text a commit stored whole in the content store (A-761), kept as a delta once its node
//! moves past it, and its whole bytes released (main's ruling on A-761; GAPS' A-727 (c); §4.17.4 "Texts").
//!
//! **Why.** A commit's texts are put whole into the repository's content store, so a head file is one copy, read in
//! place (A-761, `svn_texts::HeadTexts`). Once a later revision changes the file, the old text is history: read rarely,
//! and kept whole it costs the store its full length for every version. The records keep every other text as an svndiff
//! against its node's skip-delta predecessor (S4 c2: at most `log2` of a node's predecessors deltas a read), and the
//! deltify job brings a superseded stored text to that same form, so the history holds deltas whatever made it.
//!
//! **What.** A landed revision hands the job the stored texts it moved past ([`after_revision`]; the records'
//! `Repository::deltify_after` walks its rows alone), and a recovery hands it every one the records still keep stored
//! ([`resume`]). For each, in order, oldest first: its delta is made a window a slice (the records' `DeltaWriter`, the
//! window Subversion cuts, each target piece read from the content store beside its base's piece read from the records;
//! a piece the content store does not hold is read through the node's changeset store, the job's task fetching it
//! between slices and the window's target read so far kept across the wait, C2 stage B part 2),
//! then landed as one changeset on the node's store and committed by an entry of the register naming it
//! (`Record::Deltified`, through the step for a record that moves no version, `repository::svn_commit::land_record`,
//! C2), and only then is the store's hold on the whole text given back.
//! A text whose skip-delta predecessor is itself still stored waits (the records name it again once its base is a
//! delta), so a chain is made from its oldest text up.
//!
//! **Durable.** The entry holds the delta, as the records' other texts are held (S4 c2), and a replay applies it through
//! the same step. A replay holds the texts the records still keep stored once its whole log is applied
//! (`super::hold_svn_stored`), never at the commit that stored them, so a text a later entry deltified, whose bytes the
//! store no longer holds, is not asked for. A crash between the entry and the release leaves the whole text held by
//! nothing the records name, which the recovery's sweep of unheld content frees.
//!
//! **Bounds.** One job a repository, kept by the repository's key, so the jobs are bounded by the partition's
//! repositories; its queue holds a candidate a stored text, at most the records' stored texts. A slice makes windows
//! until the shard's step budget is spent, at least one: each window reads at most Subversion's window of the text and
//! of its base. A refused text is counted and passed over; the rest go on.

use std::collections::{BTreeMap, VecDeque};

use slates_db::catalog::{Principal, RepositoryId};
use slates_svn::commit::record::Record;
use slates_svn::migration::{DeltaWriter, Deltifiable};

use crate::state::ShardState;

/// Counter: a stored text kept as a delta from now on, its entry landed and its whole bytes given back. Format: a
/// counter name in the status report.
pub(crate) const DELTIFIED: &str = "vcs_store.svn_deltify.deltified";
/// Counter: a delta window made; more windows than texts shows a text was made a window a slice. Format: a counter name.
pub(crate) const WINDOWS: &str = "vcs_store.svn_deltify.windows";
/// Counter: a text the job passed over: its bytes or its base could not be read, or its entry was refused. Format: a
/// counter name.
pub(crate) const REFUSED: &str = "vcs_store.svn_deltify.refused";

/// The jobs of the shard's repositories, by the repository's key.
#[derive(Debug, Default)]
pub(crate) struct Deltifies(BTreeMap<RepositoryId, Job>);

/// One repository's job: the texts waiting, and the one being made.
#[derive(Debug)]
struct Job {
  /// Who the entries are landed as: the repository's owner.
  owner: Principal,
  /// The texts waiting, oldest first.
  queue: VecDeque<Deltifiable>,
  /// The text being made.
  making: Option<Making>,
}

/// A text being made: its delta so far, the target bytes its windows made, and the next window's target bytes read so
/// far, kept across a wait for a piece the content store does not hold (C2 stage B part 2).
#[derive(Debug)]
struct Making {
  /// The text.
  candidate: Deltifiable,
  /// Its delta so far.
  writer: DeltaWriter,
  /// The target bytes its windows made.
  made: u64,
  /// The next window's target bytes read so far.
  partial: Vec<u8>,
}

/// What making a text's next window came to.
enum Windowed {
  /// The window made: the target bytes made after it.
  Made(u64),
  /// A piece of the text the content store does not hold, read through the node's store once fetched.
  Waiting(slates_svn::reads::Wanted),
  /// The text or its base cannot be read, or the writer refused.
  Refused,
}

/// What one unit of a job did.
enum Unit {
  /// A window made, or a text passed over: more may follow.
  Went,
  /// A text's windows are all made: its record lands next, as the job's owner, then its whole bytes are given back.
  Land(Landing),
  /// A piece of the text being made is waited for (C2 stage B part 2).
  Waiting(slates_svn::reads::Wanted),
  /// The queue is empty: the job ends.
  Ended,
}

/// A text's delta made whole, to land: its record, the stored text it deltifies, and the job's owner it lands as.
type Landing = (Record, slates_svn::index::ContentIdentity, Principal);

/// What a slice of a job left.
pub(crate) enum Slice {
  /// More units follow.
  More,
  /// A text's record lands before the next unit.
  Land(Landing),
  /// A piece of the text being made is fetched through the node's store before the next unit (C2 stage B part 2).
  Waiting(slates_svn::reads::Wanted),
  /// The job ended.
  Ended,
}

/// Hands repository `repository`'s job the stored texts `revision` moved past, starting the job when none runs.
pub(crate) fn after_revision(state: &mut ShardState, repository: RepositoryId, revision: u64) {
  let Some(candidates) = state
    .vcs_stores
    .subversion_records(repository)
    .map(|records| records.deltify_after(revision))
  else {
    return;
  };
  hand(state, repository, candidates);
}

/// Hands repository `repository`'s job every text its records keep stored that the job may take now: what a recovery
/// asks once the repository's log is replayed, so a job a restart cut off goes on.
pub(crate) fn resume(state: &mut ShardState, repository: RepositoryId) {
  let Some(candidates) = state
    .vcs_stores
    .subversion_records(repository)
    .map(slates_svn::migration::Repository::deltify_candidates)
  else {
    return;
  };
  hand(state, repository, candidates);
}

/// Queues `candidates` on `repository`'s job, each text once, and starts the job's task if none runs.
fn hand(state: &mut ShardState, repository: RepositoryId, candidates: Vec<Deltifiable>) {
  if candidates.is_empty() {
    return;
  }
  let jobs = &mut state.vcs_stores.svn_deltifies.0;
  if let Some(job) = jobs.get_mut(&repository) {
    for candidate in candidates {
      let queued = job.queue.iter().any(|queued| queued.text == candidate.text)
        || job
          .making
          .as_ref()
          .is_some_and(|making| making.candidate.text == candidate.text);
      if !queued {
        job.queue.push_back(candidate);
      }
    }
    return;
  }
  let Some(owner) = state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .map(|record| record.owner.clone())
  else {
    return;
  };
  jobs.insert(
    repository,
    Job {
      owner,
      queue: candidates.into(),
      making: None,
    },
  );
  if let Ok(task) = slates_rt::futures::spawn(drive(repository)) {
    let _ = slates_rt::futures::detach(task);
  }
}

/// Drives `repository`'s job a slice at a time, yielding between slices, until it ends; a text's record landed on the
/// node's store and committed in the register (`svn_commit::land_record`, C2) before its whole bytes are given back.
async fn drive(repository: RepositoryId) {
  // The piece fetched for the text being made, held until the slice after the fetch has read it (C2 stage B part 2).
  let mut fetched = None;
  loop {
    let sliced = crate::state::with_state_counted(|state| {
      let budget = slates_rt::futures::step_budget_ns().unwrap_or(0);
      let sliced = slice(state, repository, budget);
      if let Some(read) = fetched {
        super::svn_texts::let_go(state, read);
      }
      // A text waiting again for the piece just fetched for it would wait for good: passed over.
      match sliced {
        Slice::Waiting(wanted) if fetched == Some(wanted) => {
          pass_over(state, repository);
          Slice::More
        }
        sliced => sliced,
      }
    });
    fetched = None;
    match sliced {
      Some(Slice::More) => slates_rt::futures::yield_now().await,
      Some(Slice::Waiting(wanted)) => {
        if super::svn_texts::fetch_piece(repository, wanted).await {
          fetched = Some(wanted);
        } else {
          let _ = crate::state::with_state_counted(|state| pass_over(state, repository));
        }
      }
      Some(Slice::Land((record, text, owner))) => {
        let landed = crate::repository::svn_commit::land_record((&owner, repository), record).await;
        let _ = crate::state::with_state_counted(|state| landed_text(state, text, landed));
      }
      Some(Slice::Ended) | None => return,
    }
  }
}

/// Passes over the text `repository`'s job is making, counted: a piece of it the node's store could not give.
fn pass_over(state: &mut ShardState, repository: RepositoryId) {
  if let Some(job) = state.vcs_stores.svn_deltifies.0.get_mut(&repository) {
    job.making = None;
  }
  state.count(REFUSED, 1);
}

/// What a text's landing did: its whole bytes given back once its record stands, counted either way.
fn landed_text(
  state: &mut ShardState,
  text: slates_svn::index::ContentIdentity,
  landed: Option<Result<(), crate::repository::svn_commit::Uncommitted>>,
) {
  match landed {
    Some(Ok(())) => {
      crate::repository::svn_commit::release_texts(state, None, &[text]);
      state.count(DELTIFIED, 1);
    }
    refused => {
      slates_machine::log_line!(
        "slates-server: partition {}: a stored Subversion text was not deltified: {refused:?}",
        state.partition
      );
      state.count(REFUSED, 1);
    }
  }
}

/// One slice of `repository`'s job: units until `budget_ns` is spent, at least one, or until a text's record is due to
/// land.
pub(crate) fn slice(state: &mut ShardState, repository: RepositoryId, budget_ns: u64) -> Slice {
  let began = slates_rt::futures::now_ns();
  loop {
    match unit(state, repository) {
      Unit::Ended => {
        state.vcs_stores.svn_deltifies.0.remove(&repository);
        return Slice::Ended;
      }
      Unit::Land(landing) => return Slice::Land(landing),
      Unit::Waiting(wanted) => return Slice::Waiting(wanted),
      Unit::Went => {
        if slates_rt::futures::now_ns().saturating_sub(began) >= budget_ns {
          return Slice::More;
        }
      }
    }
  }
}

/// One unit of `repository`'s job: the next window of the text being made, or the text landed once its windows are
/// made, or the next text begun.
fn unit(state: &mut ShardState, repository: RepositoryId) -> Unit {
  let Some(job) = state.vcs_stores.svn_deltifies.0.get_mut(&repository) else {
    return Unit::Ended;
  };
  let mut making = match job.making.take() {
    Some(making) => making,
    None => match job.queue.pop_front() {
      Some(candidate) => Making {
        candidate,
        writer: DeltaWriter::new(),
        made: 0,
        partial: Vec::new(),
      },
      None => return Unit::Ended,
    },
  };
  let owner = job.owner.clone();
  if making.made < making.candidate.len {
    let unit = match window(state, repository, &mut making) {
      Windowed::Made(made) => {
        state.count(WINDOWS, 1);
        making.made = made;
        Unit::Went
      }
      Windowed::Waiting(wanted) => Unit::Waiting(wanted),
      Windowed::Refused => {
        state.count(REFUSED, 1);
        return Unit::Went;
      }
    };
    if let Some(job) = state.vcs_stores.svn_deltifies.0.get_mut(&repository) {
      job.making = Some(making);
    }
    return unit;
  }
  let Making {
    candidate, writer, ..
  } = making;
  let record = Record::Deltified {
    text: candidate.text,
    base: candidate.base.map(|(base, _)| base),
    delta: writer.finish(),
  };
  Unit::Land((record, candidate.text, owner))
}

/// Makes the next window of the text `making`'s delta: at most a window of its text from what its windows made, read
/// from the content store, or through the node's store where it does not hold it (C2 stage B part 2), against the piece
/// of its base beside it, read from the records (Subversion's windows: source piece *i* beside target piece *i*, an
/// empty view at the source's end once it runs out).
fn window(state: &mut ShardState, repository: RepositoryId, making: &mut Making) -> Windowed {
  let room = DeltaWriter::window_room();
  let Making {
    candidate,
    writer,
    made,
    partial,
  } = making;
  let (candidate, made) = (&*candidate, *made);
  let want = room.min(candidate.len.saturating_sub(made));
  let read = crate::vcs_store::within_budget(state, 0, |stores| {
    read_range(stores, candidate, made, (want, &mut *partial))
  });
  match read {
    Some(Read::Whole) => {}
    Some(Read::Waiting(wanted)) => return Windowed::Waiting(wanted),
    None => return Windowed::Refused,
  }
  let target = std::mem::take(partial);
  let made = crate::vcs_store::within_budget(state, 0, |stores| {
    let records = stores.subversion_records(repository)?;
    let (source, source_offset) = match candidate.base {
      Some((base, base_len)) if made < base_len => {
        let len = room.min(base_len.saturating_sub(made));
        (records.kept_range(&base, made, len)?, made)
      }
      Some((_, base_len)) => (Vec::new(), base_len),
      None => (Vec::new(), 0),
    };
    writer.window(&source, source_offset, &target)?;
    made.checked_add(u64::try_from(target.len()).ok()?)
  });
  made.map_or(Windowed::Refused, Windowed::Made)
}

/// What a read of a window's target came to.
enum Read {
  /// The window's target bytes are all read.
  Whole,
  /// A piece neither the content store nor the fetched pieces hold, read through the node's store once fetched.
  Waiting(slates_svn::reads::Wanted),
}

/// Reads `want` bytes of the stored text `candidate` from `offset` into `partial`, from where it stands: a store window
/// at a time (`ContentStore::run_at`), else a piece fetched through the node's store; none where the bytes run out.
fn read_range(
  stores: &super::VcsStores,
  candidate: &Deltifiable,
  offset: u64,
  (want, partial): (u64, &mut Vec<u8>),
) -> Option<Read> {
  let id = slates_vcs::ContentId(candidate.text);
  let end = offset.checked_add(want)?;
  let mut at = offset.checked_add(u64::try_from(partial.len()).ok()?)?;
  while at < end {
    let held = stores
      .content()
      .and_then(|store| store.run_at(&id, candidate.len, at));
    let Some(run) = held.or_else(|| stores.svn_pieces.at(&candidate.text, at).map(Into::into))
    else {
      return Some(Read::Waiting(slates_svn::reads::Wanted {
        text: candidate.text,
        offset: at,
      }));
    };
    let take = usize::try_from(end.saturating_sub(at)).ok()?.min(run.len());
    if take == 0 {
      return None;
    }
    partial.extend_from_slice(run.get(..take)?);
    at = at.checked_add(u64::try_from(take).ok()?)?;
  }
  Some(Read::Whole)
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_svn::commit::drive::TextSink;

  /// Commits `bytes` at `/a.txt` on revision `head`, its text put into the content store as a client's commit puts it
  /// (A-761): the step's answer.
  async fn land(
    repository: RepositoryId,
    head: u64,
    action: slates_svn::index::Action,
    bytes: Vec<u8>,
  ) -> bool {
    let text = slates_svn::commit::drive::text_of(&bytes).unwrap();
    let texts = slates_svn::commit::drive::HeldTexts::new();
    let staged = texts.stage().unwrap();
    texts.append(staged, &bytes).unwrap();
    texts.seal(staged, text.identity).unwrap();
    let work = slates_svn::commit::drive::Work {
      head,
      record: slates_svn::index::RevisionRecord {
        transaction: format!("{head}-1"),
        changes: vec![slates_svn::index::ChangedPath {
          path: "/a.txt".to_owned(),
          action,
          kind: slates_svn::index::NodeKind::File,
          copy_from: None,
          text: Some(text),
          properties: None,
          mergeinfo_modified: false,
        }],
      },
      lock_entries: Vec::new(),
      revision_properties: vec![(b"svn:author".to_vec(), b"alice".to_vec())],
      texts: std::iter::once(text.identity).collect(),
      property_lists: BTreeMap::new(),
      locks: slates_svn::lock::CommitLocks::default(),
    };
    let owner = crate::repository::svn_commit::tests::owner();
    crate::repository::svn_commit::land_work((&owner, repository), work, &texts)
      .await
      .is_some_and(|landed| landed.is_ok())
  }

  /// C2 stage B part 2 (the deltify job reads a stored text RAM does not hold through the node's changeset store, R6).
  /// Do: commit a 250,000-byte text at `/a.txt`, let go the content store's hold on it, then commit a revision changing
  /// it; let the job run. Expect: the first text kept as a delta by the records, read back byte for byte, made in three
  /// windows with nothing refused, its bytes read through the store a piece at a time
  /// (`vcs_store.svn_texts.piece_fetched` at least once), every fetched piece let go after. Before, the job passed the
  /// text over (`vcs_store.svn_deltify.refused`), leaving it stored whole.
  #[test]
  fn a_stored_text_the_content_store_let_go_is_deltified_through_the_store() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    let first = crate::test_store::noise(250_000);
    let mut second = first.clone();
    second[1_000] ^= 1;
    second.extend_from_slice(b"tail");
    let identity = *blake3::hash(&first).as_bytes();
    let (sent, got) = std::sync::mpsc::channel();
    let (one, two) = (first.clone(), second.clone());
    crate::daemon::observe_first(&daemon, move |_state| {
      let task = slates_rt::futures::spawn(async move {
        let (repository, _) = crate::repository::svn_commit::tests::subversion("deltify").await;
        let added = land(repository, 0, slates_svn::index::Action::Add, one).await;
        // RAM lets the text go: only the node's store holds it now.
        crate::state::with_state(|state| {
          crate::vcs_store::within_budget(state, 0, |stores| {
            if let Some(content) = stores.content_mut() {
              let content_id = slates_vcs::ContentId(identity);
              while content.contains(&content_id) {
                if content.release(&content_id).is_err() {
                  break;
                }
              }
            }
          });
        });
        let changed = land(repository, 1, slates_svn::index::Action::Modify, two).await;
        let _ = sent.send((repository, added, changed));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let (repository, added, changed) = got
      .recv_timeout(std::time::Duration::from_secs(60))
      .expect("both commits answered");
    assert_eq!((added, changed), (true, true));
    let counted = |name: &'static str| {
      crate::daemon::observe_first(&daemon, move |state| {
        state.refusals.get(name).copied().unwrap_or(0)
      })
    };
    let waited = std::time::Instant::now();
    while counted(DELTIFIED) + counted(REFUSED) == 0
      && waited.elapsed() < std::time::Duration::from_secs(60)
    {
      std::thread::yield_now();
    }
    let counts = (counted(DELTIFIED), counted(WINDOWS), counted(REFUSED));
    let fetched = counted(crate::vcs_store::svn_texts::PIECE_FETCHED);
    let (kept, left) = crate::daemon::observe_first(&daemon, move |state| {
      let records = state.vcs_stores.subversion_records(repository).unwrap();
      (
        (
          records.stored_len(&identity),
          records.text(&identity).map(std::borrow::Cow::into_owned),
        ),
        state.vcs_stores.svn_pieces_held(),
      )
    });
    daemon.stop();
    assert_eq!(
      counts,
      (1, 3, 0),
      "one text deltified, a window of 100 KiB a unit"
    );
    assert!(
      kept == (None, Some(first)),
      "kept as a delta, read back whole"
    );
    assert!(fetched >= 1, "no piece read through the store");
    assert_eq!(left, 0, "a fetched piece was kept");
  }

  /// Main's ruling on A-761 (the deltify job; w-perf's C1). Do: commit a 250,000-byte text at `/a.txt` (stored whole in
  /// the content store), then a revision changing two of its bytes and adding four; let the job run; then make the
  /// repository's store again from its log. Expect: the first text kept as a delta by the records (no longer stored),
  /// made in three windows of Subversion's 100 KiB, its whole bytes no longer held by the store, read back byte for byte
  /// before and after the rebuild, and the rebuild refusing nothing (its replay holds no text a later entry deltified).
  #[test]
  fn a_stored_text_its_node_moved_past_is_kept_as_a_delta_and_its_whole_bytes_released() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    let first: Vec<u8> = (0..250_000_u32).map(|at| (at % 251) as u8).collect();
    let mut second = first.clone();
    second[1_000] ^= 1;
    second[200_000] ^= 1;
    second.extend_from_slice(b"tail");
    let identity = *blake3::hash(&first).as_bytes();
    let (sent, got) = std::sync::mpsc::channel();
    let (one, two) = (first.clone(), second.clone());
    crate::daemon::observe_first(&daemon, move |_state| {
      let task = slates_rt::futures::spawn(async move {
        let (repository, _) = crate::repository::svn_commit::tests::subversion("deltify").await;
        let added = land(repository, 0, slates_svn::index::Action::Add, one).await;
        let changed = land(repository, 1, slates_svn::index::Action::Modify, two).await;
        let _ = sent.send((repository, added, changed));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let (repository, added, changed) = got
      .recv_timeout(std::time::Duration::from_secs(60))
      .expect("both commits answered");
    assert_eq!((added, changed), (true, true));
    let counted = |name: &'static str| {
      crate::daemon::observe_first(&daemon, move |state| {
        state.refusals.get(name).copied().unwrap_or(0)
      })
    };
    let waited = std::time::Instant::now();
    while counted(DELTIFIED) == 0 && waited.elapsed() < std::time::Duration::from_secs(60) {
      std::thread::yield_now();
    }
    assert_eq!(
      (counted(DELTIFIED), counted(WINDOWS), counted(REFUSED)),
      (1, 3, 0),
      "one text deltified, a window of 100 KiB a unit"
    );
    let read = move |state: &mut ShardState| {
      let records = state.vcs_stores.subversion_records(repository).unwrap();
      let id = slates_vcs::ContentId(identity);
      let holders = state
        .vcs_stores
        .content()
        .map_or(0, |store| store.holders(&store.root_of(&id)));
      (
        records.stored_len(&identity),
        records.text(&identity).map(std::borrow::Cow::into_owned),
        holders,
      )
    };
    let want = first.clone();
    assert_eq!(
      crate::daemon::observe_first(&daemon, read),
      (None, Some(want.clone()), 0),
      "kept as a delta, read back whole, the store's hold given back"
    );
    crate::daemon::observe_first(&daemon, move |state| {
      crate::vcs_store::rebuild(state, repository)
    });
    // A rebuild runs in turns (`rebuild_turns`): its records are read once it is done.
    let waited = std::time::Instant::now();
    let rebuilt = loop {
      let seen = crate::daemon::observe_first(&daemon, move |state| {
        state
          .vcs_stores
          .subversion_records(repository)
          .is_some()
          .then(|| {
            (
              state
                .refusals
                .get(crate::vcs_store::REPLAY_REFUSED)
                .copied()
                .unwrap_or(0),
              read(state),
            )
          })
      });
      if let Some(seen) = seen {
        break seen;
      }
      assert!(
        waited.elapsed() < std::time::Duration::from_secs(60),
        "the rebuild ended"
      );
      std::thread::yield_now();
    };
    assert_eq!(
      rebuilt,
      (0, (None, Some(want), 0)),
      "the log replays to the same records"
    );
  }
}
