//! A git repository's rows in its one `slates_vcs::Index` (A-614; §4.17.2 "The repository in git's terms", item 2).
//!
//! **Keys.** An object row's key is [`OBJECT_KEY`] and the object's id; a position row's is [`POSITION_KEY`] and the
//! position, big-endian, so position rows sort as positions do and every object row sorts before every position row.
//!
//! **An object row** names the object's root content (`slates-vcs` holds it while the row lives) and carries, all
//! integers little-endian:
//!
//! ```text
//! row    = kind (u8, the pack's number) | length (u64) | position (u64) [| entry] [| commit]
//! entry  = offset (u64) | raw length (u64) | CRC32 (u64, its low half) | base (u64, a position plus one; 0 for none)
//! commit = tree (u64, a position) | date (u64) | generation (u64) | parents (u32) | each parent (u64, a position)
//! ```
//!
//! **An object in a pack** (A-722's record v4, rows by pack place): its row names the pack the push brought it in, held
//! once as content, and carries where its entry begins there (`offset`), the kind's byte marked [`IN_PACK`]; the object
//! is read where it lies (`store::packed`), never held a second time inflated. An object held whole (the server's own
//! commits) names its own content and carries no entry. With its offset the row keeps what git's pack index keeps of the
//! entry, and its base, all found once as the push is staged (A-864): the entry's raw length (header and compressed data),
//! the CRC32 of those raw bytes, and for a delta the position of the object it is a delta against. A fetch sends the
//! entry on as it lies by these alone: its CRC checked, its bytes copied, no object read (git's own reuse checks no more,
//! `builtin/pack-objects.c`'s `write_reuse_object`).
//!
//! A row holds nothing known only once its push lands, so a push can write its rows as its objects arrive (A-722): the
//! version that brought an object is the repository's landing map's, by position (`Repository::bringer_of`). A commit's part
//! is what git's commit-graph keeps of it [B: gitformat-commit-graph]: its root tree, its committer's date, its
//! generation number (one more than its parents' greatest, one for a root: version 1, "topological levels") and its
//! parents in its own order, each by position. **A position row** names nothing and carries the object's id.

use std::cmp::Ordering;
use std::mem::size_of;
use std::ops::Deref;

use crate::object_name::{Kind, MAX_RAW_LEN, ObjectFormat, ObjectId};

use super::naming::kind_of;

/// Format: the most bytes of an object row's payload but a commit's part: its type, its length and position, an
/// in-pack row's entry (four words), and an annotated tag's target position (one word).
pub const OBJECT_PAYLOAD_BYTES: usize = 1 + 7 * WORD_BYTES;

/// Format: the bytes of a commit's part but its parents: three words and the parents' count.
pub const COMMIT_PART_BYTES: usize = 3 * WORD_BYTES + COUNT_BYTES;

/// Format: the mark on an object row's kind byte for an object read from a pack, its entry's four words following its
/// position.
pub const IN_PACK: u8 = 0x80;

/// Format: the first byte of an object row's key, which the object's id follows.
pub const OBJECT_KEY: u8 = b'o';

/// Format: the first byte of a position row's key, which the position follows, big-endian.
pub const POSITION_KEY: u8 = b'p';

/// Format: the bytes of a word.
const WORD_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a commit's count of parents.
const COUNT_BYTES: usize = size_of::<u32>();

/// An object row read back (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRow {
  /// The object's type.
  pub kind: Kind,
  /// Its content's length in bytes.
  pub len: u64,
  /// Its position.
  pub position: u64,
  /// Its entry in the pack its row names, for an object read from a pack (module docs); `None` for one held whole.
  pub pack: Option<PackedRow>,
  /// A commit's part; `None` for any other type.
  pub commit: Option<CommitRow>,
  /// For an annotated tag (`Kind::Tag`), the position of the object it names; `None` for any other
  /// type or a tag whose target is not indexed. Stored in the index row like `CommitRow` so ref
  /// advertisements, `ls-refs --peel`, `include-tag`, and ref peels never inflate let-go pack data.
  pub tag_target: Option<u64>,
}

