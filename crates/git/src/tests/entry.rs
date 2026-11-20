use proptest::prelude::*;

use super::{Header, PACK_HEADER_LEN, Refusal, Representation, read, write, written_len};
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::pack_fixtures::{Bases, Listed, Pack, begins_zlib_stream, packs};
use crate::{Admission, PastAdmission};

/// An admission that holds anything, for reads whose charge a test does not look at.
fn unbounded() -> Admission {
  Admission::new(u64::MAX)
}

/// `header` as `write` writes it for the entry at `at`.
fn written(header: Header, at: u64) -> Vec<u8> {
  let mut out = Vec::new();
  write(header, at, &mut out).unwrap();
  out
}

/// The header the listing says `entry` of `pack` has: its object whole, or a delta of the delta's size naming its base
/// as the pack names bases.
fn listed_header(pack: &Pack, entry: &Listed) -> Header {
  let representation = match (entry.base, pack.bases) {
    (None, _) => Representation::Whole(entry.kind),
    (Some(base), Bases::ById) => Representation::RefDelta { base },
    (Some(base), Bases::ByOffset) => Representation::OfsDelta {
      base: pack.entry(base).offset,
    },
  };
  Header {
    representation,
    size: entry.size,
  }
}

/// The header of `entry` and the bytes it takes, read from its pack.
fn read_listed(pack: &Pack, entry: &Listed) -> (Header, usize) {
  read(
    pack.starting_at(entry),
    entry.offset,
    ObjectFormat::Sha1,
    &mut unbounded(),
  )
  .unwrap()
  .unwrap()
}

/// A-582 part 3 (§4.17.2 piece 2; golden vectors: `fixtures/ofs.pack` and `ref.pack`, which stock git 2.50.1's
/// `pack-objects` wrote, and `git verify-pack -v`'s listing of each). Do: read the header of every listed entry of both
/// packs at its listed offset, charged to an admission, then write each back for the same place. Expect: each pack's
/// twelve-byte header `PACK`, version 2 and the listing's count; every entry's header as the listing says (an object
/// whole of its type and size, or a delta of its delta's size naming its base by offset in one pack and by id in the
/// other, two deltas in each); its size charged, nothing else; its compressed data beginning where the header ends (a
/// zlib stream's first two bytes, RFC 1950); and each written back to the very bytes git wrote, as long as
/// `written_len` says.
#[test]
fn every_entry_stock_git_packed_reads_as_its_listing_says_and_writes_back_to_its_bytes() {
  for pack in packs() {
    let name = pack.name;
    assert_eq!(pack.listing.len(), 12, "{name}: twelve objects listed");
    assert_eq!(&pack.bytes[..4], b"PACK", "{name}");
    assert_eq!(pack.bytes[4..8], 2u32.to_be_bytes(), "{name}: version 2");
    assert_eq!(pack.bytes[8..12], 12u32.to_be_bytes(), "{name}: the count");
    let deltas = pack.listing.iter().filter(|entry| entry.base.is_some());
    assert_eq!(deltas.count(), 2, "{name}: two deltas, a chain of two");
    for entry in &pack.listing {
      reads_as_listed_and_writes_back(&pack, entry);
    }
  }
}

/// Holds `entry`'s header in `pack` to its listing: read as listed, its size charged and nothing else, its compressed
/// data beginning where the header ends, and written back to the very bytes git wrote, as long as `written_len` says.
fn reads_as_listed_and_writes_back(pack: &Pack, entry: &Listed) {
  let name = pack.name;
  let bytes = pack.starting_at(entry);
  let mut admission = unbounded();
  let (header, len) = read(bytes, entry.offset, ObjectFormat::Sha1, &mut admission)
    .unwrap()
    .unwrap();
  assert_eq!(header, listed_header(pack, entry), "{name} {:?}", entry.id);
  assert_eq!(u64::MAX - admission.left(), entry.size, "{name}: charged");
  assert!(begins_zlib_stream(&bytes[len..]), "{name} {:?}", entry.id);
  assert_eq!(
    written(header, entry.offset),
    bytes[..len],
    "{name} {:?}",
    entry.id
  );
  assert_eq!(
    written_len(header, entry.offset),
    Ok(u64::try_from(len).unwrap())
  );
}

