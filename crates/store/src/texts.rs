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

use crate::staging::{Identity, Staging};
use crate::store::{Store, StoreError};

/// Format: the bytes of a count, a length or an offset.
const LONG: usize = 8;
/// Format: the bytes of an identity.
const IDENTITY: usize = 32;
/// Format: the mark of an absent base.
const ABSENT: u8 = 0;
/// Format: the mark of a present base.
const PRESENT: u8 = 1;
/// Shape: the most a coalesced read of places reads, as a multiple of the bytes its places ask ([`Store::places_bytes`]):
/// the gaps between them (in a changegroup, each chunk's header before its delta) at most as many bytes as the places,
/// so one read stands for many and never reads more than double what was asked.
const COALESCED_SPAN_TIMES: u64 = 2;
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

/// A hunk read from an edit: its start, end and bytes, and the edit's bytes after it.
type HunkParts<'e> = (u64, u64, &'e [u8], &'e [u8]);

/// The next hunk of `edit`: its start, end and bytes, and what follows it.
fn hunk(edit: &[u8]) -> Result<HunkParts<'_>, TextsRefused> {
  let (head, rest) = edit.split_at_checked(HUNK_HEAD).ok_or(TextsRefused::Hunk)?;
  // Derived: a hunk's head is three big-endian u32 words (start, end, length), each a u32's width in turn.
  const WORD: usize = size_of::<u32>();
  let field = |n: usize| -> Result<u64, TextsRefused> {
    let at = n.saturating_mul(WORD);
    let bytes: [u8; WORD] = head
      .get(at..at.saturating_add(WORD))
      .and_then(|bytes| bytes.try_into().ok())
      .ok_or(TextsRefused::Hunk)?;
    Ok(u64::from(u32::from_be_bytes(bytes)))
  };
  let (start, end, len) = (field(0)?, field(1)?, field(2)?);
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

/// Format: the text table's tag: a repository's text by its identity to how it is made.
const TEXTS: u8 = b'T';
/// Format: the path history table's tag: a repository's path (by BLAKE3 of its bytes) and a sequence to the text it kept.
const HISTORY: u8 = b'H';
/// Format: the bytes of a repository's id (A-515's 128 bits, big-endian in a key so a repository's rows are one range).
const REPOSITORY: usize = 16;
/// Format: a text row's key: the tag, the repository, the text's identity.
const TEXT_KEY_BYTES: usize = 1 + REPOSITORY + IDENTITY;
/// Format: a text row's value: the sequence, the length, the base's mark and identity, the place.
const TEXT_VALUE_BYTES: usize = LONG + LONG + 1 + IDENTITY + IDENTITY + LONG + LONG;
/// Format: the bytes of an entry's place in its changeset's `Texts` section, a big-endian `u32` in a key (a section holds
/// at most `u32::MAX` entries, refused past it).
const ORDINAL: usize = 4;
/// Format: a history row's key: the tag, the repository, the path's BLAKE3, the sequence and the entry's place in the
/// changeset's section (each big-endian, so in order: a push's several versions of one path are each a row).
const HISTORY_KEY_BYTES: usize = 1 + REPOSITORY + IDENTITY + LONG + ORDINAL;
/// Format: the tool id table's tag: a repository's version of a path by its tool's id (Mercurial's node), to the version
/// as it landed: its text's identity and its own row (its base is a version of the same path, as a revlog's is).
const TOOLS: u8 = b'I';
/// Format: a tool id row's key before the id: the tag, the repository, the path's BLAKE3 (the id's bytes follow).
const TOOL_KEY_FIXED: usize = 1 + REPOSITORY + IDENTITY;

/// How a landed text is made ([`Store::text_row`]): the changeset that kept it, its length, its edit's base (none for a
/// whole), and where the edit or the whole lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextRow {
  /// The changeset's sequence in its repository.
  pub sequence: u64,
  /// The full text's length.
  pub len: u64,
  /// The full text the edit is against; none for a whole.
  pub base: Option<Identity>,
  /// Where the edit or the whole lies.
  pub place: Place,
}

