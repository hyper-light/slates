use proptest::prelude::*;

use super::{
  CollisionAttack, Hasher, Header, HeaderRefusal, HexRefusal, Kind, KindRefusal, MAX_HEADER_LEN,
  ObjectFormat, ObjectId, UnknownFormat, name_of, read_header, write_header,
};
use crate::out::Measure;
use crate::{Admission, PastAdmission};

/// The names stock git 2.50.1 gave the objects of `fixtures/objects/sha1`, from `make-object-fixtures.sh`: a line a
/// type, then the empty blob's.
const SHA1_NAMES: &str = include_str!("../../fixtures/objects/sha1/names.txt");
/// The same objects, made in a SHA-256 repository.
const SHA256_NAMES: &str = include_str!("../../fixtures/objects/sha256/names.txt");
/// Each object of the SHA-1 repository, its content as `git cat-file` printed it raw.
const SHA1_OBJECTS: [(Kind, &[u8]); 4] = [
  (
    Kind::Blob,
    include_bytes!("../../fixtures/objects/sha1/blob.bin"),
  ),
  (
    Kind::Tree,
    include_bytes!("../../fixtures/objects/sha1/tree.bin"),
  ),
  (
    Kind::Commit,
    include_bytes!("../../fixtures/objects/sha1/commit.bin"),
  ),
  (
    Kind::Tag,
    include_bytes!("../../fixtures/objects/sha1/tag.bin"),
  ),
];
/// Each object of the SHA-256 repository.
const SHA256_OBJECTS: [(Kind, &[u8]); 4] = [
  (
    Kind::Blob,
    include_bytes!("../../fixtures/objects/sha256/blob.bin"),
  ),
  (
    Kind::Tree,
    include_bytes!("../../fixtures/objects/sha256/tree.bin"),
  ),
  (
    Kind::Commit,
    include_bytes!("../../fixtures/objects/sha256/commit.bin"),
  ),
  (
    Kind::Tag,
    include_bytes!("../../fixtures/objects/sha256/tag.bin"),
  ),
];
/// git's own t0013 vector, `shattered-1.pdf`, cut to the 320 bytes that hold both colliding blocks.
const SHATTERED_1: &[u8] = include_bytes!("../../fixtures/objects/shattered-1.prefix.bin");
/// Its SHAttered twin from shattered.io, cut the same.
const SHATTERED_2: &[u8] = include_bytes!("../../fixtures/objects/shattered-2.prefix.bin");
/// The plain SHA-1 both cuts share (`shasum -a 1` of each, 2026-11-19): what git's refusal names.
const SHATTERED_SHA1: &str = "f92d74e3874587aaf443d1db961d4e26dde13e9c";

/// The objects of `format`'s repository and the names git gave them: each object's type, content and name, the empty
/// blob last.
fn golden(format: ObjectFormat) -> Vec<(Kind, &'static [u8], ObjectId)> {
  let (names, objects) = match format {
    ObjectFormat::Sha1 => (SHA1_NAMES, SHA1_OBJECTS),
    ObjectFormat::Sha256 => (SHA256_NAMES, SHA256_OBJECTS),
  };
  names
    .lines()
    .map(|line| {
      let (label, hex) = line.split_once(' ').unwrap();
      let name = ObjectId::from_hex(format, hex.as_bytes()).unwrap();
      if label == "empty" {
        return (Kind::Blob, &b""[..], name);
      }
      let kind = Kind::from_name(label.as_bytes()).unwrap();
      let (_, content) = objects.iter().find(|(known, _)| *known == kind).unwrap();
      (kind, *content, name)
    })
    .collect()
}

/// `header` as `write_header` writes it.
fn written(header: Header) -> Vec<u8> {
  let mut out = Vec::new();
  write_header(header, &mut out);
  out
}

