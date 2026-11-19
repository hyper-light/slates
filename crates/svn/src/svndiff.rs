//! svndiff, Subversion's delta format, in its versions 0 and 2 (§4.17.4 "5. The wire", its svndiff item; A-500).
//!
//! A delta is a four-byte header, `SVN` and a version byte, then windows. Each window makes a stretch of the target, at
//! most [`WINDOW_LIMIT`] bytes, from three places: a view of the source (at most [`WINDOW_LIMIT`] bytes, never sliding
//! back from the previous window's view), the target the window has made so far, and the window's own new data. Version
//! 2 begins each of a window's two sections, its instructions and its new data, with the section's original length, and
//! stores the rest as an LZ4 block when that is shorter (`notes/svndiff`, Subversion's `1.14.x` branch at `d434dc14`;
//! `docs/wip/research/vcs-servers-subversion.md` §4).
//!
//! Why versions 0 and 2 and not 1: version 1 is zlib, and a Subversion peer sends it only to a peer that announced or
//! advertised it (`libsvn_ra_svn/marshal.c`, `svn_ra_svn__svndiff_version`; `mod_dav_svn/repos.c`,
//! `negotiate_encoding_prefs`). slates never does, so it never receives zlib, and a version-1 delta is refused
//! ([`SvndiffError::VersionNotOffered`]).
//!
//! What this module holds to:
//! - The decoder refuses everything Subversion's decoder refuses (`libsvn_delta/svndiff.c`), each refusal typed and
//!   carrying the number of Subversion's own error ([`SvndiffError::subversion_code`]), and one thing more: an integer
//!   encoded past 64 bits, which Subversion's `svn__decode_uint` truncates without a word.
//! - The encoder refuses any window the decoder would refuse, and writes every integer in its shortest form and every
//!   length under 64 inside its instruction's first byte, as Subversion's encoder does; so a window decoded and encoded
//!   again is the same bytes.
//! - The decoder keeps no bytes, only its state. The caller holds the input and drops what [`Decoded`] says was
//!   consumed, and [`Decoded::NeedMore`] says how much the next part needs, already held to the format's limits, so the
//!   caller's buffer is bounded by the format, never by the peer.
//! - [`apply`] is the reference: it makes the target byte by byte, a target copy that runs past the position repeating
//!   what it copied, as `notes/svndiff` defines.
//!
//! No I/O, no clock, no randomness, no panic.

use std::borrow::Cow;

/// The first three bytes of every svndiff delta.
/// Format: `notes/svndiff` 21-22: "four bytes, "SVN" followed by a byte which represents a format version number".
const MAGIC: [u8; 3] = *b"SVN";

/// A delta's header length: the magic and the version byte.
/// Format: `svndiff.c` `SVNDIFF_HEADER_SIZE`, the size of `SVNDIFF_V0`.
pub const HEADER_LEN: usize = 4;

/// The largest target view and the largest source view a window may name, and the most new data it may use.
/// Format: `SVN_DELTA_WINDOW_SIZE` (`libsvn_delta/delta.h` 45); every stock decoder refuses a window naming more
/// (`svndiff.c` 714-720), so it bounds what slates accepts and what it may send.
pub const WINDOW_LIMIT: u64 = 102_400;

/// The most bytes one encoded integer may take.
/// Format: `SVN__MAX_ENCODED_UINT_LEN` (`svn_subr_private.h` 492): ten bytes of seven bits hold every 64-bit value.
pub const INTEGER_LIMIT: usize = 10;

/// The largest instruction section a window may carry, encoded or decoded.
/// Format: `svndiff.c` `MAX_INSTRUCTION_SECTION_LEN`, `SVN_DELTA_WINDOW_SIZE` times `MAX_INSTRUCTION_LEN`, which is
/// two of the longest integers and the instruction's first byte: 102,400 × 21.
pub const INSTRUCTION_SECTION_LIMIT: u64 = 2_150_400;

/// The largest new-data length a window header may state, in every version.
/// Format: `svndiff.c` 714-720 (`newlen > SVN_DELTA_WINDOW_SIZE + SVN__MAX_ENCODED_UINT_LEN`): the window limit and
/// the original-length integer a version-2 section begins with.
pub const NEW_DATA_SECTION_LIMIT: u64 = 102_410;

/// The largest offset a window's source view may start at.
/// Format: Subversion's file offsets are signed 64-bit (`svn_filesize_t`), and its decoder refuses a negative one
/// (`svndiff.c` 722-728, `sview_offset < 0`).
const SOURCE_OFFSET_LIMIT: u64 = i64::MAX.unsigned_abs();

/// The bit that says another byte of the integer follows.
/// Format: `notes/svndiff` 45-48: "The high bit of each byte is used as a continuation bit".
const CONTINUATION: u8 = 0x80;

/// The seven data bits of each byte of an integer.
/// Format: `notes/svndiff` 45-48: "The other seven bits of each byte are data".
const DATA_BITS: u8 = 0x7f;

