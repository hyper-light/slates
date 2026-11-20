use std::io::Write as _;

use flate2::write::ZlibEncoder;
use flate2::{Compress, Compression, FlushCompress};
use proptest::prelude::*;

use super::{
  Beside, Chain, Contents, IndexMismatch, Keep, Locate, NoIndex, NoObjects, Object, Outside,
  Reader, Refusal, Unpacked, check_index, read_whole,
};
use crate::delta::{self, Sizes};
use crate::entry::{self, Header, Representation};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::pack::{self, idx::Idx};
use crate::pack_fixtures::{Pack, every_pack};
use crate::{Admission, Hold, PastAdmission};

/// `n` as a `u64`.
fn wide(n: usize) -> u64 {
  u64::try_from(n).unwrap()
}

/// `n` as a `usize`.
fn narrow(n: u64) -> usize {
  usize::try_from(n).unwrap()
}

/// An admission that holds anything, for reads whose charges a test does not look at.
fn unbounded() -> Admission {
  Admission::new(u64::MAX)
}

/// `bytes` fed to a reader `chunk` bytes at a time, then finished: refused as bytes after the
/// pack where a piece was not taken whole.
fn fed_in_chunks<L: Locate + ?Sized>(
  bytes: &[u8],
  format: ObjectFormat,
  index: &L,
  chunk: usize,
) -> Result<Unpacked, Refusal> {
  let mut admission = unbounded();
  let mut reader = Reader::new(format, index);
  let mut fed = 0u64;
  for piece in bytes.chunks(chunk) {
    let took = reader.read(piece, &mut admission)?;
    fed = fed.saturating_add(u64::try_from(took).unwrap_or(u64::MAX));
    if took < piece.len() {
      return Err(Refusal::TrailingBytes { at: fed });
    }
  }
  reader.finish(&mut admission)
}

/// An index a test places names with: each name at the offset given.
struct Placed(Vec<(ObjectId, u64)>);

impl Locate for Placed {
  fn offset_of(&self, id: &ObjectId) -> Option<u64> {
    self
      .0
      .iter()
      .find(|(known, _)| known == id)
      .map(|(_, at)| *at)
  }
}

/// `data` as one zlib stream, deflated by this crate's engine (A-607): tests hold what streams mean, never their bytes.
fn zlib(data: &[u8]) -> Vec<u8> {
  let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(data).unwrap();
  encoder.finish().unwrap()
}

/// `data` as a zlib stream that has not ended: flushed to a byte boundary, with no last block and no Adler-32.
fn zlib_unended(data: &[u8]) -> Vec<u8> {
  let mut compress = Compress::new(Compression::default(), true);
  let mut out = Vec::with_capacity(data.len() + 64);
  compress
    .compress_vec(data, &mut out, FlushCompress::Sync)
    .unwrap();
  out
}

/// A pack a test writes entry by entry.
struct Builder {
  format: ObjectFormat,
  bytes: Vec<u8>,
  count: u32,
}

impl Builder {
  /// A pack of `format`, its header's count filled in by [`Builder::finish`].
  fn new(format: ObjectFormat) -> Self {
    Self::versioned(format, 2)
  }

  /// The same, naming `version` in its header.
  fn versioned(format: ObjectFormat, version: u32) -> Self {
    let mut bytes = pack::SIGNATURE.to_vec();
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    Self {
      format,
      bytes,
      count: 0,
    }
  }

  /// Where the next entry begins.
  fn at(&self) -> u64 {
    u64::try_from(self.bytes.len()).unwrap()
  }

  /// An entry whose header names `size` and whose data is `stream`: where it begins.
  fn raw(&mut self, representation: Representation, size: u64, stream: &[u8]) -> u64 {
    let at = self.at();
    let header = Header {
      representation,
      size,
    };
    entry::write(header, at, &mut self.bytes).unwrap();
    self.bytes.extend_from_slice(stream);
    self.count += 1;
    at
  }

  /// An entry holding `data`, its header naming its size: where it begins.
  fn put(&mut self, representation: Representation, data: &[u8]) -> u64 {
    self.raw(representation, wide(data.len()), &zlib(data))
  }

  /// The pack: its count written into its header, then its trailer.
  fn finish(mut self) -> Vec<u8> {
    self.bytes[8..12].copy_from_slice(&self.count.to_be_bytes());
    let checksum = pack::checksum_of(self.format, &self.bytes).unwrap();
    self.bytes.extend_from_slice(checksum.as_bytes());
    self.bytes
  }
}

/// A delta from `source` to `target`: a copy of their common start, then the rest of the target inserted.
fn delta_between(source: &[u8], target: &[u8]) -> Vec<u8> {
  let common = source
    .iter()
    .zip(target)
    .take_while(|(a, b)| a == b)
    .count();
  let mut out = Vec::new();
  let sizes = Sizes {
    source: wide(source.len()),
    target: wide(target.len()),
  };
  delta::write_sizes(sizes, &mut out);
  if common > 0 {
    delta::write_copy(0, wide(common), &mut out).unwrap();
  }
  delta::write_insert(&target[common..], &mut out);
  out
}

/// The name of a blob holding `content`, in `format`.
fn blob_id(format: ObjectFormat, content: &[u8]) -> ObjectId {
  name_of(format, Kind::Blob, content).unwrap()
}

/// What an object of a pack must read as: where it begins, its type, its content and its delta chain.
type Expected = (u64, Kind, Vec<u8>, Option<Chain>);

