//! Subversion's commit, landed by the repository's owner (A-655 part 2b; §4.17.4 items 1, 3.3 to 3.5 and piece 7;
//! §4.17.1 "A push is a work" and "Durability and residency"; A-635, A-662, A-645).
//!
//! **The step** ([`commit`]): one for a client's accepted work and for an imported revision (A-652 part 3), on the
//! repository's owner shard, in one turn:
//! 1. the repository's records (A-662's map, `crate::vcs_store`), lent the room the shard could admit;
//! 2. a client's work judged again where its verdict's facts moved (`Repository::judge_again`: FSFS's merge again at
//!    the youngest revision, then its locks verified as the table stands, `tree.c` `svn_fs_fs__commit_txn` and
//!    `transaction.c` `commit_body`), so a work whose head moved is refused only where the merge finds a conflict, as
//!    FSFS refuses one; an imported revision as it was made;
//! 3. the check (`Repository::check`, which changes nothing), and the records' growth charged to the shard's budget;
//! 4. the green's next version ([`Building`]): the increment moving the repository's one green from its head, which is
//!    the records' youngest revision (version N is revision N, item 1), to the revision;
//! 5. one entry of the repository's logs through A-635's `RepositoryLogs::append`, holding the green's version and the
//!    revision's record (`slates_svn::commit::record`) as one durable step (item 3.5);
//! 6. the apply (`Repository::apply`), which the check in the same turn made certain; the charge trued up.
//!
//! **A record that moves no version** ([`record`], A-652 part 3): the repository's UUID, or a revision's properties set
//! whole, as an import names a repository that holds no revision but 0 by its stream's UUID and sets revision 0's
//! properties to its stream's. Checked against the records first (a revision they hold, the room), its growth charged,
//! one entry holding the record alone and moving no green, then applied.
//!
//! A refusal before the append leaves the records, the green and the logs as they were, and gives back what was
//! charged. Once the entry is recorded the revision is the repository's: should the green answer another version or
//! the records refuse the apply (neither met in a correct use, each counted), the records are made again from the log,
//! which holds the revision (`vcs_store::rebuild`).
//!
//! **The green's version** (item 1). Files hold Subversion's repository-normal bytes; a node's property list is named in
//! the reserved attribute [`PROPS`] by its content identity, the list itself held by the records; a copy is the engine's
//! `Copy` of the source's tree at its revision, by reference (A-586), over what the destination holds with `REPLACE`
//! (A-626); every operation is node-exclusive (A-542), so the engine judges each as touching its whole node, as FSFS
//! does. The rows are read in path order, parents first. A deletion removes its node and everything beneath it, named one
//! by one, since a node-exclusive removal of a directory needs the same increment to empty it (`Green::beneath` lists
//! what stands there; the engine drops a removed node's attributes itself). An addition without history creates its
//! node; a text the row sets replaces the node's whole (a `Delete` of the old length and an `Insert`); a list the row
//! sets or empties sets or removes the attribute. A node one row removes and a later row makes again as its own kind
//! is changed in place, since the engine refuses a creation over a node the same increment removes.
//!
//! **Not declared yet** (refused typed, [`Uncommitted::Unsupported`], counted [`UNSUPPORTED`], nothing changed; GAPS
//! §4.17.4): a directory deleted or replaced beneath a copy the same revision made from a revision older than the head,
//! whose tree no public read of the engine lists at that version; and a replacement without history that changes a
//! node's kind, which the engine refuses as `TypeChanged` (GAPS §8f).
//!
//! **A client's work** ([`commit_work`], A-698): what both ra_svn transports call with the work their connection
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
use slates_svn::index::{Action, ChangedPath, CopySource, NodeKind, PropertyList, Text};
use slates_svn::lock::{AprTime, CommitLocks};
use slates_svn::migration::{Before, Checked, Repository, before};

use super::push::{
  AppendRefused, Appended, BranchAppended, BranchChange, EntryParts, RepositoryLogs,
};
use crate::state::ShardState;

#[cfg(test)]
mod tests;

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
    };
    Answered::Refused { code, message }
  }
}

/// A client's accepted `work` (module docs, "A client's work") landed on `repository`'s logs by `principal`: the
/// revision made of it at the owner's instant `now`, judged again with the commit's locks at `now`.
pub(crate) fn commit_work(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  work: Work,
  now: AprTime,
) -> Result<Committed, Uncommitted> {
  let locks = work.locks.clone();
  let revision = Revision::of_work(work, now).map_err(Uncommitted::Step)?;
  commit(
    state,
    (principal, repository),
    revision,
    Some((&locks, now)),
  )
}

/// The refusal of records that do not hold together.
fn corrupt() -> Uncommitted {
  Uncommitted::Step(StepRefusal::Corrupt)
}

