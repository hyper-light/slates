//! The FSFS view (A-645 route A, A-738 P1): a repository's revisions as the files FSFS keeps on disk, generated from
//! the index and the green, so a stock hook's `svnlook` and `svnadmin` read the repository a hook was given.
//!
//! A Subversion hook is handed `REPOS-PATH` and reads it with `svnlook`, which opens the directory as an FSFS
//! filesystem. slates holds no such directory (§4.17.1: a repository is one green, revision N its version N), so this
//! module derives one: every byte below is a function of the index (`crate::index`, which already holds each
//! node-revision with the identities FSFS gives it) and of the texts and property lists the green holds. Nothing here
//! is stored but the layout a reader needs to name a node-revision: its offset in its revision's file and where its
//! representations lie.
//!
//! The format is FSFS format 8 (Subversion 1.10), sharded by 1000, physical addressing; a text is the `DELTA`
//! representation the svn records keep it as (an svndiff of version 2 against its skip-delta base, S4 c2, read in place,
//! A-645 P3 ruling 3), entries lists and property lists `PLAIN`; every stock release since 1.10 reads it (`fs.h` `SVN_FS_FS__FORMAT_NUMBER` and the formats it opens). The evidence is
//! Subversion's own writer, read at `libsvn_fs_fs` of the `1.14.x` branch (`d434dc14`): `low_level.c`
//! (`svn_fs_fs__write_noderev`, `svn_fs_fs__unparse_representation`, `write_change_entry`,
//! `svn_fs_fs__unparse_revision_trailer`), `id.c` (`svn_fs_fs__id_unparse`) and `structure`. What a revision file
//! holds, in order:
//! - For each node-revision the revision created, children before their directory (byte order of the created path,
//!   descending, so the root is last): its text if the revision gave it a new one, its property list if the revision
//!   gave it a new one, then its node-revision header. A text or a list equal to its predecessor's is the
//!   predecessor's representation, as FSFS keeps one it never touched (a copy's predecessor is its source). A
//!   directory gets a new entries list when a child was created or deleted beneath it in the revision
//!   (`svn_fs_fs__dag_clone_child` and `set_entry` make its representation mutable then, and only then); a new
//!   directory with no entries has none, as `make_entry` leaves it, but revision 0's root, which
//!   `svn_fs_fs__create` writes with an empty one.
//! - The changed paths, sorted by path (`svn_fs_fs__write_changes`), each `<id> <action>-<kind> <text-mod> <prop-mod>
//!   <mergeinfo-mod> <path>` (format 7 and later) and its copy source on the next line, then an empty line.
//! - The trailer: the root's offset and the changed paths' offset.
//!
//! A node or copy identity made in revision R is `<number in base 36>-R`, and the root line's `0`
//! (`svn_fs_fs__id_part_unparse`): the index numbers them within the revision as FSFS does, so only their equality is
//! FSFS's, which is all a reader compares. A file's text names its SHA-1 and a uniquifier (format 4 and later write
//! both or neither, `svn_fs_fs__unparse_representation`); a list's and a directory's name the MD5 alone.
//!
//! Derived and lazy (A-645 ruling P1): the layout is advanced a revision at a time under the caller's budget
//! ([`View::advance`]), since a revision's offsets name the node-revisions of every earlier one; a revision's file is
//! generated only when read ([`View::revision_file`]), its texts streamed from the green, never copied. A revision is
//! the slice: laying one out costs what the index's own commit of it cost, one row a node-revision.
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::dump::Digests;
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopyId, Index, IndexRefusal, Node, NodeId, NodeKind,
  NodeRevisionId, PropertyList, Revision, Trial,
};
use crate::reads::Repository as Source;

/// The repository's format file.
/// Format: `SVN_REPOS__FORMAT_NUMBER` 5 (`repos.h`), a line.
const REPOSITORY_FORMAT: &[u8] = b"5\n";

/// The filesystem's format file: format 8, sharded, physical addressing.
/// Format: `fs_fs.c` `write_format` for format 8 with `max_files_per_dir` set and physical addressing.
const FILESYSTEM_FORMAT: &[u8] = b"8\nlayout sharded 1000\naddressing physical\n";

/// The revisions one shard directory holds.
/// Format: the `layout sharded 1000` line above (`SVN_FS_FS_DEFAULT_MAX_FILES_PER_DIR`).
const SHARD_REVISIONS: u64 = 1000;

/// The filesystem type file.
/// Format: `SVN_FS_TYPE_FSFS` and a line feed (`fs-loader.c` `svn_fs__path_valid`'s neighbour `write_fs_type`).
const FILESYSTEM_TYPE: &[u8] = b"fsfs\n";

/// The unpacked revisions begin at 0: nothing is packed.
/// Format: `min-unpacked-rev`, a decimal line (`structure`, "Packing revisions").
const MIN_UNPACKED: &[u8] = b"0\n";

/// The next transaction's number, base 36: the view holds no transaction (P2 adds them).
/// Format: `txn-current`, a base-36 line (`structure`, "Transaction IDs").
const TRANSACTION_COUNTER: &[u8] = b"0\n";

/// What begins a representation stored as a delta against nothing (format 4 and later): the delta's own bytes follow.
/// Format: `REP_DELTA` and a line feed (`low_level.c` `svn_fs_fs__write_rep_header`).
const DELTA_SELF: &[u8] = b"DELTA\n";

/// What begins a representation stored whole.
/// Format: `REP_PLAIN` and a line feed (`low_level.c` `svn_fs_fs__write_rep_header`).
const PLAIN: &[u8] = b"PLAIN\n";

/// What ends every representation.
/// Format: `ENDREP` and a line feed (`transaction.c`, `structure` "Representations").
const END_REPRESENTATION: &[u8] = b"ENDREP\n";

/// The entries list of an empty directory: a hash dump with nothing in it.
/// Format: `SVN_HASH_TERMINATOR` and a line feed (`hash.c`).
const EMPTY_LIST: &[u8] = b"END\n";

/// The base of FSFS's identity numbers.
/// Format: `svn__ui64tobase36`.
const BASE36: u64 = 36;

