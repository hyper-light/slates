//! The commit step (A-655; §4.17.4 item 3.5): one revision checked against a repository's records, then applied, all
//! of it or none; every refusal leaves the records as they were; and the same revisions committed again into a fresh
//! repository reach the same records, as a recovery replaying the owner's log must.

use std::collections::BTreeMap;

use super::support::{History, ROOMY, created, created_within, digests, history, properties};
use crate::commit::revision::{Revision as RevisionToCommit, StepRefusal};
use crate::index::{
  Action, ChangedPath, ContentIdentity, IndexRefusal, NodeKind, PropertyList, RevisionRecord, Text,
};
use crate::lock::{AprTime, CommitLocks, Lock, LockEntry, LockRefusal, LockToken};
use crate::migration::properties::encode;
use crate::migration::{Repository, Room};

/// The date of the first revision the tests commit.
const FIRST_DATE: &str = "2026-11-19T07:00:00.000000Z";

/// The file every revision after the first changes.
const FILE: &str = "/trunk/a.txt";

/// What the tests compare of a repository, whole: its history (every revision's properties, transaction and changed
/// paths, and every content they name), its index as it prints, the bytes it holds and its locks.
type Snapshot = (History, String, u64, Vec<Lock>);

/// `repository`, whole.
fn snapshot(repository: &Repository) -> Snapshot {
  (
    history(repository),
    format!("{:?}", repository.index()),
    repository.held(),
    repository.locks().locks().cloned().collect(),
  )
}

/// `bytes` held under their identity.
fn content(bytes: &[u8]) -> (ContentIdentity, Vec<u8>) {
  (*blake3::hash(bytes).as_bytes(), bytes.to_vec())
}

/// A text of `bytes` as a row names it.
fn text_of(bytes: &[u8]) -> Text {
  let (md5, sha1) = digests(bytes);
  Text {
    identity: *blake3::hash(bytes).as_bytes(),
    md5,
    sha1,
    size: u64::try_from(bytes.len()).unwrap(),
  }
}

/// A row of `action` on the node of `kind` at `path`, changing neither text nor properties.
fn row(path: &str, action: Action, kind: NodeKind) -> ChangedPath {
  ChangedPath {
    path: path.to_owned(),
    action,
    kind,
    copy_from: None,
    text: None,
    properties: None,
    mergeinfo_modified: false,
  }
}

/// Revision 1: `/trunk` added, and [`FILE`] added holding `alpha` and `svn:eol-style`.
fn first() -> RevisionToCommit {
  let list = encode(&properties(&[("svn:eol-style", "native")]));
  let (list_identity, list) = content(&list);
  let file = ChangedPath {
    text: Some(text_of(b"alpha\n")),
    properties: Some(PropertyList::Held {
      identity: list_identity,
      has_mergeinfo: false,
    }),
    ..row(FILE, Action::Add, NodeKind::File)
  };
  RevisionToCommit {
    head: 0,
    record: RevisionRecord {
      transaction: "0-1".to_owned(),
      changes: vec![row("/trunk", Action::Add, NodeKind::Directory), file],
    },
    properties: properties(&[
      ("svn:author", "alice"),
      ("svn:date", FIRST_DATE),
      ("svn:log", "first"),
    ]),
    contents: BTreeMap::from([content(b"alpha\n"), (list_identity, list)]),
    stored: BTreeMap::new(),
    lock_entries: Vec::new(),
  }
}

/// The revision after `head`: [`FILE`]'s text `bytes`, applying `entries`.
fn edit(head: u64, bytes: &[u8], entries: Vec<LockEntry>) -> RevisionToCommit {
  let file = ChangedPath {
    text: Some(text_of(bytes)),
    ..row(FILE, Action::Modify, NodeKind::File)
  };
  RevisionToCommit {
    head,
    record: RevisionRecord {
      transaction: format!("{head}-1"),
      changes: vec![file],
    },
    properties: properties(&[
      ("svn:author", "bob"),
      ("svn:date", "2026-11-19T08:00:00.000000Z"),
      ("svn:log", "an edit"),
    ]),
    contents: BTreeMap::from([content(bytes)]),
    stored: BTreeMap::new(),
    lock_entries: entries,
  }
}

