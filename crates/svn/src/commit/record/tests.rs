//! A-655 part 2b's record (§4.17.4 item 3.5): its bytes pinned by hand, read back whole, and every hostile input
//! refused by kind, never a panic.

use std::collections::BTreeMap;

use super::{Record, RecordRefused};
use crate::commit::revision::Revision;
use crate::dump::Uuid;
use crate::index::{Action, ChangedPath, CopySource, NodeKind, PropertyList, RevisionRecord, Text};
use crate::lock::{AprTime, Lock, LockEntry, LockToken};

/// Shape: the UUID the tests' repository has.
const UUID: &[u8; 36] = b"6b4b2a05-7c19-4f3e-9a10-2f5e3c1d0b7a";

/// Shape: revision 0's date.
const DATE: &[u8] = b"2026-11-19T07:00:00.000000Z";

/// Shape: the bytes a content of the tests' revision holds.
const HELLO: &[u8] = b"hello";

/// A count, a length, a revision or a size as the record writes it.
fn long(value: u64) -> [u8; 8] {
  value.to_le_bytes()
}

/// A length and its bytes as the record writes them.
fn string(bytes: &[u8]) -> Vec<u8> {
  [long(u64::try_from(bytes.len()).unwrap()).as_slice(), bytes].concat()
}

/// The tests' creation record.
fn creation() -> Record {
  Record::Created {
    uuid: Uuid::parse(UUID).unwrap(),
    date: DATE.to_vec(),
  }
}

/// A token as a request supplies it.
fn token(text: &str) -> LockToken {
  LockToken::supplied(text).unwrap()
}

/// The tests' revision: one row of each part, a property, a content, a lock removed and one set.
fn revision() -> Revision {
  let row = ChangedPath {
    path: "/a".to_owned(),
    action: Action::Modify,
    kind: NodeKind::File,
    copy_from: None,
    text: Some(Text {
      identity: [0x11; 32],
      md5: [0x22; 16],
      sha1: [0x33; 20],
      size: 5,
    }),
    properties: Some(PropertyList::Held {
      identity: [0x44; 32],
      has_mergeinfo: false,
    }),
    mergeinfo_modified: false,
  };
  let set = Lock {
    path: "/b".to_owned(),
    token: token("opaquelocktoken:y"),
    owner: "bob".to_owned(),
    comment: Some("c".to_owned()),
    is_dav_comment: true,
    created: AprTime(5),
    expires: None,
  };
  Revision {
    head: 1,
    record: RevisionRecord {
      transaction: "1-2".to_owned(),
      changes: vec![row],
    },
    properties: BTreeMap::from([(b"svn:log".to_vec(), b"x".to_vec())]),
    contents: BTreeMap::from([(*blake3::hash(HELLO).as_bytes(), HELLO.to_vec())]),
    stored: BTreeMap::from([([0x55; 32], 7)]),
    lock_entries: vec![
      LockEntry::Removed {
        path: "/a".to_owned(),
        token: token("opaquelocktoken:x"),
      },
      LockEntry::Set(set),
    ],
  }
}