/// A-600 (§4.17.2 piece 2; golden vectors from `fixtures/objects/make-object-fixtures.sh`, stock git 2.50.1). Do: name
/// each object stock git made in its SHA-1 repository and in its SHA-256 one (a blob, a tree with a subtree, a commit,
/// an annotated tag, and the empty blob), from its type and content alone, at once and fed a byte at a time. Expect:
/// each name the one `git hash-object` gave it, written back as git writes it, in lower-case hexadecimal of the
/// format's length.
#[test]
fn every_object_is_named_as_stock_git_names_it_in_both_formats() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let golden = golden(format);
    assert_eq!(golden.len(), 5, "{format:?}: four types and the empty blob");
    for (kind, content, name) in golden {
      assert_eq!(
        name_of(format, kind, content),
        Ok(name),
        "{format:?} {kind:?}"
      );
      let mut hasher = Hasher::object(
        format,
        Header {
          kind,
          size: u64::try_from(content.len()).unwrap(),
        },
      );
      for byte in content {
        hasher.update(core::slice::from_ref(byte));
      }
      assert_eq!(
        hasher.finish(),
        Ok(name),
        "{format:?} {kind:?}, fed a byte at a time"
      );
      let hex = name.to_string();
      assert_eq!(hex.len(), format.hex_len());
      assert_eq!(hex, hex.to_ascii_lowercase());
      assert_eq!(ObjectId::from_hex(format, hex.as_bytes()), Ok(name));
      assert_eq!(name.as_bytes().len(), format.raw_len());
      assert_eq!(name.format(), format);
    }
  }
}

/// A-600 (git's `sha1dc_git.c`: detection on, the safe hash off; `t/t0013-sha1dc.sh`). Do: hash git's own t0013
/// vector and its SHAttered twin, each cut to the 320 bytes that hold both colliding blocks, as raw SHA-1 the way git's
/// `test-tool sha1` does; then hash both under SHA-256. Expect: the two cuts different bytes; each refused as a
/// collision attack under SHA-1, named by the plain SHA-1 they share, as git's refusal names it; and under SHA-256 two
/// different names and no refusal.
#[test]
fn the_shattered_pair_is_refused_as_part_of_a_collision_attack() {
  assert_ne!(SHATTERED_1, SHATTERED_2, "the two cuts differ");
  let shared = ObjectId::from_hex(ObjectFormat::Sha1, SHATTERED_SHA1.as_bytes()).unwrap();
  for cut in [SHATTERED_1, SHATTERED_2] {
    let mut hasher = Hasher::raw(ObjectFormat::Sha1);
    hasher.update(cut);
    assert_eq!(hasher.finish(), Err(CollisionAttack { sha1: shared }));
  }
  let mut names = Vec::new();
  for cut in [SHATTERED_1, SHATTERED_2] {
    let mut hasher = Hasher::raw(ObjectFormat::Sha256);
    hasher.update(cut);
    names.push(hasher.finish().unwrap());
  }
  assert_ne!(names[0], names[1], "SHA-256 tells the two apart");
}

/// A-600 (`object-file.c`'s `parse_loose_header` and `format_object_header`). Do: read headers of each type and of
/// sizes from zero to `u64::MAX`, and write each back. Expect: each read as its type and size, its length the bytes up
/// to and including its NUL, the content after it left unread, the size charged to the admission, and each written
/// back byte for byte.
#[test]
fn a_header_is_read_as_git_reads_it_and_written_back() {
  let cases: [(&[u8], Kind, u64); 5] = [
    (b"blob 0\0", Kind::Blob, 0),
    (b"tree 62\0", Kind::Tree, 62),
    (b"commit 180\0", Kind::Commit, 180),
    (b"tag 147\0", Kind::Tag, 147),
    (b"blob 18446744073709551615\0", Kind::Blob, u64::MAX),
  ];
  for (header, kind, size) in cases {
    let mut input = header.to_vec();
    input.extend_from_slice(b"the content after the header");
    let mut admission = Admission::new(u64::MAX);
    let read = read_header(&input, &mut admission);
    assert_eq!(read, Ok((Header { kind, size }, header.len())));
    assert_eq!(admission.left(), u64::MAX - size, "the size is charged");
    assert_eq!(written(Header { kind, size }), header);
    let mut measure = Measure::default();
    write_header(Header { kind, size }, &mut measure);
    assert_eq!(measure.0, u64::try_from(header.len()).unwrap());
  }
}