/// Whole texts made from their chains, by identity, oldest out first, bounded in bytes: the bound a caller starts it
/// with, grown to the bytes of the longest chain a read has had to make (a learned size, Ada's sizing rule: a read needs
/// its chain's texts at once, so no smaller bound serves it); and what the reads did, the red test's non-vacuity
/// counters.
#[derive(Debug)]
pub struct TextCache {
  /// The texts held.
  held: std::collections::HashMap<Identity, Vec<u8>>,
  /// Their order of arrival, oldest first.
  order: std::collections::VecDeque<Identity>,
  /// The bytes they hold.
  bytes: u64,
  /// The most bytes they may hold.
  bound: u64,
  /// Texts made from their chains.
  pub made: u64,
  /// Edits applied.
  pub edits: u64,
  /// Places read from the log or the staging.
  pub places: u64,
  /// The changesets those places lie in, each counted at each read ([`TextCache::changesets_read`]).
  pub changesets: std::collections::BTreeSet<u64>,
}

impl TextCache {
  /// A cache holding at most `bound` bytes of texts.
  pub fn new(bound: u64) -> Self {
    Self {
      held: std::collections::HashMap::new(),
      order: std::collections::VecDeque::new(),
      bytes: 0,
      bound,
      made: 0,
      edits: 0,
      places: 0,
      changesets: std::collections::BTreeSet::new(),
    }
  }

  /// The text held under `identity`, if it is.
  pub fn get(&self, identity: &Identity) -> Option<&[u8]> {
    self.held.get(identity).map(Vec::as_slice)
  }

  /// Keeps `text` under `identity`, the oldest given up until it fits; a text past the whole bound is handed back.
  fn keep(&mut self, identity: Identity, text: Vec<u8>) -> Option<Vec<u8>> {
    let len = to_u64(text.len());
    if len > self.bound {
      return Some(text);
    }
    if self.held.contains_key(&identity) {
      return None;
    }
    while self.bytes.saturating_add(len) > self.bound {
      let Some(oldest) = self.order.pop_front() else {
        break;
      };
      if let Some(given) = self.held.remove(&oldest) {
        self.bytes = self.bytes.saturating_sub(to_u64(given.len()));
      }
    }
    self.bytes = self.bytes.saturating_add(len);
    self.order.push_back(identity);
    self.held.insert(identity, text);
    None
  }
}

/// Puts the `T` and `H` rows of a landed changeset's `Texts` section `bytes` (A-645's derived per-path read form): at
/// its landing, and again at a recovery's replay, as the `P` rows are.
///
/// # Errors
/// [`StoreError::BadRow`] for a section that does not decode; the engine's refusals.
pub(crate) fn put_rows<F: hyper_block::block::BlockFile>(
  rows: &mut mantle_engine::shard_db::ShardDb<F>,
  header: &crate::log::Header,
  bytes: &[u8],
) -> Result<(), StoreError> {
  let entries = decode(bytes).map_err(|_| StoreError::BadRow)?;
  let mut value = Vec::with_capacity(TEXT_VALUE_BYTES);
  let mut held = Vec::with_capacity(TEXT_VALUE_BYTES);
  for (ordinal, entry) in entries.into_iter().enumerate() {
    let ordinal = u32::try_from(ordinal).map_err(|_| StoreError::BadRow)?;
    rows.put(
      &history_key(
        header.repository,
        blake3::hash(&entry.path).as_bytes(),
        (header.sequence, ordinal),
      ),
      &entry.text,
    )?;
    // The version of its path by its tool's id, as it landed (its own base and place, never another path's: a text
    // two paths share keeps one `T` row, whose base may be a version of the other), the first landing's.
    if !entry.tool_id.is_empty() {
      let key = tool_key(
        header.repository,
        blake3::hash(&entry.path).as_bytes(),
        &entry.tool_id,
      );
      if !rows.get(&key, &mut held)? {
        value.clear();
        value.extend_from_slice(&entry.text);
        put_row_value(&mut value, header.sequence, &entry);
        rows.put(&key, &value)?;
      }
    }
    // A text's row is the first changeset's that kept it, never replaced: a text that comes back (a file reverted, a
    // copy, a version with its base's bytes) is made as it first was, so every base names an older row and no chain
    // loops (A, then B against A, then A against B would otherwise make A and B each other's base).
    if rows.get(&text_key(header.repository, &entry.text), &mut held)? {
      continue;
    }
    value.clear();
    put_row_value(&mut value, header.sequence, &entry);
    rows.put(&text_key(header.repository, &entry.text), &value)?;
  }
  Ok(())
}

