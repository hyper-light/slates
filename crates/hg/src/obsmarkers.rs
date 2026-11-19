//! Obsolescence markers in the two formats 7.2 reads and writes (A-545; `obsolete.py` and `cext/parsers.c` [C];
//! `evolution.txt` [B]): the `obsmarkers` part's payload, and the store's `obsstore` file, are a version byte and then
//! markers.
//!
//! - Format 0 (`>BIB20s`): the successor count, the metadata's length (32 bits), the flags (a byte), the predecessor,
//!   the successors (20 bytes each), then the metadata: `key:value` entries joined by NUL bytes, the date (`date`) and
//!   the predecessor's parents (`p0`, `p1`, `p2`) among them.
//! - Format 1 (`>IdhHBBB`): the marker's total size (32 bits), the date (a big-endian float64 of seconds), the timezone
//!   (minutes, signed 16 bits), the flags (16 bits), the successor, parent and metadata counts (a byte each, a parent
//!   count of 3 meaning "no parent data"), the predecessor, the successors, the parents (20 bytes each, or 32 with flag
//!   2, `usingsha256`), a (key length, value length) byte pair per metadata entry, then the keys and values.
//!
//! 7.2's readers disagree with each other and trust what they are given in ways this module does not, each refused by
//! kind here. Format 1's total size: the C reader (`fm1readmarker`) steps from marker to marker by it and reads each
//! marker's fields only within it, so trailing bytes inside a marker are skipped unseen, while the pure reader
//! (`_fm1purereadmarkers`) ignores it and walks by the counts; a marker whose size is not exactly its fields is
//! refused, so both readers would see the same markers. A parent count other than 0, 1, 2 or 3 (the C reader reads it as
//! no parent data, the pure one reads that many parents). A format 0 metadata entry with no `:` (7.2's
//! `split(b':', 1)` fails on one). A format 0 marker flagged SHA-256 (7.2 reads 20-byte nodes under it; its writer
//! refuses one: "cannot handle sha256 with old obsstore format"). A format 1 date is kept as its float's bits, so a
//! marker encodes back to exactly its bytes.
//!
//! Nothing is allocated in decoding: the payload is checked whole, then its markers are read in place, so the payload
//! is the only cap. An encode is checked whole before a byte is written, and its exact length is known before it is
//! made ([`encoded_len`]). Converting a marker between the formats (format 0 keeps the date and parents in its
//! metadata) is the exchange's (piece 7), not this codec's.

use core::slice::{ChunksExact, Iter, Split};

use crate::Truncated;
use crate::out::{Measure, Out};
use crate::read::{Cursor, to_u64};

/// Format: format 0's version byte (`_fm0version`).
pub const VERSION_0: u8 = 0;

/// Format: format 1's version byte (`_fm1version`).
pub const VERSION_1: u8 = 1;

/// Format: the flag that makes a marker's nodes SHA-256 (`usingsha256 = 2`, `obsolete.py`).
pub const FLAG_SHA256: u16 = 2;

/// Format: a SHA-1 node's length (`_fm0node`, `_fm1nodesha1`).
const SHA1_LEN: usize = 20;

/// Format: a SHA-256 node's length (`_fm1nodesha256`).
const SHA256_LEN: usize = 32;

/// Format: the most parents a format 1 marker records (a changeset's two), before the count that means none.
const MAX_PARENTS: u8 = 2;

/// Format: format 1's parent count that means "no parent data" (`_fm1parentnone`).
const NO_PARENTS: u8 = 3;

/// Format: format 1's fixed part (`_fm1fixed = '>IdhHBBB'`): the size (4 bytes), the date (8), the timezone (2), the
/// flags (2), and the three counts (1 each).
const V1_FIXED_LEN: usize = 19;

/// Format: format 1's metadata sizes: a key length and a value length per entry, a byte each (`_fm1metapair = 'BB'`).
const SIZE_PAIR_LEN: usize = 2;

/// Format: format 0's metadata: entries are joined by NUL, and a key ends at the first `:` (`_fm0encodemeta`).
const ENTRY_SEPARATOR: u8 = b'\0';

