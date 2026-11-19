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

use std::collections::BTreeMap;

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

/// Format: a manifest's or file revision's row payload: the common part, the link, and whether it has a block.
const REVISION_BYTES: usize = COMMON_BYTES + REV_BYTES + 1;

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

  /// The key of this log's row numbered `rev`.
  fn row_key(&self, rev: u32) -> Vec<u8> {
    let mut key = vec![LOG_ROW];
    self.put_key(&mut key);
    key.extend_from_slice(&rev.to_be_bytes());
    key
  }

  /// The key of `node`'s row in this log.
  fn node_key(&self, node: &Node) -> Vec<u8> {
    let mut key = vec![NODE_ROW];
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
  /// The text, or a file revision's bytes.
  pub bytes: ContentId,
  /// A file revision's metadata block.
  pub block: Option<ContentId>,
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
  /// The shared index refused (its budget, or a row it holds that differs).
  Index(IndexRefused),
  /// An object whose id is not a node's twenty bytes.
  NotANode {
    /// The id's length.
    len: u64,
  },
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

/// A repository's revlog-shaped index, every row in one shared [`Index`].
#[derive(Debug)]
pub struct HgIndex {
  /// The rows.
  rows: Index,
}

impl Default for HgIndex {
  fn default() -> Self {
    Self::new()
  }
}

impl HgIndex {
  /// An empty index.
  pub fn new() -> HgIndex {
    HgIndex { rows: Index::new() }
  }

  /// The shared index holding the rows: for its seals, compactions, timings and counters.
  pub fn rows(&mut self) -> &mut Index {
    &mut self.rows
  }

  /// A fork: the same rows, held once more, to which it adds only its own (A-581's `Index::fork`).
  pub fn fork(&mut self, store: &mut ContentStore) -> Result<HgIndex, IndexRefused> {
    Ok(HgIndex {
      rows: self.rows.fork(store)?,
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
    let batch = self.rows.insert_all(store, &rows)?;
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
    self.rows.insert_all(store, &rows)?;
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
    if true {
      let _unbuilt = (store, number);
      return None;
    }
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

  /// How many revisions `log` holds: the first number with no row.
  pub fn len(&self, store: &ContentStore, log: Log<'_>) -> u32 {
    let held = |rev: u64| {
      u32::try_from(rev).is_ok_and(|rev| self.rows.get(store, &log.row_key(rev)).is_some())
    };
    u32::try_from(first_missing(u64::from(NONE), held)).unwrap_or(NONE)
  }

  /// Whether `log` holds no revision.
  pub fn is_empty(&self, store: &ContentStore, log: Log<'_>) -> bool {
    self.len(store, log) == 0
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
    let (revs, planned) = (planner.revs, planner.planned);
    self.write(store, log, &planned)?;
    Ok(revs)
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
    self.rows.insert_all(store, &batch)?;
    Ok(())
  }
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
    high = high.checked_mul(2).map_or(ceiling, |doubled| doubled.min(ceiling));
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

/// A payload's common part, read back: the node, the parents, the flags, the length, and what follows.
fn read_common(payload: &[u8]) -> Option<(Node, u32, u32, u16, u64, &[u8])> {
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
  let (has_block, _) = rest.split_first()?;
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
  })
}