/// The value of one integer byte's place.
/// Format: seven data bits a byte, higher-order bits first (`notes/svndiff` 49).
const DIGIT_BASE: u64 = 128;

/// The bits a byte of an integer carries.
/// Format: seven data bits a byte (`notes/svndiff` 45-48).
const BITS_PER_DIGIT: u32 = 7;

/// The two high bits of an instruction's first byte, which select its action.
/// Format: `notes/svndiff` 54-60: 00 source, 01 target, 10 new data, 11 invalid.
const SELECTOR_MASK: u8 = 0xc0;

/// The selector of a copy from the source view.
/// Format: `notes/svndiff` 56, `00`.
const SOURCE_SELECTOR: u8 = 0x00;

/// The selector of a copy from the target view.
/// Format: `notes/svndiff` 57, `01`.
const TARGET_SELECTOR: u8 = 0x40;

/// The selector of a copy from the new data.
/// Format: `notes/svndiff` 58, `10`.
const NEW_SELECTOR: u8 = 0x80;

/// The six low bits of an instruction's first byte, its length when nonzero.
/// Format: `notes/svndiff` 62-64: "If those six bits are all zero, then the length is encoded as an integer".
const SHORT_LENGTH_MASK: u8 = 0x3f;

/// A delta's format version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
  /// Version 0: each section as it is.
  Zero,
  /// Version 2: each section begins with its original length and is an LZ4 block when that is shorter.
  Two,
}

impl Version {
  /// The version's header byte.
  fn byte(self) -> u8 {
    match self {
      Self::Zero => 0,
      Self::Two => 2,
    }
  }
}

/// How the encoder writes a version-2 window's sections (version 0 has one form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
  /// Each section as it is, after its original length.
  Stored,
  /// Each section as an LZ4 block when that is shorter than the section, else as it is, as Subversion's own encoder
  /// decides (`compress_lz4.c`: "Compression didn't help").
  Lz4,
}

/// One instruction of a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instruction {
  /// Copies `length` bytes of the window's source view, from `offset` in the view.
  Source {
    /// Where the copy starts in the source view.
    offset: u64,
    /// How many bytes it copies.
    length: u64,
  },
  /// Copies `length` bytes of the target the window has made, from `offset`, which lies before the position written
  /// next; a copy that runs past that position repeats what it copied.
  Target {
    /// Where the copy starts in the window's target.
    offset: u64,
    /// How many bytes it copies.
    length: u64,
  },
  /// Copies the next `length` bytes of the window's new data.
  New {
    /// How many bytes it copies.
    length: u64,
  },
}

impl Instruction {
  /// How many target bytes the instruction makes.
  fn length(self) -> u64 {
    match self {
      Self::Source { length, .. } | Self::Target { length, .. } | Self::New { length } => length,
    }
  }
}

/// One window: how to make `target_len` bytes of the target from the source's bytes in the view
/// `[source_offset, source_offset + source_len)`, the target the window makes, and its new data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
  /// Where the window's source view starts in the source.
  pub source_offset: u64,
  /// How long the source view is.
  pub source_len: u64,
  /// How many target bytes the window makes.
  pub target_len: u64,
  /// The instructions, in order.
  pub instructions: Vec<Instruction>,
  /// The bytes the `New` instructions copy, in order.
  pub new_data: Vec<u8>,
}

/// Why an instruction was refused (Subversion's `count_and_verify_instructions`, `svndiff.c` 486-543).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionFault {
  /// It cannot be read: its selector is `11`, or its section ends inside it, or an integer in it is too long.
  Undecodable,
  /// Its length is zero.
  ZeroLength,
  /// It makes more bytes than are left of the target view.
  PastTarget,
  /// It copies from beyond the source view.
  PastSource,
  /// It copies from the target at or after the position written next.
  TargetNotBehind,
  /// It copies more new data than is left.
  PastNewData,
}