/// The digits of base 36.
/// Format: `svn__ui64tobase36`, lowercase.
const BASE36_DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Why the view could not be laid out or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewRefusal {
  /// The index refused a question.
  Index(IndexRefusal),
  /// A revision past those laid out so far: [`View::advance`] first.
  NotLaidOut {
    /// The revision asked for.
    revision: Revision,
  },
  /// A node-revision the layout never placed, or a path the index names but cannot resolve: the index contradicts
  /// itself.
  Unplaced,
  /// The green holds no such text.
  MissingText,
  /// The green holds no such property list.
  MissingProperties,
  /// The green holds no properties for this revision.
  MissingRevisionProperties {
    /// The revision.
    revision: Revision,
  },
  /// A digest could not be made.
  Digest,
  /// A length or an offset past what the platform counts.
  Overflow,
}

impl From<IndexRefusal> for ViewRefusal {
  fn from(refusal: IndexRefusal) -> Self {
    Self::Index(refusal)
  }
}

/// Where a representation lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Representation {
  /// The revision whose file holds it.
  revision: Revision,
  /// The offset of its `PLAIN` line.
  offset: u64,
  /// Its bytes, header and `ENDREP` apart: a `PLAIN` representation's text, a `DELTA` one's svndiff.
  size: u64,
  /// The bytes it stands for: a text's length, a `PLAIN` representation's own size.
  expanded: u64,
  /// The MD5 of its bytes.
  md5: [u8; 16],
  /// A text's SHA-1 and its uniquifier's number within its revision.
  shared: Option<([u8; 20], u64)>,
}

/// Where one node-revision and its representations lie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
  /// The offset of its header in its revision's file.
  offset: u64,
  /// Its text or entries list.
  data: Option<Representation>,
  /// Its property list.
  properties: Option<Representation>,
}

/// One run of a revision file.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
  /// Bytes the view made.
  Bytes(Vec<u8>),
  /// A text the green holds, whole.
  Text {
    /// Its content identity.
    identity: ContentIdentity,
    /// Its length.
    size: u64,
  },
  /// A text's delta as the records keep it ([`TextDeltas`]), read in place.
  Delta {
    /// The text's content identity.
    identity: ContentIdentity,
    /// The delta's length.
    size: u64,
  },
}

/// A text's delta as the svn records keep it (S4 c2): an svndiff of version 2 against the text of its node's skip-delta
/// predecessor, or against nothing for a first version -- FSFS 1.10's own representation, so the view names it as a
/// `DELTA` representation and reads its bytes where they are kept, never making the text (A-645 P3, main's ruling 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDelta<'a> {
  /// The text it is against; none for a delta against nothing.
  pub base: Option<ContentIdentity>,
  /// Its svndiff bytes.
  pub bytes: std::borrow::Cow<'a, [u8]>,
}

/// Where the view reads a text's stored delta.
pub trait TextDeltas {
  /// The delta the text `identity` is kept as; none for a text kept whole (one whose bytes a property list holds too),
  /// which the view writes as a `PLAIN` representation read through [`Source::text`].
  fn text_delta(&self, identity: &ContentIdentity) -> Option<StoredDelta<'_>>;
}

impl TextDeltas for crate::migration::Repository {
  fn text_delta(&self, identity: &ContentIdentity) -> Option<StoredDelta<'_>> {
    let (base, bytes) = crate::migration::Repository::text_delta(self, identity)?;
    Some(StoredDelta {
      base,
      bytes: std::borrow::Cow::Borrowed(bytes),
    })
  }
}

/// A revision file being made.
#[derive(Default)]
struct Writer {
  /// Its runs, each with its offset.
  segments: Vec<(u64, Segment)>,
  /// Its length so far.
  length: u64,
}

impl Writer {
  /// Appends bytes the view made.
  fn bytes(&mut self, bytes: &[u8]) -> Result<(), ViewRefusal> {
    let added = u64::try_from(bytes.len()).map_err(|_| ViewRefusal::Overflow)?;
    match self.segments.last_mut() {
      Some((_, Segment::Bytes(held))) => held.extend_from_slice(bytes),
      _ => self
        .segments
        .push((self.length, Segment::Bytes(bytes.to_vec()))),
    }
    self.length = self
      .length
      .checked_add(added)
      .ok_or(ViewRefusal::Overflow)?;
    Ok(())
  }

  /// Appends a text the green holds.
  fn text(&mut self, identity: ContentIdentity, size: u64) -> Result<(), ViewRefusal> {
    self
      .segments
      .push((self.length, Segment::Text { identity, size }));
    self.length = self.length.checked_add(size).ok_or(ViewRefusal::Overflow)?;
    Ok(())
  }

  /// Appends a text's stored delta, read in place.
  fn delta(&mut self, identity: ContentIdentity, size: u64) -> Result<(), ViewRefusal> {
    self
      .segments
      .push((self.length, Segment::Delta { identity, size }));
    self.length = self.length.checked_add(size).ok_or(ViewRefusal::Overflow)?;
    Ok(())
  }

  /// Appends a representation the view made whole, `PLAIN` and `ENDREP` around it, and answers where it lies.
  fn representation(
    &mut self,
    revision: Revision,
    bytes: &[u8],
  ) -> Result<Representation, ViewRefusal> {
    let offset = self.length;
    self.bytes(PLAIN)?;
    self.bytes(bytes)?;
    self.bytes(END_REPRESENTATION)?;
    let size = u64::try_from(bytes.len()).map_err(|_| ViewRefusal::Overflow)?;
    Ok(Representation {
      revision,
      offset,
      size,
      expanded: size,
      md5: md5_of(bytes)?,
      shared: None,
    })
  }
}

/// A revision's file as generated: read a run at a time ([`RevisionFile::read`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionFile {
  /// Its runs, each with its offset.
  segments: Vec<(u64, Segment)>,
  /// Its length.
  length: u64,
}

impl RevisionFile {
  /// Its length in bytes.
  pub fn len(&self) -> u64 {
    self.length
  }

  /// Whether it holds no bytes; no revision file is empty.
  pub fn is_empty(&self) -> bool {
    self.length == 0
  }

