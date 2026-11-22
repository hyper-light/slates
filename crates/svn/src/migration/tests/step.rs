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
use crate::migration::{Repository, Room, Standalone};

/// The date of the first revision the tests commit.
const FIRST_DATE: &str = "2026-11-19T07:00:00.000000Z";

/// The file every revision after the first changes.
const FILE: &str = "/trunk/a.txt";

/// What the tests compare of a repository, whole: its history (every revision's properties, transaction and changed
/// paths, and every content they name), its index as it prints, the bytes it holds and its locks.
type Snapshot = (History, String, u64, Vec<Lock>);

/// `repository`, whole.
fn snapshot(repository: &Standalone) -> Snapshot {
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
fn owners_history(repository: &mut Standalone) -> Result<(), StepRefusal> {
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

/// The KeptText move (main's 10:35 item 3 of 2026-11-21; module docs of the records, "Texts"). Do: commit [`first`],
/// whose text `alpha\n` it carries inline, into bare records, not taking its texts out first; then the same revision
/// through [`Standalone`], which takes them out. Expect: the bare records refuse it `Corrupt` (`E160004`) and are as
/// they were, keeping no text; the standalone ones commit revision 1, the text named stored by its length and held
/// apart, the records holding none of its bytes. Before, the records kept the text as a delta of their own.
#[test]
fn a_text_carried_inline_is_refused_and_records_standing_alone_hold_it_apart() {
  let mut repository = created();
  let before = snapshot(&repository);
  let refusal = repository.records_mut().commit(first()).unwrap_err();
  assert_eq!(
    (refusal.subversion_code(), refusal),
    (160_004, StepRefusal::Corrupt)
  );
  assert_eq!(snapshot(&repository), before);
  let (alpha, alpha_bytes) = content(b"alpha\n");
  assert_eq!(repository.commit(first()), Ok(1));
  assert_eq!(
    (
      repository.stored_len(&alpha),
      repository.records().text(&alpha),
      repository.text(&alpha).map(std::borrow::Cow::into_owned),
    ),
    (
      Some(u64::try_from(alpha_bytes.len()).unwrap()),
      None,
      Some(alpha_bytes)
    )
  );
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
  // Its texts are stored, held apart: the room holds its properties and property lists.
  let mut revision = first();
  let records = created();
  let _ = records.take_texts(&mut revision);
  let needed = records.held() + added(&revision);
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

/// C2 stage B part 2 (the export reads a stored text a store reads asynchronously, waiting for each piece). Do: commit
/// [`first`] with its text `alpha\n` stored beside the records, and export it whole in formats 2 and 3 through a source
/// that holds none of its pieces until the export, having waited for one ([`Exporting::Waiting`]), has it fetched: two
/// bytes a piece, kept once fetched. Expect: the dump byte for byte the one of the text kept in the records; each wait
/// for the piece the reading had reached and appending nothing, three in all for the six bytes (any later reading of
/// the text finds its pieces fetched).
#[test]
fn an_export_waits_for_each_piece_of_a_stored_text_and_writes_the_same_dump() {
  use std::cell::RefCell;
  use std::collections::BTreeSet;

  use crate::dump::Format;
  use crate::migration::{Exporter, Exporting, TextReading, TextSource};
  /// Shape: the bytes a fetched piece holds.
  const PIECE: u64 = 2;
  /// The records, and one text stored beside them whose pieces are read only once fetched.
  struct Withheld<'a> {
    records: &'a Repository,
    stored: (ContentIdentity, Vec<u8>),
    fetched: RefCell<BTreeSet<u64>>,
  }
  impl TextSource for Withheld<'_> {
    fn text(&self, identity: &ContentIdentity) -> Option<std::borrow::Cow<'_, [u8]>> {
      if *identity == self.stored.0 {
        return None;
      }
      TextSource::text(self.records, identity)
    }

    fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
      if *identity != self.stored.0 {
        return TextSource::read_step(self.records, identity, reading);
      }
      let (read, len) = (reading.read(), u64::try_from(self.stored.1.len()).unwrap());
      if read >= len {
        return Some(true);
      }
      if !self.fetched.borrow().contains(&read) {
        return None;
      }
      let end = (read + PIECE).min(len);
      let piece = &self.stored.1[usize::try_from(read).unwrap()..usize::try_from(end).unwrap()];
      Some(reading.append(piece, len))
    }

    fn fetchable(&self, identity: &ContentIdentity, offset: u64) -> bool {
      *identity == self.stored.0 && !self.fetched.borrow().contains(&offset)
    }
  }
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
  for format in [Format::Two, Format::Three] {
    let mut whole = Exporter::new(&kept, 0, kept.youngest(), format).unwrap();
    let mut expected = Vec::new();
    while whole.export(&kept, &kept, &mut expected).unwrap() != Exporting::Done {}
    let withheld = Withheld {
      records: &stored,
      stored: (identity, bytes.clone()),
      fetched: RefCell::new(BTreeSet::new()),
    };
    let mut exporter = Exporter::new(&stored, 0, stored.youngest(), format).unwrap();
    let (mut out, mut waits) = (Vec::new(), Vec::new());
    loop {
      let before = out.len();
      match exporter.export(&stored, &withheld, &mut out).unwrap() {
        Exporting::Waiting(wanted) => {
          assert_eq!(out.len(), before, "a waiting call appended");
          assert_eq!(wanted.text, identity);
          waits.push(wanted.offset);
          withheld.fetched.borrow_mut().insert(wanted.offset);
        }
        Exporting::Wrote(_) => {}
        Exporting::Done => break,
      }
    }
    assert_eq!(out, expected, "{format:?}");
    assert_eq!(waits, vec![0, 2, 4], "{format:?}");
  }
}

