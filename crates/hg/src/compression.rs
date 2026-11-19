//! Compressed bundles: zlib (`GZ`), bzip2 (`BZ`) and zstd (`ZS`), inflated within the caller's admission and
//! compressed as Mercurial 7.2 writes them (A-598; §4.17.3 piece 1; D-17).
//!
//! **What 7.2 writes** (`bundle2.py`, `utils/compression.py` \[C\]). A bundle1 names its compression in its six-byte
//! header: `HG10UN` and the changegroup, `HG10GZ` and a zlib stream of it, or `HG10` and a whole bzip2 stream, whose
//! own magic `BZ` completes the header (`bundletypes`). A bundle2 names it in its `Compression` stream parameter
//! (`GZ`, `BZ` or `ZS`), and every byte after the stream parameters, every part and the stream's end, is one stream of
//! that engine (`bundle20.setcompression`, `getchunks`). The engines are the ones 7.2's Python reaches: zlib's format,
//! with its header and its Adler-32 (RFC 1950; not gzip), one bzip2 stream with its block and stream CRCs, and one zstd
//! frame (RFC 8878).
//!
//! **Reading: the plain bundle.** [`plain`] gives a bundle as the plain bundle of its kind, as 7.2's own relay of a
//! bundle2 makes one (`unbundle20._forwardchunks`): its stream inflated and its `Compression` parameter dropped, or a
//! bundle1's changegroup inflated behind `HG10UN`. An uncompressed bundle comes back as it was, borrowed, nothing
//! copied. [`Bundle2::decode`](crate::bundle2::Bundle2::decode) and [`changegroup`](crate::bundle1::changegroup)
//! read the plain bundle, and refuse by kind one given them still compressed.
//!
//! **The admission (§4.9, A-16).** A few compressed bytes can claim gigabytes, so nothing is held that the caller's
//! [`Admission`] has not granted first. Before a decoder is made, the memory its stream's header sizes is charged: the
//! zlib window, the tables of the bzip2 block its header names, or libzstd's two buffers for the frame's window. Then
//! room for inflated bytes is charged before it is reserved: the compressed stream's own length first, then as much
//! again as is inflated so far, never past what the admission has left. A stream that needs more is refused
//! [`StreamRefusal::PastAdmission`], a compression bomb refused by kind, with nothing past the admission ever held;
//! and since a call inflates no more than its admission, its work is bounded with it. The decoders' own fixed state,
//! the same for every stream whatever its bytes, is the engines' and is not charged.
//!
//! **Writing.** [`compress_bundle`] writes a plain bundle compressed as 7.2 writes it, and [`compress`] one stream.
//! The level is the caller's (§4.17.3's learned response compression); each engine's range is its own
//! ([`Engine::levels`]) and a level outside it is refused, never clamped. A zstd stream is one frame that names its
//! content's size, which 7.2's reader takes as its own (it reads one frame), and which lets libzstd choose a window no
//! larger than the content, so a small bundle costs its reader little.
//!
//! **Stricter than 7.2, each on purpose.** 7.2 reads a stream lazily and stops at the bundle's end, so it ignores bytes
//! after a compressed stream and a zlib trailer it never reached; slates inflates the whole stream, refuses one cut
//! short anywhere ([`StreamRefusal::Truncated`]) and bytes after its end ([`StreamRefusal::TrailingBytes`]). 7.2's set
//! of bundle types holds `_truncatedBZ` and `ZS` for either kind, so its readers take a bundle2
//! `Compression=_truncatedBZ` and a bundle1 `HG10ZS`, which no 7.2 writer makes (`_truncatedBZ` has no bundle-spec
//! name, and `zstd-v1` is refused: "compression engine zstd is not supported on v1 bundles", `bundlecaches.py`);
//! slates refuses both as unknown, and a bundle2 that names `Compression` twice (7.2 takes the last). A zstd stream
//! must open with a standard frame: libzstd's legacy formats, which the workspace's `zstd` can decode, never reach it.
//!
//! **Engines.** zlib through flate2's own API, on the backend the workspace chose by measurement (A-607; nothing here
//! names one), bzip2 through the `bzip2` crate on its pure-Rust `libbz2-rs-sys`, zstd through the workspace's `zstd`
//! (libzstd 1.5.7). Proven against golden vectors stock 7.2 made (`fixtures/compressed`, the command in
//! `make-compressed-fixtures.sh`): each compressed bundle is made plain to exactly the uncompressed bundle 7.2 made of
//! the same repository. The tests hold what a stream inflates to, never the compressed bytes, since two engines may
//! compress one input two ways and both be right.

use std::borrow::Cow;
use std::ops::RangeInclusive;

use bzip2::{
  Action as Bzip2Action, Compress as Bzip2Compress, Compression as Bzip2Level,
  Decompress as Bzip2Decompress, Error as Bzip2Error, Status as Bzip2Status,
};
use flate2::{
  Compress as ZlibCompress, Compression as ZlibLevel, Decompress as ZlibDecompress, FlushCompress,
  FlushDecompress, Status as ZlibStatus,
};
use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{self, CCtx, CParameter, DCtx, DParameter, InBuffer, OutBuffer};

use crate::bundle2::{self, StreamParam};
use crate::out::Out;
use crate::quote;
use crate::read::{Cursor, to_u64};
use crate::{Admission, PastAdmission, Truncated, bundle1};

/// A compression engine, as a bundle names it (`compressionengine.bundletype()` \[C\]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
  /// `UN`: no compression (7.2's `none`).
  Uncompressed,
  /// `GZ`: zlib, a DEFLATE stream with zlib's header and Adler-32 trailer (RFC 1950; 7.2's `zlib`).
  Zlib,
  /// `BZ`: one bzip2 stream (7.2's `bz2`).
  Bzip2,
  /// `ZS`: one zstd frame (RFC 8878; 7.2's `zstd`).
  Zstd,
}

/// Format: the stream parameter that names a bundle2's compression, as 7.2 writes it: mandatory, so capitalised
/// (`bundle20.setcompression`: `addparam(b'Compression', alg)`).
const COMPRESSION_PARAMETER: &[u8] = b"Compression";

/// Format: the engines this crate reads, in the order 7.2 registers them (`utils/compression.py`).
const ENGINES: [Engine; 4] = [
  Engine::Zlib,
  Engine::Bzip2,
  Engine::Uncompressed,
  Engine::Zstd,
];

/// Format: zlib's levels, 0 (stored) to 9 (zlib's manual, `deflateInit`); 7.2's `-1` is zlib's default, 6, which a
/// caller names as 6.
const ZLIB_LEVELS: RangeInclusive<i32> = 0..=9;

/// Format: bzip2's levels, 1 to 9: its block size in units of [`BZIP2_BLOCK_UNIT`] bytes, the digit its stream's
/// header carries (bzip2 manual §2.4, `-1 .. -9`).
const BZIP2_LEVELS: RangeInclusive<i32> = 1..=9;