  /// Appends to `out` at most `most` of its bytes from `offset`, one run's worth (a text as the green holds it in one
  /// piece), and answers how many: none at its end.
  ///
  /// # Errors
  /// [`ViewRefusal::MissingText`] when the green no longer holds a text the file names.
  pub fn read<S: Source + TextDeltas + ?Sized>(
    &self,
    source: &S,
    offset: u64,
    most: usize,
    out: &mut Vec<u8>,
  ) -> Result<usize, ViewRefusal> {
    let after = self.segments.partition_point(|(start, _)| *start <= offset);
    let Some((start, segment)) = after.checked_sub(1).and_then(|at| self.segments.get(at)) else {
      return Ok(0);
    };
    let inner = offset.checked_sub(*start).ok_or(ViewRefusal::Overflow)?;
    let run: std::borrow::Cow<'_, [u8]> = match segment {
      Segment::Bytes(bytes) => {
        let inner = usize::try_from(inner).map_err(|_| ViewRefusal::Overflow)?;
        std::borrow::Cow::Borrowed(bytes.get(inner..).unwrap_or_default())
      }
      Segment::Text { identity, size } => {
        if inner >= *size {
          std::borrow::Cow::Borrowed(&[][..])
        } else {
          source
            .text(identity, inner)
            .ok_or(ViewRefusal::MissingText)?
        }
      }
      Segment::Delta { identity, size } => {
        let delta = source
          .text_delta(identity)
          .ok_or(ViewRefusal::MissingText)?;
        let inner = usize::try_from(inner.min(*size)).map_err(|_| ViewRefusal::Overflow)?;
        match delta.bytes {
          std::borrow::Cow::Borrowed(bytes) => {
            std::borrow::Cow::Borrowed(bytes.get(inner..).unwrap_or_default())
          }
          std::borrow::Cow::Owned(bytes) => {
            std::borrow::Cow::Owned(bytes.get(inner..).unwrap_or_default().to_vec())
          }
        }
      }
    };
    let taken = run.len().min(most);
    out.extend_from_slice(run.get(..taken).unwrap_or_default());
    Ok(taken)
  }
}

/// What [`View::advance`] left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advance {
  /// Every revision of the index is laid out.
  Done,
  /// The budget was spent first; call again.
  More,
}

/// The layout of a repository's FSFS view: where each node-revision lies, laid out a revision at a time (module
/// documentation).
#[derive(Clone, Debug, Default)]
pub struct View {
  /// Every node-revision laid out so far.
  placed: BTreeMap<NodeRevisionId, Placed>,
  /// Where each text's first representation lies: a later node of the same text names it, and a delta names its
  /// base's.
  texts: BTreeMap<ContentIdentity, Representation>,
  /// Each laid-out revision's file length.
  lengths: Vec<u64>,
}

impl View {
  /// A view with nothing laid out.
  pub fn new() -> Self {
    Self::default()
  }

  /// The revisions laid out so far: revision 0 through one before this.
  pub fn laid_out(&self) -> u64 {
    u64::try_from(self.lengths.len()).unwrap_or(u64::MAX)
  }

  /// Lays out the next revisions of `index` until every one is or `budget` node-revisions were placed, at least one
  /// revision a call.
  ///
  /// # Errors
  /// [`ViewRefusal`] when the index or the green contradicts itself.
  pub fn advance<S: Source + TextDeltas + ?Sized>(
    &mut self,
    index: &Index,
    source: &S,
    budget: usize,
  ) -> Result<Advance, ViewRefusal> {
    let mut spent = 0_usize;
    loop {
      let next = self.laid_out();
      if next > index.youngest() {
        return Ok(Advance::Done);
      }
      if spent > 0 && spent >= budget {
        return Ok(Advance::More);
      }
      let (writer, placed, texts) = build((&self.placed, &self.texts), index, source, next)?;
      spent = spent.saturating_add(placed.len().max(1));
      self.lengths.push(writer.length);
      self.placed.extend(placed);
      self.texts.extend(texts);
    }
  }

  /// The length of revision `revision`'s file, once laid out.
  pub fn revision_length(&self, revision: Revision) -> Option<u64> {
    usize::try_from(revision)
      .ok()
      .and_then(|at| self.lengths.get(at))
      .copied()
  }

  /// Revision `revision`'s file, generated from its layout: its node-revisions, its new representations and its
  /// changed paths, its texts named for reading from the green.
  ///
  /// # Errors
  /// [`ViewRefusal::NotLaidOut`] for a revision [`View::advance`] has not reached; otherwise as `advance`.
  pub fn revision_file<S: Source + TextDeltas + ?Sized>(
    &self,
    index: &Index,
    source: &S,
    revision: Revision,
  ) -> Result<RevisionFile, ViewRefusal> {
    if revision >= self.laid_out() {
      return Err(ViewRefusal::NotLaidOut { revision });
    }
    let (writer, _, _) = build((&self.placed, &self.texts), index, source, revision)?;
    Ok(RevisionFile {
      segments: writer.segments,
      length: writer.length,
    })
  }
}

/// Revision `revision`'s properties file: the hash dump FSFS keeps them in (`revprops.c` `write_non_packed_revprop`).
///
/// # Errors
/// [`ViewRefusal::MissingRevisionProperties`] when the green holds none for it.
pub fn revision_properties_file<S: Source + TextDeltas + ?Sized>(
  source: &S,
  revision: Revision,
) -> Result<Vec<u8>, ViewRefusal> {
  let pairs = source
    .revision_properties(revision)
    .ok_or(ViewRefusal::MissingRevisionProperties { revision })?;
  Ok(hash_dump(pairs))
}

/// Where revision `revision`'s file lies beneath the repository: `db/revs/<shard>/<revision>`.
pub fn revision_file_path(revision: Revision) -> String {
  format!(
    "db/revs/{}/{revision}",
    revision.checked_div(SHARD_REVISIONS).unwrap_or(0)
  )
}

/// Where revision `revision`'s properties lie beneath the repository: `db/revprops/<shard>/<revision>`.
pub fn revision_properties_path(revision: Revision) -> String {
  format!(
    "db/revprops/{}/{revision}",
    revision.checked_div(SHARD_REVISIONS).unwrap_or(0)
  )
}

