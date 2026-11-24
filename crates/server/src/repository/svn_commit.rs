//! Subversion's commit, landed by the repository's owner (A-655 part 2b; §4.17.4 items 1, 3.3 to 3.5 and piece 7;
//! §4.17.1 "A push is a work" and "Durability and residency"; A-635, A-662, A-645).
//!
//! **The step** ([`land_revision`], [`land_work`]): one for a client's accepted work and for an imported revision (A-652
//! part 3), on the repository's owner shard:
//! 1. the repository's records (A-662's map, `crate::vcs_store`), lent the room the shard could admit;
//! 2. a client's work judged again where its verdict's facts moved (`Repository::judge_again`: FSFS's merge again at
//!    the youngest revision, then its locks verified as the table stands, `tree.c` `svn_fs_fs__commit_txn` and
//!    `transaction.c` `commit_body`), so a work whose head moved is refused only where the merge finds a conflict, as
//!    FSFS refuses one; an imported revision as it was made;
//! 3. the check (`Repository::check`, which changes nothing), and the records' growth charged to the shard's budget;
//! 4. the green's next version ([`Building`]): the increment moving the repository's one green from its head, which is
//!    the records' youngest revision (version N is revision N, item 1), to the revision;
//! 5. the revision's record (`slates_svn::commit::record`) landed whole on the node's changeset store, durable, as the
//!    repository's next changeset ([`landing`]; C2 of Ada's close list: the store is the truth);
//! 6. one entry of the repository's logs through A-635's `RepositoryLogs::append`, holding the green's version and the
//!    reference naming that changeset, never its bytes (item 3.5), where the records stand as the check found them;
//! 7. the apply (`Repository::apply`), which the check made certain; the charge trued up.
//!
//! Steps 1 to 4 run in one turn and 6 and 7 in another, the landing between them; the repository's landing turn is
//! held from the first to the last, so no other landing moves the records meanwhile.
//!
//! **A record that moves no version** ([`record`], A-652 part 3): the repository's UUID, or a revision's properties set
//! whole, as an import names a repository that holds no revision but 0 by its stream's UUID and sets revision 0's
//! properties to its stream's ([`land_record`]; a lock verb's entries too). Checked against the
//! records first (a revision they hold, the room), landed on the store, checked again, its growth charged, one entry
//! holding its reference alone and moving no green, then applied.
//!
//! A refusal before the append leaves the records, the green and the logs as they were, and gives back what was
//! charged; a changeset it landed on the store is named by no entry and is taken again by the next landing. Once the
//! entry is recorded the revision is the repository's: should the green answer another version or the records refuse
//! the apply (neither met in a correct use, each counted), the records are made again from the store's changesets the
//! register names (`vcs_store::rebuild`).
//!
//! **The green's version** (item 1). Files hold Subversion's repository-normal bytes; a node's property list is named in
//! the reserved attribute [`PROPS`] by its content identity, the list itself held by the records; a copy is the engine's
//! `Copy` of the source's tree at the head, by reference (A-586), over what the destination holds with `REPLACE`
//! (A-626), and a copy from an older revision is then set back, path by path, where its source changed since
//! ([`restore`], P7's finding (1)): the green keeps no version for a copy, so it folds its history to its live readers'
//! floor after every landing ([`fold_to_readers`]); every operation is node-exclusive (A-542), so the engine judges each as touching its whole node, as FSFS
//! does. The rows are read in path order, parents first. A deletion removes its node and everything beneath it, named one
//! by one, since a node-exclusive removal of a directory needs the same increment to empty it (`Green::beneath` lists
//! what stands there; the engine drops a removed node's attributes itself). An addition without history creates its
//! node; a text the row sets replaces the node's whole (a `Delete` of the old length and an `Insert`); a list the row
//! sets or empties sets or removes the attribute. A node one row removes and a later row makes again as its own kind
//! is changed in place, since the engine refuses a creation over a node the same increment removes.
//!
//! **Not declared yet** (refused typed, [`Uncommitted::Unsupported`], counted [`UNSUPPORTED`], nothing changed; GAPS
//! §4.17.4): a directory deleted or replaced beneath a copy the same revision made from a revision older than the head,
//! whose tree no public read of the engine lists at that version; a node of another kind beneath a copy from an older
//! revision than the head holds there, a replacement by a copy of a source the head no longer holds, and an addition
//! or replacement by a row of the same revision over a path the copy sets back ([`restore`]); and a replacement without
//! history that changes a node's kind, which the engine refuses as `TypeChanged` (GAPS §8f).
//!
//! **A client's work** ([`land_work`], A-698): what both ra_svn transports call with the work their connection
//! accepted at `close-edit`, `svn://` and `svnserve -t` through the SSH tunnel sharing A-656's connection. The revision
//! is made of the work at the owner's instant (`Revision::of_work`, its `svn:date`), judged again with the commit's
//! locks at that instant, and landed; the connection answers `commit-info` with the revision, or `close-edit`'s
//! failure with the refusal ([`Uncommitted::answered`]): the step's own in FSFS's words, the others in slates' words
//! under Subversion's nearest error.
//!
//! **Owed**: the svn:// connection task's call of the step (A-656 part 2); the lock verbs' entries recorded as the
//! commits' are; the records' texts read from the green's content rather than held beside it and in the record.

use std::collections::BTreeMap;

use slates_db::catalog::{Principal, RepositoryId, VcsTool, VolumeId as DbVolumeId};
use slates_merge::engine::{Green, Increment};
use slates_merge::ops_doc::{NODE_EXCLUSIVE, Op, OpKind, OpsDoc, REPLACE};
use slates_svn::commit::drive::Work;
use slates_svn::commit::record::Record;
use slates_svn::commit::revision::{Revision, StepRefusal};
use slates_svn::index::{
  Action, ChangedPath, ContentIdentity, CopySource, NodeKind, PropertyList, Text,
};
use slates_svn::lock::{AprTime, CommitLocks};
use slates_svn::migration::{Before, Checked, Repository, before};

use super::push::{
  AppendRefused, Appended, BranchAppended, BranchChange, EntryParts, RepositoryLogs,
};
use crate::state::ShardState;

pub(crate) mod landing;
pub(crate) mod restore;
pub(crate) mod staging;

#[cfg(test)]
pub(crate) mod tests;

/// Format: the reserved attribute a Subversion repository's green names a node's property list in, by the list's
/// content identity (§4.17.4 item 1).
pub(crate) const PROPS: &str = "slates.svn.props";

/// Format: the BLAKE3 key-derivation context a revision's work identity is hashed under, over its record, so it is
/// never a work volume's or a push's.
const IDENTITY_CONTEXT: &str = "slates 2026-11-19 a Subversion revision as a work";

/// Format: the source of an operation that adds no bytes (`slates_merge::ops_doc::Op::src`'s unused value).
const NO_SOURCE: u64 = u64::MAX;

/// Format: the revision property a commit's date is (`SVN_PROP_REVISION_DATE`, `svn_props.h`).
const REVISION_DATE: &[u8] = b"svn:date";

/// Format: the revision property a commit's author is (`SVN_PROP_REVISION_AUTHOR`, `svn_props.h`).
const REVISION_AUTHOR: &[u8] = b"svn:author";

/// Counter: the history bytes a Subversion green folded after a landing, to its reachable floor (P7's finding (1),
/// [`fold_to_readers`]): the versions it no longer keeps. Format: a counter name in the status report.
pub(crate) const FOLDED_BYTES: &str = "repository.svn_commit.folded_bytes";

/// Counter: a Subversion repository whose green was not at its records' youngest revision, or answered a version other
/// than the revision's: zero in every correct use, since only the step moves the green. Format: a counter name.
pub(crate) const DIVERGED: &str = "repository.svn.diverged";

/// Counter: a revision the records refused at its apply after its entry was recorded: zero in every correct use, since
/// the check in the same turn decided it; the records are made again from the log. Format: a counter name.
pub(crate) const APPLY_REFUSED: &str = "repository.svn.apply_refused";

/// Counter: a revision whose chain entry the green chains had no room for, its green checkpointed at its floor first so
/// the entry is appended once (A-742): the non-vacuity count of that path. Format: a counter name.
pub(crate) const CHAIN_ROOM_CHECKPOINTED: &str = "repository.svn.chain_room_checkpointed";

/// Counter: a revision refused because its green version cannot be declared yet (module docs). Format: a counter name.
pub(crate) const UNSUPPORTED: &str = "repository.svn.unsupported";

/// A revision the step committed: what `commit-info` carries (`serve.c` 1555-1577).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Committed {
  /// The new revision.
  pub(crate) revision: u64,
  /// Its `svn:date`; none for an imported revision that has none.
  pub(crate) date: Option<String>,
  /// Its `svn:author`; none for an anonymous commit.
  pub(crate) author: Option<String>,
}

/// Why the step did not commit a revision. Each leaves the records, the green and the logs as they were.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Uncommitted {
  /// No Subversion repository of this id is held on this shard, its records were not made, or its green is not here or
  /// is fenced.
  NotHeld,
  /// The commit step refused the revision (A-655's `StepRefusal`), a work judged again among its reasons.
  Step(StepRefusal),
  /// The repository's green is not at its records' youngest revision (module docs; [`DIVERGED`]).
  Diverged,
  /// A row whose green version this build cannot declare yet (module docs, "Not declared yet").
  Unsupported {
    /// The row's path.
    path: String,
  },
  /// The shard's budget could not hold the records' growth: the bytes it could admit.
  Budget {
    /// The bytes available.
    available: u64,
  },
  /// The append refused (A-635's refusal): nothing applied.
  Append(AppendRefused),
  /// The node's changeset store refused the record's changeset, or the daemon holds none (C2, A-767 C6): nothing of it
  /// is in the register.
  Unstored,
}

/// Format: `SVN_ERR_FS_GENERAL`, the filesystem category's start (160,000): the error a refusal of the step's own that
/// FSFS has no closer error for is answered with (`svn_error_codes.h`).
const SVN_ERR_FS_GENERAL: u32 = 160_000;
/// Format: `SVN_ERR_FS_CORRUPT`, the filesystem category's start plus 4 (`svn_error_codes.h`).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;
/// Format: `SVN_ERR_UNSUPPORTED_FEATURE`, the miscellaneous category's start (200,000) plus 7 (`svn_error_codes.h`).
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;