/// Format: the level an uncompressed stream takes: none, so 0 alone.
const UNCOMPRESSED_LEVELS: RangeInclusive<i32> = 0..=0;

/// Format: bzip2's block unit: a stream's block size digit counts blocks of 100,000 bytes (bzip2 manual §2.4;
/// libbzip2's `blockSize100k`).
const BZIP2_BLOCK_UNIT: u64 = 100_000;

/// Format: a bzip2 stream's magic and version, before its block size digit (libbzip2's `BZ2_decompress`).
const BZIP2_MAGIC: [u8; 3] = *b"BZh";

/// Format: the largest zlib window, 32 KiB: CINFO is at most 7, a window of 2^(CINFO + 8) bytes (RFC 1950 §2.2).
/// flate2 asks its backend for that window whatever a stream's header declares (`Decompress::new` passes 15 window
/// bits), so it is what a zlib stream's decoder holds.
const ZLIB_WINDOW: u64 = 1 << 15;

/// Format: a zstd frame's magic number, read little-endian (RFC 8878 §3.1.1).
const ZSTD_MAGIC: u32 = 0xFD2F_B528;

/// Format: the Frame_Header_Descriptor's Single_Segment_flag, bit 5 (RFC 8878 §3.1.1.1.1.2).
const ZSTD_SINGLE_SEGMENT: u8 = 1 << 5;

/// Format: the Frame_Header_Descriptor's Reserved_bit, bit 3, which a decoder refuses set (RFC 8878 §3.1.1.1.1.4).
const ZSTD_RESERVED: u8 = 1 << 3;

/// Format: the shift to the Frame_Header_Descriptor's Frame_Content_Size_flag, its top two bits (RFC 8878
/// §3.1.1.1.1.1).
const ZSTD_CONTENT_SIZE_SHIFT: u32 = 6;

/// Format: the mask of the Frame_Header_Descriptor's Dictionary_ID_flag, its low two bits (RFC 8878 §3.1.1.1.1.6).
const ZSTD_DICTIONARY_FLAG: u8 = 0b11;

/// Format: the Dictionary_ID field's length for each Dictionary_ID_flag (RFC 8878 §3.1.1.1.1.6).
const ZSTD_DICTIONARY_ID_LEN: [usize; 4] = [0, 1, 2, 4];

/// Format: the Frame_Content_Size field's length for each Frame_Content_Size_flag; flag 0 is one byte in a
/// single-segment frame and none in another (RFC 8878 §3.1.1.1.1.1).
const ZSTD_CONTENT_SIZE_LEN: [usize; 4] = [0, 2, 4, 8];

/// Format: the Frame_Content_Size_flag whose two-byte field holds the size less 256 (RFC 8878 §3.1.1.1.4).
const ZSTD_CONTENT_SIZE_OFFSET_FLAG: usize = 1;

/// Format: what that two-byte Frame_Content_Size adds to the value it holds (RFC 8878 §3.1.1.1.4).
const ZSTD_CONTENT_SIZE_OFFSET: u64 = 256;

/// Format: the shift to the Window_Descriptor's Exponent, its top five bits (RFC 8878 §3.1.1.1.2).
const ZSTD_EXPONENT_SHIFT: u32 = 3;

/// Format: the mask of the Window_Descriptor's Mantissa, its low three bits (RFC 8878 §3.1.1.1.2).
const ZSTD_MANTISSA: u8 = 0b111;

/// Format: the window log of Exponent 0, windowLog = 10 + Exponent (RFC 8878 §3.1.1.1.2), which is libzstd's smallest
/// window too (`ZSTD_WINDOWLOG_ABSOLUTEMIN`).
const ZSTD_WINDOW_LOG_MIN: u32 = 10;

/// Format: the shift from a window's base to an eighth of it, what one Mantissa unit adds: windowAdd =
/// (windowBase / 8) × Mantissa (RFC 8878 §3.1.1.1.2).
const ZSTD_EIGHTH_SHIFT: u32 = 3;

/// Format: the largest block, 128 KiB: Block_Maximum_Size is the smaller of Window_Size and 128 KiB (RFC 8878
/// §3.1.1.2.4; libzstd's `ZSTD_BLOCKSIZE_MAX`).
const ZSTD_BLOCK_MAX: u64 = 1 << 17;

/// Format: the slack libzstd 1.5.7 keeps past each of its output buffer's two block margins (`WILDCOPY_OVERLENGTH`,
/// `common/zstd_internal.h`).
const ZSTD_WILDCOPY_OVERLENGTH: u64 = 32;

/// Format: the smallest input buffer libzstd 1.5.7 holds, a frame checksum's four bytes (`ZSTD_decompressStream`:
/// `MAX(blockSizeMax, 4 /* frame checksum */)`).
const ZSTD_CHECKSUM_LEN: u64 = 4;

impl Engine {
  /// The bundle type that names the engine: `UN`, `GZ`, `BZ` or `ZS`.
  pub const fn bundle_type(self) -> [u8; 2] {
    match self {
      Self::Uncompressed => *b"UN",
      Self::Zlib => *b"GZ",
      Self::Bzip2 => *b"BZ",
      Self::Zstd => *b"ZS",
    }
  }

  /// The engine's name on the wire protocol, as 7.2's `wireprotosupport` names it: what a client lists after `comp=`,
  /// what an `application/mercurial-0.2` answer's header names, and what `compression=` advertises.
  pub const fn wire_name(self) -> &'static [u8] {
    match self {
      Self::Uncompressed => b"none",
      Self::Zlib => b"zlib",
      Self::Bzip2 => b"bzip2",
      Self::Zstd => b"zstd",
    }
  }

  /// The engine a wire name names, matched exactly, or `None` for one this crate does not write.
  pub fn for_wire_name(name: &[u8]) -> Option<Self> {
    ENGINES
      .into_iter()
      .find(|engine| engine.wire_name() == name)
  }

  /// The engine a bundle type names, matched exactly as 7.2 matches it (`supportedbundletypes`, case and all), or
  /// `None` for one this crate does not read.
  pub fn for_bundle_type(name: &[u8]) -> Option<Self> {
    ENGINES
      .into_iter()
      .find(|engine| engine.bundle_type().as_slice() == name)
  }

  /// The levels the engine's compressor takes: zlib 0 to 9, bzip2 1 to 9, zstd from its fastest to its strongest as
  /// the linked libzstd states them (`ZSTD_minCLevel`, `ZSTD_maxCLevel`), and an uncompressed stream 0 alone.
  pub fn levels(self) -> RangeInclusive<i32> {
    match self {
      Self::Uncompressed => UNCOMPRESSED_LEVELS,
      Self::Zlib => ZLIB_LEVELS,
      Self::Bzip2 => BZIP2_LEVELS,
      Self::Zstd => zstd_safe::min_c_level()..=zstd_safe::max_c_level(),
    }
  }
}

