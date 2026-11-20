//! A-629 part 3: an LFS object's record, its golden bytes and every malformation refused typed.

use slates_git::lfs::Oid;
use slates_git::lfs::objects::LfsNaming;

use super::{HEAD_BYTES, LfsRecord, LfsRecordRefused, decode, encode, is_lfs};

/// Shape: an object's bytes.
const BYTES: &[u8] = b"hello world\n";

/// Shape: the record of [`BYTES`], written out by hand from the module docs' encoding: "SGLF", version 1, the oid
/// (SHA-256 of `hello world\n`), the length 12 and the bytes.
const GOLDEN: &[u8] = b"SGLF\x01\x00\x00\x00\
\xa9\x48\x90\x4f\x2f\x0f\x47\x9b\x8f\x81\x97\x69\x4b\x30\x18\x4b\
\x0d\x2e\xd1\xc1\xcd\x2a\x1e\xc0\xfb\x85\xd2\x99\xa1\x92\xa4\x47\
\x0c\x00\x00\x00\x00\x00\x00\x00hello world\n";

/// [`BYTES`]' oid.
fn oid() -> Oid {
  LfsNaming::oid(BYTES)
}

/// A-629 part 3 (golden vector). Do: encode `hello world\n`'s record, then decode it. Expect the bytes the module docs
/// describe, byte for byte, read back as the same oid and bytes.
#[test]
fn an_lfs_record_is_its_documented_bytes_and_reads_back() {
  let record = encode(&oid(), BYTES).unwrap();
  assert_eq!(record, GOLDEN);
  assert_eq!(
    decode(&record),
    Ok(LfsRecord {
      oid: oid(),
      bytes: BYTES
    })
  );
}

/// A-629 part 3. Do: ask whether an LFS record, a git push's record's first word, and too few bytes are LFS records.
/// Expect the first alone: a git repository's two kinds of record are told apart by their first word.
#[test]
fn an_lfs_record_is_told_from_a_push_record_by_its_first_word() {
  assert_eq!(
    [
      is_lfs(GOLDEN),
      is_lfs(b"SGPR\x01\x00\x00\x00"),
      is_lfs(b"SGL")
    ],
    [true, false, false]
  );
}

/// A-629 part 3 (hostile input). Do: decode the golden record cut short at every length, with its magic and its
/// version changed, its length past the bytes, its length at `u64::MAX`, and a byte after the object. Expect each
/// refused by its kind, never a panic.
#[test]
fn every_malformed_lfs_record_is_refused_by_its_kind() {
  for cut in 0..GOLDEN.len() {
    assert_eq!(
      decode(GOLDEN.get(..cut).unwrap()),
      Err(LfsRecordRefused::Truncated),
      "{cut}"
    );
  }
  let changed = |at: usize, byte: u8| {
    let mut record = GOLDEN.to_vec();
    *record.get_mut(at).unwrap() = byte;
    record
  };
  let length_at = HEAD_BYTES - size_of::<u64>();
  let mut huge = GOLDEN.to_vec();
  huge
    .get_mut(length_at..HEAD_BYTES)
    .unwrap()
    .copy_from_slice(&u64::MAX.to_le_bytes());
  let mut trailing = GOLDEN.to_vec();
  trailing.push(0);
  assert_eq!(
    [
      decode(&changed(0, b'X')),
      decode(&changed(4, 2)),
      decode(&changed(length_at, 13)),
      decode(&huge),
      decode(&trailing),
    ],
    [
      Err(LfsRecordRefused::Magic),
      Err(LfsRecordRefused::Version { version: 2 }),
      Err(LfsRecordRefused::Truncated),
      Err(LfsRecordRefused::Truncated),
      Err(LfsRecordRefused::Trailing),
    ]
  );
}