/// Why a delta, a window or an application was refused. Each carries the number of the Subversion error a client is
/// answered with ([`SvndiffError::subversion_code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvndiffError {
  /// The delta does not begin with `SVN` and a version byte of 0, 1 or 2 (`SVN_ERR_SVNDIFF_INVALID_HEADER`).
  InvalidHeader,
  /// Version 1, zlib, which slates never offers a peer and so never receives (module documentation).
  VersionNotOffered,
  /// A window names a view or a section past the format's limits (`SVN_ERR_SVNDIFF_CORRUPT_WINDOW`, "too-large").
  WindowTooLarge,
  /// A window header's integer is too long or past 64 bits, its source offset past Subversion's signed offsets, or its
  /// sums overflow (`SVN_ERR_SVNDIFF_CORRUPT_WINDOW`).
  CorruptWindowHeader,
  /// A window's source view starts before, or ends before, the previous window's (`SVN_ERR_SVNDIFF_BACKWARD_VIEW`).
  BackwardSourceView,
  /// An instruction was refused; `index` counts the window's instructions from zero (`SVN_ERR_SVNDIFF_INVALID_OPS`).
  InvalidInstruction {
    /// The refused instruction's place in its window.
    index: usize,
    /// Why it was refused.
    fault: InstructionFault,
  },
  /// The instructions do not make exactly the target view's length (`SVN_ERR_SVNDIFF_INVALID_OPS`).
  TargetNotFilled,
  /// The instructions leave new data unused (`SVN_ERR_SVNDIFF_INVALID_OPS`).
  NewDataUnused,
  /// The delta ended inside its header or a window (`SVN_ERR_SVNDIFF_UNEXPECTED_END`).
  UnexpectedEnd,
  /// A version-2 section's original length is missing, past its limit, or not what decompression made
  /// (`SVN_ERR_SVNDIFF_INVALID_COMPRESSED_DATA`).
  InvalidCompressedData,
  /// A version-2 section's LZ4 block does not decompress (`SVN_ERR_LZ4_DECOMPRESSION_FAILED`).
  Lz4,
  /// The source given to [`apply`] is shorter than the window's source view (`SVN_ERR_INCOMPLETE_DATA`, "Delta source
  /// ended unexpectedly", `text_delta.c` 785).
  SourceShort,
}

/// `SVN_ERR_SVNDIFF_INVALID_HEADER`, the svndiff category's first error.
/// Format: a category starts at 120,000 plus its index times 5,000, the svndiff category's index being 13, and each
/// error names its offset from its category's start (`svn_error_codes.h`; research notes §6).
const SVN_ERR_SVNDIFF_INVALID_HEADER: u32 = 185_000;
/// `SVN_ERR_SVNDIFF_CORRUPT_WINDOW`.
/// Format: the svndiff category's start plus 1 (`svn_error_codes.h`).
const SVN_ERR_SVNDIFF_CORRUPT_WINDOW: u32 = 185_001;
/// `SVN_ERR_SVNDIFF_BACKWARD_VIEW`.
/// Format: the svndiff category's start plus 2 (`svn_error_codes.h`).
const SVN_ERR_SVNDIFF_BACKWARD_VIEW: u32 = 185_002;
/// `SVN_ERR_SVNDIFF_INVALID_OPS`.
/// Format: the svndiff category's start plus 3 (`svn_error_codes.h`).
const SVN_ERR_SVNDIFF_INVALID_OPS: u32 = 185_003;
/// `SVN_ERR_SVNDIFF_UNEXPECTED_END`.
/// Format: the svndiff category's start plus 4 (`svn_error_codes.h`).
const SVN_ERR_SVNDIFF_UNEXPECTED_END: u32 = 185_004;
/// `SVN_ERR_SVNDIFF_INVALID_COMPRESSED_DATA`.
/// Format: the svndiff category's start plus 5 (`svn_error_codes.h`).
const SVN_ERR_SVNDIFF_INVALID_COMPRESSED_DATA: u32 = 185_005;
/// `SVN_ERR_INCOMPLETE_DATA`, "Delta source ended unexpectedly" (`text_delta.c` 785).
/// Format: the miscellaneous category's start, 200,000, plus 3 (`svn_error_codes.h`).
const SVN_ERR_INCOMPLETE_DATA: u32 = 200_003;
/// `SVN_ERR_LZ4_DECOMPRESSION_FAILED`.
/// Format: the miscellaneous category's start, 200,000, plus 46 (`svn_error_codes.h`).
const SVN_ERR_LZ4_DECOMPRESSION_FAILED: u32 = 200_046;

impl SvndiffError {
  /// The number of the Subversion error this refusal is answered with, which `svn` prints after an `E`.
  pub fn subversion_code(self) -> u32 {
    match self {
      Self::InvalidHeader | Self::VersionNotOffered => SVN_ERR_SVNDIFF_INVALID_HEADER,
      Self::WindowTooLarge | Self::CorruptWindowHeader => SVN_ERR_SVNDIFF_CORRUPT_WINDOW,
      Self::BackwardSourceView => SVN_ERR_SVNDIFF_BACKWARD_VIEW,
      Self::InvalidInstruction { .. } | Self::TargetNotFilled | Self::NewDataUnused => SVN_ERR_SVNDIFF_INVALID_OPS,
      Self::UnexpectedEnd => SVN_ERR_SVNDIFF_UNEXPECTED_END,
      Self::InvalidCompressedData => SVN_ERR_SVNDIFF_INVALID_COMPRESSED_DATA,
      Self::SourceShort => SVN_ERR_INCOMPLETE_DATA,
      Self::Lz4 => SVN_ERR_LZ4_DECOMPRESSION_FAILED,
    }
  }
}

/// Why an integer could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntegerFault {
  /// Its first [`INTEGER_LIMIT`] bytes all say another byte follows.
  TooLong,
  /// Its value is past 64 bits.
  Overflow,
}