/// The step for a record that moves no version (module docs; A-652 part 3): `record`, the repository's UUID or a
/// revision's properties set whole, committed on `repository`'s logs by `principal`, its owner. A creation or a revision
/// is refused `Corrupt`: a creation is made once, by `CreateRepository`, and a revision moves the green ([`commit`]).
pub(crate) fn record(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  record: Record,
) -> Result<(), Uncommitted> {
  green_of(state, repository)?;
  let brought = match &record {
    Record::RevisionProperties { properties, .. } => {
      bytes_of(properties.iter(), core::iter::empty::<&Vec<u8>>())
    }
    Record::Uuid(_) | Record::Locks(_) => 0,
    Record::Created { .. } | Record::Revision(_) => return Err(corrupt()),
  };
  if !crate::vcs_store::lend_svn(state, repository, brought) {
    return Err(Uncommitted::NotHeld);
  }
  let landed = record_growth(state, repository, &record)
    .and_then(|growth| land_record(state, (principal, repository), record, growth));
  crate::vcs_store::settle_svn(state, repository);
  landed
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

/// Appends `record` alone through A-635's append, its `growth` charged first; then applies it. An append that records
/// nothing is answered `Diverged`, counted, since the entry holds a record and so is always recorded; an apply refused
/// once the entry recorded it is counted and the records are made again from the log.
fn land_record(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  record: Record,
  growth: u64,
) -> Result<(), Uncommitted> {
  crate::vcs_store::reserve_svn(state, repository, growth)
    .map_err(|available| Uncommitted::Budget { available })?;
  let parts = EntryParts {
    objects: record.encode(),
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

/// The step (module docs): `revision` committed on `repository`'s logs by `principal`, its owner; a client's work
/// judged again with its commit's locks at the owner's instant (`judged`), an imported revision (`None`) as made (A-652
/// part 3's import, `crate::vcs_store::svn_import`).
pub(crate) fn commit(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  revision: Revision,
  judged: Option<(&CommitLocks, AprTime)>,
) -> Result<Committed, Uncommitted> {
  let green = green_of(state, repository)?;
  if !crate::vcs_store::lend_svn(state, repository, brought(&revision)) {
    return Err(Uncommitted::NotHeld);
  }
  let landed = checked(state, repository, revision, judged)
    .and_then(|checked| land(state, (principal, repository), green, checked));
  crate::vcs_store::settle_svn(state, repository);
  landed
}

/// A revision the server made of a version a work submitted to `repository`'s green (A-664's Subversion objects,
/// `crate::vcs_store::svn_made`): checked as an import's is, its record appended in the submit's own transaction in an
/// entry of the register holding it alone (the version moved the green already), then applied. Its revision.
pub(crate) fn land_made(
  state: &mut ShardState,
  repository: RepositoryId,
  revision: Revision,
) -> Result<u64, Uncommitted> {
  if !crate::vcs_store::lend_svn(state, repository, brought(&revision)) {
    return Err(Uncommitted::NotHeld);
  }
  let landed = checked(state, repository, revision, None)
    .and_then(|checked| append_made(state, repository, checked));
  crate::vcs_store::settle_svn(state, repository);
  landed
}

/// [`land_made`]'s entry: the record's bytes admitted and its growth charged, the entry appended in the running
/// transaction, then the records applied; an apply refused once the entry recorded it is counted and the records are
/// made again from the log.
fn append_made(
  state: &mut ShardState,
  repository: RepositoryId,
  (checked, record): (Checked, Vec<u8>),
) -> Result<u64, Uncommitted> {
  crate::vcs_store::admit_objects(state, record.len())
    .map_err(|available| Uncommitted::Budget { available })?;
  let growth = grown(&checked);
  crate::vcs_store::reserve_svn(state, repository, growth)
    .map_err(|available| Uncommitted::Budget { available })?;
  let entry = slates_db::op::Op::RepositoryAdvanced {
    entry: slates_db::catalog::RepositoryEntry {
      repository,
      objects: record,
      ..slates_db::catalog::RepositoryEntry::default()
    },
  };
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  if let Err(error) = state.db.mutate(&mut state.segment, &entry, now) {
    crate::vcs_store::unreserve_svn(state, repository, growth);
    let refusal = crate::error::refusal_of_db(&error);
    return Err(Uncommitted::Append(AppendRefused::Entry(refusal)));
  }
  let revision = checked.revision();
  let applied = state
    .vcs_stores
    .subversion(repository)
    .and_then(|svn| svn.records.as_mut())
    .map(|records| records.apply(checked));
  if applied != Some(Ok(revision)) {
    state.count(APPLY_REFUSED, 1);
    crate::vcs_store::rebuild(state, repository);
  }
  Ok(revision)
}

/// The repository's one green (item 1): its default branch's, here and not fenced. `NotHeld` otherwise, and for a
/// repository not here or not Subversion's.
fn green_of(state: &ShardState, repository: RepositoryId) -> Result<DbVolumeId, Uncommitted> {
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

/// Appends the checked revision through A-635's append, the green's version and the record one entry, its growth
/// charged first; then applies it.
fn land(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  green: DbVolumeId,
  (checked, record): (Checked, Vec<u8>),
) -> Result<Committed, Uncommitted> {
  let work = version_of(state, (repository, green), &checked, &record)?;
  // The record is the repository's content in the partition's register, charged to this shard's memory budget
  // (`vcs_store::settle_objects`): a revision is admitted only while the shard could hold it.
  crate::vcs_store::admit_objects(state, record.len())
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
  let bound =
    slates_merge::chain_entry::entry_bound(work.encoded_len(), &work).saturating_add(room);
  if state.db.partition().green_chain_room() < bound {
    state.count(CHAIN_ROOM_CHECKPOINTED, 1);
    crate::merge_service::checkpoint_green_when(state, green, crate::merge_service::Doubling::Now);
  }
  let parts = EntryParts {
    objects: record,
    ..EntryParts::default()
  };
  let change = BranchChange::Update { green, work };
  let appended = RepositoryLogs::append(state, principal, repository, vec![change], parts);
  match appended {
    Ok(appended) => {
      // Fold the green's chain by the merge service's doubling rule as every accepted version does (W3), so a
      // repository's chain stays bounded however many revisions it takes.
      crate::merge_service::checkpoint_green(state, green);
      Ok(applied(state, repository, checked, &appended))
    }
    Err(refused) => {
      crate::vcs_store::unreserve_svn(state, repository, growth);
      Err(Uncommitted::Append(refused))
    }
  }
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
    state
      .vcs_stores
      .subversion(repository)
      .and_then(|svn| svn.records.as_mut())
      .map(|records| records.apply(checked))
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
  checked: &Checked,
  record: &[u8],
) -> Result<Increment, Uncommitted> {
  let built = {
    let records = state
      .vcs_stores
      .subversion_records(repository)
      .ok_or(Uncommitted::NotHeld)?;
    let engine = state.greens.get(&green).ok_or(Uncommitted::NotHeld)?;
    let head = records.youngest();
    if engine.head() == head {
      Building::new(records, &engine, checked).build(head, identity(record))
    } else {
      Err(Uncommitted::Diverged)
    }
  };
  match &built {
    Err(Uncommitted::Diverged) => state.count(DIVERGED, 1),
    Err(Uncommitted::Unsupported { .. }) => state.count(UNSUPPORTED, 1),
    _ => {}
  }
  built
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
struct Stood {
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
  /// The post-state.
  post: Vec<u8>,
  /// The green paths the revision removes, unless a later row makes them again.
  gone: BTreeMap<String, Stood>,
}

impl<'b> Building<'b> {
  /// An empty increment for `checked` over `records` and `engine`.
  fn new(records: &'b Repository, engine: &'b Green, checked: &'b Checked) -> Self {
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
      gone: BTreeMap::new(),
    }
  }

  /// The increment, based at `head` and identified by `id`: every row in path order, then what the revision removes.
  fn build(mut self, head: u64, id: [u8; 32]) -> Result<Increment, Uncommitted> {
    let checked = self.checked;
    for row in checked.changes() {
      self.row(row)?;
    }
    for (path, stood) in core::mem::take(&mut self.gone) {
      let kind = if stood.directory {
        OpKind::Rmdir
      } else {
        OpKind::Unlink
      };
      self.op(kind, &path);
    }
    let mut doc = self.doc;
    doc.canonicalize();
    Ok(Increment {
      id,
      base: head,
      doc,
      post_state: self.post,
      evidence: Vec::new(),
    })
  }

  /// One row (module docs).
  fn row(&mut self, row: &ChangedPath) -> Result<(), Uncommitted> {
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
    if revision != self.engine.head() {
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
    self.op_at(OpKind::Copy, &path, (source.revision, 0, from), flags);
    let brought = self
      .stood(&source.path, source.revision)?
      .ok_or_else(corrupt)?;
    if let Some(text) = &row.text {
      self.replace_text(&path, brought.len, Some(text))?;
    }
    self.properties(&path, row.properties, brought.props);
    Ok(())
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

  /// `text`'s bytes: brought by the revision, or held by the records.
  fn text_bytes(&self, text: &Text) -> Result<&'b [u8], Uncommitted> {
    let checked: &'b Checked = self.checked;
    let records: &'b Repository = self.records;
    checked
      .fresh()
      .get(&text.identity)
      .map(Vec::as_slice)
      .or_else(|| records.content(&text.identity))
      .ok_or_else(corrupt)
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

  /// `text`'s bytes inserted at the start of the file at `path`; nothing for an empty text.
  fn insert(&mut self, path: &str, text: &Text) -> Result<(), Uncommitted> {
    let bytes = self.text_bytes(text)?;
    if bytes.is_empty() {
      return Ok(());
    }
    let src = u64::try_from(self.post.len()).map_err(|_| corrupt())?;
    let len = u64::try_from(bytes.len()).map_err(|_| corrupt())?;
    self.post.extend_from_slice(bytes);
    self.op_at(OpKind::Insert, path, (0, len, src), 0);
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
