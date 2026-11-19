//! The pointer file a version's tree holds in place of a large file (A-610; git-lfs v3.8.1 `docs/spec.md` [B]; its
//! reader and writer, `lfs/pointer.go` and `lfs/scanner.go` [C]).
//!
//! **Written** ([`write`]) as the specification's one encoding, which is git-lfs's `Encoded`: `version
//! https://git-lfs.github.com/spec/v1`, the extensions by priority as `ext-{priority}-{name} sha256:{oid}`, then `oid
//! sha256:{oid}` and `size {size}`, each line ending in a line feed; a pointer of size zero is the empty file ("An
//! empty file is the pointer for an empty file").
//!
//! **Read** ([`read`]) as the client reads it, since a server that disagreed would miss objects its clients fetch:
//! git-lfs's `DecodePointerFromBlob`, step by step.
//! 1. A blob of 1024 bytes or more is no pointer (`BlobSizeCutoff`; spec.md: "less than 1024 bytes"); the empty blob is
//!    the empty pointer.
//! 2. It must name `git-media`, `hawser` or `git-lfs` somewhere (`matcherRE`).
//! 3. Unicode whitespace is trimmed from both ends (Go's `bytes.TrimSpace`; Rust's `char::is_whitespace` is the same
//!    Unicode White_Space property), and it is read line by line, a line's last carriage return dropped
//!    (`bufio.ScanLines`) and blank lines skipped.
//! 4. Each line is a key, a space and a value (the value is everything after the first space). The keys are `version`,
//!    `oid` and `size` in that order; a key that is not the one expected may be an extension, `ext-` and one digit and
//!    `-` and a word character (`extRE`, `\Aext-\d{1}-\w+`), and any other is refused; a line after `size` is refused.
//! 5. Then, in Go's order: the version is one of the three URLs git-lfs has used; the oid is `sha256:` and 64
//!    lower-case hexadecimal digits; the size is a decimal integer as Go's `strconv.ParseInt` reads it (a sign allowed,
//!    leading zeros allowed) and not negative; each extension's oid as the pointer's; no two extensions of one priority
//!    (one key given twice keeps its last value, as Go's map does).
//!
//! [`Read::canonical`] says whether the bytes are exactly what [`write`] makes of what was read, git-lfs's `Canonical`:
//! a pointer read leniently (a carriage return, a missing last line feed, an older version URL, unsorted extensions) is
//! still the same object, and the server can tell. Each refusal is a kind; [`Refusal::is_not_a_pointer`] separates
//! "this blob is no pointer" from "this pointer is malformed", as git-lfs's `NotAPointerError` does.

use super::Oid;
use crate::out::Out;
use crate::to_u64;

/// Format: the version URL git-lfs writes (`pointer.go`: `latest`).
pub const VERSION_URL: &str = "https://git-lfs.github.com/spec/v1";

/// Format: the size at which a blob is no pointer (`lfs/scanner.go`: `BlobSizeCutoff = 1024`).
pub const CUTOFF: usize = 1024;

/// Format: the words one of which a pointer names (`pointer.go`: `matcherRE`, `git-media|hawser|git-lfs`).
const MARKERS: [&[u8]; 3] = [b"git-media", b"hawser", b"git-lfs"];

/// Format: the hash method an oid names (`pointer.go`: `oidType`).
const OID_TYPE: &[u8] = b"sha256";

/// Format: the extensions a pointer holds at most: one for each priority, a single digit (`extRE`'s `\d{1}`).
pub const MAX_EXTENSIONS: usize = 10;

/// Format: the SHA-256 of no bytes (FIPS 180-4), the empty pointer's oid (`pointer.go`: `EmptyPointer`, through
/// `fs.EmptyObjectSHA256`); a test hashes the empty input to check it.
const EMPTY_OID: [u8; 32] = [
  0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
  0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// Format: the base of a size's digits.
const DECIMAL_RADIX: u32 = 10;

/// Format: the bits that mark a UTF-8 continuation byte, and their value in one (RFC 3629 §3: `10xxxxxx`).
const CONTINUATION_MASK: u8 = 0xc0;

/// Format: a continuation byte's marked bits.
const CONTINUATION: u8 = 0x80;

/// Format: the longest UTF-8 sequence (RFC 3629 §3).
const MAX_UTF8_LEN: usize = 4;

/// The version URL a pointer named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Version {
  /// `https://git-lfs.github.com/spec/v1`, the one git-lfs writes.
  V1,
  /// `https://hawser.github.com/spec/v1`, the pre-release tool's.
  PreRelease,
  /// `http://git-media.io/v/2`, the alpha's.
  Alpha,
}