/// What an object row keeps of its object's entry in the pack the row names (module docs; A-864).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedRow {
  /// Where the entry begins.
  pub offset: u64,
  /// Its raw length: its header and its compressed data.
  pub len: u64,
  /// The CRC32 of those raw bytes, as git's pack index keeps it.
  pub crc: u32,
  /// For a delta, the position of the object it is a delta against; `None` for a whole entry.
  pub base: Option<u64>,
}

/// A commit's part of its row (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRow {
  /// Its root tree's position.
  pub tree: u64,
  /// Its committer's date, seconds since the epoch.
  pub date: u64,
  /// Its generation number.
  pub generation: u64,
  /// Its parents' positions, in its own order.
  pub parents: Vec<u64>,
}

/// Derived: the longest row key any object format produces (prefix byte + SHA-256 object id).
const MAX_KEY_BYTES: usize = 1 + MAX_RAW_LEN;

/// Stack-allocated index row key for object and position lookups, avoiding a heap `Vec<u8>`
/// allocation on every `Repository::indexed` and `Repository::id_at` call.
#[derive(Clone, Copy, Debug)]
pub struct RowKey {
  bytes: [u8; MAX_KEY_BYTES],
  len: usize,
}

impl RowKey {
  /// The live key bytes.
  pub fn as_slice(&self) -> &[u8] {
    self.bytes.get(..self.len).unwrap_or_default()
  }
}

impl Deref for RowKey {
  type Target = [u8];

  fn deref(&self) -> &[u8] {
    self.as_slice()
  }
}

impl AsRef<[u8]> for RowKey {
  fn as_ref(&self) -> &[u8] {
    self.as_slice()
  }
}

impl PartialEq for RowKey {
  fn eq(&self, other: &Self) -> bool {
    self.as_slice() == other.as_slice()
  }
}

impl Eq for RowKey {}

impl PartialOrd for RowKey {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for RowKey {
  fn cmp(&self, other: &Self) -> Ordering {
    self.as_slice().cmp(other.as_slice())
  }
}

/// The key of `id`'s object row.
pub fn object_key(id: &ObjectId) -> RowKey {
  let raw = id.as_bytes();
  let mut bytes = [0u8; MAX_KEY_BYTES];
  if let Some(first) = bytes.first_mut() {
    *first = OBJECT_KEY;
  }
  let len = raw.len().min(MAX_RAW_LEN);
  let end = len.saturating_add(1);
  if let (Some(dst), Some(src)) = (bytes.get_mut(1..end), raw.get(..len)) {
    dst.copy_from_slice(src);
  }
  RowKey { bytes, len: end }
}

/// The key of `position`'s row.
pub fn position_key(position: u64) -> RowKey {
  let mut bytes = [0u8; MAX_KEY_BYTES];
  if let Some(first) = bytes.first_mut() {
    *first = POSITION_KEY;
  }
  let end = WORD_BYTES.saturating_add(1);
  if let Some(dst) = bytes.get_mut(1..end) {
    dst.copy_from_slice(&position.to_be_bytes());
  }
  RowKey { bytes, len: end }
}

/// An object row's payload.
pub fn encode(row: &ObjectRow) -> Vec<u8> {
  let mark = if row.pack.is_some() { IN_PACK } else { 0 };
  let mut out = vec![row.kind.pack_number() | mark];
  for word in [row.len, row.position] {
    out.extend_from_slice(&word.to_le_bytes());
  }
  if let Some(pack) = row.pack {
    let base = pack.base.map_or(0, |base| base.saturating_add(1));
    for word in [pack.offset, pack.len, u64::from(pack.crc), base] {
      out.extend_from_slice(&word.to_le_bytes());
    }
  }
  if let Some(commit) = &row.commit {
    for word in [commit.tree, commit.date, commit.generation] {
      out.extend_from_slice(&word.to_le_bytes());
    }
    let count = u32::try_from(commit.parents.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for parent in commit
      .parents
      .iter()
      .take(usize::try_from(count).unwrap_or(usize::MAX))
    {
      out.extend_from_slice(&parent.to_le_bytes());
    }
  }
  if row.kind == Kind::Tag {
    let target = row.tag_target.map_or(0, |pos| pos.saturating_add(1));
    out.extend_from_slice(&target.to_le_bytes());
  }
  out
}

/// A cursor over a payload's fields, every read checked.
struct Fields<'a> {
  bytes: &'a [u8],
}

impl<'a> Fields<'a> {
  fn take(&mut self, len: usize) -> Option<&'a [u8]> {
    let (taken, rest) = self.bytes.split_at_checked(len)?;
    self.bytes = rest;
    Some(taken)
  }