/// Reads the integer at the start of `bytes`: its value and how many bytes it took, or `None` when `bytes` ends before
/// its last byte.
fn read_integer(bytes: &[u8]) -> Result<Option<(u64, usize)>, IntegerFault> {
  let mut value: u64 = 0;
  for (index, &byte) in bytes.iter().take(INTEGER_LIMIT).enumerate() {
    value = value
      .checked_mul(DIGIT_BASE)
      .and_then(|shifted| shifted.checked_add(u64::from(byte & DATA_BITS)))
      .ok_or(IntegerFault::Overflow)?;
    if byte & CONTINUATION == 0 {
      return Ok(Some((value, index.saturating_add(1))));
    }
  }
  if bytes.len() >= INTEGER_LIMIT {
    Err(IntegerFault::TooLong)
  } else {
    Ok(None)
  }
}

/// Appends `value` in its shortest encoding: higher-order digits first, each but the last with its continuation bit.
fn write_integer(value: u64, out: &mut Vec<u8>) {
  let mut digits = [0u8; INTEGER_LIMIT];
  let mut rest = value;
  let mut used = 0usize;
  for slot in digits.iter_mut().rev() {
    let [low, ..] = rest.to_le_bytes();
    *slot = low & DATA_BITS;
    used = used.saturating_add(1);
    rest = rest.wrapping_shr(BITS_PER_DIGIT);
    if rest == 0 {
      break;
    }
  }
  let digits = digits.get(INTEGER_LIMIT.saturating_sub(used)..).unwrap_or(&[]);
  let last = digits.len().saturating_sub(1);
  for (index, &digit) in digits.iter().enumerate() {
    out.push(if index < last { digit | CONTINUATION } else { digit });
  }
}

/// How many integers a window begins with: its source view's offset and length, its target view's length, and its two
/// sections' lengths.
/// Format: `notes/svndiff` 28-37.
const WINDOW_HEADER_FIELDS: usize = 5;

/// The integers a window begins with, and how many bytes they took.
#[derive(Clone, Copy, Debug)]
struct WindowHeader {
  source_offset: u64,
  source_len: u64,
  target_len: u64,
  instructions_len: u64,
  new_len: u64,
  encoded_len: usize,
}

impl WindowHeader {
  /// Reads a window's header from the start of `input`, or `None` when `input` ends inside it.
  fn read(input: &[u8]) -> Result<Option<Self>, SvndiffError> {
    let mut fields = [0u64; WINDOW_HEADER_FIELDS];
    let mut at = 0usize;
    for field in &mut fields {
      let rest = input.get(at..).unwrap_or(&[]);
      match read_integer(rest).map_err(|_| SvndiffError::CorruptWindowHeader)? {
        Some((value, length)) => {
          *field = value;
          at = at.saturating_add(length);
        }
        None => return Ok(None),
      }
    }
    let [source_offset, source_len, target_len, instructions_len, new_len] = fields;
    Ok(Some(Self { source_offset, source_len, target_len, instructions_len, new_len, encoded_len: at }))
  }

  /// Refuses what Subversion's decoder refuses of a window's header, in its order (`svndiff.c` 714-737): a view or a
  /// section past its limit, a source view past the signed offsets or overflowing, a source view sliding backward
  /// from `previous` (judged only when this window has a source view, against the previous window whatever its view).
  fn check(&self, previous: SourceView) -> Result<(), SvndiffError> {
    if self.target_len > WINDOW_LIMIT
      || self.source_len > WINDOW_LIMIT
      || self.new_len > NEW_DATA_SECTION_LIMIT
      || self.instructions_len > INSTRUCTION_SECTION_LIMIT
    {
      return Err(SvndiffError::WindowTooLarge);
    }
    if self.source_offset > SOURCE_OFFSET_LIMIT {
      return Err(SvndiffError::CorruptWindowHeader);
    }
    let view = SourceView { offset: self.source_offset, len: self.source_len };
    view.check_after(previous)
  }

  /// How many bytes the window takes, its header included.
  fn total_len(&self) -> Result<usize, SvndiffError> {
    let body = self
      .instructions_len
      .checked_add(self.new_len)
      .and_then(|body| usize::try_from(body).ok())
      .ok_or(SvndiffError::CorruptWindowHeader)?;
    self.encoded_len.checked_add(body).ok_or(SvndiffError::CorruptWindowHeader)
  }
}

/// A window's source view: where it starts and how long it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SourceView {
  offset: u64,
  len: u64,
}

impl SourceView {
  /// Refuses a view that overflows, and a view that is not empty yet starts before `previous` or ends before it.
  fn check_after(self, previous: Self) -> Result<(), SvndiffError> {
    let end = self.offset.checked_add(self.len).ok_or(SvndiffError::CorruptWindowHeader)?;
    let previous_end = previous.offset.saturating_add(previous.len);
    if self.len > 0 && (self.offset < previous.offset || end < previous_end) {
      return Err(SvndiffError::BackwardSourceView);
    }
    Ok(())
  }
}