/// Format: every version URL git-lfs reads (`pointer.go`: `v1Aliases`), compared byte for byte.
const VERSIONS: [(&[u8], Version); 3] = [
  (b"http://git-media.io/v/2", Version::Alpha),
  (b"https://hawser.github.com/spec/v1", Version::PreRelease),
  (VERSION_URL.as_bytes(), Version::V1),
];

/// A pointer extension: another object a filter made the file from, by priority (spec.md, extensions.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Extension<'a> {
  /// The extension's name.
  pub name: &'a str,
  /// The object's id.
  pub oid: Oid,
}

/// A pointer: the object it stands for and the extensions it was made through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pointer<'a> {
  /// The object's id.
  pub oid: Oid,
  /// The object's size in bytes, at most Go's `int64` limit.
  pub size: u64,
  /// The extensions, each at its priority's place.
  pub extensions: [Option<Extension<'a>>; MAX_EXTENSIONS],
}

/// A pointer read, with how it was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Read<'a> {
  /// The pointer.
  pub pointer: Pointer<'a>,
  /// The version URL it named.
  pub version: Version,
  /// Whether its bytes are exactly what [`write`] makes of it.
  pub canonical: bool,
}

/// A pointer's keys, in their order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
  /// `version`.
  Version,
  /// `oid`.
  Oid,
  /// `size`.
  Size,
}

/// Format: the keys in the order a pointer gives them (`pointer.go`: `pointerKeys`).
const KEYS: [(&[u8], Key); 3] = [
  (b"version", Key::Version),
  (b"oid", Key::Oid),
  (b"size", Key::Size),
];

/// Why a blob is not a pointer, or is a malformed one. Each names where it stood, from the blob's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// 1024 bytes or more.
  TooLarge,
  /// None of `git-media`, `hawser` or `git-lfs` in it.
  NoMarker,
  /// A line with no space between key and value.
  NoValue {
    /// Where the line begins.
    at: u64,
  },
  /// A line after `size`.
  ExtraLine {
    /// Where the line begins.
    at: u64,
  },
  /// A key other than the one expected next, and no extension.
  UnexpectedKey {
    /// Where the line begins.
    at: u64,
    /// The key expected.
    expected: Key,
  },
  /// No `version` (only extensions were given).
  MissingVersion,
  /// A version URL git-lfs does not read.
  Version {
    /// Where the value begins.
    at: u64,
  },
  /// No `oid`.
  MissingOid,
  /// An `oid` that is not `sha256:` and 64 lower-case hexadecimal digits.
  Oid {
    /// Where the value begins.
    at: u64,
  },
  /// No `size`.
  MissingSize,
  /// A `size` that is not a decimal integer Go reads, or is negative.
  Size {
    /// Where the value begins.
    at: u64,
  },
  /// An extension whose oid is not `sha256:` and 64 lower-case hexadecimal digits, or whose name is not UTF-8.
  Extension {
    /// Where the line begins.
    at: u64,
  },
  /// Two extensions of one priority.
  DuplicatePriority {
    /// The priority.
    priority: u8,
  },
}

impl Refusal {
  /// Whether the blob is no pointer at all, rather than a malformed one: git-lfs's `NotAPointerError`, which leaves
  /// such a blob as it is.
  pub fn is_not_a_pointer(&self) -> bool {
    matches!(
      self,
      Refusal::TooLarge
        | Refusal::NoMarker
        | Refusal::NoValue { .. }
        | Refusal::ExtraLine { .. }
        | Refusal::MissingVersion
        | Refusal::UnexpectedKey {
          expected: Key::Version,
          ..
        }
    )
  }
}

