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
//! window Subversion cuts, each target piece read from the content store beside its base's piece read from the records),
//! then landed as one entry of the repository's logs (`Record::Deltified`, through the owner's step for a record that
//! moves no version, `repository::svn_commit::record`), and only then is the store's hold on the whole text given back.
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
  /// The text being made, its delta so far, and the target bytes its windows made.
  making: Option<(Deltifiable, DeltaWriter, u64)>,
}

/// What one unit of a job did.
enum Unit {
  /// A window made, or a text landed or passed over: more may follow.
  Went,
  /// The queue is empty: the job ends.
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
          .is_some_and(|(making, _, _)| making.text == candidate.text);
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

/// Drives `repository`'s job a slice at a time, yielding between slices, until it ends.
async fn drive(repository: RepositoryId) {
  while crate::state::with_state_counted(|state| {
    let budget = slates_rt::futures::step_budget_ns().unwrap_or(0);
    slice(state, repository, budget)
  }) == Some(true)
  {
    slates_rt::futures::yield_now().await;
  }
}

/// One slice of `repository`'s job: units until `budget_ns` is spent, at least one. Whether more follow.
pub(crate) fn slice(state: &mut ShardState, repository: RepositoryId, budget_ns: u64) -> bool {
  let began = slates_rt::futures::now_ns();
  loop {
    match unit(state, repository) {
      Unit::Ended => {
        state.vcs_stores.svn_deltifies.0.remove(&repository);
        return false;
      }
      Unit::Went => {
        if slates_rt::futures::now_ns().saturating_sub(began) >= budget_ns {
          return true;
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
  let (candidate, mut writer, made) = match job.making.take() {
    Some(making) => making,
    None => match job.queue.pop_front() {
      Some(candidate) => (candidate, DeltaWriter::new(), 0),
      None => return Unit::Ended,
    },
  };
  let owner = job.owner.clone();
  if made < candidate.len {
    let Some(made) = window(state, repository, &candidate, (&mut writer, made)) else {
      state.count(REFUSED, 1);
      return Unit::Went;
    };
    state.count(WINDOWS, 1);
    if let Some(job) = state.vcs_stores.svn_deltifies.0.get_mut(&repository) {
      job.making = Some((candidate, writer, made));
    }
    return Unit::Went;
  }
  let record = Record::Deltified {
    text: candidate.text,
    base: candidate.base.map(|(base, _)| base),
    delta: writer.finish(),
  };
  match crate::repository::svn_commit::record(state, (&owner, repository), record) {
    Ok(()) => {
      crate::repository::svn_commit::release_texts(state, None, &[candidate.text]);
      state.count(DELTIFIED, 1);
    }
    Err(refused) => {
      slates_machine::log_line!(
        "slates-server: partition {}: a stored Subversion text was not deltified: {refused:?}",
        state.partition
      );
      state.count(REFUSED, 1);
    }
  }
  Unit::Went
}

/// Makes the next window of `candidate`'s delta: at most a window of its text from `made`, read from the content store,
/// against the piece of its base beside it, read from the records (Subversion's windows: source piece *i* beside target
/// piece *i*, an empty view at the source's end once it runs out). The target bytes made after it; none if the text or
/// its base cannot be read, or the writer refuses.
fn window(
  state: &mut ShardState,
  repository: RepositoryId,
  candidate: &Deltifiable,
  (writer, made): (&mut DeltaWriter, u64),
) -> Option<u64> {
  let room = DeltaWriter::window_room();
  let want = room.min(candidate.len.saturating_sub(made));
  crate::vcs_store::within_budget(state, 0, |stores| {
    let target = read_range(stores.content()?, candidate, made, want)?;
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
  })
}

/// `want` bytes of the stored text `candidate` from `offset`, read a store window at a time (`ContentStore::run_at`).
fn read_range(
  store: &slates_vcs::ContentStore,
  candidate: &Deltifiable,
  offset: u64,
  want: u64,
) -> Option<Vec<u8>> {
  let id = slates_vcs::ContentId(candidate.text);
  let mut out = Vec::with_capacity(usize::try_from(want).ok()?);
  let end = offset.checked_add(want)?;
  let mut at = offset;
  while at < end {
    let run = store.run_at(&id, candidate.len, at)?;
    let take = usize::try_from(end.saturating_sub(at)).ok()?.min(run.len());
    if take == 0 {
      return None;
    }
    out.extend_from_slice(run.get(..take)?);
    at = at.checked_add(u64::try_from(take).ok()?)?;
  }
  Some(out)
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

  /// Main's ruling on A-761 (the deltify job; w-perf's C1). Do: commit a 250,000-byte text at `/a.txt` (stored whole in
  /// the content store), then a revision changing two of its bytes and adding four; let the job run; then make the
  /// repository's store again from its log. Expect: the first text kept as a delta by the records (no longer stored),
  /// made in three windows of Subversion's 100 KiB, its whole bytes no longer held by the store, read back byte for byte
  /// before and after the rebuild, and the rebuild refusing nothing (its replay holds no text a later entry deltified).
  #[test]
  fn a_stored_text_its_node_moved_past_is_kept_as_a_delta_and_its_whole_bytes_released() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let first: Vec<u8> = (0..250_000_u32).map(|at| (at % 251) as u8).collect();
    let mut second = first.clone();
    second[1_000] ^= 1;
    second[200_000] ^= 1;
    second.extend_from_slice(b"tail");
    let identity = *blake3::hash(&first).as_bytes();
    let (sent, got) = std::sync::mpsc::channel();
    let (one, two) = (first.clone(), second.clone());
    crate::daemon::observe_first(&daemon, move |state| {
      let (repository, _) = crate::repository::svn_commit::tests::subversion(state, "deltify");
      let task = slates_rt::futures::spawn(async move {
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
