//! Object names and the object header (A-600; §4.17.2 "Pieces (git)", piece 2, and "The repository in git's terms",
//! item 1, "Object names").
//!
//! An object's name is the hash of its header and its content. The header is `<type> SP <decimal size> NUL`, the type
//! one of git's four and the size the content's length in bytes, written as git writes it (`format_object_header`), then
//! the content follows (`hash_object_file`). The same header begins a loose object once it is inflated, and git reads it
//! there (`parse_loose_header`). A repository names its objects with one of two hashes, which protocol v2 announces as
//! `object-format`: SHA-1 with collision detection, or SHA-256 [B: hash-function-transition, gitprotocol-v2].
//!
//! **Sources** (read 2026-11-19, git 2.50.1). The code [C]:
//! - `object-file.c`: `format_object_header`, `parse_loose_header` and `MAX_HEADER_LEN`;
//! - `object.c`: `object_type_strings` and `type_from_string_gently`;
//! - `object.h`: `enum object_type`, whose values "are part of pack file format";
//! - `hash.h` and `hash.c`: `hash_algos`, each format's name and lengths;
//! - `sha1dc_git.c`: the collision check git runs, with the safe hash off, and a collision refused naming the plain
//!   hash.
//!
//! The documents [B]: `gitformat-pack` ("Object types"; ofs-delta and ref-delta are "only valid in a pack file") and
//! `hash-function-transition`.
//!
//! **SHA-1 with collision detection.** git hashes SHA-1 through sha1collisiondetection ("moved to a hardened SHA-1
//! implementation by default, which isn't vulnerable to the SHAttered attack" [B: hash-function-transition]). Here it is
//! RustCrypto's `sha1-checked`, a pure-Rust port of the same detection, configured as git configures it: detection on,
//! the safe hash off (`git_SHA1DCInit`). So an input in which a block shows a known attack's disturbance is refused
//! ([`CollisionAttack`]) and named by its plain SHA-1, as `git_SHA1DCFinal` dies naming it ("SHA-1 appears to be part
//! of a collision attack"). It is proven on git's own t0013 vector (`shattered-1.pdf`) and its SHAttered twin, each
//! checked by its SHA-256 and cut to the 320 bytes that hold both colliding blocks.
//!
//! **SHA-256** is hashed through aws-lc-rs, the workspace's one cryptographic library.
//!
//! **Bounds (§4.9).** A header is read within git's own [`MAX_HEADER_LEN`]. The size it names is charged to the caller's
//! [`Admission`] before any content is read, so a header that claims more than its request may hold is refused at once.
//! Every refusal is a typed variant.

use crate::out::Out;
use crate::{Admission, PastAdmission, to_u64};

/// Format: the longest object header git reads, its NUL included (`object-file.c`'s `MAX_HEADER_LEN`). git refuses a
/// longer one ("header for %s too long, exceeds %d bytes").
pub const MAX_HEADER_LEN: usize = 32;

/// Format: the separator between an object header's type and its size [C: `format_object_header`'s `"%s %"`].
const SPACE: u8 = b' ';

/// Format: the byte that ends an object header [C: `parse_loose_header`: "The length must be followed by a zero byte"].
const NUL: u8 = 0;

/// Format: the digits a size is written in, base ten [C: `parse_loose_header`: "canonical decimal format"].
const DECIMAL_DIGITS: &[u8; 10] = b"0123456789";

/// Format: the base of a size's digits.
const DECIMAL: u64 = 10;

/// Format: the most decimal digits a size has, those of `u64::MAX`, 18446744073709551615.
const MAX_SIZE_DIGITS: usize = 20;

/// Format: the names of the types only a pack may hold [B: gitformat-pack "Deltified representation": "These
/// representations are assigned new types ofs-delta and ref-delta, which is only valid in a pack file"].
const PACK_ONLY: [&[u8]; 2] = [b"ofs-delta", b"ref-delta"];

/// An object's type as its header names it: git's four [C: `object_type_strings`]. The two delta types, `ofs-delta` and
/// `ref-delta`, exist only inside a pack, and no header may name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
  /// `commit`, pack type 1.
  Commit,
  /// `tree`, pack type 2.
  Tree,
  /// `blob`, pack type 3.
  Blob,
  /// `tag`, pack type 4.
  Tag,
}

