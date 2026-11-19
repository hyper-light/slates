//! The packs stock git 2.50.1 wrote with offset deltas and with reference deltas, and `git verify-pack -v`'s listing of
//! each (A-582 part 1's `fixtures/make-fixtures.sh`, run once on the Mac Studio on 2026-11-19), with what A-582 part
//! 3's tests read from them: each listed entry; the file `text` at each of the fixture repository's three commits,
//! rebuilt by the script's own recipe; and the two deltas the packs carry, inflated once by
//! `fixtures/make-delta-fixtures.sh`. The crate has no inflater yet (§4.17.2 piece 3), so the tests tie each captured
//! delta, and each whole file, back to its entry by the Adler-32 its zlib stream ends with (RFC 1950 §2.2). Compiled in,
//! never read at run time.

use crate::object_name::{Kind, ObjectFormat, ObjectId};

/// The pack `git pack-objects --delta-base-offset` wrote: its deltas name their bases by offset.
const OFS_PACK: &[u8] = include_bytes!("../../fixtures/ofs.pack");
/// `git verify-pack -v`'s listing of it.
const OFS_LISTING: &str = include_str!("../../fixtures/ofs.verify.txt");
/// The pack `git pack-objects` wrote without `--delta-base-offset`: its deltas name their bases by id.
const REF_PACK: &[u8] = include_bytes!("../../fixtures/ref.pack");
/// Its listing.
const REF_LISTING: &str = include_str!("../../fixtures/ref.verify.txt");
/// Each object of the fixture repository with its type and size, as `git cat-file` gave them.
const OBJECTS: &str = include_str!("../../fixtures/objects.txt");

/// The two deltas both packs carry, each named by the object it rebuilds, as `make-delta-fixtures.sh` inflated them.
pub(crate) const DELTAS: [(&str, &[u8]); 2] = [
  (
    "41debd467a3aa189a01c4e1dc6896eb686b604dc",
    include_bytes!("../../fixtures/deltas/41debd467a3aa189a01c4e1dc6896eb686b604dc.delta"),
  ),
  (
    "671f3c002c093979f501d6de93e127eb46d77a63",
    include_bytes!("../../fixtures/deltas/671f3c002c093979f501d6de93e127eb46d77a63.delta"),
  ),
];

/// How a pack's deltas name their bases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bases {
  /// By the offset back to the base's entry (`OBJ_OFS_DELTA`).
  ByOffset,
  /// By the base's name (`OBJ_REF_DELTA`).
  ById,
}

/// One entry of a pack, as `git verify-pack -v` lists it: `id type size size-in-pack offset [depth base]`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Listed {
  /// The object's name.
  pub(crate) id: ObjectId,
  /// The object's type; a delta's is its object's.
  pub(crate) kind: Kind,
  /// The size the entry's header names: the object's, or for a delta the delta's.
  pub(crate) size: u64,
  /// The bytes the entry takes in the pack, its header and its compressed data.
  pub(crate) stored: u64,
  /// Where the entry begins in the pack.
  pub(crate) offset: u64,
  /// For a delta, its base's name.
  pub(crate) base: Option<ObjectId>,
}

/// A pack under test: its name, its bytes, how its deltas name their bases, and its listing.
pub(crate) struct Pack {
  /// `ofs` or `ref`.
  pub(crate) name: &'static str,
  /// The pack's bytes.
  pub(crate) bytes: &'static [u8],
  /// How its deltas name their bases.
  pub(crate) bases: Bases,
  /// Its entries, as listed.
  pub(crate) listing: Vec<Listed>,
}

impl Pack {
  /// The pack's bytes from where `entry` begins to the pack's end.
  pub(crate) fn starting_at(&self, entry: &Listed) -> &'static [u8] {
    &self.bytes[usize::try_from(entry.offset).unwrap()..]
  }

  /// The Adler-32 the zlib stream of `entry` ends with: its last four bytes, big-endian (RFC 1950 §2.2).
  pub(crate) fn zlib_trailer(&self, entry: &Listed) -> u32 {
    let end = usize::try_from(entry.offset + entry.stored).unwrap();
    u32::from_be_bytes(self.bytes[end - 4..end].try_into().unwrap())
  }

  /// The entry listed for the object `id`.
  pub(crate) fn entry(&self, id: ObjectId) -> &Listed {
    self.listing.iter().find(|entry| entry.id == id).unwrap()
  }
}

