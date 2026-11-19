//! A-665: an object held whole as a zlib stream of stored blocks, held to zlib itself (flate2):
//! every stream inflates to its object, ends with the Adler-32 zlib writes, is the length the view
//! gives it, and reads the same in any window; and the Adler-32 is RFC 1950's.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use super::super::ReadRefused;
use super::{ADLER_LEN, BLOCK_MAX, Source, adler_at, adler32, fill, stream_len};

/// An object's bytes in memory, and its Adler-32 when `known`.
struct Bytes<'a> {
  bytes: &'a [u8],
  known: bool,
}

impl Source for Bytes<'_> {
  fn data(&mut self, from: u64, to: u64, out: &mut Vec<u8>) -> Result<(), ReadRefused> {
    let (from, to) = (usize::try_from(from).unwrap(), usize::try_from(to).unwrap());
    out.extend_from_slice(&self.bytes[from..to]);
    Ok(())
  }

  fn adler(&mut self) -> Result<u32, ReadRefused> {
    if self.known {
      Ok(adler32(self.bytes))
    } else {
      Err(ReadRefused::Unreadable)
    }
  }
}

/// Lengths at and about each block's boundary, and an empty object.
fn lengths() -> [u64; 6] {
  [
    0,
    1,
    BLOCK_MAX - 1,
    BLOCK_MAX,
    BLOCK_MAX + 1,
    3 * BLOCK_MAX + 5,
  ]
}

/// An object of `len` bytes no two neighbouring blocks repeat.
fn object(len: u64) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from(at.wrapping_mul(31) % 251).unwrap())
    .collect()
}

/// The whole stream of `bytes`.
fn stream_of(bytes: &[u8]) -> Vec<u8> {
  let mut stream = Vec::new();
  let mut source = Bytes { bytes, known: true };
  let len = u64::try_from(bytes.len()).unwrap();
  fill(len, (0, u64::MAX), &mut source, &mut stream).unwrap();
  stream
}

/// zlib's own stream of `bytes`, at its default level.
fn zlibs(bytes: &[u8]) -> Vec<u8> {
  let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(bytes).unwrap();
  encoder.finish().unwrap()
}

/// A-665 (the view's `stored` encoding). Do: make the stream of objects at and about each block's
/// boundary, and an empty one, inflate it with zlib, and compare its last four bytes with zlib's
/// own stream of the object. Expect: it inflates to the object, its length the one the view gives,
/// and its Adler-32 zlib's.
#[test]
fn a_stored_stream_inflates_to_its_object_and_ends_with_zlibs_adler32() {
  for len in lengths() {
    let bytes = object(len);
    let stream = stream_of(&bytes);
    assert_eq!(u64::try_from(stream.len()).unwrap(), stream_len(len));
    let mut inflated = Vec::new();
    ZlibDecoder::new(&stream[..])
      .read_to_end(&mut inflated)
      .unwrap();
    assert_eq!(inflated, bytes);
    let zlib = zlibs(&bytes);
    let adler = usize::try_from(ADLER_LEN).unwrap();
    assert_eq!(zlib[zlib.len() - adler..], stream[stream.len() - adler..]);
  }
}

/// A-665 (a window a read). Do: read the stream of a three-block object in windows of every size
/// about a block's header and a block, and from offsets inside each piece. Expect: every way of
/// reading it gives the whole stream's bytes.
#[test]
fn any_window_of_a_stream_is_the_same_bytes_as_the_whole() {
  let len = 2 * BLOCK_MAX + 7;
  let bytes = object(len);
  let whole = stream_of(&bytes);
  assert_eq!(u64::try_from(whole.len()).unwrap(), stream_len(len));
  for window in [
    1,
    2,
    4,
    5,
    6,
    7,
    4096,
    BLOCK_MAX,
    BLOCK_MAX + 5,
    stream_len(len),
  ] {
    let mut pieces = Vec::new();
    let mut at = 0;
    while at < stream_len(len) {
      let mut source = Bytes {
        bytes: &bytes,
        known: true,
      };
      fill(len, (at, at + window), &mut source, &mut pieces).unwrap();
      at += window;
    }
    assert_eq!(pieces, whole, "window {window}");
  }
}

/// A-665 (the seal holds what needs the whole object). Do: read a stream whose Adler-32 is not
/// known, up to its last four bytes, then those. Expect: everything before the Adler-32 read, the
/// Adler-32 refused as the source refuses it.
#[test]
fn a_read_short_of_the_adler32_never_asks_for_it() {
  let len = BLOCK_MAX + 3;
  let bytes = object(len);
  let whole = stream_of(&bytes);
  let mut source = Bytes {
    bytes: &bytes,
    known: false,
  };
  let mut before = Vec::new();
  fill(len, (0, adler_at(len)), &mut source, &mut before).unwrap();
  assert_eq!(before[..], whole[..whole.len() - 4]);
  let mut after = Vec::new();
  assert_eq!(
    fill(len, (adler_at(len), u64::MAX), &mut source, &mut after),
    Err(ReadRefused::Unreadable)
  );
}

/// A-665 (Adler-32 as zlib's `adler32.c` takes it). Do: take Adler-32 of RFC 1950's known vectors
/// and of a long run of the largest byte, where the sums reduce most. Expect: the known values, and
/// the long run's value the naive per-byte sums' modulo 65521.
#[test]
fn adler32_is_rfc_1950s() {
  assert_eq!(adler32(b""), 1);
  assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
  let long = vec![u8::MAX; 100_000];
  let (mut low, mut high) = (1_u32, 0_u32);
  for byte in &long {
    low = (low + u32::from(*byte)) % 65_521;
    high = (high + low) % 65_521;
  }
  assert_eq!(adler32(&long), (high << 16) | low);
}