/// Bob's lock on [`FILE`].
fn bobs_lock() -> Lock {
  Lock {
    path: FILE.to_owned(),
    token: LockToken::supplied("opaquelocktoken:bob").unwrap(),
    owner: "bob".to_owned(),
    comment: None,
    is_dav_comment: false,
    created: AprTime(0),
    expires: None,
  }
}

/// The entry releasing bob's lock.
fn release() -> Vec<LockEntry> {
  vec![LockEntry::Removed {
    path: FILE.to_owned(),
    token: bobs_lock().token,
  }]
}

/// Commits into `repository` what the owner did, in its order: revision 1, bob's lock taken, revision 2 releasing
/// it, revision 3.
fn owners_history(repository: &mut Repository) -> Result<(), StepRefusal> {
  repository.commit(first())?;
  repository.apply_lock_entries(&[LockEntry::Set(bobs_lock())])?;
  repository.commit(edit(1, b"beta\n", release()))?;
  repository.commit(edit(2, b"gamma\n", Vec::new()))?;
  Ok(())
}

/// A-655 (§4.17.4 item 3.5; §4.17.1 item 3). Do: commit three revisions and a lock through the step, then the same
/// revisions and lock, in the same order, into a fresh repository. Expect: the second repository equal to the first
/// whole (history, index, bytes held, locks), bob's lock released by the revision that presented it; nothing the step
/// did read a clock or drew anything a replay could not.
#[test]
fn the_same_revisions_committed_again_reach_the_same_records() {
  let mut owner = created();
  owners_history(&mut owner).unwrap();
  let mut replayed = created();
  owners_history(&mut replayed).unwrap();
  assert_eq!(snapshot(&replayed), snapshot(&owner));
  assert_eq!((owner.youngest(), owner.locks().locks().count()), (3, 0));
}

/// A-655 (`transaction.c` `commit_body` 3784-3786). Do: commit revision 1, then a revision judged against revision 0.
/// Expect: refused "Transaction out of date" (`E160028`), the repository as it was.
#[test]
fn a_revision_judged_against_an_older_head_is_out_of_date_and_changes_nothing() {
  let mut repository = created();
  repository.commit(first()).unwrap();
  let before = snapshot(&repository);
  let refusal = repository
    .commit(edit(0, b"beta\n", Vec::new()))
    .unwrap_err();
  let answered = (refusal.subversion_code(), refusal.message());
  assert_eq!(answered, (160_028, "Transaction out of date".to_owned()));
  assert_eq!(
    refusal,
    StepRefusal::OutOfDate {
      head: 0,
      youngest: 1
    }
  );
  assert_eq!(snapshot(&repository), before);
}

/// A-655 (the module documentation of the records: check, then apply). Do: check two revisions after revision 1,
/// apply the first, then the second. Expect: the first is revision 2; the second refused out of date, its check made
/// against a repository that has moved since, the repository as the first left it.
#[test]
fn a_check_is_refused_once_another_revision_was_applied_after_it() {
  let mut repository = created();
  repository.commit(first()).unwrap();
  let one = repository.check(edit(1, b"beta\n", Vec::new())).unwrap();
  let two = repository.check(edit(1, b"gamma\n", Vec::new())).unwrap();
  assert_eq!(repository.apply(one), Ok(2));
  let before = snapshot(&repository);
  let refusal = repository.apply(two).unwrap_err();
  assert_eq!(
    refusal,
    StepRefusal::OutOfDate {
      head: 1,
      youngest: 2
    }
  );
  assert_eq!(snapshot(&repository), before);
}

/// A-655 (A-602's entries applied with their revision). Do: commit a revision releasing bob's lock when no lock is
/// held. Expect: refused with the lock table's refusal (`E160004`), the repository as it was.
#[test]
fn a_lock_entry_the_table_does_not_take_refuses_the_revision_whole() {
  let mut repository = created();
  repository.commit(first()).unwrap();
  let before = snapshot(&repository);
  let refusal = repository
    .commit(edit(1, b"beta\n", release()))
    .unwrap_err();
  let unrecorded = LockRefusal::Unrecorded {
    path: FILE.to_owned(),
  };
  assert_eq!(refusal, StepRefusal::Lock(unrecorded));
  assert_eq!(refusal.subversion_code(), 160_004);
  assert_eq!(snapshot(&repository), before);
}

