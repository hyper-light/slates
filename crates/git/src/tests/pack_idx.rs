//! A-591: the pack index, version 2, held to the indexes and `git verify-pack -v` listings stock git 2.50.1 made
//! (`fixtures/make-fixtures.sh` and `fixtures/make-index-fixtures.sh`, run on the Mac Studio on 2026-11-19), to hostile
//! bytes, and to a model.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{Entry, Idx, MAGIC, Mismatch, Refusal, WriteRefusal, write, write_above, written_len};
use crate::entry::PACK_HEADER_LEN;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::out::{Measure, Out};
use crate::pack::tests::{OFS_PACK, REF_PACK, SHA256_PACK, checksum, resign};
use crate::pack::{Writer, check};
use crate::{Admission, PastAdmission};

/// `ofs.pack`'s index, as `git pack-objects` wrote it.
const OFS_IDX: &[u8] = include_bytes!("../../fixtures/ofs.idx");
/// `ref.pack`'s index.
const REF_IDX: &[u8] = include_bytes!("../../fixtures/ref.idx");
/// `ofs.pack`'s index as `git index-pack --index-version=2,0x200` wrote it: the offsets past 0x200 in the 8-byte table.
const OFS_LARGE_IDX: &[u8] = include_bytes!("../../fixtures/ofs-large.idx");
/// `sha256.pack`'s index.
const SHA256_IDX: &[u8] = include_bytes!("../../fixtures/sha256.idx");

/// `git verify-pack -v ofs.idx`.
const OFS_LISTING: &str = include_str!("../../fixtures/ofs.verify.txt");
/// `git verify-pack -v ref.idx`.
const REF_LISTING: &str = include_str!("../../fixtures/ref.verify.txt");
/// `git verify-pack -v ofs-large.idx`, beside a copy of `ofs.pack`.
const OFS_LARGE_LISTING: &str = include_str!("../../fixtures/ofs-large.verify.txt");
/// `git verify-pack --object-format=sha256 -v sha256.idx`.
const SHA256_LISTING: &str = include_str!("../../fixtures/sha256.verify.txt");

/// The offset past which git put offsets in the 8-byte table of `ofs-large.idx` (`make-index-fixtures.sh`).
const FORCED_LIMIT: u64 = 0x200;
/// The offset past which git puts them by default, 2^31 - 1 (`pack-write.c`'s `off32_limit`).
const GIT_LIMIT: u64 = 0x7fff_ffff;
/// The objects in each pack git wrote.
const OBJECTS: usize = 12;

/// An index git wrote, its pack and git's listing of both.
#[derive(Clone, Copy)]
struct Golden {
  /// Its file's name.
  name: &'static str,
  /// The index.
  idx: &'static [u8],
  /// Its pack.
  pack: &'static [u8],
  /// `git verify-pack -v`'s listing.
  listing: &'static str,
  /// The repository's hash.
  format: ObjectFormat,
  /// The offset past which git put offsets in the 8-byte table.
  limit: u64,
}

/// Every index git wrote.
const GOLDEN: [Golden; 4] = [
  Golden {
    name: "ofs.idx",
    idx: OFS_IDX,
    pack: OFS_PACK,
    listing: OFS_LISTING,
    format: ObjectFormat::Sha1,
    limit: GIT_LIMIT,
  },
  Golden {
    name: "ref.idx",
    idx: REF_IDX,
    pack: REF_PACK,
    listing: REF_LISTING,
    format: ObjectFormat::Sha1,
    limit: GIT_LIMIT,
  },
  Golden {
    name: "ofs-large.idx",
    idx: OFS_LARGE_IDX,
    pack: OFS_PACK,
    listing: OFS_LARGE_LISTING,
    format: ObjectFormat::Sha1,
    limit: FORCED_LIMIT,
  },
  Golden {
    name: "sha256.idx",
    idx: SHA256_IDX,
    pack: SHA256_PACK,
    listing: SHA256_LISTING,
    format: ObjectFormat::Sha256,
    limit: GIT_LIMIT,
  },
];

/// One object as `git verify-pack -v` listed it.
struct Listed {
  /// Its id.
  id: ObjectId,
  /// Its bytes in the pack, the listing's "size-in-packfile".
  in_pack: u64,
  /// Where it begins in the pack.
  offset: u64,
}

