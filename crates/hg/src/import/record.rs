//! A Mercurial record (A-687; §4.17.3 piece 3; §4.17.1 "Durability and residency"; R8): what a repository's entry holds
//! of the revisions it added to the index, an import's or a push's, so a recovery makes the same index again, on this
//! node or on a successor, which cannot read the importing host's directory.
//!
//! **One kind for imports and pushes** (main's ruling, 2026-11-19): the record holds revisions to append to a
//! repository's index, in groups, each one log's revisions in the order they were appended, and the markers after them.
//! An import's record ([`write`], [`Source::Import`]) is every revision its index holds: the changelog's group, the
//! manifest log's, then each file log's in its path's order. A push's ([`write_revisions`], [`Source::Push`]) is the
//! revisions it brought that the repository lacked, in the order its landing appended them (`push::commit`), so a
//! recovery appends them where the live push did. [`replay`] is the one entry point for both.
//!
//! **What a revision holds.** Its node, its parents' nodes, a manifest's or file revision's link (the node of the
//! changeset that brought it), 7.2's index flags and its whole text as 7.2 stores it (a file revision's metadata block
//! between its markers, then its bytes): the revisions themselves, never a revlog's deltas or the index's rows, which
//! are made from them again. And the room the writer ran within, which bounds what a changeset's extras take at the
//! replay as it bounded the live path, on every node alike (R8).
//!
//! **Replayed through the import's own steps**, into a fork of the index ([`replay`]): each revision's flags held to
//! what 7.2 knows and its text to its node (sha1dc, so a known collision attack is refused); a changeset's manifest,
//! branch and closing read from its text as the import reads them; a file revision's block split from its bytes; each
//! kept as content and appended at once, numbered its log's next, the tail sealed when due; each new changeset's
//! manifest held to the manifest log; the markers, each read as the obsstore's format reads it, appended in one batch.
//! The fork takes the index's place once the whole record is in, so a refused replay leaves the index and the content
//! store as they were. An import's record replays only into an empty index, as the live landing takes only an empty
//! store. Not checked again: that each path a changeset names has a file log, which the import held its listing to.
//!
//! **The encoding**, little-endian as git's record is: the magic `SHGR` (a slates Mercurial record) and the version; the
//! source (0 an import, 1 a push); the room; the groups' count, then each group's log (0 the changelog, 1 the manifest
//! log, 2 a file log, its path's length and path after it), its revisions' count and each revision: node, parents, link
//! (not a changeset's), flags, text length and text; the markers' count, then each marker's length and bytes. Every
//! count is checked against the bytes that remain before anything is allocated, and bytes after the last marker are
//! refused, each by kind ([`Refused`]), never a panic.

use std::convert::Infallible;

use slates_vcs::{ContentId, ContentStore, IndexRefused};

use super::{
  CHANGELOG, DATA, Held, ImportRefused, MANIFEST, PlannedChangeset, append_changeset, branch_of,
  changeset_refused, keep_revision, refuse_flags, settle,
};
use crate::filerev::{self, MARKER};
use crate::index::{HgIndex, Log, NONE, NewRevision};
use crate::read::to_u64;
use crate::{Admission, NODE_LEN, NULL_NODE, Node, changeset, node, obsmarkers};

/// Format: the record's magic, "SHGR" (a slates Mercurial record) little-endian, so bytes of another kind are refused at
/// the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SHGR");

/// Format: the record's version; a replay refuses any other.
const VERSION: u32 = 1;

/// Format: the bytes of a word: the magic, the version.
const WORD_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a count, a length and the room.
const LONG_BYTES: usize = size_of::<u64>();

/// Format: the bytes of 7.2's index flags.
const FLAGS_BYTES: usize = size_of::<u16>();

/// Format: the fewest bytes a changeset takes: its node, its two parents, its flags and its text's length.
const CHANGESET_MIN_BYTES: usize = NODE_LEN + NODE_LEN + NODE_LEN + FLAGS_BYTES + LONG_BYTES;

/// Format: the fewest bytes a manifest or file revision takes: a changeset's, and its link.
const REVISION_MIN_BYTES: usize = CHANGESET_MIN_BYTES + NODE_LEN;

/// Format: the bytes of a group's log tag and of the source.
const TAG_BYTES: usize = size_of::<u8>();