/// A-655 (the records' module documentation: every text and property list the rows name held). Do: commit a revision
/// naming a text it does not bring and the repository does not hold, then one naming a text the repository holds
/// without bringing it. Expect: the first refused "Filesystem is corrupt" (`E160004`), the repository as it was; the
/// second committed as revision 2, holding no byte of text more.
#[test]
fn a_text_the_rows_name_is_held_or_brought() {
  let mut repository = created();
  repository.commit(first()).unwrap();
  let before = snapshot(&repository);
  let mut missing = edit(1, b"beta\n", Vec::new());
  missing.contents.clear();
  let refusal = repository.commit(missing).unwrap_err();
  let answered = (refusal.subversion_code(), refusal.message());
  assert_eq!(answered, (160_004, "Filesystem is corrupt".to_owned()));
  assert_eq!(snapshot(&repository), before);
  let mut held = edit(1, b"alpha\n", Vec::new());
  held.contents.clear();
  let properties: u64 = held
    .properties
    .iter()
    .map(|(name, value)| u64::try_from(name.len() + value.len()).unwrap())
    .sum();
  let grown = repository.held() + properties;
  assert_eq!(repository.commit(held), Ok(2));
  assert_eq!(repository.held(), grown);
}

/// The bytes `revision` adds to a repository that holds none of its contents.
fn added(revision: &RevisionToCommit) -> u64 {
  let pairs = revision.properties.iter();
  let properties: usize = pairs.map(|(name, value)| name.len() + value.len()).sum();
  let contents: usize = revision.contents.values().map(Vec::len).sum();
  u64::try_from(properties + contents).unwrap()
}

/// A-655 (the room the records are held within). Do: commit revision 1 into a repository whose room holds one byte
/// fewer than it needs, then into one whose index holds two entries. Expect: refused `Room` (`E160000`) naming the
/// bytes it needed, then the index's own `Room`; each repository as it was.
#[test]
fn a_revision_past_the_room_is_refused_whole() {
  let needed = created().held() + added(&first());
  let bytes = Room {
    entries: ROOMY.entries,
    bytes: needed - 1,
  };
  let mut small = created_within(bytes);
  let before = snapshot(&small);
  let refusal = small.commit(first()).unwrap_err();
  let room = StepRefusal::Room {
    needed,
    room: needed - 1,
  };
  assert_eq!((refusal.subversion_code(), refusal), (160_000, room));
  assert_eq!(snapshot(&small), before);
  let entries = Room {
    entries: 2,
    bytes: ROOMY.bytes,
  };
  let mut few = created_within(entries);
  let refusal = few.commit(first()).unwrap_err();
  assert!(
    matches!(refusal, StepRefusal::Index(IndexRefusal::Room { .. })),
    "{refusal:?}"
  );
  assert_eq!(few.youngest(), 0);
}

/// A-655 part 2b (`transaction.c` `commit_body` 3797: `verify_locks` runs at the commit, over the table as it stands).
/// Do: commit revision 1; bob locks the file; check alice's edit, judged when no lock stood (no entries, her commit
/// presenting no token), through the step's check of a work; then bob's, presenting his token. Expect: alice's refused
/// as the drive refuses a lock another holds, "User 'alice' does not own lock on path '/trunk/a.txt' (currently locked
/// by 'bob')" (`E160039`), the repository as it was; bob's checked with the lock's release as the step's own entry, and
/// committed as revision 2.
#[test]
fn t_a655_the_steps_check_of_a_work_verifies_the_locks_as_they_stand() {
  let mut repository = created();
  repository.commit(first()).unwrap();
  repository
    .apply_lock_entries(&[LockEntry::Set(bobs_lock())])
    .unwrap();
  let before = snapshot(&repository);
  let alice = CommitLocks {
    user: Some("alice".to_owned()),
    ..CommitLocks::default()
  };
  let refusal = repository
    .check_work(edit(1, b"beta\n", Vec::new()), &alice, AprTime(1))
    .unwrap_err();
  let words = "User 'alice' does not own lock on path '/trunk/a.txt' (currently locked by 'bob')";
  let answered = (refusal.subversion_code(), refusal.message());
  assert_eq!(answered, (160_039, words.to_owned()));
  assert_eq!(snapshot(&repository), before);
  let token = bobs_lock().token.as_str().to_owned();
  let bob = CommitLocks {
    user: Some("bob".to_owned()),
    presented: BTreeMap::from([(FILE.to_owned(), token)]),
    keep: false,
  };
  let checked = repository
    .check_work(edit(1, b"beta\n", Vec::new()), &bob, AprTime(1))
    .unwrap();
  assert_eq!(checked.lock_entries(), release().as_slice());
  assert_eq!(repository.apply(checked), Ok(2));
}

