//! A-591: a pack's header and trailer, held to the packs stock git 2.50.1 wrote (`fixtures/make-fixtures.sh` and
//! `fixtures/make-index-fixtures.sh`, run on the Mac Studio on 2026-11-19) and to hostile bytes.

use super::{Header, Refusal, SIGNATURE, Version, Writer, check, read_header};
use crate::entry::PACK_HEADER_LEN;
use crate::object_name::{Hasher, ObjectFormat, ObjectId};
use crate::out::{Measure, Out};

/// A SHA-1 repository's twelve objects as `git pack-objects --delta-base-offset` packed them: two blobs stored as
/// deltas naming their bases by offset.
pub(crate) const OFS_PACK: &[u8] = include_bytes!("../../fixtures/ofs.pack");
/// The same objects packed without `--delta-base-offset`: the deltas name their bases by id.
pub(crate) const REF_PACK: &[u8] = include_bytes!("../../fixtures/ref.pack");
/// The same history made in a SHA-256 repository, packed with offset deltas.
pub(crate) const SHA256_PACK: &[u8] = include_bytes!("../../fixtures/sha256.pack");

/// Every pack git wrote, named, with its repository's hash.
pub(crate) const PACKS: [(&str, &[u8], ObjectFormat); 3] = [
  ("ofs.pack", OFS_PACK, ObjectFormat::Sha1),
  ("ref.pack", REF_PACK, ObjectFormat::Sha1),
  ("sha256.pack", SHA256_PACK, ObjectFormat::Sha256),
];

/// The objects in each pack git wrote: `git verify-pack -v` listed twelve for each.
const OBJECTS: u32 = 12;

/// The pack git writes for no objects: `git pack-objects --stdout < /dev/null` in an empty SHA-1 repository, git
/// 2.50.1 (Apple Git-155), 2026-11-19. Its header, version 2 and a count of zero, then its trailer.
const EMPTY_PACK_HEADER: [u8; 12] = *b"PACK\x00\x00\x00\x02\x00\x00\x00\x00";
/// That empty pack's trailer, as `xxd` showed it.
const EMPTY_PACK_TRAILER: &str = "029d08823bd8a8eab510ad6ac75c823cfd3ed31e";

/// A pack header's length, A-582's [`PACK_HEADER_LEN`], as an index into bytes.
pub(crate) fn header_len() -> usize {
  usize::try_from(PACK_HEADER_LEN).unwrap()
}

/// The checksum `format` takes of `bytes`.
pub(crate) fn checksum(format: ObjectFormat, bytes: &[u8]) -> ObjectId {
  let mut hasher = Hasher::raw(format);
  hasher.update(bytes);
  hasher.finish().unwrap()
}

/// `bytes` with their last `format` hash made the checksum of every byte before it again: a hostile file whose
/// checksum holds, so what is refused is what was changed, not the checksum.
pub(crate) fn resign(mut bytes: Vec<u8>, format: ObjectFormat) -> Vec<u8> {
  let at = bytes.len() - format.raw_len();
  let sum = checksum(format, &bytes[..at]);
  bytes[at..].copy_from_slice(sum.as_bytes());
  bytes
}

/// A-591: do check each pack git wrote. Expect it whole: version 2, twelve objects, its trailer the hash of every byte
/// before it, its objects every byte between its header and its trailer.
#[test]
fn each_pack_git_wrote_is_checked_and_its_header_read() {
  for (name, bytes, format) in PACKS {
    let pack = check(bytes, format).unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
    assert_eq!(
      pack.header(),
      Header {
        version: Version::Two,
        objects: OBJECTS
      },
      "{name}"
    );
    let trailer_at = bytes.len() - format.raw_len();
    assert_eq!(pack.trailer_at(), trailer_at, "{name}");
    assert_eq!(pack.checksum().as_bytes(), &bytes[trailer_at..], "{name}");
    assert_eq!(
      pack.checksum(),
      checksum(format, &bytes[..trailer_at]),
      "{name}"
    );
    assert_eq!(pack.objects(), &bytes[header_len()..trailer_at], "{name}");
    assert_eq!(pack.bytes(), bytes, "{name}");
    assert_eq!(pack.format(), format, "{name}");
  }
}

/// A-591: do write each pack git wrote again from its count and its objects. Expect git's bytes exactly, the trailer
/// computed as they passed, and a measured write of the same length.
#[test]
fn each_pack_is_written_back_byte_for_byte() {
  for (name, bytes, format) in PACKS {
    let pack = check(bytes, format).unwrap();
    let mut out = Vec::new();
    let mut writer = Writer::new(format, pack.header().objects, &mut out);
    writer.put(pack.objects());
    let trailer = writer.finish().unwrap();
    assert_eq!(out, bytes, "{name}");
    assert_eq!(trailer, pack.checksum(), "{name}");
    let mut measure = Measure::default();
    let mut measured = Writer::new(format, pack.header().objects, &mut measure);
    measured.put(pack.objects());
    measured.finish().unwrap();
    assert_eq!(measure, Measure(bytes.len() as u64), "{name}");
  }
}

/// A-591: do write a pack of no objects. Expect the bytes git writes for one, which read back as a pack of no objects.
#[test]
fn a_pack_of_no_objects_is_the_one_git_writes() {
  let mut out = Vec::new();
  let trailer = Writer::new(ObjectFormat::Sha1, 0, &mut out)
    .finish()
    .unwrap();
  assert_eq!(&out[..header_len()], &EMPTY_PACK_HEADER[..]);
  assert_eq!(trailer.to_string(), EMPTY_PACK_TRAILER);
  assert_eq!(&out[header_len()..], trailer.as_bytes());
  let pack = check(&out, ObjectFormat::Sha1).unwrap();
  assert_eq!(pack.header().objects, 0);
  assert!(pack.objects().is_empty());
}