/// Format: see [`ENTRY_SEPARATOR`].
const KEY_SEPARATOR: u8 = b':';

/// A marker format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
  /// Format 0.
  V0,
  /// Format 1.
  V1,
}

/// A format 1 marker's date: seconds since the epoch as a float64's bits, and the timezone in minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
  /// The bits of the float64 that counts the seconds, kept as sent so a marker encodes back to its bytes.
  pub seconds_bits: u64,
  /// The timezone offset in minutes, as 7.2 stores it (`tz // 60`).
  pub offset_minutes: i16,
}

impl Date {
  /// The seconds as a float.
  pub fn seconds(&self) -> f64 {
    f64::from_bits(self.seconds_bits)
  }
}

/// A marker's metadata, in its format's form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metadata<'a> {
  /// Format 0's: `key:value` entries joined by NUL bytes.
  Joined(&'a [u8]),
  /// Format 1's: a (key length, value length) byte pair per entry, then the keys and values.
  Sized {
    /// The byte pairs.
    sizes: &'a [u8],
    /// The keys and values.
    bytes: &'a [u8],
  },
}

/// One marker, read in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker<'a> {
  /// Its flags (a byte in format 0).
  pub flags: u16,
  /// The changeset made obsolete.
  pub predecessor: &'a [u8],
  /// Its successors' nodes, one after another (none for a prune).
  pub successors: &'a [u8],
  /// Format 1: the predecessor's parents' nodes, or `None` for "no parent data".
  pub parents: Option<&'a [u8]>,
  /// Format 1's date (format 0 keeps it in its metadata).
  pub date: Option<Date>,
  /// The metadata.
  pub metadata: Metadata<'a>,
}

impl<'a> Marker<'a> {
  /// The length of each of the marker's nodes.
  pub fn node_len(&self) -> usize {
    node_len(self.flags)
  }

  /// The successors' nodes.
  pub fn successor_nodes(&self) -> ChunksExact<'a, u8> {
    self.successors.chunks_exact(self.node_len())
  }

  /// The parents' nodes, if the marker records them (format 1).
  pub fn parent_nodes(&self) -> Option<ChunksExact<'a, u8>> {
    let len = self.node_len();
    self.parents.map(|parents| parents.chunks_exact(len))
  }

  /// The metadata's entries, keys and values, in order. Format 0's empty entries are passed over, as 7.2 passes them.
  pub fn entries(&self) -> Entries<'a> {
    let separator: fn(&u8) -> bool = is_entry_separator;
    Entries {
      walk: match self.metadata {
        Metadata::Joined(joined) => EntryWalk::Joined(joined.split(separator)),
        Metadata::Sized { sizes, bytes } => EntryWalk::Sized {
          pairs: sizes.as_chunks::<SIZE_PAIR_LEN>().0.iter(),
          bytes: Cursor::new(bytes),
        },
      },
    }
  }
}

/// Whether a byte separates format 0's metadata entries.
fn is_entry_separator(byte: &u8) -> bool {
  *byte == ENTRY_SEPARATOR
}

/// A format 0 entry's key and value, split at its first `:`; `None` for an entry with no `:`.
fn split_entry(entry: &[u8]) -> Option<(&[u8], &[u8])> {
  let at = entry.iter().position(|byte| *byte == KEY_SEPARATOR)?;
  Some((entry.get(..at)?, entry.get(at.saturating_add(1)..)?))
}

/// The entries of a marker's metadata.
pub struct Entries<'a> {
  /// Where the walk stands.
  walk: EntryWalk<'a>,
}

/// A walk over one format's metadata.
enum EntryWalk<'a> {
  /// Format 0's entries.
  Joined(Split<'a, u8, fn(&u8) -> bool>),
  /// Format 1's entries.
  Sized {
    /// The size pairs left.
    pairs: Iter<'a, [u8; SIZE_PAIR_LEN]>,
    /// The keys and values left.
    bytes: Cursor<'a>,
  },
}

impl<'a> Iterator for Entries<'a> {
  type Item = (&'a [u8], &'a [u8]);

