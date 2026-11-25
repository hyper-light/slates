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
/// Format: the keepers table's tag: each changeset that kept a repository's text, by the text, so a forgotten
/// changeset's `T` and `I` rows go to another keeper of the same text ([`forget_rows`]), whatever its path.
const KEEPERS: u8 = b'K';
/// Format: a keeper row's key: the tag, the repository, the text's identity, the sequence and the entry's place in its
/// changeset's section (each big-endian, so a text's keepers are one range, oldest first). Its value is empty.
const KEEPER_KEY_BYTES: usize = 1 + REPOSITORY + IDENTITY + LONG + ORDINAL;
/// Format: where a keeper row's key holds its sequence: past its tag, its repository and its text's identity.
const KEEPER_SEQUENCE_AT: usize = 1 + REPOSITORY + IDENTITY;
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

/// Whole texts made from their chains, by identity, the least lately read out first, bounded in bytes; and what the
/// reads did, the red tests' non-vacuity counters.
///
/// **The bound, from the work (goal 93; main 01a20182's rulings of 2026-11-23).** A read needs one text: the one it
/// lends, walked a piece a request ([`crate::store::PIECE_BYTES`]) until a piece reaches its end. So a cache made for
/// `reads` callers ([`TextCache::for_reads`], the owner's room for waiting requests) holds the texts whose walks are in
/// flight (at most `reads` of them), and besides them the larger of one chunk and the text the read lends: the finished
/// texts the newest chunk still holds, which is where the next version of a path a clone streams in order finds the one
/// before, its chain walk's stop. With no walk in flight it holds at most that chunk, so a cache whose reads have ended
/// holds no more than one. Until 2026-11-23 the bound grew to the longest chain a read had made (360 MB of chunks at the
/// end of a push of Mercurial's history), and then to the distinct texts the last `reads` reads lent, which on CI's
/// runner is hundreds: 646 MB of chunks resident after push-all and after idle (arena-13, CI 45193998198). A chain is made through the cache's own two buffers now, so it needs none
/// held, and a text past the bound is lent from the buffer it was made into. A cache made with a fixed bound
/// ([`TextCache::new`]) keeps it.
///
/// The texts lie in chunks of the mapped allocation's floor, laid end to end, the oldest chunk given back whole (goal 93;
/// A-861's rule for git's objects read lately, applied to the store's texts). Kept as a buffer each, the texts a push of
/// Mercurial's own history made passed through the system allocator one by one, freed oldest first as newer came: 83% of
/// the push's bytes under the floor that lived a second or more, 195 MB still held at its end
/// (`crates/server/tests/hg_push_lifetimes.rs`), the blocks glibc keeps. Each chunk takes the mapped path, so its going
/// gives its pages back to the OS; a text a chunk long or longer is a chunk of its own. A chain is made through two
/// buffers the cache keeps and reuses (`base`, `next`), so a chunk given back while the chain is made loses nothing the
/// next edit needs; held by `tests/text_cache_heap.rs`.
#[derive(Debug)]
pub struct TextCache {
  /// Where each text held lies: its chunk's number and its range there. A hash table, which once grown to the texts a
  /// bound holds allocates nothing as they come and go.
  texts: std::collections::HashMap<Identity, Lent>,
  /// The chunks, oldest first.
  chunks: std::collections::VecDeque<Chunk>,
  /// The texts held, oldest first, each with its chunk's number.
  order: std::collections::VecDeque<(Identity, u64)>,
  /// The number of the oldest chunk: every chunk ever laid takes the next number, so a text whose chunk has gone is a
  /// miss by its number, never another text's bytes.
  first: u64,
  /// The bytes of the texts held.
  bytes: u64,
  /// The bytes the chunks hold reserved, what the bound is held to.
  reserved: u64,
  /// The most bytes the chunks may hold: fixed, or (with `reads`) derived from the last reads.
  bound: u64,
  /// The walks in flight the bound may hold, the owner's room for waiting requests; zero for a fixed bound.
  reads: usize,
  /// The texts whose walks are in flight, by identity, with their lengths: a piece read and its end not reached.
  walks: std::collections::HashMap<Identity, u64>,
  /// Those walks, oldest begun first, so a walk past `reads` (a caller gone mid-walk) is the one let go.
  walk_order: std::collections::VecDeque<Identity>,
  /// The bytes of the texts walked.
  walk_bytes: u64,
  /// Set while a walked text is laid again from a chunk being given back, so that laying gives back without laying
  /// again.
  relaying: bool,
  /// The text the last chain made, which `base` holds until the next is made.
  last_made: Option<Identity>,
  /// The base the chain's next edit is applied to, while a chain is made.
  base: Vec<u8>,
  /// The text the edit makes, while a chain is made.
  next: Vec<u8>,
  /// Texts made from their chains.
  pub made: u64,
  /// Texts let go at the bound: what shows a cache that filled and gave texts back (the bound's non-vacuity count).
  pub let_go: u64,
  /// Edits applied.
  pub edits: u64,
  /// Places read from the log or the staging.
  pub places: u64,
  /// The changesets those places lie in, each recorded as a chain's place is read from it ([`Store::make_chain`]).
  pub changesets: std::collections::BTreeSet<u64>,
}