/// Why a compressed stream was not inflated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamRefusal {
  /// The stream ended before its engine's end: cut short.
  Truncated {
    /// The engine.
    engine: Engine,
    /// The compressed bytes read: all there were.
    read: u64,
    /// The bytes inflated from them.
    inflated: u64,
  },
  /// Bytes the engine refuses: not its magic or its header, a structure its format does not allow, a checksum that
  /// does not match, a preset dictionary. The reason is the engine's own where it gives one (libzstd's, which names
  /// its own memory refusal so too).
  Corrupt {
    /// The engine.
    engine: Engine,
    /// The compressed bytes read before the step the engine refused.
    read: u64,
    /// What the engine refused.
    reason: &'static str,
  },
  /// Bytes after the engine's stream ended.
  TrailingBytes {
    /// The engine.
    engine: Engine,
    /// How many.
    bytes: u64,
  },
  /// More memory than the admission had left, for the decoder's window or for the inflated bytes: a compression bomb,
  /// refused by kind with nothing past the admission held.
  PastAdmission {
    /// The engine.
    engine: Engine,
    /// The bytes inflated before the refusal: 0 when the window was refused.
    inflated: u64,
    /// The room asked for and the room that was left.
    past: PastAdmission,
  },
  /// The system refused memory the admission had granted.
  OutOfMemory {
    /// The engine.
    engine: Engine,
    /// The bytes asked for.
    asked: u64,
  },
}

/// Why a bundle was not made plain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// Fewer bytes than a bundle's four-byte magic.
  Truncated(Truncated),
  /// A magic that is neither bundle1's `HG10` nor bundle2's `HG20`.
  ForeignMagic {
    /// The bytes found.
    found: [u8; 4],
  },
  /// A bundle1's compression marker cut short, or one this crate does not read.
  Bundle1(bundle1::Refusal),
  /// A bundle2's stream parameters refused, exactly as [`Bundle2::decode`](crate::bundle2::Bundle2::decode) refuses
  /// them.
  Bundle2(bundle2::Refusal),
  /// The plain bundle's header past the admission.
  PastAdmission(PastAdmission),
  /// A plain bundle2's stream parameters longer than their signed 32-bit length holds. They are never longer than the
  /// compressed bundle's, whose length held them, so a bundle that was read never meets this.
  TooLong {
    /// Their length.
    len: u64,
  },
  /// The compressed stream refused.
  Stream(StreamRefusal),
}

/// Why a stream or a bundle was not compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A level outside the engine's [`Engine::levels`].
  Level {
    /// The engine.
    engine: Engine,
    /// The level asked for.
    level: i32,
  },
  /// The engine's compressor refused its call, in its own words where it gives them.
  Engine {
    /// The engine.
    engine: Engine,
    /// What it refused.
    reason: &'static str,
  },
  /// The system refused memory for the compressed bytes.
  OutOfMemory {
    /// The engine.
    engine: Engine,
    /// The bytes asked for.
    asked: u64,
  },
  /// A bundle that is not plain: shorter than its header, a magic neither `HG10` nor `HG20`, a bundle1 whose marker is
  /// not `UN`, or a bundle2 whose `Compression` names an engine.
  NotPlain {
    /// What was found.
    reason: &'static str,
  },
  /// zstd asked of a bundle1, which has no `ZS` type (7.2: "compression engine zstd is not supported on v1 bundles").
  NotForBundle1,
  /// Stream parameters, with `Compression` added, longer than a signed 32-bit length holds.
  TooLong {
    /// Their length.
    len: u64,
  },
}

/// Format: the divisor of the growth zlib-rs bounds a stream by, a byte in eight: its level 1 (`deflate_quick`, as
/// zlib-ng's) writes every literal in fixed codes, nine bits at most for eight (zlib-rs 0.6.8 `deflate_quick_overhead`,
/// zlib-ng's `DEFLATE_QUICK_OVERHEAD`).
const ZLIB_QUICK_DIVISOR: u64 = 8;

/// Format: the bytes zlib-rs's `compress_bound` adds to every stream: a block's header, end and padding bits in whole
/// bytes (`DEFLATE_BLOCK_OVERHEAD`, 3) and the zlib wrapper's header and Adler-32 (`ZLIB_WRAPLEN`, 6).
const ZLIB_BOUND_EXTRA: u64 = 9;

/// Format: the length below which zlib-rs's `compress_bound` adds a byte ("one extra byte for lengths less than 9");
/// it adds one more for an empty input ("always at least one byte for any input").
const ZLIB_SHORT_INPUT: u64 = 9;

/// Format: bzip2's bound, an output "1% larger than the uncompressed data, plus six hundred extra bytes" (the bzip2
/// manual's `BZ2_bzBuffToBuffCompress`): the divisor of that one percent.
const BZIP2_BOUND_DIVISOR: u64 = 100;

/// Format: bzip2's bound's six hundred extra bytes (as [`BZIP2_BOUND_DIVISOR`]).
const BZIP2_BOUND_EXTRA: u64 = 600;

/// The most bytes [`compress`] writes for `length` plain bytes with `engine`, at any of its levels, by each library's own
/// documented bound: zlib-rs's `compress_bound` (zlib-ng's `compressBound`), bzip2's one percent and six hundred bytes,
/// libzstd's `ZSTD_compressBound`, and the length itself uncompressed. A caller charges it to its admission before it
/// compresses, so the compressed bytes are never held past the room. Stock zlib's `compressBound` (`len + (len >> 12) +
/// (len >> 14) + (len >> 25) + 13`) is no bound for the backend the workspace links: it counts stored blocks, which
/// zlib-rs's level 1 never falls back to, so its fixed codes outgrow it on bytes that do not compress (A-650).
pub fn bound(engine: Engine, length: u64) -> u64 {
  match engine {
    Engine::Uncompressed => length,
    Engine::Zlib => length
      .saturating_add(length.div_ceil(ZLIB_QUICK_DIVISOR))
      .saturating_add(ZLIB_BOUND_EXTRA)
      .saturating_add(u64::from(length == 0))
      .saturating_add(u64::from(length < ZLIB_SHORT_INPUT)),
    Engine::Bzip2 => length
      .saturating_add(length.div_ceil(BZIP2_BOUND_DIVISOR))
      .saturating_add(BZIP2_BOUND_EXTRA),
    Engine::Zstd => usize::try_from(length)
      .ok()
      .map(zstd_safe::compress_bound)
      .map_or(u64::MAX, to_u64),
  }
}

/// Inflates `stream`, one whole stream of `engine`, onto the end of `out`. The memory the stream's header sizes for
/// its decoder is charged to `admission` before the decoder is made, and every byte of room for the inflated bytes
/// before it is reserved; a stream cut short, bytes after its end and a stream that needs more room than the admission
/// has are refused by kind.
pub fn inflate(
  engine: Engine,
  stream: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), StreamRefusal> {
  match engine {
    Engine::Uncompressed => copy(stream, admission, out),
    Engine::Zlib => run(&mut Zlib::new(admission)?, stream, admission, out),
    Engine::Bzip2 => run(&mut Bzip2::new(stream, admission)?, stream, admission, out),
    Engine::Zstd => run(&mut Zstd::new(stream, admission)?, stream, admission, out),
  }
}

