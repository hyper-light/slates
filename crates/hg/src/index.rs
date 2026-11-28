//! The repository's index on the version-control servers' shared tables (A-588 part 2; §4.17.3 "The repository's
//! index"; §4.17.1 "Objects" item 3, held by A-581's `slates-vcs`).
//!
//! Mercurial reads history by revision number, so a repository's objects keep a revlog's shape: the changelog, the
//! manifest log, and a log per file path, each numbering its revisions from 0 in the order the repository took them,
//! with a revision's parents and its link (the changeset that brought it) named by number \[B\]: `internals.revlogs`
//! ("references *always* refer to a previous entry"). Here every log is rows of one `slates-vcs` [`Index`], so the logs
//! are immutable content tables under a tail, shared by a fork, and folded by the shared rule; nothing of a log is held
//! anywhere else. The objects' bytes are content a push's quarantine admitted by [`HgNaming`], whose object rows (the
//! node, its content, its kind and length) [`HgIndex::commit_objects`] writes into the same index.
//!
//! **Rows.** Three kinds, each key beginning with its kind's tag, so a key of one kind never equals a key of another,
//! whatever a path's length (A-649; git's rows keep the same rule, `o` and `p`). A raw node is never a key here: a file
//! log row of a 13-byte path is 20 bytes long, as a node is, so the quarantine's own `commit`, which keys an object's row
//! by its raw id, is never run on these rows.
//! - a log row: `L`, then the log's key (`c` the changelog, `m` the manifest log, `f`, a path and a NUL a file's log; a
//!   path holds no NUL, so one file's rows are contiguous and sort before a longer path's), then the revision's number
//!   big-endian, so a log's rows sort by number. Its payload: the node, the parents' numbers (none, [`NONE`], for the
//!   null node), 7.2's index flags and the text's length; then a changeset's manifest node, whether it closes its
//!   branch, and its branch's name; or a manifest's or file revision's link number and whether it has a metadata block.
//!   Its contents: a changeset's or manifest's text, or a file revision's bytes and then its block.
//! - a node row: `N`, the node, then the log's key: the revision's number in that log. One file node can stand in
//!   several file logs (two paths whose first revisions hold the same bytes hash alike), so a node row names its log.
//! - an object row: `O`, then the node. It names the object's root content, and its payload is the shared store's
//!   object payload, the kind and the length (`slates_vcs::object_payload`), so a node's kind and size are answered
//!   without its log.
//! - a marker row: `M`, then the marker's number, big-endian (A-649): the repository's obsolescence markers numbered in
//!   the order they arrived, as a log numbers its revisions, each naming a content that holds it alone in its format
//!   (`obsmarkers`' encoding: the version byte, then the marker). A marker is a fact never changed once made, so it is
//!   a row; the phase roots and bookmarks move with every push, and the shared index never rewrites a row, so they are
//!   the register's, not the index's.
//! - a log number row: `X`, then the log's key: the log's number (big-endian), given in the order logs were first
//!   sized ([`HgIndex::keep_sized`]), so a sized row names its log in four bytes, not its path.
//! - a sized row: `S`, the log's number and the revision's (each big-endian, so a log's rows are one range in order):
//!   the revision's chunk as a stream sends it, its length (`u32`) and where it begins among the log's chunks (`u64`),
//!   each little-endian: its index entry's two numbers (goal 93, cut 2, the kept rendered length; main 01a20182's
//!   rulings of 2026-11-27 18:56 and 18:58). A revision's chunk length is known where it lands, so the row is inserted
//!   in the same fork as its log row, and a stream names its head and writes its entries with no count walk. Measured
//!   on the red's history: see [`SIZED_ROW_BYTES`].
//!
//! **Appending** ([`HgIndex::append_changesets`], [`HgIndex::append_revisions`]). A group is taken in the order it
//! arrives, as 7.2's `revlog.addgroup` takes a changegroup's \[C\]: a node the log holds already is passed over ("this
//! can happen if two branches make the same change"); any other revision's parents must be in its log already or
//! earlier in its group ("unknown parent"), a manifest's or file revision's link a changeset the changelog holds, and
//! it takes the log's next number. A group is checked whole before any of it is written and written as one batch, so a
//! refused group leaves the index as it was.
//!
//! **A log's length** is never kept apart from its rows: a log's numbers run from 0 without a gap, so its length is the
//! first number with no row, found in twice the logarithm of the length lookups ([`HgIndex::len`]). A fork or a
//! restart reads it from the rows themselves.
//!
//! **Bounds.** Every row is charged to the store's budget by the shared index; a number is 32 bits, with [`NONE`]
//! reserved, so a log full at `u32::MAX` revisions refuses more ([`Refused::LogFull`]).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::{
  Batch, ContentId, ContentStore, Index, IndexRefused, Naming, NewRow, Quarantine, RowRef,
  object_payload, object_row,
};

use crate::node::{self, CollisionAttack};
use crate::{NODE_LEN, NULL_NODE, Node};

/// Format: a revision number's "none", the null parent (7.2's `nullrev`, -1, as an unsigned 32-bit number).
pub const NONE: u32 = u32::MAX;

/// Format: the kind of object a changeset is, as the shared store's object rows record it.
pub const CHANGESET: u8 = 1;

/// Format: the kind of object a manifest is.
pub const MANIFEST: u8 = 2;

/// Format: the kind of object a file revision is.
pub const FILE: u8 = 3;

/// Format: the first byte of a log row's key.
const LOG_ROW: u8 = b'L';

/// Format: the first byte of a node row's key.
const NODE_ROW: u8 = b'N';

/// Format: the first byte of an object row's key, which the node follows.
const OBJECT_ROW: u8 = b'O';

/// Format: the first byte of a marker row's key, which the marker's number follows, big-endian.
const MARKER_ROW: u8 = b'M';

/// Format: the first byte of a sealed chunk of the branch heads' endings' row key, which the chunk's number follows,
/// big-endian ([`Endings`]); before every log row, so no walk of a log's rows meets one.
const ENDINGS_ROW: u8 = b'E';

/// Format: the first byte of a log number row's key, which the log's key follows (module docs).
const NUMBER_ROW: u8 = b'X';

/// Format: the first byte of a sized row's key, which the log's number and the revision's follow, big-endian (module
/// docs).
const SIZED_ROW: u8 = b'S';

/// Format: a sized row's key: its tag, the log's number and the revision's.
const SIZED_KEY_BYTES: usize = 1 + REV_BYTES + REV_BYTES;

/// Format: a sized row's body: the chunk's length (`u32`) and its offset among the log's chunks (`u64`).
const SIZED_BODY_BYTES: usize = REV_BYTES + LEN_BYTES;

/// Shape: what a sized row adds a revision, its key and body before the index table's own framing of a row
/// ([`SIZED_KEY_BYTES`] and [`SIZED_BODY_BYTES`]), the figure main 01a20182's ruling of 2026-11-27 18:58 weighed: about
/// 21 MB a million revisions, against about 50 for a key naming the path.
pub const SIZED_ROW_BYTES: usize = SIZED_KEY_BYTES + SIZED_BODY_BYTES;

/// Derived: what the shared index charges a sized row in its tail: its key and body, the body's one-byte count of
/// contents (`slates_vcs::table::encode_body`, none here) and the tail's slot (`slates_vcs::index::TAIL_SLOT_BYTES`).
/// What a landing lends for its sized rows ([`HgIndex::sized_charge`]).
pub const SIZED_ROW_CHARGE: usize = SIZED_ROW_BYTES + 1 + slates_vcs::index::TAIL_SLOT_BYTES;

/// Derived: what the shared index charges a log's number row in its tail, its path's bytes apart: its tag and a file
/// log key's two fixed bytes ([`FILE_KEY`], [`PATH_END`]), its body (the number) and the body's count of contents, and
/// the tail's slot.
const NUMBER_ROW_CHARGE: usize = 1 + 2 + REV_BYTES + 1 + slates_vcs::index::TAIL_SLOT_BYTES;

/// Format: the changelog's log key.
const CHANGELOG_KEY: u8 = b'c';

/// Format: the manifest log's log key.
const MANIFEST_KEY: u8 = b'm';

/// Format: the first byte of a file log's key, before its path and a NUL.
const FILE_KEY: u8 = b'f';

/// Format: the byte after a file log's path.
const PATH_END: u8 = 0;

/// Format: the bytes of a revision number.
const REV_BYTES: usize = 4;

/// Format: the bytes of 7.2's index flags.
const FLAGS_BYTES: usize = 2;

/// Format: the bytes of a text's length.
const LEN_BYTES: usize = 8;

/// Format: the payload every log row begins with: the node, two parents, the flags and the length.
const COMMON_BYTES: usize = NODE_LEN + REV_BYTES + REV_BYTES + FLAGS_BYTES + LEN_BYTES;

/// Format: what a changeset row's payload holds after the common part, before its branch: the manifest's node and
/// whether it closes its branch.
const CHANGESET_BYTES: usize = COMMON_BYTES + NODE_LEN + 1;

/// Format: a manifest's or file revision's row payload: the common part, the link, whether it has a block, then its
/// delta's base (A-729: a manifest's previous revision, [`NONE`] for a whole text), its content's length when it is a
/// delta, and its chain's delta bytes since the whole text it rests on.
const REVISION_BYTES: usize = COMMON_BYTES + REV_BYTES + 1 + REV_BYTES + LEN_BYTES + LEN_BYTES;

#[cfg(test)]
std::thread_local! {
  /// Non-vacuity counter: each previous manifest text an append rebuilt from its chain, not handed by its caller.
  pub(crate) static PREVIOUS_REBUILT: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// A log of the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Log<'a> {
  /// The changelog.
  Changelog,
  /// The manifest log (flat manifests: one).
  Manifest,
  /// The log of the file at a path.
  File(&'a [u8]),
}

impl Log<'_> {
  /// Appends the log's key.
  fn put_key(&self, out: &mut Vec<u8>) {
    match self {
      Self::Changelog => out.push(CHANGELOG_KEY),
      Self::Manifest => out.push(MANIFEST_KEY),
      Self::File(path) => {
        out.push(FILE_KEY);
        out.extend_from_slice(path);
        out.push(PATH_END);
      }
    }
  }

  /// The bytes [`Log::put_key`] appends.
  fn key_len(&self) -> usize {
    match self {
      Self::Changelog | Self::Manifest => 1,
      Self::File(path) => path.len().saturating_add(2),
    }
  }

  /// The key of this log's row numbered `rev`, made at its length at once: a key grown from one byte was two allocations
  /// a lookup, and a push's chain walks look up every link (main's churn of hg's 57k push at e51d998bf: 147 MB under
  /// `put_key`).
  fn row_key(&self, rev: u32) -> Vec<u8> {
    let rev = rev.to_be_bytes();
    let mut key = Vec::with_capacity(self.key_len().saturating_add(rev.len()).saturating_add(1));
    key.push(LOG_ROW);
    self.put_key(&mut key);
    key.extend_from_slice(&rev);
    key
  }

  /// The key of `node`'s row in this log, made at its length at once.
  fn node_key(&self, node: &Node) -> Vec<u8> {
    let mut key = Vec::with_capacity(self.key_len().saturating_add(node.len()).saturating_add(1));
    key.push(NODE_ROW);
    key.extend_from_slice(node);
    self.put_key(&mut key);
    key
  }
}

