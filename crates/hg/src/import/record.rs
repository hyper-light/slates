//! A Mercurial record (A-687; §4.17.3 piece 3; §4.17.1 "Durability and residency"; R8): what a repository's entry holds
//! of the revisions it added to the index, an import's or a push's, so a recovery makes the same index again, on this
//! node or on a successor, which cannot read the importing host's directory.
//!
//! **One kind for imports and pushes** (main's ruling, 2026-11-19): the record holds revisions to append to a
//! repository's index, in groups, each one log's revisions in the order they were appended, and the markers after them.
//! An import's record ([`write`], [`Source::Import`]) is every revision its index holds: the changelog's group, the
//! manifest log's, then each file log's in its path's order. A push's whole-text record ([`write_revisions`], the
//! tests' and a made changeset's; a live push writes its changegroup, below) is the
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
//! **A push's changegroup** (A-729 part 2, version 2): a push's record is the changegroup as the client sent it, its
//! version's name and its bytes after the head ([`ChangegroupWriter`]), deltas never texts, so a push's record and the
//! entry its landing appends are the size of what arrived; its replay takes the changegroup again through the live
//! push's own intake (`push::intake`), against the index as the push met it, so it keeps the same rows.
//!
//! **Identities** (A-729 part 2, version 4, an import's; main's ruling): an import's record names each revision's content
//! by identity and holds none of its bytes, since the shard's image restores the content before the records are
//! replayed (A-727 a3). A changeset is its node, parents, flags, length and its text's identity, its text read back from
//! the store at the replay; a manifest or file revision is its row as the index stores it, replayed as it was
//! (`HgIndex::append_stored`). A content the store does not hold refuses the replay ([`Refused::Unheld`]).
//!
//! **A manifest as its delta** (A-729 part 2, version 3, now a made changeset's): after a revision's flags, a form byte: 0 its
//! length and text as below, 1 the node of its delta's base, then the delta's length and bytes, as the index keeps a
//! manifest (A-729 part 1), never rebuilt for the record. The replay rebuilds it from the manifest replayed before it or
//! from the fork, then keeps it as any revision.
//!
//! **The encoding** of version 1, little-endian as git's record is: the magic `SHGR` (a slates Mercurial record) and the version; the
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
use crate::changegroup::Version;
use crate::filerev::{self, MARKER};
use crate::index::{HgIndex, Log, NONE, NewRevision};
use crate::push::intake::{Intake, Stepped};
use crate::push::verify::LogOf;
use crate::read::to_u64;
use crate::{Admission, NODE_LEN, NULL_NODE, Node, changeset, node, obsmarkers};

/// Format: the record's magic, "SHGR" (a slates Mercurial record) little-endian, so bytes of another kind are refused at
/// the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SHGR");

/// Format: the version of a record that holds revisions whole (an import's, a made changeset's).
const VERSION: u32 = 1;
/// Format: the version of a push's record that holds its changegroup as it arrived (A-729 part 2): deltas, never texts.
/// A replay refuses any version but these two.
const CHANGEGROUP_VERSION: u32 = 2;
/// Format: the version of a record of revisions each given whole or as the delta the index keeps it as (A-729 part 2,
/// an import's): a form byte after a revision's flags, and a delta's base after it.
const DELTA_VERSION: u32 = 3;
/// Format: the version of an import's record that names each revision's content by identity and holds none of its bytes
/// (A-729 part 2; main's ruling: since A-727 a3 the content a shard stores survives a restart in its image, recovered
/// before the records, so a record names what the store holds, never a second copy in the log).
const IDENTITY_VERSION: u32 = 4;
/// Format: the form byte of a revision given whole.
const TEXT_FORM: u8 = 0;
/// Format: the form byte of a revision given as its delta, its base's node after the byte.
const DELTA_FORM: u8 = 1;
/// Format: the bytes of a changegroup version's name (`01` to `05`).
const CHANGEGROUP_NAME_BYTES: usize = 2;

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

/// Format: the bytes before the groups: the magic, the version, the source and the room (a made changeset's record
/// has [`MADE_BYTES`] more, after the source).
const HEAD_BYTES: usize = WORD_BYTES + WORD_BYTES + TAG_BYTES + LONG_BYTES;
/// Format: the source byte of a made changeset's record ([`Source::Made`]).
const MADE_TAG: u8 = 2;
/// Format: what a made changeset's record holds after its source byte: the green's key and the version.
const MADE_BYTES: usize = LONG_BYTES + LONG_BYTES;

/// Whose revisions a record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
  /// An import's: every revision of an index made from a store; it replays only into an empty index.
  Import,
  /// A push's: the revisions it brought that the repository lacked, appended after what the index holds.
  Push,
  /// A changeset the server made of a version submitted to a green (A-731), appended as a push's are: the green's key
  /// and the version, so a replay names the changeset made of that version from the record alone, never from a clock.
  Made {
    /// The green's key.
    green: u64,
    /// The version.
    version: u64,
  },
}

impl Source {
  /// Format: the source's byte.
  fn byte(self) -> u8 {
    match self {
      Source::Import => 0,
      Source::Push => 1,
      Source::Made { .. } => MADE_TAG,
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
  /// A revision named by its content's identity (an [`IDENTITY_VERSION`] record) whose content the store does not hold:
  /// a record replayed where its content was never kept, or one whose identity was changed.
  Unheld {
    /// Its revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
  },
  /// A revision given as a delta whose base its log does not hold, or in a log the index keeps whole.
  Base {
    /// Its revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
  },
  /// A revision's delta that does not apply to its base.
  Delta {
    /// Its revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
    /// What applying it found.
    refusal: crate::delta::Refusal,
  },
  /// A push's changegroup refused as the live push's intake refuses it (A-729 part 2).
  Intake(crate::push::intake::Refused),
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
  /// For a made changeset's record (A-731): the green's key, the version, and the changeset the server made of it.
  pub made: Option<(u64, u64, Node)>,
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
  /// The whole text; a revision given as a delta ([`DELTA_VERSION`]), its delta against `base`.
  pub text: &'t [u8],
  /// A revision given as a delta: its base's node (A-729 part 2, a made changeset's manifest against its parent's).
  pub base: Option<Node>,
}

/// One group of a push's record: a log, and its revisions in the order they were appended.
pub type Group<'g> = (Log<'g>, &'g [Revision<'g>]);