/// Format: each type, its name in a header and its number in a pack (`object.h`'s `enum object_type`, `object.c`'s
/// `object_type_strings`).
const KINDS: [(Kind, &[u8], u8); 4] = [
  (Kind::Commit, b"commit", 1),
  (Kind::Tree, b"tree", 2),
  (Kind::Blob, b"blob", 3),
  (Kind::Tag, b"tag", 4),
];

impl Kind {
  /// The type's name as a header carries it.
  pub fn name(self) -> &'static [u8] {
    KINDS
      .iter()
      .find(|(kind, _, _)| *kind == self)
      .map_or(&[], |(_, name, _)| *name)
  }

  /// The type's number in a pack's entry header [B: gitformat-pack "Object types"].
  pub fn pack_number(self) -> u8 {
    KINDS
      .iter()
      .find(|(kind, _, _)| *kind == self)
      .map_or(0, |(_, _, number)| *number)
  }

  /// The type a header names, compared byte for byte as git compares it [C: `type_from_string_gently`].
  pub fn from_name(name: &[u8]) -> Result<Kind, KindRefusal> {
    if let Some((kind, _, _)) = KINDS.iter().find(|(_, known, _)| *known == name) {
      return Ok(*kind);
    }
    if PACK_ONLY.contains(&name) {
      return Err(KindRefusal::PackOnly);
    }
    Err(KindRefusal::Unknown)
  }
}

/// Why a header's type is not one an object may have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindRefusal {
  /// A type git does not know.
  Unknown,
  /// `ofs-delta` or `ref-delta`: a representation only a pack may hold, never an object's own type.
  PackOnly,
}

/// An object's header: its type, and its content's length in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
  /// The object's type.
  pub kind: Kind,
  /// The content's length in bytes.
  pub size: u64,
}

/// Why bytes do not begin with an object header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRefusal {
  /// The bytes ended before the header's NUL, within [`MAX_HEADER_LEN`]: not yet whole to a reader.
  Truncated,
  /// No NUL within [`MAX_HEADER_LEN`] bytes.
  TooLong,
  /// The NUL came before any space, so the header names no size.
  NoSize,
  /// The type is not one an object may have.
  Kind(KindRefusal),
  /// The size is not canonical decimal: no digit, a zero before another digit, or a byte other than a digit before the
  /// NUL [C: `parse_loose_header`: "ie "010" is not valid"].
  NotDecimal {
    /// Where the offending byte is, from the header's start (its NUL's place when the size is empty).
    at: u64,
  },
  /// The size does not fit 64 bits.
  SizeOverflow,
  /// The size is more than the caller's admission holds: refused before any content is read.
  PastAdmission(PastAdmission),
}

/// Reads the header `input` begins with and charges its size to `admission`: the header and its length in bytes, NUL
/// included, where the content begins. Reads at most [`MAX_HEADER_LEN`] bytes.
pub fn read_header(
  input: &[u8],
  admission: &mut Admission,
) -> Result<(Header, usize), HeaderRefusal> {
  let window = input.get(..MAX_HEADER_LEN).unwrap_or(input);
  let Some(nul) = window.iter().position(|byte| *byte == NUL) else {
    return Err(if input.len() >= MAX_HEADER_LEN {
      HeaderRefusal::TooLong
    } else {
      HeaderRefusal::Truncated
    });
  };
  let text = window.get(..nul).unwrap_or_default();
  let Some(space) = text.iter().position(|byte| *byte == SPACE) else {
    return Err(HeaderRefusal::NoSize);
  };
  let name = text.get(..space).unwrap_or_default();
  let digits = text.get(space.saturating_add(1)..).unwrap_or_default();
  let kind = Kind::from_name(name).map_err(HeaderRefusal::Kind)?;
  let size = canonical_decimal(digits, space.saturating_add(1))?;
  admission
    .charge(size)
    .map_err(HeaderRefusal::PastAdmission)?;
  Ok((Header { kind, size }, nul.saturating_add(1)))
}

/// The value of `digits`, a size in canonical decimal: at least one digit, no zero before another, nothing else. `from`
/// is where the digits begin in their header, for a refusal's place.
fn canonical_decimal(digits: &[u8], from: usize) -> Result<u64, HeaderRefusal> {
  decimal(digits).map_err(|refusal| match refusal {
    DecimalRefusal::NotDecimal { offset } => HeaderRefusal::NotDecimal {
      at: to_u64(from.saturating_add(offset)),
    },
    DecimalRefusal::Overflow => HeaderRefusal::SizeOverflow,
  })
}