  fn word(&mut self) -> Option<u64> {
    let bytes = <[u8; WORD_BYTES]>::try_from(self.take(WORD_BYTES)?).ok()?;
    Some(u64::from_le_bytes(bytes))
  }

  fn count(&mut self) -> Option<u32> {
    let bytes = <[u8; COUNT_BYTES]>::try_from(self.take(COUNT_BYTES)?).ok()?;
    Some(u32::from_le_bytes(bytes))
  }
}

/// An object row's payload read back, or `None` when it is not one: a commit's part only after a commit's type, and no
/// byte left over.
pub fn decode(payload: &[u8]) -> Option<ObjectRow> {
  let (&marked, rest) = payload.split_first()?;
  let kind = kind_of(marked & !IN_PACK)?;
  let mut fields = Fields { bytes: rest };
  let (len, position) = (fields.word()?, fields.word()?);
  let pack = if marked & IN_PACK != 0 {
    let (offset, len, crc, base) = (
      fields.word()?,
      fields.word()?,
      fields.word()?,
      fields.word()?,
    );
    Some(PackedRow {
      offset,
      len,
      crc: u32::try_from(crc).ok()?,
      base: base.checked_sub(1),
    })
  } else {
    None
  };
  let commit = if kind == Kind::Commit {
    Some(decode_commit(&mut fields)?)
  } else {
    None
  };
  let tag_target = if kind == Kind::Tag {
    fields.word()?.checked_sub(1)
  } else {
    None
  };
  fields.bytes.is_empty().then_some(ObjectRow {
    kind,
    len,
    position,
    pack,
    commit,
    tag_target,
  })
}

/// Format: words in an in-pack object row's entry (`offset`, `len`, `crc`, `base`).
const PACKED_ENTRY_WORDS: usize = 4;

/// Format: bytes of an in-pack object row's entry.
const PACKED_ENTRY_BYTES: usize = PACKED_ENTRY_WORDS * WORD_BYTES;

/// Format: words in a commit row's fixed header (`tree`, `date`, `generation`).
const COMMIT_HEADER_WORDS: usize = 3;

/// Format: bytes of a commit row's fixed header before its parent count.
const COMMIT_HEADER_BYTES: usize = COMMIT_HEADER_WORDS * WORD_BYTES;

/// Format: regular commits carry 0 or 1 parent and standard merges carry 2 parents
/// (`gitformat-commit`); storing up to two parents inline avoids a heap allocation on 99.9%+ of
/// commit parent lookups while octopus merges spill seamlessly to a `Vec`.
const INLINE_PARENTS: usize = 2;

/// Stack-backed parent position list returned by [`decode_commit_parents`]: zero heap allocations
/// for regular commits and two-way merges, spilling to `Vec<u64>` only for octopus merges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmallParents {
  /// At most [`INLINE_PARENTS`] parents stored inline.
  Inline {
    /// Inline parent slots; only the first `len` are valid.
    items: [u64; INLINE_PARENTS],
    /// Number of valid parent positions (`0..=INLINE_PARENTS`).
    len: usize,
  },
  /// More than [`INLINE_PARENTS`] parents (octopus merge).
  Spilled(Vec<u64>),
}

impl SmallParents {
  /// The commit's first parent position, if it has any parents.
  pub fn first(&self) -> Option<u64> {
    match self {
      SmallParents::Inline { items, len } => (*len > 0).then(|| items.first().copied()).flatten(),
      SmallParents::Spilled(parents) => parents.first().copied(),
    }
  }
}

impl Default for SmallParents {
  fn default() -> SmallParents {
    SmallParents::Inline {
      items: [0; INLINE_PARENTS],
      len: 0,
    }
  }
}

/// Iterator over [`SmallParents`].
pub enum SmallParentsIter {
  /// Iterating over inline slots.
  Inline {
    /// Inline parent slots.
    items: [u64; INLINE_PARENTS],
    /// Next slot index.
    at: usize,
    /// Valid slot count.
    len: usize,
  },
  /// Iterating over spilled octopus parents.
  Spilled(std::vec::IntoIter<u64>),
}