/// Appends a text row's value of `entry`, landed at `sequence`: the sequence, the length, the base's mark and identity,
/// the place ([`TEXT_VALUE_BYTES`]).
fn put_row_value(value: &mut Vec<u8>, sequence: u64, entry: &TextEntry) {
  value.extend_from_slice(&sequence.to_le_bytes());
  value.extend_from_slice(&entry.len.to_le_bytes());
  match entry.base {
    Some(base) => {
      value.push(PRESENT);
      value.extend_from_slice(&base);
    }
    None => {
      value.push(ABSENT);
      value.extend_from_slice(&[0; IDENTITY]);
    }
  }
  value.extend_from_slice(&entry.place.payload);
  value.extend_from_slice(&entry.place.offset.to_le_bytes());
  value.extend_from_slice(&entry.place.len.to_le_bytes());
}

/// A text row's key.
fn text_key(repository: u128, text: &Identity) -> [u8; TEXT_KEY_BYTES] {
  let mut key = [0u8; TEXT_KEY_BYTES];
  let (tag, rest) = key.split_at_mut(1);
  if let Some(tag) = tag.first_mut() {
    *tag = TEXTS;
  }
  let (repo, identity) = rest.split_at_mut(REPOSITORY);
  repo.copy_from_slice(&repository.to_be_bytes());
  identity.copy_from_slice(text);
  key
}

/// A history row's key.
fn history_key(
  repository: u128,
  path: &Identity,
  (sequence, ordinal): (u64, u32),
) -> [u8; HISTORY_KEY_BYTES] {
  let mut key = [0u8; HISTORY_KEY_BYTES];
  let (tag, rest) = key.split_at_mut(1);
  if let Some(tag) = tag.first_mut() {
    *tag = HISTORY;
  }
  let (repo, rest) = rest.split_at_mut(REPOSITORY);
  repo.copy_from_slice(&repository.to_be_bytes());
  let (hash, rest) = rest.split_at_mut(IDENTITY);
  hash.copy_from_slice(path);
  let (seq, place) = rest.split_at_mut(LONG);
  seq.copy_from_slice(&sequence.to_be_bytes());
  place.copy_from_slice(&ordinal.to_be_bytes());
  key
}

/// A tool id row's key: the tag, the repository, the path's BLAKE3, the id's bytes.
fn tool_key(repository: u128, path: &Identity, tool_id: &[u8]) -> Vec<u8> {
  let mut key = Vec::with_capacity(TOOL_KEY_FIXED.saturating_add(tool_id.len()));
  key.push(TOOLS);
  key.extend_from_slice(&repository.to_be_bytes());
  key.extend_from_slice(path);
  key.extend_from_slice(tool_id);
  key
}

/// The first key past every key that begins with `prefix`: its last byte below 0xFF raised by one, the bytes after it
/// dropped; none where every byte is 0xFF (no key follows them all).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
  let mut end = prefix.to_vec();
  while let Some(last) = end.pop() {
    if let Some(raised) = last.checked_add(1) {
      end.push(raised);
      return Some(end);
    }
  }
  None
}

/// A tool id row's value, read: the text's identity, then its own row.
fn decode_tool_value(value: &[u8]) -> Result<(Identity, TextRow), StoreError> {
  let (identity, row) = value.split_at_checked(IDENTITY).ok_or(StoreError::BadRow)?;
  let identity: Identity = identity.try_into().map_err(|_| StoreError::BadRow)?;
  let row = decode_row(row).ok_or(StoreError::BadRow)?;
  Ok((identity, row))
}

/// A text row's value, read.
fn decode_row(value: &[u8]) -> Option<TextRow> {
  let mut reader = Reader { rest: value };
  let sequence = reader.long().ok()?;
  let len = reader.long().ok()?;
  let mark = reader.byte().ok()?;
  let base = reader.identity().ok()?;
  let place = Place {
    payload: reader.identity().ok()?,
    offset: reader.long().ok()?,
    len: reader.long().ok()?,
  };
  let base = match mark {
    ABSENT => None,
    PRESENT => Some(base),
    _ => return None,
  };
  reader.rest.is_empty().then_some(TextRow {
    sequence,
    len,
    base,
    place,
  })
}