  fn next(&mut self) -> Option<Self::Item> {
    match &mut self.walk {
      EntryWalk::Joined(entries) => entries.find_map(split_entry),
      EntryWalk::Sized { pairs, bytes } => {
        let &[key_len, value_len] = pairs.next()?;
        let key = bytes.take(usize::from(key_len), "metadata key").ok()?;
        let value = bytes.take(usize::from(value_len), "metadata value").ok()?;
        Some((key, value))
      }
    }
  }
}

/// Why a marker payload was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// No version byte.
  Empty,
  /// A version 7.2 does not define (7.2: "parsing obsolete marker: unknown version").
  UnknownVersion {
    /// The version.
    version: u8,
  },
  /// A marker cut short.
  Truncated(Truncated),
  /// A format 0 marker flagged with SHA-256 nodes, which format 0 cannot carry.
  Sha256InFormat0 {
    /// The marker's offset after the version byte.
    at: u64,
  },
  /// A format 0 metadata entry with no `:`.
  MetadataEntry {
    /// The marker's offset after the version byte.
    at: u64,
  },
  /// A format 1 parent count other than 0, 1, 2 or 3.
  ParentCount {
    /// The count.
    count: u8,
    /// The marker's offset after the version byte.
    at: u64,
  },
  /// A format 1 marker whose declared size is not what it holds.
  SizeMismatch {
    /// The size it declares.
    declared: u64,
    /// The size it holds.
    actual: u64,
    /// The marker's offset after the version byte.
    at: u64,
  },
}

impl From<Truncated> for Refusal {
  fn from(truncated: Truncated) -> Self {
    Self::Truncated(truncated)
  }
}

/// Why markers could not be encoded in a format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A marker whose fields the format cannot carry.
  Unencodable {
    /// The marker's index in the list.
    marker: usize,
    /// What the format cannot carry.
    reason: &'static str,
  },
}

/// A checked marker payload.
#[derive(Clone, Copy, Debug)]
pub struct Markers<'a> {
  /// Its format.
  format: Format,
  /// The markers' bytes, after the version byte.
  body: &'a [u8],
  /// How many markers it holds.
  count: usize,
}

/// The markers of an `obsmarkers` payload or an `obsstore`, the payload checked whole first.
pub fn decode(payload: &[u8]) -> Result<Markers<'_>, Refusal> {
  let (&version, body) = payload.split_first().ok_or(Refusal::Empty)?;
  let format = match version {
    VERSION_0 => Format::V0,
    VERSION_1 => Format::V1,
    other => return Err(Refusal::UnknownVersion { version: other }),
  };
  let mut cursor = Cursor::new(body);
  let mut count: usize = 0;
  while !cursor.is_done() {
    read_marker(&mut cursor, format)?;
    count = count.saturating_add(1);
  }
  Ok(Markers {
    format,
    body,
    count,
  })
}

impl<'a> Markers<'a> {
  /// The payload's format.
  pub fn format(&self) -> Format {
    self.format
  }

  /// How many markers it holds.
  pub fn len(&self) -> usize {
    self.count
  }

  /// Whether it holds none.
  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  /// Its markers, in order.
  pub fn iter(&self) -> impl Iterator<Item = Marker<'a>> + use<'a> {
    let mut cursor = Cursor::new(self.body);
    let format = self.format;
    core::iter::from_fn(move || {
      if cursor.is_done() {
        return None;
      }
      read_marker(&mut cursor, format).ok()
    })
  }
}

/// The length of a node under `flags`.
fn node_len(flags: u16) -> usize {
  if flags & FLAG_SHA256 == 0 {
    SHA1_LEN
  } else {
    SHA256_LEN
  }
}

/// One marker at the cursor, in `format`.
fn read_marker<'a>(cursor: &mut Cursor<'a>, format: Format) -> Result<Marker<'a>, Refusal> {
  match format {
    Format::V0 => read_v0(cursor),
    Format::V1 => read_v1(cursor),
  }
}