/// The directories of a repository whose youngest revision is `youngest`, parents first.
pub fn directories(youngest: Revision) -> Vec<String> {
  let mut made: Vec<String> = [
    "conf",
    "hooks",
    "locks",
    "db",
    "db/revs",
    "db/revprops",
    "db/transactions",
    "db/txn-protorevs",
  ]
  .iter()
  .map(|name| (*name).to_owned())
  .collect();
  let shards = youngest.checked_div(SHARD_REVISIONS).unwrap_or(0);
  for shard in 0..=shards {
    made.push(format!("db/revs/{shard}"));
    made.push(format!("db/revprops/{shard}"));
  }
  made
}

/// The files beside the revisions of a repository with UUID `uuid` and youngest revision `youngest`: each path
/// beneath the repository and its bytes.
pub fn fixed_files(uuid: &[u8], youngest: Revision) -> Vec<(&'static str, Vec<u8>)> {
  // Format 7 and later keep an instance id on the line after the UUID (`fs_fs.c` `svn_fs_fs__set_uuid`); a view names
  // its repository's own UUID there, the one instance it presents.
  let mut uuid_line = uuid.to_vec();
  uuid_line.push(b'\n');
  uuid_line.extend_from_slice(uuid);
  uuid_line.push(b'\n');
  vec![
    ("format", REPOSITORY_FORMAT.to_vec()),
    ("locks/db.lock", Vec::new()),
    ("locks/db-logs.lock", Vec::new()),
    ("db/format", FILESYSTEM_FORMAT.to_vec()),
    ("db/fs-type", FILESYSTEM_TYPE.to_vec()),
    ("db/uuid", uuid_line),
    ("db/current", format!("{youngest}\n").into_bytes()),
    ("db/min-unpacked-rev", MIN_UNPACKED.to_vec()),
    ("db/txn-current", TRANSACTION_COUNTER.to_vec()),
    ("db/txn-current-lock", Vec::new()),
    ("db/write-lock", Vec::new()),
  ]
}

/// Makes revision `revision`'s file against the node-revisions `placed` holds from earlier revisions: the writer and
/// where each node-revision the revision created lies.
fn build<S: Source + TextDeltas + ?Sized>(
  (placed, texts): (
    &BTreeMap<NodeRevisionId, Placed>,
    &BTreeMap<ContentIdentity, Representation>,
  ),
  index: &Index,
  source: &S,
  revision: Revision,
) -> Result<Built, ViewRefusal> {
  let mut nodes = index.created_in(revision)?;
  nodes.sort_by(|first, second| second.created_path().cmp(first.created_path()));
  let mut layout = Layout {
    placed,
    local: BTreeMap::new(),
    texts,
    local_texts: BTreeMap::new(),
    index,
    revision,
    writer: Writer::default(),
    uniquifier: 0,
  };
  let mut root = None;
  for node in &nodes {
    let offset = layout.node(source, node)?;
    if node.created_path() == "/" {
      root = Some(offset);
    }
  }
  let root = root.ok_or(ViewRefusal::Unplaced)?;
  let changes_offset = layout.writer.length;
  layout.changes()?;
  let trailer = format!("{root} {changes_offset}\n");
  layout.writer.bytes(trailer.as_bytes())?;
  Ok((layout.writer, layout.local, layout.local_texts))
}

/// A revision's file as [`build`] made it: its writer, where each node-revision it created lies, and where each text it
/// wrote first lies.
type Built = (
  Writer,
  BTreeMap<NodeRevisionId, Placed>,
  BTreeMap<ContentIdentity, Representation>,
);

/// One revision's layout being made.
struct Layout<'a> {
  /// What earlier revisions placed.
  placed: &'a BTreeMap<NodeRevisionId, Placed>,
  /// What this revision placed so far.
  local: BTreeMap<NodeRevisionId, Placed>,
  /// Where earlier revisions wrote each text first.
  texts: &'a BTreeMap<ContentIdentity, Representation>,
  /// The texts this revision wrote first.
  local_texts: BTreeMap<ContentIdentity, Representation>,
  /// The index.
  index: &'a Index,
  /// The revision.
  revision: Revision,
  /// Its file.
  writer: Writer,
  /// The uniquifiers its texts took so far.
  uniquifier: u64,
}