/// One chunk of [`TextCache`]'s: its texts' bytes, laid end to end within the room it was reserved with.
#[derive(Debug)]
struct Chunk {
  /// The texts' bytes.
  bytes: Vec<u8>,
  /// Whether it holds one text taken whole, which nothing else is laid after.
  whole: bool,
}

/// Where a held text's bytes lie: its chunk's number, and its range there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Lent {
  /// The chunk's number ([`TextCache::first`]).
  chunk: u64,
  /// Where its bytes begin in the chunk.
  at: usize,
  /// Their length.
  len: usize,
}

impl TextCache {
  /// A cache holding at most `bound` bytes of texts.
  pub fn new(bound: u64) -> Self {
    Self {
      texts: std::collections::HashMap::new(),
      chunks: std::collections::VecDeque::new(),
      order: std::collections::VecDeque::new(),
      first: 0,
      bytes: 0,
      reserved: 0,
      bound,
      reads: 0,
      walks: std::collections::HashMap::new(),
      walk_order: std::collections::VecDeque::new(),
      walk_bytes: 0,
      relaying: false,
      last_made: None,
      base: Vec::new(),
      next: Vec::new(),
      made: 0,
      let_go: 0,
      edits: 0,
      places: 0,
      changesets: std::collections::BTreeSet::new(),
    }
  }

  /// The bytes of the texts held.
  pub fn held_bytes(&self) -> u64 {
    self.bytes
  }

  /// A cache for the reads of `reads` callers at once (the owner's room for waiting requests), its bound derived from
  /// them (the type's doc).
  ///
  /// Derived: bound = the bytes of the texts walked now (at most `reads` walks), rounded up to whole chunks (the mapped
  /// floor) + the larger of one chunk and the text the read lends: a walk holds its text until a piece reaches its end, and the newest chunk keeps
  /// the texts lately finished.
  pub fn for_reads(reads: usize) -> Self {
    let mut cache = Self::new(to_u64(slates_mem::large::floor()));
    cache.reads = reads;
    cache
  }

  /// The bytes the cache holds reserved for its texts.
  pub fn reserved_bytes(&self) -> u64 {
    self.reserved
  }

  /// The most bytes the cache may hold now.
  pub fn bound(&self) -> u64 {
    self.bound
  }

  /// The text held under `identity`, if it is.
  pub fn get(&self, identity: &Identity) -> Option<&[u8]> {
    let lent = self.texts.get(identity)?;
    let index = usize::try_from(lent.chunk.checked_sub(self.first)?).ok()?;
    self
      .chunks
      .get(index)?
      .bytes
      .get(lent.at..lent.at.checked_add(lent.len)?)
  }

  /// The text `identity` as a read lends it: from the cache, or from the buffer the last chain was made into where the
  /// bound could not hold it.
  fn lent(&self, identity: &Identity) -> Option<&[u8]> {
    self
      .get(identity)
      .or_else(|| (self.last_made == Some(*identity)).then_some(self.base.as_slice()))
  }