impl Uncommitted {
  /// The refusal's Subversion error number and words, as a command's failure carries them (a lock verb's whose
  /// entries the step refused, A-693 part 2).
  pub(crate) fn failure(&self) -> (u32, String) {
    match self.answered() {
      slates_svn::serve::Uncommitted::Step(refusal) => {
        (refusal.subversion_code(), refusal.message())
      }
      slates_svn::serve::Uncommitted::Refused { code, message } => (code, message),
      slates_svn::serve::Uncommitted::NotServed => (
        SVN_ERR_UNSUPPORTED_FEATURE,
        "This server does not make commits yet".to_owned(),
      ),
    }
  }

  /// The refusal as an ra_svn connection answers `close-edit` with it (module docs, "A client's work"): the step's in
  /// its own words; the others, which no FSFS commit meets, in slates' words under Subversion's nearest error.
  pub(crate) fn answered(&self) -> slates_svn::serve::Uncommitted {
    use slates_svn::serve::Uncommitted as Answered;
    let (code, message) = match self {
      Uncommitted::Step(refusal) => return Answered::Step(refusal.clone()),
      Uncommitted::NotHeld => (
        SVN_ERR_FS_GENERAL,
        "The repository is not held on this shard".to_owned(),
      ),
      Uncommitted::Diverged => (
        SVN_ERR_FS_CORRUPT,
        "The repository's green is not at its youngest revision".to_owned(),
      ),
      Uncommitted::Unsupported { path } => (
        SVN_ERR_UNSUPPORTED_FEATURE,
        format!("This server cannot commit '{path}' yet"),
      ),
      Uncommitted::Budget { available } => (
        SVN_ERR_FS_GENERAL,
        format!("No room for the commit: {available} bytes available"),
      ),
      Uncommitted::Append(refused) => (
        SVN_ERR_FS_GENERAL,
        format!("The repository's log refused the commit: {refused:?}"),
      ),
      Uncommitted::Unstored => (
        SVN_ERR_FS_GENERAL,
        "The repository's store did not take the commit".to_owned(),
      ),
    };
    Answered::Refused { code, message }
  }
}

/// Lands a client's accepted `work` on `repository`'s logs by `principal` (§4.17.1 "A push's intake"; main's ruling (B)
/// of 2026-11-20): its texts put from the commit's sink `texts` into the repository's content store a copy slice a step,
/// the shard served between steps; the content made durable before any entry names it (the content plane's barrier);
/// then the revision, which names the texts, made at the owner's instant, judged again with the commit's locks then,
/// and landed through the step ([`landed_with`]: the store, then the register). Committed or refused, the content store
/// lets go every text the landing put: a committed text is its changeset's on the node's store, read through it (C2
/// stage B part 2 (b)). None where the shard's state is gone (its shutdown).
///
/// The commit is counted as its repository's intake (A-724 piece 3c, §4.17.5): live among the shard's intakes while it
/// lands (`crate::vcs_share::Live`, so other repositories' rooms are divided meanwhile), begun on the repository's usage
/// row with its principal, and answered there with its texts' bytes, learned as the repository's next intake
/// (`slates_vcs::Rates`), once it comes to an answer.
pub(crate) async fn land_work(
  (principal, repository): (&Principal, RepositoryId),
  work: Work,
  texts: &dyn slates_svn::commit::drive::TextSink,
) -> Option<Result<Committed, Uncommitted>> {
  let bytes = texts_bytes(&work);
  let _live = crate::vcs_share::Live::begin(repository);
  let _ = crate::state::with_state(|state| {
    let now = slates_rt::futures::now_ns();
    state
      .vcs_usage
      .begun(&repository, principal, slates_vcs::Work::Intake, now);
  });
  let landed = landed_work((principal, repository), work, texts).await;
  if landed.is_some() {
    let _ = crate::state::with_state(|state| {
      let now = slates_rt::futures::now_ns();
      state
        .vcs_usage
        .answered(&repository, slates_vcs::Work::Intake, bytes, now);
    });
  }
  landed
}

/// The bytes of the texts `work` made, each counted once: what the commit brought.
fn texts_bytes(work: &Work) -> u64 {
  let sizes: BTreeMap<ContentIdentity, u64> = work
    .record
    .changes
    .iter()
    .filter_map(|change| change.text.map(|text| (text.identity, text.size)))
    .filter(|(identity, _)| work.texts.contains(identity))
    .collect();
  sizes
    .values()
    .fold(0u64, |total, size| total.saturating_add(*size))
}

/// [`land_work`]'s landing, uncounted.
async fn landed_work(
  (principal, repository): (&Principal, RepositoryId),
  work: Work,
  texts: &dyn slates_svn::commit::drive::TextSink,
) -> Option<Result<Committed, Uncommitted>> {
  let begun = crate::state::with_state(|state| begin_storing(state, repository, &work))?;
  let mut storing = match begun {
    Ok(storing) => storing,
    Err(refused) => return Some(Err(refused)),
  };
  loop {
    let stepped = crate::state::with_state(|state| store_step(state, &mut storing, texts))?;
    match stepped {
      Ok(true) => break,
      Ok(false) => slates_rt::futures::yield_now().await,
      Err(refused) => {
        let _ = crate::state::with_state(|state| release_stored(state, storing));
        return Some(Err(refused));
      }
    }
  }
  // A stored text a row names that the content store let go is brought back whole for the green's increment.
  let brought = match bring_stored(repository, &work.record.changes).await? {
    Ok(brought) => brought,
    Err(refused) => {
      let _ = crate::state::with_state(|state| release_stored(state, storing));
      return Some(Err(refused));
    }
  };
  // The texts this commit put land in its changeset (C2 stage B): the store holds every text its history names, so no
  // entry names a text that is durable only in the shard's image, and every reader reads one RAM does not hold back
  // from there (C2 stage B part 2). The texts the records already held landed with the changeset that put them.
  let carried = texts_carried(&work.record.changes, &storing.put);
  // The revision made of the work at the owner's instant, judged again with the commit's locks then, checked, landed on
  // the store and committed in the register (`landing`); every text the landing put given back once it ends.
  let (turn, plans, restoring) = match planned(repository, &work.record.changes).await? {
    Ok(planned) => planned,
    Err(refused) => {
      let _ = crate::state::with_state(|state| {
        release_stored(state, storing);
        release_texts(state, None, &brought);
      });
      return Some(Err(refused));
    }
  };
  // The texts it put, let go to the node's store once the landing made them durable there (A-760 (b) 4d, main's (B)).
  let put = storing.put.clone();
  let storing = core::cell::Cell::new(Some(storing));
  let give_back = |state: &mut ShardState| {
    if let Some(storing) = storing.take() {
      release_stored(state, storing);
    }
  };
  let landed = landed_with(
    turn,
    (repository, carried),
    |state| {
      let now = AprTime::from_wall_ns(slates_vfs::clock::Clock::wall_ns(&mut state.clock));
      let locks = work.locks.clone();
      let revision = Revision::of_work(work, now).map_err(Uncommitted::Step)?;
      prepare_revision(state, repository, (revision, plans), Some((&locks, now)))
    },
    |state, reference, prepared| {
      let committed = commit_prepared(state, (principal, repository), reference, prepared);
      // Committed or refused, the content store lets go what the commit put (C2 stage B part 2 (b)): a committed
      // revision's texts are its changeset's on the node's store, read through it from now on, and the head's files
      // are the green's.
      if let Some(storing) = storing.take() {
        release_stored(state, storing);
      }
      committed
    },
    give_back,
  )
  .await;
  let _ = crate::state::with_state(|state| {
    release_texts(state, None, &brought);
    release_texts(state, None, &restoring);
    // Durable on the node's store now: RAM holds nothing of the committed texts, nor of those brought back whole for the
    // increment, the green's holds keeping their entries.
    if matches!(landed, Some(Ok(_))) {
      crate::vcs_store::name_holds::let_go_landed_texts(state, &put);
    }
    crate::vcs_store::name_holds::let_go_landed_texts(state, &brought);
    crate::vcs_store::name_holds::let_go_landed_texts(state, &restoring);
  });
  landed
}

/// Repository `repository`'s landing turn, taken for a revision whose rows are `changes`, with the restores of its
/// copies from older revisions planned under it ([`restore::plan`]) and the texts they name brought whole through the
/// node's store ([`bring_texts`]), each holding one count the caller gives back once the commit's step ran. None where
/// the shard's state is gone; refused with the turn given back and nothing held.
async fn planned(
  repository: RepositoryId,
  changes: &[ChangedPath],
) -> Option<Result<(landing::Turn, restore::CopyPlans, Vec<ContentIdentity>), Uncommitted>> {
  let turn = landing::turn(repository).await?;
  let plans = match restore::plan(repository, changes).await? {
    Ok(plans) => plans,
    Err(refused) => {
      let _ = crate::state::with_state(|state| turn.give_back(state));
      return Some(Err(refused));
    }
  };
  match bring_texts(repository, restore::texts_of(&plans).into_iter()).await? {
    Ok(restoring) => Some(Ok((turn, plans, restoring))),
    Err(refused) => {
      let _ = crate::state::with_state(|state| turn.give_back(state));
      Some(Err(refused))
    }
  }
}

/// Brings back into the content store, whole, every text `changes` name that repository `repository`'s records keep
/// stored and the content store does not hold whole (lacks, or holds with windows let go) (C2 stage B part 2 (b)): the green's increment reads each whole in the commit's
/// step (`Building::text_bytes`), a row naming one a copy or a revert. Each brought holds one count, which the caller
/// gives back once the commit's step ran ([`release_texts`]). None where the shard's state is gone; refused `Unstored`
/// where the node's store could not give one whole, and what was brought given back.
async fn bring_stored(
  repository: RepositoryId,
  changes: &[ChangedPath],
) -> Option<Result<Vec<ContentIdentity>, Uncommitted>> {
  bring_texts(repository, changes.iter().filter_map(|change| change.text)).await
}