/// The objects of a `git verify-pack -v` listing: each line that opens with an id is `id type size size-in-packfile
/// offset`, then a delta's depth and base.
fn listing(text: &str, format: ObjectFormat) -> Vec<Listed> {
  text
    .lines()
    .filter_map(|line| {
      let fields: Vec<&str> = line.split_whitespace().collect();
      let id = ObjectId::from_hex(format, fields.first()?.as_bytes()).ok()?;
      Some(Listed {
        id,
        in_pack: fields[3].parse().unwrap(),
        offset: fields[4].parse().unwrap(),
      })
    })
    .collect()
}

/// Where each part of an index of `count` objects in `format`'s hash begins, from the format's own list.
struct Layout {
  /// The hash.
  format: ObjectFormat,
  /// The objects.
  count: usize,
}

impl Layout {
  /// The layout of git's indexes of twelve objects.
  fn of(format: ObjectFormat) -> Layout {
    Layout {
      format,
      count: OBJECTS,
    }
  }
  /// The fanout count of `byte`.
  fn fanout(&self, byte: usize) -> usize {
    8 + 4 * byte
  }
  /// The id at `position`.
  fn id(&self, position: usize) -> usize {
    self.fanout(256) + self.format.raw_len() * position
  }
  /// The CRC32 at `position`.
  fn crc(&self, position: usize) -> usize {
    self.id(self.count) + 4 * position
  }
  /// The 4-byte offset at `position`.
  fn offset(&self, position: usize) -> usize {
    self.crc(self.count) + 4 * position
  }
  /// The 8-byte table's entry `index`.
  fn large(&self, index: usize) -> usize {
    self.offset(self.count) + 8 * index
  }
}