/// What one call of [`Decoder::decode`] made of the bytes it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
  /// The header was read; the first `consumed` bytes are done with.
  Header {
    /// The delta's version.
    version: Version,
    /// How many bytes the header took.
    consumed: usize,
  },
  /// One window was read; the first `consumed` bytes are done with.
  Window {
    /// The window.
    window: Window,
    /// How many bytes it took.
    consumed: usize,
  },
  /// The input ends before the next part: at least `needed` bytes from its start are needed, a count already held to
  /// the format's limits.
  NeedMore {
    /// How many bytes, from the start of the input, the next part needs at least.
    needed: usize,
  },
}

/// Reads a delta a part at a time: its header, then one window a call. It holds no bytes, only its state.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
  version: Option<Version>,
  previous: SourceView,
}

impl Decoder {
  /// A decoder that has read nothing.
  pub fn new() -> Self {
    Self::default()
  }

  /// Reads the next part of the delta from the start of `input`: the header first, then one window a call.
  ///
  /// # Errors
  /// Every refusal of [`SvndiffError`] the part meets; after one, the delta is refused whole.
  pub fn decode(&mut self, input: &[u8]) -> Result<Decoded, SvndiffError> {
    match self.version {
      None => self.read_header(input),
      Some(version) => self.read_window(version, input),
    }
  }

  /// Ends the delta, `rest` being the bytes the caller still holds unconsumed: a delta ends after its header and whole
  /// windows (`svndiff.c` `close_handler`).
  ///
  /// # Errors
  /// [`SvndiffError::UnexpectedEnd`] when the header never came or a window was left part way.
  pub fn finish(&self, rest: &[u8]) -> Result<(), SvndiffError> {
    if self.version.is_none() || !rest.is_empty() {
      return Err(SvndiffError::UnexpectedEnd);
    }
    Ok(())
  }

  /// Reads the header, refusing each byte that cannot begin one as soon as it is seen, as Subversion does.
  fn read_header(&mut self, input: &[u8]) -> Result<Decoded, SvndiffError> {
    for (byte, expected) in input.iter().zip(MAGIC.iter()) {
      if byte != expected {
        return Err(SvndiffError::InvalidHeader);
      }
    }
    let Some(&version_byte) = input.get(MAGIC.len()) else {
      return Ok(Decoded::NeedMore { needed: HEADER_LEN });
    };
    let version = match version_byte {
      0 => Version::Zero,
      1 => return Err(SvndiffError::VersionNotOffered),
      2 => Version::Two,
      _ => return Err(SvndiffError::InvalidHeader),
    };
    self.version = Some(version);
    Ok(Decoded::Header { version, consumed: HEADER_LEN })
  }

  /// Reads one window, or says how many bytes it needs.
  fn read_window(&mut self, version: Version, input: &[u8]) -> Result<Decoded, SvndiffError> {
    let Some(header) = WindowHeader::read(input)? else {
      return Ok(Decoded::NeedMore { needed: input.len().saturating_add(1) });
    };
    header.check(self.previous)?;
    let total = header.total_len()?;
    let Some(body) = input.get(header.encoded_len..total) else {
      return Ok(Decoded::NeedMore { needed: total });
    };
    let window = decode_body(version, &header, body)?;
    self.previous = SourceView { offset: header.source_offset, len: header.source_len };
    Ok(Decoded::Window { window, consumed: total })
  }
}

/// Decodes a window's two sections and checks its instructions against its header.
fn decode_body(version: Version, header: &WindowHeader, body: &[u8]) -> Result<Window, SvndiffError> {
  let instructions_len = usize::try_from(header.instructions_len).map_err(|_| SvndiffError::CorruptWindowHeader)?;
  let (instruction_section, new_section) =
    body.split_at_checked(instructions_len).ok_or(SvndiffError::CorruptWindowHeader)?;
  let new_data = read_section(version, new_section, WINDOW_LIMIT)?;
  let instruction_bytes = read_section(version, instruction_section, INSTRUCTION_SECTION_LIMIT)?;
  let shape = Shape {
    source_len: header.source_len,
    target_len: header.target_len,
    new_len: u64::try_from(new_data.len()).map_err(|_| SvndiffError::CorruptWindowHeader)?,
  };
  let instructions = read_instructions(&instruction_bytes, shape)?;
  Ok(Window {
    source_offset: header.source_offset,
    source_len: header.source_len,
    target_len: header.target_len,
    instructions,
    new_data: new_data.into_owned(),
  })
}