/// A-582 part 3 (hostile input: every cut). Do: give the reader every listed header of both packs cut short at each of
/// its bytes, as a pack stream delivers it, under an admission. Expect: not yet whole every time, nothing charged, and
/// the whole header at its last byte.
#[test]
fn a_header_is_not_whole_until_its_last_byte_arrives_and_a_cut_charges_nothing() {
  for pack in packs() {
    for entry in &pack.listing {
      let bytes = pack.starting_at(entry);
      let (header, len) = read_listed(&pack, entry);
      let mut admission = Admission::new(entry.size);
      for cut in 0..len {
        let read_cut = read(
          &bytes[..cut],
          entry.offset,
          ObjectFormat::Sha1,
          &mut admission,
        );
        assert_eq!(
          read_cut,
          Ok(None),
          "{} {:?} cut at {cut}",
          pack.name,
          entry.id
        );
        assert_eq!(admission.left(), entry.size, "nothing charged");
      }
      let whole = read(
        &bytes[..len],
        entry.offset,
        ObjectFormat::Sha1,
        &mut admission,
      );
      assert_eq!(whole, Ok(Some((header, len))));
      assert_eq!(admission.left(), 0);
    }
  }
}

/// What reading one flipped header came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flipped {
  /// Read whole, within the bytes given.
  Read,
  /// Refused as type 0 or 5.
  Type,
  /// Refused for a base that is not between the pack's header and the entry.
  Base,
  /// Refused for a size or an offset past what git reads.
  TooLong,
}

/// Reads the pack's bytes from `at` with one bit flipped, and judges the read: each refusal must name the entry's place
/// and be one this module has for such bytes.
fn judge_flip(bytes: &[u8], at: u64, bit: usize) -> Flipped {
  let mut flipped = bytes.to_vec();
  flipped[bit / 8] ^= 1 << (bit % 8);
  let code = (flipped[0] >> 4) & 7;
  match read(&flipped, at, ObjectFormat::Sha1, &mut unbounded()) {
    Ok(Some((_, len))) => {
      assert!(len <= flipped.len(), "bit {bit}: read past the bytes");
      Flipped::Read
    }
    Err(Refusal::Type {
      at: place,
      code: refused,
    }) => {
      assert_eq!((place, refused), (at, code), "bit {bit}");
      assert!(code == 0 || code == 5, "bit {bit}: type {code} refused");
      Flipped::Type
    }
    Err(Refusal::BaseNotBefore { at: place } | Refusal::BaseBeforePack { at: place, .. }) => {
      assert_eq!(place, at, "bit {bit}");
      Flipped::Base
    }
    Err(Refusal::SizeTooLong { at: place } | Refusal::OffsetTooLong { at: place }) => {
      assert_eq!(place, at, "bit {bit}");
      Flipped::TooLong
    }
    other => panic!("bit {bit}: {other:?}"),
  }
}

/// A-582 part 3 (hostile input: bit flips). Do: flip each bit of every listed header of both packs, one at a time, and
/// read the entry from the rest of its pack's bytes. Expect: each read whole within the bytes or refused by kind at the
/// entry's place; a type turned to 0 or 5 refused as `Type` with that number; and, among them, flips that turn a type
/// invalid and flips that move an offset delta's base before the pack's first entry, so neither refusal is vacuous.
#[test]
fn every_bit_flip_of_a_header_is_refused_by_kind_or_read_within_its_bytes() {
  let mut seen = Vec::new();
  for pack in packs() {
    for entry in &pack.listing {
      let (_, len) = read_listed(&pack, entry);
      for bit in 0..len * 8 {
        seen.push(judge_flip(pack.starting_at(entry), entry.offset, bit));
      }
    }
  }
  for kind in [Flipped::Read, Flipped::Type, Flipped::Base] {
    assert!(seen.contains(&kind), "no flip came to {kind:?}");
  }
}

/// A-582 part 3 (hostile input: offsets past the pack's start; `packfile.c` 1265-1266). Do: read the offset delta stock
/// git wrote for `671f3c00`, 576 bytes after its base, as if its entry began at each place from 0 to 600; and read an
/// offset delta whose offset is zero. Expect: refused as `BaseBeforePack` while its base would begin before byte 12,
/// the pack's first entry, and read with its base 576 bytes back from there on; the zero offset refused as
/// `BaseNotBefore`, since it names the entry itself.
#[test]
fn an_offset_delta_whose_base_is_not_between_the_packs_header_and_itself_is_refused() {
  let [pack, _] = packs();
  let id = ObjectId::from_hex(
    ObjectFormat::Sha1,
    b"671f3c002c093979f501d6de93e127eb46d77a63",
  )
  .unwrap();
  let entry = pack.entry(id);
  let offset = entry.offset - pack.entry(entry.base.unwrap()).offset;
  assert_eq!(offset, 576);
  for at in 0..=600 {
    let read_at = read(
      pack.starting_at(entry),
      at,
      ObjectFormat::Sha1,
      &mut unbounded(),
    );
    if at < PACK_HEADER_LEN + offset {
      assert_eq!(
        read_at,
        Err(Refusal::BaseBeforePack { at, offset }),
        "at {at}"
      );
    } else {
      let (header, _) = read_at.unwrap().unwrap();
      let base = at - offset;
      assert_eq!(header.representation, Representation::OfsDelta { base });
    }
  }
  let zero_offset = [0x60, 0x00];
  let read_zero = read(&zero_offset, 100, ObjectFormat::Sha1, &mut unbounded());
  assert_eq!(read_zero, Err(Refusal::BaseNotBefore { at: 100 }));
}