/// Writes `value` at `at`, four bytes in network order.
fn put_word(bytes: &mut [u8], at: usize, value: u32) {
  bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// `base` changed by `change`, then its own checksum made to hold again.
fn changed(base: &[u8], format: ObjectFormat, change: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
  let mut bytes = base.to_vec();
  change(&mut bytes);
  resign(bytes, format)
}

/// The room [`Idx::check_pack`] takes for `count` objects: one offset and one position each.
fn room(count: usize) -> u64 {
  (count * core::mem::size_of::<(u64, usize)>()) as u64
}

/// A-591: do decode each index git wrote and hold it to its pack. Expect git's twelve ids in order, each found at the
/// offset git listed, by position and by id, and spanning the bytes git listed (the 8-byte table read for
/// `ofs-large.idx`), the pack's trailer copied, the pack held, and the exact room charged.
#[test]
fn each_index_git_wrote_reads_as_git_listed_it() {
  for golden in GOLDEN {
    let name = golden.name;
    let idx = Idx::decode(golden.idx, golden.format)
      .unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
    let pack = check(golden.pack, golden.format).unwrap();
    let listed = listing(golden.listing, golden.format);
    assert_eq!((listed.len(), idx.len()), (OBJECTS, OBJECTS), "{name}");
    let mut ids: Vec<ObjectId> = listed.iter().map(|object| object.id).collect();
    ids.sort();
    assert_eq!(
      idx.entries().map(|entry| entry.id).collect::<Vec<_>>(),
      ids,
      "{name}"
    );
    let mut offsets: Vec<u64> = idx.entries().map(|entry| entry.offset).collect();
    offsets.sort();
    for object in &listed {
      let position = idx
        .find(&object.id)
        .unwrap_or_else(|| panic!("{name}: {} not found", object.id));
      let entry = idx.entry(position).unwrap();
      assert_eq!(
        (entry.id, entry.offset),
        (object.id, object.offset),
        "{name}"
      );
      assert_eq!(idx.offset_of(&object.id), Some(object.offset), "{name}");
      let next = offsets
        .iter()
        .copied()
        .find(|offset| *offset > entry.offset)
        .unwrap_or(pack.trailer_at() as u64);
      assert_eq!(next - entry.offset, object.in_pack, "{name} {}", object.id);
    }
    assert_eq!(idx.pack_checksum(), pack.checksum(), "{name}");
    let mut admission = Admission::new(room(OBJECTS));
    assert_eq!(idx.check_pack(&pack, &mut admission), Ok(()), "{name}");
    assert_eq!(admission.left(), 0, "{name}");
  }
}

/// A-591: do decode each index git wrote, then write its entries again, handed over in reverse, at the offset limit
/// git wrote it with. Expect git's bytes exactly, and the length `written_len` foretold at git's default limit.
#[test]
fn each_index_is_written_back_byte_for_byte() {
  for golden in GOLDEN {
    let idx = Idx::decode(golden.idx, golden.format).unwrap();
    let mut entries: Vec<Entry> = idx.entries().collect();
    entries.reverse();
    let mut out = Vec::new();
    write_above(&mut entries, idx.pack_checksum(), golden.limit, &mut out).unwrap();
    assert_eq!(out, golden.idx, "{}", golden.name);
    if golden.limit == GIT_LIMIT {
      entries.reverse();
      let mut again = Vec::new();
      write(&mut entries, idx.pack_checksum(), &mut again).unwrap();
      assert_eq!(again, golden.idx, "{}", golden.name);
      assert_eq!(
        written_len(golden.format, &entries),
        golden.idx.len() as u64
      );
      let mut measure = Measure::default();
      write(&mut entries, idx.pack_checksum(), &mut measure).unwrap();
      assert_eq!(measure, Measure(golden.idx.len() as u64));
    }
  }
}

/// A-591: do flip each bit of a SHA-1 index and of a SHA-256 index, one at a time, and decode each. Expect every one
/// refused by the index's own checksum, a flip in its magic, fanout or ids too: it is verified before anything it
/// guards is read.
#[test]
fn a_flipped_bit_anywhere_in_an_index_is_refused_before_it_is_decoded() {
  for (name, bytes, format) in [
    ("ofs.idx", OFS_IDX, ObjectFormat::Sha1),
    ("sha256.idx", SHA256_IDX, ObjectFormat::Sha256),
  ] {
    for bit in 0..bytes.len() * 8 {
      let mut flipped = bytes.to_vec();
      flipped[bit / 8] ^= 1 << (bit % 8);
      assert_eq!(
        Idx::decode(&flipped, format),
        Err(Refusal::Checksum),
        "{name} bit {bit}"
      );
    }
  }
}

/// A-591: do decode every prefix of an index shorter than an index of no objects. Expect each refused as truncated,
/// naming the bytes that index takes (8 + 1024 + two hashes) and the bytes there were.
#[test]
fn an_index_shorter_than_one_of_no_objects_is_refused() {
  for (bytes, format) in [
    (OFS_IDX, ObjectFormat::Sha1),
    (SHA256_IDX, ObjectFormat::Sha256),
  ] {
    let needed = 8 + 1024 + 2 * format.raw_len();
    for len in 0..needed {
      assert_eq!(
        Idx::decode(&bytes[..len], format),
        Err(Refusal::Truncated {
          needed: needed as u64,
          left: len as u64
        }),
        "{len} bytes"
      );
    }
  }
}

/// A-591: do decode whole indexes whose magic is another, whose version is 1 or 3, or that are version 1's layout of
/// one object, which has no magic. Expect each refused by kind before its layout is read.
#[test]
fn an_index_of_another_magic_or_version_is_refused() {
  let format = ObjectFormat::Sha1;
  let magic = changed(OFS_IDX, format, |bytes| bytes[0] = 0);
  assert_eq!(Idx::decode(&magic, format), Err(Refusal::Magic));
  for version in [1, 3] {
    let other = changed(OFS_IDX, format, |bytes| put_word(bytes, 4, version));
    assert_eq!(
      Idx::decode(&other, format),
      Err(Refusal::Version { version })
    );
  }
  let mut version_one = Vec::new();
  for _ in 0..256 {
    version_one.extend_from_slice(&1u32.to_be_bytes());
  }
  version_one.extend_from_slice(&12u32.to_be_bytes());
  version_one.extend_from_slice(&[0u8; 20]);
  version_one.extend_from_slice(&[0u8; 40]);
  let version_one = resign(version_one, format);
  assert_eq!(Idx::decode(&version_one, format), Err(Refusal::Magic));
  assert_eq!(&OFS_IDX[..4], &MAGIC[..]);
}

/// A-591, hostile input: do decode an index whose fanout goes down, at byte 2 and at its last byte. Expect each
/// refused naming the first byte whose count went down, as git's "non-monotonic index".
#[test]
fn a_fanout_that_goes_down_is_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let early = changed(OFS_IDX, format, |bytes| {
    put_word(bytes, layout.fanout(2), 0)
  });
  assert_eq!(
    Idx::decode(&early, format),
    Err(Refusal::FanoutDown { byte: 2 })
  );
  let last = changed(OFS_IDX, format, |bytes| {
    put_word(bytes, layout.fanout(255), 11)
  });
  assert_eq!(
    Idx::decode(&last, format),
    Err(Refusal::FanoutDown { byte: 255 })
  );
}