/// The small pack every cut and every bit flip is tried on, in `format`, with what its five objects must read as: a
/// blob, a commit and a tag stored whole, an offset delta on the blob, and a reference delta on that delta.
fn small_pack(format: ObjectFormat) -> (Vec<u8>, Vec<Expected>) {
  let blob: &[u8] = b"hello world\n";
  let edited: &[u8] = b"hello there world\n";
  let edited_again: &[u8] = b"hello there, world!\n";
  let commit: &[u8] = b"a commit's content, as the reader sees it: bytes it names\n";
  let tag: &[u8] = b"a tag\n";
  let mut builder = Builder::new(format);
  let blob_at = builder.put(Representation::Whole(Kind::Blob), blob);
  let commit_at = builder.put(Representation::Whole(Kind::Commit), commit);
  let ofs = Representation::OfsDelta { base: blob_at };
  let edited_at = builder.put(ofs, &delta_between(blob, edited));
  let by_id = Representation::RefDelta {
    base: blob_id(format, edited),
  };
  let again_at = builder.put(by_id, &delta_between(edited, edited_again));
  let tag_at = builder.put(Representation::Whole(Kind::Tag), tag);
  let first = Chain {
    base: blob_id(format, blob),
    depth: 1,
  };
  let second = Chain {
    base: blob_id(format, edited),
    depth: 2,
  };
  let expected = vec![
    (blob_at, Kind::Blob, blob.to_vec(), None),
    (commit_at, Kind::Commit, commit.to_vec(), None),
    (edited_at, Kind::Blob, edited.to_vec(), Some(first)),
    (again_at, Kind::Blob, edited_again.to_vec(), Some(second)),
    (tag_at, Kind::Tag, tag.to_vec(), None),
  ];
  (builder.finish(), expected)
}

/// Checks `unpacked` against `pack`'s listing: every object at its listed place, of its listed type, named as git named
/// it and as its content names it, with its listed chain, and the pack's own trailer.
fn assert_listed(pack: &Pack, unpacked: &Unpacked) {
  let name = pack.name;
  assert_eq!(unpacked.objects.len(), pack.listing.len(), "{name}");
  let trailer_at = pack.bytes.len() - pack.format.raw_len();
  assert_eq!(
    unpacked.checksum.as_bytes(),
    &pack.bytes[trailer_at..],
    "{name}"
  );
  for (object, listed) in unpacked.objects.iter().zip(&pack.listing) {
    let place = (object.offset, object.stored);
    assert_eq!(
      place,
      (listed.offset, listed.stored),
      "{name} {:?}",
      listed.id
    );
    assert_eq!((object.kind, object.id), (listed.kind, listed.id), "{name}");
    let by_content = name_of(pack.format, object.kind, &object.content);
    assert_eq!(by_content, Ok(listed.id), "{name}: named by its content");
    let chain = listed.base.map(|base| Chain {
      base,
      depth: listed.depth.unwrap(),
    });
    assert_eq!(object.chain, chain, "{name} {:?}", listed.id);
    if chain.is_none() {
      assert_eq!(wide(object.content.len()), listed.size, "{name}");
    }
  }
}

/// A-608 (§4.17.2 piece 3; golden vectors: the packs stock git 2.50.1 wrote, with offset deltas and with reference
/// deltas in a SHA-1 repository and with offset deltas in a SHA-256 one, and `git verify-pack -v`'s listing of each).
/// Do: read each pack whole, with no index. Expect: every object at its listed offset and taking its listed bytes, of
/// its listed type, named as git named it (and as A-600 names its content), each delta with its listed base and depth
/// (a chain of two in each pack), each whole object of its listed size, and the trailer the pack's own.
#[test]
fn every_object_of_stock_gits_packs_reads_as_verify_pack_lists_it() {
  for pack in every_pack() {
    let unpacked = read_whole(pack.bytes, pack.format, &NoIndex, &mut unbounded()).unwrap();
    assert_listed(&pack, &unpacked);
  }
}

/// A-608 (streaming: "entries read as bytes arrive"). Do: feed each captured pack to a reader a byte at a time and in
/// chunks of 2, 3, 7, 64 and 4096 bytes. Expect: every byte taken, and the same objects and trailer as read whole.
#[test]
fn each_pack_reads_the_same_a_byte_at_a_time_and_in_any_chunks() {
  for pack in every_pack() {
    let whole = read_whole(pack.bytes, pack.format, &NoIndex, &mut unbounded()).unwrap();
    for chunk in [1, 2, 3, 7, 64, 4096] {
      let fed = fed_in_chunks(pack.bytes, pack.format, &NoIndex, chunk).unwrap();
      assert_eq!(fed, whole, "{} in chunks of {chunk}", pack.name);
    }
  }
}

/// A-608 (a reference delta's base found through an index; A-591 left an index's names to be checked against its
/// pack's objects to this piece). Do: read each captured pack through the index `git index-pack` wrote for it, and hold
/// each index to the objects read from its pack and from another pack. Expect: the same objects as with no index; each
/// index holds to its own pack; another pack's index is refused for its trailer.
#[test]
fn deltas_resolve_through_the_index_git_wrote_and_each_index_holds_to_its_objects() {
  let [ofs, by_id, sha256] = every_pack();
  for pack in [&ofs, &by_id, &sha256] {
    let idx = Idx::decode(pack.idx, pack.format).unwrap();
    let through = read_whole(pack.bytes, pack.format, &idx, &mut unbounded()).unwrap();
    let without = read_whole(pack.bytes, pack.format, &NoIndex, &mut unbounded()).unwrap();
    assert_eq!(through, without, "{}", pack.name);
    assert_eq!(check_index(&through, &idx), Ok(()), "{}", pack.name);
  }
  let unpacked = read_whole(ofs.bytes, ofs.format, &NoIndex, &mut unbounded()).unwrap();
  let other = Idx::decode(by_id.idx, by_id.format).unwrap();
  let refused = check_index(&unpacked, &other);
  assert!(
    matches!(refused, Err(IndexMismatch::Checksum { .. })),
    "{refused:?}"
  );
}

/// A-608 (an index's names held to the objects). Do: hold `ofs.idx` to the objects read from `ofs.pack` with one
/// object moved to another offset, and with one object dropped. Expect: the moved one refused, naming it, where it lies
/// and where the index places it; the dropped one refused for the count.
#[test]
fn an_index_that_places_an_object_elsewhere_or_counts_otherwise_is_refused() {
  let [ofs, _, _] = every_pack();
  let idx = Idx::decode(ofs.idx, ofs.format).unwrap();
  let unpacked = read_whole(ofs.bytes, ofs.format, &NoIndex, &mut unbounded()).unwrap();
  let mut moved = unpacked.clone();
  moved.objects[3].offset += 1;
  let object = &moved.objects[3];
  let expected = IndexMismatch::Offset {
    id: object.id,
    pack: object.offset,
    index: Some(object.offset - 1),
  };
  assert_eq!(check_index(&moved, &idx), Err(expected));
  let mut dropped = unpacked;
  dropped.objects.pop();
  let count = IndexMismatch::Count {
    index: 12,
    pack: 11,
  };
  assert_eq!(check_index(&dropped, &idx), Err(count));
}