  /// Records a read lending `identity` (`len` bytes) from `offset` as far as `reach` bytes, as the owner serves it a
  /// piece ([`crate::store::PIECE_BYTES`]) or a slice ([`Store::text_slice`]) at a time, or whole: its walk in flight
  /// until a read reaches the end, the bound derived again, the text moved to the newest chunk where it lies in an older
  /// one (so the least lately read go first), and the chunks past the bound given back.
  fn read(&mut self, identity: Identity, len: u64, (offset, reach): (u64, u64)) {
    if self.reads > 0 {
      let finished = offset.saturating_add(reach) >= len;
      if finished {
        if let Some(walked) = self.walks.remove(&identity) {
          self.walk_bytes = self.walk_bytes.saturating_sub(walked);
          self.walk_order.retain(|walking| *walking != identity);
        }
      } else if let std::collections::hash_map::Entry::Vacant(walk) = self.walks.entry(identity) {
        walk.insert(len);
        self.walk_order.push_back(identity);
        self.walk_bytes = self.walk_bytes.saturating_add(len);
        while self.walk_order.len() > self.reads {
          let Some(gone) = self.walk_order.pop_front() else {
            break;
          };
          if let Some(walked) = self.walks.remove(&gone) {
            self.walk_bytes = self.walk_bytes.saturating_sub(walked);
          }
        }
      }
      // The walked texts in whole chunks (a text shorter than a chunk holds a chunk while it is walked), and the larger
      // of a chunk and the text lent.
      let chunk = to_u64(slates_mem::large::floor()).max(1);
      let walked = self.walk_bytes.div_ceil(chunk).saturating_mul(chunk);
      self.bound = walked.saturating_add(len.max(chunk));
    }
    self.refresh(identity);
    self.make_room(0);
  }

  /// Moves `identity`'s text to the newest chunk where it lies in an older one and is shorter than a chunk.
  fn refresh(&mut self, identity: Identity) {
    let newest = self
      .first
      .saturating_add(to_u64(self.chunks.len()))
      .saturating_sub(1);
    let Some(lent) = self.texts.get(&identity).copied() else {
      return;
    };
    if lent.chunk == newest || lent.len >= self.chunk_len() {
      return;
    }
    let mut moved = std::mem::take(&mut self.next);
    moved.clear();
    if let Some(bytes) = self.get(&identity) {
      moved.extend_from_slice(bytes);
    }
    self.texts.remove(&identity);
    self.bytes = self.bytes.saturating_sub(to_u64(lent.len));
    self.keep(identity, &moved);
    self.next = moved;
  }

  /// A chunk's length: the mapped allocation's floor, so each is given back whole when it goes (`slates_mem::large`), or
  /// the bound where that is shorter.
  fn chunk_len(&self) -> usize {
    slates_mem::large::floor()
      .min(usize::try_from(self.bound).unwrap_or(usize::MAX))
      .max(1)
  }

  /// Keeps a copy of `text` under `identity`: laid in the newest chunk, or a chunk of its own when it is a chunk long or
  /// more, the oldest chunks given back until it fits. None kept where it alone passes the bound, or it is held already.
  fn keep(&mut self, identity: Identity, text: &[u8]) {
    if to_u64(text.len()) > self.bound || self.texts.contains_key(&identity) {
      return;
    }
    if text.len() >= self.chunk_len() {
      self.make_room(text.len());
      let mut whole = Vec::new();
      if whole.try_reserve_exact(text.len()).is_err() {
        return;
      }
      whole.extend_from_slice(text);
      self.push_chunk(whole, true);
    } else {
      let room = self
        .chunks
        .back()
        .filter(|chunk| !chunk.whole)
        .map_or(0, |chunk| {
          chunk.bytes.capacity().saturating_sub(chunk.bytes.len())
        });
      if room < text.len() {
        let len = self.chunk_len();
        self.make_room(len);
        let mut fresh = Vec::new();
        if fresh.try_reserve_exact(len).is_err() {
          return;
        }
        self.push_chunk(fresh, false);
      }
    }
    let number = self
      .first
      .saturating_add(to_u64(self.chunks.len()))
      .saturating_sub(1);
    let Some(chunk) = self.chunks.back_mut() else {
      return;
    };
    let at = chunk.bytes.len();
    chunk.bytes.extend_from_slice(text);
    self.order.push_back((identity, number));
    self.bytes = self.bytes.saturating_add(to_u64(text.len()));
    self.texts.insert(
      identity,
      Lent {
        chunk: number,
        at,
        len: text.len(),
      },
    );
  }