/// A-591, hostile input: do decode indexes whose fanout ends at one object more than the file holds, and at the
/// largest count. Expect each refused as a count past the file, naming the count and the file's length.
#[test]
fn a_count_past_the_file_is_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  for count in [13, u32::MAX] {
    let past = changed(OFS_IDX, format, |bytes| {
      put_word(bytes, layout.fanout(255), count)
    });
    assert_eq!(
      Idx::decode(&past, format),
      Err(Refusal::CountPastFile {
        count: u64::from(count),
        len: OFS_IDX.len() as u64
      })
    );
  }
}

/// A-591, hostile input: do decode indexes whose 8-byte table is not whole entries (a count one short, leaving 28
/// bytes; four bytes added) or holds more entries than objects less one (twelve added), and one holding exactly that
/// many. Expect the first three refused naming the table's bytes and the count, and the last read, as git reads it.
#[test]
fn an_8_byte_table_that_is_not_whole_entries_or_too_long_is_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let short = changed(OFS_IDX, format, |bytes| {
    for byte in 0xe9..=0xff {
      put_word(bytes, layout.fanout(byte), 11);
    }
  });
  assert_eq!(
    Idx::decode(&short, format),
    Err(Refusal::LargeTable { len: 28, count: 11 })
  );
  let with_table = |table: usize| {
    changed(OFS_IDX, format, |bytes| {
      let at = bytes.len() - 2 * format.raw_len();
      let trailing = bytes.split_off(at);
      bytes.extend(core::iter::repeat_n(0u8, table));
      bytes.extend_from_slice(&trailing);
    })
  };
  for table in [4, 12 * 8] {
    assert_eq!(
      Idx::decode(&with_table(table), format),
      Err(Refusal::LargeTable {
        len: table as u64,
        count: 12
      })
    );
  }
  assert!(Idx::decode(&with_table(11 * 8), format).is_ok());
}

/// A-591, hostile input: do decode indexes with two ids swapped and with an id written twice. Expect the first refused
/// as unsorted and the second as a duplicate, each naming the second id's position.
#[test]
fn unsorted_or_duplicated_ids_are_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let (fourth, fifth) = (layout.id(4), layout.id(5));
  let swapped = changed(OFS_IDX, format, |bytes| {
    let id = bytes[fourth..fifth].to_vec();
    bytes.copy_within(fifth..fifth + 20, fourth);
    bytes[fifth..fifth + 20].copy_from_slice(&id);
  });
  assert_eq!(
    Idx::decode(&swapped, format),
    Err(Refusal::Unsorted { position: 5 })
  );
  let twice = changed(OFS_IDX, format, |bytes| {
    bytes.copy_within(fourth..fifth, fifth)
  });
  assert_eq!(
    Idx::decode(&twice, format),
    Err(Refusal::Duplicate { position: 5 })
  );
}

/// A-591, hostile input: do decode an index whose fanout never goes down and ends at its count but says one id begins
/// with byte 0, which none does. Expect it refused naming the byte, what the fanout says and the ids counted.
#[test]
fn a_fanout_that_miscounts_the_ids_is_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let miscount = changed(OFS_IDX, format, |bytes| {
    put_word(bytes, layout.fanout(0), 1)
  });
  assert_eq!(
    Idx::decode(&miscount, format),
    Err(Refusal::FanoutMismatch {
      byte: 0,
      says: 1,
      counted: 0
    })
  );
}

/// A-591, hostile input: do decode indexes whose large offsets name the entry just past an 8-byte table of eight, and
/// the first entry of a table of none. Expect each refused naming the object, the entry and the table's entries.
#[test]
fn a_large_offset_naming_an_entry_past_its_table_is_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let past = changed(OFS_LARGE_IDX, format, |bytes| {
    put_word(bytes, layout.offset(1), 0x8000_0008);
  });
  assert_eq!(
    Idx::decode(&past, format),
    Err(Refusal::LargeIndex {
      position: 1,
      index: 8,
      entries: 8
    })
  );
  let none = changed(OFS_IDX, format, |bytes| {
    put_word(bytes, layout.offset(0), 0x8000_0000);
  });
  assert_eq!(
    Idx::decode(&none, format),
    Err(Refusal::LargeIndex {
      position: 0,
      index: 0,
      entries: 0
    })
  );
}