/// S4 slice 2 c2 (texts kept as skip-delta svndiffs, records.rs "Texts"). Do: commit revision 1, then 40 edits of
/// [`FILE`], each a 4 KiB text that changes one line of the one before. Expect: every revision's text read back whole
/// from the records; no read applying more deltas than the node's predecessor count has bits, plus one (FSFS's skip-delta
/// bound); and the deltas holding under a quarter of the texts' bytes.
#[test]
fn s4_c2_a_file_changed_many_times_reads_back_through_a_logarithmic_chain_of_deltas() {
  let mut repository = created();
  repository.commit(first()).unwrap();
  let line =
    |at: usize, version: usize| format!("{at:04} line of the file, version {version:04}\n");
  let version = |n: usize| -> Vec<u8> {
    (0..64)
      .map(|at| line(at, if at == n % 64 { n } else { 0 }))
      .collect::<String>()
      .into_bytes()
  };
  let mut texts = vec![b"alpha\n".to_vec()];
  for n in 1..=40usize {
    let bytes = version(n);
    repository
      .commit(edit(u64::try_from(n).unwrap(), &bytes, Vec::new()))
      .unwrap();
    texts.push(bytes);
  }
  let mut whole = 0u64;
  for (n, bytes) in texts.iter().enumerate() {
    let identity = *blake3::hash(bytes).as_bytes();
    assert_eq!(
      repository.text(&identity).as_deref(),
      Some(bytes.as_slice()),
      "revision {}",
      n + 1
    );
    let count = u64::try_from(n).unwrap();
    let bound = usize::try_from(u64::BITS - count.leading_zeros()).unwrap() + 1;
    let applied = repository.text_chain(&identity).unwrap();
    assert!(
      applied <= bound,
      "revision {}: {applied} deltas, bound {bound}",
      n + 1
    );
    whole += u64::try_from(bytes.len()).unwrap();
  }
  assert!(
    repository.text_bytes() * 4 < whole,
    "the deltas hold {} of {whole} bytes",
    repository.text_bytes()
  );
}

/// A-761 (§4.17.1 "A push's intake", part 2). Do: commit revision 1 twice, once with [`FILE`]'s text kept in the
/// records and once with it stored (the records naming it alone, its bytes in a store beside them); export each whole in
/// formats 2 and 3, the stored one reading its text through that store. Expect: the same dumps byte for byte; and the
/// stored one exported through its records alone refused `Corrupt`, since they hold no bytes of it.
#[test]
fn a_stored_text_exports_through_the_store_beside_the_records_as_a_kept_one_does() {
  use crate::dump::Format;
  use crate::migration::{Exporter, Exporting, MigrationRefusal, TextSource};
  /// The records, and one text a commit stored beside them.
  struct Beside<'a>(&'a Repository, (ContentIdentity, Vec<u8>));
  impl TextSource for Beside<'_> {
    fn text(&self, identity: &ContentIdentity) -> Option<std::borrow::Cow<'_, [u8]>> {
      if *identity == (self.1).0 {
        return Some(std::borrow::Cow::Borrowed((self.1).1.as_slice()));
      }
      TextSource::text(self.0, identity)
    }

    fn read_step(
      &self,
      identity: &ContentIdentity,
      reading: &mut crate::migration::TextReading,
    ) -> Option<bool> {
      if *identity == (self.1).0 {
        return Some(reading.copy_step(&(self.1).1));
      }
      TextSource::read_step(self.0, identity, reading)
    }
  }
  let exported = |repository: &Repository, texts: &dyn TextSource, format: Format| {
    let mut exporter = Exporter::new(repository, 0, repository.youngest(), format)?;
    let mut out = Vec::new();
    while exporter.export(repository, texts, &mut out)? != Exporting::Done {}
    Ok::<_, MigrationRefusal>(out)
  };
  let mut kept = created();
  let checked = kept.check(first()).unwrap();
  kept.apply(checked).unwrap();
  let (identity, bytes) = content(b"alpha\n");
  let mut naming = first();
  naming.contents.remove(&identity);
  naming
    .stored
    .insert(identity, u64::try_from(bytes.len()).unwrap());
  let mut stored = created();
  let checked = stored.check(naming).unwrap();
  stored.apply(checked).unwrap();
  let beside = Beside(&stored, (identity, bytes));
  for format in [Format::Two, Format::Three] {
    assert_eq!(
      exported(&stored, &beside, format).unwrap(),
      exported(&kept, &kept, format).unwrap()
    );
  }
  assert_eq!(
    exported(&stored, &stored, Format::Two),
    Err(MigrationRefusal::Corrupt)
  );
}