  /// Adds `bytes` as the newest chunk.
  fn push_chunk(&mut self, bytes: Vec<u8>, whole: bool) {
    self.reserved = self.reserved.saturating_add(to_u64(bytes.capacity()));
    self.chunks.push_back(Chunk { bytes, whole });
  }

  /// Gives back the oldest chunks, whole, until `len` bytes more fit the bound, forgetting the texts laid in them; a text
  /// whose walk is in flight is laid again in the newest chunk first, so the walk finds it at its next piece (the bound
  /// counts it).
  fn make_room(&mut self, len: usize) {
    // At most every chunk held now is given back: a walked text laid again lies in a chunk laid after them, so a bound
    // that cannot hold the walks ends here, never in a loop.
    let mut passes = self.chunks.len();
    while self.reserved.saturating_add(to_u64(len)) > self.bound && passes > 0 {
      passes = passes.saturating_sub(1);
      let Some(oldest) = self.chunks.pop_front() else {
        return;
      };
      let number = self.first;
      self.first = self.first.saturating_add(1);
      self.reserved = self
        .reserved
        .saturating_sub(to_u64(oldest.bytes.capacity()));
      let mut walked: Vec<(Identity, Lent)> = Vec::new();
      while let Some(&(identity, chunk)) = self.order.front()
        && chunk == number
      {
        self.order.pop_front();
        if let Some(lent) = self.texts.get(&identity).copied()
          && lent.chunk == number
        {
          self.bytes = self.bytes.saturating_sub(to_u64(lent.len));
          self.texts.remove(&identity);
          if !self.relaying && self.walks.contains_key(&identity) {
            walked.push((identity, lent));
          } else {
            self.let_go = self.let_go.saturating_add(1);
          }
        }
      }
      if !walked.is_empty() {
        self.relaying = true;
        for (identity, lent) in walked {
          match lent
            .at
            .checked_add(lent.len)
            .and_then(|end| oldest.bytes.get(lent.at..end))
          {
            Some(bytes) => self.keep(identity, bytes),
            None => self.let_go = self.let_go.saturating_add(1),
          }
        }
        self.relaying = false;
      }
    }
  }