/// A changeset to append: its node, parents, 7.2's index flags, its text's length and content, and what the changelog
/// row keeps of its text (its manifest's node, whether it closes its branch, its branch's name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewChangeset<'a> {
  /// The node.
  pub node: Node,
  /// The first parent's node.
  pub p1: Node,
  /// The second parent's node.
  pub p2: Node,
  /// 7.2's index flags.
  pub flags: u16,
  /// The text's length.
  pub len: u64,
  /// The text, held as content.
  pub text: ContentId,
  /// The manifest's node.
  pub manifest: Node,
  /// Whether it closes its branch (the `close` extra).
  pub closed: bool,
  /// Its branch's name (the `branch` extra, `default` without one).
  pub branch: &'a [u8],
}

/// A manifest or file revision to append: its node, parents, link (the changeset that brought it), 7.2's index flags,
/// its length, and its content: a manifest's text, or a file revision's bytes and its metadata block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewRevision {
  /// The node.
  pub node: Node,
  /// The first parent's node.
  pub p1: Node,
  /// The second parent's node.
  pub p2: Node,
  /// The node of the changeset that brought it.
  pub link: Node,
  /// 7.2's index flags.
  pub flags: u16,
  /// The text's length, its block included.
  pub len: u64,
  /// The text, or a file revision's bytes, held as content.
  pub bytes: ContentId,
  /// A file revision's metadata block, held as content, when it has one.
  pub block: Option<ContentId>,
}

/// A changelog row, read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangesetRow<'s> {
  /// The node.
  pub node: Node,
  /// The first parent's number, or [`NONE`].
  pub p1: u32,
  /// The second parent's number, or [`NONE`].
  pub p2: u32,
  /// 7.2's index flags.
  pub flags: u16,
  /// The text's length.
  pub len: u64,
  /// The text.
  pub text: ContentId,
  /// The manifest's node.
  pub manifest: Node,
  /// Whether it closes its branch.
  pub closed: bool,
  /// Its branch's name.
  pub branch: &'s [u8],
}

/// A revision's chunk as a stream sends it, kept where the revision lands (goal 93, cut 2, main 01a20182's rulings of
/// 2026-11-27 18:56 and 18:58: the kept rendered length): its chunk's length and where it begins among its log's chunks,
/// the two numbers its index entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSpan {
  /// Where its chunk begins among its log's chunks.
  pub offset: u64,
  /// Its chunk's length.
  pub len: u32,
}

/// What a stream's head names (7.2's `generatev1`), kept as revisions land: its files and their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
  /// The files.
  pub files: u64,
  /// Their bytes.
  pub bytes: u64,
}

/// A revision's place as the changeset store keeps it, to be sized where it lands ([`HgIndex::keep_sized`]): its bytes as
/// they lie, sized as the stream compresses them, or a revlog chunk already, sized by its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed<'b> {
  /// Its delta or whole as it is.
  Raw(&'b [u8]),
  /// A revlog chunk as stock stores it, this long.
  Chunk(u64),
}

/// A log's row as a stream's index entry reads it ([`HgIndex::numbers_from`]): its number, its parents' and its link's
/// numbers (a changeset links to itself), its node, its flags and its text's length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryNumbers {
  /// Its number in its log.
  pub rev: u32,
  /// Its parents' numbers, [`NONE`] for none, as the rows keep them.
  pub parents: [u32; 2],
  /// The changeset's number it links to.
  pub link: u32,
  /// Its node.
  pub node: Node,
  /// Its flags.
  pub flags: u16,
  /// Its text's length.
  pub len: u64,
}

/// A manifest's or file revision's row, read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevisionRow {
  /// The node.
  pub node: Node,
  /// The first parent's number in its log, or [`NONE`].
  pub p1: u32,
  /// The second parent's number in its log, or [`NONE`].
  pub p2: u32,
  /// The number of the changeset that brought it.
  pub link: u32,
  /// 7.2's index flags.
  pub flags: u16,
  /// The text's length, its block included.
  pub len: u64,
  /// The text, or a file revision's bytes; a manifest kept as a delta, the delta (A-729: [`HgIndex::revision_text`]
  /// reads the text).
  pub bytes: ContentId,
  /// A file revision's metadata block.
  pub block: Option<ContentId>,
  /// The revision its content is a delta against, [`NONE`] for a whole text.
  pub base: u32,
  /// The delta's length, where its content is one.
  pub stored: u64,
  /// The delta bytes of its chain since the whole text it rests on.
  pub chain: u64,
}

/// Why a group was refused; nothing of it was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// A parent neither in the log nor earlier in the group (7.2: "unknown parent"; §4.17.3 `MissingObject`).
  MissingParent {
    /// The parent's node.
    node: Node,
  },
  /// A link that names no changeset the changelog holds (§4.17.3 `MissingObject`).
  MissingLink {
    /// The changeset's node.
    node: Node,
  },
  /// The log holds `u32::MAX` revisions already, the most a number can name beside [`NONE`].
  LogFull,
  /// A manifest's text the index could not read back to keep the next one as its delta (A-729): its number.
  Unread {
    /// The revision.
    rev: u32,
  },
  /// The shared index refused (its budget, or a row it holds that differs).
  Index(IndexRefused),
  /// An object whose id is not a node's twenty bytes.
  NotANode {
    /// The id's length.
    len: u64,
  },
  /// A sizing out of its log's order ([`HgIndex::keep_sized`]): revision `rev` is sized already, or is past the log's
  /// last revision.
  SizedOutOfOrder {
    /// The revision.
    rev: u32,
  },
  /// A chunk longer than an index entry can name (`u32`), or one the compressor refused to size.
  Unsizable {
    /// The revision.
    rev: u32,
  },
  /// The kept totals ran past their arithmetic: a sizing that does not follow what was kept.
  RenderedOverflow,
}

impl From<IndexRefused> for Refused {
  fn from(refused: IndexRefused) -> Self {
    Self::Index(refused)
  }
}

/// Why [`HgNaming`] would not name an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamingRefused {
  /// A context shorter than two parents.
  Context {
    /// Its length.
    len: u64,
  },
  /// A known SHA-1 collision attack in the parents and text (7.2: "sha1 collision attack detected").
  CollisionAttack(CollisionAttack),
}

/// Mercurial's naming for the shared store's arrival check: a node, the SHA-1 of the parents and the text, with
/// collision detection ([`node::compute_prefixed`]). Its context is the two parents, then whatever the text holds before
/// the object's own bytes: a file revision's metadata block between its markers ([`naming_context`]), so a file
/// revision's content is the file's bytes alone, the same content as the file in every tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HgNaming;

impl Naming for HgNaming {
  type Id = Node;
  type Refused = NamingRefused;

  fn name(&self, _kind: u8, context: &[u8], bytes: &[u8]) -> Result<Node, NamingRefused> {
    let short = || NamingRefused::Context {
      len: crate::read::to_u64(context.len()),
    };
    let (p1, rest) = context.split_first_chunk::<NODE_LEN>().ok_or_else(short)?;
    let (p2, prefix) = rest.split_first_chunk::<NODE_LEN>().ok_or_else(short)?;
    node::compute_prefixed(p1, p2, prefix, bytes).map_err(NamingRefused::CollisionAttack)
  }
}

/// Appends the context [`HgNaming`] reads: the parents, then a file revision's metadata block between its markers when
/// it has one.
pub fn naming_context(p1: &Node, p2: &Node, block: Option<&[u8]>, out: &mut Vec<u8>) {
  out.extend_from_slice(p1);
  out.extend_from_slice(p2);
  if let Some(block) = block {
    out.extend_from_slice(crate::filerev::MARKER);
    out.extend_from_slice(block);
    out.extend_from_slice(crate::filerev::MARKER);
  }
}

/// A revision a group will add: its log row's payload and contents, its number, and its node.
struct Planned {
  /// Its number.
  rev: u32,
  /// Its node.
  node: Node,
  /// Its log row's payload.
  payload: Vec<u8>,
  /// Its log row's contents.
  contents: Vec<ContentId>,
}

/// Every branch's heads in the changelog an index holds, by 7.2's rule (`crate::serve::discovery::branch_head_revs`:
/// "a branchhead is a revision such that no descendant is on the same branch", closed heads kept), kept up to date as
/// each changeset is appended (goal 93, main's ruling of 2026-11-23: `branchmap`, a `lookup` of a branch name and `heads`
/// read them in O(branches), where each walked the whole history in one step: 155-161 ms of CPU on CI's x86 runner, a
/// 348 ms step). Every changeset reaches an index through [`HgIndex::append_changesets`], a push's intake, an import, a
/// re-base, a replay and a made changeset alike, a revision a step, so keeping them costs each append its own revision's
/// work and no step of its own.
///
/// **Adding a revision** `r` on branch `b`: a parent on `b` that was a head is one no more (it has a child on `b`); a
/// head of `b` that `r` descends from through other branches is one no more either, so from each parent on another
/// branch its ancestors are walked down to `b`'s lowest head, the heads met ended (7.2's `uncertain` walk, from its
/// highest such parent to `floorrev`). A parent on `b` that was no head ends nothing: whatever it descends from on `b`
/// had a descendant on `b` already. Then `r` is a head. No head is ever made by another branch's revision, and none comes
/// back, so the heads after each append are the rule's over the changelog so far.
///
/// **A view that leaves changesets out** (goal 93, evolution: a hidden or secret changeset, every one at or after the
/// view's first changeset that is not public, `first`). Every changeset below `first` is served, so the served heads
/// are the rule's over the changelog's first `first` revisions with the served ones after appended ([`Self::served`]).
/// Those revisions' heads are today's heads below `first` and every head a revision at or after `first` ended, since a
/// head ended once is never one again: each append's endings are kept in the order they happen ([`Endings`]), so they
/// are read back from `first` on alone. The served answer then costs the tail's endings and the served tail's appends,
/// never a walk of the history. A full chunk of endings is sealed into a row of the index ([`Endings`]), so a fork
/// shares the sealed ones as it shares every row, by the tables' identity, and copies only the open chunk.
#[derive(Clone, Debug, Default)]
pub struct BranchHeads {
  /// Each branch's heads, by number.
  heads: BTreeMap<Vec<u8>, BTreeSet<u32>>,
  /// Every head an append ended, in append order.
  endings: Endings,
  /// How many times they answered for a view (`crate::serve::Repository::maintained_branch_heads`): the non-vacuity
  /// count a test reads, so a fallback to the walk cannot pass silently.
  answered: core::cell::Cell<u64>,
  /// The most revisions one append's walk through other branches read (main's order of 2026-11-23: a walk down to an
  /// old head is one append's work, so it is counted, and sliced where it runs past a step's worth).
  longest_walk: u64,
}