/// Whether the UTF-8 character at the start of `bytes` is white space, and its length.
fn leading_space(bytes: &[u8]) -> Option<usize> {
  let lead = *bytes.first()?;
  let width = match lead.leading_ones() {
    2 => 2,
    3 => 3,
    4 => 4,
    _ => 1,
  };
  let text = core::str::from_utf8(bytes.get(..width)?).ok()?;
  text
    .chars()
    .next()
    .filter(|named| named.is_whitespace())
    .map(|_| width)
}

/// Whether the UTF-8 character at the end of `bytes` is white space, and its length.
fn trailing_space(bytes: &[u8]) -> Option<usize> {
  let mut start = bytes.len().checked_sub(1)?;
  while bytes
    .get(start)
    .is_some_and(|byte| byte & CONTINUATION_MASK == CONTINUATION)
    && start.saturating_add(MAX_UTF8_LEN) > bytes.len()
  {
    start = start.checked_sub(1)?;
  }
  let text = core::str::from_utf8(bytes.get(start..)?).ok()?;
  let mut chars = text.chars();
  let named = chars.next()?;
  (chars.next().is_none() && named.is_whitespace()).then(|| bytes.len().saturating_sub(start))
}

/// `bytes` with Unicode white space trimmed from both ends, and where what is left begins.
fn trim_space(bytes: &[u8]) -> (&[u8], usize) {
  let mut start = 0usize;
  while let Some(width) = bytes.get(start..).and_then(leading_space) {
    start = start.saturating_add(width);
  }
  let mut end = bytes.len();
  while end > start {
    let Some(width) = bytes.get(start..end).and_then(trailing_space) else {
      break;
    };
    end = end.saturating_sub(width);
  }
  (bytes.get(start..end).unwrap_or_default(), start)
}

/// The lines of `bytes`, each with where it begins: split at line feeds, a last carriage return dropped, blank lines
/// skipped (`bufio.ScanLines`, then `len(text) == 0`).
fn lines(bytes: &[u8], offset: usize) -> impl Iterator<Item = (usize, &[u8])> {
  let mut at = 0usize;
  bytes
    .split(|byte| *byte == b'\n')
    .map(move |line| {
      let start = at;
      at = at.saturating_add(line.len()).saturating_add(1);
      let line = line.strip_suffix(b"\r").unwrap_or(line);
      (offset.saturating_add(start), line)
    })
    .filter(|(_, line)| !line.is_empty())
}

/// Whether `key` is an extension's, `\Aext-\d{1}-\w+`: its priority if so.
fn extension_priority(key: &[u8]) -> Option<u8> {
  let rest = key.strip_prefix(b"ext-")?;
  let (digit, rest) = rest.split_first()?;
  let name = rest.strip_prefix(b"-")?;
  let first = name.first()?;
  (digit.is_ascii_digit() && (first.is_ascii_alphanumeric() || *first == b'_'))
    .then(|| digit.saturating_sub(b'0'))
}

/// The lines' values as the scan gathers them: each key's value and where its line begins, and the extensions' raw
/// lines by priority.
#[derive(Default)]
struct Gathered<'a> {
  /// `version`, `oid` and `size`'s values, with where each line begins.
  values: [Option<(&'a [u8], usize)>; KEYS.len()],
  /// Each extension's name and value, with where its line begins.
  extensions: [Option<(&'a [u8], &'a [u8], usize)>; MAX_EXTENSIONS],
  /// The first pair of extensions of one priority, if any.
  duplicate: Option<u8>,
}

/// Scans the lines of a trimmed pointer (step 4).
fn gather(trimmed: &[u8], offset: usize) -> Result<Gathered<'_>, Refusal> {
  let mut gathered = Gathered::default();
  let mut next_key = 0usize;
  for (at, line) in lines(trimmed, offset) {
    let space = line
      .iter()
      .position(|byte| *byte == b' ')
      .ok_or(Refusal::NoValue { at: to_u64(at) })?;
    let key = line.get(..space).unwrap_or_default();
    let value = line.get(space.saturating_add(1)..).unwrap_or_default();
    let Some((expected, expected_key)) = KEYS.get(next_key) else {
      return Err(Refusal::ExtraLine { at: to_u64(at) });
    };
    if key == *expected {
      if let Some(slot) = gathered.values.get_mut(next_key) {
        *slot = Some((value, at));
      }
      next_key = next_key.saturating_add(1);
      continue;
    }
    let priority = extension_priority(key).ok_or(Refusal::UnexpectedKey {
      at: to_u64(at),
      expected: *expected_key,
    })?;
    let name = key.get(b"ext-0-".len()..).unwrap_or_default();
    if let Some(slot) = gathered.extensions.get_mut(usize::from(priority)) {
      if slot.is_some_and(|(held, _, _)| held != name) && gathered.duplicate.is_none() {
        gathered.duplicate = Some(priority);
      }
      *slot = Some((name, value, at));
    }
  }
  Ok(gathered)
}