/// Reads one section: as it is in version 0; in version 2, its original length then the bytes, stored when their
/// length is the original's and an LZ4 block otherwise (`compress_lz4.c` `svn__decompress_lz4`), refused past `limit`.
fn read_section(version: Version, section: &[u8], limit: u64) -> Result<Cow<'_, [u8]>, SvndiffError> {
  if version == Version::Zero {
    return Ok(Cow::Borrowed(section));
  }
  let (original, length) = read_integer(section)
    .ok()
    .flatten()
    .ok_or(SvndiffError::InvalidCompressedData)?;
  if original > limit {
    return Err(SvndiffError::InvalidCompressedData);
  }
  let original = usize::try_from(original).map_err(|_| SvndiffError::InvalidCompressedData)?;
  let rest = section.get(length..).ok_or(SvndiffError::InvalidCompressedData)?;
  if rest.len() == original {
    return Ok(Cow::Borrowed(rest));
  }
  let mut out = vec![0u8; original];
  let written = lz4_flex::block::decompress_into(rest, &mut out).map_err(|_| SvndiffError::Lz4)?;
  if written != original {
    return Err(SvndiffError::InvalidCompressedData);
  }
  Ok(Cow::Owned(out))
}

/// What a window's instructions are checked against.
#[derive(Clone, Copy, Debug)]
struct Shape {
  source_len: u64,
  target_len: u64,
  new_len: u64,
}

/// Where a window's instructions have reached: the target made and the new data used.
#[derive(Clone, Copy, Debug, Default)]
struct Progress {
  target: u64,
  new_data: u64,
}

impl Progress {
  /// Checks one instruction where the window has reached and moves past it (`svndiff.c` 486-533).
  fn take(self, instruction: Instruction, shape: Shape) -> Result<Self, InstructionFault> {
    let length = instruction.length();
    if length == 0 {
      return Err(InstructionFault::ZeroLength);
    }
    let room = shape.target_len.checked_sub(self.target).ok_or(InstructionFault::PastTarget)?;
    if length > room {
      return Err(InstructionFault::PastTarget);
    }
    let new_data = self.copied(instruction, shape)?;
    Ok(Self { target: self.target.saturating_add(length), new_data })
  }

  /// Checks what the instruction copies from, and gives the new data used once it has.
  fn copied(self, instruction: Instruction, shape: Shape) -> Result<u64, InstructionFault> {
    match instruction {
      Instruction::Source { offset, length } => {
        let left = shape.source_len.checked_sub(offset).ok_or(InstructionFault::PastSource)?;
        if length > left { Err(InstructionFault::PastSource) } else { Ok(self.new_data) }
      }
      Instruction::Target { offset, .. } => {
        if offset >= self.target { Err(InstructionFault::TargetNotBehind) } else { Ok(self.new_data) }
      }
      Instruction::New { length } => {
        let left = shape.new_len.checked_sub(self.new_data).ok_or(InstructionFault::PastNewData)?;
        if length > left { Err(InstructionFault::PastNewData) } else { Ok(self.new_data.saturating_add(length)) }
      }
    }
  }

  /// Refuses instructions that left target bytes unmade or new data unused (`svndiff.c` 534-539).
  fn finish(self, shape: Shape) -> Result<(), SvndiffError> {
    if self.target != shape.target_len {
      return Err(SvndiffError::TargetNotFilled);
    }
    if self.new_data != shape.new_len {
      return Err(SvndiffError::NewDataUnused);
    }
    Ok(())
  }
}

/// Reads and checks a window's instructions from its decoded instruction section.
fn read_instructions(bytes: &[u8], shape: Shape) -> Result<Vec<Instruction>, SvndiffError> {
  let mut instructions = Vec::new();
  let mut progress = Progress::default();
  let mut rest = bytes;
  while !rest.is_empty() {
    let index = instructions.len();
    let refuse = |fault| SvndiffError::InvalidInstruction { index, fault };
    let (instruction, used) = read_instruction(rest).map_err(refuse)?;
    progress = progress.take(instruction, shape).map_err(refuse)?;
    instructions.push(instruction);
    rest = rest.get(used..).unwrap_or(&[]);
  }
  progress.finish(shape)?;
  Ok(instructions)
}

/// Reads one instruction from the start of `bytes`, and how many bytes it took.
fn read_instruction(bytes: &[u8]) -> Result<(Instruction, usize), InstructionFault> {
  let (&first, rest) = bytes.split_first().ok_or(InstructionFault::Undecodable)?;
  let selector = first & SELECTOR_MASK;
  if selector != SOURCE_SELECTOR && selector != TARGET_SELECTOR && selector != NEW_SELECTOR {
    return Err(InstructionFault::Undecodable);
  }
  let (length, length_bytes) = match first & SHORT_LENGTH_MASK {
    0 => read_operand(rest)?,
    short => (u64::from(short), 0),
  };
  let used = length_bytes.saturating_add(1);
  if selector == NEW_SELECTOR {
    return Ok((Instruction::New { length }, used));
  }
  let (offset, offset_bytes) = read_operand(rest.get(length_bytes..).unwrap_or(&[]))?;
  let instruction = if selector == SOURCE_SELECTOR {
    Instruction::Source { offset, length }
  } else {
    Instruction::Target { offset, length }
  };
  Ok((instruction, used.saturating_add(offset_bytes)))
}

