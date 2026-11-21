//! A changeset's `Texts` section, and the one edit form a stored text is made with (A-645's derived per-path read form
//! on A-767; the interface agreed with w-hg and w-store, inbox `*-per-path-read-2026-11-20`).
//!
//! **What it holds.** One entry per path the changeset touched, in the work's order: what the landing already knows of
//! each version it kept, so no read rebuilds a text to name it (w-hg's ask: the intake rebuilds every text to check it,
//! so its identity is written down then, never derived again). Each entry: the path's bytes (a tool's own path, bytes as
//! Mercurial and git keep them; a tool's own logs ride as paths no tracked file can have, Mercurial's `\0changelog` and
//! `\0manifest`); the tool's id of the version (Mercurial's node, git's blob id, Subversion's node-revision; may be
//! empty); the full text's identity (BLAKE3 of its bytes) and length; the full text its edit is against, absent for a
//! whole; and the edit's or whole's place in the landed changeset: a payload section's identity, an offset into it and a
//! length, read with `Store::piece`.
//!
//! **The edit form**, one for every tool: Mercurial's hunk list (`mdiff`/`mpatch`), each hunk a big-endian start, end and
//! length, then that many bytes, replacing the base's bytes `start..end`, the hunks in order and not overlapping. It is
//! the simplest edit that needs no source windows, and git's copy-and-insert deltas and Subversion's svndiff windows
//! convert to it without a diff. [`apply_into`] makes a text of a base and an edit.
//!
//! **The encoding** (all integers little-endian, as the log's): the entry count (`u64`), then each entry: the path and
//! the tool id, each a `u64` length and its bytes; the text's identity (32) and length (`u64`); a mark byte (0 absent, 1
//! present) and the base's identity (32) when present; the place's payload identity (32), offset and length (`u64`
//! each). Decoding refuses, typed ([`TextsRefused`]), never a panic: every count and length checked against the bytes
//! that remain before anything is allocated, a mark of no kind, and bytes after the last entry.

use std::fmt;

use crate::staging::Identity;

/// Format: the bytes of a count, a length or an offset.
const LONG: usize = 8;
/// Format: the bytes of an identity.
const IDENTITY: usize = 32;
/// Format: the mark of an absent base.
const ABSENT: u8 = 0;
/// Format: the mark of a present base.
const PRESENT: u8 = 1;
/// Format: the fewest bytes an entry takes: the path's and the tool id's lengths, the identity and length, the mark,
/// and the place.
const ENTRY_FEWEST: usize = LONG + LONG + IDENTITY + LONG + 1 + IDENTITY + LONG + LONG;
/// Format: a hunk's head: start, end and length, each a big-endian `u32` (Mercurial's `mpatch`).
const HUNK_HEAD: usize = 12;

/// Where an edit or a whole lies in a landed changeset: a payload section by its identity, and a range of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
  /// The payload section's identity, as `Store::piece` reads it.
  pub payload: Identity,
  /// The first byte in that section.
  pub offset: u64,
  /// The bytes.
  pub len: u64,
}

/// One version a changeset kept of one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEntry {
  /// The path, as its tool keeps it.
  pub path: Vec<u8>,
  /// The tool's id of the version; may be empty.
  pub tool_id: Vec<u8>,
  /// The full text's identity.
  pub text: Identity,
  /// The full text's length.
  pub len: u64,
  /// The full text the edit is against; none for a whole.
  pub base: Option<Identity>,
  /// Where the edit or the whole lies.
  pub place: Place,
}

/// Why a `Texts` section, or an edit, was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextsRefused {
  /// The bytes end before what they declare.
  Truncated,
  /// A count or a length past what the bytes that remain could hold.
  Count,
  /// A mark of no kind.
  Mark(u8),
  /// Bytes after the last entry.
  Trailing,
  /// An edit's hunk out of order, past its base, or whose bytes run past the edit.
  Hunk,
  /// An edit that makes a text of another length than its entry names.
  Length,
}