/// A-591, hostile input: do decode indexes placing an object inside a pack's header, by a 4-byte offset of 4 and by an
/// 8-byte entry of 11. Expect each refused naming the object and the offset.
#[test]
fn an_offset_inside_the_header_is_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let small = changed(OFS_IDX, format, |bytes| {
    put_word(bytes, layout.offset(3), 4)
  });
  assert_eq!(
    Idx::decode(&small, format),
    Err(Refusal::OffsetInHeader {
      position: 3,
      offset: 4
    })
  );
  let large = changed(OFS_LARGE_IDX, format, |bytes| {
    let at = layout.large(0);
    bytes[at..at + 8].copy_from_slice(&11u64.to_be_bytes());
  });
  assert_eq!(
    Idx::decode(&large, format),
    Err(Refusal::OffsetInHeader {
      position: 1,
      offset: 11
    })
  );
}

/// A-591, hostile input: do hold to its pack indexes placing an object at the pack's trailer, at 2^31 - 1, and through
/// the 8-byte table at 2^40. Expect each read, then refused against the pack naming the object and the offset.
#[test]
fn offsets_past_the_pack_are_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let pack = check(OFS_PACK, format).unwrap();
  let trailer = pack.trailer_at() as u64;
  for (base, offset) in [(OFS_IDX, trailer), (OFS_IDX, GIT_LIMIT)] {
    let past = changed(base, format, |bytes| {
      put_word(bytes, layout.offset(3), offset as u32);
    });
    let idx = Idx::decode(&past, format).unwrap();
    assert_eq!(
      idx.check_pack(&pack, &mut Admission::new(room(OBJECTS))),
      Err(Mismatch::OffsetOutside {
        position: 3,
        offset
      })
    );
  }
  let far = changed(OFS_LARGE_IDX, format, |bytes| {
    let at = layout.large(2);
    bytes[at..at + 8].copy_from_slice(&(1u64 << 40).to_be_bytes());
  });
  let idx = Idx::decode(&far, format).unwrap();
  assert_eq!(idx.offset(3), Some(1 << 40));
  assert_eq!(
    idx.check_pack(&pack, &mut Admission::new(room(OBJECTS))),
    Err(Mismatch::OffsetOutside {
      position: 3,
      offset: 1 << 40
    })
  );
}

/// A-591, hostile input: do flip a bit in each checksum: the pack's trailer, the index's own checksum, and the index's
/// copy of the pack's trailer, that last also with the index's checksum made to hold. Expect the pack refused by its
/// trailer, the index refused by its own checksum both times, and the re-signed index refused against its pack.
#[test]
fn a_flipped_bit_in_each_checksum_is_refused() {
  let format = ObjectFormat::Sha1;
  let mut pack_trailer = OFS_PACK.to_vec();
  let last = pack_trailer.len() - 1;
  pack_trailer[last] ^= 1;
  assert_eq!(
    check(&pack_trailer, format),
    Err(crate::pack::Refusal::Checksum)
  );
  let mut own = OFS_IDX.to_vec();
  let last = own.len() - 1;
  own[last] ^= 0x80;
  assert_eq!(Idx::decode(&own, format), Err(Refusal::Checksum));
  let copy_at = OFS_IDX.len() - 2 * format.raw_len();
  let mut copy = OFS_IDX.to_vec();
  copy[copy_at] ^= 1;
  assert_eq!(Idx::decode(&copy, format), Err(Refusal::Checksum));
  let copy = resign(copy, format);
  let idx = Idx::decode(&copy, format).unwrap();
  let pack = check(OFS_PACK, format).unwrap();
  assert_eq!(
    idx.check_pack(&pack, &mut Admission::new(room(OBJECTS))),
    Err(Mismatch::PackChecksum)
  );
}

/// A-591: do hold indexes to packs not theirs: another pack of the same objects, a pack in the other hash, and the
/// index's own pack with its count changed and its trailer made to hold. Expect each refused, naming the trailer, the
/// hash and the two counts, in git's order: the count before the trailer.
#[test]
fn an_index_is_refused_against_a_pack_not_its_own() {
  let sha1 = ObjectFormat::Sha1;
  let idx = Idx::decode(OFS_IDX, sha1).unwrap();
  let mut admission = Admission::new(room(OBJECTS));
  let other = check(REF_PACK, sha1).unwrap();
  assert_eq!(
    idx.check_pack(&other, &mut admission),
    Err(Mismatch::PackChecksum)
  );
  let sha256 = check(SHA256_PACK, ObjectFormat::Sha256).unwrap();
  assert_eq!(
    idx.check_pack(&sha256, &mut admission),
    Err(Mismatch::Format)
  );
  let mut counted = OFS_PACK.to_vec();
  counted[8..12].copy_from_slice(&13u32.to_be_bytes());
  let counted = resign(counted, sha1);
  let counted = check(&counted, sha1).unwrap();
  assert_eq!(
    idx.check_pack(&counted, &mut admission),
    Err(Mismatch::Count {
      pack: 13,
      index: 12
    })
  );
  assert_eq!(admission.left(), room(OBJECTS));
}