/// [`bring_stored`] for `texts` (a copy's restore names them, `restore::texts_of`).
async fn bring_texts(
  repository: RepositoryId,
  texts: impl Iterator<Item = Text>,
) -> Option<Result<Vec<ContentIdentity>, Uncommitted>> {
  let wanted: Vec<(ContentIdentity, u64)> = crate::state::with_state(|state| {
    let records = state.vcs_stores.subversion_records(repository);
    let content = state.vcs_stores.content();
    let mut wanted: Vec<(ContentIdentity, u64)> = Vec::new();
    for text in texts {
      // Not held whole is lacking: an object whose landed windows were let go keeps its entry, and the commit's step
      // reads every byte (codex-review's cold copy of 2026-11-22: a copy from r1 refused `Unstored`).
      let lacking = records.is_some_and(|records| records.stored_len(&text.identity).is_some())
        && !content
          .is_some_and(|content| content.holds_whole(&slates_vcs::ContentId(text.identity)))
        && !wanted
          .iter()
          .any(|(identity, _)| *identity == text.identity);
      if lacking {
        wanted.push((text.identity, text.size));
      }
    }
    wanted
  })?;
  let mut brought = Vec::with_capacity(wanted.len());
  for (identity, len) in wanted {
    if !crate::vcs_store::svn_texts::bring_whole(repository, identity, len).await {
      let _ = crate::state::with_state(|state| release_texts(state, None, &brought));
      return Some(Err(Uncommitted::Unstored));
    }
    brought.push(identity);
  }
  Some(Ok(brought))
}

/// The texts of `work` that `put` names (those its landing put whole into the content store), each once, at the first
/// path a row sets it at, with its length: what its changeset carries (C2 stage B).
fn texts_carried(changes: &[ChangedPath], put: &[ContentIdentity]) -> Vec<landing::LandedText> {
  let mut carried: Vec<landing::LandedText> = Vec::with_capacity(put.len());
  for change in changes {
    let Some(text) = change.text else {
      continue;
    };
    if put.contains(&text.identity) && !carried.iter().any(|kept| kept.identity == text.identity) {
      carried.push(landing::LandedText {
        path: change.path.as_bytes().to_vec(),
        identity: text.identity,
        len: text.size,
      });
    }
  }
  carried
}

/// Lands one changeset of repository `repository` (module docs of [`landing`]) holding its landing turn `turn`, taken by
/// the caller: `prepare`d (checked, its record's bytes made) in one step; landed on the node's store, durable; then
/// `commit`ted in the register by an entry naming its sequence, in one step, where no other changeset was named
/// meanwhile. A refusal before the commit runs `undo` with what `prepare` made, where it made something. None where the
/// shard's state is gone (its shutdown).
pub(crate) async fn landed_with<P, T>(
  turn: landing::Turn,
  (repository, texts): (RepositoryId, Vec<landing::LandedText>),
  prepare: impl FnOnce(&mut ShardState) -> Result<(Vec<u8>, P), Uncommitted>,
  commit: impl FnOnce(&mut ShardState, Vec<u8>, P) -> Result<T, Uncommitted>,
  undo: impl FnOnce(&mut ShardState),
) -> Option<Result<T, Uncommitted>> {
  let prepared = crate::state::with_state(|state| {
    prepare(state).and_then(|(record, prepared)| {
      landing::landing_place(state, repository).map(|place| (record, prepared, place))
    })
  })?;
  let (record, prepared, (disk, sequence)) = match prepared {
    Ok(prepared) => prepared,
    Err(refused) => {
      crate::state::with_state(|state| {
        undo(state);
        turn.give_back(state);
      })?;
      return Some(Err(refused));
    }
  };
  let stored = landing::land_changeset(&disk, (repository, sequence), vec![record], &texts).await;
  crate::state::with_state(move |state| {
    let committed = match stored {
      // Another changeset named at this sequence meanwhile (none while the turn is held, in a correct use).
      Ok(()) if landing::next_sequence(state, repository) != sequence => {
        state.count(DIVERGED, 1);
        undo(state);
        Err(Uncommitted::Diverged)
      }
      Ok(()) => commit(state, landing::reference(sequence), prepared),
      Err(refused) => {
        undo(state);
        Err(refused)
      }
    };
    turn.give_back(state);
    committed
  })
}

/// A `revision` landed on repository `repository` by `principal`: judged again with a commit's locks at an instant
/// (`judged`), or as made (`None`, an import's, A-652 part 3, `crate::vcs_store::svn_import`); checked, landed on the
/// node's store, then committed as the green's next version ([`landed_with`]).
pub(crate) async fn land_revision(
  (principal, repository): (&Principal, RepositoryId),
  revision: Revision,
  judged: Option<(&CommitLocks, AprTime)>,
) -> Option<Result<Committed, Uncommitted>> {
  let mut revision = revision;
  // A stored text a row names that the content store let go is brought back whole for the green's increment.
  let brought = match bring_stored(repository, &revision.record.changes).await? {
    Ok(brought) => brought,
    Err(refused) => return Some(Err(refused)),
  };
  let (storing, carried) = match stage_texts(repository, &mut revision, Vec::new()).await? {
    Ok(staged) => staged,
    Err(refused) => {
      let _ = crate::state::with_state(|state| release_texts(state, None, &brought));
      return Some(Err(refused));
    }
  };
  // The texts it put, let go to the node's store once the landing made them durable there (A-760 (b) 4d, main's (B)).
  let put = storing.put.clone();
  let storing = core::cell::Cell::new(Some(storing));
  let give_back = |state: &mut ShardState| {
    if let Some(storing) = storing.take() {
      release_stored(state, storing);
    }
  };
  let (turn, plans, restoring) = match planned(repository, &revision.record.changes).await? {
    Ok(planned) => planned,
    Err(refused) => {
      let _ = crate::state::with_state(|state| {
        give_back(state);
        release_texts(state, None, &brought);
      });
      return Some(Err(refused));
    }
  };
  let landed = landed_with(
    turn,
    (repository, carried),
    |state| prepare_revision(state, repository, (revision, plans), judged),
    |state, reference, prepared| {
      let committed = commit_prepared(state, (principal, repository), reference, prepared);
      give_back(state);
      committed
    },
    give_back,
  )
  .await;
  let _ = crate::state::with_state(|state| {
    give_back(state);
    release_texts(state, None, &brought);
    release_texts(state, None, &restoring);
    // Durable on the node's store now: RAM holds nothing of the committed texts, nor of those brought back whole for the
    // increment, the green's holds keeping their entries.
    if matches!(landed, Some(Ok(_))) {
      crate::vcs_store::name_holds::let_go_landed_texts(state, &put);
    }
    crate::vcs_store::name_holds::let_go_landed_texts(state, &brought);
    crate::vcs_store::name_holds::let_go_landed_texts(state, &restoring);
  });
  landed
}

/// Stages the texts `revision` brings for its landing (C2 stage B part 2 (b)): each taken out of its contents and named
/// stored ([`take_texts`]), put through repository `repository`'s content store a copy slice a step, as a client's
/// commit's are, so its changeset carries them as Payload with Texts rows and the records keep none in RAM; and the
/// texts an import's sink put already, `imported`, each holding one count of the store (`vcs_store::svn_import::texts`),
/// taken as put ([`hand_over`]). What was put, held until the landing ends ([`release_stored`]), and what the changeset
/// carries. None where the shard's state is gone; refused with what was put given back, `imported` too.
async fn stage_texts(
  repository: RepositoryId,
  revision: &mut Revision,
  imported: Vec<ContentIdentity>,
) -> Option<Result<(Storing, Vec<landing::LandedText>), Uncommitted>> {
  let texts = crate::state::with_state(|state| take_texts(state, repository, revision))?;
  let begun = crate::state::with_state(|state| {
    match storing_of(state, repository, (&revision.record.changes, texts.keys())) {
      Ok(mut storing) => {
        hand_over(state, repository, revision, imported, &mut storing);
        Ok(storing)
      }
      Err(refused) => {
        release_texts(state, None, &imported);
        Err(refused)
      }
    }
  })?;
  let mut storing = match begun {
    Ok(storing) => storing,
    Err(refused) => return Some(Err(refused)),
  };
  // Each piece lent from the text the landing holds, never copied out (P7's finding (2)).
  let piece = |identity: &ContentIdentity, offset: u64, len: usize| {
    let bytes = texts.get(identity)?;
    let start = usize::try_from(offset).ok()?;
    bytes
      .get(start..start.saturating_add(len).min(bytes.len()))
      .map(std::borrow::Cow::Borrowed)
  };
  loop {
    let stepped = crate::state::with_state(|state| store_step_from(state, &mut storing, &piece))?;
    match stepped {
      Ok(true) => break,
      Ok(false) => slates_rt::futures::yield_now().await,
      Err(refused) => {
        let _ = crate::state::with_state(|state| release_stored(state, storing));
        return Some(Err(refused));
      }
    }
  }
  // The bytes are the content store's now, until the landing ends.
  drop(texts);
  let carried = texts_carried(&revision.record.changes, &storing.put);
  Some(Ok((storing, carried)))
}

/// Takes the holds an import's sink took on the texts `revision` names stored, `imported`, as texts `storing` put, one
/// each; gives back every other: one the records hold already, one sealed twice, one the revision does not name.
fn hand_over(
  state: &mut ShardState,
  repository: RepositoryId,
  revision: &Revision,
  imported: Vec<ContentIdentity>,
  storing: &mut Storing,
) {
  let mut spare = Vec::new();
  for identity in imported {
    let held = state
      .vcs_stores
      .subversion_records(repository)
      .is_none_or(|records| records.holds(&identity));
    if revision.stored.contains_key(&identity) && !held && !storing.put.contains(&identity) {
      storing.put.push(identity);
    } else {
      spare.push(identity);
    }
  }
  release_texts(state, None, &spare);
}

/// The texts of `revision` repository `repository`'s records do not hold, taken out and named stored
/// (`Repository::take_texts`): the texts to land in its changeset.
fn take_texts(
  state: &ShardState,
  repository: RepositoryId,
  revision: &mut Revision,
) -> BTreeMap<ContentIdentity, Vec<u8>> {
  state
    .vcs_stores
    .subversion_records(repository)
    .map(|records| records.take_texts(revision))
    .unwrap_or_default()
}