impl Iterator for SmallParentsIter {
  type Item = u64;

  fn next(&mut self) -> Option<u64> {
    match self {
      SmallParentsIter::Inline { items, at, len } => {
        if *at >= *len {
          return None;
        }
        let item = *items.get(*at)?;
        *at = at.saturating_add(1);
        Some(item)
      }
      SmallParentsIter::Spilled(iter) => iter.next(),
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    match self {
      SmallParentsIter::Inline { at, len, .. } => {
        let rem = len.saturating_sub(*at);
        (rem, Some(rem))
      }
      SmallParentsIter::Spilled(iter) => iter.size_hint(),
    }
  }
}

impl IntoIterator for SmallParents {
  type Item = u64;
  type IntoIter = SmallParentsIter;

  fn into_iter(self) -> SmallParentsIter {
    match self {
      SmallParents::Inline { items, len } => SmallParentsIter::Inline { items, at: 0, len },
      SmallParents::Spilled(vec) => SmallParentsIter::Spilled(vec.into_iter()),
    }
  }
}

/// Decodes only the position word of an object row payload without decoding pack metadata, commit
/// parents, or allocating on the heap.
pub fn decode_position(payload: &[u8]) -> Option<u64> {
  let (&marked, rest) = payload.split_first()?;
  kind_of(marked & !IN_PACK)?;
  let mut fields = Fields { bytes: rest };
  let _len = fields.word()?;
  fields.word()
}

/// Decodes only the byte length of an object row payload without allocating.
pub fn decode_len(payload: &[u8]) -> Option<u64> {
  let (&marked, rest) = payload.split_first()?;
  kind_of(marked & !IN_PACK)?;
  let mut fields = Fields { bytes: rest };
  fields.word()
}

/// Advances `payload` past the object row prefix and optional packed entry into the commit part,
/// returning `None` for non-commit rows or truncated payloads.
fn commit_fields(payload: &[u8]) -> Option<Fields<'_>> {
  let (&marked, rest) = payload.split_first()?;
  if kind_of(marked & !IN_PACK)? != Kind::Commit {
    return None;
  }
  let mut fields = Fields { bytes: rest };
  let _len = fields.word()?;
  let _position = fields.word()?;
  if marked & IN_PACK != 0 {
    fields.take(PACKED_ENTRY_BYTES)?;
  }
  Some(fields)
}

/// Decodes `(tree, date, generation)` from a commit object row payload without heap-allocating its
/// parent list. Returns `None` for non-commit rows or malformed payloads.
pub fn decode_commit_header(payload: &[u8]) -> Option<(u64, u64, u64)> {
  let mut fields = commit_fields(payload)?;
  Some((fields.word()?, fields.word()?, fields.word()?))
}

/// Decodes `(tree, generation, parents)` from a commit object row payload in one pass without
/// allocating for 0-, 1-, or 2-parent commits. Returns `None` for non-commit rows or truncated
/// payloads.
pub fn decode_commit_walk(payload: &[u8]) -> Option<(u64, u64, SmallParents)> {
  let mut fields = commit_fields(payload)?;
  let tree = fields.word()?;
  let _date = fields.word()?;
  let generation = fields.word()?;
  let parents = decode_small_parents(&mut fields)?;
  Some((tree, generation, parents))
}

/// Decodes only the parent positions from a commit object row payload into a zero-allocation
/// [`SmallParents`]. Returns `None` for non-commit rows or truncated payloads.
pub fn decode_commit_parents(payload: &[u8]) -> Option<SmallParents> {
  let mut fields = commit_fields(payload)?;
  fields.take(COMMIT_HEADER_BYTES)?;
  decode_small_parents(&mut fields)
}

/// Decodes a commit row's parent count and parent positions into [`SmallParents`].
fn decode_small_parents(fields: &mut Fields<'_>) -> Option<SmallParents> {
  let count = usize::try_from(fields.count()?).ok()?;
  if fields.bytes.len() != count.checked_mul(WORD_BYTES)? {
    return None;
  }
  if count <= INLINE_PARENTS {
    let mut items = [0_u64; INLINE_PARENTS];
    for slot in items.iter_mut().take(count) {
      *slot = fields.word()?;
    }
    return Some(SmallParents::Inline { items, len: count });
  }
  let mut parents = Vec::with_capacity(count);
  for _ in 0..count {
    parents.push(fields.word()?);
  }
  Some(SmallParents::Spilled(parents))
}