  /// Gives back a chain buffer that took the mapped path (a text a floor long or longer made it so), before the next
  /// chain is made, so a cache does not hold a large text's pages past the read that lent it; one under the floor is
  /// kept for the next chain.
  fn shrink_buffers(&mut self) {
    let floor = slates_mem::large::floor();
    for buffer in [&mut self.base, &mut self.next] {
      if buffer.capacity() >= floor {
        *buffer = Vec::new();
      }
    }
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
      &keeper_key(header.repository, &entry.text, (header.sequence, ordinal)),
      &[],
    )?;
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

/// What reads back a changeset's text entries by its sequence, over the store's rows: the store's own reader, lent to
/// [`forget_rows`] and [`next_keeper`].
pub(crate) type EntriesOf<'a, F> = dyn FnMut(&mut mantle_engine::shard_db::ShardDb<F>, u64) -> Result<Vec<TextEntry>, StoreError>
  + 'a;

/// A keeper row's key.
fn keeper_key(
  repository: u128,
  text: &Identity,
  (sequence, ordinal): (u64, u32),
) -> [u8; KEEPER_KEY_BYTES] {
  let mut key = [0u8; KEEPER_KEY_BYTES];
  let (repository, sequence, ordinal) = (
    repository.to_be_bytes(),
    sequence.to_be_bytes(),
    ordinal.to_be_bytes(),
  );
  // Each field in turn, by checked splits: the key's length is the fields' sum (`KEEPER_KEY_BYTES`), so every split
  // holds; none can run past the key.
  let fields: [&[u8]; 5] = [&[KEEPERS], &repository, text, &sequence, &ordinal];
  let mut rest: &mut [u8] = &mut key;
  for field in fields {
    let Some((head, tail)) = rest.split_at_mut_checked(field.len()) else {
      break;
    };
    head.copy_from_slice(field);
    rest = tail;
  }
  key
}

/// The keepers of repository `repository`'s text `text` other than `forgotten`'s, oldest first: each changeset's
/// sequence and its entry's place in its section.
fn keepers_of<F: hyper_block::block::BlockFile>(
  rows: &mut mantle_engine::shard_db::ShardDb<F>,
  (repository, text): (u128, &Identity),
  forgotten: u64,
) -> Result<Vec<(u64, u32)>, StoreError> {
  let prefix = &keeper_key(repository, text, (0, 0))[..1 + REPOSITORY + IDENTITY];
  let end = prefix_end(prefix);
  let (mut found, mut start, mut next) = (Vec::new(), prefix.to_vec(), Vec::new());
  loop {
    let mut page = mantle_engine::rows::Rows::default();
    let more = rows.scan(&start, end.as_deref(), KEEPERS_A_SCAN, &mut page, &mut next)?;
    for (key, _) in page.iter() {
      // The sequence, then the entry's place, read by checked chunks from where the text's identity ends: a key shorter
      // than a keeper's is refused, never read past.
      let (sequence, ordinal) = key
        .get(KEEPER_SEQUENCE_AT..)
        .and_then(|rest| rest.split_first_chunk::<LONG>())
        .and_then(|(sequence, rest)| {
          let ordinal = rest.first_chunk::<ORDINAL>()?;
          Some((u64::from_be_bytes(*sequence), u32::from_be_bytes(*ordinal)))
        })
        .ok_or(StoreError::BadRow)?;
      if sequence != forgotten {
        found.push((sequence, ordinal));
      }
    }
    if !more {
      return Ok(found);
    }
    core::mem::swap(&mut start, &mut next);
  }
}

/// Format: the keeper rows a scan's page reads: a page of the engine's scan, not a tuning of the forgetting.
const KEEPERS_A_SCAN: usize = 256;

/// Forgets the rows the `Texts` section `entries` of repository `repository`'s changeset `sequence` put ([`put_rows`]):
/// its history and keeper rows; and its `T` and `I` rows where they are its own (the first keeper's), each moved to the
/// next keeper of the same text (and, an `I` row, of the same path and tool id), read back by `entries_of`, or
/// forgotten where none other kept it. The rows forgotten or moved.
///
/// # Errors
/// [`StoreError::BadRow`] for a row that does not decode; `entries_of`'s and the engine's refusals.
pub(crate) fn forget_rows<F: hyper_block::block::BlockFile>(
  rows: &mut mantle_engine::shard_db::ShardDb<F>,
  (repository, sequence): (u128, u64),
  entries: &[TextEntry],
  entries_of: &mut EntriesOf<'_, F>,
) -> Result<u64, StoreError> {
  let mut forgotten = 0_u64;
  let mut held = Vec::with_capacity(TEXT_VALUE_BYTES);
  for (ordinal, entry) in entries.iter().enumerate() {
    let ordinal = u32::try_from(ordinal).map_err(|_| StoreError::BadRow)?;
    let path = *blake3::hash(&entry.path).as_bytes();
    rows.delete(&keeper_key(repository, &entry.text, (sequence, ordinal)))?;
    rows.delete(&history_key(repository, &path, (sequence, ordinal)))?;
    forgotten = forgotten.saturating_add(2);
    let keepers = keepers_of(rows, (repository, &entry.text), sequence)?;
    let text = text_key(repository, &entry.text);
    if rows.get(&text, &mut held)? && decode_row(&held).is_some_and(|row| row.sequence == sequence)
    {
      let next = next_keeper(rows, &keepers, entries_of, |kept| kept.text == entry.text)?;
      match next {
        Some((keeper, kept)) => {
          held.clear();
          put_row_value(&mut held, keeper, &kept);
          rows.put(&text, &held)?;
        }
        None => rows.delete(&text)?,
      }
      forgotten = forgotten.saturating_add(1);
    }
    if entry.tool_id.is_empty() {
      continue;
    }
    let tool = tool_key(repository, &path, &entry.tool_id);
    if rows.get(&tool, &mut held)?
      && decode_tool_value(&held).is_ok_and(|(_, row)| row.sequence == sequence)
    {
      let next = next_keeper(rows, &keepers, entries_of, |kept| {
        kept.text == entry.text && kept.path == entry.path && kept.tool_id == entry.tool_id
      })?;
      match next {
        Some((keeper, kept)) => {
          held.clear();
          held.extend_from_slice(&kept.text);
          put_row_value(&mut held, keeper, &kept);
          rows.put(&tool, &held)?;
        }
        None => rows.delete(&tool)?,
      }
      forgotten = forgotten.saturating_add(1);
    }
  }
  Ok(forgotten)
}

/// The first of `keepers` whose entry is `wanted`: its sequence and its entry, read back by `entries_of`.
fn next_keeper<F: hyper_block::block::BlockFile>(
  rows: &mut mantle_engine::shard_db::ShardDb<F>,
  keepers: &[(u64, u32)],
  entries_of: &mut EntriesOf<'_, F>,
  wanted: impl Fn(&TextEntry) -> bool,
) -> Result<Option<(u64, TextEntry)>, StoreError> {
  for (keeper, ordinal) in keepers {
    let entries = entries_of(rows, *keeper)?;
    if let Some(kept) = usize::try_from(*ordinal)
      .ok()
      .and_then(|at| entries.into_iter().nth(at))
      .filter(|kept| wanted(kept))
    {
      return Ok(Some((*keeper, kept)));
    }
  }
  Ok(None)
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
    let len = cache
      .lent(text)
      .map(<[u8]>::len)
      .ok_or(StoreError::BadRow)?;
    cache.read(
      *text,
      to_u64(len),
      (offset, to_u64(crate::store::PIECE_BYTES)),
    );
    // Held, or in the buffer the chain was made into where the bound could not hold it.
    let Some(whole) = cache.lent(text) else {
      return Err(StoreError::BadRow);
    };
    let start = usize::try_from(offset).map_err(|_| StoreError::BadRow)?;
    Ok(whole.get(start..))
  }