impl fmt::Display for TextsRefused {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Truncated => f.write_str("a texts section ends before what it declares"),
      Self::Count => f.write_str("a texts section's count or length past its bytes"),
      Self::Mark(byte) => write!(f, "a texts section's mark {byte} of no kind"),
      Self::Trailing => f.write_str("bytes after a texts section's last entry"),
      Self::Hunk => f.write_str("an edit's hunk out of order or past its base"),
      Self::Length => f.write_str("an edit that makes a text of another length"),
    }
  }
}

impl std::error::Error for TextsRefused {}

/// The section's bytes of `entries`, in order.
pub fn encode(entries: &[TextEntry]) -> Vec<u8> {
  let len = entries.iter().fold(LONG, |sum, entry| {
    sum
      .saturating_add(ENTRY_FEWEST)
      .saturating_add(entry.path.len())
      .saturating_add(entry.tool_id.len())
      .saturating_add(if entry.base.is_some() { IDENTITY } else { 0 })
  });
  let mut out = Vec::with_capacity(len);
  put_long(&mut out, to_u64(entries.len()));
  for entry in entries {
    put_bytes(&mut out, &entry.path);
    put_bytes(&mut out, &entry.tool_id);
    out.extend_from_slice(&entry.text);
    put_long(&mut out, entry.len);
    match &entry.base {
      Some(base) => {
        out.push(PRESENT);
        out.extend_from_slice(base);
      }
      None => out.push(ABSENT),
    }
    out.extend_from_slice(&entry.place.payload);
    put_long(&mut out, entry.place.offset);
    put_long(&mut out, entry.place.len);
  }
  out
}

/// The entries `bytes` hold.
///
/// # Errors
/// Each [`TextsRefused`] of the encoding, by kind (the module docs).
pub fn decode(bytes: &[u8]) -> Result<Vec<TextEntry>, TextsRefused> {
  let mut reader = Reader { rest: bytes };
  let count = usize::try_from(reader.long()?).map_err(|_| TextsRefused::Count)?;
  if count
    .checked_mul(ENTRY_FEWEST)
    .is_none_or(|needed| needed > reader.rest.len())
  {
    return Err(TextsRefused::Count);
  }
  let mut entries = Vec::with_capacity(count);
  for _ in 0..count {
    let path = reader.bytes()?.to_vec();
    let tool_id = reader.bytes()?.to_vec();
    let text = reader.identity()?;
    let len = reader.long()?;
    let base = match reader.byte()? {
      ABSENT => None,
      PRESENT => Some(reader.identity()?),
      byte => return Err(TextsRefused::Mark(byte)),
    };
    let place = Place {
      payload: reader.identity()?,
      offset: reader.long()?,
      len: reader.long()?,
    };
    entries.push(TextEntry {
      path,
      tool_id,
      text,
      len,
      base,
      place,
    });
  }
  if !reader.rest.is_empty() {
    return Err(TextsRefused::Trailing);
  }
  Ok(entries)
}