/// Format: the fewest bytes a group takes: its log's tag and its revisions' count.
const GROUP_MIN_BYTES: usize = TAG_BYTES + LONG_BYTES;

/// Format: each log's tag in a group: the changelog's, the manifest log's, a file log's (its path follows).
const CHANGELOG_TAG: u8 = 0;
/// Format: see [`CHANGELOG_TAG`].
const MANIFEST_TAG: u8 = 1;
/// Format: see [`CHANGELOG_TAG`].
const FILE_TAG: u8 = 2;

/// Format: the fewest bytes a marker takes: its length.
const MARKER_MIN_BYTES: usize = LONG_BYTES;

/// Format: the bytes before the groups: the magic, the version, the source and the room.
const HEAD_BYTES: usize = WORD_BYTES + WORD_BYTES + TAG_BYTES + LONG_BYTES;

/// Whose revisions a record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
  /// An import's: every revision of an index made from a store; it replays only into an empty index.
  Import,
  /// A push's: the revisions it brought that the repository lacked, appended after what the index holds.
  Push,
}

impl Source {
  /// Format: the source's byte.
  fn byte(self) -> u8 {
    match self {
      Source::Import => 0,
      Source::Push => 1,
    }
  }
}

/// Why a record was refused. A refused replay leaves the content store as it found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The bytes end before what they declare.
  Truncated,
  /// Bytes of another kind: the magic is not an import record's.
  Magic,
  /// A version this build does not read.
  Version {
    /// The version read.
    version: u32,
  },
  /// A count past what the bytes that remain could hold, or a log's past what its numbers can name.
  Count,
  /// Bytes after the last marker.
  Trailing,
  /// A source byte or a group's log tag the format does not name.
  Tag {
    /// The byte read.
    tag: u8,
  },
  /// An import's record met an index that holds rows: an import's revisions replay only into an empty index, as its
  /// live landing takes only an empty store.
  NotEmpty,
  /// A revision whose text is not what its node names, or whose hashing shows a known collision attack.
  Node {
    /// Its revlog's radix, as the import names it (`00changelog`, `data/a.txt`).
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
    /// What hashing found.
    refusal: node::Refusal,
  },
  /// A marker that is not one marker alone in a format the obsstore reads.
  Marker {
    /// The marker's number, from 0.
    number: u64,
  },
  /// The import's own step refused: flags 7.2 does not know, a changeset's or file revision's text 7.2 would not read, a
  /// revision numbered other than its log's own, a link or a manifest the index lacks, the index's or the content
  /// store's refusal (its budget among them).
  Import(ImportRefused<Infallible>),
  /// Giving back what a refused replay held was refused too; `cause` is the replay's refusal.
  Unreleased {
    /// The replay's refusal.
    cause: Box<Refused>,
    /// The release's.
    release: IndexRefused,
  },
}

impl From<ImportRefused<Infallible>> for Refused {
  fn from(refused: ImportRefused<Infallible>) -> Self {
    Self::Import(refused)
  }
}

/// Why a record was not written. The index and the content store are as they were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteRefused {
  /// A revision whose row, parents or text the index or the content store no longer holds whole: its revlog's radix and
  /// the revision.
  Unreadable {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
  },
  /// A marker the content store no longer holds.
  UnreadableMarker {
    /// The marker's number.
    number: u64,
  },
  /// The record's length is past what an address can hold, or the allocator refused its room.
  Allocation,
}

/// What a replay appended, and whether the index it took the place of was given back whole.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Replayed {
  /// The changesets.
  pub changesets: u32,
  /// The manifest revisions.
  pub manifests: u32,
  /// The file logs it appended to.
  pub file_logs: u64,
  /// The file revisions, over every file log.
  pub file_revisions: u64,
  /// The obsolescence markers.
  pub markers: u64,
  /// Whether the index the replayed one took the place of was given back whole; not, the content store's count of
  /// holds is off, which its caller counts.
  pub whole: bool,
}

/// One revision a push's record holds ([`write_revisions`]): its node, parents, link (the null node for a changeset,
/// which has none), 7.2's index flags, and its whole text as 7.2 stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revision<'t> {
  /// The node.
  pub node: Node,
  /// The first parent's node.
  pub p1: Node,
  /// The second parent's node.
  pub p2: Node,
  /// The link's node.
  pub link: Node,
  /// 7.2's index flags.
  pub flags: u16,
  /// The whole text.
  pub text: &'t [u8],
}