/// The oid `value` names: `sha256:` and 64 lower-case hexadecimal digits (`pointer.go`: `parseOid`).
fn parse_oid(value: &[u8]) -> Option<Oid> {
  let colon = value.iter().position(|byte| *byte == b':')?;
  if value.get(..colon)? != OID_TYPE {
    return None;
  }
  Oid::from_hex(value.get(colon.saturating_add(1)..)?)
}

/// The integer `value` writes as Go's `strconv.ParseInt(value, 10, 64)` reads it: an optional sign, then one or more
/// decimal digits, within 64 bits signed.
fn parse_go_int64(value: &[u8]) -> Option<i64> {
  let (negative, digits) = match value.split_first()? {
    (b'+', rest) => (false, rest),
    (b'-', rest) => (true, rest),
    _ => (false, value),
  };
  if digits.is_empty() {
    return None;
  }
  let mut magnitude: u64 = 0;
  for digit in digits {
    let value = char::from(*digit).to_digit(DECIMAL_RADIX)?;
    magnitude = magnitude
      .checked_mul(u64::from(DECIMAL_RADIX))?
      .checked_add(u64::from(value))?;
  }
  if negative {
    return 0i64.checked_sub_unsigned(magnitude);
  }
  i64::try_from(magnitude).ok()
}

/// Reads `blob` as git-lfs reads a pointer (the module's steps).
pub fn read(blob: &[u8]) -> Result<Read<'_>, Refusal> {
  if blob.len() >= CUTOFF {
    return Err(Refusal::TooLarge);
  }
  if blob.is_empty() {
    return Ok(Read {
      pointer: empty(),
      version: Version::V1,
      canonical: true,
    });
  }
  let (trimmed, offset) = trim_space(blob);
  if !MARKERS.iter().any(|marker| {
    trimmed
      .windows(marker.len())
      .any(|window| window == *marker)
  }) {
    return Err(Refusal::NoMarker);
  }
  let gathered = gather(trimmed, offset)?;
  let [version, oid, size] = gathered.values;
  let (version_value, version_at) = version.ok_or(Refusal::MissingVersion)?;
  let version = VERSIONS
    .iter()
    .find(|(url, _)| *url == version_value)
    .map(|(_, version)| *version)
    .ok_or(Refusal::Version {
      at: value_at(version_at, b"version"),
    })?;
  let (oid_value, oid_at) = oid.ok_or(Refusal::MissingOid)?;
  let oid = parse_oid(oid_value).ok_or(Refusal::Oid {
    at: value_at(oid_at, b"oid"),
  })?;
  let (size_value, size_at) = size.ok_or(Refusal::MissingSize)?;
  let size_refused = Refusal::Size {
    at: value_at(size_at, b"size"),
  };
  let size = parse_go_int64(size_value)
    .and_then(|size| u64::try_from(size).ok())
    .ok_or(size_refused)?;
  let pointer = Pointer {
    oid,
    size,
    extensions: extensions(&gathered.extensions)?,
  };
  if let Some(priority) = gathered.duplicate {
    return Err(Refusal::DuplicatePriority { priority });
  }
  let mut same = Same::new(blob);
  write_unchecked(&pointer, &mut same);
  Ok(Read {
    pointer,
    version,
    canonical: same.matches(),
  })
}

/// Where the value of the line at `line` with key `key` begins.
fn value_at(line: usize, key: &[u8]) -> u64 {
  to_u64(line.saturating_add(key.len()).saturating_add(1))
}