impl BranchHeads {
  /// Every branch's heads, by number, lowest first, as `branch_head_revs` lists them; counted as an answer.
  pub fn answer(&self) -> Vec<u32> {
    self.answered.set(self.answered.get().saturating_add(1));
    let mut revs: Vec<u32> = self.heads.values().flatten().copied().collect();
    revs.sort_unstable();
    revs
  }

  /// How many times they answered.
  pub fn answered(&self) -> u64 {
    self.answered.get()
  }

  /// The most revisions one append's walk through other branches read, over the index's life (its forks' included).
  pub fn longest_walk(&self) -> u64 {
    self.longest_walk
  }

  /// Notes changeset `rev` on `branch` with parents `p1` and `p2` (numbers, [`NONE`] for none) appended to `index`,
  /// every changeset before it held already (module docs of [`BranchHeads`]).
  fn add(
    &mut self,
    (index, store): (&HgIndex, &ContentStore),
    (rev, branch, parents): (u32, &[u8], [u32; 2]),
  ) -> Result<(), Refused> {
    let endings = &mut self.endings;
    let walked = note(
      &mut self.heads,
      (index, store),
      (rev, branch, parents),
      |ended| endings.push((rev, ended)),
    )?;
    self.longest_walk = self.longest_walk.max(walked);
    Ok(())
  }

  /// The served view's heads where it serves every changeset below `first` and, from `first` on, the changelog's
  /// revisions `served` alone, in order (module docs, "A view that leaves changesets out"), by changelog number, lowest
  /// first; counted as an answer. Refused for a row `index` cannot read.
  pub fn served(
    &self,
    (index, store): (&HgIndex, &ContentStore),
    first: u32,
    served: impl Iterator<Item = u32>,
  ) -> Result<Vec<u32>, Refused> {
    let mut heads: BTreeMap<Vec<u8>, BTreeSet<u32>> = BTreeMap::new();
    for (branch, revs) in &self.heads {
      let below: BTreeSet<u32> = revs.range(..first).copied().collect();
      if !below.is_empty() {
        heads.insert(branch.clone(), below);
      }
    }
    for (_, ended) in self.endings.from((index, store), first)? {
      if ended >= first {
        continue;
      }
      let row = index
        .changeset(store, ended)
        .ok_or(Refused::Unread { rev: ended })?;
      heads.entry(row.branch.to_vec()).or_default().insert(ended);
    }
    for rev in served {
      let row = index.changeset(store, rev).ok_or(Refused::Unread { rev })?;
      note(
        &mut heads,
        (index, store),
        (rev, row.branch, [row.p1, row.p2]),
        |_| {},
      )?;
    }
    self.answered.set(self.answered.get().saturating_add(1));
    let mut revs: Vec<u32> = heads.values().flatten().copied().collect();
    revs.sort_unstable();
    Ok(revs)
  }
}

/// Notes changeset `rev` on `branch` with parents `parents` appended to `by_branch`'s heads (module docs of
/// [`BranchHeads`]), every head it ends handed to `ended`: how many revisions its walk through other branches read.
fn note(
  by_branch: &mut BTreeMap<Vec<u8>, BTreeSet<u32>>,
  (index, store): (&HgIndex, &ContentStore),
  (rev, branch, parents): (u32, &[u8], [u32; 2]),
  mut ended: impl FnMut(u32),
) -> Result<u64, Refused> {
  let mut walked = 0;
  let heads = by_branch.entry(branch.to_vec()).or_default();
  let mut others: Vec<u32> = Vec::new();
  for parent in parents.into_iter().filter(|parent| *parent != NONE) {
    let row = index
      .changeset(store, parent)
      .ok_or(Refused::Unread { rev: parent })?;
    if row.branch == branch {
      if heads.remove(&parent) {
        ended(parent);
      }
    } else {
      others.push(parent);
    }
  }
  if let Some(&floor) = heads.first().filter(|_| !others.is_empty()) {
    walked = walk_ended(heads, (index, store), (&others, floor), &mut ended)?;
  }
  heads.insert(rev);
  Ok(walked)
}

/// Ends each of a branch's `heads` that an ancestor of a parent from another branch (`others`) is, walking those
/// ancestors down to the branch's lowest head `floor`, each once from the highest (every head among them has the
/// appended revision for a descendant on its branch), each head ended handed to `ended`: how many revisions it read.
fn walk_ended(
  heads: &mut BTreeSet<u32>,
  (index, store): (&HgIndex, &ContentStore),
  (others, floor): (&[u32], u32),
  ended: &mut impl FnMut(u32),
) -> Result<u64, Refused> {
  let mut walked = 0_u64;
  let mut reach: BTreeSet<u32> = others
    .iter()
    .copied()
    .filter(|parent| *parent >= floor)
    .collect();
  while let Some(at) = reach.pop_last() {
    if at < floor {
      break;
    }
    walked = walked.saturating_add(1);
    if heads.remove(&at) {
      ended(at);
    }
    let row = index
      .changeset(store, at)
      .ok_or(Refused::Unread { rev: at })?;
    reach.extend(
      [row.p1, row.p2]
        .into_iter()
        .filter(|parent| *parent != NONE && *parent >= floor),
    );
    if heads.is_empty() {
      break;
    }
  }
  Ok(walked)
}

/// Shape: the most endings a sealed chunk of [`Endings`] holds: what a fork copies of them at most, 4 KiB. Fewer where
/// the store's window is smaller, since a row is at most a window's table ([`endings_a_chunk`]).
const ENDINGS_CHUNK: usize = 512;

/// Shape: a bound on a sealed chunk's row beyond its endings: a table's frame, the row's offset, its key's length, its
/// key and its body's count (42 bytes, `slates_vcs`'s table encoding), rounded up.
const ENDINGS_ROW_FRAME: usize = 64;

/// Format: the bytes of one ending in a sealed chunk's row: the appended revision, then the head, each little-endian.
const ENDING_BYTES: usize = 8;

/// One head an append ended: the appended revision, and the head.
type Ending = (u32, u32);

/// Every head the changelog's appends ended, in append order, so by appended revision ([`BranchHeads`], "A view that
/// leaves changesets out"): each full chunk of [`ENDINGS_CHUNK`] sealed into a row of the index (its key
/// [`ENDINGS_ROW`] and the chunk's number), immutable once written, so the index owns it once and a fork shares it by
/// the tables' identity; and the open chunk, which a fork copies. About one ending a changeset: an append ends its
/// parent on its branch at most, and a walk's ending is a merge's.
#[derive(Clone, Debug, Default)]
struct Endings {
  /// How many chunks are sealed: their rows are numbered from 0.
  sealed: u32,
  /// The endings after them.
  open: Vec<Ending>,
}

impl Endings {
  /// Keeps `ending`, after every one before it.
  fn push(&mut self, ending: Ending) {
    self.open.push(ending);
  }

  /// The endings by revisions at or after `rev` that `index` holds, in their order: the sealed chunks from the first
  /// whose last ending is by `rev` or after (found by a search of the chunks' rows), then the open chunk. Refused for a
  /// sealed chunk's row `index` does not hold whole.
  fn from(
    &self,
    (index, store): (&HgIndex, &ContentStore),
    rev: u32,
  ) -> Result<Vec<Ending>, Refused> {
    let chunk = |number: u32| -> Result<Vec<Ending>, Refused> {
      let row = index
        .rows
        .get(store, &endings_key(number))
        .ok_or(Refused::Unread { rev })?;
      let (pairs, rest) = row.payload().as_chunks::<ENDING_BYTES>();
      if !rest.is_empty() || pairs.is_empty() {
        return Err(Refused::Unread { rev });
      }
      Ok(
        pairs
          .iter()
          .map(|pair| {
            let (by, ended) = pair.split_at(ENDING_BYTES / 2);
            let word = |bytes: &[u8]| <[u8; 4]>::try_from(bytes).map_or(NONE, u32::from_le_bytes);
            (word(by), word(ended))
          })
          .collect(),
      )
    };
    // The first sealed chunk whose last ending is by `rev` or after, each probe reading that ending alone.
    let last_by = |number: u32| -> Result<u32, Refused> {
      let row = index
        .rows
        .get(store, &endings_key(number))
        .ok_or(Refused::Unread { rev })?;
      let (_, last) = row
        .payload()
        .split_last_chunk::<ENDING_BYTES>()
        .ok_or(Refused::Unread { rev })?;
      let (by, _) = last
        .split_first_chunk::<REV_BYTES>()
        .ok_or(Refused::Unread { rev })?;
      Ok(u32::from_le_bytes(*by))
    };
    let (mut low, mut high) = (0_u32, self.sealed);
    while low < high {
      let middle = low.midpoint(high);
      let last = last_by(middle)?;
      if last < rev {
        low = middle.saturating_add(1);
      } else {
        high = middle;
      }
    }
    let mut endings = Vec::new();
    for number in low..self.sealed {
      endings.extend(chunk(number)?.into_iter().filter(|(by, _)| *by >= rev));
    }
    endings.extend(self.open.iter().copied().filter(|(by, _)| *by >= rev));
    Ok(endings)
  }

  /// The full open chunks of `chunk` endings, taken out to be sealed, each with its number; the count of sealed chunks
  /// moved past them.
  fn take_full_chunks(&mut self, chunk: usize) -> Vec<(u32, Vec<u8>)> {
    let mut sealing = Vec::new();
    while self.open.len() >= chunk {
      let rest = self.open.split_off(chunk);
      let full = core::mem::replace(&mut self.open, rest);
      let mut payload = Vec::with_capacity(chunk.saturating_mul(ENDING_BYTES));
      for (by, ended) in full {
        payload.extend_from_slice(&by.to_le_bytes());
        payload.extend_from_slice(&ended.to_le_bytes());
      }
      sealing.push((self.sealed, payload));
      self.sealed = self.sealed.saturating_add(1);
    }
    sealing
  }
}

/// The endings a sealed chunk holds in `store`: [`ENDINGS_CHUNK`], or as many as a window's table holds, one at least.
fn endings_a_chunk(store: &ContentStore) -> usize {
  (store.window().saturating_sub(ENDINGS_ROW_FRAME) / ENDING_BYTES).clamp(1, ENDINGS_CHUNK)
}

/// The key of the sealed chunk of endings numbered `number`.
fn endings_key(number: u32) -> Vec<u8> {
  let mut key = Vec::with_capacity(core::mem::size_of::<u32>().saturating_add(1));
  key.push(ENDINGS_ROW);
  key.extend_from_slice(&number.to_be_bytes());
  key
}