/// A-600 (`parse_loose_header`: "canonical decimal format (ie "010" is not valid)", "followed by a zero byte";
/// `type_from_string_gently`; gitformat-pack: ofs-delta and ref-delta "only valid in a pack file"; `MAX_HEADER_LEN`).
/// Do: read headers git refuses, and some it reads, at the edges. Expect: each refusal typed and where it stopped; a
/// pack-only type refused by kind, apart from an unknown one; a header whose NUL is the last of `MAX_HEADER_LEN` bytes
/// read, one byte longer refused; a header the bytes have not finished yet refused as not yet whole.
#[test]
fn a_header_git_refuses_is_refused_by_kind_and_place() {
  let mut room = Admission::new(u64::MAX);
  let refused: [(&[u8], HeaderRefusal); 13] = [
    (b"blob 012\0", HeaderRefusal::NotDecimal { at: 6 }),
    (b"blob 00\0", HeaderRefusal::NotDecimal { at: 6 }),
    (b"blob \0", HeaderRefusal::NotDecimal { at: 5 }),
    (b"blob 1a\0", HeaderRefusal::NotDecimal { at: 6 }),
    (b"blob +1\0", HeaderRefusal::NotDecimal { at: 5 }),
    (b"blob  1\0", HeaderRefusal::NotDecimal { at: 5 }),
    (b"blob 18446744073709551616\0", HeaderRefusal::SizeOverflow),
    (b"ofs-delta 5\0", HeaderRefusal::Kind(KindRefusal::PackOnly)),
    (b"ref-delta 5\0", HeaderRefusal::Kind(KindRefusal::PackOnly)),
    (b"blobs 5\0", HeaderRefusal::Kind(KindRefusal::Unknown)),
    (b"BLOB 5\0", HeaderRefusal::Kind(KindRefusal::Unknown)),
    (b"blob\0 5", HeaderRefusal::NoSize),
    (b"blob 5", HeaderRefusal::Truncated),
  ];
  for (header, refusal) in refused {
    assert_eq!(
      read_header(header, &mut room),
      Err(refusal),
      "{:?}",
      String::from_utf8_lossy(header)
    );
  }
  assert_eq!(room.left(), u64::MAX, "nothing refused was charged");
  let within = [vec![b'x'; MAX_HEADER_LEN - 3], b" 5\0".to_vec()].concat();
  assert_eq!(within.len(), MAX_HEADER_LEN);
  assert_eq!(
    read_header(&within, &mut room),
    Err(HeaderRefusal::Kind(KindRefusal::Unknown)),
    "a NUL at the last byte git reads ends the header"
  );
  let past = [vec![b'x'; MAX_HEADER_LEN - 2], b" 5\0".to_vec()].concat();
  assert_eq!(read_header(&past, &mut room), Err(HeaderRefusal::TooLong));
  assert_eq!(
    read_header(&[b'x'; MAX_HEADER_LEN - 1], &mut room),
    Err(HeaderRefusal::Truncated),
    "fewer bytes than git reads, no NUL among them, may yet be whole"
  );
}

/// A-600 (§4.2, A-16: a decode charges what it will hold before it holds it). Do: read a header naming 100 bytes under
/// an admission of 99, then of 100. Expect: the first refused, naming what it asked and what was left, with nothing
/// charged; the second read, the admission spent.
#[test]
fn a_header_naming_more_than_the_admission_holds_is_refused_before_its_content() {
  let mut short = Admission::new(99);
  assert_eq!(
    read_header(b"blob 100\0", &mut short),
    Err(HeaderRefusal::PastAdmission(PastAdmission {
      asked: 100,
      left: 99
    }))
  );
  assert_eq!(short.left(), 99);
  let mut enough = Admission::new(100);
  assert_eq!(
    read_header(b"blob 100\0", &mut enough),
    Ok((
      Header {
        kind: Kind::Blob,
        size: 100
      },
      9
    ))
  );
  assert_eq!(enough.left(), 0);
}