/// A-608 (hostile input: every cut). Do: read every cut of the small pack (each prefix, from none of its bytes to all
/// but the last), in both formats, and each captured pack cut at each entry's first, second and last byte and inside
/// its trailer. Expect: every byte given taken without a refusal, then the end refused as `Truncated` at the cut.
#[test]
fn every_cut_of_a_pack_is_refused_at_its_end() {
  let mut cases = Vec::new();
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let (bytes, _) = small_pack(format);
    let cuts: Vec<usize> = (0..bytes.len()).collect();
    cases.push((format, bytes, cuts));
  }
  for pack in every_pack() {
    let mut cuts = vec![0, 1, 11, 12];
    for listed in &pack.listing {
      let at = narrow(listed.offset);
      cuts.extend([at, at + 1, at + narrow(listed.stored) - 1]);
    }
    let len = pack.bytes.len();
    cuts.extend([len - pack.format.raw_len(), len - 1]);
    cases.push((pack.format, pack.bytes.to_vec(), cuts));
  }
  for (format, bytes, cuts) in cases {
    for cut in cuts {
      let mut admission = unbounded();
      let mut reader = Reader::new(format, &NoIndex);
      assert_eq!(
        reader.read(&bytes[..cut], &mut admission),
        Ok(cut),
        "cut at {cut}"
      );
      let at = wide(cut);
      assert_eq!(
        reader.finish(&mut admission),
        Err(Refusal::Truncated { at })
      );
    }
  }
}

/// The name of a refusal's kind, for a tally.
fn kind_of(refusal: &Refusal) -> &'static str {
  match refusal {
    Refusal::Pack(pack::Refusal::Signature { .. }) => "Signature",
    Refusal::Pack(pack::Refusal::Version { .. }) => "Version",
    Refusal::Pack(pack::Refusal::Checksum) => "Checksum",
    Refusal::Pack(_) => "Pack",
    Refusal::Entry(_) => "Entry",
    Refusal::Inflate { .. } => "Inflate",
    Refusal::LongerThanSize { .. } => "LongerThanSize",
    Refusal::ShorterThanSize { .. } => "ShorterThanSize",
    Refusal::Truncated { .. } => "Truncated",
    Refusal::TrailingBytes { .. } => "TrailingBytes",
    Refusal::BaseNotAnEntry { .. } => "BaseNotAnEntry",
    Refusal::DeltaBaseMissing { .. } => "DeltaBaseMissing",
    Refusal::BaseMisplaced { .. } => "BaseMisplaced",
    Refusal::Cycle { .. } => "Cycle",
    Refusal::Delta { .. } => "Delta",
    Refusal::Collision { .. } => "Collision",
    Refusal::PastAdmission(_) => "PastAdmission",
    Refusal::AllocationRefused { .. } => "AllocationRefused",
  }
}

/// A-608 (hostile input: bit flips). Do: flip each bit of the small pack, in both formats, one at a time, and read it
/// whole. Expect: every flip refused, never read, since the trailer covers every byte; and among the refusals the
/// pack's signature, its version, an entry's header, a stream the inflater refuses, an offset delta's base that is no
/// entry, and the trailer, so none of them is vacuous. (A base found nowhere is judged once the trailer holds, as git
/// judges it, so a flip meets the trailer first; `a_delta_whose_base_is_missing_is_refused_naming_it` reaches it.)
#[test]
fn every_bit_flip_of_a_pack_is_refused_by_kind() {
  let mut seen = Vec::new();
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let (bytes, _) = small_pack(format);
    for bit in 0..bytes.len() * 8 {
      let mut flipped = bytes.clone();
      flipped[bit / 8] ^= 1 << (bit % 8);
      match read_whole(&flipped, format, &NoIndex, &mut unbounded()) {
        Ok(_) => panic!("{format:?} bit {bit}: read"),
        Err(refusal) => seen.push(kind_of(&refusal)),
      }
    }
  }
  for kind in [
    "Signature",
    "Version",
    "Entry",
    "Inflate",
    "BaseNotAnEntry",
    "Checksum",
  ] {
    assert!(seen.contains(&kind), "no flip refused as {kind}");
  }
}

/// A-608 (hostile input: a delta whose base is missing; git's "pack has %d unresolved deltas"). Do: read a pack whose
/// reference delta names a base that is in neither the pack nor its index, with no index and with one that knows other
/// names. Expect: refused as `DeltaBaseMissing`, naming the delta's place and its base.
#[test]
fn a_delta_whose_base_is_missing_is_refused_naming_it() {
  let format = ObjectFormat::Sha1;
  let absent = blob_id(format, b"never packed\n");
  let mut builder = Builder::new(format);
  builder.put(Representation::Whole(Kind::Blob), b"present\n");
  let source: &[u8] = b"never packed\n";
  let delta = delta_between(source, b"never packed, edited\n");
  let at = builder.put(Representation::RefDelta { base: absent }, &delta);
  let bytes = builder.finish();
  let missing = Err(Refusal::DeltaBaseMissing { at, base: absent });
  assert_eq!(
    read_whole(&bytes, format, &NoIndex, &mut unbounded()),
    missing
  );
  let others = Placed(vec![(blob_id(format, b"present\n"), 12)]);
  assert_eq!(
    read_whole(&bytes, format, &others, &mut unbounded()),
    missing
  );
}