/// One format 0 marker.
fn read_v0<'a>(cursor: &mut Cursor<'a>) -> Result<Marker<'a>, Refusal> {
  let at = to_u64(cursor.at());
  let count = usize::from(cursor.u8("successor count")?);
  let metadata_len = cursor.u32("metadata length")?;
  let flags = u16::from(cursor.u8("flags")?);
  if flags & FLAG_SHA256 != 0 {
    return Err(Refusal::Sha256InFormat0 { at });
  }
  let predecessor = cursor.take(SHA1_LEN, "predecessor")?;
  let successors = cursor.take(count.saturating_mul(SHA1_LEN), "successors")?;
  let metadata_len = usize::try_from(metadata_len).unwrap_or(usize::MAX);
  let metadata = cursor.take(metadata_len, "metadata")?;
  if !joined_entries_have_keys(metadata) {
    return Err(Refusal::MetadataEntry { at });
  }
  Ok(Marker {
    flags,
    predecessor,
    successors,
    parents: None,
    date: None,
    metadata: Metadata::Joined(metadata),
  })
}

/// Whether every entry of format 0's metadata that is not empty holds a `:`, as 7.2's `_fm0decodemeta` needs.
fn joined_entries_have_keys(metadata: &[u8]) -> bool {
  metadata
    .split(is_entry_separator)
    .all(|entry| entry.is_empty() || entry.contains(&KEY_SEPARATOR))
}

/// One format 1 marker, its declared size held to what it holds.
fn read_v1<'a>(cursor: &mut Cursor<'a>) -> Result<Marker<'a>, Refusal> {
  let start = cursor.at();
  let at = to_u64(start);
  let declared = cursor.u32("marker size")?;
  let seconds_bits = u64::from_be_bytes(cursor.array("date")?);
  let offset_minutes = i16::from_be_bytes(cursor.array("timezone")?);
  let flags = cursor.u16("flags")?;
  let successor_count = usize::from(cursor.u8("successor count")?);
  let parent_count = cursor.u8("parent count")?;
  let metadata_count = usize::from(cursor.u8("metadata count")?);
  let node_len = node_len(flags);
  let predecessor = cursor.take(node_len, "predecessor")?;
  let successors = cursor.take(successor_count.saturating_mul(node_len), "successors")?;
  let parents = match parent_count {
    NO_PARENTS => None,
    count if count <= MAX_PARENTS => {
      Some(cursor.take(usize::from(count).saturating_mul(node_len), "parents")?)
    }
    count => return Err(Refusal::ParentCount { count, at }),
  };
  let sizes = cursor.take(
    metadata_count.saturating_mul(SIZE_PAIR_LEN),
    "metadata sizes",
  )?;
  let bytes = cursor.take(sum_of(sizes), "metadata")?;
  let actual = to_u64(cursor.at().saturating_sub(start));
  if u64::from(declared) != actual {
    return Err(Refusal::SizeMismatch {
      declared: u64::from(declared),
      actual,
      at,
    });
  }
  Ok(Marker {
    flags,
    predecessor,
    successors,
    parents,
    date: Some(Date {
      seconds_bits,
      offset_minutes,
    }),
    metadata: Metadata::Sized { sizes, bytes },
  })
}

/// The sum of format 1's metadata sizes: the length of the keys and values they describe.
fn sum_of(sizes: &[u8]) -> usize {
  sizes
    .iter()
    .fold(0, |total: usize, size| total.saturating_add(usize::from(*size)))
}