/// A-582 part 3 (`packfile.c` 1124-1126 and 1255-1262: a size of at most 60 bits, an offset of at most 64). Do: read a
/// blob of the largest size git reads, 2^60 - 1, in nine bytes, cut a byte short and with its ninth byte saying another
/// follows; read nine offset bytes each saying another follows, and eight; and write and read back the largest offset,
/// an entry at `u64::MAX` whose base is the pack's first. Expect: the largest size whole, its cut not yet whole, and the
/// longer size refused as `SizeTooLong` with no tenth byte given; the nine offset bytes refused as `OffsetTooLong` and
/// the eight not yet whole; the largest offset written in ten bytes and read back.
#[test]
fn sizes_and_offsets_at_and_past_what_git_reads() {
  let mut largest = vec![0xbf];
  largest.extend([0xff; 7]);
  largest.push(0x7f);
  let read_largest = read(&largest, 12, ObjectFormat::Sha1, &mut unbounded());
  let blob = Representation::Whole(Kind::Blob);
  let expected = Header {
    representation: blob,
    size: (1 << 60) - 1,
  };
  assert_eq!(read_largest, Ok(Some((expected, 9))));
  assert_eq!(written(expected, 12), largest);
  let cut = read(&largest[..8], 12, ObjectFormat::Sha1, &mut unbounded());
  assert_eq!(cut, Ok(None));
  let mut longer = largest.clone();
  longer[8] = 0xff;
  let read_longer = read(&longer, 12, ObjectFormat::Sha1, &mut unbounded());
  assert_eq!(read_longer, Err(Refusal::SizeTooLong { at: 12 }));
  let mut offset_bytes = vec![0x60];
  offset_bytes.extend([0xff; 8]);
  let eight = read(&offset_bytes, 99, ObjectFormat::Sha1, &mut unbounded());
  assert_eq!(eight, Ok(None));
  offset_bytes.push(0xff);
  let nine = read(&offset_bytes, 99, ObjectFormat::Sha1, &mut unbounded());
  assert_eq!(nine, Err(Refusal::OffsetTooLong { at: 99 }));
  let farthest = Header {
    representation: Representation::OfsDelta {
      base: PACK_HEADER_LEN,
    },
    size: 0,
  };
  let bytes = written(farthest, u64::MAX);
  assert_eq!(bytes.len(), 11, "one byte of type and size, ten of offset");
  let read_farthest = read(&bytes, u64::MAX, ObjectFormat::Sha1, &mut unbounded());
  assert_eq!(read_farthest, Ok(Some((farthest, 11))));
}

/// A-582 part 3 (`gitformat-pack.adoc` 81: "Type 5 is reserved for future expansion. Type 0 is invalid."). Do: read a
/// first byte of type 0 and of type 5, with and without the bit that says the size goes on, and of every other type.
/// Expect: 0 and 5 refused as `Type` from that one byte, whatever would follow; every other type read, or waiting for
/// its size's next byte.
#[test]
fn types_zero_and_five_are_refused_from_the_first_byte() {
  for code in 0u8..8 {
    for first in [code << 4, (code << 4) | 0x8f] {
      let read_first = read(&[first], 40, ObjectFormat::Sha1, &mut unbounded());
      match code {
        0 | 5 => assert_eq!(read_first, Err(Refusal::Type { at: 40, code })),
        _ => assert!(read_first.is_ok(), "type {code}: {read_first:?}"),
      }
    }
  }
}

/// A-582 part 3 (the caller's admission). Do: read a blob's header naming 100 bytes under admissions of 99 and of 100,
/// and a SHA-256 reference delta's naming 16 under 15 and 16. Expect: each refused past its admission with nothing
/// charged, and charged exactly at it.
#[test]
fn a_header_naming_more_than_its_admission_is_refused_and_charges_nothing() {
  let base = ObjectId::from_bytes(ObjectFormat::Sha256, &[7; 32]).unwrap();
  for (header, format) in [
    (
      Header {
        representation: Representation::Whole(Kind::Blob),
        size: 100,
      },
      ObjectFormat::Sha1,
    ),
    (
      Header {
        representation: Representation::RefDelta { base },
        size: 16,
      },
      ObjectFormat::Sha256,
    ),
  ] {
    let bytes = written(header, 500);
    let mut short = Admission::new(header.size - 1);
    let past = PastAdmission {
      asked: header.size,
      left: header.size - 1,
    };
    let refused = read(&bytes, 500, format, &mut short);
    assert_eq!(refused, Err(Refusal::PastAdmission(past)));
    assert_eq!(short.left(), header.size - 1, "nothing charged");
    let mut exact = Admission::new(header.size);
    let read_exact = read(&bytes, 500, format, &mut exact);
    assert_eq!(read_exact, Ok(Some((header, bytes.len()))));
    assert_eq!(exact.left(), 0);
  }
}