/// The tests' revision as the record lays it out, written by hand from the module documentation.
fn revision_bytes() -> Vec<u8> {
  let header: Vec<u8> = [b"SSVR".as_slice(), &2u32.to_le_bytes(), &[1]].concat();
  let text: Vec<u8> = [[0x11u8; 32].as_slice(), &[0x22; 16], &[0x33; 20], &long(5)].concat();
  let path = string(b"/a");
  let row: Vec<u8> = [
    path.as_slice(),
    &[3, 0, 0, 1],
    text.as_slice(),
    &[2],
    &[0x44; 32],
    &[0, 0],
  ]
  .concat();
  let removed: Vec<u8> = [
    [1u8].as_slice(),
    string(b"/a").as_slice(),
    string(b"opaquelocktoken:x").as_slice(),
  ]
  .concat();
  let set: Vec<u8> = [
    [0u8].as_slice(),
    string(b"/b").as_slice(),
    string(b"opaquelocktoken:y").as_slice(),
    string(b"bob").as_slice(),
    &[1],
    string(b"c").as_slice(),
    &[1],
    &5i64.to_le_bytes(),
    &[0],
  ]
  .concat();
  [
    header.as_slice(),
    &long(1),
    string(b"1-2").as_slice(),
    &long(1),
    row.as_slice(),
    &long(1),
    string(b"svn:log").as_slice(),
    string(b"x").as_slice(),
    &long(1),
    blake3::hash(HELLO).as_bytes(),
    string(HELLO).as_slice(),
    &long(1),
    &[0x55; 32],
    &long(7),
    &long(2),
    removed.as_slice(),
    set.as_slice(),
  ]
  .concat()
}

/// A-655 part 2b (the record's encoding, the module documentation). Do: encode the tests' creation and revision.
/// Expect: each byte for byte as laid out by hand, and each read back equal.
#[test]
fn t_a655_the_records_bytes_are_pinned_and_read_back() {
  let created: Vec<u8> = [
    b"SSVR".as_slice(),
    &2u32.to_le_bytes(),
    &[0],
    UUID,
    string(DATE).as_slice(),
  ]
  .concat();
  let revision = Record::Revision(revision());
  assert_eq!(
    (creation().encode(), revision.encode()),
    (created.clone(), revision_bytes())
  );
  let read = (Record::decode(&created), Record::decode(&revision_bytes()));
  assert_eq!(read, (Ok(creation()), Ok(revision)));
}

/// A revision of every row shape: an addition by copy, a deletion, a replacement leaving no properties, a directory
/// whose list holds mergeinfo; an expiring lock with no comment; no lock removed.
fn rows_of_every_shape() -> Revision {
  let copied = ChangedPath {
    path: "/branches/b".to_owned(),
    action: Action::Add,
    kind: NodeKind::Directory,
    copy_from: Some(CopySource {
      path: "/trunk".to_owned(),
      revision: 7,
    }),
    text: None,
    properties: Some(PropertyList::Held {
      identity: [9; 32],
      has_mergeinfo: true,
    }),
    mergeinfo_modified: true,
  };
  let deleted = ChangedPath {
    path: "/old".to_owned(),
    action: Action::Delete,
    kind: NodeKind::Directory,
    copy_from: None,
    text: None,
    properties: None,
    mergeinfo_modified: false,
  };
  let replaced = ChangedPath {
    path: "/trunk/é".to_owned(),
    action: Action::Replace,
    properties: Some(PropertyList::Empty),
    kind: NodeKind::File,
    ..deleted.clone()
  };
  let lock = Lock {
    path: "/trunk/é".to_owned(),
    token: token("opaquelocktoken:z"),
    owner: "carol".to_owned(),
    comment: None,
    is_dav_comment: false,
    created: AprTime(-1),
    expires: Some(AprTime(i64::MAX)),
  };
  Revision {
    head: u64::MAX - 1,
    record: RevisionRecord {
      transaction: "9-a".to_owned(),
      changes: vec![copied, deleted, replaced],
    },
    properties: BTreeMap::new(),
    contents: BTreeMap::new(),
    stored: BTreeMap::new(),
    lock_entries: vec![LockEntry::Set(lock)],
  }
}