/// The entries of a listing; its summary lines at the end name no object, so they are passed over.
fn listed(listing: &str) -> Vec<Listed> {
  listing
    .lines()
    .filter_map(|line| {
      let fields: Vec<&str> = line.split_whitespace().collect();
      let id = ObjectId::from_hex(ObjectFormat::Sha1, fields.first()?.as_bytes()).ok()?;
      Some(Listed {
        id,
        kind: Kind::from_name(fields[1].as_bytes()).unwrap(),
        size: fields[2].parse().unwrap(),
        stored: fields[3].parse().unwrap(),
        offset: fields[4].parse().unwrap(),
        base: fields
          .get(6)
          .map(|hex| ObjectId::from_hex(ObjectFormat::Sha1, hex.as_bytes()).unwrap()),
      })
    })
    .collect()
}

/// Both packs.
pub(crate) fn packs() -> [Pack; 2] {
  [
    Pack {
      name: "ofs",
      bytes: OFS_PACK,
      bases: Bases::ByOffset,
      listing: listed(OFS_LISTING),
    },
    Pack {
      name: "ref",
      bytes: REF_PACK,
      bases: Bases::ById,
      listing: listed(REF_LISTING),
    },
  ]
}

/// The object `id`'s size, from `objects.txt`.
pub(crate) fn object_size(id: ObjectId) -> u64 {
  OBJECTS
    .lines()
    .find_map(|line| {
      let fields: Vec<&str> = line.split_whitespace().collect();
      let named = ObjectId::from_hex(ObjectFormat::Sha1, fields.first()?.as_bytes()).ok()?;
      (named == id).then(|| fields[2].parse().unwrap())
    })
    .unwrap()
}

/// The file `text` at the fixture repository's commit `commit` (1 to 3), by `make-fixtures.sh`'s own recipe: two
/// hundred lines `line N of a text long enough to be stored as a delta`; the second commit spells line 100
/// `one hundred`, and the third spells line 7 `seven` as well.
pub(crate) fn text_at_commit(commit: u32) -> Vec<u8> {
  let mut text = Vec::new();
  for line in 1..=200u32 {
    let number = match line {
      100 if commit >= 2 => "one hundred".to_owned(),
      7 if commit >= 3 => "seven".to_owned(),
      _ => line.to_string(),
    };
    text.extend_from_slice(
      format!("line {number} of a text long enough to be stored as a delta\n").as_bytes(),
    );
  }
  text
}

/// Format: Adler-32's modulus, the largest prime below 65536 (RFC 1950 §8.2, `BASE`).
const ADLER_BASE: u32 = 65521;

/// The Adler-32 of `bytes` (RFC 1950 §8.2 and §9): what a zlib stream's last four bytes hold for the bytes it inflates
/// to ("ADLER32 ... a checksum value of the uncompressed data", §2.2).
pub(crate) fn adler32(bytes: &[u8]) -> u32 {
  let (mut low, mut high) = (1u32, 0u32);
  for byte in bytes {
    low = (low + u32::from(*byte)) % ADLER_BASE;
    high = (high + low) % ADLER_BASE;
  }
  (high << 16) | low
}

/// Whether `bytes` begin a zlib stream as git writes one: the method deflate (8) and a header check that makes the
/// first two bytes a multiple of 31 (RFC 1950 §2.2). So a header read one byte long or short of git's does not land on
/// one.
pub(crate) fn begins_zlib_stream(bytes: &[u8]) -> bool {
  let (method_and_info, flags) = (bytes[0], bytes[1]);
  method_and_info & 0x0f == 8 && ((u16::from(method_and_info) << 8) | u16::from(flags)) % 31 == 0
}