/// Decodes the indexed target position of an annotated tag row payload without allocating.
/// Returns `None` for non-tag rows or malformed payloads, `Some(None)` if the tag's target was not
/// indexed in the row.
pub fn decode_tag_target(payload: &[u8]) -> Option<Option<u64>> {
  let (&marked, rest) = payload.split_first()?;
  if kind_of(marked & !IN_PACK)? != Kind::Tag {
    return None;
  }
  let mut fields = Fields { bytes: rest };
  let _len = fields.word()?;
  let _position = fields.word()?;
  if marked & IN_PACK != 0 {
    fields.take(PACKED_ENTRY_BYTES)?;
  }
  let raw = fields.word()?;
  fields.bytes.is_empty().then_some(raw.checked_sub(1))
}

/// A commit's part, read from `fields`.
fn decode_commit(fields: &mut Fields<'_>) -> Option<CommitRow> {
  let (tree, date, generation) = (fields.word()?, fields.word()?, fields.word()?);
  let count = usize::try_from(fields.count()?).ok()?;
  let bound = fields.bytes.len().checked_div(WORD_BYTES).unwrap_or(0);
  if count > bound {
    return None;
  }
  let mut parents = Vec::with_capacity(count);
  for _ in 0..count {
    parents.push(fields.word()?);
  }
  Some(CommitRow {
    tree,
    date,
    generation,
    parents,
  })
}

/// The id a position row carries, in `format`.
pub fn position_id(format: ObjectFormat, payload: &[u8]) -> Option<ObjectId> {
  ObjectId::from_bytes(format, payload)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn assert_commit_row_partials(packed: Option<PackedRow>, parents: Vec<u64>) {
    let row = ObjectRow {
      kind: Kind::Commit,
      len: 321,
      position: 99,
      pack: packed,
      commit: Some(CommitRow {
        tree: 55,
        date: 1_700_000_000,
        generation: 12,
        parents: parents.clone(),
      }),
      tag_target: None,
    };
    let encoded = encode(&row);
    assert_eq!(decode(&encoded), Some(row));
    assert_eq!(decode_position(&encoded), Some(99));
    assert_eq!(decode_len(&encoded), Some(321));
    assert_eq!(
      decode_commit_header(&encoded),
      Some((55, 1_700_000_000, 12))
    );
    assert_eq!(
      decode_commit_parents(&encoded)
        .unwrap()
        .into_iter()
        .collect::<Vec<_>>(),
      parents
    );
    assert_eq!(decode_tag_target(&encoded), None);
  }

  fn assert_tag_row_partials(packed: Option<PackedRow>) {
    let tag = ObjectRow {
      kind: Kind::Tag,
      len: 140,
      position: 101,
      pack: packed,
      commit: None,
      tag_target: Some(99),
    };
    let encoded_tag = encode(&tag);
    assert_eq!(decode(&encoded_tag), Some(tag));
    assert_eq!(decode_position(&encoded_tag), Some(101));
    assert_eq!(decode_len(&encoded_tag), Some(140));
    assert_eq!(decode_commit_header(&encoded_tag), None);
    assert_eq!(decode_commit_parents(&encoded_tag), None);
    assert_eq!(decode_tag_target(&encoded_tag), Some(Some(99)));
  }

  /// A-614. Do: encode commit (0, 1, 2, and 4 octopus parents), blob, and annotated tag rows both
  /// whole and in-pack, then read them with the zero-allocation partial decoders. Expect: every
  /// partial decoder agrees identically with full `decode`.
  #[test]
  fn partial_row_decoders_match_full_decode_across_kinds_and_parent_counts() {
    let pack = Some(PackedRow {
      offset: 1234,
      len: 567,
      crc: 0xDEAD_BEEF,
      base: Some(42),
    });
    for packed in [None, pack] {
      for parents in [vec![], vec![7], vec![7, 9], vec![1, 2, 3, 4]] {
        assert_commit_row_partials(packed, parents);
      }
      assert_tag_row_partials(packed);
    }
  }
}