/// One revision as the record holds it, read from the index: its node, parents, link (none for a changeset), flags,
/// text length, and the contents its text is made of.
pub(crate) struct Kept {
  /// The node.
  pub(crate) node: Node,
  /// The first parent's node.
  pub(crate) p1: Node,
  /// The second parent's node.
  pub(crate) p2: Node,
  /// The link's node, a manifest's or file revision's.
  pub(crate) link: Option<Node>,
  /// 7.2's index flags.
  pub(crate) flags: u16,
  /// The text's length, a file revision's block and its markers included.
  pub(crate) len: u64,
  /// A file revision's metadata block, written between its markers before the bytes.
  block: Option<ContentId>,
  /// The text, or a file revision's bytes.
  bytes: ContentId,
  /// A manifest kept as a delta: its text, rebuilt from its chain (A-729), where its reader asked for texts.
  rebuilt: Option<Vec<u8>>,
}

/// The record of the index an import made (module docs), `room` the room it ran within: every revision it holds, the
/// changelog's group, the manifest log's, then each file log's in its path's order, and its markers. Refused, nothing
/// is written.
pub fn write(index: &HgIndex, store: &ContentStore, room: u64) -> Result<Vec<u8>, WriteRefused> {
  let mut writer = Writer::begin(index, store, room)?;
  while writer.step(index, store)? {}
  writer.finish().ok_or(WriteRefused::Allocation)
}

/// An import's record written a revision at a time (A-732: written whole at the landing, a large repository's record
/// held its shard past the watchdog's window): [`write`]'s bytes, cut into steps its owner advances within its step
/// budget. A step writes one log's group head with its first revision, or one revision, or the markers last. The
/// record grows as it is written, each part's room reserved before it is put, refused typed where it cannot be.
pub struct Writer {
  /// The logs, each its log's tag, a file log's path, and the number of the first revision the record holds of it: an
  /// import's, the changelog, the manifest log, then each file log in its path's order, each from its first; a push's,
  /// the logs it kept revisions in, each from its first kept.
  logs: Vec<(u8, Vec<u8>, u32)>,
  /// The log being written, and its next revision.
  at: (usize, u32),
  /// The record so far.
  out: Vec<u8>,
  /// Whether the markers are written.
  done: bool,
  /// Whether each revision is named by its content's identity ([`IDENTITY_VERSION`], an import's) or written whole (a
  /// push's, the tests' oracle).
  identities: bool,
}

impl Writer {
  /// Starts the record of `index` (module docs): its head written, `room` the room the import ran within.
  pub fn begin(index: &HgIndex, store: &ContentStore, room: u64) -> Result<Writer, WriteRefused> {
    let mut logs = vec![
      (CHANGELOG_TAG, Vec::new(), 0),
      (MANIFEST_TAG, Vec::new(), 0),
    ];
    logs
      .try_reserve(index.file_logs(store).count())
      .map_err(|_| WriteRefused::Allocation)?;
    logs.extend(
      index
        .file_logs(store)
        .map(|path| (FILE_TAG, path.to_vec(), 0)),
    );
    Writer::of(logs, (Source::Import, room), true)
  }

  /// Starts a push's record (A-680 part 2c, A-720) of the revisions it kept in `logs` (each a log and its first kept
  /// number in the fork), as [`write_kept`] writes it whole, `room` the room its landing runs within.
  pub fn begin_kept(
    logs: &[(crate::push::verify::LogOf, u32)],
    room: u64,
  ) -> Result<Writer, WriteRefused> {
    use crate::push::verify::LogOf;
    let mut kept = Vec::new();
    kept
      .try_reserve(logs.len())
      .map_err(|_| WriteRefused::Allocation)?;
    kept.extend(logs.iter().map(|(log, first)| match log {
      LogOf::Changelog => (CHANGELOG_TAG, Vec::new(), *first),
      LogOf::Manifests => (MANIFEST_TAG, Vec::new(), *first),
      LogOf::File(path) => (FILE_TAG, path.clone(), *first),
    }));
    Writer::of(kept, (Source::Push, room), false)
  }

  /// A writer of `logs` from `source`'s head.
  fn of(
    logs: Vec<(u8, Vec<u8>, u32)>,
    (source, room): (Source, u64),
    identities: bool,
  ) -> Result<Writer, WriteRefused> {
    let mut out = Vec::new();
    out
      .try_reserve(HEAD_BYTES.saturating_add(LONG_BYTES))
      .map_err(|_| WriteRefused::Allocation)?;
    let version = if identities {
      IDENTITY_VERSION
    } else {
      VERSION
    };
    write_head(&mut out, (version, source, room), to_u64(logs.len()));
    let at = (0, logs.first().map_or(0, |(_, _, first)| *first));
    Ok(Writer {
      logs,
      at,
      out,
      done: false,
      identities,
    })
  }

  /// Writes the next part of `index`'s record: whether more follow.
  pub fn step(&mut self, index: &HgIndex, store: &ContentStore) -> Result<bool, WriteRefused> {
    if self.done {
      return Ok(false);
    }
    let (at, rev) = self.at;
    let Some((tag, path, first)) = self.logs.get(at) else {
      let mut markers = Vec::new();
      if self.imports() {
        write_markers(&mut markers, index, store)
          .map_err(|number| WriteRefused::UnreadableMarker { number })?;
      } else {
        // A push's record holds no markers (its obsmarkers are not taken yet): their count, none.
        markers.extend_from_slice(&0_u64.to_le_bytes());
      }
      self.put(&markers)?;
      self.done = true;
      return Ok(false);
    };
    let first = *first;
    let log = match *tag {
      CHANGELOG_TAG => Log::Changelog,
      MANIFEST_TAG => Log::Manifest,
      _ => Log::File(path),
    };
    let radix: Vec<u8> = match *tag {
      CHANGELOG_TAG => CHANGELOG.to_vec(),
      MANIFEST_TAG => MANIFEST.to_vec(),
      _ => [DATA, path.as_slice()].concat(),
    };
    let len = index.len(store, log);
    let mut part = Vec::new();
    if rev == first {
      write_group_head(&mut part, log, u64::from(len.saturating_sub(first)));
    }
    if rev < len {
      let written = if self.identities {
        write_identity_revision(&mut part, (index, store), log, rev)
      } else {
        kept(index, store, log, rev).and_then(|kept| write_kept_revision(&mut part, store, &kept))
      };
      written.ok_or_else(|| unreadable(&radix, rev))?;
      self.at = (at, rev.saturating_add(1));
    }
    if rev.saturating_add(1) >= len {
      let next = at.saturating_add(1);
      self.at = (next, self.logs.get(next).map_or(0, |(_, _, first)| *first));
    }
    self.put(&part)?;
    Ok(true)
  }