/// A-582 part 3 (the writer refuses what git would not read). Do: write a size of 2^60, and offset deltas whose base is
/// the entry itself, after it, and inside the pack's header. Expect: `SizeTooLong`, `BaseNotBefore` twice and
/// `BaseBeforePack` naming the offset back, each with nothing written and the same refusal from `written_len`.
#[test]
fn the_writer_refuses_a_header_git_would_not_read_and_writes_nothing() {
  let at = 1000;
  let blob = Representation::Whole(Kind::Blob);
  let delta = |base| Representation::OfsDelta { base };
  for (representation, size, refusal) in [
    (blob, 1 << 60, Refusal::SizeTooLong { at }),
    (delta(at), 3, Refusal::BaseNotBefore { at }),
    (delta(at + 1), 3, Refusal::BaseNotBefore { at }),
    (delta(11), 3, Refusal::BaseBeforePack { at, offset: 989 }),
  ] {
    let header = Header {
      representation,
      size,
    };
    let mut out = Vec::new();
    assert_eq!(write(header, at, &mut out), Err(refusal));
    assert!(out.is_empty(), "{refusal:?}: nothing written");
    assert_eq!(written_len(header, at), Err(refusal));
  }
}

/// Every type a whole object may have, for a selector to pick from.
const KINDS: [Kind; 4] = [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag];

/// Any entry a pack may hold, with where it begins and its pack's format: a whole object of any
/// type, an offset delta whose base is anywhere from the pack's first entry to just before it (near
/// and far), or a reference delta with any base name; any size git reads, small and large. Drawn as
/// plain tuples and selectors, each choice as likely as the others, so the strategy needs no
/// `Arc`-backed `prop_oneof!` (R2; `crates/vcs/src/tests.rs` `step` is the pattern).
fn any_entry() -> impl Strategy<Value = (Header, u64, ObjectFormat)> {
  let at = (
    any::<bool>(),
    PACK_HEADER_LEN + 1..300,
    PACK_HEADER_LEN + 1..=u64::MAX,
  );
  let size = (any::<bool>(), 0..300u64, 0..1u64 << 60);
  (at, size, any::<bool>()).prop_flat_map(|((near, near_at, far_at), sizes, sha256)| {
    let at = if near { near_at } else { far_at };
    let size = if sizes.0 { sizes.1 } else { sizes.2 };
    let format = if sha256 {
      ObjectFormat::Sha256
    } else {
      ObjectFormat::Sha1
    };
    let farthest = at - PACK_HEADER_LEN;
    let offsets = (any::<bool>(), 1..=farthest.min(300), 1..=farthest);
    let name = proptest::collection::vec(any::<u8>(), format.raw_len());
    (0u8..3, 0..KINDS.len(), offsets, name).prop_map(
      move |(selector, kind, (near, near_offset, far_offset), name)| {
        let representation = match selector {
          0 => Representation::Whole(KINDS[kind]),
          1 => Representation::OfsDelta {
            base: at - if near { near_offset } else { far_offset },
          },
          _ => Representation::RefDelta {
            base: ObjectId::from_bytes(format, &name).unwrap(),
          },
        };
        let header = Header {
          representation,
          size,
        };
        (header, at, format)
      },
    )
  })
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-582 part 3: do write any entry's header at its place, then read it back with any bytes after it, and read every
  /// cut of it. Expect: the same header and its whole length back, as many bytes as `written_len` says, its size
  /// charged exactly; every cut not yet whole, charging nothing.
  #[test]
  fn any_header_reads_back_as_written_and_no_cut_of_it_is_whole(
    (header, at, format) in any_entry(),
    after in proptest::collection::vec(any::<u8>(), 0..12),
  ) {
    let mut bytes = written(header, at);
    let len = bytes.len();
    prop_assert_eq!(written_len(header, at), Ok(u64::try_from(len).unwrap()));
    for cut in 0..len {
      let mut admission = Admission::new(0);
      prop_assert_eq!(read(&bytes[..cut], at, format, &mut admission), Ok(None));
    }
    bytes.extend_from_slice(&after);
    let mut admission = Admission::new(header.size);
    prop_assert_eq!(read(&bytes, at, format, &mut admission), Ok(Some((header, len))));
    prop_assert_eq!(admission.left(), 0);
  }
}