impl Store {
  /// How the text `text` of repository `repository` is made, from its `T` row; none where no landed changeset kept it.
  ///
  /// # Errors
  /// [`StoreError::BadRow`] for a row that does not decode; the engine's refusals.
  pub fn text_row(
    &mut self,
    repository: u128,
    text: &Identity,
  ) -> Result<Option<TextRow>, StoreError> {
    let mut value = Vec::with_capacity(TEXT_VALUE_BYTES);
    if !self
      .rows_mut()
      .get(&text_key(repository, text), &mut value)?
    {
      return Ok(None);
    }
    decode_row(&value).map(Some).ok_or(StoreError::BadRow)
  }

  /// The text `text` of repository `repository` from `offset` to its end, made from its chain into `cache` where the
  /// cache lacks it: its `T` rows walked down to a whole, then each edit read from its own changeset's payload and
  /// applied, so no changeset outside the chain is read. Empty at the text's end; none past it, or for a text no landed
  /// changeset kept.
  ///
  /// # Errors
  /// [`StoreError::BadRow`] for a row, a place or an edit that does not hold together (a chain that loops, a place
  /// past its payload, an edit making another length); the engine's and the log's refusals.
  pub fn text_at<'c>(
    &mut self,
    staging: &Staging,
    repository: u128,
    text: &Identity,
    offset: u64,
    cache: &'c mut TextCache,
  ) -> Result<Option<&'c [u8]>, StoreError> {
    if cache.get(text).is_none() && !self.make(staging, repository, text, cache)? {
      return Ok(None);
    }
    // The cache's bound grew to the chain, so the text made is held.
    let Some(whole) = cache.held.get(text) else {
      return Err(StoreError::BadRow);
    };
    let start = usize::try_from(offset).map_err(|_| StoreError::BadRow)?;
    Ok(whole.get(start..))
  }

  /// A path's history in repository `repository`: each changeset that kept a version of `path` from sequence `from` on,
  /// at most `limit`, in order, with the text it kept.
  ///
  /// # Errors
  /// The engine's refusals; [`StoreError::BadRow`] for a row that does not decode.
  pub fn path_texts(
    &mut self,
    repository: u128,
    path: &[u8],
    from: u64,
    limit: usize,
  ) -> Result<Vec<(u64, Identity)>, StoreError> {
    let hash = *blake3::hash(path).as_bytes();
    let start = history_key(repository, &hash, (from, 0));
    let end = history_key(repository, &hash, (u64::MAX, u32::MAX));
    let mut out = mantle_engine::rows::Rows::default();
    let mut next = Vec::new();
    self
      .rows_mut()
      .scan(&start, Some(&end), limit, &mut out, &mut next)?;
    let mut found = Vec::with_capacity(limit.min(out.len()));
    for (key, value) in out.iter() {
      let sequence = key
        .get(
          HISTORY_KEY_BYTES.saturating_sub(LONG + ORDINAL)
            ..HISTORY_KEY_BYTES.saturating_sub(ORDINAL),
        )
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or(StoreError::BadRow)?;
      let identity: Identity = value.try_into().map_err(|_| StoreError::BadRow)?;
      found.push((sequence, identity));
    }
    Ok(found)
  }

  /// The versions of `path` in repository `repository` its tool names in `tool_ids` (Mercurial's nodes), in order, as
  /// they landed: each one's text identity and its own row (its base a version of the same path, its place its own
  /// edit's); none for one no landed changeset named.
  ///
  /// # Errors
  /// [`StoreError::BadRow`] for a row that does not decode; the engine's refusals.
  pub fn texts_by_tools(
    &mut self,
    repository: u128,
    path: &[u8],
    tool_ids: &[Vec<u8>],
  ) -> Result<Vec<Option<(Identity, TextRow)>>, StoreError> {
    // The path's tool id rows share their key's prefix, so they are read in one scan of it, a page as large as the
    // ask, keeping the ones asked: a stream asks for a log's every version, one random lookup each before (r27 on
    // hg-src: about 230,000 a stream, 1.35 s of its 3.7 s waiting on the store).
    let hash = *blake3::hash(path).as_bytes();
    let start = tool_key(repository, &hash, &[]);
    let end = prefix_end(&start);
    let wanted: std::collections::BTreeSet<&[u8]> = tool_ids.iter().map(Vec::as_slice).collect();
    let mut kept: std::collections::BTreeMap<Vec<u8>, (Identity, TextRow)> =
      std::collections::BTreeMap::new();
    let mut from = start.clone();
    let mut next = Vec::new();
    loop {
      let mut out = mantle_engine::rows::Rows::default();
      self.tool_read();
      let more = self.rows_mut().scan(
        &from,
        end.as_deref(),
        tool_ids.len().max(1),
        &mut out,
        &mut next,
      )?;
      for (key, value) in out.iter() {
        let Some(tool_id) = key.strip_prefix(start.as_slice()) else {
          continue;
        };
        if wanted.contains(tool_id) {
          kept.insert(tool_id.to_vec(), decode_tool_value(value)?);
        }
      }
      if !more || kept.len() == wanted.len() {
        break;
      }
      core::mem::swap(&mut from, &mut next);
    }
    Ok(
      tool_ids
        .iter()
        .map(|tool_id| kept.get(tool_id).copied())
        .collect(),
    )
  }

  /// The bytes at `place` in the log (an edit or a whole as its `T` row names it), read whole: what a stream sends as a
  /// revision's chunk, in place, no text made.
  ///
  /// # Errors
  /// [`StoreError::BadRow`] for a place past its payload, or a payload no landed changeset holds; the log's refusals.
  pub fn place_bytes(&mut self, staging: &Staging, place: &Place) -> Result<Vec<u8>, StoreError> {
    self
      .places_bytes(staging, core::slice::from_ref(place))?
      .pop()
      .ok_or(StoreError::BadRow)
  }

  /// The bytes at each of `places`, in order (a stream's chunks, read in place): each run of places in one landed
  /// payload, in order, read in one read of the log while that read stays within [`COALESCED_SPAN_TIMES`] the bytes the
  /// run asks, each payload's place in the log found once. A log's revisions lie in order in their push's payload,
  /// apart only by each chunk's header, so a stream's batch is a read or a few, never a read a chunk (r25 on hg-src:
  /// about 230,000 reads a stream, a later stream 10.7 s). A place whose payload is still in flight is read from the
  /// staging.
  ///
  /// # Errors
  /// [`StoreError::BadRow`] for a place past its payload, or a payload no landed changeset holds; the log's refusals.
  pub fn places_bytes(
    &mut self,
    staging: &Staging,
    places: &[Place],
  ) -> Result<Vec<Vec<u8>>, StoreError> {
    let mut out = Vec::with_capacity(places.len());
    let mut at = 0;
    while let Some(first) = places.get(at) {
      if staging.get(&first.payload).is_some() {
        let mut counted = TextCache::new(0);
        out.push(self.read_place(staging, first, &mut counted)?);
        at = at.saturating_add(1);
        continue;
      }
      let (base, payload_len) = self
        .content_place(&first.payload)?
        .ok_or(StoreError::BadRow)?;
      let span_start = first.offset;
      let mut span_end = first
        .offset
        .checked_add(first.len)
        .ok_or(StoreError::BadRow)?;
      let mut asked = first.len;
      let mut end = at.saturating_add(1);
      while let Some(next) = places.get(end) {
        let Some(next_end) = next.offset.checked_add(next.len) else {
          break;
        };
        let grown = asked.saturating_add(next.len);
        let within =
          next_end.saturating_sub(span_start) <= grown.saturating_mul(COALESCED_SPAN_TIMES);
        if next.payload != first.payload || next.offset < span_end || !within {
          break;
        }
        span_end = next_end;
        asked = grown;
        end = end.saturating_add(1);
      }
      if span_end > payload_len {
        return Err(StoreError::BadRow);
      }
      let mut span = vec![
        0u8;
        usize::try_from(span_end.saturating_sub(span_start))
          .map_err(|_| StoreError::BadRow)?
      ];
      self.log().read_at(
        &mut span,
        base.checked_add(span_start).ok_or(StoreError::BadRow)?,
      )?;
      self.place_read();
      for place in places.get(at..end).unwrap_or_default() {
        let from = usize::try_from(place.offset.saturating_sub(span_start))
          .map_err(|_| StoreError::BadRow)?;
        let len = usize::try_from(place.len).map_err(|_| StoreError::BadRow)?;
        let bytes = span
          .get(from..from.checked_add(len).ok_or(StoreError::BadRow)?)
          .ok_or(StoreError::BadRow)?;
        out.push(bytes.to_vec());
      }
      at = end;
    }
    Ok(out)
  }

  /// Makes `text` and every text of its chain the cache lacks, keeping each: whether a landed changeset kept it.
  fn make(
    &mut self,
    staging: &Staging,
    repository: u128,
    text: &Identity,
    cache: &mut TextCache,
  ) -> Result<bool, StoreError> {
    // The chain, newest first, down to a whole or a text the cache holds; a chain longer than the texts its repository
    // could hold loops, refused.
    let mut chain: Vec<(Identity, TextRow)> = Vec::new();
    let mut at = *text;
    let mut seen = std::collections::BTreeSet::new();
    loop {
      if cache.get(&at).is_some() {
        break;
      }
      if !seen.insert(at) {
        return Err(StoreError::BadRow);
      }
      let Some(row) = self.text_row(repository, &at)? else {
        if chain.is_empty() {
          return Ok(false);
        }
        return Err(StoreError::BadRow);
      };
      chain.push((at, row));
      match row.base {
        Some(base) => at = base,
        None => break,
      }
    }
    // The chain's texts are held at once while it is made: the cache's bound grows to them where it is short.
    let need = chain
      .iter()
      .fold(0u64, |need, (_, row)| need.saturating_add(row.len));
    cache.bound = cache.bound.max(need);
    let mut made: Vec<u8> = Vec::new();
    while let Some((identity, row)) = chain.pop() {
      let bytes = self.read_place(staging, &row.place, cache)?;
      cache.changesets.insert(row.sequence);
      match row.base {
        None => {
          if to_u64(bytes.len()) != row.len {
            return Err(StoreError::BadRow);
          }
          made = bytes;
        }
        Some(base) => {
          // The base from the cache, or the text made just before it where the cache could not hold that one.
          let mut out = Vec::new();
          let base_bytes = cache.held.get(&base).map_or(made.as_slice(), Vec::as_slice);
          apply_into(base_bytes, &bytes, row.len, &mut out).map_err(|_| StoreError::BadRow)?;
          cache.edits = cache.edits.saturating_add(1);
          made = out;
        }
      }
      cache.made = cache.made.saturating_add(1);
      made = cache.keep(identity, made).unwrap_or_default();
    }
    Ok(true)
  }

  /// The bytes at `place`: from the staging while its payload is in flight, else from the log, a piece at a time.
  fn read_place(
    &mut self,
    staging: &Staging,
    place: &Place,
    cache: &mut TextCache,
  ) -> Result<Vec<u8>, StoreError> {
    cache.places = cache.places.saturating_add(1);
    let start = usize::try_from(place.offset).map_err(|_| StoreError::BadRow)?;
    let len = usize::try_from(place.len).map_err(|_| StoreError::BadRow)?;
    if let Some(bytes) = staging.get(&place.payload) {
      return bytes
        .get(start..start.checked_add(len).ok_or(StoreError::BadRow)?)
        .map(<[u8]>::to_vec)
        .ok_or(StoreError::BadRow);
    }
    let (at, payload_len) = self
      .content_place(&place.payload)?
      .ok_or(StoreError::BadRow)?;
    let end = place
      .offset
      .checked_add(place.len)
      .ok_or(StoreError::BadRow)?;
    if end > payload_len {
      return Err(StoreError::BadRow);
    }
    let mut bytes = vec![0u8; len];
    self.log().read_at(
      &mut bytes,
      at.checked_add(place.offset).ok_or(StoreError::BadRow)?,
    )?;
    Ok(bytes)
  }
}

#[cfg(test)]
mod tests;