/// C2 stage B part 2 (b), P4 (an import's delta whose base the records keep elsewhere asks its caller for the base).
/// Do: commit [`first`] into records standing alone, its text `alpha\n` held apart (the records naming it stored);
/// import a format-3 stream whose revision 2 changes [`FILE`] by a delta against that text (a window copying it and
/// adding `beta\n`), supplying the base whole when the import asks for it. Expect: the import asks once, for
/// `alpha\n`'s identity, before any record is made, and takes it; revision 2 imported with the text `alpha\nbeta\n`,
/// held apart and named stored; and a base the import did not ask for, or bytes not its identity's, refused `Corrupt`.
#[test]
fn an_import_asks_its_caller_for_a_delta_base_the_records_keep_elsewhere() {
  use super::support::{Dump, revision_properties};
  use crate::dump::{DeltaBase, Format};
  use crate::migration::{Importer, Importing, MigrationRefusal};
  use crate::svndiff::{Instruction, Window};
  let (alpha, alpha_bytes) = content(b"alpha\n");
  let made = b"alpha\nbeta\n";
  let window = Window {
    source_offset: 0,
    source_len: 6,
    target_len: 11,
    instructions: vec![
      Instruction::Source {
        offset: 0,
        length: 6,
      },
      Instruction::New { length: 5 },
    ],
    new_data: b"beta\n".to_vec(),
  };
  let base = digests(b"alpha\n");
  let mut dump = Dump::new(Format::Three, None);
  dump.revision(2, &revision_properties(2)).delta(
    FILE,
    DeltaBase {
      md5: Some(base.0),
      sha1: Some(base.1),
    },
    &[window],
    digests(made),
  );
  let stream = dump.bytes();
  // Imports `stream` into `repository`, supplying the base when asked: the bases asked for, and revision 2's text.
  let imported = |repository: &mut Standalone| {
    let mut importer = Importer::new(super::support::ADMISSION);
    let mut rest = stream.as_slice();
    let mut asked = Vec::new();
    loop {
      let (records, texts) = repository.importing();
      match importer.import(records, texts, rest, true).unwrap() {
        Importing::Base { consumed, text } => {
          asked.push(text);
          assert_eq!(
            importer.supply_base(repository.records(), &text, b"not it".to_vec()),
            Err(MigrationRefusal::Corrupt)
          );
          importer
            .supply_base(repository.records(), &text, alpha_bytes.clone())
            .unwrap();
          rest = &rest[consumed..];
        }
        Importing::Commit { consumed, record } => {
          rest = &rest[consumed..];
          repository.commit_record(*record).unwrap();
        }
        Importing::NeedMore { .. } => panic!("the whole stream was given"),
        Importing::Done => break,
      }
    }
    let identity = *blake3::hash(made).as_bytes();
    let text = repository.text(&identity).map(std::borrow::Cow::into_owned);
    (
      asked,
      repository.youngest(),
      text,
      repository.stored_len(&identity),
    )
  };
  let mut stored = created();
  stored.commit(first()).unwrap();
  assert_eq!(stored.stored_len(&alpha), Some(6));
  let made_len = u64::try_from(made.len()).unwrap();
  assert_eq!(
    imported(&mut stored),
    (vec![alpha], 2, Some(made.to_vec()), Some(made_len))
  );
  let mut unasked = Importer::new(super::support::ADMISSION);
  assert_eq!(
    unasked.supply_base(stored.records(), &alpha, alpha_bytes),
    Err(MigrationRefusal::Corrupt)
  );
}