/// The text `edit` (a hunk list) makes of `base`, written over `out`: emptied, then filled, grown only where its
/// capacity falls short. `len` is the length the text must have, checked before a byte is written.
///
/// # Errors
/// [`TextsRefused::Hunk`] for a hunk out of order, past the base, or whose bytes run past the edit;
/// [`TextsRefused::Length`] for an edit that makes another length than `len`. `out` then holds nothing of the text.
pub fn apply_into(
  base: &[u8],
  edit: &[u8],
  len: u64,
  out: &mut Vec<u8>,
) -> Result<(), TextsRefused> {
  // The length first, from the hunks alone, so nothing is written for an edit that does not hold together.
  let mut made = 0u64;
  let mut last = 0u64;
  let mut rest = edit;
  while !rest.is_empty() {
    let (start, end, data, after) = hunk(rest)?;
    if start < last || end < start || end > to_u64(base.len()) {
      return Err(TextsRefused::Hunk);
    }
    made = made
      .checked_add(start.checked_sub(last).ok_or(TextsRefused::Hunk)?)
      .and_then(|made| made.checked_add(to_u64(data.len())))
      .ok_or(TextsRefused::Hunk)?;
    last = end;
    rest = after;
  }
  made = made
    .checked_add(
      to_u64(base.len())
        .checked_sub(last)
        .ok_or(TextsRefused::Hunk)?,
    )
    .ok_or(TextsRefused::Hunk)?;
  if made != len {
    return Err(TextsRefused::Length);
  }
  out.clear();
  out.reserve_exact(usize::try_from(len).map_err(|_| TextsRefused::Length)?);
  let mut last = 0usize;
  let mut rest = edit;
  while !rest.is_empty() {
    let (start, end, data, after) = hunk(rest)?;
    let (start, end) = (index(start)?, index(end)?);
    out.extend_from_slice(base.get(last..start).ok_or(TextsRefused::Hunk)?);
    out.extend_from_slice(data);
    last = end;
    rest = after;
  }
  out.extend_from_slice(base.get(last..).ok_or(TextsRefused::Hunk)?);
  Ok(())
}

/// The next hunk of `edit`: its start, end and bytes, and what follows it.
fn hunk(edit: &[u8]) -> Result<(u64, u64, &[u8], &[u8]), TextsRefused> {
  let (head, rest) = edit.split_at_checked(HUNK_HEAD).ok_or(TextsRefused::Hunk)?;
  let field = |at: usize| -> Result<u64, TextsRefused> {
    let bytes: [u8; 4] = head
      .get(at..at.saturating_add(4))
      .and_then(|bytes| bytes.try_into().ok())
      .ok_or(TextsRefused::Hunk)?;
    Ok(u64::from(u32::from_be_bytes(bytes)))
  };
  let (start, end, len) = (field(0)?, field(4)?, field(8)?);
  let (data, after) = rest
    .split_at_checked(index(len)?)
    .ok_or(TextsRefused::Hunk)?;
  Ok((start, end, data, after))
}

/// A hunk's position as an index.
fn index(value: u64) -> Result<usize, TextsRefused> {
  usize::try_from(value).map_err(|_| TextsRefused::Hunk)
}

/// A length as the encoding's `u64`.
fn to_u64(len: usize) -> u64 {
  u64::try_from(len).unwrap_or(u64::MAX)
}

/// Appends a `u64`.
fn put_long(out: &mut Vec<u8>, value: u64) {
  out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a length and its bytes.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
  put_long(out, to_u64(bytes.len()));
  out.extend_from_slice(bytes);
}

/// The section's bytes, read from the front.
struct Reader<'b> {
  rest: &'b [u8],
}

impl<'b> Reader<'b> {
  fn take(&mut self, count: usize) -> Result<&'b [u8], TextsRefused> {
    let (taken, rest) = self
      .rest
      .split_at_checked(count)
      .ok_or(TextsRefused::Truncated)?;
    self.rest = rest;
    Ok(taken)
  }

  fn byte(&mut self) -> Result<u8, TextsRefused> {
    self
      .take(1)?
      .first()
      .copied()
      .ok_or(TextsRefused::Truncated)
  }

  fn long(&mut self) -> Result<u64, TextsRefused> {
    let bytes: [u8; LONG] = self
      .take(LONG)?
      .try_into()
      .map_err(|_| TextsRefused::Truncated)?;
    Ok(u64::from_le_bytes(bytes))
  }

  fn identity(&mut self) -> Result<Identity, TextsRefused> {
    self
      .take(IDENTITY)?
      .try_into()
      .map_err(|_| TextsRefused::Truncated)
  }

  fn bytes(&mut self) -> Result<&'b [u8], TextsRefused> {
    let len = usize::try_from(self.long()?).map_err(|_| TextsRefused::Count)?;
    if len > self.rest.len() {
      return Err(TextsRefused::Count);
    }
    self.take(len)
  }
}

#[cfg(test)]
mod tests;