/// A revision checked for its landing ([`prepare_revision`]): the check, the work identity of its record, the room
/// its step was lent, and the records it was checked against (their generation and youngest revision), which its
/// commit finds unchanged.
#[derive(Debug)]
pub(crate) struct Prepared {
  /// The check.
  checked: Checked,
  /// Its record's work identity, the green's version's.
  identity: [u8; 32],
  /// The bytes the revision brings.
  brought: u64,
  /// The records' generation and youngest revision at the check.
  stamp: Option<(u64, u64)>,
  /// The restores of its copies from older revisions, planned holding the landing turn ([`restore::plan`]).
  plans: restore::CopyPlans,
}

/// The records' generation and youngest revision: what a prepared revision's commit finds unchanged.
fn stamp(state: &ShardState, repository: RepositoryId) -> Option<(u64, u64)> {
  match state.vcs_stores.store(repository)? {
    crate::vcs_store::RepositoryStore::Subversion(store) => store
      .records
      .as_ref()
      .map(|records| (store.generation, records.youngest())),
    _ => None,
  }
}

/// `revision` checked on `repository`'s records, a client's work judged again (`judged`), an imported revision as made:
/// its record's bytes, which land on the store, and what its commit takes ([`Prepared`]). Changes nothing.
fn prepare_revision(
  state: &mut ShardState,
  repository: RepositoryId,
  (revision, plans): (Revision, restore::CopyPlans),
  judged: Option<(&CommitLocks, AprTime)>,
) -> Result<(Vec<u8>, Prepared), Uncommitted> {
  green_of(state, repository)?;
  let brought = brought(&revision);
  if !crate::vcs_store::lend_svn(state, repository, brought) {
    return Err(Uncommitted::NotHeld);
  }
  let checked = checked(state, repository, revision, judged);
  crate::vcs_store::settle_svn(state, repository);
  let (checked, record) = checked?;
  let prepared = Prepared {
    checked,
    identity: identity(&record),
    brought,
    stamp: stamp(state, repository),
    plans,
  };
  Ok((record, prepared))
}

/// Commits a prepared revision (module docs, "The step") once its changeset `reference` names is durable on the store:
/// the green's next version and the reference one entry of the register, then applied. Refused `Diverged` where the
/// records moved since its check.
fn commit_prepared(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  reference: Vec<u8>,
  prepared: Prepared,
) -> Result<Committed, Uncommitted> {
  let green = green_of(state, repository)?;
  if stamp(state, repository) != prepared.stamp {
    state.count(DIVERGED, 1);
    return Err(Uncommitted::Diverged);
  }
  if !crate::vcs_store::lend_svn(state, repository, prepared.brought) {
    return Err(Uncommitted::NotHeld);
  }
  let landed = land(
    state,
    (principal, repository),
    green,
    (prepared.checked, prepared.identity, reference),
    prepared.plans,
  );
  crate::vcs_store::settle_svn(state, repository);
  landed
}

/// The refusal of records that do not hold together.
fn corrupt() -> Uncommitted {
  Uncommitted::Step(StepRefusal::Corrupt)
}

/// A commit's texts being put into the repository's content store from its sink, a slice a step, before its revision
/// lands (§4.17.1 "A push's intake"; main's ruling (B) of 2026-11-20): the texts still to put, the one being put, and
/// those put whole, each holding one count of the store until the landing ends, committed or refused
/// ([`release_stored`]): the changeset streams them to the node's store from there, and the records name them by
/// identity and length, read through that store (C2 stage B part 2 (b)). Until the sink writes into the store itself
/// (w-perf's wiring), this is where a text's bytes enter it.
#[derive(Debug, Default)]
pub(crate) struct Storing {
  /// The texts still to put, each with its length.
  queue: std::collections::VecDeque<(ContentIdentity, u64)>,
  /// The text being put: its identity, its length, its writer and the bytes put so far.
  current: Option<(ContentIdentity, u64, slates_vcs::Writing, u64)>,
  /// The texts put whole, each holding one count of the store.
  put: Vec<ContentIdentity>,
}

/// What `work` brings that the repository's records do not hold: its texts to put ([`store_step`]).
pub(crate) fn begin_storing(
  state: &ShardState,
  repository: RepositoryId,
  work: &Work,
) -> Result<Storing, Uncommitted> {
  storing_of(state, repository, (&work.record.changes, &work.texts))
}

/// The texts of `texts` that `changes` name and repository `repository`'s records do not hold, to put
/// ([`store_step`]).
fn storing_of<'t>(
  state: &ShardState,
  repository: RepositoryId,
  (changes, texts): (
    &[ChangedPath],
    impl IntoIterator<Item = &'t ContentIdentity>,
  ),
) -> Result<Storing, Uncommitted> {
  let records = state
    .vcs_stores
    .subversion_records(repository)
    .ok_or(Uncommitted::NotHeld)?;
  let lengths: BTreeMap<ContentIdentity, u64> = changes
    .iter()
    .filter_map(|change| change.text.map(|text| (text.identity, text.size)))
    .collect();
  let mut queue = std::collections::VecDeque::new();
  for identity in texts {
    if records.holds(identity) {
      continue;
    }
    let len = lengths.get(identity).copied().ok_or_else(corrupt)?;
    queue.push_back((*identity, len));
  }
  Ok(Storing {
    queue,
    ..Storing::default()
  })
}

/// One step of putting a commit's texts into the store: at most the shard's copy slice of the text being put, read from
/// `texts` (the commit's sink) and kept a window at a time, admitted against the shard's room and charged as kept.
/// Whether every text is put. A text whose bytes are not the identity its commit named is refused `Corrupt`.
///
/// # Errors
/// [`Uncommitted::Budget`] past the shard's room; [`Uncommitted::NotHeld`] with no content store; `Corrupt` for a text
/// the sink does not hold or whose bytes are not its identity.
pub(crate) fn store_step(
  state: &mut ShardState,
  storing: &mut Storing,
  texts: &dyn slates_svn::commit::drive::TextSink,
) -> Result<bool, Uncommitted> {
  store_step_from(state, storing, &|identity, offset, len| {
    texts
      .sealed_piece(identity, offset, len)
      .map(std::borrow::Cow::Owned)
  })
}

/// A reader of a text's pieces: a text, an offset, at most a length; its bytes there, lent where the reader holds the
/// text (an import's landing: no copy, P7's finding (2)), or none.
type PieceOf<'a> = dyn Fn(&ContentIdentity, u64, usize) -> Option<std::borrow::Cow<'a, [u8]>> + 'a;

/// [`store_step`] reading each piece from `piece` (a text, an offset, at most a length).
fn store_step_from(
  state: &mut ShardState,
  storing: &mut Storing,
  piece: &PieceOf<'_>,
) -> Result<bool, Uncommitted> {
  let slice = usize::try_from(state.config.copy_slice_bytes())
    .unwrap_or(usize::MAX)
    .max(1);
  // The text's writer stays in `storing` until its object is finished, so a refusal at any step (a piece the reader
  // lacks, a window or the finish refused) leaves it where the landing's release abandons it (`release_stored`) with
  // the windows it kept. Taken out first, a refusal dropped it with them still held.
  if storing.current.is_none() {
    let Some((identity, len)) = storing.queue.pop_front() else {
      return Ok(true);
    };
    // Begun with its length, the text's window buffer is reserved once rather than doubled a copy slice at a time
    // (goal 93, svn finding (2)).
    let store = state.vcs_stores.content().ok_or(Uncommitted::NotHeld)?;
    storing.current = Some((identity, len, store.begin_object_of_length(len), 0));
  }
  let Some((identity, len, writing, sent)) = storing.current.as_mut() else {
    return Ok(true);
  };
  let (identity, len) = (*identity, *len);
  let piece = piece(&identity, *sent, slice).ok_or_else(corrupt)?;
  let piece_len = u64::try_from(piece.len()).unwrap_or(u64::MAX);
  let finished = crate::vcs_store::within_budget(state, piece_len, |stores| {
    let store = stores.content_mut().ok_or(Uncommitted::NotHeld)?;
    store
      .put_window(writing, &piece)
      .map_err(|refused| store_refused(&refused))?;
    *sent = sent.saturating_add(piece_len);
    if *sent < len && !piece.is_empty() {
      return Ok(false);
    }
    let made = store
      .finish_object(writing)
      .map_err(|refused| store_refused(&refused))?;
    if made != slates_vcs::ContentId(identity) {
      let _ = store.release(&made);
      return Err(corrupt());
    }
    Ok(true)
  })?;
  if finished {
    storing.current = None;
    storing.put.push(identity);
  }
  Ok(storing.queue.is_empty() && storing.current.is_none())
}

/// Reads every run `work` names (A-760) from the shard's content store into its own buffer, in this one step: its bound
/// is the copy the post-state made of the same texts before, a copy slice a turn coming with the batch's turns (main
/// 01a20182's ruling of 2026-11-22 ~11:35). Counted ([`NAMED_READ_BYTES`]). Refused `Unstored` where the store does
/// not hold a text it names: the landing brings back every stored text the content store let go before it builds
/// (`bring_stored`).
pub(crate) fn read_named(state: &mut ShardState, work: &mut Increment) -> Result<(), Uncommitted> {
  if work.unread().is_none() {
    return Ok(());
  }
  let total = work
    .named
    .runs
    .iter()
    .fold(0u64, |total, run| total.saturating_add(run.len));
  {
    let store = state.vcs_stores.content().ok_or(Uncommitted::NotHeld)?;
    let objects = crate::vcs_store::named_objects::PlaneObjects {
      store,
      kept_in: &state.store.content,
    };
    while !work
      .read_step(&objects, total.max(1))
      .map_err(|_| Uncommitted::Unstored)?
    {}
  }
  state.count(NAMED_READ_BYTES, total);
  Ok(())
}

/// Counter: the bytes of the texts a Subversion increment names (A-760) read from the content store into its runs'
/// buffers before it is judged ([`read_named`]): once a byte of each text a landing builds. Format: a counter name in
/// the status report.
pub(crate) const NAMED_READ_BYTES: &str = "repository.svn_commit.named_read_bytes";

/// The commit step's answer to a content store's refusal: past the shard's room, `Budget`; anything else the store
/// refuses, `Corrupt`.
fn store_refused(refused: &slates_vcs::ContentRefused) -> Uncommitted {
  match *refused {
    slates_vcs::ContentRefused::OverBudget { room, .. } => Uncommitted::Budget { available: room },
    _ => corrupt(),
  }
}