  /// Whether the record is an import's, whose markers it holds.
  fn imports(&self) -> bool {
    self
      .out
      .get(WORD_BYTES.saturating_mul(2))
      .is_some_and(|source| *source == Source::Import.byte())
  }

  /// The record, once every part is written.
  pub fn finish(self) -> Option<Vec<u8>> {
    self.done.then_some(self.out)
  }

  /// Appends `part`, its room reserved first.
  fn put(&mut self, part: &[u8]) -> Result<(), WriteRefused> {
    self
      .out
      .try_reserve(part.len())
      .map_err(|_| WriteRefused::Allocation)?;
    self.out.extend_from_slice(part);
    Ok(())
  }
}

/// One kept revision as the record writes it: its node, parents, link (a manifest's or file revision's), flags, length
/// and text; none where its text is not its length.
fn write_kept_revision(out: &mut Vec<u8>, store: &ContentStore, kept: &Kept) -> Option<()> {
  out.extend_from_slice(&kept.node);
  out.extend_from_slice(&kept.p1);
  out.extend_from_slice(&kept.p2);
  if let Some(link) = kept.link {
    out.extend_from_slice(&link);
  }
  out.extend_from_slice(&kept.flags.to_le_bytes());
  out.extend_from_slice(&kept.len.to_le_bytes());
  (write_text(out, store, kept) == kept.len).then_some(())
}

/// One revision as a [`IDENTITY_VERSION`] record names it (A-729 part 2): its node, its parents' nodes, a manifest's or
/// file revision's link's node, 7.2's flags and its text's length; then a changeset's text by its identity, or a
/// manifest's or file revision's row as the index stores it: its delta fields (base number, delta length, chain bytes),
/// its content's identity (the text, a file revision's bytes, or a delta), and a file revision's metadata block's
/// identity behind a byte that says whether it has one. None where the index lacks the row or a parent.
fn write_identity_revision(
  out: &mut Vec<u8>,
  (index, store): (&HgIndex, &ContentStore),
  log: Log<'_>,
  rev: u32,
) -> Option<()> {
  let node_of = |number: u32| {
    if number == NONE {
      return Some(NULL_NODE);
    }
    match log {
      Log::Changelog => index.changeset(store, number).map(|row| row.node),
      _ => index.revision(store, log, number).map(|row| row.node),
    }
  };
  if log == Log::Changelog {
    let row = index.changeset(store, rev)?;
    out.extend_from_slice(&row.node);
    out.extend_from_slice(&node_of(row.p1)?);
    out.extend_from_slice(&node_of(row.p2)?);
    out.extend_from_slice(&row.flags.to_le_bytes());
    out.extend_from_slice(&row.len.to_le_bytes());
    out.extend_from_slice(&row.text.0);
    return Some(());
  }
  let row = index.revision(store, log, rev)?;
  out.extend_from_slice(&row.node);
  out.extend_from_slice(&node_of(row.p1)?);
  out.extend_from_slice(&node_of(row.p2)?);
  out.extend_from_slice(&changeset_node(index, store, row.link)?);
  out.extend_from_slice(&row.flags.to_le_bytes());
  out.extend_from_slice(&row.len.to_le_bytes());
  out.extend_from_slice(&row.base.to_le_bytes());
  out.extend_from_slice(&row.stored.to_le_bytes());
  out.extend_from_slice(&row.chain.to_le_bytes());
  out.extend_from_slice(&row.bytes.0);
  match row.block {
    Some(block) => {
      out.push(1);
      out.extend_from_slice(&block.0);
    }
    None => out.push(0),
  }
  Some(())
}