/// One group of a push's record: a log, and its revisions in the order they were appended.
pub type Group<'g> = (Log<'g>, &'g [Revision<'g>]);

/// One revision as the record holds it, read from the index: its node, parents, link (none for a changeset), flags,
/// text length, and the contents its text is made of.
struct Kept {
  /// The node.
  node: Node,
  /// The first parent's node.
  p1: Node,
  /// The second parent's node.
  p2: Node,
  /// The link's node, a manifest's or file revision's.
  link: Option<Node>,
  /// 7.2's index flags.
  flags: u16,
  /// The text's length, a file revision's block and its markers included.
  len: u64,
  /// A file revision's metadata block, written between its markers before the bytes.
  block: Option<ContentId>,
  /// The text, or a file revision's bytes.
  bytes: ContentId,
}

/// One log being written: which, and its radix as the import names it.
type Written<'p> = (Log<'p>, Vec<u8>);

/// The record of the index an import made (module docs), `room` the room it ran within: every revision it holds, the
/// changelog's group, the manifest log's, then each file log's in its path's order, and its markers. Refused, nothing
/// is written.
pub fn write(index: &HgIndex, store: &ContentStore, room: u64) -> Result<Vec<u8>, WriteRefused> {
  let mut logs: Vec<Written<'_>> = vec![
    (Log::Changelog, CHANGELOG.to_vec()),
    (Log::Manifest, MANIFEST.to_vec()),
  ];
  logs.extend(
    index
      .file_logs(store)
      .map(|path| (Log::File(path), [DATA, path].concat())),
  );
  let len = record_len(index, store, &logs)?;
  let mut out = Vec::new();
  out
    .try_reserve_exact(len)
    .map_err(|_| WriteRefused::Allocation)?;
  write_head(&mut out, (Source::Import, room), to_u64(logs.len()));
  for log in &logs {
    write_group_head(&mut out, log.0, u64::from(index.len(store, log.0)));
    write_log(&mut out, index, store, log)?;
  }
  write_markers(&mut out, index, store)
    .map(|()| out)
    .map_err(|number| WriteRefused::UnreadableMarker { number })
}