/// `bundle` as the plain bundle of its kind: an uncompressed bundle as it is, borrowed; a bundle1's changegroup
/// inflated behind `HG10UN`; a bundle2's stream inflated behind its stream parameters, `Compression` dropped from them
/// (7.2's `_forwardchunks`). A bundle2's stream parameters are judged first, exactly as
/// [`Bundle2::decode`](crate::bundle2::Bundle2::decode) judges them, so a bundle refused for them is refused before any
/// of its bytes are inflated.
pub fn plain<'a>(bundle: &'a [u8], admission: &mut Admission) -> Result<Cow<'a, [u8]>, Refusal> {
  let mut cursor = Cursor::new(bundle);
  let magic: [u8; 4] = cursor.array("bundle magic").map_err(Refusal::Truncated)?;
  if magic == bundle1::MAGIC {
    plain_bundle1(bundle, &mut cursor, admission)
  } else if magic == bundle2::MAGIC {
    plain_bundle2(bundle, &mut cursor, admission)
  } else {
    Err(Refusal::ForeignMagic { found: magic })
  }
}

/// Appends `plain`, compressed by `engine` at `level`, to `out`: one zlib stream with its header and Adler-32, one
/// bzip2 stream, one zstd frame that names its content's size, or `plain` itself.
pub fn compress(
  engine: Engine,
  level: i32,
  plain: &[u8],
  out: &mut Vec<u8>,
) -> Result<(), EncodeRefusal> {
  if !engine.levels().contains(&level) {
    return Err(EncodeRefusal::Level { engine, level });
  }
  match engine {
    Engine::Uncompressed => {
      out
        .try_reserve(plain.len())
        .map_err(|_| EncodeRefusal::OutOfMemory {
          engine,
          asked: to_u64(plain.len()),
        })?;
      out.extend_from_slice(plain);
      Ok(())
    }
    Engine::Zlib => compress_zlib(level, plain, out),
    Engine::Bzip2 => compress_bzip2(level, plain, out),
    Engine::Zstd => compress_zstd(level, plain, out),
  }
}

/// Appends `bundle`, a plain bundle, compressed by `engine` at `level` as 7.2 writes it, to `out`: a bundle1 as
/// `HG10GZ` and a zlib stream or `HG10` and a bzip2 stream, whose magic completes the header (`bundletypes`); a bundle2
/// with `Compression=<type>` first among its stream parameters, as `writebundle` sets it before any other, and every
/// byte after them one stream (`bundle20.setcompression`). A `Compression=UN` the bundle2 names is dropped, and
/// `Uncompressed` writes the bundle as it is.
pub fn compress_bundle(
  bundle: &[u8],
  engine: Engine,
  level: i32,
  out: &mut Vec<u8>,
) -> Result<(), EncodeRefusal> {
  if !engine.levels().contains(&level) {
    return Err(EncodeRefusal::Level { engine, level });
  }
  let mut cursor = Cursor::new(bundle);
  let magic: [u8; 4] = cursor
    .array("bundle magic")
    .map_err(|_| not_plain("shorter than a bundle's magic"))?;
  if magic == bundle1::MAGIC {
    compress_bundle1(&mut cursor, engine, level, out)
  } else if magic == bundle2::MAGIC {
    compress_bundle2(&mut cursor, engine, level, out)
  } else {
    Err(not_plain("a magic neither HG10 nor HG20"))
  }
}

/// A bundle1 made plain: `HG10UN` as it is; `HG10GZ`'s zlib stream and `HG10`'s bzip2 stream inflated behind `HG10UN`.
fn plain_bundle1<'a>(
  bundle: &'a [u8],
  cursor: &mut Cursor<'a>,
  admission: &mut Admission,
) -> Result<Cow<'a, [u8]>, Refusal> {
  let marker: [u8; 2] = cursor
    .array("bundle1 compression")
    .map_err(|cut| Refusal::Bundle1(bundle1::Refusal::Truncated(cut)))?;
  if marker == bundle1::UNCOMPRESSED {
    return Ok(Cow::Borrowed(bundle));
  }
  let engine = Engine::for_bundle_type(&marker)
    .filter(|_| bundle1::COMPRESSED.contains(&marker))
    .ok_or(Refusal::Bundle1(bundle1::Refusal::UnknownCompression {
      engine: marker,
    }))?;
  // A bzip2 stream begins with its own magic `BZ`, which is the header's marker (7.2's `bundletypes`: `HG10BZ` writes
  // `HG10` and the stream); a zlib stream begins after the marker.
  let stream = if engine == Engine::Bzip2 {
    bundle.get(bundle1::MAGIC.len()..).unwrap_or_default()
  } else {
    cursor.rest()
  };
  let mut out = header(
    bundle1::MAGIC
      .len()
      .saturating_add(bundle1::UNCOMPRESSED.len()),
    admission,
  )?;
  out.extend_from_slice(&bundle1::MAGIC);
  out.extend_from_slice(&bundle1::UNCOMPRESSED);
  inflate(engine, stream, admission, &mut out).map_err(Refusal::Stream)?;
  Ok(Cow::Owned(out))
}

/// A bundle2 made plain: as it is when it names no engine or `UN`; otherwise its stream parameters but `Compression`,
/// then its stream inflated.
fn plain_bundle2<'a>(
  bundle: &'a [u8],
  cursor: &mut Cursor<'a>,
  admission: &mut Admission,
) -> Result<Cow<'a, [u8]>, Refusal> {
  let block = bundle2::sized(cursor, "stream parameters length", "stream parameters")
    .map_err(Refusal::Bundle2)?;
  let params = bundle2::stream_params(block, admission).map_err(Refusal::Bundle2)?;
  let engine = named_engine(&params)?;
  if engine == Engine::Uncompressed {
    return Ok(Cow::Borrowed(bundle));
  }
  let (bytes, count) = kept(block, &params).fold((0, 0), |(bytes, count): (usize, usize), item| {
    (bytes.saturating_add(item.len()), count.saturating_add(1))
  });
  let len = bytes.saturating_add(count.saturating_sub(1));
  let length = i32::try_from(len).map_err(|_| Refusal::TooLong { len: to_u64(len) })?;
  let mut out = header(
    bundle2::MAGIC
      .len()
      .saturating_add(core::mem::size_of::<i32>())
      .saturating_add(len),
    admission,
  )?;
  out.extend_from_slice(&bundle2::MAGIC);
  out.extend_from_slice(&length.to_be_bytes());
  for (position, item) in kept(block, &params).enumerate() {
    if position > 0 {
      out.push(b' ');
    }
    out.extend_from_slice(item);
  }
  inflate(engine, cursor.rest(), admission, &mut out).map_err(Refusal::Stream)?;
  Ok(Cow::Owned(out))
}

