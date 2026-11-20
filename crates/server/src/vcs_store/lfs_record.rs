//! A Git LFS object's record as its entry holds it (A-629 part 3; §4.17.2 "Large files", §4.17.1 "Durability and
//! residency"): what a recovery replays into the repository's LFS index (`slates_git::lfs::objects::LfsObjects`) to
//! make it again, and nothing derived.
//!
//! **What it holds.** The object's oid, the SHA-256 of its bytes that LFS names it by, and its bytes. The index's row,
//! the content's windows and its BLAKE3 identity are computed from these again and never recorded: the replay admits
//! the bytes through the path the upload took (`super::replay_lfs`), so bytes that do not hash to the oid are refused
//! there, typed, and the repository is named unrecovered rather than served wrong.
//!
//! **One record an upload.** An upload's entry holds its object's record alone ([`encode`]): an LFS object is no git
//! object and moves no green, so it never shares an entry with a push. A git repository's entries hold either record,
//! told apart by their first word ([`is_lfs`]): a git push's record begins with its own magic (`super::record`), so
//! neither is ever read as the other.
//!
//! **The encoding**, little-endian as the push's record is: the magic and the version; the oid's 32 bytes; the
//! object's length and its bytes. The length is checked against the bytes that remain before they are read, and bytes
//! after the object are refused, each typed ([`LfsRecordRefused`]), never a panic.

use slates_git::lfs::Oid;

/// Format: the record's magic, "SGLF" (a slates git LFS object) little-endian; a git push's record begins "SGPR".
const MAGIC: u32 = u32::from_le_bytes(*b"SGLF");

/// Format: the record's version; a decoder refuses any other.
const VERSION: u32 = 1;

/// Format: the bytes of a word: the magic, the version.
const WORD_BYTES: usize = size_of::<u32>();

/// Format: the bytes of the object's length.
const LONG_BYTES: usize = size_of::<u64>();

/// Format: the bytes of an oid, a SHA-256 (git-lfs `docs/spec.md`: the oid is the SHA-256 of the content).
const OID_BYTES: usize = 32;

/// Format: the bytes before the object's own: the magic, the version, the oid and the length.
const HEAD_BYTES: usize = WORD_BYTES + WORD_BYTES + OID_BYTES + LONG_BYTES;

/// Why a record was refused. A refused record replays nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LfsRecordRefused {
  /// The bytes end before what they declare.
  Truncated,
  /// Bytes of another kind: the magic is not an LFS object's record's.
  Magic,
  /// A version this build does not read.
  Version {
    /// The version read.
    version: u32,
  },
  /// A length this machine cannot address.
  Length,
  /// Bytes after the object.
  Trailing,
  /// The record's bytes could not be had: memory refused them.
  Memory,
}

/// A decoded record: the object's oid and its bytes, borrowed from the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LfsRecord<'b> {
  /// The object's oid.
  pub(crate) oid: Oid,
  /// Its bytes.
  pub(crate) bytes: &'b [u8],
}

/// Whether `bytes` are an LFS object's record by their first word (module docs, "One record an upload").
pub(crate) fn is_lfs(bytes: &[u8]) -> bool {
  bytes.get(..WORD_BYTES) == Some(MAGIC.to_le_bytes().as_slice())
}

/// The record of the object `oid` whose bytes are `bytes` (module docs, "The encoding"); refused only where memory for
/// it cannot be had.
pub(crate) fn encode(oid: &Oid, bytes: &[u8]) -> Result<Vec<u8>, LfsRecordRefused> {
  let len = u64::try_from(bytes.len()).map_err(|_| LfsRecordRefused::Length)?;
  let total = HEAD_BYTES
    .checked_add(bytes.len())
    .ok_or(LfsRecordRefused::Length)?;
  let mut record = Vec::new();
  record
    .try_reserve_exact(total)
    .map_err(|_| LfsRecordRefused::Memory)?;
  record.extend_from_slice(&MAGIC.to_le_bytes());
  record.extend_from_slice(&VERSION.to_le_bytes());
  record.extend_from_slice(oid.as_bytes());
  record.extend_from_slice(&len.to_le_bytes());
  record.extend_from_slice(bytes);
  Ok(record)
}

/// Reads `bytes` as a record (module docs, "The encoding"), each malformation refused typed.
pub(crate) fn decode(bytes: &[u8]) -> Result<LfsRecord<'_>, LfsRecordRefused> {
  let (magic, rest) = word(bytes)?;
  if magic != MAGIC {
    return Err(LfsRecordRefused::Magic);
  }
  let (version, rest) = word(rest)?;
  if version != VERSION {
    return Err(LfsRecordRefused::Version { version });
  }
  let (oid, rest) = rest
    .split_first_chunk::<OID_BYTES>()
    .ok_or(LfsRecordRefused::Truncated)?;
  let (len, rest) = rest
    .split_first_chunk::<LONG_BYTES>()
    .ok_or(LfsRecordRefused::Truncated)?;
  let len = usize::try_from(u64::from_le_bytes(*len)).map_err(|_| LfsRecordRefused::Length)?;
  let object = rest.get(..len).ok_or(LfsRecordRefused::Truncated)?;
  if rest.len() != len {
    return Err(LfsRecordRefused::Trailing);
  }
  Ok(LfsRecord {
    oid: Oid::from_bytes(*oid),
    bytes: object,
  })
}

/// The first word of `bytes` and the rest.
fn word(bytes: &[u8]) -> Result<(u32, &[u8]), LfsRecordRefused> {
  let (word, rest) = bytes
    .split_first_chunk::<WORD_BYTES>()
    .ok_or(LfsRecordRefused::Truncated)?;
  Ok((u32::from_le_bytes(*word), rest))
}

#[cfg(test)]
#[path = "lfs_record/tests.rs"]
mod tests;