/// A repository's revlog-shaped index, every row in one shared [`Index`].
#[derive(Debug)]
pub struct HgIndex {
  /// The rows.
  rows: Index,
  /// Every branch's heads in its changelog, kept as it grows.
  branch_heads: BranchHeads,
  /// The buffers [`HgIndex::revision_text`] rebuilds a chain through ([`TextBuffers`]).
  buffers: core::cell::RefCell<TextBuffers>,
  /// Seals the store refused after an append (its room): the append stood, its rows charged within the budget, and
  /// the tail is sealed at the next append instead ([`HgIndex::insert`]). Expected none.
  seals_refused: u64,
  /// What a stream's head names, kept as revisions land and are sized ([`KeptRendered`]).
  kept: KeptRendered,
}

/// What a stream's head names, kept as revisions land (goal 93, cut 2; module docs, "a sized row"): the revisions
/// appended and those sized, the files and bytes the sized ones render to, whether a revision kept as it is cannot be
/// streamed, and the number the next log sized takes. A field of the index, copied by its fork, so a generation's
/// totals are its rows'. A stream is served only while every revision appended is sized and none is unstreamable
/// ([`HgIndex::rendered`]): after a replay that did not size them (an import's record names no chunk length), until
/// the recount ([`crate::render::Recount`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct KeptRendered {
  appended: u64,
  sized: u64,
  files: u64,
  bytes: u64,
  unstreamable: bool,
  next_log: u32,
}

/// A log's revisions to size ([`HgIndex::keep_sized`]): each one's chunk length as a stream sends it, from `from` on,
/// and whether a stream can send them as they are kept (a changelog's delta against another revision than the one
/// before it cannot be, [`crate::render`]'s `changelog_servable`).
#[derive(Clone, Copy, Debug)]
pub struct ToSize<'l> {
  /// The first revision's number.
  pub from: u32,
  /// Each revision's chunk length, in order.
  pub lengths: &'l [u32],
  /// Whether a stream can send them as they are kept.
  pub streamable: bool,
}

/// What [`HgIndex::keep_sized`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeptSized {
  /// Sized: the revisions not sized before, kept.
  Sized {
    /// The revisions newly sized.
    new: u64,
  },
  /// Not sized: an earlier revision of the log is not, so these wait for the repository's recount, nothing written.
  Unsized,
}

/// The buffers a revision's text is rebuilt from its chain through (goal 93, C1; w-perf's attribution of Mercurial's
/// own push: `revision_text`'s rebuilds allocated 2.57 GB freed within a second, 1.24 GB of it in the band glibc keeps,
/// and `object_bytes`' copies of the whole each chain begins from 0.62 GB): the text before the delta applied last, and
/// the delta read from the store's windows, each kept for the next read, so a read allocates the text it hands out
/// alone.
#[derive(Debug, Default)]
struct TextBuffers {
  /// The text a delta is applied into.
  next: Vec<u8>,
  /// The delta read.
  delta: Vec<u8>,
}

impl Default for HgIndex {
  fn default() -> Self {
    Self::new()
  }
}

impl HgIndex {
  /// An empty index.
  pub fn new() -> HgIndex {
    HgIndex {
      rows: Index::new(),
      branch_heads: BranchHeads::default(),
      buffers: core::cell::RefCell::new(TextBuffers::default()),
      seals_refused: 0,
      kept: KeptRendered::default(),
    }
  }

  /// The shared index holding the rows: for its seals, compactions, timings and counters.
  pub fn rows(&mut self) -> &mut Index {
    &mut self.rows
  }

  /// Every branch's heads in its changelog, kept as it grows ([`BranchHeads`]).
  pub fn branch_heads(&self) -> &BranchHeads {
    &self.branch_heads
  }

  /// Inserts `rows` as one batch and seals the tail where it is due: the one place every append's rows go in (an
  /// intake's, a replay's, an import's, a made changeset's, the branch heads' endings, objects and markers), so the tail
  /// is at most a window of rows past one batch's, and its sealed tables are content (`slates_vcs::index`, "The tail
  /// and its seal"). Until 2026-11-23 only an import sealed (`import::settle`): a daemon's push of Mercurial's own
  /// history held 56 MB of tail rows at its end, each row two buffers of its own on the system heap
  /// (`crates/server/tests/hg_push_lifetimes.rs`). A seal is a step's work: the full tables of a tail one batch past a
  /// window. A seal the store refuses leaves the batch standing (its rows were charged within the budget when
  /// admitted, so "refused, nothing is written" still holds for every append) and is counted ([`HgIndex::seals_refused`]);
  /// the next append seals instead. A fork's sealed runs are its own, given back by [`HgIndex::release`].
  fn insert(&mut self, store: &mut ContentStore, rows: &[NewRow<'_>]) -> Result<Batch, Refused> {
    let batch = self.rows.insert_all(store, rows)?;
    if self.rows.seal_due(store) && self.rows.seal(store).is_err() {
      self.seals_refused = self.seals_refused.saturating_add(1);
    }
    Ok(batch)
  }

  /// Seals the store refused after an append ([`HgIndex::insert`]): expected none.
  pub fn seals_refused(&self) -> u64 {
    self.seals_refused
  }

  /// The bytes its rows hold in `store` (`slates_vcs::Index::held_bytes`).
  pub fn held_bytes(&self, store: &ContentStore) -> u64 {
    self.rows.held_bytes(store)
  }

  /// A fork: the same rows, held once more, to which it adds only its own (A-581's `Index::fork`).
  pub fn fork(&mut self, store: &mut ContentStore) -> Result<HgIndex, IndexRefused> {
    Ok(HgIndex {
      rows: self.rows.fork(store)?,
      branch_heads: BranchHeads {
        heads: self.branch_heads.heads.clone(),
        endings: self.branch_heads.endings.clone(),
        answered: core::cell::Cell::new(0),
        longest_walk: self.branch_heads.longest_walk,
      },
      buffers: core::cell::RefCell::new(TextBuffers::default()),
      seals_refused: 0,
      kept: self.kept,
    })
  }

  /// Gives back every hold of the index's rows.
  pub fn release(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    self.rows.release(store)
  }

  /// Commits a push's admitted objects as object rows (`O`, the node) in one batch, then gives back the quarantine's
  /// holds, leaving it empty: the rows hold the contents now. Each id must be a node. Refused, nothing is written and
  /// the quarantine keeps every object, for its caller to abandon.
  pub fn commit_objects(
    &mut self,
    store: &mut ContentStore,
    quarantine: &mut Quarantine,
  ) -> Result<Batch, Refused> {
    let mut keys = Vec::with_capacity(quarantine.len());
    let mut bodies = Vec::with_capacity(quarantine.len());
    for (id, kind, content, len) in quarantine.objects() {
      let node = <[u8; NODE_LEN]>::try_from(id).map_err(|_| Refused::NotANode {
        len: crate::read::to_u64(id.len()),
      })?;
      keys.push(object_key(&node));
      bodies.push(([content], object_payload(kind, len)));
    }
    let rows: Vec<NewRow<'_>> = keys
      .iter()
      .zip(&bodies)
      .map(|(key, (contents, payload))| NewRow {
        key,
        contents,
        payload,
      })
      .collect();
    let batch = self.insert(store, &rows)?;
    quarantine
      .abandon(store)
      .map_err(|refused| Refused::Index(IndexRefused::Content(refused)))?;
    Ok(batch)
  }

  /// The object row of `node`: its kind, its length and its root content, if the index holds one.
  pub fn object(&self, store: &ContentStore, node: &Node) -> Option<(u8, u64, ContentId)> {
    object_row(&self.rows.get(store, &object_key(node))?)
  }

  /// Appends obsolescence markers in the order they arrived, each a content holding it alone in its format, numbered
  /// after the markers the index holds; their numbers. One batch: refused, nothing is written.
  pub fn append_markers(
    &mut self,
    store: &mut ContentStore,
    markers: &[ContentId],
  ) -> Result<Vec<u64>, Refused> {
    let first = self.markers_len(store);
    let mut numbers = Vec::with_capacity(markers.len());
    for offset in 0..markers.len() {
      let number = first
        .checked_add(crate::read::to_u64(offset))
        .filter(|number| *number != u64::MAX)
        .ok_or(Refused::LogFull)?;
      numbers.push(number);
    }
    let keys: Vec<Vec<u8>> = numbers.iter().map(|number| marker_key(*number)).collect();
    let contents: Vec<[ContentId; 1]> = markers.iter().map(|marker| [*marker]).collect();
    let rows: Vec<NewRow<'_>> = keys
      .iter()
      .zip(&contents)
      .map(|(key, contents)| NewRow {
        key,
        contents,
        payload: &[],
      })
      .collect();
    self.insert(store, &rows)?;
    Ok(numbers)
  }

  /// How many markers the index holds: the first number with no row.
  pub fn markers_len(&self, store: &ContentStore) -> u64 {
    first_missing(u64::MAX, |number| {
      self.rows.get(store, &marker_key(number)).is_some()
    })
  }

  /// The content of the marker numbered `number`: the marker alone in its format.
  pub fn marker(&self, store: &ContentStore, number: u64) -> Option<ContentId> {
    let row = self.rows.get(store, &marker_key(number))?;
    let mut contents = row.contents();
    let marker = contents.next()?;
    contents.next().is_none().then_some(marker)
  }

  /// The number `node` has in `log`, if the log holds it.
  pub fn rev(&self, store: &ContentStore, log: Log<'_>, node: &Node) -> Option<u32> {
    let row = self.rows.get(store, &log.node_key(node))?;
    let bytes = <[u8; REV_BYTES]>::try_from(row.payload()).ok()?;
    Some(u32::from_le_bytes(bytes))
  }

  /// The changesets whose node begins with `bytes` and then, where given, the high four bits `nibble` (a hexadecimal
  /// prefix of odd length), each with its number, lowest node first, at most `most` of those `counted` keeps: read from
  /// the node rows, which are keyed by node first, so the walk starts where the prefix does and stops where it ends or
  /// once `most` are found (goal 93: `lookup`'s scan of every node, 91 ms of a step on x86, CI's arena-7). The node rows
  /// of the other logs sharing the prefix are passed over.
  pub fn changesets_prefixed(
    &self,
    store: &ContentStore,
    (bytes, nibble): (&[u8], Option<u8>),
    (most, counted): (usize, &dyn Fn(u32) -> bool),
  ) -> Vec<(Node, u32)> {
    let mut start = vec![NODE_ROW];
    start.extend_from_slice(bytes);
    if let Some(nibble) = nibble {
      start.push(nibble << 4);
    }
    let mut found = Vec::new();
    for row in self.rows.iter_from(store, &start) {
      let key = row.key();
      let Some(node) = key
        .get(1..)
        .and_then(|rest| rest.first_chunk::<NODE_LEN>())
        .copied()
      else {
        break;
      };
      let within = key.first() == Some(&NODE_ROW)
        && node.starts_with(bytes)
        && nibble.is_none_or(|nibble| node.get(bytes.len()).map(|byte| byte >> 4) == Some(nibble));
      if !within {
        break;
      }
      if key != Log::Changelog.node_key(&node).as_slice() {
        continue;
      }
      let Some(rev) = <[u8; REV_BYTES]>::try_from(row.payload())
        .ok()
        .map(u32::from_le_bytes)
      else {
        continue;
      };
      if counted(rev) {
        found.push((node, rev));
        if found.len() >= most {
          break;
        }
      }
    }
    found
  }

  /// How many revisions `log` holds: the first number with no row.
  pub fn len(&self, store: &ContentStore, log: Log<'_>) -> u32 {
    // One key, its number rewritten for each probe: a key a probe was the push's commonest allocation (18,455 for an
    // 80-edit history in `push_heap`'s attribution, 2026-11-25).
    let key = core::cell::RefCell::new(log.row_key(0));
    let number_at = key
      .borrow()
      .len()
      .saturating_sub(core::mem::size_of::<u32>());
    let held = |rev: u64| {
      u32::try_from(rev).is_ok_and(|rev| {
        let Ok(mut key) = key.try_borrow_mut() else {
          return false;
        };
        for (to, from) in key.iter_mut().skip(number_at).zip(rev.to_be_bytes()) {
          *to = from;
        }
        self.rows.get(store, &key).is_some()
      })
    };
    u32::try_from(first_missing(u64::from(NONE), held)).unwrap_or(NONE)
  }

  /// Whether `log` holds no revision.
  pub fn is_empty(&self, store: &ContentStore, log: Log<'_>) -> bool {
    self.len(store, log) == 0
  }

  /// The path of every file log that holds a revision, in its path's order: the path its first row names (A-687, an
  /// import's record walks the logs by them). One pass over the rows, since the shared index answers lookups by key
  /// and keeps no list of the logs apart from their rows.
  pub fn file_logs<'s>(&'s self, store: &'s ContentStore) -> impl Iterator<Item = &'s [u8]> + 's {
    self
      .rows
      .iter(store)
      .filter_map(|row| first_file_row(row.key()))
  }