/// The engine a bundle2's `Compression` names, `Uncompressed` when it names none. [`bundle2::stream_params`] has
/// already refused an unknown value and a second `Compression`.
fn named_engine(params: &[StreamParam]) -> Result<Engine, Refusal> {
  let Some(param) = params
    .iter()
    .find(|param| param.name.eq_ignore_ascii_case(bundle2::COMPRESSION))
  else {
    return Ok(Engine::Uncompressed);
  };
  param
    .value
    .as_deref()
    .and_then(Engine::for_bundle_type)
    .ok_or_else(|| {
      Refusal::Bundle2(bundle2::Refusal::UnknownCompression {
        engine: param.value.clone(),
      })
    })
}

/// The stream parameters a plain bundle2 keeps: every one but `Compression`, as it was quoted, in order.
fn kept<'b>(block: &'b [u8], params: &'b [StreamParam]) -> impl Iterator<Item = &'b [u8]> + 'b {
  block
    .split(|byte| *byte == b' ')
    .zip(params)
    .filter(|(_, param)| !param.name.eq_ignore_ascii_case(bundle2::COMPRESSION))
    .map(|(item, _)| item)
}

/// Room for a plain bundle's header of `len` bytes, charged to `admission` before it is reserved; the inflated stream
/// follows it.
fn header(len: usize, admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
  admission
    .charge(to_u64(len))
    .map_err(Refusal::PastAdmission)?;
  Ok(Vec::with_capacity(len))
}

/// Copies an uncompressed stream onto the end of `out`, its length charged first.
fn copy(stream: &[u8], admission: &mut Admission, out: &mut Vec<u8>) -> Result<(), StreamRefusal> {
  let engine = Engine::Uncompressed;
  admission
    .charge(to_u64(stream.len()))
    .map_err(|past| StreamRefusal::PastAdmission {
      engine,
      inflated: 0,
      past,
    })?;
  out
    .try_reserve_exact(stream.len())
    .map_err(|_| StreamRefusal::OutOfMemory {
      engine,
      asked: to_u64(stream.len()),
    })?;
  out.extend_from_slice(stream);
  Ok(())
}

/// What one step of a decoder did.
struct Step {
  /// The compressed bytes it read.
  read: usize,
  /// Whether its stream ended.
  ended: bool,
}

/// Why a decoder's step failed.
enum Fault {
  /// The engine refused the bytes, in these words.
  Corrupt(&'static str),
  /// The engine could not have the memory it holds for the stream, this many bytes.
  Memory(u64),
}

/// One engine's decoder, as [`run`] steps it.
trait Decoder {
  /// The engine.
  fn engine(&self) -> Engine;

  /// Inflates what it can of `input` into the room `out` has, never growing `out`.
  fn step(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Step, Fault>;
}

/// Steps `decoder` over `stream` to its end, making room in `out` within `admission` whenever the decoder can go no
/// further without it.
fn run(
  decoder: &mut impl Decoder,
  stream: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), StreamRefusal> {
  let engine = decoder.engine();
  let start = out.len();
  let mut read: usize = 0;
  loop {
    let before = out.len();
    let rest = stream.get(read..).unwrap_or_default();
    let step = decoder.step(rest, out).map_err(|fault| match fault {
      Fault::Corrupt(reason) => corrupt(engine, read, reason),
      Fault::Memory(asked) => StreamRefusal::OutOfMemory { engine, asked },
    })?;
    read = read.saturating_add(step.read);
    if step.ended {
      return finished(engine, stream.len(), read);
    }
    if step.read == 0 && out.len() == before {
      stalled(engine, stream.len(), read, start, admission, out)?;
    }
  }
}

/// A step that read nothing and wrote nothing. With room left, the decoder wants input: the stream was cut short if
/// it has none left. With no room, it wants room, which is made if the admission has it.
fn stalled(
  engine: Engine,
  stream_len: usize,
  read: usize,
  start: usize,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), StreamRefusal> {
  if out.len() < out.capacity() {
    return Err(if read < stream_len {
      corrupt(engine, read, "the decoder stopped with input and room left")
    } else {
      StreamRefusal::Truncated {
        engine,
        read: to_u64(read),
        inflated: to_u64(out.len().saturating_sub(start)),
      }
    });
  }
  grow(engine, out, start, stream_len, admission)
}

/// Makes room in `out` for more inflated bytes: as many as are inflated so far, the compressed stream's own length at
/// first, never more than the admission has left, every byte charged before it is reserved.
fn grow(
  engine: Engine,
  out: &mut Vec<u8>,
  start: usize,
  stream_len: usize,
  admission: &mut Admission,
) -> Result<(), StreamRefusal> {
  let inflated = out.len().saturating_sub(start);
  let wanted = inflated.max(stream_len).max(1);
  let room = usize::try_from(admission.left()).map_or(wanted, |left| left.min(wanted));
  let past = |left| StreamRefusal::PastAdmission {
    engine,
    inflated: to_u64(inflated),
    past: PastAdmission {
      asked: to_u64(wanted),
      left,
    },
  };
  if room == 0 {
    return Err(past(0));
  }
  admission
    .charge(to_u64(room))
    .map_err(|refused| past(refused.left))?;
  out
    .try_reserve_exact(room)
    .map_err(|_| StreamRefusal::OutOfMemory {
      engine,
      asked: to_u64(room),
    })
}

/// The stream's end: refused if bytes follow it.
fn finished(engine: Engine, stream_len: usize, read: usize) -> Result<(), StreamRefusal> {
  match stream_len.checked_sub(read) {
    Some(0) => Ok(()),
    Some(bytes) => Err(StreamRefusal::TrailingBytes {
      engine,
      bytes: to_u64(bytes),
    }),
    None => Err(corrupt(engine, read, "the decoder read past its input")),
  }
}

/// A [`StreamRefusal::Corrupt`].
fn corrupt(engine: Engine, read: usize, reason: &'static str) -> StreamRefusal {
  StreamRefusal::Corrupt {
    engine,
    read: to_u64(read),
    reason,
  }
}

/// Charges the memory a decoder holds for its stream, before the decoder is made.
fn charge(engine: Engine, held: u64, admission: &mut Admission) -> Result<(), StreamRefusal> {
  admission
    .charge(held)
    .map_err(|past| StreamRefusal::PastAdmission {
      engine,
      inflated: 0,
      past,
    })
}

/// The bytes a library counter moved between two readings.
fn consumed(before: u64, after: u64) -> usize {
  usize::try_from(after.saturating_sub(before)).unwrap_or(usize::MAX)
}

/// The refusal of a stream whose header is cut short.
fn header_cut(engine: Engine, stream: &[u8]) -> StreamRefusal {
  StreamRefusal::Truncated {
    engine,
    read: to_u64(stream.len()),
    inflated: 0,
  }
}

/// zlib's decoder, through flate2 (RFC 1950).
struct Zlib(ZlibDecompress);

impl Zlib {
  /// A decoder, its window charged first.
  fn new(admission: &mut Admission) -> Result<Self, StreamRefusal> {
    charge(Engine::Zlib, ZLIB_WINDOW, admission)?;
    Ok(Self(ZlibDecompress::new(true)))
  }
}

impl Decoder for Zlib {
  fn engine(&self) -> Engine {
    Engine::Zlib
  }