/// A-608 (hostile input: a chain that loops, which only an index can make, an offset delta's base always lying before
/// it). Do: read a pack of two reference deltas through an index that places each one's base at the other, and a pack
/// of one through an index that places its base at itself. Expect: each refused as `Cycle` at the first delta.
#[test]
fn a_chain_of_deltas_that_loops_through_its_index_is_refused() {
  let format = ObjectFormat::Sha1;
  let (one, two) = (blob_id(format, b"one\n"), blob_id(format, b"two\n"));
  let delta = delta_between(b"one\n", b"one, then two\n");
  let mut builder = Builder::new(format);
  let first = builder.put(Representation::RefDelta { base: one }, &delta);
  let second = builder.put(Representation::RefDelta { base: two }, &delta);
  let bytes = builder.finish();
  let crossed = Placed(vec![(one, second), (two, first)]);
  let refused = read_whole(&bytes, format, &crossed, &mut unbounded());
  assert_eq!(refused, Err(Refusal::Cycle { at: first }));
  let mut builder = Builder::new(format);
  let alone = builder.put(Representation::RefDelta { base: one }, &delta);
  let bytes = builder.finish();
  let itself = Placed(vec![(one, alone)]);
  let refused = read_whole(&bytes, format, &itself, &mut unbounded());
  assert_eq!(refused, Err(Refusal::Cycle { at: alone }));
}

/// A-608 (a base offset must name an entry's first byte). Do: read an offset delta whose base lies inside the entry
/// before it, and reference deltas an index places inside an entry before them and past the pack's last entry. Expect:
/// each refused as `BaseNotAnEntry`, naming the delta and the offset; the one placed past the end once the pack ends.
#[test]
fn a_base_that_is_no_entry_is_refused() {
  let format = ObjectFormat::Sha1;
  let blob: &[u8] = b"a blob of some length\n";
  let delta = delta_between(blob, b"a blob of some other length\n");
  let mut builder = Builder::new(format);
  let blob_at = builder.put(Representation::Whole(Kind::Blob), blob);
  let inside = blob_at + 1;
  let at = builder.put(Representation::OfsDelta { base: inside }, &delta);
  let bytes = builder.finish();
  let refused = read_whole(&bytes, format, &NoIndex, &mut unbounded());
  assert_eq!(refused, Err(Refusal::BaseNotAnEntry { at, base: inside }));
  let id = blob_id(format, blob);
  let mut builder = Builder::new(format);
  builder.put(Representation::Whole(Kind::Blob), blob);
  let at = builder.put(Representation::RefDelta { base: id }, &delta);
  let bytes = builder.finish();
  for base in [inside, 1 << 40] {
    let placed = Placed(vec![(id, base)]);
    let refused = read_whole(&bytes, format, &placed, &mut unbounded());
    assert_eq!(refused, Err(Refusal::BaseNotAnEntry { at, base }), "{base}");
  }
}

/// A-608 (an index's answer checked). Do: read a reference delta through an index that places its base at another
/// object, before the delta and after it. Expect: refused as `BaseMisplaced`, naming the delta, the base and the name
/// found there.
#[test]
fn a_base_an_index_places_at_another_object_is_refused() {
  let format = ObjectFormat::Sha1;
  let (base, other) = (b"the base\n".as_slice(), b"another object\n".as_slice());
  let delta = delta_between(base, b"the base, edited\n");
  let (base_id, other_id) = (blob_id(format, base), blob_id(format, other));
  let mut builder = Builder::new(format);
  let other_before = builder.put(Representation::Whole(Kind::Blob), other);
  let at = builder.put(Representation::RefDelta { base: base_id }, &delta);
  builder.put(Representation::Whole(Kind::Blob), base);
  let other_after = builder.put(Representation::Whole(Kind::Blob), b"yet another\n");
  let bytes = builder.finish();
  let after_id = blob_id(format, b"yet another\n");
  for (offset, found) in [(other_before, other_id), (other_after, after_id)] {
    let placed = Placed(vec![(base_id, offset)]);
    let refused = read_whole(&bytes, format, &placed, &mut unbounded());
    let expected = Refusal::BaseMisplaced {
      at,
      base: base_id,
      found,
    };
    assert_eq!(refused, Err(expected), "placed at {offset}");
  }
}

/// A-608 (deltas whose bases come later, as `git index-pack` resolves them). Do: read a pack whose reference delta
/// comes before its base, followed by an offset delta on that delta, with no index and with one placing the base after
/// the delta. Expect: both resolved once the pack ends, the second two deep.
#[test]
fn a_delta_before_its_base_resolves_once_the_base_arrives() {
  let format = ObjectFormat::Sha256;
  let base: &[u8] = b"a base that comes last\n";
  let first: &[u8] = b"a base that comes last, edited\n";
  let second: &[u8] = b"a base that comes last, edited twice\n";
  let id = blob_id(format, base);
  let mut builder = Builder::new(format);
  let first_at = builder.put(
    Representation::RefDelta { base: id },
    &delta_between(base, first),
  );
  let ofs = Representation::OfsDelta { base: first_at };
  builder.put(ofs, &delta_between(first, second));
  let base_at = builder.put(Representation::Whole(Kind::Blob), base);
  let bytes = builder.finish();
  for index in [Placed(vec![]), Placed(vec![(id, base_at)])] {
    let unpacked = read_whole(&bytes, format, &index, &mut unbounded()).unwrap();
    let contents: Vec<&[u8]> = unpacked
      .objects
      .iter()
      .map(|object| object.content.as_slice())
      .collect();
    assert_eq!(contents, [first, second, base]);
    let chains: Vec<Option<Chain>> = unpacked.objects.iter().map(|object| object.chain).collect();
    let one = Chain { base: id, depth: 1 };
    let two = Chain {
      base: blob_id(format, first),
      depth: 2,
    };
    assert_eq!(chains, [Some(one), Some(two), None]);
  }
}

