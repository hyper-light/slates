//! An import's record (A-687; §4.17.3 piece 3; §4.17.1 "Durability and residency"; R8): what the register's entry of an
//! imported store holds, so a recovery makes the same index again, on this node or on a successor, which cannot read
//! the importing host's directory.
//!
//! **What it holds.** Every revision the import kept, in the order it kept them: the changelog, the manifest log, then
//! each file log in its path's order; each by its node, its parents' nodes, a manifest's or file revision's link (the
//! node of the changeset that brought it), 7.2's index flags and its whole text as 7.2 stores it (a file revision's
//! metadata block between its markers, then its bytes). Then the obsolescence markers in the order the index numbers
//! them, each alone in its obsstore's format. And the room the import ran within. The revisions themselves, never a
//! revlog's deltas or the index's rows: a revlog's encoding is the importing host's, and the rows are made from the
//! revisions again.
//!
//! **Written from the import's index** ([`write`]) once the import is done, as git's record is written when its append
//! commits (A-662): its length measured first and its room reserved whole, then each text read back from the content
//! store by the row that names it, refused where the store no longer holds one whole.
//!
//! **Replayed through the import's own steps** ([`replay`]): each revision's flags held to what 7.2 knows and its text
//! to its node, as the revlog reader holds them (sha1dc, so a known collision attack is refused); a changeset's
//! manifest, branch and closing read from its text as the import reads them; a file revision's block split from its
//! bytes; each kept as content and appended at once, numbered its log's own, the tail sealed when due; each changeset's
//! manifest held to the manifest log; the markers, each read as the obsstore's format reads it, appended in one batch.
//! So the replayed index is the import's row for row, and a recomputation that differs is refused (§4.16). The room
//! recorded bounds what a changeset's extras take as it bounded the import, so every node replays within the same
//! bound (R8). A refused replay releases its index and gives back every hold, leaving the content store as it found
//! it.
//!
//! **Not checked again:** that each path a changeset names has a file log. The import held its listing to that; the
//! record is the listing now, and a changeset's paths are its text's, which its node holds.
//!
//! **The encoding**, little-endian as git's record is: the magic and the version; the room; the changesets' count, then
//! each changeset's node, parents, flags, text length and text; the manifests' count, then each manifest's node,
//! parents, link, flags, text length and text; the file logs' count, then each log's path length, path, revisions'
//! count and each revision as a manifest's; the markers' count, then each marker's length and bytes. Every count is
//! checked against the bytes that remain before anything is allocated, and bytes after the last marker are refused,
//! each by kind ([`Refused`]), never a panic.

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

/// Format: the record's magic, "SHIR" (a slates Mercurial import's record) little-endian, so bytes of another kind are
/// refused at the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SHIR");

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

/// Format: the fewest bytes a file log takes: its path's length and its revisions' count.
const FILE_LOG_MIN_BYTES: usize = LONG_BYTES + LONG_BYTES;

/// Format: the fewest bytes a marker takes: its length.
const MARKER_MIN_BYTES: usize = LONG_BYTES;

/// Format: the bytes before the changesets: the magic, the version and the room.
const HEAD_BYTES: usize = WORD_BYTES + WORD_BYTES + LONG_BYTES;

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

/// An index replayed from its record, and what it holds.
#[derive(Debug)]
pub struct Replayed {
  /// The index.
  pub index: HgIndex,
  /// The changesets.
  pub changesets: u32,
  /// The manifest revisions.
  pub manifests: u32,
  /// The file logs.
  pub file_logs: u64,
  /// The file revisions, over every file log.
  pub file_revisions: u64,
  /// The obsolescence markers.
  pub markers: u64,
}

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

/// The record of the index an import made (module docs), `room` the room it ran within. Refused, nothing is written.
pub fn write(index: &HgIndex, store: &ContentStore, room: u64) -> Result<Vec<u8>, WriteRefused> {
  let paths: Vec<&[u8]> = index.file_logs(store).collect();
  let logs: Vec<Written<'_>> = paths
    .iter()
    .map(|path| (Log::File(*path), [DATA, *path].concat()))
    .collect();
  let len = record_len(index, store, &logs)?;
  let mut out = Vec::new();
  out
    .try_reserve_exact(len)
    .map_err(|_| WriteRefused::Allocation)?;
  out.extend_from_slice(&MAGIC.to_le_bytes());
  out.extend_from_slice(&VERSION.to_le_bytes());
  out.extend_from_slice(&room.to_le_bytes());
  write_log(&mut out, index, store, &(Log::Changelog, CHANGELOG.to_vec()))?;
  write_log(&mut out, index, store, &(Log::Manifest, MANIFEST.to_vec()))?;
  out.extend_from_slice(&to_u64(logs.len()).to_le_bytes());
  for log in &logs {
    if let Log::File(path) = log.0 {
      out.extend_from_slice(&to_u64(path.len()).to_le_bytes());
      out.extend_from_slice(path);
    }
    write_log(&mut out, index, store, log)?;
  }
  write_markers(&mut out, index, store)
    .map(|()| out)
    .map_err(|number| WriteRefused::UnreadableMarker { number })
}