  fn step(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Step, Fault> {
    let before = self.0.total_in();
    let status = self.0.decompress_vec(input, out, FlushDecompress::None);
    let read = consumed(before, self.0.total_in());
    match status {
      Ok(ZlibStatus::StreamEnd) => Ok(Step { read, ended: true }),
      Ok(ZlibStatus::Ok | ZlibStatus::BufError) => Ok(Step { read, ended: false }),
      Err(error) if error.needs_dictionary().is_some() => Err(Fault::Corrupt(
        "a preset dictionary, which Mercurial never writes (RFC 1950 FDICT)",
      )),
      Err(_) => Err(Fault::Corrupt(
        "not zlib's format: its header, a DEFLATE block or its Adler-32 (RFC 1950, RFC 1951)",
      )),
    }
  }
}

/// bzip2's decoder, through the `bzip2` crate on libbz2-rs-sys, in its fast mode as 7.2's Python runs it.
struct Bzip2 {
  /// The decoder.
  decoder: Bzip2Decompress,
  /// The memory it holds for the stream's block, charged.
  held: u64,
}

impl Bzip2 {
  /// A decoder for `stream`, the memory its header sizes charged first.
  fn new(stream: &[u8], admission: &mut Admission) -> Result<Self, StreamRefusal> {
    let held = bzip2_held(stream)?;
    charge(Engine::Bzip2, held, admission)?;
    Ok(Self {
      decoder: Bzip2Decompress::new(false),
      held,
    })
  }
}

/// The memory the fast bzip2 decoder holds for `stream`: one 32-bit entry for each byte of the block size its header
/// names (`tt`, libbzip2's and libbz2-rs-sys's `BZ2_decompress`; the bzip2 manual's "4 × block size").
fn bzip2_held(stream: &[u8]) -> Result<u64, StreamRefusal> {
  let engine = Engine::Bzip2;
  let mut cursor = Cursor::new(stream);
  let magic: [u8; 3] = cursor
    .array("bzip2 magic")
    .map_err(|_| header_cut(engine, stream))?;
  let digit = cursor
    .u8("bzip2 block size")
    .map_err(|_| header_cut(engine, stream))?;
  let blocks = digit
    .checked_sub(b'0')
    .map(i32::from)
    .filter(|blocks| BZIP2_LEVELS.contains(blocks) && magic == BZIP2_MAGIC)
    .and_then(|blocks| u64::try_from(blocks).ok())
    .ok_or_else(|| {
      corrupt(
        engine,
        0,
        "not a bzip2 stream: its magic or its block size (bzip2 manual §2.4)",
      )
    })?;
  Ok(
    blocks
      .saturating_mul(BZIP2_BLOCK_UNIT)
      .saturating_mul(to_u64(core::mem::size_of::<u32>())),
  )
}

impl Decoder for Bzip2 {
  fn engine(&self) -> Engine {
    Engine::Bzip2
  }

  fn step(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Step, Fault> {
    let before = self.decoder.total_in();
    let status = self.decoder.decompress_vec(input, out);
    let read = consumed(before, self.decoder.total_in());
    match status {
      Ok(Bzip2Status::StreamEnd) => Ok(Step { read, ended: true }),
      Ok(Bzip2Status::MemNeeded) => Err(Fault::Memory(self.held)),
      Ok(_) => Ok(Step { read, ended: false }),
      Err(Bzip2Error::DataMagic) => Err(Fault::Corrupt("not a bzip2 stream: its magic")),
      Err(Bzip2Error::Data) => Err(Fault::Corrupt(
        "a bzip2 block's data, or a block's or the stream's CRC, is wrong",
      )),
      Err(_) => Err(Fault::Corrupt("the bzip2 decoder refused its call")),
    }
  }
}

/// A zstd frame's header: what sizes its decoder's memory (RFC 8878 §3.1.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZstdFrame {
  /// Window_Size: a single-segment frame's content size, otherwise its Window_Descriptor's.
  window: u64,
  /// Frame_Content_Size, where the header carries it.
  content_size: Option<u64>,
}

impl ZstdFrame {
  /// Reads the frame header at the front of `stream`: its magic, its descriptor (a reserved bit refused), its window,
  /// its dictionary id and its content size.
  fn read(stream: &[u8]) -> Result<Self, StreamRefusal> {
    let engine = Engine::Zstd;
    let cut = |_| header_cut(engine, stream);
    let mut cursor = Cursor::new(stream);
    let magic = u32::from_le_bytes(cursor.array("zstd magic").map_err(cut)?);
    let descriptor = cursor.u8("zstd frame header descriptor").map_err(cut)?;
    if magic != ZSTD_MAGIC || descriptor & ZSTD_RESERVED != 0 {
      return Err(corrupt(
        engine,
        0,
        "not a zstd frame: its magic number or a reserved bit (RFC 8878 §3.1.1)",
      ));
    }
    let single_segment = descriptor & ZSTD_SINGLE_SEGMENT != 0;
    let window_descriptor = if single_segment {
      None
    } else {
      Some(cursor.u8("zstd window descriptor").map_err(cut)?)
    };
    let dictionary = ZSTD_DICTIONARY_ID_LEN
      .get(usize::from(descriptor & ZSTD_DICTIONARY_FLAG))
      .copied()
      .unwrap_or_default();
    cursor.take(dictionary, "zstd dictionary id").map_err(cut)?;
    let content_size = content_size(&mut cursor, descriptor, single_segment).map_err(cut)?;
    let window = match (window_descriptor, content_size) {
      (Some(descriptor), _) => window_size(descriptor),
      (None, Some(size)) => size,
      (None, None) => 0,
    };
    Ok(Self {
      window,
      content_size,
    })
  }

  /// The bytes libzstd 1.5.7's streaming decoder holds for the frame beside its own context: an input buffer of one
  /// block, at least a checksum's four bytes, and an output buffer of the window, at least libzstd's smallest, and two
  /// blocks with their slack, never more than the frame's content (`ZSTD_decompressStream`,
  /// `ZSTD_decodingBufferSize_internal`).
  fn held(&self) -> u64 {
    let block = self.window.min(ZSTD_BLOCK_MAX);
    let window = self.window.max(smallest_window());
    let input = block.max(ZSTD_CHECKSUM_LEN);
    let margins = block
      .saturating_add(ZSTD_WILDCOPY_OVERLENGTH)
      .saturating_mul(2);
    let output = window.saturating_add(margins);
    let output = self.content_size.map_or(output, |size| output.min(size));
    input.saturating_add(output)
  }