/// A-608 (each stream inflates to exactly its header's size: `unpack_entry_data`). Do: read entries whose header names
/// one byte fewer than their data, one byte more, and an empty blob of size zero. Expect: `LongerThanSize` and
/// `ShorterThanSize` naming the size (and what was inflated), and the empty blob read as git names it.
#[test]
fn a_stream_longer_or_shorter_than_its_size_is_refused() {
  let format = ObjectFormat::Sha1;
  let data: &[u8] = b"six b\n";
  for (size, refusal) in [
    (5, Refusal::LongerThanSize { at: 12, size: 5 }),
    (
      7,
      Refusal::ShorterThanSize {
        at: 12,
        size: 7,
        inflated: 6,
      },
    ),
  ] {
    let mut builder = Builder::new(format);
    builder.raw(Representation::Whole(Kind::Blob), size, &zlib(data));
    let bytes = builder.finish();
    assert_eq!(
      read_whole(&bytes, format, &NoIndex, &mut unbounded()),
      Err(refusal)
    );
  }
  let mut builder = Builder::new(format);
  builder.put(Representation::Whole(Kind::Blob), b"");
  let bytes = builder.finish();
  let unpacked = read_whole(&bytes, format, &NoIndex, &mut unbounded()).unwrap();
  let empty = ObjectId::from_hex(format, b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
  assert_eq!(unpacked.objects[0].id, empty);
}

/// A-608 (a stream that does not end is refused). Do: read a pack whose only entry's stream is flushed but never ended,
/// with nothing after it; then followed by a block git's inflate refuses; then by a last block carrying one byte more.
/// Expect: `Truncated` at the end of the bytes, `Inflate`, and `LongerThanSize`.
#[test]
fn a_stream_that_does_not_end_is_refused() {
  let format = ObjectFormat::Sha1;
  let data: &[u8] = b"a stream with no end\n";
  let unended = zlib_unended(data);
  let header = Representation::Whole(Kind::Blob);
  let mut builder = Builder::new(format);
  builder.raw(header, wide(data.len()), &unended);
  let mut bytes = builder.bytes;
  bytes[8..12].copy_from_slice(&1u32.to_be_bytes());
  let at = wide(bytes.len());
  assert_eq!(
    read_whole(&bytes, format, &NoIndex, &mut unbounded()),
    Err(Refusal::Truncated { at })
  );
  let reserved_block = [0x07];
  let last_block_one_byte = [0x01, 0x01, 0x00, 0xfe, 0xff, b'x'];
  for (after, refusal) in [
    (&reserved_block[..], Refusal::Inflate { at: 12 }),
    (
      &last_block_one_byte[..],
      Refusal::LongerThanSize {
        at: 12,
        size: wide(data.len()),
      },
    ),
  ] {
    let mut stream = unended.clone();
    stream.extend_from_slice(after);
    let mut builder = Builder::new(format);
    builder.raw(header, wide(data.len()), &stream);
    let bytes = builder.finish();
    assert_eq!(
      read_whole(&bytes, format, &NoIndex, &mut unbounded()),
      Err(refusal)
    );
  }
}

/// Objects outside a pack that a test holds, as a receiver's repository holds a thin pack's bases.
struct Held(Vec<(ObjectId, Kind, Vec<u8>)>);

impl Outside for Held {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    self
      .0
      .iter()
      .find(|(known, _, _)| known == id)
      .map(|(_, kind, content)| (*kind, content.as_slice()))
  }
}

/// A-620 (a thin pack's bases from outside it, as `index-pack --fix-thin` takes them from the repository). Do: read a
/// pack of a reference delta on a blob only the receiver holds and a second delta on the first one's object, finished
/// with that blob held outside it and with nothing outside it. Expect: both objects made, one delta and two deep, the
/// held blob not among the answer's objects; with nothing outside, the first delta's base refused as found nowhere.
#[test]
fn a_thin_pack_resolves_its_bases_from_outside_the_pack() {
  let format = ObjectFormat::Sha1;
  let base: &[u8] = b"a base only the receiver holds\n";
  let first: &[u8] = b"a base only the receiver holds, edited\n";
  let second: &[u8] = b"a base only the receiver holds, edited twice\n";
  let base_id = blob_id(format, base);
  let mut builder = Builder::new(format);
  let first_at = builder.put(
    Representation::RefDelta { base: base_id },
    &delta_between(base, first),
  );
  let on_first = Representation::RefDelta {
    base: blob_id(format, first),
  };
  builder.put(on_first, &delta_between(first, second));
  let bytes = builder.finish();
  let held = Held(vec![(base_id, Kind::Blob, base.to_vec())]);
  let mut admission = unbounded();
  let mut reader = Reader::new(format, &NoIndex);
  reader.read(&bytes, &mut admission).unwrap();
  let unpacked = reader.finish_with(&held, &mut admission).unwrap();
  let contents: Vec<&[u8]> = unpacked
    .objects
    .iter()
    .map(|object| object.content.as_slice())
    .collect();
  assert_eq!(contents, [first, second]);
  let chains: Vec<Option<Chain>> = unpacked.objects.iter().map(|object| object.chain).collect();
  let one = Chain {
    base: base_id,
    depth: 1,
  };
  let two = Chain {
    base: blob_id(format, first),
    depth: 2,
  };
  assert_eq!(chains, [Some(one), Some(two)]);
  let refused = read_whole(&bytes, format, &NoIndex, &mut unbounded());
  assert_eq!(
    refused,
    Err(Refusal::DeltaBaseMissing {
      at: first_at,
      base: base_id
    })
  );
}

/// A-608 (a stream's Adler-32 checked). Do: read an entry stored uncompressed whose one data byte was changed after
/// deflating. Expect: refused as `Inflate`, the stream's own check failing.
#[test]
fn a_stream_whose_data_was_changed_is_refused_by_its_adler32() {
  let format = ObjectFormat::Sha1;
  let mut encoder = ZlibEncoder::new(Vec::new(), Compression::none());
  encoder.write_all(b"hello world\n").unwrap();
  let mut stream = encoder.finish().unwrap();
  let h = stream.iter().position(|byte| *byte == b'h').unwrap();
  stream[h] = b'j';
  let mut builder = Builder::new(format);
  builder.raw(Representation::Whole(Kind::Blob), 12, &stream);
  let bytes = builder.finish();
  let refused = read_whole(&bytes, format, &NoIndex, &mut unbounded());
  assert_eq!(refused, Err(Refusal::Inflate { at: 12 }));
}