impl Layout<'_> {
  /// Where node-revision `id` lies.
  fn placed(&self, id: NodeRevisionId) -> Result<Placed, ViewRefusal> {
    self
      .local
      .get(&id)
      .or_else(|| self.placed.get(&id))
      .copied()
      .ok_or(ViewRefusal::Unplaced)
  }

  /// Node-revision `id`'s ID as FSFS writes it.
  fn id_of(&self, id: NodeRevisionId) -> Result<String, ViewRefusal> {
    let node = self.index.node_revision(id)?;
    let offset = self.placed(id)?.offset;
    Ok(id_text(
      node.node_id(),
      node.copy_id(),
      node.created_revision(),
      offset,
    ))
  }

  /// Writes `node`'s representations and header, and answers the header's offset.
  fn node<S: Source + TextDeltas + ?Sized>(
    &mut self,
    source: &S,
    node: &Node<'_>,
  ) -> Result<u64, ViewRefusal> {
    let predecessor = match node.predecessor() {
      Some(id) => Some((self.index.node_revision(id)?, self.placed(id)?)),
      None => None,
    };
    let data = match node.kind() {
      NodeKind::File => self.file_text(source, node, predecessor.as_ref())?,
      NodeKind::Directory => self.entries(node, predecessor.as_ref())?,
    };
    let properties = match node.properties() {
      PropertyList::Empty => None,
      PropertyList::Held { identity, .. } => match &predecessor {
        Some((before, at)) if before.properties() == node.properties() => at.properties,
        _ => {
          let pairs = source
            .properties(&identity)
            .ok_or(ViewRefusal::MissingProperties)?;
          Some(
            self
              .writer
              .representation(self.revision, &hash_dump(pairs))?,
          )
        }
      },
    };
    let offset = self.writer.length;
    self.local.insert(
      node.id(),
      Placed {
        offset,
        data,
        properties,
      },
    );
    let header = self.header(node, data, properties)?;
    self.writer.bytes(header.as_bytes())?;
    Ok(offset)
  }

  /// Where a text was written first, by this revision or an earlier one.
  fn text_placed(&self, identity: &ContentIdentity) -> Option<Representation> {
    // The view's own map counts only what earlier revisions wrote: regenerating this revision finds its own texts there
    // too, and must write them again where it first did.
    self.local_texts.get(identity).copied().or_else(|| {
      self
        .texts
        .get(identity)
        .filter(|placed| placed.revision < self.revision)
        .copied()
    })
  }

  /// A file's text: its predecessor's when equal, the representation a text already has when it has one, otherwise
  /// its stored delta written here as a `DELTA` representation against its base's (format 8, main's ruling 3), the
  /// delta's bytes read where the records keep them; a text kept whole (no delta) a `PLAIN` one read through the source.
  fn file_text<S: Source + TextDeltas + ?Sized>(
    &mut self,
    source: &S,
    node: &Node<'_>,
    predecessor: Option<&(Node<'_>, Placed)>,
  ) -> Result<Option<Representation>, ViewRefusal> {
    let Some(text) = node.text() else {
      return Ok(None);
    };
    if let Some((before, at)) = predecessor
      && before.text().map(|held| held.identity) == Some(text.identity)
    {
      return Ok(at.data);
    }
    if let Some(placed) = self.text_placed(&text.identity) {
      return Ok(Some(placed));
    }
    let offset = self.writer.length;
    let size = match source.text_delta(&text.identity) {
      Some(delta) => {
        let size = u64::try_from(delta.bytes.len()).map_err(|_| ViewRefusal::Overflow)?;
        match delta.base {
          Some(base) => {
            let base = self.text_placed(&base).ok_or(ViewRefusal::Unplaced)?;
            let header = format!("DELTA {} {} {}\n", base.revision, base.offset, base.size);
            self.writer.bytes(header.as_bytes())?;
          }
          None => self.writer.bytes(DELTA_SELF)?,
        }
        self.writer.delta(text.identity, size)?;
        size
      }
      None => {
        self.writer.bytes(PLAIN)?;
        self.writer.text(text.identity, text.size)?;
        text.size
      }
    };
    self.writer.bytes(END_REPRESENTATION)?;
    let number = self.uniquifier;
    self.uniquifier = number.checked_add(1).ok_or(ViewRefusal::Overflow)?;
    let placed = Representation {
      revision: self.revision,
      offset,
      size,
      expanded: text.size,
      md5: text.md5,
      shared: Some((text.sha1, number)),
    };
    self.local_texts.insert(text.identity, placed);
    Ok(Some(placed))
  }

  /// A directory's entries list: its predecessor's when no child was created or deleted beneath it in this revision,
  /// otherwise written here.
  fn entries(
    &mut self,
    node: &Node<'_>,
    predecessor: Option<&(Node<'_>, Placed)>,
  ) -> Result<Option<Representation>, ViewRefusal> {
    let path = node.created_path();
    let names = self
      .index
      .children(path, self.revision)?
      .unwrap_or_default();
    let mut children = Vec::with_capacity(names.len());
    let mut changed = self
      .index
      .changes(self.revision)?
      .iter()
      .any(|change| change.action == Action::Delete && parent_of(&change.path) == path);
    for name in names {
      let child = self
        .index
        .node(&joined(path, &name), self.revision)?
        .ok_or(ViewRefusal::Unplaced)?;
      changed |= child.created_revision() == self.revision;
      children.push((name, child.kind(), child.id()));
    }
    if !changed {
      return Ok(match predecessor {
        Some((_, at)) => at.data,
        None if self.revision == 0 => Some(self.writer.representation(0, EMPTY_LIST)?),
        None => None,
      });
    }
    let mut pairs = Vec::with_capacity(children.len());
    for (name, kind, id) in children {
      let value = format!("{} {}", kind_text(kind), self.id_of(id)?);
      pairs.push((name.into_bytes(), value.into_bytes()));
    }
    let bytes = hash_dump(
      pairs
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    );
    Ok(Some(self.writer.representation(self.revision, &bytes)?))
  }

  /// `node`'s header (`svn_fs_fs__write_noderev`), its empty line included.
  fn header(
    &self,
    node: &Node<'_>,
    data: Option<Representation>,
    properties: Option<Representation>,
  ) -> Result<String, ViewRefusal> {
    let mut header = String::new();
    let mut line = |text: std::fmt::Arguments<'_>| -> Result<(), ViewRefusal> {
      header.write_fmt(text).map_err(|_| ViewRefusal::Overflow)?;
      header.push('\n');
      Ok(())
    };
    line(format_args!("id: {}", self.id_of(node.id())?))?;
    line(format_args!("type: {}", kind_text(node.kind())))?;
    if let Some(predecessor) = node.predecessor() {
      line(format_args!("pred: {}", self.id_of(predecessor)?))?;
    }
    line(format_args!("count: {}", node.predecessor_count()))?;
    if let Some(data) = data {
      line(format_args!("text: {}", representation_text(&data)))?;
    }
    if let Some(properties) = properties {
      line(format_args!("props: {}", representation_text(&properties)))?;
    }
    line(format_args!("cpath: {}", node.created_path()))?;
    if let Some((path, revision)) = node.copy_from() {
      line(format_args!("copyfrom: {revision} {path}"))?;
    }
    let root = self.index.node_revision(node.copy_root())?;
    if root.created_revision() != node.created_revision()
      || root.created_path() != node.created_path()
    {
      line(format_args!(
        "copyroot: {} {}",
        root.created_revision(),
        root.created_path()
      ))?;
    }
    if node.mergeinfo_count() > 0 {
      line(format_args!("minfo-cnt: {}", node.mergeinfo_count()))?;
    }
    if node.has_mergeinfo() {
      line(format_args!("minfo-here: y"))?;
    }
    header.push('\n');
    Ok(header)
  }

  /// The changed paths and the empty line ending them (`svn_fs_fs__write_changes`, format 8).
  fn changes(&mut self) -> Result<(), ViewRefusal> {
    let mut rows: Vec<_> = self.index.changes(self.revision)?.iter().collect();
    rows.sort_by(|first, second| first.path.as_bytes().cmp(second.path.as_bytes()));
    let mut section = String::new();
    for row in rows {
      let at = if row.action == Action::Delete {
        self.revision.checked_sub(1).ok_or(ViewRefusal::Unplaced)?
      } else {
        self.revision
      };
      let node = self
        .index
        .node(&row.path, at)?
        .ok_or(ViewRefusal::Unplaced)?;
      change_line(&mut section, &self.id_of(node.id())?, row)?;
    }
    section.push('\n');
    self.writer.bytes(section.as_bytes())
  }
}