/// Gives back every text `storing` put whole and the one it was putting: a landing that ended, committed or refused.
pub(crate) fn release_stored(state: &mut ShardState, storing: Storing) {
  let Storing { current, put, .. } = storing;
  release_texts(state, current.map(|(_, _, writing, _)| writing), &put);
}

/// Gives back one store hold of each of `texts`, and abandons `writing`'s object.
pub(crate) fn release_texts(
  state: &mut ShardState,
  writing: Option<slates_vcs::Writing>,
  texts: &[ContentIdentity],
) {
  // A free decision: the greens' holds on their named texts taken first (A-760 (b) 4d), so a text a green named in
  // this turn outlives the landing's own hold given back here.
  crate::vcs_store::name_holds::freeing(state, |store| {
    let Some(store) = store else {
      return;
    };
    if let Some(writing) = writing {
      store.abandon_object(writing);
    }
    for identity in texts {
      let _ = store.release(&slates_vcs::ContentId(*identity));
    }
  });
}

/// The step for a record that moves no version (module docs; A-652 part 3): `record`, the repository's UUID, a
/// revision's properties set whole or a lock verb's entries, landed on the node's store and committed
/// on `repository`'s register by `principal`, its owner ([`landed_with`]). A creation or a revision is refused `Corrupt`: a
/// creation is made once, by `CreateRepository`, and a revision moves the green ([`land_revision`]).
pub(crate) async fn land_record(
  (principal, repository): (&Principal, RepositoryId),
  record: Record,
) -> Option<Result<(), Uncommitted>> {
  let turn = landing::turn(repository).await?;
  land_record_with(turn, (principal, repository), record).await
}

/// [`land_record`] holding repository `repository`'s landing turn `turn` already: what a verb that judges its record
/// over the records first takes, holding the turn from its judgment (a lock verb's entries, A-693 part 2; a revision
/// property's change, A-702 part 2), so no other landing moves what it judged before its record lands.
pub(crate) async fn land_record_with(
  turn: landing::Turn,
  (principal, repository): (&Principal, RepositoryId),
  record: Record,
) -> Option<Result<(), Uncommitted>> {
  landed_with(
    turn,
    (repository, Vec::new()),
    |state| record_prepared(state, repository, record),
    |state, reference, record| record_committed(state, (principal, repository), reference, record),
    |_| {},
  )
  .await
}

/// `record` checked for its landing: its bytes, which land on the store, and the record its commit applies.
fn record_prepared(
  state: &mut ShardState,
  repository: RepositoryId,
  record: Record,
) -> Result<(Vec<u8>, Record), Uncommitted> {
  record_checked(state, repository, &record)?;
  Ok((record.encode(), record))
}

/// Commits `record` once its changeset `reference` names is durable on the store: checked again where it is committed
/// (what it checks is the records', which only a landing moves), its growth charged, the reference one entry of the
/// register, then applied.
fn record_committed(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  reference: Vec<u8>,
  record: Record,
) -> Result<(), Uncommitted> {
  let brought = record_checked(state, repository, &record)?;
  if !crate::vcs_store::lend_svn(state, repository, brought) {
    return Err(Uncommitted::NotHeld);
  }
  let landed = record_growth(state, repository, &record)
    .and_then(|growth| append_record(state, (principal, repository), (record, reference), growth));
  crate::vcs_store::settle_svn(state, repository);
  landed
}

/// `record` checked against `repository`'s records as its commit checks it, lent the room it brings: the bytes it
/// brings. Changes nothing.
fn record_checked(
  state: &mut ShardState,
  repository: RepositoryId,
  record: &Record,
) -> Result<u64, Uncommitted> {
  green_of(state, repository)?;
  let brought = match record {
    Record::RevisionProperties { properties, .. } => {
      bytes_of(properties.iter(), core::iter::empty::<&Vec<u8>>())
    }
    Record::Uuid(_) | Record::Locks(_) => 0,
    Record::Created { .. } | Record::Revision(_) => return Err(corrupt()),
  };
  if !crate::vcs_store::lend_svn(state, repository, brought) {
    return Err(Uncommitted::NotHeld);
  }
  let checked = record_growth(state, repository, record);
  crate::vcs_store::settle_svn(state, repository);
  checked.map(|_| brought)
}

/// The bytes `record` adds to the records once applied, checked against them as its apply checks it: none for a UUID.
fn record_growth(
  state: &ShardState,
  repository: RepositoryId,
  record: &Record,
) -> Result<u64, Uncommitted> {
  let records = state
    .vcs_stores
    .subversion_records(repository)
    .ok_or(Uncommitted::NotHeld)?;
  match record {
    Record::Uuid(_) => Ok(0),
    // A lock verb's entries, judged by the lock table as the records apply them (`check_entries`).
    Record::Locks(entries) => records
      .locks()
      .check_entries(entries)
      .map(|()| 0)
      .map_err(|refusal| Uncommitted::Step(StepRefusal::Lock(refusal))),
    Record::RevisionProperties {
      revision,
      properties,
    } => records
      .check_revision_properties(*revision, properties)
      .map_err(Uncommitted::Step),
    Record::Created { .. } | Record::Revision(_) => Err(corrupt()),
  }
}

/// Appends the `reference` to `record`'s changeset alone through A-635's append, its `growth` charged first; then
/// applies the record. An append that records nothing is answered `Diverged`, counted, since the entry holds a
/// reference and so is always recorded; an apply refused once the entry recorded it is counted and the records are
/// made again from the store.
fn append_record(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  (record, reference): (Record, Vec<u8>),
  growth: u64,
) -> Result<(), Uncommitted> {
  crate::vcs_store::reserve_svn(state, repository, growth)
    .map_err(|available| Uncommitted::Budget { available })?;
  let parts = EntryParts {
    objects: reference,
    ..EntryParts::default()
  };
  match RepositoryLogs::append(state, principal, repository, Vec::new(), parts) {
    Ok(appended) if appended.recorded => {}
    Ok(_) => {
      crate::vcs_store::unreserve_svn(state, repository, growth);
      state.count(DIVERGED, 1);
      return Err(Uncommitted::Diverged);
    }
    Err(refused) => {
      crate::vcs_store::unreserve_svn(state, repository, growth);
      return Err(Uncommitted::Append(refused));
    }
  }
  let applied = state
    .vcs_stores
    .subversion(repository)
    .and_then(|svn| svn.records.as_mut())
    .map(|records| records.commit_record(record));
  if applied != Some(Ok(())) {
    state.count(APPLY_REFUSED, 1);
    crate::vcs_store::rebuild(state, repository);
  }
  Ok(())
}

/// A revision the server made of a version a work submitted to `repository`'s green (A-664's Subversion objects,
/// `crate::vcs_store::svn_made`), made while the repository's landing turn `turn` is held: its texts staged as an
/// import's are ([`stage_texts`], the records keeping none in RAM), checked as an import's is, landed on the node's
/// store, then committed by `commit` ([`MadeEntry::commit`]: an entry of the register naming it alone, then applied;
/// [`landed_with`]): in the step that commits the version too (`crate::made_commit`), or alone after a version committed
/// already (a bare dispatch's). What `commit` answered.
pub(crate) async fn land_made<T>(
  turn: landing::Turn,
  repository: RepositoryId,
  revision: Revision,
  commit: impl FnOnce(&mut ShardState, MadeEntry) -> Result<T, Uncommitted>,
) -> Option<Result<T, Uncommitted>> {
  let mut revision = revision;
  let (storing, carried) = match stage_texts(repository, &mut revision, Vec::new()).await? {
    Ok(staged) => staged,
    Err(refused) => {
      let _ = crate::state::with_state(|state| turn.give_back(state));
      return Some(Err(refused));
    }
  };
  // The texts it put, let go to the node's store once the landing made them durable there (A-760 (b) 4d, main's (B)).
  let put = storing.put.clone();
  let storing = core::cell::Cell::new(Some(storing));
  let give_back = |state: &mut ShardState| {
    if let Some(storing) = storing.take() {
      release_stored(state, storing);
    }
  };
  let landed = landed_with(
    turn,
    (repository, carried),
    |state| {
      prepare_revision(
        state,
        repository,
        (revision, restore::CopyPlans::new()),
        None,
      )
    },
    |state, reference, prepared| {
      let entry = MadeEntry {
        repository,
        reference,
        prepared,
      };
      let committed = commit(state, entry);
      give_back(state);
      committed
    },
    give_back,
  )
  .await;
  let _ = crate::state::with_state(|state| {
    give_back(state);
    // Durable on the node's store now: RAM holds nothing of the committed texts, the green's holds keep their entries.
    if matches!(landed, Some(Ok(_))) {
      crate::vcs_store::name_holds::let_go_landed_texts(state, &put);
    }
  });
  landed
}

/// A made revision landed on the node's store, its register entry not yet committed: what [`land_made`]'s caller
/// commits, in its own step's transaction where it has one (`crate::made_commit`).
pub(crate) struct MadeEntry {
  /// The repository.
  repository: RepositoryId,
  /// The reference naming the landed changeset's sequence.
  reference: Vec<u8>,
  /// The revision, checked.
  prepared: Prepared,
}

impl MadeEntry {
  /// Commits the entry naming the landed changeset ([`commit_made`]): the revision's number.
  pub(crate) fn commit(self, state: &mut ShardState) -> Result<u64, Uncommitted> {
    commit_made(state, self.repository, self.reference, self.prepared)
  }
}

/// [`land_made`]'s commit: where the records are as its check found them, the reference admitted and the revision's
/// growth charged, the entry appended as its repository's owner, then the records applied; an apply refused once the
/// entry recorded it is counted and the records are made again from the store.
fn commit_made(
  state: &mut ShardState,
  repository: RepositoryId,
  reference: Vec<u8>,
  prepared: Prepared,
) -> Result<u64, Uncommitted> {
  if stamp(state, repository) != prepared.stamp {
    state.count(DIVERGED, 1);
    return Err(Uncommitted::Diverged);
  }
  let owner = state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .map(|record| record.owner.clone())
    .ok_or(Uncommitted::NotHeld)?;
  if !crate::vcs_store::lend_svn(state, repository, prepared.brought) {
    return Err(Uncommitted::NotHeld);
  }
  let made = append_made(state, (&owner, repository), (prepared.checked, reference));
  crate::vcs_store::settle_svn(state, repository);
  made
}

