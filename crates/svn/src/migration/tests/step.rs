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