/// A-608 (the pack's header and trailer, as A-591 reads them). Do: read the small pack with its signature's last byte
/// changed, at version 4, at version 3, and with its trailer's last byte changed. Expect: the signature refused at
/// byte 3 as soon as it arrives, version 4 refused, version 3 read as version 2 is, and the trailer refused.
#[test]
fn the_packs_header_and_trailer_are_held_to_gits_rules() {
  let format = ObjectFormat::Sha1;
  let (bytes, _) = small_pack(format);
  let mut reader = Reader::new(format, &NoIndex);
  let signature = Refusal::Pack(pack::Refusal::Signature { at: 3 });
  assert_eq!(reader.read(b"PACX", &mut unbounded()), Err(signature));
  let mut builder = Builder::versioned(format, 4);
  builder.put(Representation::Whole(Kind::Blob), b"x");
  let refused = read_whole(&builder.finish(), format, &NoIndex, &mut unbounded());
  assert_eq!(
    refused,
    Err(Refusal::Pack(pack::Refusal::Version { version: 4 }))
  );
  let mut builder = Builder::versioned(format, 3);
  builder.put(Representation::Whole(Kind::Blob), b"x");
  let three = read_whole(&builder.finish(), format, &NoIndex, &mut unbounded()).unwrap();
  assert_eq!(three.objects[0].content, b"x");
  let mut changed = bytes;
  *changed.last_mut().unwrap() ^= 1;
  let refused = read_whole(&changed, format, &NoIndex, &mut unbounded());
  assert_eq!(refused, Err(Refusal::Pack(pack::Refusal::Checksum)));
}

/// Whether `refusal` is an admission's, at any level: the reader's, an entry's or the applier's.
fn past_admission(refusal: &Refusal) -> bool {
  matches!(
    refusal,
    Refusal::PastAdmission(_)
      | Refusal::Entry(entry::Refusal::PastAdmission(_))
      | Refusal::Delta {
        refusal: delta::Refusal::PastAdmission(_),
        ..
      }
  )
}

/// A-608 (every size charged to the caller's admission before it is built). Do: read the small pack under exactly what
/// reading it charges, and one byte less; and give a reader a header counting 2^32 - 1 objects under an admission of a
/// mebibyte. Expect: the first read whole with the admission spent; the second refused by an admission; the header
/// refused as soon as its twelve bytes are in, before any entry.
#[test]
fn every_size_is_charged_to_the_admission_before_it_is_built() {
  let format = ObjectFormat::Sha1;
  let (bytes, _) = small_pack(format);
  let mut measure = unbounded();
  read_whole(&bytes, format, &NoIndex, &mut measure).unwrap();
  let charged = u64::MAX - measure.left();
  let mut exact = Admission::new(charged);
  assert!(read_whole(&bytes, format, &NoIndex, &mut exact).is_ok());
  assert_eq!(exact.left(), 0);
  let refused = read_whole(&bytes, format, &NoIndex, &mut Admission::new(charged - 1)).unwrap_err();
  assert!(past_admission(&refused), "{refused:?}");
  let mut header = pack::SIGNATURE.to_vec();
  header.extend_from_slice(&2u32.to_be_bytes());
  header.extend_from_slice(&u32::MAX.to_be_bytes());
  let mut reader = Reader::new(format, &NoIndex);
  let refused = reader.read(&header, &mut Admission::new(1 << 20));
  assert!(
    matches!(refused, Err(Refusal::PastAdmission(PastAdmission { .. }))),
    "{refused:?}"
  );
}

/// A-608 (a chain's depth bounded by the admission, never by a number). Do: read a chain of 300 deltas, each on the
/// one before, six times git's default `pack.depth` of 50, each version a kibibyte so its targets outweigh the rest,
/// under an admission that holds it and under one short of half its targets. Expect: the first read whole, the last
/// delta 300 deep; the second refused by an admission.
#[test]
fn a_chain_as_deep_as_the_admission_holds_resolves_and_no_deeper() {
  let format = ObjectFormat::Sha1;
  let versions: Vec<Vec<u8>> = (0..=300)
    .map(|version| {
      format!(
        "version {version}\n{}\n",
        "of a file kept as a chain of deltas ".repeat(28)
      )
      .into_bytes()
    })
    .collect();
  let mut builder = Builder::new(format);
  let mut previous = builder.put(Representation::Whole(Kind::Blob), &versions[0]);
  for pair in versions.windows(2) {
    let ofs = Representation::OfsDelta { base: previous };
    previous = builder.put(ofs, &delta_between(&pair[0], &pair[1]));
  }
  let bytes = builder.finish();
  let mut measure = unbounded();
  let unpacked = read_whole(&bytes, format, &NoIndex, &mut measure).unwrap();
  assert_eq!(unpacked.objects[300].chain.unwrap().depth, 300);
  assert_eq!(unpacked.objects[300].content, versions[300]);
  let targets: u64 = versions[1..]
    .iter()
    .map(|version| wide(version.len()))
    .sum();
  let charged = u64::MAX - measure.left();
  let short = &mut Admission::new(charged - targets / 2);
  let refused = read_whole(&bytes, format, &NoIndex, short).unwrap_err();
  assert!(past_admission(&refused), "{refused:?}");
}

/// A keeper that stores each object outside the reader's admission and gives every hold back, as a push's intake stores
/// objects in its repository's store (A-719 part 2).
#[derive(Default)]
struct Elsewhere {
  /// The objects kept, by position.
  objects: std::collections::BTreeMap<usize, Object>,
}

impl Keep for Elsewhere {
  fn keep(
    &mut self,
    at: usize,
    object: Object,
    hold: Hold,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    self.objects.insert(at, object);
    admission.release(hold);
    Ok(())
  }

  fn release(&mut self, hold: Hold, admission: &mut Admission) {
    admission.release(hold);
  }
}

impl Contents for Elsewhere {
  fn content(&self, at: usize, id: &ObjectId) -> Option<std::borrow::Cow<'_, [u8]>> {
    self
      .objects
      .get(&at)
      .filter(|object| object.id == *id)
      .map(|object| std::borrow::Cow::Borrowed(object.content.as_slice()))
  }
}

impl Outside for Elsewhere {
  fn object(&self, _id: &ObjectId) -> Option<(Kind, &[u8])> {
    None
  }
}