/// A-591: do read a pack's header from each prefix of a pack, as a stream delivers it. Expect no answer until the
/// twelfth byte has arrived, then the header, whatever follows.
#[test]
fn a_header_is_read_as_its_bytes_arrive() {
  for len in 0..header_len() {
    assert_eq!(read_header(&OFS_PACK[..len]), Ok(None), "{len} bytes");
  }
  let header = Header {
    version: Version::Two,
    objects: OBJECTS,
  };
  assert_eq!(read_header(&OFS_PACK[..header_len()]), Ok(Some(header)));
  assert_eq!(read_header(OFS_PACK), Ok(Some(header)));
}

/// A-591: do change each byte of a pack's signature and read the header as far as that byte. Expect a refusal naming
/// the byte as soon as it arrives, before the rest of the header has.
#[test]
fn a_wrong_signature_byte_is_refused_as_soon_as_it_arrives() {
  for at in 0..SIGNATURE.len() {
    let mut bytes = OFS_PACK[..header_len()].to_vec();
    bytes[at] ^= 0x20;
    let refusal = Err(Refusal::Signature { at: at as u64 });
    assert_eq!(read_header(&bytes[..=at]), refusal, "byte {at}");
    assert_eq!(read_header(&bytes), refusal, "byte {at}");
  }
  assert_eq!(read_header(b"PACX"), Err(Refusal::Signature { at: 3 }));
}

/// A-591: do read a header naming each version from 0 to 4 and the largest. Expect versions 2 and 3 read, 3 as itself,
/// and every other refused as soon as its four bytes have arrived.
#[test]
fn only_versions_two_and_three_are_read() {
  for version in [0, 1, 2, 3, 4, u32::MAX] {
    let mut bytes = SIGNATURE.to_vec();
    bytes.extend_from_slice(&version.to_be_bytes());
    let early = read_header(&bytes);
    bytes.extend_from_slice(&7u32.to_be_bytes());
    let whole = read_header(&bytes);
    match version {
      2 | 3 => {
        let version = if version == 2 {
          Version::Two
        } else {
          Version::Three
        };
        assert_eq!(early, Ok(None));
        assert_eq!(
          whole,
          Ok(Some(Header {
            version,
            objects: 7
          }))
        );
      }
      _ => {
        assert_eq!(early, Err(Refusal::Version { version }));
        assert_eq!(whole, Err(Refusal::Version { version }));
      }
    }
  }
}

/// A-591: do flip each bit of a SHA-1 pack and of a SHA-256 pack, one at a time, and check each. Expect every one
/// refused by its trailer, a flip inside the header too: the trailer is verified before any byte it guards is read.
#[test]
fn a_flipped_bit_anywhere_in_a_pack_is_refused_by_its_trailer() {
  for (name, bytes, format) in [PACKS[0], PACKS[2]] {
    for bit in 0..bytes.len() * 8 {
      let mut flipped = bytes.to_vec();
      flipped[bit / 8] ^= 1 << (bit % 8);
      assert_eq!(
        check(&flipped, format),
        Err(Refusal::Checksum),
        "{name} bit {bit}"
      );
    }
  }
}

/// A-591: do check every prefix of a pack too short for a header and a trailer. Expect each refused as truncated,
/// naming the bytes a pack of no objects takes and the bytes there were.
#[test]
fn a_pack_shorter_than_its_header_and_trailer_is_refused() {
  for (name, bytes, format) in PACKS {
    let needed = PACK_HEADER_LEN + format.raw_len() as u64;
    for len in 0..header_len() + format.raw_len() {
      assert_eq!(
        check(&bytes[..len], format),
        Err(Refusal::Truncated {
          needed,
          left: len as u64
        }),
        "{name} {len} bytes"
      );
    }
  }
}

/// A-591: do check packs whose trailers hold but whose headers are wrong: a signature byte, a version of 4, and the
/// version 3 git also reads. Expect the first two refused by their headers and the third read as version 3.
#[test]
fn a_whole_pack_with_a_wrong_header_is_refused_by_its_header() {
  let format = ObjectFormat::Sha1;
  let mut signature = OFS_PACK.to_vec();
  signature[2] = b'X';
  assert_eq!(
    check(&resign(signature, format), format),
    Err(Refusal::Signature { at: 2 })
  );
  let mut version = OFS_PACK.to_vec();
  version[4..8].copy_from_slice(&4u32.to_be_bytes());
  assert_eq!(
    check(&resign(version, format), format),
    Err(Refusal::Version { version: 4 })
  );
  let mut three = OFS_PACK.to_vec();
  three[4..8].copy_from_slice(&3u32.to_be_bytes());
  let three = resign(three, format);
  assert_eq!(
    check(&three, format).map(|pack| pack.header()),
    Ok(Header {
      version: Version::Three,
      objects: OBJECTS
    })
  );
}

/// A-591: do check a SHA-1 pack as SHA-256 and a SHA-256 pack as SHA-1. Expect each refused by its trailer: a pack
/// does not say its hash, and the wrong one never matches.
#[test]
fn a_pack_checked_in_the_other_hash_is_refused() {
  assert_eq!(
    check(OFS_PACK, ObjectFormat::Sha256),
    Err(Refusal::Checksum)
  );
  assert_eq!(
    check(SHA256_PACK, ObjectFormat::Sha1),
    Err(Refusal::Checksum)
  );
}