/// Why digits are not a number in canonical decimal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecimalRefusal {
  /// No digit, a zero before another digit, or a byte that is not a digit.
  NotDecimal {
    /// Where, from the digits' start (0 when there are none).
    offset: usize,
  },
  /// The number does not fit 64 bits.
  Overflow,
}

/// The value of `digits` in canonical decimal, as git writes a number (`%d`, `%"PRIuMAX`, `%"PRItime`): at least one
/// digit, no zero before another, no sign, nothing else. A header's size reads so (A-600), and so do protocol v0's
/// depth and time (A-604).
pub(crate) fn decimal(digits: &[u8]) -> Result<u64, DecimalRefusal> {
  match digits {
    [] => return Err(DecimalRefusal::NotDecimal { offset: 0 }),
    [b'0', _, ..] => return Err(DecimalRefusal::NotDecimal { offset: 1 }),
    _ => {}
  }
  let mut value: u64 = 0;
  for (offset, byte) in digits.iter().enumerate() {
    let Some(digit) = DECIMAL_DIGITS.iter().position(|known| known == byte) else {
      return Err(DecimalRefusal::NotDecimal { offset });
    };
    value = value
      .checked_mul(DECIMAL)
      .and_then(|tens| tens.checked_add(to_u64(digit)))
      .ok_or(DecimalRefusal::Overflow)?;
  }
  Ok(value)
}

/// Writes `header` as git writes it: its type's name, a space, its size in decimal and a NUL
/// (`format_object_header`).
pub fn write_header(header: Header, out: &mut impl Out) {
  out.put(header.kind.name());
  out.put(&[SPACE]);
  write_decimal(header.size, out);
  out.put(&[NUL]);
}

/// Writes `value` in decimal as git writes a number: no sign, no zero first, one `0` for zero.
pub(crate) fn write_decimal(value: u64, out: &mut (impl Out + ?Sized)) {
  let mut digits = [0u8; MAX_SIZE_DIGITS];
  let mut start = digits.len();
  let mut left = value;
  loop {
    let digit = left.checked_rem(DECIMAL).unwrap_or(0);
    left = left.checked_div(DECIMAL).unwrap_or(0);
    let Some(next) = start.checked_sub(1) else {
      break;
    };
    start = next;
    if let (Some(slot), Some(byte)) = (
      digits.get_mut(start),
      usize::try_from(digit)
        .ok()
        .and_then(|digit| DECIMAL_DIGITS.get(digit)),
    ) {
      *slot = *byte;
    }
    if left == 0 {
      break;
    }
  }
  out.put(digits.get(start..).unwrap_or_default());
}

/// The hash a repository names its objects with: what protocol v2's `object-format` capability names
/// [B: gitprotocol-v2; C: `hash_algos`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectFormat {
  /// `sha1`: SHA-1 with collision detection, 20 bytes.
  Sha1,
  /// `sha256`: SHA-256, 32 bytes.
  Sha256,
}

/// Format: each object format, its `object-format` value and its name's length in bytes (`hash.c`'s `hash_algos`:
/// `"sha1"` with `GIT_SHA1_RAWSZ` 20, `"sha256"` with `GIT_SHA256_RAWSZ` 32).
const FORMATS: [(ObjectFormat, &str, usize); 2] = [
  (ObjectFormat::Sha1, "sha1", 20),
  (ObjectFormat::Sha256, "sha256", 32),
];

/// Derived: the longest object name any format makes, SHA-256's.
const MAX_RAW_LEN: usize = 32;

/// Format: the bits one hexadecimal digit of an object name carries.
const HEX_DIGIT_BITS: u32 = 4;

/// Format: the digits git writes an object name in, lower case (`hex.c`'s `hash_to_hex`).
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Format: the bits of a byte its low hexadecimal digit carries.
const LOW_DIGIT: u8 = 0x0f;

/// An `object-format` value git does not know, compared exactly as `hash_algo_by_name` compares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownFormat;