/// c2's exporter text rebuild sliced (main's item 2 of 2026-11-20; Ada's rule: a performance gap is fixed first). Do:
/// commit [`FILE`] with a 512 KiB text, then edit a few of its bytes in each of 31 revisions, so the youngest text is
/// kept as a chain of skip deltas; export the youngest revision alone in format 2 through a source that counts what
/// each call asks of it. Expect: the dump carries the text; no call reads a text whole, and none takes more than one
/// bounded step of reading (a window of the chain); and the reading took at least a step for each window of the chain,
/// so it walked the chain rather than skipping it. Before, the call that wrote the text's first piece read it whole
/// (512 KiB, a chain of 6: that call 6.18 ms against a whole read of 6.04 ms, debug, this Mac).
#[test]
fn an_export_rebuilds_a_long_delta_chain_a_window_a_call() {
  use std::cell::Cell;

  use crate::dump::Format;
  use crate::migration::{Exporter, Exporting, TextReading, TextSource};
  use crate::svndiff::WINDOW_LIMIT;
  /// The records as a text source, counting the whole reads and the steps asked of it.
  struct Counted<'a> {
    records: &'a Repository,
    whole: Cell<u64>,
    steps: Cell<u64>,
  }
  impl TextSource for Counted<'_> {
    fn text(&self, identity: &ContentIdentity) -> Option<std::borrow::Cow<'_, [u8]>> {
      self.whole.set(self.whole.get() + 1);
      TextSource::text(self.records, identity)
    }

    fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
      self.steps.set(self.steps.get() + 1);
      TextSource::read_step(self.records, identity, reading)
    }
  }
  const LEN: usize = 512 * 1024;
  let mut bytes: Vec<u8> = (0..LEN).map(|at| (at % 251) as u8).collect();
  let mut repository = created();
  let mut opening = first();
  let (alpha, _) = content(b"alpha\n");
  opening.contents.remove(&alpha);
  let (identity, held) = content(&bytes);
  opening.contents.insert(identity, held);
  if let Some(file) = opening
    .record
    .changes
    .iter_mut()
    .find(|row| row.path == FILE)
  {
    file.text = Some(text_of(&bytes));
  }
  let checked = repository.check(opening).unwrap();
  repository.apply(checked).unwrap();
  for head in 1..32u64 {
    let at = usize::try_from(head).unwrap() * 4_099 % LEN;
    bytes[at] = bytes[at].wrapping_add(1);
    let checked = repository.check(edit(head, &bytes, Vec::new())).unwrap();
    repository.apply(checked).unwrap();
  }
  let youngest = repository.youngest();
  let identity = *blake3::hash(&bytes).as_bytes();
  let chain = u64::try_from(repository.text_chain(&identity).unwrap()).unwrap();
  assert!(chain >= 5, "a chain of {chain}");
  let counted = Counted {
    records: &repository,
    whole: Cell::new(0),
    steps: Cell::new(0),
  };
  let mut exporter = Exporter::new(&repository, youngest, youngest, Format::Two).unwrap();
  let mut dump = Vec::new();
  let mut most_steps = 0;
  loop {
    let before = counted.steps.get();
    let exported = exporter.export(&repository, &counted, &mut dump).unwrap();
    most_steps = most_steps.max(counted.steps.get() - before);
    if exported == Exporting::Done {
      break;
    }
  }
  let windows = chain * u64::try_from(LEN).unwrap().div_ceil(WINDOW_LIMIT);
  assert!(
    dump
      .windows(bytes.len())
      .any(|window| window == bytes.as_slice())
  );
  assert_eq!((counted.whole.get(), most_steps), (0, 1));
  assert!(
    counted.steps.get() >= windows,
    "{} steps for {windows} windows",
    counted.steps.get()
  );
}