/// A-655 part 2b (one value per record: the encoding canonical). Do: encode a revision of every row shape and an empty
/// one, decode each, and encode what was read. Expect: each read back equal, its bytes again the same, and each encoding
/// reserved at its exact length (A-742).
#[test]
fn t_a655_every_shape_reads_back_and_encodes_again_the_same() {
  let empty = Revision {
    head: 0,
    record: RevisionRecord {
      transaction: String::new(),
      changes: Vec::new(),
    },
    properties: BTreeMap::new(),
    contents: BTreeMap::new(),
    stored: BTreeMap::new(),
    lock_entries: Vec::new(),
  };
  for record in [
    Record::Revision(rows_of_every_shape()),
    Record::Revision(revision()),
    Record::Revision(empty),
  ] {
    let bytes = record.encode();
    // A-742: reserved at its exact length once, never grown.
    assert_eq!(bytes.capacity(), bytes.len());
    let read = Record::decode(&bytes);
    let again = read
      .as_ref()
      .map(Record::encode)
      .map_err(|refused| *refused);
    assert_eq!((read, again), (Ok(record), Ok(bytes)));
  }
}

/// A-655 part 2b (hostile inputs: truncated headers). Do: decode every prefix of the tests' revision and creation
/// records shorter than the record. Expect: each refused, `Truncated` or `Count`, never read as a record.
#[test]
fn t_a655_every_truncation_is_refused() {
  for bytes in [revision_bytes(), creation().encode()] {
    for end in 0..bytes.len() {
      let refused = Record::decode(&bytes[..end]);
      assert!(
        matches!(
          refused,
          Err(RecordRefused::Truncated | RecordRefused::Count)
        ),
        "{end}: {refused:?}"
      );
    }
  }
}

/// `bytes` with the bytes at `at` replaced by `with`.
fn patched(bytes: &[u8], at: usize, with: &[u8]) -> Vec<u8> {
  let mut patched = bytes.to_vec();
  patched.splice(at..at + with.len(), with.iter().copied());
  patched
}

/// Where the tests' revision record's parts begin.
struct Places {
  /// The rows' count.
  rows: usize,
  /// The first row's path's length.
  path: usize,
  /// The first row's copy mark.
  copy_mark: usize,
  /// The content's first byte.
  content: usize,
  /// The first lock entry's token's first byte.
  token: usize,
}

/// The places of the tests' revision record (`revision_bytes`'s layout).
fn places() -> Places {
  let header = 4 + 4 + 1;
  let rows = header + 8 + 8 + 3;
  let path = rows + 8;
  let copy_mark = path + 8 + 2 + 2;
  let row_end = path + 8 + 2 + 4 + (32 + 16 + 20 + 8) + 1 + 32 + 2;
  let properties_end = row_end + 8 + (8 + 7) + (8 + 1);
  let content = properties_end + 8 + 32 + 8;
  // The stored texts' count and one entry (identity, length) lie between the content and the lock entries.
  let token = content + 5 + (8 + 32 + 8) + 8 + 1 + (8 + 2) + 8;
  Places {
    rows,
    path,
    copy_mark,
    content,
    token,
  }
}

/// A-655 part 2b (hostile inputs, the module documentation's refusals). Do: decode the tests' records with another
/// magic, another version, a kind of none, a count and a length of `u64::MAX`, a mark of none, a path that is not
/// UTF-8, a UUID with no dash, a token not `opaquelocktoken:`, a content not its identity, two properties out of order,
/// and a byte after the last item. Expect: each refused by its kind.
#[test]
fn t_a655_hostile_records_are_refused_by_kind() {
  let revision = revision_bytes();
  let places = places();
  let unordered = Record::Revision(revision_unordered_base()).encode();
  let cases = [
    (patched(&revision, 0, b"SGPR"), RecordRefused::Magic),
    (
      patched(&revision, 4, &3u32.to_le_bytes()),
      RecordRefused::Version { version: 3 },
    ),
    (patched(&revision, 8, &[7]), RecordRefused::Tag { byte: 7 }),
    (
      patched(&revision, places.rows, &long(u64::MAX)),
      RecordRefused::Count,
    ),
    (
      patched(&revision, places.path, &long(u64::MAX)),
      RecordRefused::Truncated,
    ),
    (
      patched(&revision, places.copy_mark, &[2]),
      RecordRefused::Tag { byte: 2 },
    ),
    (
      patched(&revision, places.path + 8, &[0xff]),
      RecordRefused::Utf8,
    ),
    (
      patched(&creation().encode(), 9 + 8, b"x"),
      RecordRefused::Uuid,
    ),
    (patched(&revision, places.token, b"x"), RecordRefused::Token),
    (
      patched(&revision, places.content, b"j"),
      RecordRefused::Identity,
    ),
    (unordered_properties(&unordered), RecordRefused::Order),
    (
      [revision.as_slice(), &[0u8]].concat(),
      RecordRefused::Trailing,
    ),
  ];
  for (bytes, refusal) in cases {
    assert_eq!(Record::decode(&bytes), Err(refusal), "{refusal:?}");
  }
}