/// [`commit_made`]'s entry and apply.
fn append_made(
  state: &mut ShardState,
  (owner, repository): (&Principal, RepositoryId),
  (checked, reference): (Checked, Vec<u8>),
) -> Result<u64, Uncommitted> {
  crate::vcs_store::admit_objects(state, reference.len())
    .map_err(|available| Uncommitted::Budget { available })?;
  let growth = grown(&checked);
  crate::vcs_store::reserve_svn(state, repository, growth)
    .map_err(|available| Uncommitted::Budget { available })?;
  let parts = EntryParts {
    objects: reference,
    ..EntryParts::default()
  };
  match RepositoryLogs::append(state, owner, repository, Vec::new(), parts) {
    Ok(appended) if appended.recorded => {}
    Ok(_) => {
      crate::vcs_store::unreserve_svn(state, repository, growth);
      state.count(DIVERGED, 1);
      return Err(Uncommitted::Diverged);
    }
    Err(refused) => {
      crate::vcs_store::unreserve_svn(state, repository, growth);
      return Err(Uncommitted::Append(refused));
    }
  }
  let revision = checked.revision();
  let applied = apply_holding(state, repository, checked);
  if applied != Some(Ok(revision)) {
    state.count(APPLY_REFUSED, 1);
    crate::vcs_store::rebuild(state, repository);
  }
  Ok(revision)
}

/// Applies `checked` to repository `repository`'s records and follows it in the green's held table (S4 c1,
/// `vcs_store::svn_texts`): the texts its paths held before let go, those they hold after held by path. What the apply
/// answered; none when the records are not held here.
fn apply_holding(
  state: &mut ShardState,
  repository: RepositoryId,
  checked: Checked,
) -> Option<Result<u64, StepRefusal>> {
  let changes = checked.changes().to_vec();
  let records = state.vcs_stores.subversion_records(repository)?;
  let released =
    crate::vcs_store::svn_texts::released_before(records, &changes, records.youngest());
  let applied = state
    .vcs_stores
    .subversion(repository)
    .and_then(|svn| svn.records.as_mut())
    .map(|records| records.apply(checked))?;
  if let (Ok(revision), Ok(green)) = (&applied, green_of(state, repository)) {
    let held = state
      .vcs_stores
      .subversion_records(repository)
      .map(|records| crate::vcs_store::svn_texts::held_after(records, &changes, *revision))
      .unwrap_or_default();
    crate::vcs_store::svn_texts::follow(state, green, &released, &held);
  }
  Some(applied)
}

/// The repository's one green (item 1): its default branch's, here and not fenced. `NotHeld` otherwise, and for a
/// repository not here or not Subversion's.
pub(crate) fn green_of(
  state: &ShardState,
  repository: RepositoryId,
) -> Result<DbVolumeId, Uncommitted> {
  let repositories = state.db.partition().repositories();
  let record = repositories
    .repository(repository)
    .ok_or(Uncommitted::NotHeld)?;
  if !matches!(record.tool, VcsTool::Subversion) {
    return Err(Uncommitted::NotHeld);
  }
  let name = record
    .default_branch
    .as_deref()
    .ok_or(Uncommitted::NotHeld)?;
  let green = repositories
    .branch(repository, name)
    .ok_or(Uncommitted::NotHeld)?;
  if state.fenced_greens.contains_key(&green) || !state.greens.contains_key(&green) {
    return Err(Uncommitted::NotHeld);
  }
  Ok(green)
}

/// The bytes of a revision's properties and of `contents`' values, saturating.
fn bytes_of<'v>(
  properties: impl Iterator<Item = (&'v Vec<u8>, &'v Vec<u8>)>,
  contents: impl Iterator<Item = &'v Vec<u8>>,
) -> u64 {
  let total = properties
    .map(|(name, value)| name.len().saturating_add(value.len()))
    .chain(contents.map(Vec::len))
    .fold(0usize, usize::saturating_add);
  u64::try_from(total).unwrap_or(u64::MAX)
}

/// The bytes `revision` brings: the room its step is lent.
fn brought(revision: &Revision) -> u64 {
  bytes_of(revision.properties.iter(), revision.contents.values())
}

/// The bytes the records hold more once `checked` is applied: its properties and the contents they lacked.
fn grown(checked: &Checked) -> u64 {
  bytes_of(checked.properties().iter(), checked.fresh().values())
}

/// The revision as the step commits it, checked, with its record: a client's work judged again (`judged`), an
/// imported revision as made; then the check, which changes nothing.
fn checked(
  state: &ShardState,
  repository: RepositoryId,
  revision: Revision,
  judged: Option<(&CommitLocks, AprTime)>,
) -> Result<(Checked, Vec<u8>), Uncommitted> {
  let records = state
    .vcs_stores
    .subversion_records(repository)
    .ok_or(Uncommitted::NotHeld)?;
  let revision = match judged {
    Some((locks, now)) => records
      .judge_again(revision, locks, now)
      .map_err(Uncommitted::Step)?,
    None => revision,
  };
  let record = Record::revision_bytes(&revision);
  let checked = records.check(revision).map_err(Uncommitted::Step)?;
  Ok((checked, record))
}

/// Appends the checked revision through A-635's append, the green's version and the reference to its changeset on the
/// store one entry, its growth charged first; then applies it.
fn land(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  green: DbVolumeId,
  (checked, identity, reference): (Checked, [u8; 32], Vec<u8>),
  plans: restore::CopyPlans,
) -> Result<Committed, Uncommitted> {
  let work = version_of(state, (repository, green), (&checked, plans), identity)?;
  // The reference is the repository's content in the partition's register, charged to this shard's memory budget
  // (`vcs_store::settle_objects`).
  crate::vcs_store::admit_objects(state, reference.len())
    .map_err(|available| Uncommitted::Budget { available })?;
  let growth = grown(&checked);
  crate::vcs_store::reserve_svn(state, repository, growth)
    .map_err(|available| Uncommitted::Budget { available })?;
  // The chain entry's bound (its copies' fixes' room with it, A-626), known before the append: where the chains have
  // less room than that, the green is checkpointed at its floor first, as a push's admission meets the cap (A-238),
  // and the entry appended once. Before A-742 the entry was appended, refused at the cap, and appended again from a
  // copy of its parts kept for that, a revision's bytes copied at every commit (201 MiB in a 1,000-revision import).
  let room = state
    .greens
    .get(&green)
    .map_or(0, |engine| engine.copy_entry_room(&work));
  let bound = slates_merge::chain_entry::entry_bound(&work).saturating_add(room);
  if state.db.partition().green_chain_room() < bound {
    state.count(CHAIN_ROOM_CHECKPOINTED, 1);
    crate::merge_service::checkpoint_green_when(state, green, crate::merge_service::Doubling::Now);
  }
  let parts = EntryParts {
    objects: reference,
    ..EntryParts::default()
  };
  let change = BranchChange::Update { green, work };
  let appended = RepositoryLogs::append(state, principal, repository, vec![change], parts);
  match appended {
    Ok(appended) => {
      // Fold the green's chain by the merge service's doubling rule as every accepted version does (W3), so a
      // repository's chain stays bounded however many revisions it takes.
      crate::merge_service::checkpoint_green(state, green);
      fold_to_readers(state, green);
      Ok(applied(state, repository, checked, &appended))
    }
    Err(refused) => {
      crate::vcs_store::unreserve_svn(state, repository, growth);
      Err(Uncommitted::Append(refused))
    }
  }
}

/// Folds a Subversion green's history to its reachable floor and settles its retention charge (P7's finding (1)): no copy
/// reads a version it kept (`restore`: a copy from an older revision is made from the head and set back from the
/// records), and svn's reads are the records', so only a live reader's version (a work's base, a pinned attachment, a
/// mount) is kept. Before, it folded only past its retention budget, so git-2000's import kept 440 MB of versions.
pub(crate) fn fold_to_readers(state: &mut ShardState, green: DbVolumeId) {
  let floor = crate::merge_service::reachable_floor(state, green);
  let folded = state
    .greens
    .get_mut(&green)
    .map_or(0, |mut engine| engine.fold_history_before(floor));
  if folded > 0 {
    state.count(FOLDED_BYTES, u64::try_from(folded).unwrap_or(u64::MAX));
  }
  // A charge the budget cannot take leaves the fold, which only releases, and is charged again at the next settle.
  let _ = crate::merge_service::settle_green_retention(state, green);
}

/// The records once the entry recorded the revision: applied, as the check made certain; the answer `commit-info`
/// carries. A green answering another version, or an apply refused, is counted, and the records are made again from
/// the log, which holds the revision.
fn applied(
  state: &mut ShardState,
  repository: RepositoryId,
  checked: Checked,
  appended: &Appended,
) -> Committed {
  let answer = answer(&checked);
  let moved = matches!(
    appended.changes.as_slice(),
    [BranchAppended::Moved { version, before: false, .. }] if *version == answer.revision
  );
  let made = if moved {
    apply_holding(state, repository, checked)
  } else {
    None
  };
  if made != Some(Ok(answer.revision)) {
    state.count(if moved { APPLY_REFUSED } else { DIVERGED }, 1);
    crate::vcs_store::rebuild(state, repository);
  }
  answer
}

/// What `commit-info` carries of `checked`: its revision, its date and its author.
fn answer(checked: &Checked) -> Committed {
  let text = |name: &[u8]| {
    checked
      .properties()
      .get(name)
      .and_then(|value| std::str::from_utf8(value).ok())
      .map(str::to_owned)
  };
  Committed {
    revision: checked.revision(),
    date: text(REVISION_DATE),
    author: text(REVISION_AUTHOR),
  }
}