impl ObjectFormat {
  /// The format's `object-format` value.
  pub fn name(self) -> &'static str {
    FORMATS
      .iter()
      .find(|(format, _, _)| *format == self)
      .map_or("", |(_, name, _)| *name)
  }

  /// The format an `object-format` value names.
  pub fn from_name(name: &[u8]) -> Result<ObjectFormat, UnknownFormat> {
    FORMATS
      .iter()
      .find(|(_, known, _)| known.as_bytes() == name)
      .map(|(format, _, _)| *format)
      .ok_or(UnknownFormat)
  }

  /// The length of an object name in this format, in bytes.
  pub fn raw_len(self) -> usize {
    FORMATS
      .iter()
      .find(|(format, _, _)| *format == self)
      .map_or(0, |(_, _, len)| *len)
  }

  /// The length of an object name in this format written in hexadecimal: two digits a byte.
  pub fn hex_len(self) -> usize {
    self.raw_len().saturating_mul(2)
  }
}

/// An object's name: the hash of its header and content, in its repository's format.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
  format: ObjectFormat,
  /// The name's bytes, the format's [`ObjectFormat::raw_len`] of them, zeros after.
  bytes: [u8; MAX_RAW_LEN],
}

/// Why text is not an object name of a format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HexRefusal {
  /// Not the format's [`ObjectFormat::hex_len`] digits.
  Length {
    /// The digits given.
    len: u64,
  },
  /// A byte that is not a hexadecimal digit.
  NotHex {
    /// Where it is.
    at: u64,
  },
}

impl ObjectId {
  /// The name `bytes` make in `format`, or `None` when they are not that format's length.
  pub fn from_bytes(format: ObjectFormat, bytes: &[u8]) -> Option<ObjectId> {
    if bytes.len() != format.raw_len() {
      return None;
    }
    let mut name = [0u8; MAX_RAW_LEN];
    name.get_mut(..bytes.len())?.copy_from_slice(bytes);
    Some(ObjectId {
      format,
      bytes: name,
    })
  }

  /// The name `hex` writes in `format`, its digits of either case as git reads them (`hex.c`'s `hexval`).
  pub fn from_hex(format: ObjectFormat, hex: &[u8]) -> Result<ObjectId, HexRefusal> {
    if hex.len() != format.hex_len() {
      return Err(HexRefusal::Length {
        len: to_u64(hex.len()),
      });
    }
    let mut name = [0u8; MAX_RAW_LEN];
    for (index, (slot, pair)) in name.iter_mut().zip(hex.chunks_exact(2)).enumerate() {
      let at = index.saturating_mul(2);
      let high = hex_value(pair.first().copied()).ok_or(HexRefusal::NotHex { at: to_u64(at) })?;
      let low = hex_value(pair.get(1).copied()).ok_or(HexRefusal::NotHex {
        at: to_u64(at.saturating_add(1)),
      })?;
      *slot = high.checked_shl(HEX_DIGIT_BITS).unwrap_or(0) | low;
    }
    Ok(ObjectId {
      format,
      bytes: name,
    })
  }

  /// The name's format.
  pub fn format(&self) -> ObjectFormat {
    self.format
  }

  /// The name's bytes.
  pub fn as_bytes(&self) -> &[u8] {
    self
      .bytes
      .get(..self.format.raw_len())
      .unwrap_or(&self.bytes)
  }

  /// The null name of `format`, every byte zero (git's `null_oid`): the name no object has, which the advertisement of a
  /// repository with no refs carries (A-604).
  pub fn null(format: ObjectFormat) -> ObjectId {
    ObjectId {
      format,
      bytes: [0; MAX_RAW_LEN],
    }
  }

  /// Whether this is its format's null name.
  pub fn is_null(&self) -> bool {
    self.as_bytes().iter().all(|byte| *byte == 0)
  }

  /// Writes the name in lower-case hexadecimal, as git writes it (`hex.c`'s `hash_to_hex`): the format's
  /// [`ObjectFormat::hex_len`] digits.
  pub fn write_hex(&self, out: &mut (impl Out + ?Sized)) {
    for byte in self.as_bytes() {
      out.put(&hex_pair(*byte));
    }
  }
}

/// The value of one hexadecimal digit of either case.
fn hex_value(digit: Option<u8>) -> Option<u8> {
  let digit = digit?.to_ascii_lowercase();
  HEX_DIGITS
    .iter()
    .position(|known| *known == digit)
    .and_then(|value| u8::try_from(value).ok())
}

/// The name's bytes, as the object store keys and compares names (A-614: `slates_vcs::Naming::Id`).
impl AsRef<[u8]> for ObjectId {
  fn as_ref(&self) -> &[u8] {
    self.as_bytes()
  }
}