/// Reads an instruction's length or offset, which must end inside its section.
fn read_operand(bytes: &[u8]) -> Result<(u64, usize), InstructionFault> {
  read_integer(bytes).ok().flatten().ok_or(InstructionFault::Undecodable)
}

/// Appends one instruction: its selector with a length under 64 in the same byte, else the length after it, then the
/// offset of a source or target copy, as Subversion's encoder writes it (`svndiff.c` `encode_window`).
fn write_instruction(instruction: Instruction, out: &mut Vec<u8>) {
  let (selector, length, offset) = match instruction {
    Instruction::Source { offset, length } => (SOURCE_SELECTOR, length, Some(offset)),
    Instruction::Target { offset, length } => (TARGET_SELECTOR, length, Some(offset)),
    Instruction::New { length } => (NEW_SELECTOR, length, None),
  };
  match u8::try_from(length) {
    Ok(short) if short != 0 && short <= SHORT_LENGTH_MASK => out.push(selector | short),
    _ => {
      out.push(selector);
      write_integer(length, out);
    }
  }
  if let Some(offset) = offset {
    write_integer(offset, out);
  }
}

/// Writes a delta a part at a time: its header, then windows, each refused if a decoder would refuse it.
#[derive(Clone, Debug)]
pub struct Encoder {
  version: Version,
  previous: SourceView,
}

impl Encoder {
  /// An encoder of the given version that has written nothing.
  pub fn new(version: Version) -> Self {
    Self { version, previous: SourceView::default() }
  }

  /// The delta's header.
  pub fn header(&self) -> [u8; HEADER_LEN] {
    let [s, v, n] = MAGIC;
    [s, v, n, self.version.byte()]
  }

  /// Appends one window's encoding to `out`. A version-2 window's sections follow `compression`; version 0 ignores it.
  ///
  /// # Errors
  /// The refusal a decoder would make of the window ([`SvndiffError`]); nothing is appended then.
  pub fn window(&mut self, window: &Window, compression: Compression, out: &mut Vec<u8>) -> Result<(), SvndiffError> {
    let view = SourceView { offset: window.source_offset, len: window.source_len };
    check_window(window, self.previous)?;
    let mut instruction_bytes = Vec::new();
    for &instruction in &window.instructions {
      write_instruction(instruction, &mut instruction_bytes);
    }
    let instructions = self.write_section(&instruction_bytes, compression)?;
    let new_data = self.write_section(&window.new_data, compression)?;
    let instructions_len = u64::try_from(instructions.len()).map_err(|_| SvndiffError::WindowTooLarge)?;
    let new_len = u64::try_from(new_data.len()).map_err(|_| SvndiffError::WindowTooLarge)?;
    if instructions_len > INSTRUCTION_SECTION_LIMIT || new_len > NEW_DATA_SECTION_LIMIT {
      return Err(SvndiffError::WindowTooLarge);
    }
    for value in [window.source_offset, window.source_len, window.target_len, instructions_len, new_len] {
      write_integer(value, out);
    }
    out.extend_from_slice(&instructions);
    out.extend_from_slice(&new_data);
    self.previous = view;
    Ok(())
  }

  /// One section as this version writes it: as it is in version 0; in version 2, its original length and then the
  /// bytes, an LZ4 block when asked for and shorter.
  fn write_section(&self, section: &[u8], compression: Compression) -> Result<Vec<u8>, SvndiffError> {
    if self.version == Version::Zero {
      return Ok(section.to_vec());
    }
    let mut out = Vec::new();
    write_integer(u64::try_from(section.len()).map_err(|_| SvndiffError::WindowTooLarge)?, &mut out);
    if compression == Compression::Lz4 {
      let mut block = vec![0u8; lz4_flex::block::get_maximum_output_size(section.len())];
      let written = lz4_flex::block::compress_into(section, &mut block).map_err(|_| SvndiffError::Lz4)?;
      if let Some(compressed) = block.get(..written).filter(|compressed| compressed.len() < section.len()) {
        out.extend_from_slice(compressed);
        return Ok(out);
      }
    }
    out.extend_from_slice(section);
    Ok(out)
  }
}

/// Refuses a window a decoder would refuse after `previous`: its views and new data past their limits, its source view
/// past the signed offsets or sliding backward, and its instructions as [`Progress::take`] judges them.
fn check_window(window: &Window, previous: SourceView) -> Result<(), SvndiffError> {
  let new_len = u64::try_from(window.new_data.len()).map_err(|_| SvndiffError::WindowTooLarge)?;
  if window.target_len > WINDOW_LIMIT || window.source_len > WINDOW_LIMIT || new_len > WINDOW_LIMIT {
    return Err(SvndiffError::WindowTooLarge);
  }
  if window.source_offset > SOURCE_OFFSET_LIMIT {
    return Err(SvndiffError::CorruptWindowHeader);
  }
  SourceView { offset: window.source_offset, len: window.source_len }.check_after(previous)?;
  let shape = Shape { source_len: window.source_len, target_len: window.target_len, new_len };
  let mut progress = Progress::default();
  for (index, &instruction) in window.instructions.iter().enumerate() {
    progress = progress.take(instruction, shape).map_err(|fault| SvndiffError::InvalidInstruction { index, fault })?;
  }
  progress.finish(shape)
}