/// The extensions gathered, each name UTF-8 and each oid `sha256:` and 64 digits.
fn extensions<'a>(
  gathered: &[Option<(&'a [u8], &'a [u8], usize)>; MAX_EXTENSIONS],
) -> Result<[Option<Extension<'a>>; MAX_EXTENSIONS], Refusal> {
  let mut extensions = [None; MAX_EXTENSIONS];
  for (slot, raw) in extensions.iter_mut().zip(gathered) {
    let Some((name, value, at)) = raw else {
      continue;
    };
    let refused = Refusal::Extension { at: to_u64(*at) };
    *slot = Some(Extension {
      name: core::str::from_utf8(name).map_err(|_| refused)?,
      oid: parse_oid(value).ok_or(refused)?,
    });
  }
  Ok(extensions)
}

/// The pointer of the empty file: SHA-256 of no bytes, size zero.
pub fn empty() -> Pointer<'static> {
  Pointer {
    oid: Oid::from_bytes(EMPTY_OID),
    size: 0,
    extensions: [None; MAX_EXTENSIONS],
  }
}

/// An [`Out`] that says whether what is written is exactly `expected`, holding nothing.
struct Same<'a> {
  /// The bytes to match.
  expected: &'a [u8],
  /// How many matched so far.
  at: usize,
  /// Whether all written so far matched.
  equal: bool,
}

impl<'a> Same<'a> {
  /// Nothing written yet.
  fn new(expected: &'a [u8]) -> Self {
    Self {
      expected,
      at: 0,
      equal: true,
    }
  }

  /// Whether what was written is exactly the expected bytes, no more and no fewer.
  fn matches(&self) -> bool {
    self.equal && self.at == self.expected.len()
  }
}

impl Out for Same<'_> {
  fn put(&mut self, bytes: &[u8]) {
    let end = self.at.saturating_add(bytes.len());
    self.equal = self.equal && self.expected.get(self.at..end) == Some(bytes);
    self.at = end;
  }
}

/// Why a pointer cannot be written. Nothing is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
  /// A size past Go's `int64` limit, which no client reads.
  Size,
  /// An extension name that is not a key the specification allows ("Keys MUST only use the characters `[a-z] [0-9] .
  /// -`") beginning with a letter or digit, as git-lfs reads it back.
  ExtensionName {
    /// The extension's priority.
    priority: u8,
  },
}

/// Writes `pointer` as the specification's one encoding (the module's list): nothing when its size is zero.
pub fn write(pointer: &Pointer<'_>, out: &mut impl Out) -> Result<(), WriteRefusal> {
  if i64::try_from(pointer.size).is_err() {
    return Err(WriteRefusal::Size);
  }
  for (priority, extension) in pointer.extensions.iter().enumerate() {
    let Some(extension) = extension else {
      continue;
    };
    let name = extension.name.as_bytes();
    let first_ok = name
      .first()
      .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let rest_ok = name.iter().all(|byte| {
      byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'.' || *byte == b'-'
    });
    if !(first_ok && rest_ok) {
      return Err(WriteRefusal::ExtensionName {
        priority: u8::try_from(priority).unwrap_or(u8::MAX),
      });
    }
  }
  write_unchecked(pointer, out);
  Ok(())
}

/// Writes `pointer`'s encoding without checking it: git-lfs's `Encoded`.
fn write_unchecked(pointer: &Pointer<'_>, out: &mut impl Out) {
  if pointer.size == 0 {
    return;
  }
  out.put(b"version ");
  out.put(VERSION_URL.as_bytes());
  out.put(b"\n");
  for (priority, extension) in pointer.extensions.iter().enumerate() {
    let Some(extension) = extension else {
      continue;
    };
    out.put(b"ext-");
    super::json::put_u64(out, to_u64(priority));
    out.put(b"-");
    out.put(extension.name.as_bytes());
    out.put(b" sha256:");
    extension.oid.put_hex(out);
    out.put(b"\n");
  }
  out.put(b"oid sha256:");
  pointer.oid.put_hex(out);
  out.put(b"\nsize ");
  super::json::put_u64(out, pointer.size);
  out.put(b"\n");
}

#[cfg(test)]
#[path = "../tests/lfs_pointer.rs"]
mod tests;