/// A-719 part 2 (§4.17.1: a push's working set bounded whatever its size). Do: write the 300-deep chain of
/// [`a_chain_as_deep_as_the_admission_holds_resolves_and_no_deeper`] and read it into a keeper that stores each object
/// elsewhere, fed in pieces of 61 bytes, under an admission of the reader's tables (256 bytes an object) and eight
/// versions' bytes, which cannot hold a third of its targets; then read it whole into memory under the same admission. Expect: kept whole
/// in the keeper, every object as the pack read whole without bound makes it, the last 300 deep, and each placed in
/// pack order with its name; read into memory, refused past its admission.
#[test]
fn a_pack_read_into_a_keeper_that_stores_elsewhere_holds_a_bounded_working_set() {
  let format = ObjectFormat::Sha1;
  let versions: Vec<Vec<u8>> = (0..=300)
    .map(|version| {
      format!(
        "version {version}\n{}\n",
        "of a file kept as a chain of deltas ".repeat(28)
      )
      .into_bytes()
    })
    .collect();
  let mut builder = Builder::new(format);
  let mut previous = builder.put(Representation::Whole(Kind::Blob), &versions[0]);
  for pair in versions.windows(2) {
    let ofs = Representation::OfsDelta { base: previous };
    previous = builder.put(ofs, &delta_between(&pair[0], &pair[1]));
  }
  let bytes = builder.finish();
  let whole = read_whole(&bytes, format, &NoIndex, &mut unbounded()).unwrap();
  let room = wide(versions.len() * 256 + 8 * versions[300].len());
  let mut keeper = Elsewhere::default();
  let mut admission = Admission::new(room);
  let mut reader = Reader::new(format, &NoIndex);
  for piece in bytes.chunks(61) {
    assert_eq!(
      reader.read_into(piece, &mut admission, &mut keeper),
      Ok(piece.len())
    );
  }
  let kept = reader.finish_into(&mut admission, &mut keeper).unwrap();
  let placed: Vec<(ObjectId, u64)> = kept
    .objects
    .iter()
    .map(|placed| (placed.id, placed.offset))
    .collect();
  let read: Vec<(ObjectId, u64)> = whole
    .objects
    .iter()
    .map(|object| (object.id, object.offset))
    .collect();
  assert_eq!(placed, read);
  let stored: Vec<Object> = keeper.objects.into_values().collect();
  assert_eq!(stored, whole.objects);
  assert_eq!(kept.objects[300].chain.map(|chain| chain.depth), Some(300));
  let refused = read_whole(&bytes, format, &NoIndex, &mut Admission::new(room)).unwrap_err();
  assert!(past_admission(&refused), "{refused:?}");
}

/// A-608 ("pack has junk at the end"). Do: read the small pack whole with a byte after its trailer, and give a reader
/// the same bytes. Expect: the whole read refused as `TrailingBytes` where the trailer ends; the reader takes the
/// pack's bytes and no more.
#[test]
fn bytes_after_the_trailer_are_not_the_packs() {
  let format = ObjectFormat::Sha1;
  let (mut bytes, _) = small_pack(format);
  let len = bytes.len();
  bytes.push(0);
  let refused = read_whole(&bytes, format, &NoIndex, &mut unbounded());
  assert_eq!(refused, Err(Refusal::TrailingBytes { at: wide(len) }));
  let mut admission = unbounded();
  let mut reader = Reader::new(format, &NoIndex);
  assert_eq!(reader.read(&bytes, &mut admission), Ok(len));
  assert!(reader.finish(&mut admission).is_ok());
}

/// A-608 (the small pack, as the cut and flip tests use it). Do: read it in both formats. Expect: its five objects at
/// their places with their contents, types, names and chains, the reference delta resolved once the pack ended.
#[test]
fn the_small_pack_reads_as_it_was_written() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let (bytes, expected) = small_pack(format);
    let unpacked = read_whole(&bytes, format, &NoIndex, &mut unbounded()).unwrap();
    let read: Vec<(u64, Kind, Vec<u8>, Option<Chain>)> = unpacked
      .objects
      .iter()
      .map(|object| {
        (
          object.offset,
          object.kind,
          object.content.clone(),
          object.chain,
        )
      })
      .collect();
    assert_eq!(read, expected, "{format:?}");
    for object in &unpacked.objects {
      assert_eq!(name_of(format, object.kind, &object.content), Ok(object.id));
    }
  }
}

/// One object of a generated pack: its content, and for a delta the position of its base among the objects made
/// before it.
#[derive(Clone, Debug)]
struct Made {
  content: Vec<u8>,
  base: Option<usize>,
}

/// Up to six blobs and six deltas, each delta on any object made before it, every content distinct (each begins with
/// its own number, two digits wide); and the order the pack holds them in, any permutation.
fn generated() -> impl Strategy<Value = (Vec<Made>, Vec<usize>)> {
  let blobs = proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..40), 1..6);
  let deltas = proptest::collection::vec(
    (
      any::<prop::sample::Index>(),
      proptest::collection::vec(any::<u8>(), 0..40),
    ),
    0..6,
  );
  (blobs, deltas).prop_flat_map(|(blobs, deltas)| {
    let mut made: Vec<Made> = Vec::new();
    for content in blobs
      .into_iter()
      .chain(deltas.iter().map(|(_, content)| content.clone()))
    {
      let number = format!("{:02}|", made.len()).into_bytes();
      made.push(Made {
        content: [number, content].concat(),
        base: None,
      });
    }
    let blob_count = made.len() - deltas.len();
    for (position, (base, _)) in deltas.iter().enumerate() {
      let made_at = blob_count + position;
      made[made_at].base = Some(base.index(made_at));
    }
    let order: Vec<usize> = (0..made.len()).collect();
    (Just(made), Just(order).prop_shuffle())
  })
}

/// The pack `made` makes in `order`: an offset delta where the base lies before it, a reference delta otherwise. Its
/// bytes, and each object's offset by its number.
fn generated_pack(format: ObjectFormat, made: &[Made], order: &[usize]) -> (Vec<u8>, Vec<u64>) {
  let mut builder = Builder::new(format);
  let mut offsets = vec![0u64; made.len()];
  let mut placed: Vec<Option<u64>> = vec![None; made.len()];
  for &number in order {
    let object = &made[number];
    let at = match object.base {
      None => builder.put(Representation::Whole(Kind::Blob), &object.content),
      Some(base) => {
        let delta = delta_between(&made[base].content, &object.content);
        let representation = match placed[base] {
          Some(base_at) => Representation::OfsDelta { base: base_at },
          None => Representation::RefDelta {
            base: blob_id(format, &made[base].content),
          },
        };
        builder.put(representation, &delta)
      }
    };
    offsets[number] = at;
    placed[number] = Some(at);
  }
  (builder.finish(), offsets)
}