/// Applies one window: appends the target bytes it makes to `target`, given `source_view`, the source's bytes in the
/// window's source view (bytes past the view are not read). The reference applier: byte by byte, a target copy that
/// runs past the position repeating what it copied (`notes/svndiff` 66-70).
///
/// # Errors
/// [`SvndiffError::SourceShort`] when `source_view` is shorter than the window's view, and the refusal a decoder would
/// make of a window that does not hold together; `target` is unchanged then.
pub fn apply(window: &Window, source_view: &[u8], target: &mut Vec<u8>) -> Result<(), SvndiffError> {
  check_window(window, SourceView::default())?;
  let view_len = usize::try_from(window.source_len).map_err(|_| SvndiffError::SourceShort)?;
  let view = source_view.get(..view_len).ok_or(SvndiffError::SourceShort)?;
  let target_len = usize::try_from(window.target_len).map_err(|_| SvndiffError::WindowTooLarge)?;
  let mut made: Vec<u8> = Vec::with_capacity(target_len);
  let mut new_data = window.new_data.as_slice();
  for (index, &instruction) in window.instructions.iter().enumerate() {
    let refuse = |fault| SvndiffError::InvalidInstruction { index, fault };
    match instruction {
      Instruction::Source { offset, length } => {
        made.extend_from_slice(span(view, offset, length).ok_or(refuse(InstructionFault::PastSource))?);
      }
      Instruction::Target { offset, length } => {
        copy_behind(&mut made, offset, length).ok_or(refuse(InstructionFault::TargetNotBehind))?;
      }
      Instruction::New { length } => {
        let count = usize::try_from(length).map_err(|_| refuse(InstructionFault::PastNewData))?;
        let (taken, left) = new_data.split_at_checked(count).ok_or(refuse(InstructionFault::PastNewData))?;
        made.extend_from_slice(taken);
        new_data = left;
      }
    }
  }
  target.extend_from_slice(&made);
  Ok(())
}

/// The bytes `[offset, offset + length)` of `bytes`, if all of them are there.
fn span(bytes: &[u8], offset: u64, length: u64) -> Option<&[u8]> {
  let start = usize::try_from(offset).ok()?;
  let end = start.checked_add(usize::try_from(length).ok()?)?;
  bytes.get(start..end)
}

/// Copies `length` bytes of `made` from `offset`, one at a time, so a copy that runs past the position repeats.
fn copy_behind(made: &mut Vec<u8>, offset: u64, length: u64) -> Option<()> {
  let start = usize::try_from(offset).ok()?;
  let count = usize::try_from(length).ok()?;
  for step in 0..count {
    let byte = *made.get(start.checked_add(step)?)?;
    made.push(byte);
  }
  Some(())
}

/// Decodes a whole delta held in memory: its version and windows.
///
/// # Errors
/// The first refusal any part meets, and [`SvndiffError::UnexpectedEnd`] when the delta stops part way.
pub fn decode_all(input: &[u8]) -> Result<(Version, Vec<Window>), SvndiffError> {
  let mut decoder = Decoder::new();
  let mut rest = input;
  let mut version = None;
  let mut windows = Vec::new();
  while version.is_none() || !rest.is_empty() {
    let consumed = match decoder.decode(rest)? {
      Decoded::Header { version: read, consumed } => {
        version = Some(read);
        consumed
      }
      Decoded::Window { window, consumed } => {
        windows.push(window);
        consumed
      }
      Decoded::NeedMore { .. } => return Err(SvndiffError::UnexpectedEnd),
    };
    rest = rest.get(consumed..).unwrap_or(&[]);
  }
  decoder.finish(rest)?;
  let version = version.ok_or(SvndiffError::UnexpectedEnd)?;
  Ok((version, windows))
}

/// Applies windows in order to a whole source held in memory, each to its own source view: the reference for the
/// server, which applies a window at a time against a file's extents.
///
/// # Errors
/// The first refusal [`apply`] makes, including a source view that runs past the source.
pub fn apply_all(source: &[u8], windows: &[Window]) -> Result<Vec<u8>, SvndiffError> {
  let mut target = Vec::new();
  for window in windows {
    let view = span(source, window.source_offset, window.source_len).ok_or(SvndiffError::SourceShort)?;
    apply(window, view, &mut target)?;
  }
  Ok(target)
}

#[cfg(test)]
mod tests;