/// A transaction's directory and prototype revision file as FSFS keeps them while the transaction is open (A-738
/// part 2): what a pre-commit hook's `svnlook -t` reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionFiles {
  /// The directories to make beneath the repository, parents first.
  pub directories: Vec<String>,
  /// Each file beneath the repository and its bytes: the transaction's properties, its next identities, its changed
  /// paths, each node-revision it made with its property list and its entries where they changed, the prototype's
  /// lock.
  pub files: Vec<(String, Vec<u8>)>,
  /// Where the prototype revision file lies beneath the repository.
  pub prototype_path: String,
  /// The prototype revision file: the texts the transaction gave, read from the source in runs.
  pub prototype: RevisionFile,
}

/// The directory a transaction named `name` keeps (`structure`, "Transaction layout"), beneath the repository.
pub fn transaction_path(name: &str) -> String {
  format!("db/transactions/{name}.txn")
}

/// The transaction `trial` stands for, named `name` and holding the properties `properties`, made by the changed-path
/// rows `work` on `trial`'s base: its directory as FSFS format 8 keeps it (`structure`, "Transaction layout"). Each
/// node-revision it made is a `node.<node>.<copy>` file with the transaction's identities (`_<number>` for one it
/// made, `t<name>` its place); a text it gave lies in the prototype file and is named `-1 <offset> …`; a property list
/// it changed is `-1` and a `.props` file; a directory whose entries changed is `-1` and a `.children` file holding
/// them whole; anything it kept is the committed representation the view placed. The changed paths are the
/// revision's form, unterminated, as the transaction appends them. `view` must be laid out through the base.
///
/// # Errors
/// [`ViewRefusal::NotLaidOut`] for a base the view has not reached; otherwise as [`View::advance`].
pub fn transaction_files<S: Source + TextDeltas + ?Sized>(
  view: &View,
  index: &Index,
  trial: &Trial<'_>,
  work: &[ChangedPath],
  name: &str,
  properties: Vec<(&[u8], &[u8])>,
  source: &S,
) -> Result<TransactionFiles, ViewRefusal> {
  if trial.base() >= view.laid_out() {
    return Err(ViewRefusal::NotLaidOut {
      revision: trial.base(),
    });
  }
  let made = trial.made()?;
  let mut transaction = Transaction {
    view,
    index,
    trial,
    name,
    writer: Writer::default(),
    files: Vec::new(),
    uniquifier: 0,
  };
  let directory = transaction_path(name);
  let mut next_node = 0_u64;
  let mut next_copy = 0_u64;
  for node in &made {
    for (part, next) in [
      (node.node_id(), &mut next_node),
      (
        NodeId {
          revision: node.copy_id().revision,
          number: node.copy_id().number,
        },
        &mut next_copy,
      ),
    ] {
      if part.revision == trial.revision() {
        *next = (*next).max(part.number.checked_add(1).ok_or(ViewRefusal::Overflow)?);
      }
    }
    transaction.node(source, node, work, &directory)?;
  }
  let mut changes = String::new();
  let mut rows: Vec<&ChangedPath> = work.iter().collect();
  rows.sort_by(|first, second| first.path.as_bytes().cmp(second.path.as_bytes()));
  for row in rows {
    let id = if row.action == Action::Delete {
      index
        .node(&row.path, trial.base())?
        .ok_or(ViewRefusal::Unplaced)?
        .id()
    } else {
      trial.node(&row.path)?.ok_or(ViewRefusal::Unplaced)?.id
    };
    change_line(&mut changes, &transaction.id_of(id)?, row)?;
  }
  let mut files = vec![
    (format!("{directory}/props"), hash_dump(properties)),
    (
      format!("{directory}/next-ids"),
      format!("{} {}\n", base36(next_node), base36(next_copy)).into_bytes(),
    ),
    (format!("{directory}/changes"), changes.into_bytes()),
    (format!("db/txn-protorevs/{name}.rev-lock"), Vec::new()),
  ];
  files.append(&mut transaction.files);
  Ok(TransactionFiles {
    directories: vec![directory],
    files,
    prototype_path: format!("db/txn-protorevs/{name}.rev"),
    prototype: RevisionFile {
      segments: transaction.writer.segments,
      length: transaction.writer.length,
    },
  })
}

/// A transaction's directory being made.
struct Transaction<'a, 'trial> {
  /// The committed revisions' layout.
  view: &'a View,
  /// The index.
  index: &'a Index,
  /// The transaction.
  trial: &'a Trial<'trial>,
  /// Its name.
  name: &'a str,
  /// Its prototype revision file.
  writer: Writer,
  /// Its node-revision files so far.
  files: Vec<(String, Vec<u8>)>,
  /// The uniquifiers its texts took so far.
  uniquifier: u64,
}

impl Transaction<'_, '_> {
  /// Whether `node` is one the transaction made.
  fn made(&self, node: &Node<'_>) -> bool {
    node.created_revision() == self.trial.revision()
  }

  /// The node and copy parts of a node-revision's ID: `_<number>` for an identity the transaction made.
  fn parts(&self, node: &Node<'_>) -> (String, String) {
    let part = |number: u64, revision: Revision| {
      if revision == self.trial.revision() {
        format!("_{}", base36(number))
      } else {
        id_part(number, revision)
      }
    };
    (
      part(node.node_id().number, node.node_id().revision),
      part(node.copy_id().number, node.copy_id().revision),
    )
  }