/// The depth `made`'s object `number` lies at.
fn depth_of(made: &[Made], number: usize) -> u64 {
  let mut depth = 0;
  let mut current = number;
  while let Some(base) = made[current].base {
    depth += 1;
    current = base;
  }
  depth
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-608 (a model oracle). Do: write any blobs and any deltas on them, in any order, offset deltas where the base
  /// lies before and reference deltas where it lies after, and read the pack whole with no index, with an index that
  /// places every object, and fed in chunks of any size. Expect: each object at its offset with its content, named by
  /// it, a delta's chain its base's name and its depth in the model, the same each way.
  #[test]
  fn any_pack_of_blobs_and_deltas_reads_as_the_model_says(
    (made, order) in generated(),
    chunk in 1usize..64,
    sha256 in any::<bool>(),
  ) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let (bytes, offsets) = generated_pack(format, &made, &order);
    let unpacked = read_whole(&bytes, format, &NoIndex, &mut unbounded()).unwrap();
    for (object, &number) in unpacked.objects.iter().zip(&order) {
      prop_assert_eq!(object.offset, offsets[number]);
      prop_assert_eq!(&object.content, &made[number].content);
      prop_assert_eq!(object.id, blob_id(format, &made[number].content));
      let chain = made[number].base.map(|base| Chain {
        base: blob_id(format, &made[base].content),
        depth: depth_of(&made, number),
      });
      prop_assert_eq!(object.chain, chain);
    }
    let every = Placed(
      made
        .iter()
        .zip(&offsets)
        .map(|(object, offset)| (blob_id(format, &object.content), *offset))
        .collect(),
    );
    let through = read_whole(&bytes, format, &every, &mut unbounded()).unwrap();
    prop_assert_eq!(&through, &unpacked);
    prop_assert_eq!(fed_in_chunks(&bytes, format, &NoIndex, chunk).unwrap(), unpacked);
  }
}

/// A clock that moves a nanosecond each time it is read, so a turn of one nanosecond is spent after its first unit of
/// work.
struct Ticking(u64);

impl crate::store::learn::Clock for Ticking {
  fn now_ns(&mut self) -> u64 {
    self.0 += 1;
    self.0
  }
}

/// Shape: a resolution turn's budget, in units of the ticking clock (one unit of work a read).
const BUDGET: u64 = 4;

/// The objects `held` keeps.
fn kept(held: &super::Held) -> u64 {
  held.objects.iter().flatten().count() as u64
}

/// Resolves `reader`'s waiting deltas in turns of [`BUDGET`] on a ticking clock: the turns taken, and how many resolved
/// more than their budget.
fn in_turns(
  reader: &mut Reader<'_, Placed>,
  admission: &mut Admission,
  held: &mut super::Held,
) -> (u32, u32) {
  let mut clock = Ticking(0);
  let (mut turns, mut past_budget) = (0u32, 0u32);
  loop {
    turns += 1;
    assert!(turns <= 1_000, "the resolution never ended");
    let before = kept(held);
    let quantum = crate::store::hook_view::Quantum {
      clock: &mut clock,
      ns: BUDGET,
    };
    let done = reader
      .resolve_turn(
        admission,
        &mut Beside {
          outside: &NoObjects,
          held: &mut *held,
        },
        quantum,
      )
      .unwrap();
    if kept(held) - before > BUDGET {
      past_budget += 1;
    }
    if done {
      return (turns, past_budget);
    }
  }
}

/// A-724's starvation fix (found by use: a push's last input step resolved every delta waiting on a late base in one
/// poll, 1,435.7 ms for git/git v1.5.0 on the 32-core Studio, starving the small repositories on its shard and ending
/// the daemon at ten such pushes). Do: read a pack of 64 reference deltas whose base comes last (the input step), then
/// resolve its waiting deltas in turns of a budget of four units on a clock that ticks each read, counting each step
/// that resolved more than its budget. Expect: the past-budget count 0 for the input step (it resolves none of the 64:
/// before the fix it resolved all of them, past any budget) and 0 over every turn after it; many turns; every object as reading the pack whole in one call makes it; and the pack then finished with
/// nothing left to resolve.
#[test]
fn deltas_waiting_for_a_late_base_resolve_a_turn_at_a_time() {
  let format = ObjectFormat::Sha256;
  let base = b"a base every delta waits for, and it comes last\n".repeat(4);
  let id = blob_id(format, &base);
  let mut builder = Builder::new(format);
  for n in 0..64u32 {
    let target = [base.as_slice(), format!("edit {n}\n").as_bytes()].concat();
    builder.put(
      Representation::RefDelta { base: id },
      &delta_between(&base, &target),
    );
  }
  builder.put(Representation::Whole(Kind::Blob), &base);
  let bytes = builder.finish();
  let index = Placed(vec![]);
  let whole = read_whole(&bytes, format, &index, &mut unbounded()).unwrap();
  let mut reader = Reader::new(format, &index);
  let mut admission = unbounded();
  let mut held = super::Held::default();
  reader
    .read_into(
      &bytes,
      &mut admission,
      &mut Beside {
        outside: &NoObjects,
        held: &mut held,
      },
    )
    .unwrap();
  assert!(reader.is_whole());
  let input_step = kept(&held);
  assert_eq!(input_step, 1, "the input step resolved waiting deltas");
  let (turns, past_budget) = in_turns(&mut reader, &mut admission, &mut held);
  assert_eq!(past_budget, 0, "turns past their budget");
  assert!(
    turns >= 16,
    "64 waiting deltas resolved in {turns} turns of {BUDGET} units"
  );
  for (at, object) in whole.objects.iter().enumerate() {
    let kept = held.take(at).expect("each object kept");
    assert_eq!(kept.content, object.content, "object {at}");
  }
  let kept = reader
    .finish_into(
      &mut admission,
      &mut Beside {
        outside: &NoObjects,
        held: &mut held,
      },
    )
    .unwrap();
  assert_eq!(kept.objects.len(), 65);
}