/// A-591: do hold to its pack an index with one bit of one CRC32 flipped. Expect it refused naming that object, as
/// git's "index CRC mismatch".
#[test]
fn a_crc_that_is_not_its_objects_is_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let pack = check(OFS_PACK, format).unwrap();
  for position in [0, 6, 11] {
    let crc = changed(OFS_IDX, format, |bytes| bytes[layout.crc(position)] ^= 0x10);
    let idx = Idx::decode(&crc, format).unwrap();
    assert_eq!(
      idx.check_pack(&pack, &mut Admission::new(room(OBJECTS))),
      Err(Mismatch::Crc {
        position: position as u64
      })
    );
  }
}

/// A-591: do hold to its pack indexes placing two objects at one offset, and moving the first object one byte past the
/// header's end. Expect the first refused naming the second object at that offset, and the second refused naming the
/// header's end as bytes no object covers.
#[test]
fn shared_offsets_and_bytes_before_the_first_object_are_refused() {
  let format = ObjectFormat::Sha1;
  let layout = Layout::of(format);
  let pack = check(OFS_PACK, format).unwrap();
  let idx = Idx::decode(OFS_IDX, format).unwrap();
  let first = (0..OBJECTS)
    .find(|position| idx.offset(*position) == Some(PACK_HEADER_LEN))
    .unwrap();
  let shared_offset = idx.offset(4).unwrap();
  let shared = changed(OFS_IDX, format, |bytes| {
    put_word(bytes, layout.offset(0), shared_offset as u32);
  });
  let shared = Idx::decode(&shared, format).unwrap();
  assert_eq!(
    shared.check_pack(&pack, &mut Admission::new(room(OBJECTS))),
    Err(Mismatch::SharedOffset {
      position: 4,
      offset: shared_offset
    })
  );
  let moved = changed(OFS_IDX, format, |bytes| {
    put_word(bytes, layout.offset(first), 13);
  });
  let moved = Idx::decode(&moved, format).unwrap();
  assert_eq!(
    moved.check_pack(&pack, &mut Admission::new(room(OBJECTS))),
    Err(Mismatch::Unindexed { at: 12 })
  );
}

/// A-591: do hold an index to its pack with one byte less room than its table of offsets takes. Expect it refused
/// before the table is built, naming what it asked and what was left, with the room untouched.
#[test]
fn a_check_past_its_admission_is_refused_and_takes_nothing() {
  let format = ObjectFormat::Sha1;
  let idx = Idx::decode(OFS_IDX, format).unwrap();
  let pack = check(OFS_PACK, format).unwrap();
  let mut admission = Admission::new(room(OBJECTS) - 1);
  assert_eq!(
    idx.check_pack(&pack, &mut admission),
    Err(Mismatch::PastAdmission(PastAdmission {
      asked: room(OBJECTS),
      left: room(OBJECTS) - 1
    }))
  );
  assert_eq!(admission.left(), room(OBJECTS) - 1);
}

/// A SHA-1 id of twenty bytes `byte`.
fn sha1_id(byte: u8) -> ObjectId {
  ObjectId::from_bytes(ObjectFormat::Sha1, &[byte; 20]).unwrap()
}