/// The two lower-case hexadecimal digits of `byte`, high first.
fn hex_pair(byte: u8) -> [u8; 2] {
  let digit = |value: u8| HEX_DIGITS.get(usize::from(value)).copied().unwrap_or(b'0');
  [
    digit(byte.checked_shr(HEX_DIGIT_BITS).unwrap_or(0)),
    digit(byte & LOW_DIGIT),
  ]
}

impl core::fmt::Display for ObjectId {
  /// The name in lower-case hexadecimal, as git writes it.
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    for byte in self.as_bytes() {
      for digit in hex_pair(*byte) {
        core::fmt::Write::write_char(f, char::from(digit))?;
      }
    }
    Ok(())
  }
}

impl core::fmt::Debug for ObjectId {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}:{self}", self.format.name())
  }
}

/// An input SHA-1's collision detection found to be part of a known collision attack: refused, as git refuses it
/// ("SHA-1 appears to be part of a collision attack: %s", `sha1dc_git.c`), with the plain SHA-1 it computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollisionAttack {
  /// The input's plain SHA-1, the name git's message gives it.
  pub sha1: ObjectId,
}

/// Hashes bytes into an object name: an object's header and then its content ([`Hasher::object`]), or raw bytes
/// ([`Hasher::raw`]), in either format.
pub struct Hasher {
  state: State,
}

/// The running hash of a [`Hasher`].
enum State {
  /// SHA-1 with collision detection, boxed: its detection keeps the expanded message of the block it checks.
  Sha1(Box<sha1_checked::Sha1>),
  /// SHA-256.
  Sha256(aws_lc_rs::digest::Context),
}

impl Hasher {
  /// A hasher of raw bytes in `format`, with no header: what git's `test-tool sha1` hashes.
  pub fn raw(format: ObjectFormat) -> Hasher {
    let state = match format {
      // git's own configuration of the detection: on, with the safe hash off (`git_SHA1DCInit`).
      ObjectFormat::Sha1 => State::Sha1(Box::new(
        sha1_checked::Sha1::builder()
          .detect_collision(true)
          .safe_hash(false)
          .build(),
      )),
      ObjectFormat::Sha256 => {
        State::Sha256(aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256))
      }
    };
    Hasher { state }
  }

  /// A hasher of an object with `header` in `format`: the header hashed first, as git's `hash_object_file` hashes it,
  /// the content to follow through [`Hasher::update`].
  pub fn object(format: ObjectFormat, header: Header) -> Hasher {
    let mut hasher = Hasher::raw(format);
    write_header(header, &mut hasher);
    hasher
  }

  /// Hashes `bytes` next.
  pub fn update(&mut self, bytes: &[u8]) {
    match &mut self.state {
      State::Sha1(hash) => sha1_checked::Digest::update(hash.as_mut(), bytes),
      State::Sha256(context) => context.update(bytes),
    }
  }

  /// The name of what was hashed, or the attack SHA-1's detection found in it.
  pub fn finish(self) -> Result<ObjectId, CollisionAttack> {
    match self.state {
      State::Sha1(hash) => {
        let result = (*hash).try_finalize();
        let name = ObjectId::from_bytes(ObjectFormat::Sha1, result.hash().as_slice());
        let name = name.unwrap_or(ObjectId {
          format: ObjectFormat::Sha1,
          bytes: [0; MAX_RAW_LEN],
        });
        if result.has_collision() {
          return Err(CollisionAttack { sha1: name });
        }
        Ok(name)
      }
      State::Sha256(context) => {
        let digest = context.finish();
        Ok(
          ObjectId::from_bytes(ObjectFormat::Sha256, digest.as_ref()).unwrap_or(ObjectId {
            format: ObjectFormat::Sha256,
            bytes: [0; MAX_RAW_LEN],
          }),
        )
      }
    }
  }
}

impl Out for Hasher {
  fn put(&mut self, bytes: &[u8]) {
    self.update(bytes);
  }
}

/// The name of an object of `kind` whose whole content is `content`, in `format`.
pub fn name_of(
  format: ObjectFormat,
  kind: Kind,
  content: &[u8],
) -> Result<ObjectId, CollisionAttack> {
  let mut hasher = Hasher::object(
    format,
    Header {
      kind,
      size: to_u64(content.len()),
    },
  );
  hasher.update(content);
  hasher.finish()
}

#[cfg(test)]
#[path = "tests/object_name.rs"]
mod tests;