  /// The first file revision at or after revision `rev` of `path`'s log in the file logs' order (by path, then number):
  /// its path, its number and its row; none past the last file log. A fetch's file section walks the logs from one to
  /// the next by it (A-745), so nothing lists them: each call costs one search of the rows and one row.
  pub fn file_revision_from(
    &self,
    store: &ContentStore,
    (path, rev): (&[u8], u32),
  ) -> Option<(Vec<u8>, u32, RevisionRow)> {
    let start = Log::File(path).row_key(rev);
    let row = self.rows.iter_from(store, &start).next()?;
    let rest = row.key().strip_prefix(&[LOG_ROW, FILE_KEY])?;
    let (found, number) = rest.split_last_chunk::<REV_BYTES>()?;
    let found = found.strip_suffix(&[PATH_END])?;
    Some((
      found.to_vec(),
      u32::from_be_bytes(*number),
      read_revision(&row)?,
    ))
  }

  /// The changelog's row numbered `rev`.
  pub fn changeset<'s>(&'s self, store: &'s ContentStore, rev: u32) -> Option<ChangesetRow<'s>> {
    let row = self.rows.get(store, &Log::Changelog.row_key(rev))?;
    read_changeset(&row)
  }

  /// The manifest log's or a file log's row numbered `rev`.
  pub fn revision(&self, store: &ContentStore, log: Log<'_>, rev: u32) -> Option<RevisionRow> {
    if log == Log::Changelog {
      return None;
    }
    let row = self.rows.get(store, &log.row_key(rev))?;
    read_revision(&row)
  }

  /// Keeps the chunk lengths of `log`'s revisions from `sizes.from` on, where they land (module docs, "a sized row"):
  /// each one's sized row, in one batch with the log's number row where it has none yet, and the totals a stream's head
  /// names. A revision sized already with the same span is kept as it is (a recount over revisions a replay sized);
  /// [`KeptSized::Unsized`], nothing written, where an earlier revision of the log is not sized (a push replayed over
  /// an imported history before its recount). Refused, nothing changed: a revision past the log's last, a span that
  /// differs from the one kept, or totals that do not follow.
  ///
  /// # Errors
  /// [`Refused::SizedOutOfOrder`], [`Refused::RenderedOverflow`], or the shared index's refusal.
  pub fn keep_sized(
    &mut self,
    store: &mut ContentStore,
    log: Log<'_>,
    sizes: ToSize<'_>,
  ) -> Result<KeptSized, Refused> {
    let ToSize {
      from,
      lengths,
      streamable,
    } = sizes;
    let end = from
      .checked_add(u32::try_from(lengths.len()).map_err(|_| Refused::LogFull)?)
      .ok_or(Refused::LogFull)?;
    if end > self.len(store, log) {
      return Err(Refused::SizedOutOfOrder { rev: end });
    }
    let (number, numbered) = match self.log_number(store, log) {
      Some(number) => (number, false),
      None => (self.kept.next_log, true),
    };
    let start = match from.checked_sub(1) {
      None => 0,
      Some(last) => match self.span_of(store, number, last) {
        Some(span) => span.offset.saturating_add(u64::from(span.len)),
        None => return Ok(KeptSized::Unsized),
      },
    };
    let rows = sized_rows(number, from, (start, lengths))?;
    let data = rows.last().map_or(start, |(_, span)| {
      span.offset.saturating_add(u64::from(span.len))
    });
    let new = self.sized_new(store, number, from, &rows)?;
    let totals = self.totals_after(log, sized_through(from, start, (&rows, new)), (end, data))?;
    self.insert_sized(store, (log, number, numbered), &rows)?;
    self.kept.files = totals.files;
    self.kept.bytes = totals.bytes;
    self.kept.sized = self.kept.sized.saturating_add(new);
    self.kept.unstreamable |= !streamable;
    if numbered {
      self.kept.next_log = number.checked_add(1).ok_or(Refused::LogFull)?;
    }
    Ok(KeptSized::Sized { new })
  }

  /// How many of `rows` (the sized rows of log `number` from `from`) are not held yet, each held one the same span;
  /// refused [`Refused::SizedOutOfOrder`] at the first that differs, or at one held after one that is not.
  fn sized_new(
    &self,
    store: &ContentStore,
    number: u32,
    from: u32,
    rows: &[(Vec<u8>, ChunkSpan)],
  ) -> Result<u64, Refused> {
    let mut new = 0_u64;
    for (rev, (_, span)) in (from..).zip(rows) {
      match self.span_of(store, number, rev) {
        Some(held) if held == *span && new == 0 => {}
        Some(_) => return Err(Refused::SizedOutOfOrder { rev }),
        None => new = new.saturating_add(1),
      }
    }
    Ok(new)
  }

  /// The totals once `log` grows from `revisions` (with `data` bytes of chunks) to `grown`: its files' share before
  /// taken out and after put in ([`crate::render::rendered_files`]).
  fn totals_after(
    &self,
    log: Log<'_>,
    (revisions, data): (u32, u64),
    grown: (u32, u64),
  ) -> Result<Rendered, Refused> {
    let before = crate::render::rendered_files(log, (revisions, data));
    let after = crate::render::rendered_files(log, grown);
    let files = self
      .kept
      .files
      .checked_sub(before.files)
      .and_then(|files| files.checked_add(after.files));
    let bytes = self
      .kept
      .bytes
      .checked_sub(before.bytes)
      .and_then(|bytes| bytes.checked_add(after.bytes));
    match (files, bytes) {
      (Some(files), Some(bytes)) => Ok(Rendered { files, bytes }),
      _ => Err(Refused::RenderedOverflow),
    }
  }

