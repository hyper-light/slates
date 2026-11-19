//! What a client's accepted work becomes as the revision the step commits (A-655, §4.17.4 item 3.5): dated by the
//! owner's instant as `svn_time_to_cstring` writes it, its properties, contents and lock entries the work's.

use std::collections::BTreeMap;

use super::*;
use crate::index::{Action, ChangedPath, NodeKind, PropertyList};
use crate::lock::LockToken;
use crate::migration::properties::decode;

/// The owner's instant the tests date a revision at: 2026-11-19T07:26:35.123456Z, in microseconds since the epoch.
/// Shape: 1,795,073,195 seconds and 123,456 microseconds, computed from the calendar date.
const NOW: AprTime = AprTime(1_795_073_195_123_456);

/// A text's identity in the work: any the green could name, distinct from the list's.
/// Shape: a content identity's 32 bytes, each 7.
const TEXT_IDENTITY: ContentIdentity = [7; 32];

/// A property list's identity in the work.
/// Shape: a content identity's 32 bytes, each 9.
const LIST_IDENTITY: ContentIdentity = [9; 32];

/// A work changing `/trunk/a.txt`'s text and properties, its client having sent its own `svn:date`, releasing one lock.
fn work() -> Work {
  let change = ChangedPath {
    path: "/trunk/a.txt".to_owned(),
    action: Action::Modify,
    kind: NodeKind::File,
    copy_from: None,
    text: None,
    properties: Some(PropertyList::Held {
      identity: LIST_IDENTITY,
      has_mergeinfo: false,
    }),
    mergeinfo_modified: false,
  };
  Work {
    head: 5,
    record: RevisionRecord {
      transaction: "5-1".to_owned(),
      changes: vec![change],
    },
    lock_entries: vec![LockEntry::Removed {
      path: "/trunk/a.txt".to_owned(),
      token: LockToken::supplied("opaquelocktoken:x").unwrap(),
    }],
    revision_properties: vec![
      (b"svn:author".to_vec(), b"alice".to_vec()),
      (
        b"svn:date".to_vec(),
        b"1999-01-01T00:00:00.000000Z".to_vec(),
      ),
      (b"svn:log".to_vec(), b"an edit".to_vec()),
    ],
    texts: BTreeMap::from([(TEXT_IDENTITY, b"alpha\n".to_vec())]),
    property_lists: BTreeMap::from([(
      LIST_IDENTITY,
      vec![(b"svn:eol-style".to_vec(), b"native".to_vec())],
    )]),
  }
}

/// A-655 (`transaction.c` `write_final_revprop` 3617-3630; `fs-wrap.c` 151-152; `time.c` `svn_time_to_cstring`). Do:
/// make the work into a revision at the owner's instant 2026-11-19T07:26:35.123456Z. Expect: its `svn:date` that instant
/// as `svn_time_to_cstring` writes it, in place of the date the client sent; its author and log the work's; the date
/// and author `commit-info` carries read back.
#[test]
fn a_work_is_dated_by_the_owners_instant_at_the_step() {
  let revision = Revision::of_work(work(), NOW).unwrap();
  let expected = Properties::from([
    (b"svn:author".to_vec(), b"alice".to_vec()),
    (
      b"svn:date".to_vec(),
      b"2026-11-19T07:26:35.123456Z".to_vec(),
    ),
    (b"svn:log".to_vec(), b"an edit".to_vec()),
  ]);
  assert_eq!(revision.properties, expected);
  let answered = (revision.date(), revision.author());
  assert_eq!(
    answered,
    (Some("2026-11-19T07:26:35.123456Z"), Some("alice"))
  );
}

/// A-655 (§4.17.4 item 3.5; `hash.c` `svn_hash_write2`). Do: make the work into a revision. Expect: its text held as
/// its bytes and its property list as FSFS keeps one, each under the identity its row names; its head, its record and
/// its lock entries the work's.
#[test]
fn a_works_contents_and_entries_are_the_revisions() {
  let made = work();
  let revision = Revision::of_work(made.clone(), NOW).unwrap();
  let text = revision.contents.get(&TEXT_IDENTITY).cloned();
  assert_eq!(text, Some(b"alpha\n".to_vec()));
  let list = revision
    .contents
    .get(&LIST_IDENTITY)
    .map(Vec::as_slice)
    .and_then(decode);
  let expected = Properties::from([(b"svn:eol-style".to_vec(), b"native".to_vec())]);
  assert_eq!(list, Some(expected));
  let kept = (revision.head, revision.record, revision.lock_entries);
  assert_eq!(kept, (made.head, made.record, made.lock_entries));
}

/// A-655 (module documentation). Do: make the work into a revision at an instant before 1970. Expect: refused
/// `Date`, answered `E160000` "General filesystem error".
#[test]
fn an_instant_before_1970_is_refused() {
  let before = AprTime(-1);
  let refusal = Revision::of_work(work(), before).unwrap_err();
  let answered = (refusal.subversion_code(), refusal.message());
  assert_eq!(answered, (160_000, "General filesystem error".to_owned()));
  assert_eq!(refusal, StepRefusal::Date { now: before });
}