  /// The text `text` of repository `repository` whole, as [`Store::text_at`] makes and lends it, for a caller that takes
  /// it whole in one read (the owner's batch of a fetch's texts, `owner::StoreHandle::texts`). None for a text no landed
  /// changeset kept.
  ///
  /// The read is its walk's end, so the cache keeps no walk of it in flight (goal 93, main's round at 15b4f1061: read
  /// at its start through [`Store::text_at`] and taken whole, a text longer than a piece stayed a walk no later piece
  /// ended, and the cache's bound grew by each, to 4,792 chunks still live after hg's 57k clone).
  ///
  /// # Errors
  /// As [`Store::text_at`].
  pub fn text_whole<'c>(
    &mut self,
    staging: &Staging,
    repository: u128,
    text: &Identity,
    cache: &'c mut TextCache,
  ) -> Result<Option<&'c [u8]>, StoreError> {
    if cache.get(text).is_none() && !self.make(staging, repository, text, cache)? {
      return Ok(None);
    }
    let len = to_u64(
      cache
        .lent(text)
        .map(<[u8]>::len)
        .ok_or(StoreError::BadRow)?,
    );
    // Read whole, its reach its length: the walk, if any piece began one, ends here, and none begins.
    cache.read(*text, len, (0, len));
    cache.lent(text).map(Some).ok_or(StoreError::BadRow)
  }

  /// At most `most` bytes of the text `text` of repository `repository` from `offset`, as [`Store::text_at`] makes and
  /// lends it, for a caller that copies a text in slices of a step's bytes (goal 93, main's ruling of 2026-11-25: a
  /// fetch's texts copied by the owner at most a step's copy budget a request, a text past it in slices across
  /// requests). The read reaches `most` bytes, so its walk ends at the slice that reaches the text's end, whatever the
  /// slice's length against a piece. Empty at the text's end; none past it, or for a text no landed changeset kept.
  ///
  /// # Errors
  /// As [`Store::text_at`].
  pub fn text_slice<'c>(
    &mut self,
    staging: &Staging,
    repository: u128,
    (text, offset, most): (&Identity, u64, u64),
    cache: &'c mut TextCache,
  ) -> Result<Option<&'c [u8]>, StoreError> {
    if cache.get(text).is_none() && !self.make(staging, repository, text, cache)? {
      return Ok(None);
    }
    let len = to_u64(
      cache
        .lent(text)
        .map(<[u8]>::len)
        .ok_or(StoreError::BadRow)?,
    );
    cache.read(*text, len, (offset, most));
    let Some(whole) = cache.lent(text) else {
      return Err(StoreError::BadRow);
    };
    let start = usize::try_from(offset).map_err(|_| StoreError::BadRow)?;
    let end = start
      .saturating_add(usize::try_from(most).unwrap_or(usize::MAX))
      .min(whole.len());
    Ok(whole.get(start..end.max(start)))
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
    // The path's tool id rows share their key's prefix and are ordered by tool id, so they are read in scans of it, each
    // begun at the next asked id not yet passed and a page as large as the asks left: a stream asking for a log's every
    // version reads it in one scan (r27 on hg-src: about 230,000 random lookups a stream, 1.35 s of its 3.7 s waiting on
    // the store), and a lone ask reads at its own key. Until 2026-11-24 every scan began at the prefix's first row, so a
    // lone ask read a page of one row a row up to its own: half the log a text, a clone after P5 quadratic in its history
    // (main's interop bench at ed71ac0be: hg's 57k clone past 13.5 minutes, stock's 42.4 s).
    let hash = *blake3::hash(path).as_bytes();
    let start = tool_key(repository, &hash, &[]);
    let end = prefix_end(&start);
    let wanted: std::collections::BTreeSet<&[u8]> = tool_ids.iter().map(Vec::as_slice).collect();
    let mut kept: std::collections::BTreeMap<Vec<u8>, (Identity, TextRow)> =
      std::collections::BTreeMap::new();
    let mut next = Vec::new();
    // The asked ids not yet passed, in key order: the first is where the next scan begins.
    let mut left = wanted.iter().copied().peekable();
    while let Some(first) = left.peek().copied() {
      let from = tool_key(repository, &hash, first);
      let page = left.clone().count().max(1);
      let mut out = mantle_engine::rows::Rows::default();
      self.tool_read();
      let more = self
        .rows_mut()
        .scan(&from, end.as_deref(), page, &mut out, &mut next)?;
      let mut last: Option<Vec<u8>> = None;
      for (key, value) in out.iter() {
        let Some(tool_id) = key.strip_prefix(start.as_slice()) else {
          continue;
        };
        if wanted.contains(tool_id) {
          kept.insert(tool_id.to_vec(), decode_tool_value(value)?);
        }
        last = Some(tool_id.to_vec());
      }
      if !more {
        break;
      }
      // Every asked id at or before the last row read is passed: found, or held by no row.
      match last {
        Some(last) => while left.next_if(|id| *id <= last.as_slice()).is_some() {},
        None => break,
      }
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
    // The chain's texts are kept as they are made, within the bound, so the next read of the path finds them. The chain is
    // made through the cache's two buffers, taken out while it is made and put back after, so a chunk the cache gives
    // back meanwhile loses nothing the next edit needs; the last text made stays in `base` for the read to lend.
    cache.shrink_buffers();
    cache.last_made = None;
    let mut base = std::mem::take(&mut cache.base);
    let mut next = std::mem::take(&mut cache.next);
    let made = self.make_chain(staging, chain, cache, &mut base, &mut next);
    cache.base = base;
    cache.next = next;
    made?;
    cache.last_made = Some(*text);
    Ok(true)
  }

  /// Makes the texts of `chain` (newest first, its oldest a whole or an edit of a text the cache holds), oldest first,
  /// each applied to the one before in `base` and made into `next`, and keeps each.
  fn make_chain(
    &mut self,
    staging: &Staging,
    mut chain: Vec<(Identity, TextRow)>,
    cache: &mut TextCache,
    base: &mut Vec<u8>,
    next: &mut Vec<u8>,
  ) -> Result<(), StoreError> {
    base.clear();
    if let Some(held) = chain
      .last()
      .and_then(|(_, row)| row.base)
      .map(|at| cache.get(&at).ok_or(StoreError::BadRow))
    {
      base.extend_from_slice(held?);
    }
    while let Some((identity, row)) = chain.pop() {
      let bytes = self.read_place(staging, &row.place, cache)?;
      cache.changesets.insert(row.sequence);
      match row.base {
        None => {
          if to_u64(bytes.len()) != row.len {
            return Err(StoreError::BadRow);
          }
          next.clear();
          next.extend_from_slice(&bytes);
        }
        Some(_) => {
          apply_into(base, &bytes, row.len, next).map_err(|_| StoreError::BadRow)?;
          cache.edits = cache.edits.saturating_add(1);
        }
      }
      cache.made = cache.made.saturating_add(1);
      cache.keep(identity, next);
      std::mem::swap(base, next);
    }
    Ok(())
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