  /// Inserts the sized `rows` of `log` (numbered `number`), with its number row where `numbered`, as one batch.
  fn insert_sized(
    &mut self,
    store: &mut ContentStore,
    (log, number, numbered): (Log<'_>, u32, bool),
    rows: &[(Vec<u8>, ChunkSpan)],
  ) -> Result<(), Refused> {
    let bodies: Vec<[u8; SIZED_BODY_BYTES]> =
      rows.iter().map(|(_, span)| sized_body(*span)).collect();
    let number_key = numbered.then(|| number_key(log));
    let number_body = number.to_be_bytes();
    let mut batch: Vec<NewRow<'_>> = rows
      .iter()
      .zip(&bodies)
      .map(|((key, _), body)| NewRow {
        key,
        contents: &[],
        payload: body,
      })
      .collect();
    if let Some(key) = &number_key {
      batch.push(NewRow {
        key,
        contents: &[],
        payload: &number_body,
      });
    }
    self.insert(store, &batch)?;
    Ok(())
  }

  /// What sizing `revisions` revisions over `logs` logs charges the store at most ([`SIZED_ROW_CHARGE`] a revision, and
  /// a log's number row, its key's `path_bytes` and its body, a log): the room a landing lends its sizing.
  pub fn sized_charge(revisions: u64, (logs, path_bytes): (u64, u64)) -> u64 {
    revisions
      .saturating_mul(to_u64(SIZED_ROW_CHARGE))
      .saturating_add(logs.saturating_mul(to_u64(NUMBER_ROW_CHARGE)))
      .saturating_add(path_bytes)
  }

  /// `log`'s number ([`NUMBER_ROW`]); none for a log never sized.
  fn log_number(&self, store: &ContentStore, log: Log<'_>) -> Option<u32> {
    let row = self.rows.get(store, &number_key(log))?;
    Some(u32::from_be_bytes(row.payload().try_into().ok()?))
  }

  /// The span the sized row of log `number`'s revision `rev` holds.
  fn span_of(&self, store: &ContentStore, number: u32, rev: u32) -> Option<ChunkSpan> {
    let row = self.rows.get(store, &sized_key(number, rev))?;
    read_span(row.payload())
  }

  /// `log`'s revision `rev`'s kept chunk span ([`HgIndex::keep_sized`]); none where it is not sized.
  pub fn chunk_span(&self, store: &ContentStore, log: Log<'_>, rev: u32) -> Option<ChunkSpan> {
    self.span_of(store, self.log_number(store, log)?, rev)
  }

  /// The bytes of `log`'s chunks, where every revision of it is sized; none where its last is not.
  pub fn log_data(&self, store: &ContentStore, log: Log<'_>) -> Option<u64> {
    let Some(last) = self.len(store, log).checked_sub(1) else {
      return Some(0);
    };
    let span = self.chunk_span(store, log, last)?;
    Some(span.offset.saturating_add(u64::from(span.len)))
  }

  /// `log`'s kept chunk spans from revision `from` to its last sized, in order: one range walk of its sized rows
  /// (`Index::iter_range`), as [`HgIndex::numbers_from`] walks its log rows. None for a log never sized.
  pub fn spans_from<'s>(
    &'s self,
    store: &'s ContentStore,
    log: Log<'_>,
    from: u32,
  ) -> Option<impl Iterator<Item = Option<ChunkSpan>> + use<'s>> {
    let number = self.log_number(store, log)?;
    let start = sized_key(number, from);
    let end = sized_key(number.checked_add(1)?, 0);
    Some(
      self
        .rows
        .iter_range(store, (&start, &end))
        .map(|row| read_span(row.payload())),
    )
  }

  /// The files a stream of the repository sends and their bytes, where every revision appended is sized and each can
  /// be streamed as it is kept; none where one is not, which serves no stream (`1\n`, the client clones).
  pub fn rendered(&self) -> Option<Rendered> {
    (self.kept.appended == self.kept.sized && !self.kept.unstreamable).then_some(Rendered {
      files: self.kept.files,
      bytes: self.kept.bytes,
    })
  }

  /// `log`'s rows from revision `rev` on, in order, each as a stream's index entry reads it ([`EntryNumbers`]; none for
  /// a row that does not decode), to the log's last: one range walk of the index (`Index::iter_range`, which opens only
  /// the tables that can hold the log's keys), then a row a step, where a row looked up by its number searches them each time (goal 93,
  /// hg's whole-history gap: a stream's index rows read 4.5 times a revision so).
  pub fn numbers_from<'s>(
    &'s self,
    store: &'s ContentStore,
    log: Log<'_>,
    rev: u32,
  ) -> impl Iterator<Item = Option<EntryNumbers>> + use<'s> {
    let start = log.row_key(rev);
    let changelog = log == Log::Changelog;
    let prefix_len = start.len().saturating_sub(REV_BYTES);
    let prefix = start
      .get(..prefix_len)
      .map(<[u8]>::to_vec)
      .unwrap_or_default();
    // The range's end: past every key of the log's rows (its prefix and any four bytes).
    let end: Vec<u8> = prefix
      .iter()
      .copied()
      .chain([u8::MAX; REV_BYTES + 1])
      .collect();
    self
      .rows
      .iter_range(store, (&start, &end))
      .take_while(move |row| {
        row.key().len() == prefix.len().saturating_add(REV_BYTES) && row.key().starts_with(&prefix)
      })
      .map(move |row| {
        let rev = u32::from_be_bytes(row.key().get(prefix_len..)?.try_into().ok()?);
        if changelog {
          let found = read_changeset(&row)?;
          return Some(EntryNumbers {
            rev,
            parents: [found.p1, found.p2],
            link: rev,
            node: found.node,
            flags: found.flags,
            len: found.len,
          });
        }
        let found = read_revision(&row)?;
        Some(EntryNumbers {
          rev,
          parents: [found.p1, found.p2],
          link: found.link,
          node: found.node,
          flags: found.flags,
          len: found.len,
        })
      })
  }

  /// Appends a group of changesets in the order it arrived; each one's number, the number it had already for one the
  /// changelog held.
  pub fn append_changesets(
    &mut self,
    store: &mut ContentStore,
    group: &[NewChangeset<'_>],
  ) -> Result<Vec<u32>, Refused> {
    let log = Log::Changelog;
    let mut planner = Planner::new(self, store, log);
    for changeset in group {
      let Some((rev, p1, p2)) = planner.place(changeset.node, changeset.p1, changeset.p2)? else {
        continue;
      };
      let mut payload = common_payload(changeset.node, p1, p2, changeset.flags, changeset.len);
      payload.extend_from_slice(&changeset.manifest);
      payload.push(u8::from(changeset.closed));
      payload.extend_from_slice(changeset.branch);
      planner.planned.push(Planned {
        rev,
        node: changeset.node,
        payload,
        contents: vec![changeset.text],
      });
    }
    let (revs, planned) = (planner.revs, planner.planned);
    self.write(store, log, &planned)?;
    // Each changeset written, oldest first, noted in the branch heads, its parents' rows read back from the index.
    let mut heads = core::mem::take(&mut self.branch_heads);
    let noted = planned.iter().try_for_each(|written| {
      let row = self
        .changeset(store, written.rev)
        .ok_or(Refused::Unread { rev: written.rev })?;
      heads.add((self, store), (written.rev, row.branch, [row.p1, row.p2]))
    });
    let sealing = heads.endings.take_full_chunks(endings_a_chunk(store));
    self.branch_heads = heads;
    noted?;
    // Each full chunk of the endings sealed into a row of its own (`Endings`).
    let keys: Vec<Vec<u8>> = sealing
      .iter()
      .map(|(number, _)| endings_key(*number))
      .collect();
    let rows: Vec<NewRow<'_>> = keys
      .iter()
      .zip(&sealing)
      .map(|(key, (_, payload))| NewRow {
        key,
        contents: &[],
        payload,
      })
      .collect();
    if !rows.is_empty() {
      self.insert(store, &rows)?;
    }
    Ok(revs)
  }

  /// Appends a group of the manifest log's or a file log's revisions in the order it arrived; each one's number, the
  /// number it had already for one the log held. A link must name a changeset the changelog holds.
  pub fn append_revisions(
    &mut self,
    store: &mut ContentStore,
    log: Log<'_>,
    group: &[NewRevision],
  ) -> Result<Vec<u32>, Refused> {
    self.append_revisions_after(store, log, group, (None, &[]))
  }

  /// [`HgIndex::append_revisions`], `previous` the caller's node and whole text of the revision before the group where
  /// it holds them (A-732: a push's intake keeps a revision a step and holds the one it kept last): a manifest's delta
  /// is made against that text, never one rebuilt from its chain. A chain is up to its text's length of deltas, so
  /// rebuilding it applied each in turn over a copy of the whole text: hg's own history spent 130 s of a restart's
  /// replay, its longest step 277 ms, rebuilding each manifest's predecessor. A text the caller does not hold, or
  /// holds for another node, is rebuilt from the log as before.
  ///
  /// `given` are the group's own whole texts the caller holds, by node (a push's intake holds the one it keeps): a
  /// manifest's delta is made from it, never from its text read back from the store it was just put in (goal 93: that
  /// read was a whole copy of every manifest a push kept). A row whose text is not given is read from the store.
  pub fn append_revisions_after<'t>(
    &mut self,
    store: &mut ContentStore,
    log: Log<'_>,
    group: &[NewRevision],
    (previous, given): (Option<(&Node, &'t [u8])>, GivenTexts<'_, 't>),
  ) -> Result<Vec<u32>, Refused> {
    let (revs, mut planned) = self.planned(store, log, group.iter())?;
    let previous = match (log, planned.first()) {
      (Log::Manifest, Some(first)) if first.rev > 0 => {
        let rev = first.rev.saturating_sub(1);
        let row = self
          .revision(store, log, rev)
          .ok_or(Refused::Unread { rev })?;
        let text = match previous {
          Some((node, text)) if *node == row.node => Cow::Borrowed(text),
          _ => {
            #[cfg(test)]
            PREVIOUS_REBUILT.with(|count| count.set(count.get().saturating_add(1)));
            Cow::Owned(
              self
                .revision_text(store, log, rev)
                .ok_or(Refused::Unread { rev })?,
            )
          }
        };
        Some((rev, text, row.chain))
      }
      _ => None,
    };
    let puts = kept_as(store, log, &mut planned, (previous, given))?;
    let written = self.write(store, log, &planned);
    for put in &puts {
      drop(store.release(put));
    }
    written.map(|()| revs)
  }

  /// Appends a group of revisions to `log` as they were stored (A-729 part 2: an import's record names each row's
  /// content by identity, its delta's base and lengths, so its replay appends the rows as the import kept them, nothing
  /// read, rebuilt or put): each one's number, the number it had already for one the log held. Each content must be one
  /// the store holds; the rows hold it again.
  pub fn append_stored(
    &mut self,
    store: &mut ContentStore,
    log: Log<'_>,
    group: &[(NewRevision, Stored)],
  ) -> Result<Vec<u32>, Refused> {
    let (revs, mut planned) =
      self.planned(store, log, group.iter().map(|(revision, _)| revision))?;
    for row in &mut planned {
      let stored = group
        .iter()
        .find(|(revision, _)| revision.node == row.node)
        .map_or(WHOLE, |(_, stored)| *stored);
      row.payload.extend_from_slice(&stored.0.to_le_bytes());
      row.payload.extend_from_slice(&stored.1.to_le_bytes());
      row.payload.extend_from_slice(&stored.2.to_le_bytes());
    }
    self.write(store, log, &planned).map(|()| revs)
  }

  /// A group's rows planned (its numbering, parents and links read, as 7.2's `addgroup` places them), their delta
  /// fields not yet written: every number, and the rows to write.
  fn planned<'g>(
    &self,
    store: &ContentStore,
    log: Log<'_>,
    group: impl Iterator<Item = &'g NewRevision>,
  ) -> Result<(Vec<u32>, Vec<Planned>), Refused> {
    if log == Log::Changelog {
      return Err(Refused::MissingLink { node: NULL_NODE });
    }
    let mut planner = Planner::new(self, store, log);
    for revision in group {
      let Some((rev, p1, p2)) = planner.place(revision.node, revision.p1, revision.p2)? else {
        continue;
      };
      let link = self
        .rev(store, Log::Changelog, &revision.link)
        .ok_or(Refused::MissingLink {
          node: revision.link,
        })?;
      let mut payload = common_payload(revision.node, p1, p2, revision.flags, revision.len);
      payload.extend_from_slice(&link.to_le_bytes());
      payload.push(u8::from(revision.block.is_some()));
      let mut contents = vec![revision.bytes];
      contents.extend(revision.block);
      planner.planned.push(Planned {
        rev,
        node: revision.node,
        payload,
        contents,
      });
    }
    Ok((planner.revs, planner.planned))
  }

  /// The whole text of `log`'s revision `rev` (A-729): a manifest kept as a delta rebuilt from the whole text its chain
  /// rests on, each delta applied in turn; a file revision's metadata block and bytes between its markers; any other
  /// revision's bytes. `None` where the index or the store does not hold it whole. The text handed out is its one
  /// allocation: a whole is read at its length, and a chain is rebuilt through the index's own buffers ([`TextBuffers`]).
  pub fn revision_text(&self, store: &ContentStore, log: Log<'_>, rev: u32) -> Option<Vec<u8>> {
    self.revision_text_over(store, log, rev, &[])
  }

  /// [`HgIndex::revision_text`] where the texts of some revisions of `log` are `given` by node, each read in place of
  /// its row's object (goal 93, C1 (1): a manifest's chain resting on one whose text was let go once landed, read back
  /// from the node's changeset store): the chain is rebuilt from the nearest given text, its deltas above it applied.
  pub fn revision_text_over(
    &self,
    store: &ContentStore,
    log: Log<'_>,
    rev: u32,
    given: &[(Node, Vec<u8>)],
  ) -> Option<Vec<u8>> {
    let given_text = |node: &Node| {
      given
        .iter()
        .find(|(given, _)| given == node)
        .map(|(_, text)| text.as_slice())
    };
    let row = self.revision(store, log, rev)?;
    if let Some(text) = given_text(&row.node) {
      return Some(text.to_vec());
    }
    // Its own object's first window first: a revision let go once landed (held, its windows away) has no text here, so
    // none is begun and no chain gathered (main's churn at e51d998bf: hg's 57k clone allocated 5.51 GB of texts and 0.79
    // GB of chains reading manifests the store had let go, about a manifest a changeset, none of them made). An object
    // the store holds no entry for (the empty one) is read as before.
    if store.contains(&row.bytes) && !matches!(store.windows(&row.bytes).next(), Some(Ok(_))) {
      return None;
    }
    if let Some(block) = row.block {
      return framed_text(store, (&block, &row.bytes), row.len);
    }
    if row.base == NONE {
      return object_text(store, &row.bytes, row.len);
    }
    let (mut chain, over) = self.chain_below(store, log, row, given_text)?;
    // Each delta is applied into the other of two text buffers and read into one reused delta buffer from the store's
    // windows (main's order of 2026-11-20, Ada's no-allocation rule): a chain costs the two longest texts and its
    // longest delta, never a text and a delta a link. A chain is up to its text's length of deltas, so a text a link
    // was its length squared in copies (`delta::apply_into`, w-perf's b568499fe, for the revlog reader's same shape).
    // The buffers are the index's, kept from the read before (a read the index is already making, which none is, as no
    // read calls another, would rebuild through buffers of its own); the text handed out leaves its buffer behind.
    let mut held = self.buffers.try_borrow_mut().ok();
    let mut own = TextBuffers::default();
    let buffers = held.as_deref_mut().unwrap_or(&mut own);
    let mut text = match over {
      Some(text) => text.to_vec(),
      None => {
        let whole = chain.pop()?;
        object_text(store, &whole.bytes, whole.len)?
      }
    };
    let mut next = core::mem::take(&mut buffers.next);
    let mut room = crate::Admission::new(u64::MAX);
    let rebuilt = chain.iter().rev().try_for_each(|step| {
      buffers.delta.clear();
      for window in store.windows(&step.bytes) {
        // A delta window let go: the text is none here, never rebuilt over a short delta.
        buffers.delta.extend_from_slice(&window.ok()?);
      }
      if crate::read::to_u64(buffers.delta.len()) != step.stored {
        return None;
      }
      crate::delta::apply_into(&text, &buffers.delta, &mut next, &mut room).ok()?;
      core::mem::swap(&mut text, &mut next);
      Some(())
    });
    buffers.next = next;
    rebuilt?;
    (crate::read::to_u64(text.len()) == row.len).then_some(text)
  }

  /// The delta chain of `log` below `row`, `row` first: each base's row down to the nearest whole revision, or to the
  /// nearest one whose text is `given` by node, which ends the chain as its text instead ([`HgIndex::revision_text_over`]).
  /// `None` where a base does not come before its revision. Each base is before its revision, so the walk takes at most
  /// as many steps as `row`'s revision number.
  fn chain_below<'g>(
    &self,
    store: &ContentStore,
    log: Log<'_>,
    row: RevisionRow,
    given_text: impl Fn(&Node) -> Option<&'g [u8]>,
  ) -> Option<(Vec<RevisionRow>, Option<&'g [u8]>)> {
    let mut chain = vec![row];
    let mut at = row.base;
    while let Some(link) = self.revision(store, log, at) {
      if let Some(text) = given_text(&link.node) {
        return Some((chain, Some(text)));
      }
      chain.push(link);
      if link.base == NONE {
        break;
      }
      if link.base >= at {
        return None;
      }
      at = link.base;
    }
    Some((chain, None))
  }

  /// Writes a group's planned rows, sorted by key, as one batch.
  fn write(
    &mut self,
    store: &mut ContentStore,
    log: Log<'_>,
    planned: &[Planned],
  ) -> Result<(), Refused> {
    let mut rows: Vec<(Vec<u8>, &[ContentId], Vec<u8>)> =
      Vec::with_capacity(planned.len().saturating_mul(2));
    for revision in planned {
      rows.push((
        log.row_key(revision.rev),
        &revision.contents,
        revision.payload.clone(),
      ));
      rows.push((
        log.node_key(&revision.node),
        &[],
        revision.rev.to_le_bytes().to_vec(),
      ));
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let batch: Vec<NewRow<'_>> = rows
      .iter()
      .map(|(key, contents, payload)| NewRow {
        key,
        contents,
        payload,
      })
      .collect();
    self.insert(store, &batch)?;
    self.kept.appended = self.kept.appended.saturating_add(to_u64(planned.len()));
    Ok(())
  }
}