/// The record of revisions given whole (module docs): a push's, the revisions it appended in `groups`, in their order,
/// then its `markers` each alone in its obsstore's format; `room` the room its landing ran within. Refused, nothing is
/// written.
pub fn write_revisions(
  (source, room): (Source, u64),
  groups: &[Group<'_>],
  markers: &[&[u8]],
) -> Result<Vec<u8>, WriteRefused> {
  let mut len = to_u64(HEAD_BYTES).saturating_add(to_u64(LONG_BYTES));
  for (log, revisions) in groups {
    len = len.saturating_add(group_head_len(*log));
    for revision in *revisions {
      len = len
        .saturating_add(fixed_len(*log))
        .saturating_add(to_u64(revision.text.len()));
    }
  }
  len = len.saturating_add(to_u64(LONG_BYTES));
  for marker in markers {
    len = len
      .saturating_add(to_u64(MARKER_MIN_BYTES))
      .saturating_add(to_u64(marker.len()));
  }
  let mut out = Vec::new();
  out
    .try_reserve_exact(usize::try_from(len).map_err(|_| WriteRefused::Allocation)?)
    .map_err(|_| WriteRefused::Allocation)?;
  write_head(&mut out, (source, room), to_u64(groups.len()));
  for (log, revisions) in groups {
    write_group_head(&mut out, *log, to_u64(revisions.len()));
    for revision in *revisions {
      write_revision_head(&mut out, *log, revision);
      out.extend_from_slice(revision.text);
    }
  }
  out.extend_from_slice(&to_u64(markers.len()).to_le_bytes());
  for marker in markers {
    out.extend_from_slice(&to_u64(marker.len()).to_le_bytes());
    out.extend_from_slice(marker);
  }
  Ok(out)
}

/// Writes the head: the magic, the version, the source, the room, and the groups' count.
fn write_head(out: &mut Vec<u8>, (source, room): (Source, u64), groups: u64) {
  out.extend_from_slice(&MAGIC.to_le_bytes());
  out.extend_from_slice(&VERSION.to_le_bytes());
  out.push(source.byte());
  out.extend_from_slice(&room.to_le_bytes());
  out.extend_from_slice(&groups.to_le_bytes());
}

/// The bytes a group's head takes: its tag, a file log's path, and its revisions' count.
fn group_head_len(log: Log<'_>) -> u64 {
  let path = match log {
    Log::File(path) => to_u64(LONG_BYTES).saturating_add(to_u64(path.len())),
    Log::Changelog | Log::Manifest => 0,
  };
  to_u64(GROUP_MIN_BYTES).saturating_add(path)
}

/// The fixed bytes a revision of `log` takes before its text.
fn fixed_len(log: Log<'_>) -> u64 {
  to_u64(match log {
    Log::Changelog => CHANGESET_MIN_BYTES,
    Log::Manifest | Log::File(_) => REVISION_MIN_BYTES,
  })
}

/// Writes a group's head: its log's tag (a file log's path after it) and its revisions' count.
fn write_group_head(out: &mut Vec<u8>, log: Log<'_>, count: u64) {
  match log {
    Log::Changelog => out.push(CHANGELOG_TAG),
    Log::Manifest => out.push(MANIFEST_TAG),
    Log::File(path) => {
      out.push(FILE_TAG);
      out.extend_from_slice(&to_u64(path.len()).to_le_bytes());
      out.extend_from_slice(path);
    }
  }
  out.extend_from_slice(&count.to_le_bytes());
}

/// Writes a revision's fixed part: node, parents, link (not a changeset's), flags and text length.
fn write_revision_head(out: &mut Vec<u8>, log: Log<'_>, revision: &Revision<'_>) {
  out.extend_from_slice(&revision.node);
  out.extend_from_slice(&revision.p1);
  out.extend_from_slice(&revision.p2);
  if log != Log::Changelog {
    out.extend_from_slice(&revision.link);
  }
  out.extend_from_slice(&revision.flags.to_le_bytes());
  out.extend_from_slice(&to_u64(revision.text.len()).to_le_bytes());
}

/// The bytes the record takes; refused past what an address can hold, or where a row or a marker is unreadable.
fn record_len(
  index: &HgIndex,
  store: &ContentStore,
  logs: &[Written<'_>],
) -> Result<usize, WriteRefused> {
  let mut len = to_u64(HEAD_BYTES).saturating_add(to_u64(LONG_BYTES));
  for log in logs {
    let revisions = log_len(index, store, log)?;
    len = len
      .checked_add(group_head_len(log.0))
      .and_then(|len| len.checked_add(revisions))
      .ok_or(WriteRefused::Allocation)?;
  }
  let markers =
    markers_len(index, store).map_err(|number| WriteRefused::UnreadableMarker { number })?;
  len = len.checked_add(markers).ok_or(WriteRefused::Allocation)?;
  usize::try_from(len).map_err(|_| WriteRefused::Allocation)
}

/// The bytes one log's revisions take: each revision's fixed part and text (its group's head counts the rest).
fn log_len(
  index: &HgIndex,
  store: &ContentStore,
  (log, radix): &Written<'_>,
) -> Result<u64, WriteRefused> {
  let fixed = to_u64(match log {
    Log::Changelog => CHANGESET_MIN_BYTES,
    Log::Manifest | Log::File(_) => REVISION_MIN_BYTES,
  });
  let mut len = 0u64;
  for rev in 0..index.len(store, *log) {
    let kept = kept(index, store, *log, rev).ok_or_else(|| unreadable(radix, rev))?;
    len = len
      .checked_add(fixed)
      .and_then(|len| len.checked_add(kept.len))
      .ok_or(WriteRefused::Allocation)?;
  }
  Ok(len)
}

/// The bytes the markers take: their count, then each one's length and bytes; refused naming a marker the store lacks.
fn markers_len(index: &HgIndex, store: &ContentStore) -> Result<u64, u64> {
  let mut len = to_u64(LONG_BYTES);
  for number in 0..index.markers_len(store) {
    let marker = index.marker(store, number).ok_or(number)?;
    let bytes = window_bytes(store, &marker);
    len = len
      .saturating_add(to_u64(MARKER_MIN_BYTES))
      .saturating_add(bytes);
  }
  Ok(len)
}

/// The bytes a content's windows hold.
fn window_bytes(store: &ContentStore, root: &ContentId) -> u64 {
  store
    .windows(root)
    .fold(0, |bytes, window| bytes.saturating_add(to_u64(window.len())))
}

/// A record writer's refusal naming `radix` and `rev`.
fn unreadable(radix: &[u8], rev: u32) -> WriteRefused {
  WriteRefused::Unreadable {
    revlog: Box::from(radix),
    rev,
  }
}

/// Revision `rev` of `log` as the record holds it, its parents and link read back to their nodes; `None` where the
/// index does not answer one of them.
fn kept(index: &HgIndex, store: &ContentStore, log: Log<'_>, rev: u32) -> Option<Kept> {
  if log == Log::Changelog {
    let row = index.changeset(store, rev)?;
    let parent = |number: u32| changeset_node(index, store, number);
    return Some(Kept {
      node: row.node,
      p1: parent(row.p1)?,
      p2: parent(row.p2)?,
      link: None,
      flags: row.flags,
      len: row.len,
      block: None,
      bytes: row.text,
    });
  }
  let row = index.revision(store, log, rev)?;
  let parent = |number: u32| {
    if number == NONE {
      return Some(NULL_NODE);
    }
    index.revision(store, log, number).map(|parent| parent.node)
  };
  Some(Kept {
    node: row.node,
    p1: parent(row.p1)?,
    p2: parent(row.p2)?,
    link: Some(changeset_node(index, store, row.link)?),
    flags: row.flags,
    len: row.len,
    block: row.block,
    bytes: row.bytes,
  })
}

/// The node of changeset `number`, the null node for [`NONE`].
fn changeset_node(index: &HgIndex, store: &ContentStore, number: u32) -> Option<Node> {
  if number == NONE {
    return Some(NULL_NODE);
  }
  index.changeset(store, number).map(|row| row.node)
}

/// Writes one log's revisions: each one's node, parents, link (not a changeset's), flags, text length and text.
fn write_log(
  out: &mut Vec<u8>,
  index: &HgIndex,
  store: &ContentStore,
  (log, radix): &Written<'_>,
) -> Result<(), WriteRefused> {
  for rev in 0..index.len(store, *log) {
    let kept = kept(index, store, *log, rev).ok_or_else(|| unreadable(radix, rev))?;
    out.extend_from_slice(&kept.node);
    out.extend_from_slice(&kept.p1);
    out.extend_from_slice(&kept.p2);
    if let Some(link) = kept.link {
      out.extend_from_slice(&link);
    }
    out.extend_from_slice(&kept.flags.to_le_bytes());
    out.extend_from_slice(&kept.len.to_le_bytes());
    if write_text(out, store, &kept) != kept.len {
      return Err(unreadable(radix, rev));
    }
  }
  Ok(())
}

/// Writes a revision's text as 7.2 stores it, a file revision's block between its markers first: the bytes written.
fn write_text(out: &mut Vec<u8>, store: &ContentStore, kept: &Kept) -> u64 {
  let mut written = 0u64;
  let mut put = |bytes: &[u8], written: &mut u64| {
    out.extend_from_slice(bytes);
    *written = written.saturating_add(to_u64(bytes.len()));
  };
  if let Some(block) = kept.block {
    put(MARKER, &mut written);
    for window in store.windows(&block) {
      put(window, &mut written);
    }
    put(MARKER, &mut written);
  }
  for window in store.windows(&kept.bytes) {
    put(window, &mut written);
  }
  written
}

/// Writes the markers: their count, then each one's length and bytes; refused naming a marker the store lacks.
fn write_markers(out: &mut Vec<u8>, index: &HgIndex, store: &ContentStore) -> Result<(), u64> {
  let count = index.markers_len(store);
  out.extend_from_slice(&count.to_le_bytes());
  for number in 0..count {
    let marker = index.marker(store, number).ok_or(number)?;
    out.extend_from_slice(&window_bytes(store, &marker).to_le_bytes());
    for window in store.windows(&marker) {
      out.extend_from_slice(window);
    }
  }
  Ok(())
}

/// The bytes a record has left, read from the front.
struct Reader<'a> {
  /// What remains.
  rest: &'a [u8],
}

/// One revision read from a record: its node, parents, link (a manifest's or file revision's; the null node for a
/// changeset, which has none), flags and text.
struct Read<'a> {
  /// The node.
  node: Node,
  /// The first parent's node.
  p1: Node,
  /// The second parent's node.
  p2: Node,
  /// The link's node.
  link: Node,
  /// 7.2's index flags.
  flags: u16,
  /// The text.
  text: &'a [u8],
}