  /// Node-revision `id`'s ID: the transaction's form for one it made, the revision's otherwise.
  fn id_of(&self, id: NodeRevisionId) -> Result<String, ViewRefusal> {
    let node = self.trial.node_revision(id)?;
    if self.made(&node) {
      let (node_part, copy_part) = self.parts(&node);
      return Ok(format!("{node_part}.{copy_part}.t{}", self.name));
    }
    let placed = self.view.placed.get(&id).ok_or(ViewRefusal::Unplaced)?;
    Ok(id_text(
      node.node_id(),
      node.copy_id(),
      node.created_revision(),
      placed.offset,
    ))
  }

  /// Writes `node`'s file, and its property and entries files where it changed them.
  fn node<S: Source + TextDeltas + ?Sized>(
    &mut self,
    source: &S,
    node: &Node<'_>,
    work: &[ChangedPath],
    directory: &str,
  ) -> Result<(), ViewRefusal> {
    let (node_part, copy_part) = self.parts(node);
    let file = format!("{directory}/node.{node_part}.{copy_part}");
    let predecessor = match node.predecessor() {
      Some(id) => Some((
        self.trial.node_revision(id)?,
        self.view.placed.get(&id).copied(),
      )),
      None => None,
    };
    let text = match node.kind() {
      NodeKind::File => self.file_text(node, predecessor.as_ref())?,
      NodeKind::Directory => self.entries(node, predecessor.as_ref(), work, &file)?,
    };
    let properties = match node.properties() {
      PropertyList::Empty => None,
      PropertyList::Held { identity, .. } => match &predecessor {
        Some((before, Some(at))) if before.properties() == node.properties() => {
          at.properties.as_ref().map(representation_text)
        }
        _ => {
          let pairs = source
            .properties(&identity)
            .ok_or(ViewRefusal::MissingProperties)?;
          self.files.push((format!("{file}.props"), hash_dump(pairs)));
          Some("-1".to_owned())
        }
      },
    };
    let header = self.header(node, text, properties, work.is_empty())?;
    self.files.push((file, header.into_bytes()));
    Ok(())
  }

  /// A file's text line: its predecessor's representation when equal, otherwise the text written to the prototype.
  fn file_text(
    &mut self,
    node: &Node<'_>,
    predecessor: Option<&(Node<'_>, Option<Placed>)>,
  ) -> Result<Option<String>, ViewRefusal> {
    let Some(text) = node.text() else {
      return Ok(None);
    };
    if let Some((before, at)) = predecessor
      && before.text().map(|held| held.identity) == Some(text.identity)
    {
      let at = at.ok_or(ViewRefusal::Unplaced)?;
      return Ok(at.data.as_ref().map(representation_text));
    }
    if let Some(placed) = self.view.texts.get(&text.identity) {
      return Ok(Some(representation_text(placed)));
    }
    let offset = self.writer.length;
    self.writer.bytes(PLAIN)?;
    self.writer.text(text.identity, text.size)?;
    self.writer.bytes(END_REPRESENTATION)?;
    let number = self.uniquifier;
    self.uniquifier = number.checked_add(1).ok_or(ViewRefusal::Overflow)?;
    Ok(Some(format!(
      "-1 {offset} {} {} {} {} {}/_{}",
      text.size,
      text.size,
      hex(&text.md5),
      hex(&text.sha1),
      self.name,
      base36(number)
    )))
  }

  /// A directory's text line: its predecessor's entries when no child was made or deleted beneath it, otherwise `-1`
  /// and its entries whole in its `.children` file.
  fn entries(
    &mut self,
    node: &Node<'_>,
    predecessor: Option<&(Node<'_>, Option<Placed>)>,
    work: &[ChangedPath],
    file: &str,
  ) -> Result<Option<String>, ViewRefusal> {
    let path = node.created_path();
    let mut names: Vec<String> = match predecessor {
      Some((before, _)) => self
        .index
        .children(before.created_path(), before.created_revision())?
        .unwrap_or_default(),
      None => Vec::new(),
    };
    let mut changed = false;
    for row in work
      .iter()
      .filter(|row| parent_of(&row.path) == path && row.path != path)
    {
      let Some((_, name)) = row.path.rsplit_once('/') else {
        continue;
      };
      if row.action != Action::Modify {
        changed = true;
      }
      if row.action != Action::Delete && !names.iter().any(|held| held == name) {
        names.push(name.to_owned());
      }
    }
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
      let Some(child) = self.trial.node(&joined(path, &name))? else {
        continue;
      };
      let child_node = self.trial.node_revision(child.id)?;
      changed |= self.made(&child_node);
      entries.push((name, child.kind, child.id));
    }
    if !changed {
      return match predecessor {
        Some((_, at)) => Ok(
          at.ok_or(ViewRefusal::Unplaced)?
            .data
            .as_ref()
            .map(representation_text),
        ),
        None => Ok(None),
      };
    }
    let mut pairs = Vec::with_capacity(entries.len());
    for (name, kind, id) in entries {
      let value = format!("{} {}", kind_text(kind), self.id_of(id)?);
      pairs.push((name.into_bytes(), value.into_bytes()));
    }
    let bytes = hash_dump(
      pairs
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    );
    self.files.push((format!("{file}.children"), bytes));
    Ok(Some("-1".to_owned()))
  }

  /// `node`'s file (`svn_fs_fs__write_noderev` as a transaction writes it): a copy root the transaction made is
  /// revision `-1`, and named only where it is not the node itself.
  fn header(
    &self,
    node: &Node<'_>,
    text: Option<String>,
    properties: Option<String>,
    fresh: bool,
  ) -> Result<String, ViewRefusal> {
    let mut header = String::new();
    let mut line = |text: std::fmt::Arguments<'_>| -> Result<(), ViewRefusal> {
      header.write_fmt(text).map_err(|_| ViewRefusal::Overflow)?;
      header.push('\n');
      Ok(())
    };
    line(format_args!("id: {}", self.id_of(node.id())?))?;
    line(format_args!("type: {}", kind_text(node.kind())))?;
    if let Some(predecessor) = node.predecessor() {
      line(format_args!("pred: {}", self.id_of(predecessor)?))?;
    }
    line(format_args!("count: {}", node.predecessor_count()))?;
    if let Some(text) = text {
      line(format_args!("text: {text}"))?;
    }
    if let Some(properties) = properties {
      line(format_args!("props: {properties}"))?;
    }
    line(format_args!("cpath: {}", node.created_path()))?;
    if let Some((path, revision)) = node.copy_from() {
      line(format_args!("copyfrom: {revision} {path}"))?;
    }
    let root = self.trial.node_revision(node.copy_root())?;
    if self.made(&root) {
      if root.created_path() != node.created_path() {
        line(format_args!("copyroot: -1 {}", root.created_path()))?;
      }
    } else {
      line(format_args!(
        "copyroot: {} {}",
        root.created_revision(),
        root.created_path()
      ))?;
    }
    if fresh && node.created_path() == "/" {
      line(format_args!("is-fresh-txn-root: y"))?;
    }
    if node.mergeinfo_count() > 0 {
      line(format_args!("minfo-cnt: {}", node.mergeinfo_count()))?;
    }
    if node.has_mergeinfo() {
      line(format_args!("minfo-here: y"))?;
    }
    header.push('\n');
    Ok(header)
  }
}