/// A-591: do write entries naming one id twice, entries with an id in the other hash, entries with more offsets past
/// 2^31 - 1 than objects less one, and entries placing an object inside a pack's header. Expect each refused by kind
/// with nothing written: no index the reader refuses is written.
#[test]
fn write_refuses_what_the_reader_refuses() {
  let pack_checksum = checksum(ObjectFormat::Sha1, b"a pack");
  let entry = |byte: u8, offset: u64| Entry {
    id: sha1_id(byte),
    crc32: u32::from(byte),
    offset,
  };
  let mut out = Vec::new();
  let mut twice = [entry(3, 12), entry(1, 40), entry(3, 90)];
  assert_eq!(
    write(&mut twice, pack_checksum, &mut out),
    Err(WriteRefusal::Duplicate { position: 2 })
  );
  let other = Entry {
    id: checksum(ObjectFormat::Sha256, b"an object"),
    crc32: 0,
    offset: 40,
  };
  let mut mixed = [entry(1, 12), other];
  assert_eq!(
    write(&mut mixed, pack_checksum, &mut out),
    Err(WriteRefusal::Format { position: 1 })
  );
  let mut alone = [entry(1, 1 << 31)];
  assert_eq!(
    write(&mut alone, pack_checksum, &mut out),
    Err(WriteRefusal::LargeOffsets { count: 1 })
  );
  let mut all = [entry(1, 1 << 31), entry(2, 1 << 32)];
  assert_eq!(
    write(&mut all, pack_checksum, &mut out),
    Err(WriteRefusal::LargeOffsets { count: 2 })
  );
  let mut inside = [entry(1, 12), entry(2, 11)];
  assert_eq!(
    write(&mut inside, pack_checksum, &mut out),
    Err(WriteRefusal::OffsetInHeader { position: 1 })
  );
  assert!(out.is_empty());
}

/// A-591: do write four entries whose offsets are 12, 2^31 - 1, 2^31 and 2^40, handed over out of id order. Expect the
/// last two in the 8-byte table numbered in id order (the 2^40 one first, its id being smaller), 2^31 - 1 kept in four
/// bytes, and every offset read back.
#[test]
fn offsets_past_31_bits_go_to_the_8_byte_table_in_id_order() {
  let pack_checksum = checksum(ObjectFormat::Sha1, b"a pack");
  let mut entries = [
    Entry {
      id: sha1_id(0x40),
      crc32: 4,
      offset: 12,
    },
    Entry {
      id: sha1_id(0x30),
      crc32: 3,
      offset: GIT_LIMIT,
    },
    Entry {
      id: sha1_id(0x20),
      crc32: 2,
      offset: 1 << 31,
    },
    Entry {
      id: sha1_id(0x10),
      crc32: 1,
      offset: 1 << 40,
    },
  ];
  let mut out = Vec::new();
  write(&mut entries, pack_checksum, &mut out).unwrap();
  let layout = Layout {
    format: ObjectFormat::Sha1,
    count: 4,
  };
  let words: Vec<u32> = (0..4)
    .map(|position| {
      let at = layout.offset(position);
      u32::from_be_bytes(out[at..at + 4].try_into().unwrap())
    })
    .collect();
  assert_eq!(words, [0x8000_0000, 0x8000_0001, 0x7fff_ffff, 12]);
  let table: Vec<u64> = (0..2)
    .map(|index| {
      let at = layout.large(index);
      u64::from_be_bytes(out[at..at + 8].try_into().unwrap())
    })
    .collect();
  assert_eq!(table, [1 << 40, 1 << 31]);
  let idx = Idx::decode(&out, ObjectFormat::Sha1).unwrap();
  let offsets: Vec<u64> = idx.entries().map(|entry| entry.offset).collect();
  assert_eq!(offsets, [1 << 40, 1 << 31, GIT_LIMIT, 12]);
  assert_eq!(written_len(ObjectFormat::Sha1, &entries), out.len() as u64);
}

/// One generated object: its id's bytes (cut to its hash's length), its CRC32 and its offset.
type Generated = (Vec<u8>, u32, u64);

/// Entries for an index of `format`'s hash from generated objects, one per id: the model, a map from id to CRC32 and
/// offset, in id order. The first object generated begins at the header's end, so at most all but one are large.
fn model(format: ObjectFormat, objects: &[Generated]) -> BTreeMap<Vec<u8>, (u32, u64)> {
  let mut model = BTreeMap::new();
  for (index, (id, crc32, offset)) in objects.iter().enumerate() {
    let offset = if index == 0 { PACK_HEADER_LEN } else { *offset };
    model
      .entry(id[..format.raw_len()].to_vec())
      .or_insert((*crc32, offset));
  }
  model
}

