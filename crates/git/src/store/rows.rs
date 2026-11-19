//! A git repository's rows in its one `slates_vcs::Index` (A-614; §4.17.2 "The repository in git's terms", item 2).
//!
//! **Keys.** An object row's key is [`OBJECT_KEY`] and the object's id; a position row's is [`POSITION_KEY`] and the
//! position, big-endian, so position rows sort as positions do and every object row sorts before every position row.
//!
//! **An object row** names the object's root content (`slates-vcs` holds it while the row lives) and carries, all
//! integers little-endian:
//!
//! ```text
//! row    = kind (u8, the pack's number) | length (u64) | position (u64) | green (u64) | version (u64) [| commit]
//! commit = tree (u64, a position) | date (u64) | generation (u64) | parents (u32) | each parent (u64, a position)
//! ```
//!
//! The green and the version are those whose commit indexed the object: the version that brought it. A commit's part
//! is what git's commit-graph keeps of it [B: gitformat-commit-graph]: its root tree, its committer's date, its
//! generation number (one more than its parents' greatest, one for a root: version 1, "topological levels") and its
//! parents in its own order, each by position. **A position row** names nothing and carries the object's id.

use std::mem::size_of;

use crate::object_name::{Kind, ObjectFormat, ObjectId};

use super::naming::kind_of;

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
  /// The green whose commit indexed it.
  pub green: u64,
  /// That green's version.
  pub version: u64,
  /// A commit's part; `None` for any other type.
  pub commit: Option<CommitRow>,
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

/// The key of `id`'s object row.
pub fn object_key(id: &ObjectId) -> Vec<u8> {
  let mut key = Vec::with_capacity(id.as_bytes().len().saturating_add(1));
  key.push(OBJECT_KEY);
  key.extend_from_slice(id.as_bytes());
  key
}

/// The key of `position`'s row.
pub fn position_key(position: u64) -> Vec<u8> {
  let mut key = Vec::with_capacity(WORD_BYTES.saturating_add(1));
  key.push(POSITION_KEY);
  key.extend_from_slice(&position.to_be_bytes());
  key
}

/// An object row's payload.
pub fn encode(row: &ObjectRow) -> Vec<u8> {
  let mut out = vec![row.kind.pack_number()];
  for word in [row.len, row.position, row.green, row.version] {
    out.extend_from_slice(&word.to_le_bytes());
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
  let (&number, rest) = payload.split_first()?;
  let kind = kind_of(number)?;
  let mut fields = Fields { bytes: rest };
  let (len, position, green, version) = (
    fields.word()?,
    fields.word()?,
    fields.word()?,
    fields.word()?,
  );
  let commit = if kind == Kind::Commit {
    Some(decode_commit(&mut fields)?)
  } else {
    None
  };
  fields.bytes.is_empty().then_some(ObjectRow {
    kind,
    len,
    position,
    green,
    version,
    commit,
  })
}

/// A commit's part, read from `fields`.
fn decode_commit(fields: &mut Fields<'_>) -> Option<CommitRow> {
  let (tree, date, generation) = (fields.word()?, fields.word()?, fields.word()?);
  let count = usize::try_from(fields.count()?).ok()?;
  let mut parents = Vec::new();
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