/// The length of a payload of `markers` in `format`, for a caller that charges it before encoding; refused as
/// [`encode`] refuses.
pub fn encoded_len(format: Format, markers: &[Marker<'_>]) -> Result<u64, EncodeRefusal> {
  check(format, markers)?;
  let mut measure = Measure(0);
  write(format, markers, &mut measure);
  Ok(measure.0)
}

/// Appends a payload of `markers` in `format`: the version byte, then each marker, after checking every marker can be
/// written in that format; nothing is written on a refusal.
pub fn encode(
  format: Format,
  markers: &[Marker<'_>],
  out: &mut Vec<u8>,
) -> Result<(), EncodeRefusal> {
  check(format, markers)?;
  write(format, markers, out);
  Ok(())
}

/// Refuses the first marker `format` cannot carry, naming it.
fn check(format: Format, markers: &[Marker<'_>]) -> Result<(), EncodeRefusal> {
  for (index, marker) in markers.iter().enumerate() {
    match format {
      Format::V0 => check_v0(marker),
      Format::V1 => check_v1(marker),
    }
    .map_err(|reason| EncodeRefusal::Unencodable {
      marker: index,
      reason,
    })?;
  }
  Ok(())
}

/// Writes the version byte and markers [`check`] passed.
fn write(format: Format, markers: &[Marker<'_>], out: &mut impl Out) {
  let version = match format {
    Format::V0 => VERSION_0,
    Format::V1 => VERSION_1,
  };
  out.put(&[version]);
  for marker in markers {
    match format {
      Format::V0 => write_v0(marker, out),
      Format::V1 => write_v1(marker, out),
    }
  }
}

/// Fails with `reason` unless `holds`.
fn require(holds: bool, reason: &'static str) -> Result<(), &'static str> {
  if holds { Ok(()) } else { Err(reason) }
}

/// What format 0 can carry.
fn check_v0(marker: &Marker<'_>) -> Result<(), &'static str> {
  let Metadata::Joined(metadata) = marker.metadata else {
    return Err("format 0 metadata is joined key:value entries");
  };
  require(
    marker.date.is_none() && marker.parents.is_none(),
    "format 0 keeps the date and parents in its metadata",
  )?;
  require(
    u8::try_from(marker.flags).is_ok() && marker.flags & FLAG_SHA256 == 0,
    "format 0 flags are a byte, never SHA-256",
  )?;
  require(
    marker.predecessor.len() == SHA1_LEN,
    "format 0 nodes are 20 bytes",
  )?;
  require(
    marker.successor_nodes().remainder().is_empty(),
    "format 0 successors are whole nodes",
  )?;
  require(
    u8::try_from(marker.successor_nodes().len()).is_ok(),
    "format 0 counts at most 255 successors",
  )?;
  require(
    joined_entries_have_keys(metadata),
    "format 0 metadata entries are key:value",
  )?;
  require(
    u32::try_from(metadata.len()).is_ok(),
    "format 0 metadata fits a 32-bit length",
  )
}

/// What format 1 can carry.
fn check_v1(marker: &Marker<'_>) -> Result<(), &'static str> {
  let Metadata::Sized { sizes, bytes } = marker.metadata else {
    return Err("format 1 metadata is sized entries");
  };
  require(marker.date.is_some(), "format 1 carries its date")?;
  require(
    marker.predecessor.len() == marker.node_len(),
    "the predecessor is a node of the flags' length",
  )?;
  require(
    marker.successor_nodes().remainder().is_empty(),
    "format 1 successors are whole nodes",
  )?;
  require(
    u8::try_from(marker.successor_nodes().len()).is_ok(),
    "format 1 counts at most 255 successors",
  )?;
  check_v1_parents(marker)?;
  require(
    sizes.len().is_multiple_of(SIZE_PAIR_LEN)
      && u8::try_from(sizes.len() / SIZE_PAIR_LEN).is_ok(),
    "metadata sizes are byte pairs, at most 255",
  )?;
  require(
    sum_of(sizes) == bytes.len(),
    "metadata sizes add up to its bytes",
  )?;
  require(
    u32::try_from(v1_len(marker)).is_ok(),
    "the marker fits a 32-bit size",
  )
}

/// What format 1 can carry of a marker's parents: none recorded, or at most two whole nodes.
fn check_v1_parents(marker: &Marker<'_>) -> Result<(), &'static str> {
  let Some(parents) = marker.parent_nodes() else {
    return Ok(());
  };
  require(
    parents.remainder().is_empty(),
    "parents are whole nodes",
  )?;
  require(
    parents.len() <= usize::from(MAX_PARENTS),
    "a marker records at most two parents",
  )
}

/// A format 1 marker's total size: its fixed part, its nodes, its metadata.
fn v1_len(marker: &Marker<'_>) -> usize {
  let (sizes, bytes) = match marker.metadata {
    Metadata::Sized { sizes, bytes } => (sizes.len(), bytes.len()),
    Metadata::Joined(_) => (0, 0),
  };
  [
    V1_FIXED_LEN,
    marker.predecessor.len(),
    marker.successors.len(),
    marker.parents.map_or(0, <[u8]>::len),
    sizes,
    bytes,
  ]
  .iter()
  .fold(0, |total: usize, part| total.saturating_add(*part))
}

/// Writes a format 0 marker that [`check_v0`] passed.
fn write_v0(marker: &Marker<'_>, out: &mut impl Out) {
  let metadata = match marker.metadata {
    Metadata::Joined(metadata) => metadata,
    Metadata::Sized { .. } => &[],
  };
  out.put(&[u8::try_from(marker.successor_nodes().len()).unwrap_or(u8::MAX)]);
  out.put(
    &u32::try_from(metadata.len())
      .unwrap_or(u32::MAX)
      .to_be_bytes(),
  );
  out.put(&[u8::try_from(marker.flags).unwrap_or(u8::MAX)]);
  out.put(marker.predecessor);
  out.put(marker.successors);
  out.put(metadata);
}

/// Writes a format 1 marker that [`check_v1`] passed.
fn write_v1(marker: &Marker<'_>, out: &mut impl Out) {
  let (sizes, bytes) = match marker.metadata {
    Metadata::Sized { sizes, bytes } => (sizes, bytes),
    Metadata::Joined(_) => (&[][..], &[][..]),
  };
  let date = marker.date.unwrap_or(Date {
    seconds_bits: 0,
    offset_minutes: 0,
  });
  let parents = marker.parent_nodes().map_or(NO_PARENTS, |nodes| {
    u8::try_from(nodes.len()).unwrap_or(NO_PARENTS)
  });
  out.put(
    &u32::try_from(v1_len(marker))
      .unwrap_or(u32::MAX)
      .to_be_bytes(),
  );
  out.put(&date.seconds_bits.to_be_bytes());
  out.put(&date.offset_minutes.to_be_bytes());
  out.put(&marker.flags.to_be_bytes());
  out.put(&[
    u8::try_from(marker.successor_nodes().len()).unwrap_or(u8::MAX),
    parents,
    u8::try_from(sizes.len() / SIZE_PAIR_LEN).unwrap_or(u8::MAX),
  ]);
  out.put(marker.predecessor);
  out.put(marker.successors);
  out.put(marker.parents.unwrap_or_default());
  out.put(sizes);
  out.put(bytes);
}

#[cfg(test)]
mod tests {
  use super::{EncodeRefusal, Format, Marker, Metadata, Refusal, decode, encode, encoded_len};
  use proptest::prelude::*;

  /// One format 1 marker's bytes, field by field, so a test can put any value in any field.
  fn v1(size: u32, parents: u8, tail: &[u8]) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&0_u64.to_be_bytes());
    out.extend_from_slice(&0_i16.to_be_bytes());
    out.extend_from_slice(&0_u16.to_be_bytes());
    out.extend_from_slice(&[0, parents, 0]);
    out.extend_from_slice(&[5; 20]);
    out.extend_from_slice(tail);
    out
  }

  /// One format 0 marker's bytes with no successors: its flags and its metadata.
  fn v0(flags: u8, metadata: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0];
    out.extend_from_slice(&u32::try_from(metadata.len()).unwrap().to_be_bytes());
    out.push(flags);
    out.extend_from_slice(&[5; 20]);
    out.extend_from_slice(metadata);
    out
  }

  /// AC-HG.9 (hostile input), A-545: do give an empty payload, a version 7.2 does not define, and a format 1 marker
  /// whose declared size is not its own (`len = u32::MAX` among them). Expect each refused by its kind, and the marker
  /// whose size is its own read.
  #[test]
  fn empty_unknown_and_missized_payloads_are_refused() {
    assert_eq!(decode(&[]).err(), Some(Refusal::Empty));
    assert_eq!(
      decode(&[2]).err(),
      Some(Refusal::UnknownVersion { version: 2 })
    );
    assert!(decode(&v1(39, 3, b"")).is_ok());
    for size in [38, 40, u32::MAX] {
      assert_eq!(
        decode(&v1(size, 3, b"")).err(),
        Some(Refusal::SizeMismatch {
          declared: u64::from(size),
          actual: 39,
          at: 0
        })
      );
    }
  }

  /// AC-HG.9 (hostile input), A-545: do give a format 1 marker with four parents, a format 0 marker flagged SHA-256,
  /// and a format 0 marker whose metadata entry has no `:`. Expect each refused by its kind, at the marker's offset.
  #[test]
  fn markers_mercurial_would_misread_are_refused() {
    assert_eq!(
      decode(&v1(39, 4, b"")).err(),
      Some(Refusal::ParentCount { count: 4, at: 0 })
    );
    assert_eq!(
      decode(&v0(2, b"")).err(),
      Some(Refusal::Sha256InFormat0 { at: 0 })
    );
    assert_eq!(
      decode(&v0(0, b"user")).err(),
      Some(Refusal::MetadataEntry { at: 0 })
    );
  }

  /// A-545: do encode a format 0 marker in format 1, and the reverse. Expect both refused naming the marker: converting
  /// between the formats is the exchange's, not the codec's.
  #[test]
  fn a_marker_is_not_written_in_the_other_format() {
    let zero = v0(0, b"");
    let markers: Vec<_> = decode(&zero).unwrap().iter().collect();
    assert!(matches!(
      encode(Format::V1, &markers, &mut Vec::new()),
      Err(EncodeRefusal::Unencodable { marker: 0, .. })
    ));
    let one = v1(39, 3, b"");
    let markers: Vec<_> = decode(&one).unwrap().iter().collect();
    assert!(encode(Format::V0, &markers, &mut Vec::new()).is_err());
  }

  /// A-545: do encode a format 0 marker whose metadata holds an entry with no `:`, and one whose entries are whole.
  /// Expect the first refused with nothing written (7.2 could not read it back), and the second written at the length
  /// `encoded_len` gave, its entries read back in order with the empty one passed over.
  #[test]
  fn format_0_metadata_is_written_only_as_entries_mercurial_reads() {
    let marker = |metadata: &'static [u8]| Marker {
      flags: 0,
      predecessor: &[5; 20],
      successors: &[],
      parents: None,
      date: None,
      metadata: Metadata::Joined(metadata),
    };
    let mut out = Vec::new();
    assert!(encode(Format::V0, &[marker(b"user")], &mut out).is_err());
    assert!(out.is_empty());
    let whole = [marker(b"date:0 0\0\0user:test")];
    encode(Format::V0, &whole, &mut out).unwrap();
    assert_eq!(encoded_len(Format::V0, &whole), Ok(u64::try_from(out.len()).unwrap()));
    let read: Vec<_> = decode(&out).unwrap().iter().next().unwrap().entries().collect();
    let expected: [(&[u8], &[u8]); 2] = [(b"date", b"0 0"), (b"user", b"test")];
    assert_eq!(read, expected);
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// AC-HG.9, A-545: do read arbitrary bytes after each version byte as markers. Expect markers that encode back to
    /// exactly the bytes read, at the length `encoded_len` gives, or a refusal; never a panic.
    #[test]
    fn arbitrary_payloads_read_exactly_or_are_refused(
      version in 0_u8..3,
      bytes in proptest::collection::vec(any::<u8>(), 0..160),
    ) {
      let mut payload = vec![version];
      payload.extend_from_slice(&bytes);
      if let Ok(markers) = decode(&payload) {
        let list: Vec<_> = markers.iter().collect();
        prop_assert_eq!(list.len(), markers.len());
        let mut out = Vec::new();
        encode(markers.format(), &list, &mut out).unwrap();
        prop_assert_eq!(encoded_len(markers.format(), &list), Ok(u64::try_from(out.len()).unwrap()));
        prop_assert_eq!(out, payload);
      }
    }
  }
}