  /// The smallest window log whose window holds this frame's, libzstd's smallest at least: the most its decoder is
  /// let accept, so it never holds more than was charged for the frame read here.
  fn window_log(&self) -> u32 {
    self
      .window
      .checked_next_power_of_two()
      .map_or(u64::BITS, u64::trailing_zeros)
      .max(ZSTD_WINDOW_LOG_MIN)
  }
}

/// libzstd's smallest window, 1 KiB: a smaller one is decoded as this one (`ZSTD_WINDOWLOG_ABSOLUTEMIN`).
fn smallest_window() -> u64 {
  1_u64.checked_shl(ZSTD_WINDOW_LOG_MIN).unwrap_or(u64::MAX)
}

/// Window_Size from a Window_Descriptor: windowBase = 1 << (10 + Exponent), and Mantissa eighths of it more (RFC 8878
/// §3.1.1.1.2).
fn window_size(descriptor: u8) -> u64 {
  let exponent = descriptor
    .checked_shr(ZSTD_EXPONENT_SHIFT)
    .unwrap_or_default();
  let mantissa = descriptor & ZSTD_MANTISSA;
  let log = ZSTD_WINDOW_LOG_MIN.saturating_add(u32::from(exponent));
  let base = 1_u64.checked_shl(log).unwrap_or(u64::MAX);
  let eighth = base.checked_shr(ZSTD_EIGHTH_SHIFT).unwrap_or_default();
  base.saturating_add(eighth.saturating_mul(u64::from(mantissa)))
}

/// Frame_Content_Size, little-endian in the field its flag sizes: one byte in a single-segment frame whose flag is 0,
/// none in another, and 256 more than the field holds in two bytes (RFC 8878 §3.1.1.1.4).
fn content_size(
  cursor: &mut Cursor<'_>,
  descriptor: u8,
  single_segment: bool,
) -> Result<Option<u64>, Truncated> {
  let flag = usize::from(
    descriptor
      .checked_shr(ZSTD_CONTENT_SIZE_SHIFT)
      .unwrap_or_default(),
  );
  let len = ZSTD_CONTENT_SIZE_LEN.get(flag).copied().unwrap_or_default();
  let len = if len == 0 && single_segment { 1 } else { len };
  if len == 0 {
    return Ok(None);
  }
  let field = cursor.take(len, "zstd frame content size")?;
  let mut bytes = [0_u8; core::mem::size_of::<u64>()];
  for (slot, byte) in bytes.iter_mut().zip(field) {
    *slot = *byte;
  }
  let offset = if flag == ZSTD_CONTENT_SIZE_OFFSET_FLAG {
    ZSTD_CONTENT_SIZE_OFFSET
  } else {
    0
  };
  Ok(Some(u64::from_le_bytes(bytes).saturating_add(offset)))
}

/// zstd's decoder, through libzstd's streaming context.
struct Zstd(DCtx<'static>);

impl Zstd {
  /// A decoder for `stream`, the buffers its frame header sizes charged first and the window it may accept held to
  /// that frame's.
  fn new(stream: &[u8], admission: &mut Admission) -> Result<Self, StreamRefusal> {
    let engine = Engine::Zstd;
    let frame = ZstdFrame::read(stream)?;
    let held = frame.held();
    charge(engine, held, admission)?;
    let mut context = DCtx::try_create().ok_or(StreamRefusal::OutOfMemory {
      engine,
      asked: held,
    })?;
    context
      .set_parameter(DParameter::WindowLogMax(frame.window_log()))
      .map_err(|code| corrupt(engine, 0, zstd_safe::get_error_name(code)))?;
    Ok(Self(context))
  }
}

impl Decoder for Zstd {
  fn engine(&self) -> Engine {
    Engine::Zstd
  }

  fn step(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Step, Fault> {
    let mut source = InBuffer::around(input);
    let at = out.len();
    let result = {
      let mut sink = OutBuffer::around_pos(&mut *out, at);
      self.0.decompress_stream(&mut sink, &mut source)
    };
    let read = source.pos();
    match result {
      Ok(0) => Ok(Step { read, ended: true }),
      Ok(_) => Ok(Step { read, ended: false }),
      Err(code) => Err(Fault::Corrupt(zstd_safe::get_error_name(code))),
    }
  }
}

/// Makes room in `out` once it is full, by the vector's own amortized growth, which doubles what it holds: the
/// compressed bytes are the server's own, bounded by its input and the engine's worst case over it.
fn make_room(out: &mut Vec<u8>, engine: Engine) -> Result<(), EncodeRefusal> {
  if out.len() < out.capacity() {
    return Ok(());
  }
  out.try_reserve(1).map_err(|_| EncodeRefusal::OutOfMemory {
    engine,
    asked: to_u64(out.capacity()),
  })
}

/// A compressor that made no progress with input or room left.
fn stuck(engine: Engine) -> EncodeRefusal {
  EncodeRefusal::Engine {
    engine,
    reason: "the compressor stopped with input or room left",
  }
}

/// One zlib stream, through flate2: the input fed whole, then finished, so no backend's limit on one call's input
/// ends the stream early.
fn compress_zlib(level: i32, plain: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  let engine = Engine::Zlib;
  let level = u32::try_from(level).map_err(|_| EncodeRefusal::Level { engine, level })?;
  let mut encoder = ZlibCompress::new(ZlibLevel::new(level), true);
  let mut read: usize = 0;
  loop {
    let rest = plain.get(read..).unwrap_or_default();
    let flush = if rest.is_empty() {
      FlushCompress::Finish
    } else {
      FlushCompress::None
    };
    make_room(out, engine)?;
    let (before, written) = (encoder.total_in(), out.len());
    let status = encoder
      .compress_vec(rest, out, flush)
      .map_err(|_| EncodeRefusal::Engine {
        engine,
        reason: "the zlib compressor refused its call",
      })?;
    let step = consumed(before, encoder.total_in());
    read = read.saturating_add(step);
    if status == ZlibStatus::StreamEnd {
      return Ok(());
    }
    if step == 0 && out.len() == written && out.len() < out.capacity() {
      return Err(stuck(engine));
    }
  }
}

/// One bzip2 stream, through the `bzip2` crate at libbzip2's default work factor (0 names it, as 7.2's Python's
/// `BZ2Compressor` does): the input fed whole, then finished.
fn compress_bzip2(level: i32, plain: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  let engine = Engine::Bzip2;
  let level = u32::try_from(level)
    .ok()
    .and_then(Bzip2Level::try_new)
    .ok_or(EncodeRefusal::Level { engine, level })?;
  let mut encoder = Bzip2Compress::new(level, 0);
  let mut read: usize = 0;
  loop {
    let rest = plain.get(read..).unwrap_or_default();
    let action = if rest.is_empty() {
      Bzip2Action::Finish
    } else {
      Bzip2Action::Run
    };
    make_room(out, engine)?;
    let (before, written) = (encoder.total_in(), out.len());
    let status = encoder
      .compress_vec(rest, out, action)
      .map_err(|_| EncodeRefusal::Engine {
        engine,
        reason: "the bzip2 compressor refused its call",
      })?;
    let step = consumed(before, encoder.total_in());
    read = read.saturating_add(step);
    if status == Bzip2Status::StreamEnd {
      return Ok(());
    }
    if step == 0 && out.len() == written && out.len() < out.capacity() {
      return Err(stuck(engine));
    }
  }
}

/// One zstd frame, through libzstd, its content's size pledged so the frame names it and libzstd sizes the window to
/// it, ended (`ZSTD_e_end`) as one frame whatever the room, since 7.2's reader reads one.
fn compress_zstd(level: i32, plain: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  let engine = Engine::Zstd;
  let refused = |code| EncodeRefusal::Engine {
    engine,
    reason: zstd_safe::get_error_name(code),
  };
  let mut context = CCtx::try_create().ok_or(EncodeRefusal::OutOfMemory { engine, asked: 0 })?;
  context
    .set_parameter(CParameter::CompressionLevel(level))
    .map_err(refused)?;
  context
    .set_pledged_src_size(Some(to_u64(plain.len())))
    .map_err(refused)?;
  let mut source = InBuffer::around(plain);
  loop {
    make_room(out, engine)?;
    let (before, written) = (source.pos(), out.len());
    let pending = {
      let mut sink = OutBuffer::around_pos(&mut *out, written);
      context
        .compress_stream2(&mut sink, &mut source, ZSTD_EndDirective::ZSTD_e_end)
        .map_err(refused)?
    };
    if pending == 0 {
      return Ok(());
    }
    if source.pos() == before && out.len() == written && out.len() < out.capacity() {
      return Err(stuck(engine));
    }
  }
}

/// The refusal of a bundle that is not plain.
fn not_plain(reason: &'static str) -> EncodeRefusal {
  EncodeRefusal::NotPlain { reason }
}

/// A plain bundle1 compressed: `HG10GZ` and a zlib stream, `HG10` and a bzip2 stream, or `HG10UN` as it was.
fn compress_bundle1(
  cursor: &mut Cursor<'_>,
  engine: Engine,
  level: i32,
  out: &mut Vec<u8>,
) -> Result<(), EncodeRefusal> {
  let marker: [u8; 2] = cursor
    .array("bundle1 compression")
    .map_err(|_| not_plain("a bundle1 header cut short"))?;
  if marker != bundle1::UNCOMPRESSED {
    return Err(not_plain("a bundle1 already compressed"));
  }
  let changegroup = cursor.rest();
  match engine {
    Engine::Zstd => Err(EncodeRefusal::NotForBundle1),
    // The bzip2 stream's own magic `BZ` completes the header (7.2's `bundletypes`: `HG10BZ` writes `HG10`).
    Engine::Bzip2 => {
      out.extend_from_slice(&bundle1::MAGIC);
      compress(engine, level, changegroup, out)
    }
    Engine::Zlib | Engine::Uncompressed => {
      out.extend_from_slice(&bundle1::MAGIC);
      out.extend_from_slice(&engine.bundle_type());
      compress(engine, level, changegroup, out)
    }
  }
}

/// A plain bundle2 compressed: `Compression=<type>` first among its stream parameters, a `Compression=UN` dropped,
/// the others as they were; then every byte after them one stream of the engine.
fn compress_bundle2(
  cursor: &mut Cursor<'_>,
  engine: Engine,
  level: i32,
  out: &mut Vec<u8>,
) -> Result<(), EncodeRefusal> {
  let value = cursor
    .i32("stream parameters length")
    .map_err(|_| not_plain("a bundle2 header cut short"))?;
  let len = usize::try_from(value).map_err(|_| not_plain("a negative stream parameters length"))?;
  let block = cursor
    .take(len, "stream parameters")
    .map_err(|_| not_plain("stream parameters cut short"))?;
  let named = [
    COMPRESSION_PARAMETER,
    b"=".as_slice(),
    engine.bundle_type().as_slice(),
  ]
  .concat();
  let mut items: Vec<&[u8]> = Vec::new();
  if engine != Engine::Uncompressed {
    items.push(&named);
  }
  if !block.is_empty() {
    for item in block.split(|byte| *byte == b' ') {
      if !names_compression(item) {
        items.push(item);
      } else if !value_is(item, &Engine::Uncompressed.bundle_type()) {
        return Err(not_plain("a bundle2 whose Compression is not UN"));
      }
    }
  }
  let joined = items.join(&b' ');
  let length = i32::try_from(joined.len()).map_err(|_| EncodeRefusal::TooLong {
    len: to_u64(joined.len()),
  })?;
  out.extend_from_slice(&bundle2::MAGIC);
  out.extend_from_slice(&length.to_be_bytes());
  out.extend_from_slice(&joined);
  compress(engine, level, cursor.rest(), out)
}

/// Whether a quoted stream parameter's name, unquoted, is `Compression` in any case (7.2's `name.lower()`).
fn names_compression(item: &[u8]) -> bool {
  let name = item.split(|byte| *byte == b'=').next().unwrap_or_default();
  let mut matches = Matches::new(bundle2::COMPRESSION, true);
  quote::unquote(name, &mut matches);
  matches.matched()
}

/// Whether a quoted stream parameter's value, unquoted, is exactly `expected`; a parameter with no value is not.
fn value_is(item: &[u8], expected: &[u8]) -> bool {
  let Some(value) = item.splitn(2, |byte| *byte == b'=').nth(1) else {
    return false;
  };
  let mut matches = Matches::new(expected, false);
  quote::unquote(value, &mut matches);
  matches.matched()
}

/// An [`Out`] that compares what is written to it with `expected`, allocating nothing.
struct Matches<'e> {
  /// The bytes expected.
  expected: &'e [u8],
  /// How many bytes were written.
  at: usize,
  /// Whether every byte so far matched.
  equal: bool,
  /// Whether ASCII case is ignored.
  ignore_case: bool,
}

impl<'e> Matches<'e> {
  /// A comparison with `expected`.
  fn new(expected: &'e [u8], ignore_case: bool) -> Self {
    Self {
      expected,
      at: 0,
      equal: true,
      ignore_case,
    }
  }

  /// Whether exactly `expected` was written.
  fn matched(&self) -> bool {
    self.equal && self.at == self.expected.len()
  }
}

impl Out for Matches<'_> {
  fn put(&mut self, bytes: &[u8]) {
    for byte in bytes {
      let same = self.expected.get(self.at).is_some_and(|wanted| {
        if self.ignore_case {
          wanted.eq_ignore_ascii_case(byte)
        } else {
          wanted == byte
        }
      });
      self.equal = self.equal && same;
      self.at = self.at.saturating_add(1);
    }
  }
}

#[cfg(test)]
#[path = "tests/compression.rs"]
mod tests;