/// A revision with no row, content or lock entry, whose properties the hostile test writes by hand.
fn revision_unordered_base() -> Revision {
  Revision {
    head: 0,
    record: RevisionRecord {
      transaction: String::new(),
      changes: Vec::new(),
    },
    properties: BTreeMap::new(),
    contents: BTreeMap::new(),
    stored: BTreeMap::new(),
    lock_entries: Vec::new(),
  }
}

/// `empty`, a record of a revision with no properties, given the properties `b` then `a`, out of their order.
fn unordered_properties(empty: &[u8]) -> Vec<u8> {
  let before = 4 + 4 + 1 + 8 + 8 + 8;
  let properties: Vec<u8> = [
    long(2).as_slice(),
    string(b"b").as_slice(),
    string(b"1").as_slice(),
    string(b"a").as_slice(),
    string(b"2").as_slice(),
  ]
  .concat();
  [
    &empty[..before],
    properties.as_slice(),
    &empty[before + 8..],
  ]
  .concat()
}

/// A-655 part 2b (hostile inputs: bit flips). Do: decode the tests' revision record with each byte's bits all
/// flipped in turn. Expect: never a panic; what is read, when anything is, encodes to the bytes read, the encoding
/// being canonical.
#[test]
fn t_a655_a_flipped_byte_never_panics_and_what_reads_is_canonical() {
  let bytes = revision_bytes();
  for at in 0..bytes.len() {
    let flipped = patched(&bytes, at, &[!bytes[at]]);
    if let Ok(record) = Record::decode(&flipped) {
      assert_eq!(record.encode(), flipped, "byte {at}");
    }
  }
}

/// The tests' record of the repository named by [`UUID`] (A-652 part 3).
fn uuid_set() -> Record {
  Record::Uuid(Uuid::parse(UUID).unwrap())
}

/// The tests' record of revision 0's properties set whole: its date and an empty log (A-652 part 3).
fn properties_set() -> Record {
  Record::RevisionProperties {
    revision: 0,
    properties: BTreeMap::from([
      (b"svn:date".to_vec(), DATE.to_vec()),
      (b"svn:log".to_vec(), Vec::new()),
    ]),
  }
}

/// A record's first bytes: the magic, the version and the kind byte `kind`.
fn head_of(kind: u8) -> Vec<u8> {
  [b"SSVR".as_slice(), &2u32.to_le_bytes(), &[kind]].concat()
}

/// The bytes of revision 0's properties set to the two entries `first` and `second`, in that order.
fn properties_set_bytes(first: &[u8], second: &[u8]) -> Vec<u8> {
  [head_of(3).as_slice(), &long(0), &long(2), first, second].concat()
}

/// The entries of [`properties_set`], as the record writes each: its date's, then its log's.
fn property_entries() -> (Vec<u8>, Vec<u8>) {
  (
    [string(b"svn:date"), string(DATE)].concat(),
    [string(b"svn:log"), string(b"")].concat(),
  )
}