/// The bytes the record takes; refused past what an address can hold, or where a row or a marker is unreadable.
fn record_len(
  index: &HgIndex,
  store: &ContentStore,
  logs: &[Written<'_>],
) -> Result<usize, WriteRefused> {
  let mut len = to_u64(HEAD_BYTES);
  let mut add = |bytes: u64| -> Result<(), WriteRefused> {
    len = len.checked_add(bytes).ok_or(WriteRefused::Allocation)?;
    Ok(())
  };
  let core = [
    (Log::Changelog, CHANGELOG.to_vec()),
    (Log::Manifest, MANIFEST.to_vec()),
  ];
  for log in &core {
    add(log_len(index, store, log)?)?;
  }
  add(to_u64(LONG_BYTES))?;
  for log in logs {
    let path = match log.0 {
      Log::File(path) => to_u64(path.len()),
      Log::Changelog | Log::Manifest => 0,
    };
    add(to_u64(LONG_BYTES).saturating_add(path))?;
    add(log_len(index, store, log)?)?;
  }
  let markers = markers_len(index, store).map_err(|number| WriteRefused::UnreadableMarker { number })?;
  add(markers)?;
  usize::try_from(len).map_err(|_| WriteRefused::Allocation)
}

/// The bytes one log takes: its count, then each revision's fixed part and text.
fn log_len(
  index: &HgIndex,
  store: &ContentStore,
  (log, radix): &Written<'_>,
) -> Result<u64, WriteRefused> {
  let fixed = to_u64(match log {
    Log::Changelog => CHANGESET_MIN_BYTES,
    Log::Manifest | Log::File(_) => REVISION_MIN_BYTES,
  });
  let mut len = to_u64(LONG_BYTES);
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

/// Writes one log: its count, then each revision's node, parents, link (not a changeset's), flags, text length and text.
fn write_log(
  out: &mut Vec<u8>,
  index: &HgIndex,
  store: &ContentStore,
  (log, radix): &Written<'_>,
) -> Result<(), WriteRefused> {
  let count = index.len(store, *log);
  out.extend_from_slice(&u64::from(count).to_le_bytes());
  for rev in 0..count {
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

/// What a replay kept: the changesets, the manifests, the file logs, the file revisions over them, the markers.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
  /// The changesets.
  changesets: u32,
  /// The manifest revisions.
  manifests: u32,
  /// The file logs.
  file_logs: u64,
  /// The file revisions, over every file log.
  file_revisions: u64,
  /// The obsolescence markers.
  markers: u64,
}

/// The index a record makes, through the import's own steps, its texts kept in `store` (module docs). Refused, the
/// index is released and every hold given back.
pub fn replay(bytes: &[u8], store: &mut ContentStore) -> Result<Replayed, Refused> {
  let mut index = HgIndex::new();
  match replay_into(bytes, &mut index, store) {
    Ok(counts) => Ok(Replayed {
      index,
      changesets: counts.changesets,
      manifests: counts.manifests,
      file_logs: counts.file_logs,
      file_revisions: counts.file_revisions,
      markers: counts.markers,
    }),
    Err(refused) => Err(match index.release(store) {
      Ok(()) => refused,
      Err(release) => Refused::Unreleased {
        cause: Box::new(refused),
        release,
      },
    }),
  }
}

/// [`replay`]'s steps into `index`, in the import's order: the head, the changelog, the manifest log and each
/// changeset's manifest held to it, the file logs, the markers, and nothing after them.
fn replay_into(
  bytes: &[u8],
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<Counts, Refused> {
  let mut reader = Reader { rest: bytes };
  let room = head(&mut reader)?;
  let mut counts = Counts {
    changesets: replay_changesets(&mut reader, index, store, room)?,
    manifests: replay_manifests(&mut reader, index, store)?,
    ..Counts::default()
  };
  (counts.file_logs, counts.file_revisions) = replay_files(&mut reader, index, store)?;
  counts.markers = replay_markers(&mut reader, index, store)?;
  if reader.rest.is_empty() {
    Ok(counts)
  } else {
    Err(Refused::Trailing)
  }
}

/// The record's head read: its magic and version held to this build's, and the room the import ran within.
fn head(reader: &mut Reader<'_>) -> Result<Admission, Refused> {
  if reader.word()? != MAGIC {
    return Err(Refused::Magic);
  }
  let version = reader.word()?;
  if version != VERSION {
    return Err(Refused::Version { version });
  }
  Ok(Admission::new(reader.long()?))
}

/// The changelog, each changeset kept and appended in turn within `room`: how many.
fn replay_changesets(
  reader: &mut Reader<'_>,
  index: &mut HgIndex,
  store: &mut ContentStore,
  room: Admission,
) -> Result<u32, Refused> {
  let changesets = reader.revisions(CHANGESET_MIN_BYTES)?;
  for rev in 0..changesets {
    let read = reader.revision(false)?;
    replay_changeset(index, store, (rev, &read), room)?;
  }
  Ok(changesets)
}

/// The manifest log, each manifest kept and appended in turn, then each changeset's manifest held to it: how many.
fn replay_manifests(
  reader: &mut Reader<'_>,
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<u32, Refused> {
  let manifests = reader.revisions(REVISION_MIN_BYTES)?;
  for rev in 0..manifests {
    let read = reader.revision(true)?;
    replay_revision(index, store, (Log::Manifest, MANIFEST), (rev, &read))?;
  }
  for rev in 0..index.len(store, Log::Changelog) {
    check_manifest(index, store, rev)?;
  }
  Ok(manifests)
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

/// Every file log the record holds, each revision kept and appended in turn: how many logs, and revisions over them.
fn replay_files(
  reader: &mut Reader<'_>,
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<(u64, u64), Refused> {
  let logs = reader.count(FILE_LOG_MIN_BYTES)?;
  let mut revisions_over_logs = 0u64;
  for _ in 0..logs {
    let path = reader.bytes()?;
    let radix = [DATA, path].concat();
    let revisions = reader.revisions(REVISION_MIN_BYTES)?;
    for rev in 0..revisions {
      let read = reader.revision(true)?;
      replay_revision(index, store, (Log::File(path), &radix), (rev, &read))?;
    }
    revisions_over_logs = revisions_over_logs.saturating_add(u64::from(revisions));
  }
  Ok((logs, revisions_over_logs))
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