/// `count` as a `u64`, saturating (a count of rows or bytes in memory never reaches it).
fn to_u64(count: usize) -> u64 {
  u64::try_from(count).unwrap_or(u64::MAX)
}

/// A group's numbering, as 7.2's `addgroup` numbers one.
struct Planner<'i> {
  /// The index as it was before the group.
  index: &'i HgIndex,
  /// The store its rows live in.
  store: &'i ContentStore,
  /// The log the group goes into.
  log: Log<'i>,
  /// The next number.
  next: u32,
  /// The nodes the group adds, with their numbers.
  added: BTreeMap<Node, u32>,
  /// Each revision's number, in the group's order.
  revs: Vec<u32>,
  /// The revisions the group adds.
  planned: Vec<Planned>,
}

impl<'i> Planner<'i> {
  /// A planner for a group going into `log`.
  fn new(index: &'i HgIndex, store: &'i ContentStore, log: Log<'i>) -> Self {
    Self {
      index,
      store,
      log,
      next: index.len(store, log),
      added: BTreeMap::new(),
      revs: Vec::new(),
      planned: Vec::new(),
    }
  }

  /// The number `node` has in the log or the group so far.
  fn known(&self, node: &Node) -> Option<u32> {
    self
      .added
      .get(node)
      .copied()
      .or_else(|| self.index.rev(self.store, self.log, node))
  }

  /// A parent's number: [`NONE`] for the null node, else its number in the log or the group.
  fn parent(&self, node: &Node) -> Result<u32, Refused> {
    if *node == NULL_NODE {
      return Ok(NONE);
    }
    self
      .known(node)
      .ok_or(Refused::MissingParent { node: *node })
  }

  /// Places a revision: `None` when the log or the group holds its node already (its number recorded), else its new
  /// number and its parents' numbers.
  fn place(&mut self, node: Node, p1: Node, p2: Node) -> Result<Option<(u32, u32, u32)>, Refused> {
    if let Some(rev) = self.known(&node) {
      self.revs.push(rev);
      return Ok(None);
    }
    let (p1, p2) = (self.parent(&p1)?, self.parent(&p2)?);
    let rev = self.next;
    if rev == NONE {
      return Err(Refused::LogFull);
    }
    self.next = rev.saturating_add(1);
    self.added.insert(node, rev);
    self.revs.push(rev);
    Ok(Some((rev, p1, p2)))
  }
}

/// The first number with no row among numbers held from 0 without a gap, none at `ceiling` or past it: found by
/// doubling, then halving, in twice the logarithm of the count lookups.
fn first_missing(ceiling: u64, held: impl Fn(u64) -> bool) -> u64 {
  if !held(0) {
    return 0;
  }
  let (mut low, mut high) = (0_u64, 1_u64);
  while high < ceiling && held(high) {
    low = high;
    high = high
      .checked_mul(2)
      .map_or(ceiling, |doubled| doubled.min(ceiling));
  }
  while high.saturating_sub(low) > 1 {
    let middle = low.saturating_add(high.saturating_sub(low) / 2);
    if held(middle) {
      low = middle;
    } else {
      high = middle;
    }
  }
  high
}

/// The path a file log's first row's key names (`L`, `f`, the path, a NUL and revision 0); `None` for any other key.
fn first_file_row(key: &[u8]) -> Option<&[u8]> {
  let rest = key.strip_prefix(&[LOG_ROW, FILE_KEY])?;
  let (path, rev) = rest.split_last_chunk::<REV_BYTES>()?;
  let path = path.strip_suffix(&[PATH_END])?;
  (u32::from_be_bytes(*rev) == 0).then_some(path)
}

/// The key of the marker numbered `number`.
fn marker_key(number: u64) -> Vec<u8> {
  let mut key = Vec::with_capacity(core::mem::size_of::<u64>().saturating_add(1));
  key.push(MARKER_ROW);
  key.extend_from_slice(&number.to_be_bytes());
  key
}

/// The key of `node`'s object row.
fn object_key(node: &Node) -> Vec<u8> {
  let mut key = Vec::with_capacity(NODE_LEN.saturating_add(1));
  key.push(OBJECT_ROW);
  key.extend_from_slice(node);
  key
}

/// The key of `log`'s number row ([`NUMBER_ROW`]).
fn number_key(log: Log<'_>) -> Vec<u8> {
  let mut key = Vec::with_capacity(log.key_len().saturating_add(1));
  key.push(NUMBER_ROW);
  log.put_key(&mut key);
  key
}

/// The key of log `number`'s revision `rev`'s sized row ([`SIZED_ROW`]).
fn sized_key(number: u32, rev: u32) -> Vec<u8> {
  let mut key = Vec::with_capacity(SIZED_KEY_BYTES);
  key.push(SIZED_ROW);
  key.extend_from_slice(&number.to_be_bytes());
  key.extend_from_slice(&rev.to_be_bytes());
  key
}

/// A sized row's body: the chunk's length, then its offset, little-endian.
fn sized_body(span: ChunkSpan) -> [u8; SIZED_BODY_BYTES] {
  let mut body = [0; SIZED_BODY_BYTES];
  let (len, offset) = body.split_at_mut(REV_BYTES);
  len.copy_from_slice(&span.len.to_le_bytes());
  offset.copy_from_slice(&span.offset.to_le_bytes());
  body
}

/// A sized row's span, read back; none for a body not [`SIZED_BODY_BYTES`] long.
fn read_span(body: &[u8]) -> Option<ChunkSpan> {
  let (len, offset) = body.split_at_checked(REV_BYTES)?;
  Some(ChunkSpan {
    len: u32::from_le_bytes(len.try_into().ok()?),
    offset: u64::from_le_bytes(offset.try_into().ok()?),
  })
}