/// Appends changed-path row `row`'s entry, its node-revision `id` (`write_change_entry`, format 7 and later: the
/// mergeinfo flag after the property flag).
fn change_line(section: &mut String, id: &str, row: &ChangedPath) -> Result<(), ViewRefusal> {
  let action = match row.action {
    Action::Add => "add",
    Action::Delete => "delete",
    Action::Replace => "replace",
    Action::Modify => "modify",
  };
  writeln!(
    section,
    "{id} {action}-{} {} {} {} {}",
    kind_text(row.kind),
    row.text.is_some(),
    row.properties.is_some(),
    row.mergeinfo_modified,
    row.path
  )
  .map_err(|_| ViewRefusal::Overflow)?;
  if let Some(source) = &row.copy_from {
    write!(section, "{} {}", source.revision, source.path).map_err(|_| ViewRefusal::Overflow)?;
  }
  section.push('\n');
  Ok(())
}

/// A node-revision ID: `<node>.<copy>.r<revision>/<offset>` (`id.c` `svn_fs_fs__id_unparse`).
fn id_text(node: NodeId, copy: CopyId, revision: Revision, offset: u64) -> String {
  format!(
    "{}.{}.r{revision}/{offset}",
    id_part(node.number, node.revision),
    id_part(copy.number, copy.revision)
  )
}

/// One part of an ID: its number in base 36, and `-<revision>` when made after revision 0
/// (`svn_fs_fs__id_part_unparse`).
fn id_part(number: u64, revision: Revision) -> String {
  if revision == 0 {
    base36(number)
  } else {
    format!("{}-{revision}", base36(number))
  }
}

/// A representation's line (`svn_fs_fs__unparse_representation`, format 8): revision, offset, size, expanded size and
/// MD5, and a text's SHA-1 and uniquifier `<transaction>/_<number>`, its transaction named after the revision before.
fn representation_text(representation: &Representation) -> String {
  let mut text = format!(
    "{} {} {} {} {}",
    representation.revision,
    representation.offset,
    representation.size,
    representation.expanded,
    hex(&representation.md5)
  );
  if let Some((sha1, number)) = representation.shared {
    let base = representation.revision.saturating_sub(1);
    let _ = write!(
      text,
      " {} {base}-{}/_{}",
      hex(&sha1),
      base36(base),
      base36(number)
    );
  }
  text
}

/// A node kind as FSFS writes it.
/// Format: `SVN_FS_FS__KIND_FILE`, `SVN_FS_FS__KIND_DIR`.
fn kind_text(kind: NodeKind) -> &'static str {
  match kind {
    NodeKind::File => "file",
    NodeKind::Directory => "dir",
  }
}

/// `pairs` as a hash dump, sorted by name (`svn_hash_write2`).
fn hash_dump(mut pairs: Vec<(&[u8], &[u8])>) -> Vec<u8> {
  pairs.sort();
  let mut bytes = Vec::new();
  for (name, value) in pairs {
    bytes.extend_from_slice(format!("K {}\n", name.len()).as_bytes());
    bytes.extend_from_slice(name);
    bytes.push(b'\n');
    bytes.extend_from_slice(format!("V {}\n", value.len()).as_bytes());
    bytes.extend_from_slice(value);
    bytes.push(b'\n');
  }
  bytes.extend_from_slice(EMPTY_LIST);
  bytes
}

/// The MD5 of `bytes`.
fn md5_of(bytes: &[u8]) -> Result<[u8; 16], ViewRefusal> {
  let mut digests = Digests::new().map_err(|_| ViewRefusal::Digest)?;
  digests.update(bytes).map_err(|_| ViewRefusal::Digest)?;
  Ok(digests.finish().map_err(|_| ViewRefusal::Digest)?.0)
}

/// `bytes` in lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
  bytes.iter().fold(String::new(), |mut text, byte| {
    let _ = write!(text, "{byte:02x}");
    text
  })
}

/// `number` in base 36 (`svn__ui64tobase36`).
fn base36(number: u64) -> String {
  let mut digits = Vec::new();
  let mut rest = number;
  loop {
    let digit = rest
      .checked_rem(BASE36)
      .and_then(|place| usize::try_from(place).ok())
      .and_then(|place| BASE36_DIGITS.get(place))
      .copied()
      .unwrap_or(b'0');
    digits.push(char::from(digit));
    rest = rest.checked_div(BASE36).unwrap_or(0);
    if rest == 0 {
      break;
    }
  }
  digits.iter().rev().collect()
}

/// The directory holding canonical path `path`; the root's is itself.
fn parent_of(path: &str) -> &str {
  match path.rsplit_once('/') {
    Some(("", _)) | None => "/",
    Some((parent, _)) => parent,
  }
}

/// `name` beneath directory `path`.
fn joined(path: &str, name: &str) -> String {
  if path == "/" {
    format!("/{name}")
  } else {
    format!("{path}/{name}")
  }
}