/// The green's next version (module docs): the increment moving the green from its head, which must be the records'
/// youngest revision, to the revision; its identity the record's.
fn version_of(
  state: &mut ShardState,
  (repository, green): (RepositoryId, DbVolumeId),
  (checked, plans): (&Checked, restore::CopyPlans),
  identity: [u8; 32],
) -> Result<Increment, Uncommitted> {
  // Built over the records and the green alone: a text the commit stored is named (A-760), read after
  // ([`read_named`]).
  let built = crate::vcs_store::lent_state(state, |state| {
    let records = state
      .vcs_stores
      .subversion_records(repository)
      .ok_or(Uncommitted::NotHeld)?;
    let engine = state.greens.get(&green).ok_or(Uncommitted::NotHeld)?;
    let head = records.youngest();
    if engine.head() == head {
      Building::new(records, &engine, (checked, plans)).build(head, identity)
    } else {
      Err(Uncommitted::Diverged)
    }
  });
  match &built {
    Err(Uncommitted::Diverged) => state.count(DIVERGED, 1),
    Err(Uncommitted::Unsupported { .. }) => state.count(UNSUPPORTED, 1),
    _ => {}
  }
  // The texts it names read before it is judged (A-760: the engine refuses an unread run).
  let mut built = built?;
  read_named(state, &mut built)?;
  Ok(built)
}

/// The work identity of the revision whose record is `record` (module docs): one revision's alone, as its record is.
fn identity(record: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(IDENTITY_CONTEXT);
  hasher.update(record);
  *hasher.finalize().as_bytes()
}

/// A Subversion path's place in its green: the path without its leading `/`, the root the empty path.
fn green_path(path: &str) -> &str {
  path.strip_prefix('/').unwrap_or(path)
}

/// The green path standing at `found`'s place beneath `from`, beneath `to` instead.
fn moved(found: &str, from: &str, to: &str) -> Option<String> {
  let rest = if from.is_empty() {
    found
  } else {
    found.strip_prefix(from)?.strip_prefix('/')?
  };
  Some(if to.is_empty() {
    rest.to_owned()
  } else {
    format!("{to}/{rest}")
  })
}

/// What stood at a path before the revision changed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stood {
  /// Whether a directory stood there.
  directory: bool,
  /// Its text's length, for a file.
  len: u64,
  /// Whether it held a property list.
  props: bool,
}

/// The increment under construction (module docs), as the deriver lays one out: paths interned, bytes in the post-state
/// by offset, every operation node-exclusive.
struct Building<'b> {
  /// The repository's records, at the revision's head.
  records: &'b Repository,
  /// The repository's green, at the same head.
  engine: &'b Green,
  /// The revision, checked.
  checked: &'b Checked,
  /// Its rows, by path.
  rows: BTreeMap<&'b str, &'b ChangedPath>,
  /// The document.
  doc: OpsDoc,
  /// The post-state's inline bytes: the property lists' identities, and a text the revision or the records hold
  /// themselves.
  post: Vec<u8>,
  /// The texts stored in the content store that the increment names rather than carries (A-760), each whole, in the
  /// order their ops were made.
  runs: Vec<slates_merge::named::NamedRun>,
  /// The named runs' bytes so far: where the next run starts, counted from the first run's start.
  runs_len: u64,
  /// The ops whose sources lie in the named runs, counted from the first run's start until the build ends, when the
  /// inline bytes' length is added to each.
  run_ops: Vec<usize>,
  /// The green paths the revision removes, unless a later row makes them again.
  gone: BTreeMap<String, Stood>,
  /// The restores of the revision's copies from older revisions, by their rows' paths ([`restore::plan`]), each taken
  /// as its copy is made.
  plans: restore::CopyPlans,
  /// The green paths a copy from an older revision sets back, merged with the rows that change them, made at the end.
  restores: BTreeMap<String, restore::Restore>,
}