/// The record of revisions given whole (module docs): a push's, the revisions it appended in `groups`, in their order,
/// then its `markers` each alone in its obsstore's format; `room` the room its landing ran within. Refused, nothing is
/// written.
pub fn write_revisions(
  (source, room): (Source, u64),
  groups: &[Group<'_>],
  markers: &[&[u8]],
) -> Result<Vec<u8>, WriteRefused> {
  write_groups((VERSION, source, room), groups, markers)
}

/// [`write_revisions`] in `version`: [`DELTA_VERSION`] writes each revision's form, its base after a delta's.
fn write_groups(
  (version, source, room): (u32, Source, u64),
  groups: &[Group<'_>],
  markers: &[&[u8]],
) -> Result<Vec<u8>, WriteRefused> {
  let formed = version == DELTA_VERSION;
  let made = if matches!(source, Source::Made { .. }) {
    MADE_BYTES
  } else {
    0
  };
  let mut len = to_u64(HEAD_BYTES)
    .saturating_add(to_u64(LONG_BYTES))
    .saturating_add(to_u64(made));
  for (log, revisions) in groups {
    len = len.saturating_add(group_head_len(*log));
    for revision in *revisions {
      let form = match (formed, revision.base) {
        (false, _) => 0,
        (true, None) => TAG_BYTES,
        (true, Some(_)) => TAG_BYTES.saturating_add(NODE_LEN),
      };
      len = len
        .saturating_add(fixed_len(*log))
        .saturating_add(to_u64(form))
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
  write_head(&mut out, (version, source, room), to_u64(groups.len()));
  for (log, revisions) in groups {
    write_group_head(&mut out, *log, to_u64(revisions.len()));
    for revision in *revisions {
      write_revision_head(&mut out, *log, revision, formed);
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

/// A push's record (A-680 part 2c; [`write_revisions`], [`Source::Push`]): the revisions `verified` brought, in the
/// order its landing appends them (`push::commit::fork_with`: the changelog's group, the manifest log's, then each
/// file log's in the bundle's order), each with flags 0 as the landing keeps it, a changeset's link the null node;
/// `room` the room its landing ran within. No markers: a push's obsmarkers are not taken yet.
pub fn write_push(
  verified: &crate::push::verify::Verified,
  room: u64,
) -> Result<Vec<u8>, WriteRefused> {
  write_verified(verified, Source::Push, room)
}

/// A made changeset's record (A-731, [`Source::Made`]): the revisions the server made of `green`'s `version`, written as
/// [`write_push`] writes a push's, the green's key and the version beside them.
///
/// Written in [`DELTA_VERSION`] (A-729 part 2): its manifest as its delta against `parent`, its first parent's text
/// (`delta::encode_lines`), where it has one, so a made changeset's record holds the lines it changed, never the whole
/// listing; every other revision whole.
pub fn write_made(
  verified: &crate::push::verify::Verified,
  (green, version): (u64, u64),
  parent: Option<&[u8]>,
  room: u64,
) -> Result<Vec<u8>, WriteRefused> {
  let changesets = pushed(&verified.changesets, false);
  let mut deltas: Vec<Vec<u8>> = Vec::new();
  for (manifest, base) in verified
    .manifests
    .iter()
    .zip(core::iter::once(parent).chain(core::iter::repeat(None)))
  {
    let mut delta = Vec::new();
    if let Some(base) = base.filter(|_| manifest.p1 != NULL_NODE) {
      crate::delta::encode_lines(base, &manifest.text, &mut delta)
        .map_err(|_| WriteRefused::Allocation)?;
    }
    deltas.push(delta);
  }
  let manifests: Vec<Revision<'_>> = verified
    .manifests
    .iter()
    .zip(&deltas)
    .zip(core::iter::once(parent).chain(core::iter::repeat(None)))
    .map(|((manifest, delta), base)| {
      let delta_of = base.is_some() && manifest.p1 != NULL_NODE;
      Revision {
        node: manifest.node,
        p1: manifest.p1,
        p2: manifest.p2,
        link: manifest.link,
        flags: 0,
        text: if delta_of { delta } else { &manifest.text },
        base: delta_of.then_some(manifest.p1),
      }
    })
    .collect();
  let files: Vec<(Log<'_>, Vec<Revision<'_>>)> = verified
    .files
    .iter()
    .map(|(path, arrived)| (Log::File(path), pushed(arrived, true)))
    .collect();
  let mut groups: Vec<Group<'_>> = vec![
    (Log::Changelog, changesets.as_slice()),
    (Log::Manifest, manifests.as_slice()),
  ];
  groups.extend(
    files
      .iter()
      .map(|(log, revisions)| (*log, revisions.as_slice())),
  );
  write_groups(
    (DELTA_VERSION, Source::Made { green, version }, room),
    &groups,
    &[],
  )
}

/// [`write_push`] and [`write_made`]'s one writer.
fn write_verified(
  verified: &crate::push::verify::Verified,
  source: Source,
  room: u64,
) -> Result<Vec<u8>, WriteRefused> {
  let changesets = pushed(&verified.changesets, false);
  let manifests = pushed(&verified.manifests, true);
  let files: Vec<(Log<'_>, Vec<Revision<'_>>)> = verified
    .files
    .iter()
    .map(|(path, arrived)| (Log::File(path), pushed(arrived, true)))
    .collect();
  let mut groups: Vec<Group<'_>> = vec![
    (Log::Changelog, changesets.as_slice()),
    (Log::Manifest, manifests.as_slice()),
  ];
  groups.extend(
    files
      .iter()
      .map(|(log, revisions)| (*log, revisions.as_slice())),
  );
  write_revisions((source, room), &groups, &[])
}

/// A push's record (A-680 part 2c's, [`Source::Push`]) written from the fork A-720's intake kept it in: each of `logs`'
/// revisions from its first kept number to the fork's end, in the order kept, read back from the fork's rows (texts
/// whole, as [`write_push`] writes them); `room` the room the landing ran within. Refused, nothing is written.
pub fn write_kept(
  fork: &HgIndex,
  store: &ContentStore,
  logs: &[(crate::push::verify::LogOf, u32)],
  room: u64,
) -> Result<Vec<u8>, WriteRefused> {
  let mut writer = Writer::begin_kept(logs, room)?;
  while writer.step(fork, store)? {}
  writer.finish().ok_or(WriteRefused::Allocation)
}

/// A push's record as its changegroup arrived (A-729 part 2, [`CHANGEGROUP_VERSION`]): the head (the magic, the
/// version, the push's source, the room its intake ran within), the changegroup's version name, its length and its
/// bytes: deltas as the client sent them, never the texts they rebuild, so the record is the size of what arrived. Its
/// replay takes the changegroup again through the live push's intake ([`Replay`]). Written a part a step, so a large
/// push's record never holds its shard past a step: each step copies the changegroup's mean revision's bytes.
pub struct ChangegroupWriter {
  /// The record so far.
  out: Vec<u8>,
  /// The changegroup's bytes copied so far.
  at: usize,
  /// The changegroup's length.
  len: usize,
  /// Derived: the bytes a step copies, the changegroup's length over its revisions (its mean revision's bytes), at
  /// least one.
  part: usize,
}

impl ChangegroupWriter {
  /// Starts the record of a changegroup of `version`, `len` bytes holding `revisions` revisions, `room` the room the
  /// push's intake ran within: the head written, the whole record's room reserved.
  pub fn begin(
    room: u64,
    (version, len): (Version, usize),
    revisions: u64,
  ) -> Result<ChangegroupWriter, WriteRefused> {
    let head = WORD_BYTES
      .saturating_add(WORD_BYTES)
      .saturating_add(TAG_BYTES)
      .saturating_add(LONG_BYTES)
      .saturating_add(CHANGEGROUP_NAME_BYTES)
      .saturating_add(LONG_BYTES);
    let mut out = Vec::new();
    out
      .try_reserve(head.checked_add(len).ok_or(WriteRefused::Allocation)?)
      .map_err(|_| WriteRefused::Allocation)?;
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&CHANGEGROUP_VERSION.to_le_bytes());
    out.push(Source::Push.byte());
    out.extend_from_slice(&room.to_le_bytes());
    out.extend_from_slice(version.name());
    out.extend_from_slice(&to_u64(len).to_le_bytes());
    let revisions = usize::try_from(revisions).unwrap_or(usize::MAX).max(1);
    Ok(ChangegroupWriter {
      out,
      at: 0,
      len,
      part: len.div_ceil(revisions).max(1),
    })
  }

  /// Copies the next part of `payload`, the changegroup [`ChangegroupWriter::begin`] was sized for: whether more
  /// follow.
  pub fn step(&mut self, payload: &[u8]) -> Result<bool, WriteRefused> {
    let end = self.at.saturating_add(self.part).min(self.len);
    let part = payload.get(self.at..end).ok_or(WriteRefused::Allocation)?;
    self.out.extend_from_slice(part);
    self.at = end;
    Ok(self.at < self.len)
  }

  /// The record, once every byte is copied.
  pub fn finish(self) -> Option<Vec<u8>> {
    (self.at == self.len).then_some(self.out)
  }
}

/// A push's changegroup record written whole ([`ChangegroupWriter`]'s steps at once).
pub fn write_changegroup(
  room: u64,
  (version, payload): (Version, &[u8]),
  revisions: u64,
) -> Result<Vec<u8>, WriteRefused> {
  let mut writer = ChangegroupWriter::begin(room, (version, payload.len()), revisions)?;
  while writer.step(payload)? {}
  writer.finish().ok_or(WriteRefused::Allocation)
}

/// One log's arrived revisions as a push's record holds them: flags 0, and a link only where the log has one.
fn pushed(arrived: &[crate::push::verify::Arrived], linked: bool) -> Vec<Revision<'_>> {
  arrived
    .iter()
    .map(|revision| Revision {
      node: revision.node,
      p1: revision.p1,
      p2: revision.p2,
      link: if linked { revision.link } else { NULL_NODE },
      flags: 0,
      text: &revision.text,
      base: None,
    })
    .collect()
}

/// Writes the head: the magic, the version, the source, the room, and the groups' count.
fn write_head(out: &mut Vec<u8>, (version, source, room): (u32, Source, u64), groups: u64) {
  out.extend_from_slice(&MAGIC.to_le_bytes());
  out.extend_from_slice(&version.to_le_bytes());
  out.push(source.byte());
  if let Source::Made { green, version } = source {
    out.extend_from_slice(&green.to_le_bytes());
    out.extend_from_slice(&version.to_le_bytes());
  }
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
fn write_revision_head(out: &mut Vec<u8>, log: Log<'_>, revision: &Revision<'_>, formed: bool) {
  out.extend_from_slice(&revision.node);
  out.extend_from_slice(&revision.p1);
  out.extend_from_slice(&revision.p2);
  if log != Log::Changelog {
    out.extend_from_slice(&revision.link);
  }
  out.extend_from_slice(&revision.flags.to_le_bytes());
  if formed {
    match revision.base {
      None => out.push(TEXT_FORM),
      Some(base) => {
        out.push(DELTA_FORM);
        out.extend_from_slice(&base);
      }
    }
  }
  out.extend_from_slice(&to_u64(revision.text.len()).to_le_bytes());
}

/// The bytes a content's windows hold.
fn window_bytes(store: &ContentStore, root: &ContentId) -> u64 {
  store.windows(root).fold(0, |bytes, window| {
    bytes.saturating_add(to_u64(window.len()))
  })
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
pub(crate) fn kept(index: &HgIndex, store: &ContentStore, log: Log<'_>, rev: u32) -> Option<Kept> {
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
      rebuilt: None,
    });
  }
  let row = index.revision(store, log, rev)?;
  let parent = |number: u32| {
    if number == NONE {
      return Some(NULL_NODE);
    }
    index.revision(store, log, number).map(|parent| parent.node)
  };
  // A manifest kept as a delta (A-729): its text rebuilt from its chain, the one revision's text held.
  let rebuilt = if row.base == NONE {
    None
  } else {
    Some(index.revision_text(store, log, rev)?)
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
    rebuilt,
  })
}

/// The node of changeset `number`, the null node for [`NONE`].
fn changeset_node(index: &HgIndex, store: &ContentStore, number: u32) -> Option<Node> {
  if number == NONE {
    return Some(NULL_NODE);
  }
  index.changeset(store, number).map(|row| row.node)
}

/// Writes a revision's text as 7.2 stores it, a file revision's block between its markers first: the bytes written.
pub(crate) fn write_text(out: &mut Vec<u8>, store: &ContentStore, kept: &Kept) -> u64 {
  let mut written = 0u64;
  let mut put = |bytes: &[u8], written: &mut u64| {
    out.extend_from_slice(bytes);
    *written = written.saturating_add(to_u64(bytes.len()));
  };
  if let Some(text) = &kept.rebuilt {
    put(text, &mut written);
    return written;
  }
  if let Some(block) = kept.block {
    put(MARKER, &mut written);
    for window in store.windows(&block) {
      put(&window, &mut written);
    }
    put(MARKER, &mut written);
  }
  for window in store.windows(&kept.bytes) {
    put(&window, &mut written);
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
      out.extend_from_slice(&window);
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
  /// The text; a revision given as a delta, its delta.
  text: &'a [u8],
  /// A revision given as a delta: its base's node.
  base: Option<Node>,
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
  fn revision(&mut self, linked: bool, formed: bool) -> Result<Read<'a>, Refused> {
    let node = self.node()?;
    let p1 = self.node()?;
    let p2 = self.node()?;
    let link = if linked { self.node()? } else { NULL_NODE };
    let flags =
      <[u8; FLAGS_BYTES]>::try_from(self.take(FLAGS_BYTES)?).map_err(|_| Refused::Truncated)?;
    let base = match formed.then(|| self.byte()).transpose()? {
      None | Some(TEXT_FORM) => None,
      Some(DELTA_FORM) => Some(self.node()?),
      Some(tag) => return Err(Refused::Tag { tag }),
    };
    let text = self.bytes()?;
    Ok(Read {
      node,
      p1,
      p2,
      link,
      flags: u16::from_le_bytes(flags),
      text,
      base,
    })
  }
}

/// The index with a record's revisions appended (module docs), through the import's own steps, into a fork of `index`
/// that takes its place once the whole record is in; its texts kept in `store`. Refused, `index` holds the rows it held
/// and every hold the replay took is given back (a fork may have sealed the index's tail into a table, the same rows); an
/// import's record meeting an index that holds rows is refused before anything is made. The one entry point for an
/// import's record and a push's.
pub fn replay(
  bytes: &[u8],
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<Replayed, Refused> {
  let mut replay = Replay::begin(bytes, index, store)?;
  loop {
    match replay.step(bytes, store) {
      Ok(true) => {}
      Ok(false) => return replay.finish(index, store),
      Err(refused) => {
        return Err(match replay.abandon(store) {
          Ok(()) => refused,
          Err(release) => Refused::Unreleased {
            cause: Box::new(refused),
            release,
          },
        });
      }
    }
  }
}

/// A record of revisions replayed a unit at a time (A-732: a recovery of a large repository's record in one step spun its
/// shard past the watchdog's window at every restart): a step takes one unit, a group's head, one revision of it (kept
/// and appended into the fork as [`replay`] keeps it), one new changeset's manifest held to the manifest log, or the
/// markers and the record's end. Its place in the record is a byte offset, so it borrows nothing between steps.
struct Revisions {
  /// The record's source.
  source: Source,
  /// The room the record was written within.
  room: Admission,
  /// The fork the record is replayed into.
  fork: HgIndex,
  /// The changelog's length before the record: its new changesets are the fork's from here on.
  first: u32,
  /// Where the next unit begins, in the record's bytes.
  at: usize,
  /// The groups still to read.
  groups: u64,
  /// The group being read: its log's tag, its path's place in the record (a file log's), and its revisions still to
  /// replay and in all.
  group: Option<(u8, (usize, usize), u32, u32)>,
  /// The changeset to check next, once the groups are read; past the fork's changelog, the markers are next.
  check: Option<u32>,
  /// Whether the markers and the record's end are read.
  done: bool,
  /// What was replayed.
  replayed: Replayed,
  /// Whether each revision names its form ([`DELTA_VERSION`]).
  formed: bool,
  /// Whether each revision is named by its content's identity ([`IDENTITY_VERSION`]).
  identities: bool,
  /// The manifest replayed last and its text: the next one's base where its delta names it.
  last: Option<(Node, Vec<u8>)>,
}

/// A record replayed a unit at a time (A-732: a recovery of a large repository's record in one step spun its shard past
/// the watchdog's window at every restart): the unit [`replay`] takes whole, cut into steps its owner advances within
/// its step budget, a yield between. A record of revisions ([`VERSION`]) is replayed a group's head, a revision, a
/// manifest's check or the markers a step; a push's changegroup ([`CHANGEGROUP_VERSION`], A-729 part 2) is taken
/// again through the live push's own intake (`push::intake`), a unit a step, against the index as the push met it. Its
/// place in the record is a byte offset, so it borrows nothing between steps: its owner hands it the record's bytes
/// again at each step ([`Replay::step`]). Finished, the fork takes the index's place ([`Replay::finish`]); given up,
/// the fork is given back and the index is as it was ([`Replay::abandon`]).
pub struct Replay {
  /// The record's kind, replayed.
  kind: Kind,
}

/// What a record holds, being replayed.
enum Kind {
  /// Revisions whole.
  Revisions(Box<Revisions>),
  /// A push's changegroup as it arrived.
  Changegroup(Box<Retaken>),
}

/// A push's changegroup taken again through the intake (A-729 part 2).
struct Retaken {
  /// The intake, into its own fork of the index.
  intake: Intake,
  /// Where the changegroup lies in the record's bytes.
  payload: core::ops::Range<usize>,
  /// The room the push's intake ran within.
  room: Admission,
  /// The changelog's and the manifest log's lengths before the record.
  first: (u32, u32),
  /// Whether every unit is taken.
  done: bool,
}

impl Replay {
  /// Starts replaying `bytes` into a fork of `index`: its head read, an import's record held to an empty index, a
  /// push's changegroup framed.
  pub fn begin(
    bytes: &[u8],
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Replay, Refused> {
    let mut reader = Reader { rest: bytes };
    let (version, source, room) = head(&mut reader)?;
    let kind = if version == CHANGEGROUP_VERSION {
      Kind::Changegroup(Box::new(Retaken::begin(
        (bytes, reader),
        room,
        index,
        store,
      )?))
    } else {
      let begun = Revisions::begin((bytes, reader), (version, source, room), index, store)?;
      Kind::Revisions(Box::new(begun))
    };
    Ok(Replay { kind })
  }

  /// Replays the next unit of `bytes`, the record [`Replay::begin`] was given: whether more follow. Refused, its owner
  /// gives the fork back ([`Replay::abandon`]).
  pub fn step(&mut self, bytes: &[u8], store: &mut ContentStore) -> Result<bool, Refused> {
    match &mut self.kind {
      Kind::Revisions(revisions) => revisions.step(bytes, store),
      Kind::Changegroup(retaken) => retaken.step(bytes, store),
    }
  }

  /// The fork in the index's place, what was replayed answered.
  pub fn finish(self, index: &mut HgIndex, store: &mut ContentStore) -> Result<Replayed, Refused> {
    match self.kind {
      Kind::Revisions(revisions) => revisions.finish(index, store),
      Kind::Changegroup(retaken) => retaken.finish(index, store),
    }
  }

  /// Gives the fork back: the index as it was before the replay began.
  pub fn abandon(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    match self.kind {
      Kind::Revisions(revisions) => revisions.abandon(store),
      Kind::Changegroup(retaken) => retaken.intake.abandon(store),
    }
  }
}

impl Retaken {
  /// Frames the changegroup after the head `reader` has read, and begins its intake into a fork of `index`.
  fn begin(
    (bytes, mut reader): (&[u8], Reader<'_>),
    mut room: Admission,
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Retaken, Refused> {
    let name = reader.take(CHANGEGROUP_NAME_BYTES)?;
    let version = Version::from_name(name).map_err(|_| Refused::Tag {
      tag: name.first().copied().unwrap_or_default(),
    })?;
    let payload = reader.bytes()?;
    if !reader.rest.is_empty() {
      return Err(Refused::Trailing);
    }
    let end = bytes.len();
    let start = end.saturating_sub(payload.len());
    let first = (
      index.len(store, Log::Changelog),
      index.len(store, Log::Manifest),
    );
    let intake =
      Intake::begin((version, payload), index, store, &mut room).map_err(Refused::Intake)?;
    Ok(Retaken {
      intake,
      payload: start..end,
      room,
      first,
      done: false,
    })
  }

  /// Takes the next unit of the changegroup in `bytes`: whether more follow.
  fn step(&mut self, bytes: &[u8], store: &mut ContentStore) -> Result<bool, Refused> {
    if self.done {
      return Ok(false);
    }
    let payload = bytes.get(self.payload.clone()).ok_or(Refused::Truncated)?;
    let stepped = self
      .intake
      .step(payload, store, &mut self.room)
      .map_err(Refused::Intake)?;
    self.done = stepped == Stepped::Done;
    Ok(!self.done)
  }

  /// The fork in the index's place, what was replayed answered.
  fn finish(self, index: &mut HgIndex, store: &mut ContentStore) -> Result<Replayed, Refused> {
    let (changesets, manifests) = self.first;
    let (fork, intaken) = match self.intake.finish() {
      Ok(finished) => finished,
      Err(intake) => {
        return Err(match intake.abandon(store) {
          Ok(()) => Refused::Truncated,
          Err(release) => Refused::Unreleased {
            cause: Box::new(Refused::Truncated),
            release,
          },
        });
      }
    };
    let changesets = fork.len(store, Log::Changelog).saturating_sub(changesets);
    let manifests = fork.len(store, Log::Manifest).saturating_sub(manifests);
    let file_logs = intaken
      .logs
      .iter()
      .filter(|(log, _)| matches!(log, LogOf::File(_)))
      .count();
    let file_revisions = intaken
      .kept
      .saturating_sub(u64::from(changesets))
      .saturating_sub(u64::from(manifests));
    let replaced = core::mem::replace(index, fork);
    Ok(Replayed {
      changesets,
      manifests,
      file_logs: to_u64(file_logs),
      file_revisions,
      markers: 0,
      whole: replaced.release(store).is_ok(),
      made: None,
    })
  }
}

impl Revisions {
  /// Starts replaying `bytes`, whose head `reader` has read, into a fork of `index`: an import's record held to an
  /// empty index.
  fn begin(
    (bytes, mut reader): (&[u8], Reader<'_>),
    (version, source, room): (u32, Source, Admission),
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Revisions, Refused> {
    if source == Source::Import && index.rows().rows() != 0 {
      return Err(Refused::NotEmpty);
    }
    let groups = reader.count(GROUP_MIN_BYTES)?;
    let at = bytes.len().saturating_sub(reader.rest.len());
    let fork = index
      .fork(store)
      .map_err(|refused| ImportRefused::<Infallible>::Index(refused.into()))?;
    let first = fork.len(store, Log::Changelog);
    Ok(Revisions {
      source,
      room,
      fork,
      first,
      at,
      groups,
      group: None,
      check: None,
      done: false,
      replayed: Replayed::default(),
      formed: version == DELTA_VERSION,
      identities: version == IDENTITY_VERSION,
      last: None,
    })
  }

  /// Replays the next unit of `bytes`: whether more follow.
  fn step(&mut self, bytes: &[u8], store: &mut ContentStore) -> Result<bool, Refused> {
    if self.done {
      return Ok(false);
    }
    let mut reader = Reader {
      rest: bytes.get(self.at..).ok_or(Refused::Truncated)?,
    };
    self.unit(&mut reader, bytes, store)?;
    self.at = bytes.len().saturating_sub(reader.rest.len());
    Ok(!self.done)
  }

  /// The fork in the index's place, what was replayed answered.
  fn finish(self, index: &mut HgIndex, store: &mut ContentStore) -> Result<Replayed, Refused> {
    let Revisions {
      fork,
      mut replayed,
      done,
      ..
    } = self;
    if !done {
      return Err(match fork.release(store) {
        Ok(()) => Refused::Truncated,
        Err(release) => Refused::Unreleased {
          cause: Box::new(Refused::Truncated),
          release,
        },
      });
    }
    let replaced = core::mem::replace(index, fork);
    replayed.whole = replaced.release(store).is_ok();
    Ok(replayed)
  }

  /// Gives the fork back: the index as it was before the replay began.
  fn abandon(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    self.fork.release(store)
  }

  /// One unit (the type's docs).
  fn unit(
    &mut self,
    reader: &mut Reader<'_>,
    bytes: &[u8],
    store: &mut ContentStore,
  ) -> Result<(), Refused> {
    if let Some((tag, path, left, all)) = self.group {
      return self.revision(reader, bytes, store, (tag, path, left, all));
    }
    if self.groups > 0 {
      self.groups = self.groups.saturating_sub(1);
      return self.group_head(reader, bytes);
    }
    let rev = self.check.unwrap_or(self.first);
    if rev < self.fork.len(store, Log::Changelog) {
      check_manifest(&self.fork, store, rev)?;
      self.check = Some(rev.saturating_add(1));
      return Ok(());
    }
    self.replayed.markers = replay_markers(reader, &mut self.fork, store)?;
    if let Source::Made { green, version } = self.source {
      let node = self
        .fork
        .changeset(store, self.first)
        .map(|row| row.node)
        .ok_or(Refused::Truncated)?;
      self.replayed.made = Some((green, version, node));
    }
    if !reader.rest.is_empty() {
      return Err(Refused::Trailing);
    }
    self.done = true;
    Ok(())
  }

  /// A group's head: its log's tag, a file log's path, and its revisions' count.
  fn group_head(&mut self, reader: &mut Reader<'_>, bytes: &[u8]) -> Result<(), Refused> {
    let tag = reader.byte()?;
    let path = match tag {
      CHANGELOG_TAG | MANIFEST_TAG => (0, 0),
      FILE_TAG => {
        let path = reader.bytes()?;
        let end = bytes.len().saturating_sub(reader.rest.len());
        (end.saturating_sub(path.len()), end)
      }
      tag => return Err(Refused::Tag { tag }),
    };
    let least = if tag == CHANGELOG_TAG {
      CHANGESET_MIN_BYTES
    } else {
      REVISION_MIN_BYTES
    };
    let revisions = reader.revisions(least)?;
    self.group = Some((tag, path, revisions, revisions));
    if revisions == 0 {
      self.group_done(tag, 0);
    }
    Ok(())
  }

  /// One revision of the group being read, kept and appended as [`replay`] keeps it.
  fn revision(
    &mut self,
    reader: &mut Reader<'_>,
    bytes: &[u8],
    store: &mut ContentStore,
    (tag, (start, end), left, all): (u8, (usize, usize), u32, u32),
  ) -> Result<(), Refused> {
    let path = bytes.get(start..end).ok_or(Refused::Truncated)?;
    let (log, radix) = match tag {
      CHANGELOG_TAG => (Log::Changelog, CHANGELOG.to_vec()),
      MANIFEST_TAG => (Log::Manifest, MANIFEST.to_vec()),
      _ => (Log::File(path), [DATA, path].concat()),
    };
    let linked = log != Log::Changelog;
    let rev = self.fork.len(store, log);
    if self.identities {
      self.identified(reader, store, (log, radix.as_slice()), rev)?;
      let left = left.saturating_sub(1);
      self.group = Some((tag, (start, end), left, all));
      if left == 0 {
        self.group_done(tag, all);
      }
      return Ok(());
    }
    let read = reader.revision(linked, self.formed)?;
    if self.formed && linked {
      self.formed_revision(store, (log, radix.as_slice()), (rev, &read))?;
    } else if read.base.is_some() {
      // A changeset is never written as a delta, nor any revision before the form byte.
      return Err(Refused::Base {
        revlog: radix.into_boxed_slice(),
        rev,
      });
    } else if linked {
      replay_revision(&mut self.fork, store, (log, radix.as_slice()), (rev, &read))?;
    } else {
      replay_changeset(&mut self.fork, store, (rev, &read), self.room)?;
    }
    let left = left.saturating_sub(1);
    self.group = Some((tag, (start, end), left, all));
    if left == 0 {
      self.group_done(tag, all);
    }
    Ok(())
  }

  /// One revision of a record that names identities (A-729 part 2): a changeset's text read back from the store by its
  /// identity and replayed as any changeset is (its manifest, branch and closing read from it); a manifest's or file
  /// revision's row appended as it was stored (`HgIndex::append_stored`), its content held again by identity, nothing
  /// read or put. Its node is not hashed again: the text was held to it when it was kept, and the store names it by its
  /// bytes. A content the store does not hold refuses the replay by kind.
  fn identified(
    &mut self,
    reader: &mut Reader<'_>,
    store: &mut ContentStore,
    (log, radix): (Log<'_>, &[u8]),
    rev: u32,
  ) -> Result<(), Refused> {
    let unheld = || Refused::Unheld {
      revlog: Box::from(radix),
      rev,
    };
    let node = reader.node()?;
    let p1 = reader.node()?;
    let p2 = reader.node()?;
    if log == Log::Changelog {
      let flags = flags_of(reader)?;
      let len = reader.long()?;
      let text_id = identity_of(reader)?;
      let text = store.object_bytes(&text_id, len).ok_or_else(unheld)?;
      let read = Read {
        node,
        p1,
        p2,
        link: NULL_NODE,
        flags,
        text: &text,
        base: None,
      };
      return replay_changeset(&mut self.fork, store, (rev, &read), self.room);
    }
    let link = reader.node()?;
    let flags = flags_of(reader)?;
    let len = reader.long()?;
    let stored = (reader.word()?, reader.long()?, reader.long()?);
    let bytes = identity_of(reader)?;
    let block = match reader.byte()? {
      0 => None,
      1 => Some(identity_of(reader)?),
      tag => return Err(Refused::Tag { tag }),
    };
    if store.len_of(&bytes).is_none() || block.is_some_and(|block| store.len_of(&block).is_none()) {
      return Err(unheld());
    }
    let row = NewRevision {
      node,
      p1,
      p2,
      link,
      flags,
      len,
      bytes,
      block,
    };
    let numbers = self
      .fork
      .append_stored(store, log, &[(row, stored)])
      .map_err(|refused| ImportRefused::<Infallible>::Index(refused))?;
    settle::<Infallible>(&mut self.fork, store, radix, rev, &numbers)?;
    Ok(())
  }

  /// One manifest or file revision of a record that names forms, given whole or as its delta against the revision
  /// replayed last or one the fork holds: rebuilt, then kept and appended as [`replay`] keeps it, its text held for the
  /// next.
  fn formed_revision(
    &mut self,
    store: &mut ContentStore,
    (log, radix): (Log<'_>, &[u8]),
    (rev, read): (u32, &Read<'_>),
  ) -> Result<(), Refused> {
    let text = match read.base {
      None => read.text.to_vec(),
      Some(base) => {
        let unheld = || Refused::Base {
          revlog: Box::from(radix),
          rev,
        };
        let held;
        let base_text: &[u8] = match &self.last {
          Some((node, text)) if *node == base => text,
          _ => {
            let number = self.fork.rev(store, log, &base).ok_or_else(unheld)?;
            held = self
              .fork
              .revision_text(store, log, number)
              .ok_or_else(unheld)?;
            &held
          }
        };
        let mut room = self.room;
        crate::delta::apply(base_text, read.text, &mut room).map_err(|refusal| Refused::Delta {
          revlog: Box::from(radix),
          rev,
          refusal,
        })?
      }
    };
    let whole = Read {
      text: &text,
      base: None,
      ..*read
    };
    replay_revision(&mut self.fork, store, (log, radix), (rev, &whole))?;
    self.last = Some((read.node, text));
    Ok(())
  }

  /// A group read whole: counted, and the next group's head next.
  fn group_done(&mut self, tag: u8, revisions: u32) {
    self.group = None;
    self.last = None;
    let replayed = &mut self.replayed;
    match tag {
      CHANGELOG_TAG => replayed.changesets = replayed.changesets.saturating_add(revisions),
      MANIFEST_TAG => replayed.manifests = replayed.manifests.saturating_add(revisions),
      _ => {
        replayed.file_logs = replayed.file_logs.saturating_add(1);
        replayed.file_revisions = replayed.file_revisions.saturating_add(u64::from(revisions));
      }
    }
  }
}

/// A revision's flags, read.
fn flags_of(reader: &mut Reader<'_>) -> Result<u16, Refused> {
  let flags =
    <[u8; FLAGS_BYTES]>::try_from(reader.take(FLAGS_BYTES)?).map_err(|_| Refused::Truncated)?;
  Ok(u16::from_le_bytes(flags))
}

/// A content's identity, read.
fn identity_of(reader: &mut Reader<'_>) -> Result<ContentId, Refused> {
  let bytes = reader.take(slates_vcs::content::IDENTITY_BYTES)?;
  ContentId::from_slice(bytes).ok_or(Refused::Truncated)
}

/// The record's head read: its magic, its version held to the two this build reads, its source (a changegroup's record
/// a push's alone), and the room it was written within.
fn head(reader: &mut Reader<'_>) -> Result<(u32, Source, Admission), Refused> {
  if reader.word()? != MAGIC {
    return Err(Refused::Magic);
  }
  let version = reader.word()?;
  if ![
    VERSION,
    CHANGEGROUP_VERSION,
    DELTA_VERSION,
    IDENTITY_VERSION,
  ]
  .contains(&version)
  {
    return Err(Refused::Version { version });
  }
  let source = match reader.byte()? {
    tag if tag == Source::Import.byte() => Source::Import,
    tag if tag == Source::Push.byte() => Source::Push,
    MADE_TAG => Source::Made {
      green: reader.long()?,
      version: reader.long()?,
    },
    tag => return Err(Refused::Tag { tag }),
  };
  if version == CHANGEGROUP_VERSION && source != Source::Push {
    return Err(Refused::Tag { tag: source.byte() });
  }
  Ok((version, source, Admission::new(reader.long()?)))
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
      let split =
        filerev::split(read.text).map_err(|refusal| ImportRefused::<Infallible>::FileRevision {
          revlog: Box::from(radix),
          rev,
          refusal,
        })?;
      (split.bytes, split.metadata)
    }
    Log::Changelog | Log::Manifest => (read.text, None),
  };
  let mut held = Held::default();
  let outcome =
    keep_revision::<Infallible>(store, &mut held, bytes, block).and_then(|(bytes, block)| {
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