/// A-652 part 3 (the records an import sets beside its revisions, the module documentation). Do: encode the tests'
/// UUID record and revision 0's properties set whole, and decode each. Expect: each byte for byte as laid out by hand
/// (the kind byte 2 and the UUID's 36 bytes; the kind byte 3, the revision, the count and each entry in name order),
/// and each read back equal.
#[test]
fn t_a652_a_uuid_and_a_revisions_properties_are_pinned_and_read_back() {
  let (date, log) = property_entries();
  let uuid_bytes = [head_of(2).as_slice(), UUID].concat();
  let set_bytes = properties_set_bytes(&date, &log);
  assert_eq!(
    (uuid_set().encode(), properties_set().encode()),
    (uuid_bytes.clone(), set_bytes.clone())
  );
  assert_eq!(
    (Record::decode(&uuid_bytes), Record::decode(&set_bytes)),
    (Ok(uuid_set()), Ok(properties_set()))
  );
}

/// A-652 part 3 (hostile inputs: truncated headers). Do: decode every prefix of the two records shorter than each.
/// Expect: each refused, `Truncated` or `Count`, never read as a record.
#[test]
fn t_a652_every_truncation_of_either_record_is_refused() {
  for bytes in [uuid_set().encode(), properties_set().encode()] {
    for end in 0..bytes.len() {
      let refused = Record::decode(&bytes[..end]);
      assert!(
        matches!(
          refused,
          Err(RecordRefused::Truncated | RecordRefused::Count)
        ),
        "{end}: {refused:?}"
      );
    }
  }
}

/// A-652 part 3 (hostile inputs, by kind). Do: decode the properties' record with its names out of order and with a
/// byte after its last, and the UUID's record with a UUID not in RFC 4122's form. Expect: `Order`, `Trailing` and
/// `Uuid`.
#[test]
fn t_a652_hostile_uuid_and_properties_records_are_refused_by_kind() {
  let (date, log) = property_entries();
  let unordered = properties_set_bytes(&log, &date);
  let trailing = [properties_set().encode().as_slice(), &[0]].concat();
  let undashed = UUID.map(|byte| if byte == b'-' { b'0' } else { byte });
  let dashless = [head_of(2).as_slice(), &undashed].concat();
  let refused = [unordered, trailing, dashless].map(|bytes| Record::decode(&bytes));
  assert_eq!(
    refused,
    [
      Err(RecordRefused::Order),
      Err(RecordRefused::Trailing),
      Err(RecordRefused::Uuid)
    ]
  );
}

/// A-652 part 3 (hostile inputs: bit flips). Do: decode the two records with each byte's bits all flipped in turn.
/// Expect: never a panic; what is read, when anything is, encodes to the bytes read.
#[test]
fn t_a652_a_flipped_byte_of_either_record_never_panics_and_what_reads_is_canonical() {
  for bytes in [uuid_set().encode(), properties_set().encode()] {
    for at in 0..bytes.len() {
      let flipped = patched(&bytes, at, &[!bytes[at]]);
      if let Ok(record) = Record::decode(&flipped) {
        assert_eq!(record.encode(), flipped, "byte {at}");
      }
    }
  }
}

/// The record's version 2 (§4.17.1 "A push's intake"): a revision names the texts its commit stored. Do: read the tests'
/// revision written as version 1 wrote it, with no stored texts' count. Expect: read back as the revision with no
/// stored texts, so a log written before version 2 replays.
#[test]
fn a_version_one_revision_reads_back_with_no_stored_texts() {
  let current = revision_bytes();
  let places = places();
  // The stored texts' count follows the content's bytes.
  let stored_at = places.content + HELLO.len();
  let older: Vec<u8> = [
    b"SSVR".as_slice(),
    &1u32.to_le_bytes(),
    &current[8..stored_at],
    &current[stored_at + 8 + 32 + 8..],
  ]
  .concat();
  let expected = Revision {
    stored: BTreeMap::new(),
    ..revision()
  };
  assert_eq!(Record::decode(&older), Ok(Record::Revision(expected)));
}