impl<'a> Reader<'a> {
  /// The next `len` bytes.
  fn take(&mut self, len: usize) -> Result<&'a [u8], Refused> {
    let (taken, rest) = self.rest.split_at_checked(len).ok_or(Refused::Truncated)?;
    self.rest = rest;
    Ok(taken)
  }

  /// The next byte.
  fn byte(&mut self) -> Result<u8, Refused> {
    self
      .take(TAG_BYTES)?
      .first()
      .copied()
      .ok_or(Refused::Truncated)
  }

  /// The next word.
  fn word(&mut self) -> Result<u32, Refused> {
    let bytes =
      <[u8; WORD_BYTES]>::try_from(self.take(WORD_BYTES)?).map_err(|_| Refused::Truncated)?;
    Ok(u32::from_le_bytes(bytes))
  }

  /// The next long.
  fn long(&mut self) -> Result<u64, Refused> {
    let bytes =
      <[u8; LONG_BYTES]>::try_from(self.take(LONG_BYTES)?).map_err(|_| Refused::Truncated)?;
    Ok(u64::from_le_bytes(bytes))
  }

  /// The next node.
  fn node(&mut self) -> Result<Node, Refused> {
    <Node>::try_from(self.take(NODE_LEN)?).map_err(|_| Refused::Truncated)
  }

  /// The next length-prefixed bytes.
  fn bytes(&mut self) -> Result<&'a [u8], Refused> {
    let len = usize::try_from(self.long()?).map_err(|_| Refused::Count)?;
    self.take(len)
  }

  /// A count of items each at least `least` bytes long, refused when the bytes that remain could not hold that many: so
  /// nothing is allocated or walked for more items than the record could hold.
  fn count(&mut self, least: usize) -> Result<u64, Refused> {
    let count = self.long()?;
    let needed = usize::try_from(count)
      .ok()
      .and_then(|count| count.checked_mul(least))
      .ok_or(Refused::Count)?;
    if needed > self.rest.len() {
      return Err(Refused::Count);
    }
    Ok(count)
  }

  /// A log's count of revisions, refused past what a log's numbers can name.
  fn revisions(&mut self, least: usize) -> Result<u32, Refused> {
    u32::try_from(self.count(least)?)
      .ok()
      .filter(|count| *count != NONE)
      .ok_or(Refused::Count)
  }

  /// One revision: a changeset's when `linked` is false, else a manifest's or file revision's.
  fn revision(&mut self, linked: bool) -> Result<Read<'a>, Refused> {
    let node = self.node()?;
    let p1 = self.node()?;
    let p2 = self.node()?;
    let link = if linked { self.node()? } else { NULL_NODE };
    let flags =
      <[u8; FLAGS_BYTES]>::try_from(self.take(FLAGS_BYTES)?).map_err(|_| Refused::Truncated)?;
    let text = self.bytes()?;
    Ok(Read {
      node,
      p1,
      p2,
      link,
      flags: u16::from_le_bytes(flags),
      text,
    })
  }
}