/// A-600 (gitprotocol-v2 `object-format`; `hash.c`'s `hash_algos`, compared by `hash_algo_by_name` with `strcmp`). Do:
/// map each format to its value and back, and read values git does not know. Expect: `sha1` and `sha256` each way, and
/// any other spelling refused.
#[test]
fn object_formats_map_to_their_values_both_ways() {
  for (format, value) in [
    (ObjectFormat::Sha1, "sha1"),
    (ObjectFormat::Sha256, "sha256"),
  ] {
    assert_eq!(format.name(), value);
    assert_eq!(ObjectFormat::from_name(value.as_bytes()), Ok(format));
  }
  for unknown in [&b"SHA1"[..], b"sha-1", b"sha256 ", b"sha512", b""] {
    assert_eq!(ObjectFormat::from_name(unknown), Err(UnknownFormat));
  }
}

/// A-600 (`hex.c`: `hexval` reads either case; `hash_to_hex` writes lower case). Do: read a name written in upper case,
/// a name of the other format's length, and one with a byte that is no digit. Expect: the upper-case name equal to the
/// lower-case one; the wrong length refused with the length; the bad byte refused with its place.
#[test]
fn an_object_name_is_read_from_hex_of_either_case_and_refused_by_place() {
  let lower = ObjectId::from_hex(ObjectFormat::Sha1, SHATTERED_SHA1.as_bytes()).unwrap();
  let upper = ObjectId::from_hex(
    ObjectFormat::Sha1,
    SHATTERED_SHA1.to_ascii_uppercase().as_bytes(),
  )
  .unwrap();
  assert_eq!(upper, lower);
  assert_eq!(upper.to_string(), SHATTERED_SHA1);
  assert_eq!(
    ObjectId::from_hex(ObjectFormat::Sha256, SHATTERED_SHA1.as_bytes()),
    Err(HexRefusal::Length { len: 40 })
  );
  let mut bad = SHATTERED_SHA1.as_bytes().to_vec();
  bad[7] = b'g';
  assert_eq!(
    ObjectId::from_hex(ObjectFormat::Sha1, &bad),
    Err(HexRefusal::NotHex { at: 7 })
  );
  assert_eq!(
    ObjectId::from_bytes(ObjectFormat::Sha256, lower.as_bytes()),
    None,
    "twenty bytes are no SHA-256 name"
  );
}

/// Any of git's four types.
fn kind() -> impl Strategy<Value = Kind> {
  prop_oneof![
    Just(Kind::Commit),
    Just(Kind::Tree),
    Just(Kind::Blob),
    Just(Kind::Tag)
  ]
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-600: do write a header of any type and size, read it back from before any content, and name content of any
  /// length fed in any cut. Expect: the same header and its whole length back, and every cut naming the content as
  /// one feed does, in both formats.
  #[test]
  fn headers_round_trip_and_a_name_is_the_same_however_its_content_is_fed(
    kind in kind(),
    size in any::<u64>(),
    content in proptest::collection::vec(any::<u8>(), 0..300),
    cut in any::<prop::sample::Index>(),
  ) {
    let header = Header { kind, size };
    let mut bytes = written(header);
    let len = bytes.len();
    bytes.extend_from_slice(&content);
    prop_assert_eq!(read_header(&bytes, &mut Admission::new(u64::MAX)), Ok((header, len)));
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
      let whole = name_of(format, kind, &content);
      let (first, second) = content.split_at(cut.index(content.len() + 1));
      let mut hasher = Hasher::object(format, Header { kind, size: u64::try_from(content.len()).unwrap() });
      hasher.update(first);
      hasher.update(second);
      prop_assert_eq!(hasher.finish(), whole);
    }
  }
}