/// The sized rows of log `number` from revision `from`, the first chunk at `start`, each of `lengths` in turn: each
/// row's key and span.
fn sized_rows(
  number: u32,
  from: u32,
  (start, lengths): (u64, &[u32]),
) -> Result<Vec<(Vec<u8>, ChunkSpan)>, Refused> {
  let mut offset = start;
  let mut rows = Vec::with_capacity(lengths.len());
  for (rev, len) in (from..).zip(lengths) {
    rows.push((sized_key(number, rev), ChunkSpan { offset, len: *len }));
    offset = offset
      .checked_add(u64::from(*len))
      .ok_or(Refused::RenderedOverflow)?;
  }
  Ok(rows)
}

/// What a log rendered to before a sizing of `rows` from revision `from` (its first chunk at `start`), of which the last
/// `new` were not held: its revisions and its chunks' bytes through the last one held already, a recount's rows over a
/// replay's (none held: `from` and `start`).
fn sized_through(from: u32, start: u64, (rows, new): (&[(Vec<u8>, ChunkSpan)], u64)) -> (u32, u64) {
  let held = rows
    .len()
    .saturating_sub(usize::try_from(new).unwrap_or(usize::MAX));
  match rows.get(..held).and_then(<[_]>::last) {
    Some((_, span)) => (
      from.saturating_add(u32::try_from(held).unwrap_or(u32::MAX)),
      span.offset.saturating_add(u64::from(span.len)),
    ),
    None => (from, start),
  }
}

/// The length of the chunk a stream sends of a revision placed as `placed` ([`Placed`]): a raw place compressed by the
/// stream's own compressor (`revlog_chunk::chunk_len`, its count pass), a chunk place its length. What a landing sizes
/// a revision by once, so no stream compresses to size (main 01a20182's ruling of 2026-11-27 18:56).
///
/// # Errors
/// [`Refused::Unsizable`] where the compressor refuses or the chunk is longer than an entry names.
pub fn chunk_len_of(placed: Placed<'_>, rev: u32) -> Result<u32, Refused> {
  let len = match placed {
    Placed::Raw(bytes) => crate::revlog_chunk::chunk_len(bytes, crate::revlog_chunk::Pass::Count)
      .map_err(|_| Refused::Unsizable { rev })?,
    Placed::Chunk(len) => len,
  };
  u32::try_from(len).map_err(|_| Refused::Unsizable { rev })
}

/// A log row's payload's common part.
fn common_payload(node: Node, p1: u32, p2: u32, flags: u16, len: u64) -> Vec<u8> {
  let mut payload = Vec::with_capacity(CHANGESET_BYTES);
  payload.extend_from_slice(&node);
  payload.extend_from_slice(&p1.to_le_bytes());
  payload.extend_from_slice(&p2.to_le_bytes());
  payload.extend_from_slice(&flags.to_le_bytes());
  payload.extend_from_slice(&len.to_le_bytes());
  payload
}

/// How a revision's content is stored (A-729): the number of its delta's base ([`NONE`] for a whole text), the delta's
/// length, and the delta bytes of its chain since the whole text it rests on.
pub type Stored = (u32, u64, u64);

/// Format: a whole text's delta fields: no base, no delta, no chain.
const WHOLE: Stored = (NONE, 0, 0);

/// Each planned manifest kept as a delta against the one before it where its chain allows, else whole (A-729): the
/// delta put as content and named in the row's place of the text, its base, length and chain added to the payload;
/// every other row's payload given the whole text's fields. Derived: a delta is kept while its chain's delta bytes stay
/// within the text's own length, so a text is rebuilt from at most its own length of deltas over the whole text it rests
/// on (7.2's `deltas.py` refuses a delta longer than its text by the same bound). The puts' holds, for the caller to
/// give back once the rows hold their contents.
/// The texts a push already holds for a group's rows, by node, as the intake handed them (no copy read back).
type GivenTexts<'g, 't> = &'g [(Node, &'t [u8])];

/// The row a group's first delta applies to: its revision, its text, and its stored length.
type KeptPrevious<'t> = Option<(u32, Cow<'t, [u8]>, u64)>;

fn kept_as<'t>(
  store: &mut ContentStore,
  log: Log<'_>,
  planned: &mut [Planned],
  (mut previous, given): (KeptPrevious<'t>, GivenTexts<'_, 't>),
) -> Result<Vec<ContentId>, Refused> {
  let mut puts = Vec::new();
  for row in planned.iter_mut() {
    let mut fields = WHOLE;
    if log == Log::Manifest {
      let len = row
        .payload
        .get(COMMON_BYTES - LEN_BYTES..COMMON_BYTES)
        .and_then(|len| len.try_into().ok().map(u64::from_le_bytes));
      // The caller's own copy of the text where it gave one for this node, of the length the row names; else read back.
      let held = given
        .iter()
        .find(|(node, text)| *node == row.node && len == Some(crate::read::to_u64(text.len())))
        .map(|(_, text)| Cow::Borrowed(*text));
      let text = match held {
        Some(text) => text,
        None => row
          .contents
          .first()
          .zip(len)
          .and_then(|(id, len)| store.object_bytes(id, len))
          .map(Cow::Owned)
          .ok_or(Refused::Unread { rev: row.rev })?,
      };
      if let Some((rev, prior, chain)) = &previous
        && rev.checked_add(1) == Some(row.rev)
      {
        let mut delta = Vec::new();
        crate::delta::encode_lines(prior.as_ref(), &text, &mut delta)
          .map_err(|_| Refused::Unread { rev: row.rev })?;
        let grown = chain.saturating_add(crate::read::to_u64(delta.len()));
        if grown <= crate::read::to_u64(text.len()) {
          let put = store
            .put_object(&delta)
            .map_err(|refused| Refused::Index(IndexRefused::Content(refused)))?;
          puts.push(put);
          if let Some(content) = row.contents.first_mut() {
            *content = put;
          }
          fields = (*rev, crate::read::to_u64(delta.len()), grown);
        }
      }
      previous = Some((row.rev, text, fields.2));
    }
    row.payload.extend_from_slice(&fields.0.to_le_bytes());
    row.payload.extend_from_slice(&fields.1.to_le_bytes());
    row.payload.extend_from_slice(&fields.2.to_le_bytes());
  }
  Ok(puts)
}

/// A file revision's text: its metadata block and its bytes between the markers, `len` long in all.
fn framed_text(
  store: &ContentStore,
  (block, bytes): (&ContentId, &ContentId),
  len: u64,
) -> Option<Vec<u8>> {
  // A block window let go is a text not here, never a block framed short (the 57k push's hang's shape).
  let mut block_bytes: Vec<u8> = Vec::new();
  for window in store.windows(block) {
    block_bytes.extend_from_slice(&window.ok()?);
  }
  let block = block_bytes;
  let markers = crate::read::to_u64(crate::filerev::MARKER.len()).checked_mul(2)?;
  let framing = crate::read::to_u64(block.len()).checked_add(markers)?;
  let bytes = object_text(store, bytes, len.checked_sub(framing)?)?;
  let mut text = Vec::with_capacity(usize::try_from(len).ok()?);
  text.extend_from_slice(crate::filerev::MARKER);
  text.extend_from_slice(&block);
  text.extend_from_slice(crate::filerev::MARKER);
  text.extend_from_slice(&bytes);
  Some(text)
}

/// The object `root` names, `len` bytes, read from its windows into a text of that length (no copy grown from empty);
/// `None` where the store holds another length, or none of it (nothing allocated then).
fn object_text(store: &ContentStore, root: &ContentId, len: u64) -> Option<Vec<u8>> {
  // The text is made only once its first window is read: an object let go allocates nothing.
  let mut windows = store.windows(root);
  let Some(first) = windows.next() else {
    return (len == 0).then(Vec::new);
  };
  // A window let go, or not held: none here, nothing allocated where it is the first.
  let first = first.ok()?;
  let mut text = Vec::with_capacity(usize::try_from(len).ok()?);
  text.extend_from_slice(&first);
  for window in windows {
    text.extend_from_slice(&window.ok()?);
  }
  (crate::read::to_u64(text.len()) == len).then_some(text)
}

/// A payload's common part, read back: the node, the parents, the flags, the length, and what follows.
type Common<'p> = (Node, u32, u32, u16, u64, &'p [u8]);

/// A payload's common part, read back ([`Common`]).
fn read_common(payload: &[u8]) -> Option<Common<'_>> {
  let (node, rest) = payload.split_first_chunk::<NODE_LEN>()?;
  let (p1, rest) = rest.split_first_chunk::<REV_BYTES>()?;
  let (p2, rest) = rest.split_first_chunk::<REV_BYTES>()?;
  let (flags, rest) = rest.split_first_chunk::<FLAGS_BYTES>()?;
  let (len, rest) = rest.split_first_chunk::<LEN_BYTES>()?;
  Some((
    *node,
    u32::from_le_bytes(*p1),
    u32::from_le_bytes(*p2),
    u16::from_le_bytes(*flags),
    u64::from_le_bytes(*len),
    rest,
  ))
}

/// A changelog row read back, or `None` for a row that is not one.
fn read_changeset<'s>(row: &RowRef<'s>) -> Option<ChangesetRow<'s>> {
  let (node, p1, p2, flags, len, rest) = read_common(row.payload())?;
  let (manifest, rest) = rest.split_first_chunk::<NODE_LEN>()?;
  let (closed, branch) = rest.split_first()?;
  let mut contents = row.contents();
  let text = contents.next()?;
  if contents.next().is_some() || *closed > 1 {
    return None;
  }
  Some(ChangesetRow {
    node,
    p1,
    p2,
    flags,
    len,
    text,
    manifest: *manifest,
    closed: *closed == 1,
    branch,
  })
}

/// A manifest's or file revision's row read back, or `None` for a row that is not one.
fn read_revision(row: &RowRef<'_>) -> Option<RevisionRow> {
  if row.payload().len() != REVISION_BYTES {
    return None;
  }
  let (node, p1, p2, flags, len, rest) = read_common(row.payload())?;
  let (link, rest) = rest.split_first_chunk::<REV_BYTES>()?;
  let (has_block, rest) = rest.split_first()?;
  let (base, rest) = rest.split_first_chunk::<REV_BYTES>()?;
  let (stored, rest) = rest.split_first_chunk::<LEN_BYTES>()?;
  let (chain, _) = rest.split_first_chunk::<LEN_BYTES>()?;
  let mut contents = row.contents();
  let bytes = contents.next()?;
  let block = contents.next();
  if contents.next().is_some() || block.is_some() != (*has_block == 1) || *has_block > 1 {
    return None;
  }
  Some(RevisionRow {
    node,
    p1,
    p2,
    link: u32::from_le_bytes(*link),
    flags,
    len,
    bytes,
    block,
    base: u32::from_le_bytes(*base),
    stored: u64::from_le_bytes(*stored),
    chain: u64::from_le_bytes(*chain),
  })
}