/// The index with a record's revisions appended (module docs), through the import's own steps, into a fork of `index`
/// that takes its place once the whole record is in; its texts kept in `store`. Refused, `index` and `store` are as they
/// were: the fork is released and every hold given back. The one entry point for an import's record and a push's.
pub fn replay(
  bytes: &[u8],
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<Replayed, Refused> {
  let mut fork = index
    .fork(store)
    .map_err(|refused| ImportRefused::<Infallible>::Index(refused.into()))?;
  match replay_into(bytes, &mut fork, store) {
    Ok(mut replayed) => {
      let replaced = core::mem::replace(index, fork);
      replayed.whole = replaced.release(store).is_ok();
      Ok(replayed)
    }
    Err(refused) => Err(match fork.release(store) {
      Ok(()) => refused,
      Err(release) => Refused::Unreleased {
        cause: Box::new(refused),
        release,
      },
    }),
  }
}

/// [`replay`]'s steps into the fork: the head, each group in turn, each new changeset's manifest held to the manifest
/// log, the markers, and nothing after them.
fn replay_into(
  bytes: &[u8],
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<Replayed, Refused> {
  let mut reader = Reader { rest: bytes };
  let (source, room) = head(&mut reader)?;
  if source == Source::Import && index.rows().rows() != 0 {
    return Err(Refused::NotEmpty);
  }
  let first = index.len(store, Log::Changelog);
  let mut replayed = Replayed::default();
  for _ in 0..reader.count(GROUP_MIN_BYTES)? {
    replay_group(&mut reader, (index, store), room, &mut replayed)?;
  }
  for rev in first..index.len(store, Log::Changelog) {
    check_manifest(index, store, rev)?;
  }
  replayed.markers = replay_markers(&mut reader, index, store)?;
  if reader.rest.is_empty() {
    Ok(replayed)
  } else {
    Err(Refused::Trailing)
  }
}

/// The record's head read: its magic and version held to this build's, its source, and the room it was written within.
fn head(reader: &mut Reader<'_>) -> Result<(Source, Admission), Refused> {
  if reader.word()? != MAGIC {
    return Err(Refused::Magic);
  }
  let version = reader.word()?;
  if version != VERSION {
    return Err(Refused::Version { version });
  }
  let source = match reader.byte()? {
    tag if tag == Source::Import.byte() => Source::Import,
    tag if tag == Source::Push.byte() => Source::Push,
    tag => return Err(Refused::Tag { tag }),
  };
  Ok((source, Admission::new(reader.long()?)))
}

/// One group: its log read from its tag, then each revision kept and appended in turn, numbered its log's next, a
/// changeset's within `room`; counted into `replayed`.
fn replay_group(
  reader: &mut Reader<'_>,
  (index, store): (&mut HgIndex, &mut ContentStore),
  room: Admission,
  replayed: &mut Replayed,
) -> Result<(), Refused> {
  let tag = reader.byte()?;
  let (log, radix) = match tag {
    CHANGELOG_TAG => (Log::Changelog, CHANGELOG.to_vec()),
    MANIFEST_TAG => (Log::Manifest, MANIFEST.to_vec()),
    FILE_TAG => {
      let path = reader.bytes()?;
      (Log::File(path), [DATA, path].concat())
    }
    tag => return Err(Refused::Tag { tag }),
  };
  let linked = log != Log::Changelog;
  let least = if linked {
    REVISION_MIN_BYTES
  } else {
    CHANGESET_MIN_BYTES
  };
  let revisions = reader.revisions(least)?;
  for _ in 0..revisions {
    let read = reader.revision(linked)?;
    let rev = index.len(store, log);
    if linked {
      replay_revision(index, store, (log, radix.as_slice()), (rev, &read))?;
    } else {
      replay_changeset(index, store, (rev, &read), room)?;
    }
  }
  match log {
    Log::Changelog => replayed.changesets = replayed.changesets.saturating_add(revisions),
    Log::Manifest => replayed.manifests = replayed.manifests.saturating_add(revisions),
    Log::File(_) => {
      replayed.file_logs = replayed.file_logs.saturating_add(1);
      replayed.file_revisions = replayed
        .file_revisions
        .saturating_add(u64::from(revisions));
    }
  }
  Ok(())
}

/// Holds `read`'s flags to what 7.2 knows and its text to its node, as the import's revlog reader holds them.
fn held_to_node(radix: &[u8], (rev, read): (u32, &Read<'_>)) -> Result<(), Refused> {
  refuse_flags::<Infallible>(radix, rev, read.flags)?;
  node::check(&read.node, &read.p1, &read.p2, read.text).map_err(|refusal| Refused::Node {
    revlog: Box::from(radix),
    rev,
    refusal,
  })
}

/// One changeset kept and appended as the import keeps it: its manifest, branch and closing read from its text within
/// `room`, its text kept as content, and appended numbered `rev`.
fn replay_changeset(
  index: &mut HgIndex,
  store: &mut ContentStore,
  (rev, read): (u32, &Read<'_>),
  mut room: Admission,
) -> Result<(), Refused> {
  held_to_node(CHANGELOG, (rev, read))?;
  let refused = |refusal| changeset_refused::<Infallible>(rev, refusal);
  let decoded = changeset::decode(read.text).map_err(refused)?;
  let manifest = decoded.manifest_node().map_err(refused)?;
  let (branch, closed) = branch_of(&decoded, &mut room).map_err(refused)?;
  let mut held = Held::default();
  let outcome = held.put::<Infallible>(store, read.text).and_then(|text| {
    let planned = PlannedChangeset {
      node: read.node,
      p1: read.p1,
      p2: read.p2,
      flags: read.flags,
      len: to_u64(read.text.len()),
      text,
      manifest,
      closed,
      branch,
    };
    append_changeset(index, store, rev, &planned)
  });
  Ok(held.give_back(store, outcome)?)
}

/// One manifest or file revision kept and appended as the import keeps it: a file revision's block split from its
/// bytes, both kept as content, and appended to `log` numbered `rev`.
fn replay_revision(
  index: &mut HgIndex,
  store: &mut ContentStore,
  (log, radix): (Log<'_>, &[u8]),
  (rev, read): (u32, &Read<'_>),
) -> Result<(), Refused> {
  held_to_node(radix, (rev, read))?;
  let (bytes, block) = match log {
    Log::File(_) => {
      let split = filerev::split(read.text).map_err(|refusal| {
        ImportRefused::<Infallible>::FileRevision {
          revlog: Box::from(radix),
          rev,
          refusal,
        }
      })?;
      (split.bytes, split.metadata)
    }
    Log::Changelog | Log::Manifest => (read.text, None),
  };
  let mut held = Held::default();
  let outcome = keep_revision::<Infallible>(store, &mut held, bytes, block).and_then(|(bytes, block)| {
    let row = NewRevision {
      node: read.node,
      p1: read.p1,
      p2: read.p2,
      link: read.link,
      flags: read.flags,
      len: to_u64(read.text.len()),
      bytes,
      block,
    };
    let numbers = index.append_revisions(store, log, &[row])?;
    settle(index, store, radix, rev, &numbers)
  });
  Ok(held.give_back(store, outcome)?)
}

/// Changeset `rev`'s manifest held to the manifest log: the null node, or one it holds (the import's check).
fn check_manifest(index: &HgIndex, store: &ContentStore, rev: u32) -> Result<(), Refused> {
  let row = index
    .changeset(store, rev)
    .ok_or(ImportRefused::<Infallible>::Unindexed { rev })?;
  if row.manifest == NULL_NODE || index.rev(store, Log::Manifest, &row.manifest).is_some() {
    return Ok(());
  }
  Err(Refused::Import(ImportRefused::UnknownManifest {
    rev,
    node: row.manifest,
  }))
}

/// The markers, each read as the obsstore's format reads it and alone in it, kept as content and appended in one
/// batch, as the import appends them: how many.
fn replay_markers(
  reader: &mut Reader<'_>,
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<u64, Refused> {
  let count = reader.count(MARKER_MIN_BYTES)?;
  let mut held = Held::default();
  let kept = match keep_markers(reader, store, &mut held, count) {
    Ok(kept) => kept,
    Err(refused) => return Err(given_back(held, store, refused)),
  };
  let appended = index
    .append_markers(store, &kept)
    .map(|_| count)
    .map_err(ImportRefused::<Infallible>::from);
  Ok(held.give_back(store, appended)?)
}

/// `count` markers read and kept as content, each held in `held`: their identities, in order.
fn keep_markers(
  reader: &mut Reader<'_>,
  store: &mut ContentStore,
  held: &mut Held,
  count: u64,
) -> Result<Vec<ContentId>, Refused> {
  let mut kept = Vec::new();
  for number in 0..count {
    let marker = reader.bytes()?;
    if !obsmarkers::decode(marker).is_ok_and(|markers| markers.len() == 1) {
      return Err(Refused::Marker { number });
    }
    kept.push(held.put::<Infallible>(store, marker)?);
  }
  Ok(kept)
}

/// `refused`, after every hold in `held` is given back; with the release's refusal beside it where one was refused.
fn given_back(held: Held, store: &mut ContentStore, refused: Refused) -> Refused {
  match held.give_back::<(), Infallible>(store, Ok(())) {
    Err(ImportRefused::Content(release)) => Refused::Unreleased {
      cause: Box::new(refused),
      release: IndexRefused::Content(release),
    },
    // A give-back of a held outcome answers only the content store's refusal.
    Ok(()) | Err(_) => refused,
  }
}