impl<'b> Building<'b> {
  /// An empty increment for `checked` over `records` and `engine`.
  fn new(
    records: &'b Repository,
    engine: &'b Green,
    (checked, plans): (&'b Checked, restore::CopyPlans),
  ) -> Self {
    let rows = checked
      .changes()
      .iter()
      .map(|row| (row.path.as_str(), row))
      .collect();
    Self {
      records,
      engine,
      checked,
      rows,
      doc: OpsDoc::default(),
      post: Vec::new(),
      runs: Vec::new(),
      runs_len: 0,
      run_ops: Vec::new(),
      gone: BTreeMap::new(),
      plans,
      restores: BTreeMap::new(),
    }
  }

  /// The increment, based at `head` and identified by `id`: every row in path order, then what the revision removes.
  fn build(mut self, head: u64, id: [u8; 32]) -> Result<Increment, Uncommitted> {
    let checked = self.checked;
    // The post-state holds each text the rows set that is not named (`Building::insert`): sized once at their length,
    // so no append grows it (P7 finding (2): grown a text at a time it doubled). A copy's restored texts may still grow
    // it past them.
    let texts = checked
      .changes()
      .iter()
      .filter_map(|row| row.text)
      .filter(|text| self.held_inline(text).is_some())
      .try_fold(0usize, |total, text| {
        usize::try_from(text.size)
          .ok()
          .and_then(|size| total.checked_add(size))
      })
      .ok_or_else(corrupt)?;
    self
      .post
      .try_reserve_exact(texts)
      .map_err(|_| Uncommitted::Budget { available: 0 })?;
    for row in checked.changes() {
      self.row(row)?;
    }
    for (path, restore) in core::mem::take(&mut self.restores) {
      self.restored(&path, restore)?;
    }
    for (path, stood) in core::mem::take(&mut self.gone) {
      let kind = if stood.directory {
        OpKind::Rmdir
      } else {
        OpKind::Unlink
      };
      self.op(kind, &path);
    }
    // A named run's bytes follow the inline ones in the post-state's one address space (`slates_merge::named`), so each
    // op sourced from a run is moved past them now that their length is known.
    let inline = u64::try_from(self.post.len()).map_err(|_| corrupt())?;
    let mut doc = self.doc;
    for at in &self.run_ops {
      let op = doc.ops.get_mut(*at).ok_or_else(corrupt)?;
      op.src = op.src.checked_add(inline).ok_or_else(corrupt)?;
    }
    doc.canonicalize();
    let mut increment = Increment {
      id,
      base: head,
      doc,
      post_state: self.post,
      evidence: Vec::new(),
      named: Default::default(),
    };
    let mut at = inline;
    for run in self.runs {
      // Each run where its ops' sources say it starts: the engine numbers it the same way.
      if increment.push_named(run) != Some(at) {
        return Err(corrupt());
      }
      at = at.checked_add(run.len).ok_or_else(corrupt)?;
    }
    Ok(increment)
  }

  /// One row (module docs).
  fn row(&mut self, row: &ChangedPath) -> Result<(), Uncommitted> {
    let path = green_path(&row.path);
    if let Some(restore) = self.restores.get(path).copied() {
      return self.over_restore(row, restore);
    }
    match (row.action, &row.copy_from) {
      (Action::Delete, _) => self.remove(row, true),
      (Action::Add | Action::Replace, Some(source)) => self.copy(row, source),
      (Action::Replace, None) => self.replace(row),
      (Action::Add, None) => self.add(row),
      (Action::Modify, _) => self.modify(row),
    }
  }

  /// The node at `path` in `revision`, as the records hold it; none where nothing stands.
  fn stood(&self, path: &str, revision: u64) -> Result<Option<Stood>, Uncommitted> {
    let node = self
      .records
      .index()
      .node(path, revision)
      .map_err(|refusal| Uncommitted::Step(StepRefusal::Index(refusal)))?;
    Ok(node.map(|node| Stood {
      directory: node.kind() == NodeKind::Directory,
      len: node.text().map_or(0, |text| text.size),
      props: node.properties() != PropertyList::Empty,
    }))
  }

  /// Where the node at `path` stood before the revision changed it (`slates_svn::migration::before`): its path and
  /// revision, none where nothing stood.
  fn place_before(&self, path: &str) -> Option<(String, u64)> {
    match before(&self.rows, path) {
      Before::Head => Some((path.to_owned(), self.records.youngest())),
      Before::Copied { path, revision } => Some((path, revision)),
      Before::Absent => None,
    }
  }

  /// The node that stood at `path` before the revision changed it; records that do not hold together where none did.
  fn node_before(&self, path: &str) -> Result<Stood, Uncommitted> {
    let (at, revision) = self.place_before(path).ok_or_else(corrupt)?;
    self.stood(&at, revision)?.ok_or_else(corrupt)
  }

  /// A node the revision removes: noted gone, `itself` or only what stands beneath it, with everything beneath it.
  fn remove(&mut self, row: &ChangedPath, itself: bool) -> Result<(), Uncommitted> {
    let stood = self.node_before(&row.path)?;
    if itself {
      self.gone.insert(green_path(&row.path).to_owned(), stood);
    }
    if stood.directory {
      for (path, beneath) in self.tree_beneath(&row.path)? {
        self.gone.insert(path, beneath);
      }
    }
    Ok(())
  }

  /// Every node beneath the directory at `path` as the green holds it before the revision, at its green path: beneath
  /// the path at the head, or beneath a copy's source when the copy is of the head; a copy of an older revision is not
  /// listed (module docs, "Not declared yet").
  fn tree_beneath(&self, path: &str) -> Result<Vec<(String, Stood)>, Uncommitted> {
    let (source, revision) = self.place_before(path).ok_or_else(corrupt)?;
    // The head in the records' numbering: the green's head, or an import batch's whose scratch member counts its own
    // versions from 0 (C2 stage C).
    if revision != self.records.youngest() {
      return Err(Uncommitted::Unsupported {
        path: path.to_owned(),
      });
    }
    let (from, to) = (green_path(&source), green_path(path));
    self
      .engine
      .beneath(from)
      .into_iter()
      .map(|(found, directory)| {
        let len = self
          .engine
          .content(&found)
          .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let stood = Stood {
          directory,
          len,
          props: self.engine.xattr(&found, PROPS).is_some(),
        };
        moved(&found, from, to)
          .map(|place| (place, stood))
          .ok_or_else(corrupt)
      })
      .collect()
  }

  /// An addition or a replacement by copy: the source's tree at its revision copied by reference, with `REPLACE` where
  /// the destination holds a node (the replaced one, or one a row removed, which goes with the copy); then the row's
  /// own text and property list over what the copy brought.
  fn copy(&mut self, row: &ChangedPath, source: &CopySource) -> Result<(), Uncommitted> {
    let path = green_path(&row.path).to_owned();
    let held: Vec<String> = self
      .gone
      .keys()
      .filter(|gone| moved(gone, &path, "").is_some() || **gone == path)
      .cloned()
      .collect();
    for gone in &held {
      self.gone.remove(gone);
    }
    let replace = row.action == Action::Replace || !held.is_empty();
    let from = self.doc.paths.intern(green_path(&source.path));
    let flags = if replace { REPLACE } else { 0 };
    // A copy from an older revision is made from the head and set back where its source changed since (P7's
    // finding (1), `restore`): the green keeps no version for it.
    let head = self.records.youngest();
    let version = match self.plans.remove(&row.path) {
      Some(plan) if source.revision < head => {
        if !plan.from_head && replace {
          return Err(Uncommitted::Unsupported {
            path: row.path.clone(),
          });
        }
        self.restores.extend(plan.restores);
        plan.from_head.then_some(head)
      }
      None if source.revision >= head => Some(source.revision),
      _ => return Err(corrupt()),
    };
    if let Some(version) = version {
      self.op_at(OpKind::Copy, &path, (version, 0, from), flags);
    }
    if let Some(restore) = self.restores.get(&path).copied() {
      // The copy's own root is set back: the row's text and list go over the revision's.
      return self.over_restore(row, restore);
    }
    let brought = self
      .stood(&source.path, source.revision)?
      .ok_or_else(corrupt)?;
    if let Some(text) = &row.text {
      self.replace_text(&path, brought.len, Some(text))?;
    }
    self.properties(&path, row.properties, brought.props);
    Ok(())
  }

  /// A row of the revision over a path a copy from an older revision sets back (`restore`): a change in place goes over
  /// the restore (its text and its list where it names them), a deletion removes what the copy brought there, and a
  /// copy's own root takes its row's text and list; an addition or a replacement there is not declared yet.
  fn over_restore(
    &mut self,
    row: &ChangedPath,
    restore: restore::Restore,
  ) -> Result<(), Uncommitted> {
    let path = green_path(&row.path).to_owned();
    let copied_root = row.copy_from.is_some() && self.restores.contains_key(&path);
    match (row.action, restore) {
      (
        Action::Modify,
        restore::Restore::Set {
          brought,
          directory,
          text,
          props,
        },
      )
      | (
        Action::Add | Action::Replace,
        restore::Restore::Set {
          brought,
          directory,
          text,
          props,
        },
      ) if row.action == Action::Modify || copied_root => {
        let text = if row.text.is_some() { row.text } else { text };
        let props = row.properties.unwrap_or(props);
        self.restores.insert(
          path,
          restore::Restore::Set {
            brought,
            directory,
            text,
            props,
          },
        );
        Ok(())
      }
      (
        Action::Delete,
        restore::Restore::Set {
          brought, directory, ..
        },
      ) => {
        let beneath: Vec<String> = self
          .restores
          .range(format!("{path}/")..)
          .take_while(|(found, _)| found.starts_with(&format!("{path}/")))
          .map(|(found, _)| found.clone())
          .collect();
        if !beneath.is_empty() || directory {
          return Err(Uncommitted::Unsupported {
            path: row.path.clone(),
          });
        }
        match brought {
          Some(brought) => {
            self.restores.insert(
              path,
              restore::Restore::Remove {
                directory: brought.directory,
              },
            );
          }
          None => {
            self.restores.remove(&path);
          }
        }
        Ok(())
      }
      _ => Err(Uncommitted::Unsupported {
        path: row.path.clone(),
      }),
    }
  }

  /// A path a copy from an older revision sets back, made (`restore`): removed with what the head held there, or made
  /// as the copied revision held it, over what the copy from the head brought.
  fn restored(&mut self, path: &str, restore: restore::Restore) -> Result<(), Uncommitted> {
    match restore {
      restore::Restore::Remove { directory } => {
        let stood = Stood {
          directory,
          len: 0,
          props: false,
        };
        self.gone.insert(path.to_owned(), stood);
        Ok(())
      }
      restore::Restore::Set {
        brought: None,
        directory,
        text,
        props,
      } => {
        if directory {
          self.op(OpKind::Mkdir, path);
        } else {
          self.op(OpKind::Create, path);
          if let Some(text) = &text {
            self.insert(path, text)?;
          }
        }
        self.properties(path, Some(props), false);
        Ok(())
      }
      restore::Restore::Set {
        brought: Some(brought),
        directory,
        text,
        props,
      } => {
        if !directory {
          self.replace_text(path, brought.len, text.as_ref())?;
        }
        self.properties(path, Some(props), brought.props);
        Ok(())
      }
    }
  }

  /// A replacement without history: the node changed in place ([`Building::remake`]), what stood beneath it removed.
  fn replace(&mut self, row: &ChangedPath) -> Result<(), Uncommitted> {
    let stood = self.node_before(&row.path)?;
    if stood.directory {
      self.remove(row, false)?;
    }
    self.remake(row, green_path(&row.path), stood)
  }

  /// An addition without history: a new node, or, where a row removed one, that node made again in place.
  fn add(&mut self, row: &ChangedPath) -> Result<(), Uncommitted> {
    let path = green_path(&row.path).to_owned();
    if let Some(stood) = self.gone.remove(&path) {
      return self.remake(row, &path, stood);
    }
    if row.kind == NodeKind::Directory {
      self.op(OpKind::Mkdir, &path);
    } else {
      self.op(OpKind::Create, &path);
      if let Some(text) = &row.text {
        self.insert(&path, text)?;
      }
    }
    self.properties(&path, row.properties, false);
    Ok(())
  }

  /// A node made again where one stood (module docs): of its kind, its text the row's or none and its property list the
  /// row's or none; a node of another kind is not declared yet.
  fn remake(&mut self, row: &ChangedPath, path: &str, stood: Stood) -> Result<(), Uncommitted> {
    if stood.directory != (row.kind == NodeKind::Directory) {
      return Err(Uncommitted::Unsupported {
        path: row.path.clone(),
      });
    }
    if !stood.directory {
      self.replace_text(path, stood.len, row.text.as_ref())?;
    }
    let list = row.properties.unwrap_or(PropertyList::Empty);
    self.properties(path, Some(list), stood.props);
    Ok(())
  }

  /// A change in place: the row's text replacing the node's whole, its property list set or emptied.
  fn modify(&mut self, row: &ChangedPath) -> Result<(), Uncommitted> {
    let stood = self.node_before(&row.path)?;
    let path = green_path(&row.path).to_owned();
    if let Some(text) = &row.text {
      self.replace_text(&path, stood.len, Some(text))?;
    }
    self.properties(&path, row.properties, stood.props);
    Ok(())
  }

  /// `text`'s bytes where the revision brings them or the records hold them themselves (a property list's bytes read as
  /// a text): what the post-state carries inline. None for a text stored in the content store, which is named.
  fn held_inline(&self, text: &Text) -> Option<std::borrow::Cow<'b, [u8]>> {
    let checked: &'b Checked = self.checked;
    let records: &'b Repository = self.records;
    checked
      .fresh()
      .get(&text.identity)
      .map(|bytes| std::borrow::Cow::Borrowed(bytes.as_slice()))
      .or_else(|| records.text(&text.identity))
  }

  /// The file at `path`, `len` bytes long, given `text` whole, or emptied for none.
  fn replace_text(&mut self, path: &str, len: u64, text: Option<&Text>) -> Result<(), Uncommitted> {
    if len > 0 {
      self.op_at(OpKind::Delete, path, (0, len, NO_SOURCE), 0);
    }
    match text {
      Some(text) => self.insert(path, text),
      None => Ok(()),
    }
  }

  /// `text`'s bytes inserted at the start of the file at `path`; nothing for an empty text. A text the revision or the
  /// records hold is copied into the post-state; one stored in the content store is named whole (A-760; goal 93, P7
  /// finding (2): the copy was the import's largest per-text copy after A-862), its bytes read into a buffer of their own
  /// before the increment is judged ([`read_named`]), which the commit makes the file.
  fn insert(&mut self, path: &str, text: &Text) -> Result<(), Uncommitted> {
    if let Some(bytes) = self.held_inline(text) {
      let src = u64::try_from(self.post.len()).map_err(|_| corrupt())?;
      let len = u64::try_from(bytes.len()).map_err(|_| corrupt())?;
      self.post.extend_from_slice(&bytes);
      if len > 0 {
        self.op_at(OpKind::Insert, path, (0, len, src), 0);
      }
      return Ok(());
    }
    if text.size == 0 {
      return Ok(());
    }
    let src = self.runs_len;
    self.runs.push(slates_merge::named::NamedRun {
      identity: slates_merge::named::Identity {
        hash: text.identity,
        len: text.size,
      },
      offset: 0,
      len: text.size,
    });
    self.runs_len = self.runs_len.checked_add(text.size).ok_or_else(corrupt)?;
    self.op_at(OpKind::Insert, path, (0, text.size, src), 0);
    self.run_ops.push(self.doc.ops.len().saturating_sub(1));
    Ok(())
  }

  /// The property list of the node at `path`: `list` held set (its identity in [`PROPS`]) or emptied (the attribute
  /// removed where the node had one, `had`); none leaves it.
  fn properties(&mut self, path: &str, list: Option<PropertyList>, had: bool) {
    match list {
      Some(PropertyList::Held { identity, .. }) => {
        let name = self.doc.paths.intern(PROPS);
        let src = u64::try_from(self.post.len()).unwrap_or(NO_SOURCE);
        let len = u64::try_from(identity.len()).unwrap_or(0);
        self.post.extend_from_slice(&identity);
        self.op_at(OpKind::SetXattr, path, (name, len, src), 0);
      }
      Some(PropertyList::Empty) if had => {
        let name = self.doc.paths.intern(PROPS);
        self.op_at(OpKind::RemoveXattr, path, (name, 0, NO_SOURCE), 0);
      }
      _ => {}
    }
  }

  /// An operation that names a path alone.
  fn op(&mut self, kind: OpKind, path: &str) {
    self.op_at(kind, path, (0, 0, NO_SOURCE), 0);
  }

  /// One operation on the green path `path`, node-exclusive, with `flags` beside it.
  fn op_at(&mut self, kind: OpKind, path: &str, (at, len, src): (u64, u64, u64), flags: u8) {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: NODE_EXCLUSIVE | flags,
      path,
      at,
      len,
      src,
    });
  }
}