/// A pack of `blobs` as its objects, opaque bytes back to back, with its index's entries: each blob's id, its CRC32
/// and its offset.
fn synthetic_pack(
  format: ObjectFormat,
  blobs: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> (Vec<u8>, Vec<Entry>) {
  let mut pack = Vec::new();
  let mut writer = Writer::new(format, blobs.len() as u32, &mut pack);
  let mut entries = Vec::new();
  let mut offset = PACK_HEADER_LEN;
  for (id, blob) in blobs {
    writer.put(blob);
    entries.push(Entry {
      id: ObjectId::from_bytes(format, id).unwrap(),
      crc32: crc32fast::hash(blob),
      offset,
    });
    offset += blob.len() as u64;
  }
  writer.finish().unwrap();
  (pack, entries)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-591, model: do write the index of up to 64 generated objects in either hash, handed over in any order, and
  /// decode it. Expect the model's entries in id order, every id found at its place and its offset, an id the model
  /// lacks not found, the same bytes whatever the order handed over, and the length `written_len` foretold.
  #[test]
  fn any_entries_written_decode_to_the_model(
    sha256 in any::<bool>(),
    objects in proptest::collection::vec(
      (proptest::collection::vec(any::<u8>(), 32), any::<u32>(), PACK_HEADER_LEN..=u64::MAX),
      0..64,
    ),
    absent in proptest::collection::vec(any::<u8>(), 32),
  ) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let model = model(format, &objects);
    let pack_checksum = checksum(format, b"a pack");
    let mut entries: Vec<Entry> = model
      .iter()
      .map(|(id, (crc32, offset))| Entry {
        id: ObjectId::from_bytes(format, id).unwrap(),
        crc32: *crc32,
        offset: *offset,
      })
      .collect();
    let expected = entries.clone();
    entries.reverse();
    let mut out = Vec::new();
    write(&mut entries, pack_checksum, &mut out).unwrap();
    let mut sorted_in = expected.clone();
    let mut again = Vec::new();
    write(&mut sorted_in, pack_checksum, &mut again).unwrap();
    prop_assert_eq!(&out, &again);
    prop_assert_eq!(written_len(format, &expected), out.len() as u64);
    let idx = Idx::decode(&out, format).unwrap();
    prop_assert_eq!(idx.entries().collect::<Vec<_>>(), expected.clone());
    prop_assert_eq!(idx.pack_checksum(), pack_checksum);
    for (position, entry) in expected.iter().enumerate() {
      prop_assert_eq!(idx.find(&entry.id), Some(position));
      prop_assert_eq!(idx.offset_of(&entry.id), Some(entry.offset));
    }
    let absent = ObjectId::from_bytes(format, &absent[..format.raw_len()]).unwrap();
    if !model.contains_key(absent.as_bytes()) {
      prop_assert_eq!(idx.find(&absent), None);
      prop_assert_eq!(idx.offset_of(&absent), None);
    }
  }

  /// A-591, oracle: do make a pack of up to 16 opaque objects and its index from their spans, then hold the index to
  /// the pack, and hold an index with one object's CRC32 changed. Expect the first held and the second refused naming
  /// that object's place in id order.
  #[test]
  fn an_index_made_from_a_packs_spans_holds_and_one_wrong_crc_does_not(
    sha256 in any::<bool>(),
    blobs in proptest::collection::btree_map(
      proptest::collection::vec(any::<u8>(), 32),
      proptest::collection::vec(any::<u8>(), 1..64),
      1..16,
    ),
    pick in any::<proptest::sample::Index>(),
    bit in 0u32..32,
  ) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let blobs: BTreeMap<Vec<u8>, Vec<u8>> = blobs
      .into_iter()
      .map(|(id, blob)| (id[..format.raw_len()].to_vec(), blob))
      .collect();
    let (pack_bytes, mut entries) = synthetic_pack(format, &blobs);
    let pack = check(&pack_bytes, format).unwrap();
    let mut out = Vec::new();
    write(&mut entries, pack.checksum(), &mut out).unwrap();
    let idx = Idx::decode(&out, format).unwrap();
    prop_assert_eq!(idx.check_pack(&pack, &mut Admission::new(room(blobs.len()))), Ok(()));
    let wrong = pick.index(entries.len());
    entries[wrong].crc32 ^= 1 << bit;
    let mut bad = Vec::new();
    write(&mut entries, pack.checksum(), &mut bad).unwrap();
    let bad = Idx::decode(&bad, format).unwrap();
    let position = bad.find(&entries[wrong].id).unwrap();
    prop_assert_eq!(
      bad.check_pack(&pack, &mut Admission::new(room(blobs.len()))),
      Err(Mismatch::Crc { position: position as u64 })
    );
  }
}
