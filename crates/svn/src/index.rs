//! The Subversion index (§4.17.4 "2. The Subversion index" and piece 4; A-553): every node-revision a stock client can
//! ask about, with the identities FSFS gives it, computed from each revision's changed-path record alone.
//!
//! A repository is one green whose version N is revision N (§4.17.1). What a client reads about a node beyond its bytes
//! is FSFS's bookkeeping, not the tree's: its created revision, which the out-of-date test, `stat` and
//! `svn:entry:committed-rev` read; whether two nodes are related, which the reporter and `svn_fs_node_relation` read;
//! its predecessor, copy source and copy root; and the count of nodes with `svn:mergeinfo` at and beneath a directory.
//! This module computes all of it as FSFS would have, from the revisions' changed-path rows (§4.17.4 item 3.5) and the
//! facts of the nodes they name. So a replica or a holder recomputes the index from what it already holds and never
//! receives it, and a holder whose recomputation differs is fatal and loud (§4.16).
//!
//! What it keeps:
//! - **Node rows**, one per node-revision (`structure` 551-588): its kind; its node identity, where its line of history
//!   was born, and its copy identity; its created path and revision; its predecessor and the count of predecessors; its
//!   copy source; its copy root; its text's content identity, MD5, SHA-1 and size; its property list's identity and
//!   whether it holds `svn:mergeinfo` (FSFS's `minfo-here`); and the count of nodes holding `svn:mergeinfo` at and
//!   beneath it (`minfo-cnt`).
//! - **History lists**, per path: the revisions at which a node-revision was created at the path, deletions included.
//! - **Shape lists**, per path: the revisions at which it was added, copied to, replaced or deleted, which decide what
//!   stands beneath it.
//! - **Revision rows**: each revision's changed-path rows sorted by path, and its transaction's name.
//!
//! How each row is made: by FSFS's own rules, read in its code (Subversion's `1.14.x` branch at `d434dc14`,
//! `libsvn_fs_fs`; `docs/wip/research/vcs-servers-subversion.md` §5):
//! - Every revision begins with a new node-revision of the root whose predecessor is the previous root, even a revision
//!   that changes nothing (`transaction.c` `create_new_txn_noderev_from_rev`, run when a transaction is made).
//! - A row is applied as FSFS applies the editor call it records. The path's directories are opened from the root, each
//!   one's copy-identity inheritance decided against its parent as that parent stands when opened (`tree.c`
//!   `open_path`, `get_copy_inheritance`); then each is made mutable from the root down, a new node-revision of the old
//!   one with the old as its predecessor (`make_path_mutable`, `dag.c` `svn_fs_fs__dag_clone_child`). That is FSFS's
//!   bubble-up: a directory's created revision is the last revision that changed anything beneath it. An addition is a
//!   new node with its parent's copy identity and copy root (`make_entry`); a copy keeps its source's node identity,
//!   takes a fresh copy identity and is its own copy root (`svn_fs_fs__dag_copy`); a deletion removes the path and
//!   everything beneath it; a modification sets the text or the property list of the node made mutable. A replacement
//!   is the deletion and then the addition, so a replacement by a copy of the very node-revision it replaces is a copy
//!   like any other: `copy_helper` (`tree.c`) records nothing only for a copy onto a node still standing at its
//!   destination, which no record holds (A-652).
//! - A revision's rows are applied in path order, parents first, against revision N - 1. That is the state FSFS's
//!   commit leaves (`tree.c` `merge()`): a directory changed both by the transaction and by the revisions since its base
//!   must keep its node and copy identity on both sides, or the commit is a conflict ("not a direct modification of
//!   ANCESTOR-ENTRY"), and its predecessor is then pointed at the newer revision's (`svn_fs_fs__dag_update_ancestry`).
//!   So for every commit FSFS accepts, the identities made against N - 1 are the ones FSFS committed.
//! - FSFS makes nodes mutable in its drive's order, and a stock client sends file texts last, in an order of its own,
//!   so an inheritance FSFS decides against a parent may meet that parent before or after the same transaction made it
//!   mutable. The oracle replays every generated revision in random drive orders as well as in path order, copies into
//!   their own subtrees among them, and every identity, copy root and predecessor comes out the same (module tests).
//! - A new node or copy identity is the revision and a number counted from zero within it, as FSFS's format 3 and later
//!   write `<number>-<revision>` (`structure`, "Node-revision IDs"). FSFS numbers in its drive's order and the index in
//!   path order; only equality between identities is ever read.
//!
//! Reading: the node at a path in a revision is the path's own last history entry at or before that revision, unless
//! one of its directories was added, copied to, replaced or deleted after that entry; then the latest such change
//! decides (a deeper directory's, when two share a revision, since path order applied it last). A deletion, or an
//! addition without history, leaves nothing at the path; a copy gives the node at the same place beneath the copy's
//! source, in the source's revision. So a file beneath a branch is its trunk node-revision, created on trunk, until it
//! changes on the branch, as FSFS shares it. The node relation follows (`svn_fs.h`'s three answers): the same
//! node-revision is unchanged, the same node identity a common ancestor, anything else unrelated. A directory's
//! listing (`Index::children`, `Index::listing`; A-759) is its node-revision's children map: every directory row
//! holds a persistent ordered map of its children's names and node-revisions (`children`), made as the row is made (a
//! clone keeps its predecessor's map and its parent's map points the name at it; an addition sets the name in its
//! parent's; a deletion removes it; a copy shares its source's map whole), its nodes shared between successors and
//! charged to the room. So a listing resolves the directory once and walks its live children; the oracle holds every
//! directory of every generated revision to the naive tree's, and each listed child to the node its path resolves to.
//!
//! Every record is checked whole before anything changes, and a refused record leaves the index as it was: a path out
//! of Subversion's canonical form; two rows for one path; a row naming a path its own revision leaves absent or present
//! against its action; a directory of a row's path that is a file; the root added, deleted or replaced; a kind that
//! differs from the node or copy source a row names; a copy whose source is past the previous revision or absent
//! there; a text on a directory; a row whose fields contradict each other, `svn:mergeinfo` gained or lost without the mergeinfo flag among
//! them; and an index that would outgrow the room its caller gives. Each refusal is typed and names the Subversion error
//! it is answered with ([`IndexRefusal::subversion_code`]). A record can be checked first ([`Index::plan`]) and
//! committed later ([`Index::commit_planned`]), as the owner's commit step checks a revision before its log records it
//! and commits it after (A-655); a plan committed after another revision was appended is refused, typed, with nothing
//! changed.
//!
//! The room is the caller's: the most entries (node rows and deletions) the index may hold, which the owner derives
//! from its shard's room (§4.17.4 item 12); every other table grows at most linearly in them. Holding the tables as
//! content with a mutable tail is §4.17.1's object store (shared piece 3), and slicing one large revision under a shard's
//! budget is the server's; this module is the rows and their rules.
//!
//! No I/O, no clock, no randomness, no panic.

use std::borrow::Cow;
use std::collections::BTreeMap;

/// A directory's listing page: its entries (each name with its node-revision), and whether more follow.
pub type Listing = (Vec<(String, NodeRevisionId)>, bool);

/// A revision number. Revision 0 is the empty root every repository starts with (`svn_fs.h` 1021-1104; notes §5).
pub type Revision = u64;

/// A content identity: the BLAKE3 digest the green names content by (§4.11).
pub type ContentIdentity = [u8; 32];

/// A property list as pairs: each property's name and value, in the list's order.
pub type PropertyPairs = Vec<(Vec<u8>, Vec<u8>)>;

/// The root directory's path, the one path with no parent.
/// Format: Subversion's canonical absolute path of the root (`svn_fspath__is_canonical`).
const ROOT: &str = "/";

/// The character between a path's names.
/// Format: Subversion's path separator (`svn_fspath__join`).
const SEPARATOR: char = '/';

/// The name that means the directory itself, which no stored path holds.
/// Format: `svn_fs__path_valid` refuses "'.' or '..' element\[s\]" (`libsvn_fs/fs-loader.c`).
const CURRENT_DIRECTORY: &str = ".";

/// The name that means the parent directory, which no stored path holds.
/// Format: `svn_fs__path_valid` (`libsvn_fs/fs-loader.c`).
const PARENT_DIRECTORY: &str = "..";

/// The characters no stored name holds: a line feed, which `svn_fs__path_valid` refuses because "svn:mergeinfo and
/// friends can't handle them", and a NUL, which ends a C string.
/// Format: `libsvn_fs/fs-loader.c` `svn_fs__path_valid`.
const FORBIDDEN_IN_NAMES: [char; 2] = ['\n', '\0'];

/// The bytes besides ASCII letters and digits a transaction's name may hold.
/// Format: "Transaction names are guaranteed to contain only letters (upper- and lower-case), digits, `-', and `.'"
/// (`svn_fs.h`; research notes §5).
const TRANSACTION_NAME_PUNCTUATION: [u8; 2] = *b"-.";

/// The node identity of the root directory, the same in every revision.
/// Format: FSFS's root node id `0` (`structure`, "Node-revision IDs"; `svn_fs_fs__id_part_is_root`).
const ROOT_NODE: NodeId = NodeId {
  revision: 0,
  number: 0,
};

/// The copy identity of everything never copied: the root's, which what is made beneath it inherits.
/// Format: FSFS's root copy id `0` (`structure`, "Copy-IDs and copy roots"; `svn_fs_fs__id_part_is_root`).
const ROOT_COPY: CopyId = CopyId {
  revision: 0,
  number: 0,
};

/// Revision 0's root, the index's first row: its own copy root, and every later root's.
/// Format: "if no ancestor directories derive from a copy operation, then the copyroot field identifies the root
/// directory of rev 0" (`structure`, "Copy-IDs and copy roots"); "For the transaction root, the copyroot never changes"
/// (`transaction.c` `create_new_txn_noderev_from_rev`).
const FIRST_ROOT: NodeRevisionId = NodeRevisionId(0);

/// What one node holding `svn:mergeinfo` adds to the count of every directory above it and its own.
/// Format: `minfo-cnt`, "The number of nodes under (and including) this node which have svn:mergeinfo" (`structure`).
const ONE_NODE: u64 = 1;

/// `SVN_ERR_FS_GENERAL`.
/// Format: the filesystem category's start, 120,000 + 8 × 5,000 = 160,000, plus 0 (`svn_error_codes.h` 121-122,
/// 581-582).
const SVN_ERR_FS_GENERAL: u32 = 160_000;

/// `SVN_ERR_FS_CORRUPT`.
/// Format: the filesystem category's start plus 4 (`svn_error_codes.h` 597-598).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;

/// `SVN_ERR_FS_PATH_SYNTAX`.
/// Format: the filesystem category's start plus 5 (`svn_error_codes.h` 601-602).
const SVN_ERR_FS_PATH_SYNTAX: u32 = 160_005;

/// `SVN_ERR_FS_NO_SUCH_REVISION`.
/// Format: the filesystem category's start plus 6 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;

/// `SVN_ERR_FS_NOT_FOUND`.
/// Format: the filesystem category's start plus 13 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_FOUND: u32 = 160_013;

/// `SVN_ERR_FS_ID_NOT_FOUND`.
/// Format: the filesystem category's start plus 14 (`svn_error_codes.h`).
const SVN_ERR_FS_ID_NOT_FOUND: u32 = 160_014;

/// `SVN_ERR_FS_NOT_DIRECTORY`.
/// Format: the filesystem category's start plus 16 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_DIRECTORY: u32 = 160_016;

/// `SVN_ERR_FS_NOT_FILE`.
/// Format: the filesystem category's start plus 17 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_FILE: u32 = 160_017;

/// `SVN_ERR_FS_ALREADY_EXISTS`.
/// Format: the filesystem category's start plus 20 (`svn_error_codes.h`).
const SVN_ERR_FS_ALREADY_EXISTS: u32 = 160_020;

/// `SVN_ERR_FS_ROOT_DIR`, "Attempt to remove or recreate fs root dir".
/// Format: the filesystem category's start plus 21 (`svn_error_codes.h`).
const SVN_ERR_FS_ROOT_DIR: u32 = 160_021;

/// `SVN_ERR_FS_TXN_OUT_OF_DATE`, which FSFS's commit answers "Transaction out of date" when its transaction's base is
/// no longer the youngest revision (`transaction.c` `commit_body` 3784-3786).
/// Format: the filesystem category's start plus 28 (`svn_error_codes.h` 693-694).
const SVN_ERR_FS_TXN_OUT_OF_DATE: u32 = 160_028;

/// A node's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
  /// A file: bytes and properties.
  File,
  /// A directory: entries and properties.
  Directory,
}

/// A node identity: the line of history a node-revision belongs to, born at an addition and kept by every change and
/// every copy of it. FSFS writes it `<number>-<revision>` (`structure`, "Node-revision IDs").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId {
  /// The revision the line was born in; 0 for the root's.
  pub revision: Revision,
  /// Its number among the lines born in that revision.
  pub number: u64,
}

/// A copy identity: which copy, true or soft, a node-revision descends through; FSFS writes it as a node identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CopyId {
  /// The revision the copy was made in; 0 for what was never copied.
  pub revision: Revision,
  /// Its number among the copy identities that revision made.
  pub number: u64,
}

/// A node-revision of one index: what FSFS's node-revision ID names, compared only for equality and order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeRevisionId(usize);

/// A file's text as the green holds it and a client checks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Text {
  /// The content identity the green holds the bytes by.
  pub identity: ContentIdentity,
  /// The MD5 digest `apply-textdelta` and `close-file` carry.
  pub md5: [u8; 16],
  /// The SHA-1 digest FSFS format 4 and later keep.
  pub sha1: [u8; 20],
  /// The text's length in bytes.
  pub size: u64,
}

/// A node's property list: none, or a sorted list held as content and named by its identity (§4.17.4 item 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyList {
  /// No properties; FSFS omits the representation.
  Empty,
  /// A list held as content.
  Held {
    /// The list's content identity.
    identity: ContentIdentity,
    /// Whether it holds `svn:mergeinfo` (FSFS's `minfo-here`).
    has_mergeinfo: bool,
  },
}

impl PropertyList {
  /// Whether the list holds `svn:mergeinfo`.
  pub fn has_mergeinfo(self) -> bool {
    matches!(
      self,
      Self::Held {
        has_mergeinfo: true,
        ..
      }
    )
  }
}

/// What a revision did to a path, as its changed-path row names it (`structure`, "The changed-path data").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
  /// The path was absent and now holds a new node, or a copy.
  Add,
  /// The path held a node and now holds nothing, nor does anything that was beneath it.
  Delete,
  /// The path held a node, which was deleted, and now holds a new node, or a copy.
  Replace,
  /// The path's node changed its text, its property list or both.
  Modify,
}

/// Where a copy was made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopySource {
  /// The source's path.
  pub path: String,
  /// The revision the source was read in, before the copy's own.
  pub revision: Revision,
}

/// One changed-path row of a revision, with the facts of the node it leaves (§4.17.4 item 3.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
  /// The path, in Subversion's canonical form.
  pub path: String,
  /// What the revision did to it.
  pub action: Action,
  /// The kind of the node it leaves, or for a deletion the kind of the node deleted (FSFS format 4 and later).
  pub kind: NodeKind,
  /// The copy source of an addition or a replacement made by a copy.
  pub copy_from: Option<CopySource>,
  /// The text the row leaves when it changed the text (FSFS's `text-mod`); otherwise the node keeps its predecessor's
  /// or its copy source's text, and a new file without one is empty.
  pub text: Option<Text>,
  /// The property list the row leaves when it changed the properties (FSFS's `prop-mod`); otherwise the node keeps its
  /// predecessor's or its copy source's list, and a new node without one has none.
  pub properties: Option<PropertyList>,
  /// Whether `svn:mergeinfo` was set, changed or deleted (FSFS's `mergeinfo-mod`).
  pub mergeinfo_modified: bool,
}

/// One revision's record: what the index is computed from (§4.17.4 item 3.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionRecord {
  /// The transaction's name (§4.17.4 item 3.1).
  pub transaction: String,
  /// The changed-path rows, in any order.
  pub changes: Vec<ChangedPath>,
}

/// How two node-revisions are related (`svn_fs.h` `svn_fs_node_relation_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeRelation {
  /// Different lines of history.
  Unrelated,
  /// The same node-revision.
  Unchanged,
  /// Different node-revisions of one line of history.
  CommonAncestor,
}

/// What happened at a path in one revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HistoryEvent {
  /// A node-revision was created at the path.
  Created(NodeRevisionId),
  /// The path's node-revision was deleted.
  Deleted(NodeRevisionId),
}

/// One entry of a path's history list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HistoryEntry {
  /// The revision.
  pub revision: Revision,
  /// What it did at the path.
  pub event: HistoryEvent,
}

impl HistoryEntry {
  /// The node-revision this entry left at its path, if any.
  fn created(self) -> Option<NodeRevisionId> {
    match self.event {
      HistoryEvent::Created(node) => Some(node),
      HistoryEvent::Deleted(_) => None,
    }
  }
}

/// Which of a row's own fields contradict each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowShape {
  /// A deletion carrying a copy source, a text or a property list.
  DeleteCarriesChange,
  /// A modification carrying a copy source.
  ModifyCopies,
  /// A modification changing neither the text nor the property list.
  ModifyChangesNothing,
  /// The mergeinfo flag on a row that does not change the property list.
  MergeinfoWithoutProperties,
  /// A node that gains or loses `svn:mergeinfo` without the mergeinfo flag.
  MergeinfoUnflagged,
}

/// Why the index refused a record or a question, each answered with a Subversion error
/// ([`IndexRefusal::subversion_code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexRefusal {
  /// A path out of Subversion's canonical form (`SVN_ERR_FS_PATH_SYNTAX`).
  PathSyntax {
    /// The path.
    path: String,
  },
  /// A revision past the youngest, or a copy source not before its copy (`SVN_ERR_FS_NO_SUCH_REVISION`).
  NoSuchRevision {
    /// The revision.
    revision: Revision,
  },
  /// A path absent in a revision: a question's, a copy source's, or a row's or one of its directories' in the revision
  /// its record builds (`SVN_ERR_FS_NOT_FOUND`).
  NotFound {
    /// The path.
    path: String,
    /// The revision.
    revision: Revision,
  },
  /// A node-revision this index never made (`SVN_ERR_FS_ID_NOT_FOUND`).
  UnknownNodeRevision,
  /// An addition at a path its revision already holds (`SVN_ERR_FS_ALREADY_EXISTS`).
  AlreadyExists {
    /// The path.
    path: String,
  },
  /// A directory of a row's path that is a file (`SVN_ERR_FS_NOT_DIRECTORY`).
  NotDirectory {
    /// The file.
    path: String,
  },
  /// A text on a directory (`SVN_ERR_FS_NOT_FILE`).
  DirectoryText {
    /// The directory.
    path: String,
  },
  /// A row adding, deleting or replacing the root (`SVN_ERR_FS_ROOT_DIR`).
  RootChange,
  /// A row whose kind differs from the node it changes or deletes, or from its copy's source (`SVN_ERR_FS_CORRUPT`, as
  /// for every record no FSFS commit writes).
  KindMismatch {
    /// The row's path.
    path: String,
  },
  /// Two rows for one path (`SVN_ERR_FS_CORRUPT`).
  Duplicate {
    /// The path.
    path: String,
  },
  /// A row whose own fields contradict each other (`SVN_ERR_FS_CORRUPT`).
  Malformed {
    /// The row's path.
    path: String,
    /// How.
    shape: RowShape,
  },
  /// A transaction name that is empty or holds a byte `svn_fs.h` does not allow (`SVN_ERR_FS_CORRUPT`).
  TransactionName {
    /// The name.
    name: String,
  },
  /// The index would hold more entries than its caller's room (`SVN_ERR_FS_GENERAL`: Subversion has no closer error).
  Room {
    /// The entries it would hold.
    needed: usize,
    /// The room given.
    room: usize,
  },
  /// A plan committed after another revision was appended since it was made ([`Index::commit_planned`]): FSFS's
  /// commit of a transaction whose base is no longer the youngest (`SVN_ERR_FS_TXN_OUT_OF_DATE`).
  OutOfDate {
    /// The revision the plan makes.
    planned: Revision,
    /// The revision the index makes next.
    next: Revision,
  },
  /// A count or an identity past 2^64 - 1, or a row the index names and does not hold (`SVN_ERR_FS_CORRUPT`).
  Corrupt,
}

impl IndexRefusal {
  /// The number of the Subversion error a client is answered with.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::PathSyntax { .. } => SVN_ERR_FS_PATH_SYNTAX,
      Self::NoSuchRevision { .. } => SVN_ERR_FS_NO_SUCH_REVISION,
      Self::NotFound { .. } => SVN_ERR_FS_NOT_FOUND,
      Self::UnknownNodeRevision => SVN_ERR_FS_ID_NOT_FOUND,
      Self::AlreadyExists { .. } => SVN_ERR_FS_ALREADY_EXISTS,
      Self::NotDirectory { .. } => SVN_ERR_FS_NOT_DIRECTORY,
      Self::DirectoryText { .. } => SVN_ERR_FS_NOT_FILE,
      Self::RootChange => SVN_ERR_FS_ROOT_DIR,
      Self::Room { .. } => SVN_ERR_FS_GENERAL,
      Self::OutOfDate { .. } => SVN_ERR_FS_TXN_OUT_OF_DATE,
      Self::KindMismatch { .. }
      | Self::Duplicate { .. }
      | Self::Malformed { .. }
      | Self::TransactionName { .. }
      | Self::Corrupt => SVN_ERR_FS_CORRUPT,
    }
  }
}

/// One node-revision's row (the module documentation's list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
  /// Its kind.
  kind: NodeKind,
  /// Its line of history.
  node_id: NodeId,
  /// The copy it descends through.
  copy_id: CopyId,
  /// The number of the path it was created at.
  created_path: usize,
  /// The revision it was created in.
  created_revision: Revision,
  /// The node-revision it succeeds, across copies.
  predecessor: Option<NodeRevisionId>,
  /// How many predecessors it has.
  predecessor_count: u64,
  /// The number of its copy source's path and the source's revision, when a copy made it.
  copy_from: Option<(usize, Revision)>,
  /// The node-revision made by the copy nearest above it in its history, or revision 0's root.
  copy_root: NodeRevisionId,
  /// Its text; `None` for a directory and for a file with no text.
  text: Option<Text>,
  /// Its property list.
  properties: PropertyList,
  /// The nodes holding `svn:mergeinfo` at and beneath it.
  mergeinfo_count: u64,
  /// A directory's children, its map's handle ([`children`], A-759); [`children::EMPTY`] for a file and an empty
  /// directory.
  children: u32,
}

/// How a path's shape changed in one revision, which decides what stands beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
  /// A new node without history: nothing beneath it but what is added after.
  Fresh,
  /// A copy: beneath it stands what stands beneath its source.
  Copied {
    /// The number of the source's path.
    from: usize,
    /// The source's revision.
    revision: Revision,
  },
  /// A deletion: nothing stands beneath it.
  Deleted,
}

/// One entry of a path's shape list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShapeEntry {
  /// The revision.
  revision: Revision,
  /// The change.
  shape: Shape,
}

/// What the index holds about one path.
#[derive(Clone, Debug)]
struct PathEntry {
  /// The path.
  path: Box<str>,
  /// Its history list, by revision.
  history: Vec<HistoryEntry>,
  /// Its shape list, by revision.
  shapes: Vec<ShapeEntry>,
}

/// What the index holds about one revision.
#[derive(Clone, Debug)]
struct RevisionEntry {
  /// Its transaction's name; revision 0 had none.
  transaction: Option<Box<str>>,
  /// Its changed-path rows, sorted by path.
  changes: Vec<ChangedPath>,
}

/// The entry at or before `at` that is last in `entries`, sorted by `revision_of`.
fn last_at_or_before<T>(
  entries: &[T],
  at: Revision,
  revision_of: impl Fn(&T) -> Revision,
) -> Option<&T> {
  let past = entries.partition_point(|entry| revision_of(entry) <= at);
  entries.get(past.checked_sub(1)?)
}

/// Whether `path` is in Subversion's canonical form for a repository path: `/` alone, or `/` followed by names joined
/// by `/` (`svn_fspath__is_canonical`; `svn_fs__path_valid`).
pub(crate) fn is_canonical(path: &str) -> bool {
  if path == ROOT {
    return true;
  }
  path
    .strip_prefix(SEPARATOR)
    .is_some_and(|names| names.split(SEPARATOR).all(is_valid_name))
}

/// Whether one name of a path is one FSFS stores: not empty, `.` or `..`, and holding no forbidden character.
fn is_valid_name(name: &str) -> bool {
  !name.is_empty()
    && name != CURRENT_DIRECTORY
    && name != PARENT_DIRECTORY
    && !name.contains(FORBIDDEN_IN_NAMES)
}

/// `path`, refused unless canonical.
fn checked_path(path: &str) -> Result<&str, IndexRefusal> {
  if is_canonical(path) {
    Ok(path)
  } else {
    Err(IndexRefusal::PathSyntax {
      path: path.to_owned(),
    })
  }
}

/// The directories above `path` other than the root, outermost first, each with where it ends in `path`: `/a/b/c`
/// gives `/a` ending at 2 and `/a/b` ending at 4.
fn inner_ancestors(path: &str) -> impl DoubleEndedIterator<Item = (&str, usize)> {
  path.match_indices(SEPARATOR).filter_map(move |(end, _)| {
    path
      .get(..end)
      .filter(|ancestor| !ancestor.is_empty())
      .map(|ancestor| (ancestor, end))
  })
}

/// The path at the same place beneath `source` as `path` stands beneath its directory ending at `end`.
fn beneath(source: &str, path: &str, end: usize) -> Result<String, IndexRefusal> {
  let rest = path.get(end..).ok_or(IndexRefusal::Corrupt)?;
  if source == ROOT {
    Ok(rest.to_owned())
  } else {
    Ok([source, rest].concat())
  }
}

/// The predecessor count of a node-revision's successor.
fn next_count(count: u64) -> Result<u64, IndexRefusal> {
  count.checked_add(1).ok_or(IndexRefusal::Corrupt)
}

/// `count` with the node's own `svn:mergeinfo`, which it `had`, now as it `has` it.
fn recounted(count: u64, had: bool, has: bool) -> Result<u64, IndexRefusal> {
  count
    .checked_sub(u64::from(had))
    .and_then(|rest| rest.checked_add(u64::from(has)))
    .ok_or(IndexRefusal::Corrupt)
}

/// Refuses `name` unless it is a transaction name `svn_fs.h` allows.
fn check_transaction_name(name: &str) -> Result<(), IndexRefusal> {
  let allowed =
    |byte: u8| byte.is_ascii_alphanumeric() || TRANSACTION_NAME_PUNCTUATION.contains(&byte);
  if !name.is_empty() && name.bytes().all(allowed) {
    Ok(())
  } else {
    Err(IndexRefusal::TransactionName {
      name: name.to_owned(),
    })
  }
}

/// A row's own contradiction, if it has one.
fn contradiction(change: &ChangedPath) -> Option<RowShape> {
  let changes_content = change.text.is_some() || change.properties.is_some();
  match change.action {
    Action::Delete if change.copy_from.is_some() || changes_content => {
      Some(RowShape::DeleteCarriesChange)
    }
    Action::Modify if change.copy_from.is_some() => Some(RowShape::ModifyCopies),
    Action::Modify if !changes_content => Some(RowShape::ModifyChangesNothing),
    _ if change.mergeinfo_modified && change.properties.is_none() => {
      Some(RowShape::MergeinfoWithoutProperties)
    }
    _ => None,
  }
}

/// Refuses a row out of shape on its own, before the revision is built.
fn check_row(change: &ChangedPath) -> Result<(), IndexRefusal> {
  checked_path(&change.path)?;
  if let Some(source) = &change.copy_from {
    checked_path(&source.path)?;
  }
  if change.kind == NodeKind::Directory && change.text.is_some() {
    return Err(IndexRefusal::DirectoryText {
      path: change.path.clone(),
    });
  }
  match contradiction(change) {
    Some(shape) => Err(IndexRefusal::Malformed {
      path: change.path.clone(),
      shape,
    }),
    None => Ok(()),
  }
}

/// The rows, each checked, sorted by path; refused if two name one path.
fn sorted_rows(mut changes: Vec<ChangedPath>) -> Result<Vec<ChangedPath>, IndexRefusal> {
  for change in &changes {
    check_row(change)?;
  }
  changes.sort_by(|first, second| first.path.cmp(&second.path));
  for pair in changes.windows(2) {
    if let [first, second] = pair
      && first.path == second.path
    {
      return Err(IndexRefusal::Duplicate {
        path: first.path.clone(),
      });
    }
  }
  Ok(changes)
}

/// Refuses a row whose `mergeinfo_modified` flag is clear while its node gains or loses `svn:mergeinfo`.
fn check_mergeinfo_flag(change: &ChangedPath, had: bool, has: bool) -> Result<(), IndexRefusal> {
  if had != has && !change.mergeinfo_modified {
    Err(IndexRefusal::Malformed {
      path: change.path.clone(),
      shape: RowShape::MergeinfoUnflagged,
    })
  } else {
    Ok(())
  }
}

/// A node-revision as the index answers it: its row, read through accessors.
#[derive(Clone, Copy, Debug)]
pub struct Node<'index> {
  /// Which node-revision.
  id: NodeRevisionId,
  /// Its row.
  row: &'index Row,
  /// The path it was created at.
  created_path: &'index str,
  /// Its copy source's path and revision.
  copy_from: Option<(&'index str, Revision)>,
}

impl<'index> Node<'index> {
  /// Which node-revision this is: equal for one node-revision reached by any path.
  pub fn id(&self) -> NodeRevisionId {
    self.id
  }

  /// Its kind.
  pub fn kind(&self) -> NodeKind {
    self.row.kind
  }

  /// Its node identity.
  pub fn node_id(&self) -> NodeId {
    self.row.node_id
  }

  /// Its copy identity.
  pub fn copy_id(&self) -> CopyId {
    self.row.copy_id
  }

  /// The path it was created at, which differs from the path asked when it is reached beneath a copy.
  pub fn created_path(&self) -> &'index str {
    self.created_path
  }

  /// The revision it was created in: what the out-of-date test, `stat` and `svn:entry:committed-rev` read.
  pub fn created_revision(&self) -> Revision {
    self.row.created_revision
  }

  /// The node-revision it succeeds, across copies (`structure`: "The predecessor of a node-rev crosses both soft and
  /// true copies").
  pub fn predecessor(&self) -> Option<NodeRevisionId> {
    self.row.predecessor
  }

  /// How many predecessors it has.
  pub fn predecessor_count(&self) -> u64 {
    self.row.predecessor_count
  }

  /// Its copy source's path and revision, when a copy made it.
  pub fn copy_from(&self) -> Option<(&'index str, Revision)> {
    self.copy_from
  }

  /// The node-revision of the copy nearest above it in its history, or revision 0's root; its created path and
  /// revision are FSFS's `copyroot`.
  pub fn copy_root(&self) -> NodeRevisionId {
    self.row.copy_root
  }

  /// Its text: `None` for a directory and for a file with no text.
  pub fn text(&self) -> Option<Text> {
    self.row.text
  }

  /// Its property list.
  pub fn properties(&self) -> PropertyList {
    self.row.properties
  }

  /// Whether it holds `svn:mergeinfo` (FSFS's `minfo-here`).
  pub fn has_mergeinfo(&self) -> bool {
    self.row.properties.has_mergeinfo()
  }

  /// The nodes holding `svn:mergeinfo` at and beneath it (FSFS's `minfo-cnt`).
  pub fn mergeinfo_count(&self) -> u64 {
    self.row.mergeinfo_count
  }
}

/// What decides the node at a path in a revision (module documentation, "Reading").
enum Decided {
  /// The node, or nothing.
  Node(Option<NodeRevisionId>),
  /// The node at this path in this revision.
  Copied {
    /// The path beneath the copy's source.
    source: String,
    /// The source's revision.
    revision: Revision,
  },
}

/// The Subversion index of one repository (module documentation).
#[derive(Clone, Debug)]
pub struct Index {
  /// Every path the index has named, by number.
  paths: Vec<PathEntry>,
  /// Each named path's number.
  numbers: BTreeMap<Box<str>, usize>,
  /// Every node-revision, by [`NodeRevisionId`].
  rows: Vec<Row>,
  /// Every revision's rows, by revision.
  revisions: Vec<RevisionEntry>,
  /// The youngest revision.
  youngest: Revision,
  /// The deletions the history lists hold.
  deletions: usize,
  /// The nodes of every directory row's children map (A-759).
  maps: Vec<children::MapNode>,
  /// The bytes those nodes cost (`children::MapNode::bytes`), charged to the room in [`Index::ENTRY_BYTES`] units.
  map_bytes: usize,
}

impl Default for Index {
  fn default() -> Self {
    Self::new()
  }
}

impl Index {
  /// A repository at revision 0: an empty root directory.
  pub fn new() -> Self {
    let root = Row {
      kind: NodeKind::Directory,
      node_id: ROOT_NODE,
      copy_id: ROOT_COPY,
      created_path: 0,
      created_revision: 0,
      predecessor: None,
      predecessor_count: 0,
      copy_from: None,
      copy_root: FIRST_ROOT,
      text: None,
      properties: PropertyList::Empty,
      mergeinfo_count: 0,
      children: children::EMPTY,
    };
    let created = HistoryEntry {
      revision: 0,
      event: HistoryEvent::Created(FIRST_ROOT),
    };
    Self {
      paths: vec![PathEntry {
        path: ROOT.into(),
        history: vec![created],
        shapes: Vec::new(),
      }],
      numbers: BTreeMap::from([(Box::<str>::from(ROOT), 0)]),
      rows: vec![root],
      revisions: vec![RevisionEntry {
        transaction: None,
        changes: Vec::new(),
      }],
      youngest: 0,
      deletions: 0,
      maps: Vec::new(),
      map_bytes: 0,
    }
  }

  /// The youngest revision.
  pub fn youngest(&self) -> Revision {
    self.youngest
  }

  /// Derived: the fewest bytes an entry of the index takes in memory, the smaller of a node row's and a deletion's
  /// history entry's as the compiler lays them out: what an owner deriving the room's entries from bytes divides by
  /// (A-655 part 2b), so that the entries alone can never pass the bytes the room was derived from.
  pub const ENTRY_BYTES: usize = if size_of::<Row>() < size_of::<HistoryEntry>() {
    size_of::<Row>()
  } else {
    size_of::<HistoryEntry>()
  };

  /// The entries the index holds, node rows and deletions, and the directory maps' bytes in [`Index::ENTRY_BYTES`]
  /// units (A-759): what the room given to [`Index::append`] bounds.
  pub fn entries(&self) -> usize {
    self
      .rows
      .len()
      .saturating_add(self.deletions)
      .saturating_add(map_units(self.map_bytes))
  }

  /// Applies the next revision's record and answers its number: [`Index::plan`], then [`Index::commit_planned`].
  /// Refused, typed, when the record is not one an FSFS commit writes against the youngest revision or when the index
  /// would hold more than `room` entries; a refused record changes nothing.
  pub fn append(&mut self, record: RevisionRecord, room: usize) -> Result<Revision, IndexRefusal> {
    let planned = self.plan(record, room)?;
    self.commit_planned(planned)
  }

  /// Checks the next revision's record against the youngest revision, every check [`Index::append`] makes, and
  /// changes nothing: the revision it makes, ready for [`Index::commit_planned`]. An owner checks a revision so
  /// before its log records it and commits it after, so a record the index refuses is never recorded (A-655).
  pub fn plan(&self, record: RevisionRecord, room: usize) -> Result<Planned, IndexRefusal> {
    let revision = self.youngest.checked_add(1).ok_or(IndexRefusal::Corrupt)?;
    check_transaction_name(&record.transaction)?;
    let changes = sorted_rows(record.changes)?;
    let mut builder = Builder::new(self, revision)?;
    for change in &changes {
      builder.apply(change)?;
    }
    let plan = builder.finish()?;
    let needed = self
      .rows
      .len()
      .checked_add(self.deletions)
      .and_then(|entries| entries.checked_add(plan.rows.len()))
      .and_then(|entries| entries.checked_add(plan.deletions))
      .and_then(|entries| {
        entries.checked_add(map_units(self.map_bytes.checked_add(plan.map_bytes)?))
      })
      .ok_or(IndexRefusal::Corrupt)?;
    if needed > room {
      return Err(IndexRefusal::Room { needed, room });
    }
    self.check_plan(&plan)?;
    Ok(Planned {
      revision,
      plan,
      transaction: record.transaction,
      changes,
    })
  }

  /// Commits a revision [`Index::plan`] checked, and answers its number. Refused with nothing changed when a revision
  /// was appended since the plan was made ([`IndexRefusal::OutOfDate`]): the plan numbers its rows and paths after the
  /// index as it was then.
  pub fn commit_planned(&mut self, planned: Planned) -> Result<Revision, IndexRefusal> {
    let next = self.youngest.checked_add(1).ok_or(IndexRefusal::Corrupt)?;
    if planned.revision != next {
      return Err(IndexRefusal::OutOfDate {
        planned: planned.revision,
        next,
      });
    }
    let Planned {
      revision,
      plan,
      transaction,
      changes,
    } = planned;
    self.commit(revision, plan, transaction, changes);
    Ok(revision)
  }

  /// The transaction `work` makes on revision `base` (§4.17.4 item 3), as FSFS builds it from the drive, never
  /// committed: each node it made, cloned, copied or left, with the identities FSFS gives them, for the commit verdict to
  /// compare with the base's and the head's (`crate::commit`). Its rows are checked as a record's are against the
  /// youngest revision ([`Index::append`]), here against `base`, and its new identities are the next revision's numbers,
  /// so none equals one the index holds. Refused, typed, as a record is.
  pub fn trial(&self, base: Revision, work: &RevisionRecord) -> Result<Trial<'_>, IndexRefusal> {
    self.check_revision(base)?;
    check_transaction_name(&work.transaction)?;
    let revision = self.youngest.checked_add(1).ok_or(IndexRefusal::Corrupt)?;
    let changes = sorted_rows(work.changes.clone())?;
    let mut builder = Builder::new_on(self, revision, base)?;
    for change in &changes {
      builder.apply(change)?;
    }
    Ok(Trial { builder })
  }

  /// The node at `path` in `revision`, or `None` when the path is absent there.
  pub fn node(&self, path: &str, revision: Revision) -> Result<Option<Node<'_>>, IndexRefusal> {
    let path = checked_path(path)?;
    self.check_revision(revision)?;
    match self.resolve(path, revision)? {
      Some(id) => self.node_revision(id).map(Some),
      None => Ok(None),
    }
  }

  /// The names of the directory at `path` in `revision` in byte order (`svn_fs_dir_entries`' keys), none when no
  /// directory stands there. A-759: the directory resolved once and its children map walked, O(live children).
  ///
  /// # Errors
  /// [`IndexRefusal`] for a path out of canonical form, a revision past the youngest, and an index whose map does
  /// not hold together.
  pub fn children(
    &self,
    path: &str,
    revision: Revision,
  ) -> Result<Option<Vec<String>>, IndexRefusal> {
    Ok(
      self
        .listing(path, revision)?
        .map(|listed| listed.into_iter().map(|(name, _)| name).collect()),
    )
  }

  /// The children of the directory at `path` in `revision` in byte order, each with its node-revision (A-759): what
  /// a drive makes each child's entry from with no second resolve. None when no directory stands there.
  ///
  /// # Errors
  /// [`Index::children`]'s.
  pub fn listing(
    &self,
    path: &str,
    revision: Revision,
  ) -> Result<Option<Vec<(String, NodeRevisionId)>>, IndexRefusal> {
    Ok(
      self
        .listing_after(path, revision, None, usize::MAX)?
        .map(|(listed, _)| listed),
    )
  }

  /// Up to `most` children of the directory at `path` in `revision` whose names come after `after` (all, without one),
  /// in byte order with their node-revisions, and whether more follow: a cursor a caller reads a very large directory
  /// through a few entries a step (A-759). None when no directory stands there.
  ///
  /// # Errors
  /// [`Index::children`]'s.
  pub fn listing_after(
    &self,
    path: &str,
    revision: Revision,
    after: Option<&str>,
    most: usize,
  ) -> Result<Option<Listing>, IndexRefusal> {
    let path = checked_path(path)?;
    self.check_revision(revision)?;
    let Some(id) = self.resolve(path, revision)? else {
      return Ok(None);
    };
    let row = self.rows.get(id.0).ok_or(IndexRefusal::Corrupt)?;
    if row.kind != NodeKind::Directory {
      return Ok(None);
    }
    let (held, more) =
      children::entries_after(self, row.children, after, most).ok_or(IndexRefusal::Corrupt)?;
    let mut listed = Vec::with_capacity(held.len());
    for (key, child) in held {
      let key = u32::try_from(key).map_err(|_| IndexRefusal::Corrupt)?;
      let name = children::MapRead::key_name(self, key).ok_or(IndexRefusal::Corrupt)?;
      listed.push((name.to_owned(), child));
    }
    Ok(Some((listed, more)))
  }

  /// The node-revision `id`, as [`Node::predecessor`] and [`Node::copy_root`] name one.
  pub fn node_revision(&self, id: NodeRevisionId) -> Result<Node<'_>, IndexRefusal> {
    let row = self
      .rows
      .get(id.0)
      .ok_or(IndexRefusal::UnknownNodeRevision)?;
    let created_path = self.path_name(row.created_path)?;
    let copy_from = match row.copy_from {
      Some((from, revision)) => Some((self.path_name(from)?, revision)),
      None => None,
    };
    Ok(Node {
      id,
      row,
      created_path,
      copy_from,
    })
  }

  /// The node-revisions `revision` created, in the order the index made them: what FSFS's revision file for it holds
  /// (A-738). Each revision's rows are appended together and no row is created in a revision before its own, so they
  /// are one run of the rows.
  pub fn created_in(&self, revision: Revision) -> Result<Vec<Node<'_>>, IndexRefusal> {
    self.check_revision(revision)?;
    let start = self
      .rows
      .partition_point(|row| row.created_revision < revision);
    let end = self
      .rows
      .partition_point(|row| row.created_revision <= revision);
    (start..end)
      .map(|number| self.node_revision(NodeRevisionId(number)))
      .collect()
  }

  /// The revision the node at `path` in `revision` was created in, or `None` when the path is absent there.
  pub fn created_revision(
    &self,
    path: &str,
    revision: Revision,
  ) -> Result<Option<Revision>, IndexRefusal> {
    Ok(
      self
        .node(path, revision)?
        .map(|node| node.created_revision()),
    )
  }

  /// How the nodes at two places are related (`svn_fs_node_relation`); refused when either is absent.
  pub fn relation(
    &self,
    first: (&str, Revision),
    second: (&str, Revision),
  ) -> Result<NodeRelation, IndexRefusal> {
    let first = self.present(first)?;
    let second = self.present(second)?;
    Ok(if first.id() == second.id() {
      NodeRelation::Unchanged
    } else if first.node_id() == second.node_id() {
      NodeRelation::CommonAncestor
    } else {
      NodeRelation::Unrelated
    })
  }

  /// The history list of `path`: the revisions at which a node-revision was created there, deletions included,
  /// oldest first; empty for a path no revision named.
  pub fn history(&self, path: &str) -> Result<&[HistoryEntry], IndexRefusal> {
    let path = checked_path(path)?;
    Ok(
      self
        .entry(path)
        .map(|entry| entry.history.as_slice())
        .unwrap_or_default(),
    )
  }

  /// The changed-path rows of `revision`, sorted by path.
  pub fn changes(&self, revision: Revision) -> Result<&[ChangedPath], IndexRefusal> {
    Ok(&self.revision_entry(revision)?.changes)
  }

  /// The transaction name `revision` was committed from; revision 0 had none.
  pub fn transaction(&self, revision: Revision) -> Result<Option<&str>, IndexRefusal> {
    Ok(self.revision_entry(revision)?.transaction.as_deref())
  }

  /// The node at a place, refused when absent.
  fn present(&self, (path, revision): (&str, Revision)) -> Result<Node<'_>, IndexRefusal> {
    self
      .node(path, revision)?
      .ok_or_else(|| IndexRefusal::NotFound {
        path: path.to_owned(),
        revision,
      })
  }

  /// Refuses a revision past the youngest.
  fn check_revision(&self, revision: Revision) -> Result<(), IndexRefusal> {
    if revision > self.youngest {
      Err(IndexRefusal::NoSuchRevision { revision })
    } else {
      Ok(())
    }
  }

  /// What the index holds about `revision`.
  fn revision_entry(&self, revision: Revision) -> Result<&RevisionEntry, IndexRefusal> {
    self.check_revision(revision)?;
    usize::try_from(revision)
      .ok()
      .and_then(|position| self.revisions.get(position))
      .ok_or(IndexRefusal::Corrupt)
  }

  /// What the index holds about `path`, if it named it.
  fn entry(&self, path: &str) -> Option<&PathEntry> {
    self
      .numbers
      .get(path)
      .and_then(|&number| self.paths.get(number))
  }

  /// The path numbered `number`.
  fn path_name(&self, number: usize) -> Result<&str, IndexRefusal> {
    self
      .paths
      .get(number)
      .map(|entry| &*entry.path)
      .ok_or(IndexRefusal::Corrupt)
  }

  /// The node-revision at `path` in `revision`, following copies (module documentation, "Reading").
  fn resolve(
    &self,
    path: &str,
    revision: Revision,
  ) -> Result<Option<NodeRevisionId>, IndexRefusal> {
    let mut place = Cow::Borrowed(path);
    let mut at = revision;
    loop {
      match self.decide(&place, at)? {
        Decided::Node(node) => return Ok(node),
        // A copy's source revision is before the copy's (refused otherwise at its append), so each step goes back.
        Decided::Copied { source, revision } => {
          place = Cow::Owned(source);
          at = revision;
        }
      }
    }
  }

  /// What decides the node at `path` in revision `at`: its own last history entry, or the latest shape change above
  /// it after that entry.
  fn decide(&self, path: &str, at: Revision) -> Result<Decided, IndexRefusal> {
    let own = self
      .entry(path)
      .and_then(|entry| last_at_or_before(&entry.history, at, |entry| entry.revision))
      .copied();
    match self.shape_above(path, at, own.map(|entry| entry.revision)) {
      None => Ok(Decided::Node(own.and_then(HistoryEntry::created))),
      Some((
        ShapeEntry {
          shape: Shape::Copied { from, revision },
          ..
        },
        end,
      )) => Ok(Decided::Copied {
        source: beneath(self.path_name(from)?, path, end)?,
        revision,
      }),
      Some(_) => Ok(Decided::Node(None)),
    }
  }

  /// The latest shape change at or before `at` of a directory above `path`, later than `own` (the path's own last
  /// entry), with where that directory ends in `path`; between two of one revision, the deeper.
  fn shape_above(
    &self,
    path: &str,
    at: Revision,
    own: Option<Revision>,
  ) -> Option<(ShapeEntry, usize)> {
    let mut latest: Option<(ShapeEntry, usize)> = None;
    for (ancestor, end) in inner_ancestors(path) {
      let Some(&change) = self
        .entry(ancestor)
        .and_then(|entry| last_at_or_before(&entry.shapes, at, |shape| shape.revision))
      else {
        continue;
      };
      let after_own = own.is_none_or(|own| change.revision > own);
      let not_older = latest.is_none_or(|(kept, _)| change.revision >= kept.revision);
      if after_own && not_older {
        latest = Some((change, end));
      }
    }
    latest
  }

  /// Refuses a plan naming a path past those it adds, so that [`Index::commit`] cannot miss one.
  fn check_plan(&self, plan: &Plan) -> Result<(), IndexRefusal> {
    let named = self
      .paths
      .len()
      .checked_add(plan.paths.len())
      .ok_or(IndexRefusal::Corrupt)?;
    if plan.events.iter().all(|(number, _)| *number < named) {
      Ok(())
    } else {
      Err(IndexRefusal::Corrupt)
    }
  }

  /// Applies a checked plan: nothing here can be refused.
  fn commit(
    &mut self,
    revision: Revision,
    plan: Plan,
    transaction: String,
    changes: Vec<ChangedPath>,
  ) {
    for path in plan.paths {
      self.numbers.insert(path.clone(), self.paths.len());
      self.paths.push(PathEntry {
        path,
        history: Vec::new(),
        shapes: Vec::new(),
      });
    }
    self.rows.extend(plan.rows);
    self.maps.extend(plan.maps);
    self.map_bytes = self.map_bytes.saturating_add(plan.map_bytes);
    for (number, staged) in plan.events {
      if let Some(entry) = self.paths.get_mut(number) {
        entry.record(revision, staged);
      }
    }
    self.revisions.push(RevisionEntry {
      transaction: Some(transaction.into_boxed_str()),
      changes,
    });
    self.youngest = revision;
    self.deletions = self.deletions.saturating_add(plan.deletions);
  }
}

impl PathEntry {
  /// Records what one revision did at the path.
  fn record(&mut self, revision: Revision, staged: Staged) {
    let event = match (staged.node, staged.deleted) {
      (Some(node), _) => Some(HistoryEvent::Created(node)),
      (None, Some(deleted)) => Some(HistoryEvent::Deleted(deleted)),
      (None, None) => None,
    };
    if let Some(event) = event {
      self.history.push(HistoryEntry { revision, event });
    }
    if let Some(shape) = staged.shape {
      self.shapes.push(ShapeEntry { revision, shape });
    }
  }
}

/// What a revision being built did at one path so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Staged {
  /// The node-revision this revision made there, which is mutable until the revision is done.
  node: Option<NodeRevisionId>,
  /// The node-revision this revision deleted there.
  deleted: Option<NodeRevisionId>,
  /// The shape change this revision made there.
  shape: Option<Shape>,
}

/// A revision's record checked against the youngest revision by [`Index::plan`]: what [`Index::commit_planned`]
/// commits, so long as no revision was appended since.
#[derive(Debug)]
pub struct Planned {
  /// The revision it makes.
  revision: Revision,
  /// What it makes: its node-revisions, its new paths and what it did at each path.
  plan: Plan,
  /// The transaction's name.
  transaction: String,
  /// The changed-path rows, sorted by path.
  changes: Vec<ChangedPath>,
}

impl Planned {
  /// The revision it makes.
  pub fn revision(&self) -> Revision {
    self.revision
  }

  /// Its changed-path rows, sorted by path.
  pub fn changes(&self) -> &[ChangedPath] {
    &self.changes
  }
}

/// A revision built and checked, ready to be committed.
#[derive(Debug)]
struct Plan {
  /// The node-revisions it made, numbered after the index's.
  rows: Vec<Row>,
  /// The paths it names first, numbered after the index's.
  paths: Vec<Box<str>>,
  /// What it did at each path, by number.
  events: Vec<(usize, Staged)>,
  /// The deletions it made.
  deletions: usize,
  /// The children maps' nodes it made, numbered after the index's (A-759).
  maps: Vec<children::MapNode>,
  /// The bytes they cost.
  map_bytes: usize,
}

/// How a node takes its copy identity when it is made mutable (`tree.c` `copy_id_inherit_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inherit {
  /// It keeps its own (`copy_id_inherit_self`).
  Itself,
  /// It takes its parent's, as the parent stands once mutable (`copy_id_inherit_parent`).
  Parent,
  /// It takes a fresh one: a soft copy (`copy_id_inherit_new`).
  Fresh,
}

/// A work applied to a base revision as FSFS builds its transaction, never committed ([`Index::trial`]).
pub struct Trial<'index> {
  builder: Builder<'index>,
}

/// A node of a trial's transaction: its node-revision (one the index holds when the work left it, or one the work
/// made), its kind, its identities and its property list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrialNode {
  /// Its node-revision.
  pub id: NodeRevisionId,
  /// Its kind.
  pub kind: NodeKind,
  /// Its node identity.
  pub node_id: NodeId,
  /// Its copy identity.
  pub copy_id: CopyId,
  /// Its property list.
  pub properties: PropertyList,
}

impl Trial<'_> {
  /// The revision the transaction's new identities are numbered in: one past the youngest, which no node-revision of
  /// the index was created in.
  pub fn revision(&self) -> Revision {
    self.builder.revision
  }

  /// The revision the transaction was made on.
  pub fn base(&self) -> Revision {
    self.builder.previous
  }

  /// The node-revisions the transaction made, in the order it made them: what FSFS's transaction directory holds
  /// (A-738 part 2).
  pub fn made(&self) -> Result<Vec<Node<'_>>, IndexRefusal> {
    let first = self.builder.index.rows.len();
    (0..self.builder.rows.len())
      .map(|made| {
        let id = first.checked_add(made).ok_or(IndexRefusal::Corrupt)?;
        self.node_revision(NodeRevisionId(id))
      })
      .collect()
  }

  /// Node-revision `id`, the index's or one the transaction made.
  pub fn node_revision(&self, id: NodeRevisionId) -> Result<Node<'_>, IndexRefusal> {
    let row = self.builder.row(id)?;
    let created_path = self.builder.path_name(row.created_path)?;
    let copy_from = match row.copy_from {
      Some((from, revision)) => Some((self.builder.path_name(from)?, revision)),
      None => None,
    };
    Ok(Node {
      id,
      row,
      created_path,
      copy_from,
    })
  }

  /// The transaction's node at `path`, or `None` when the work left the path absent.
  pub fn node(&self, path: &str) -> Result<Option<TrialNode>, IndexRefusal> {
    let path = checked_path(path)?;
    let Some(current) = self.builder.current(path)? else {
      return Ok(None);
    };
    let row = self.builder.row(current.node)?;
    Ok(Some(TrialNode {
      id: current.node,
      kind: row.kind,
      node_id: row.node_id,
      copy_id: row.copy_id,
      properties: row.properties,
    }))
  }
}

/// A node as the revision being built stands now.
#[derive(Clone, Copy, Debug)]
struct Current {
  /// Its node-revision.
  node: NodeRevisionId,
  /// Whether this revision made it.
  mutable: bool,
}

/// One directory on a row's path, or the path itself, as opened from the root (`tree.c` `open_path`).
#[derive(Clone, Copy, Debug)]
struct Element {
  /// Where its path ends in the row's path.
  end: usize,
  /// Its node-revision now.
  node: NodeRevisionId,
  /// Whether this revision made it.
  mutable: bool,
  /// How it takes its copy identity when made mutable, decided when it was opened.
  inherit: Inherit,
}

/// A row's path opened from the root.
struct Opened {
  /// The root and every directory down to the path's parent; empty for the root itself.
  ancestors: Vec<Element>,
  /// The path itself, when present.
  own: Option<Element>,
}

/// One revision being built against the index (the module documentation's rules).
struct Builder<'index> {
  /// The index as of the previous revision.
  index: &'index Index,
  /// The revision being built.
  revision: Revision,
  /// The revision it is built against.
  previous: Revision,
  /// The node-revisions made so far.
  rows: Vec<Row>,
  /// What has been done at each path so far.
  staged: BTreeMap<String, Staged>,
  /// The paths named so far that the index has not named, in number order.
  new_paths: Vec<Box<str>>,
  /// Their numbers.
  new_numbers: BTreeMap<Box<str>, usize>,
  /// The next node identity's number.
  next_node: u64,
  /// The next copy identity's number.
  next_copy: u64,
  /// The deletions made so far.
  deletions: usize,
  /// The children maps' nodes made so far, numbered after the index's (A-759).
  maps: Vec<children::MapNode>,
}

impl<'index> Builder<'index> {
  /// A revision begun with its new root (`transaction.c` `create_new_txn_noderev_from_rev`).
  fn new(index: &'index Index, revision: Revision) -> Result<Self, IndexRefusal> {
    Self::new_on(index, revision, index.youngest)
  }

  /// A transaction numbered `revision` begun on revision `previous`, its new root's predecessor that revision's root:
  /// the next revision when `previous` is the youngest, and a work's transaction on its base otherwise ([`Trial`]).
  fn new_on(
    index: &'index Index,
    revision: Revision,
    previous: Revision,
  ) -> Result<Self, IndexRefusal> {
    let mut builder = Self {
      index,
      revision,
      previous,
      rows: Vec::new(),
      staged: BTreeMap::new(),
      new_paths: Vec::new(),
      new_numbers: BTreeMap::new(),
      next_node: 0,
      next_copy: 0,
      deletions: 0,
      maps: Vec::new(),
    };
    let old_root = index
      .resolve(ROOT, previous)?
      .ok_or(IndexRefusal::Corrupt)?;
    let old = *builder.row(old_root)?;
    let root = Row {
      created_revision: revision,
      predecessor: Some(old_root),
      predecessor_count: next_count(old.predecessor_count)?,
      copy_from: None,
      ..old
    };
    let id = builder.push(root)?;
    builder.staged.entry(ROOT.to_owned()).or_default().node = Some(id);
    Ok(builder)
  }

  /// Applies one row, as FSFS applies the editor call it records.
  fn apply(&mut self, change: &ChangedPath) -> Result<(), IndexRefusal> {
    if change.path == ROOT && change.action != Action::Modify {
      return Err(IndexRefusal::RootChange);
    }
    let Opened { mut ancestors, own } = self.open(&change.path)?;
    match change.action {
      Action::Add => self.add(change, &mut ancestors, own),
      Action::Replace => self.replace(change, &mut ancestors, own),
      Action::Delete => self.delete(change, &mut ancestors, own),
      Action::Modify => self.modify(change, ancestors, own),
    }
  }

  /// The revision built: its rows, its new paths, and what it did at each path.
  fn finish(mut self) -> Result<Plan, IndexRefusal> {
    let staged = std::mem::take(&mut self.staged);
    let mut events = Vec::with_capacity(staged.len());
    for (path, done) in staged {
      events.push((self.intern(&path)?, done));
    }
    let map_bytes = self.maps.iter().map(children::MapNode::bytes).sum();
    Ok(Plan {
      rows: self.rows,
      paths: self.new_paths,
      events,
      deletions: self.deletions,
      maps: self.maps,
      map_bytes,
    })
  }

  /// `Add`: a new node or a copy at an absent path, its directories made mutable first.
  fn add(
    &mut self,
    change: &ChangedPath,
    ancestors: &mut [Element],
    own: Option<Element>,
  ) -> Result<(), IndexRefusal> {
    if own.is_some() {
      return Err(IndexRefusal::AlreadyExists {
        path: change.path.clone(),
      });
    }
    self.make_mutable(&change.path, ancestors)?;
    self.place(change, ancestors, None)
  }

  /// `Replace`: the present node deleted, then a new node or a copy placed.
  fn replace(
    &mut self,
    change: &ChangedPath,
    ancestors: &mut [Element],
    own: Option<Element>,
  ) -> Result<(), IndexRefusal> {
    let own = own.ok_or_else(|| self.absent(&change.path))?;
    self.make_mutable(&change.path, ancestors)?;
    let count = self.row(own.node)?.mergeinfo_count;
    self.lower(ancestors, count)?;
    self.place(change, ancestors, Some(own.node))
  }

  /// `Delete`: the present node and everything beneath it removed.
  fn delete(
    &mut self,
    change: &ChangedPath,
    ancestors: &mut [Element],
    own: Option<Element>,
  ) -> Result<(), IndexRefusal> {
    let own = own.ok_or_else(|| self.absent(&change.path))?;
    self.check_kind(change, own.node)?;
    self.make_mutable(&change.path, ancestors)?;
    let count = self.row(own.node)?.mergeinfo_count;
    self.lower(ancestors, count)?;
    let parent = ancestors.last().ok_or(IndexRefusal::Corrupt)?.node;
    self.unset_child(parent, &change.path)?;
    let staged = self.staged.entry(change.path.clone()).or_default();
    staged.deleted = Some(own.node);
    staged.shape = Some(Shape::Deleted);
    self.deletions = self.deletions.checked_add(1).ok_or(IndexRefusal::Corrupt)?;
    Ok(())
  }

  /// `Modify`: the present node made mutable, then its text or property list set.
  fn modify(
    &mut self,
    change: &ChangedPath,
    mut chain: Vec<Element>,
    own: Option<Element>,
  ) -> Result<(), IndexRefusal> {
    let own = own.ok_or_else(|| self.absent(&change.path))?;
    self.check_kind(change, own.node)?;
    chain.push(own);
    self.make_mutable(&change.path, &mut chain)?;
    let node = chain.last().ok_or(IndexRefusal::Corrupt)?.node;
    let (had, has) = self.set_content(node, change)?;
    check_mergeinfo_flag(change, had, has)?;
    match (had, has) {
      (false, true) => self.raise(&chain, ONE_NODE),
      (true, false) => self.lower(&chain, ONE_NODE),
      _ => Ok(()),
    }
  }

  /// Places the row's new node or copy under its parent, the last of `ancestors`, now mutable.
  fn place(
    &mut self,
    change: &ChangedPath,
    ancestors: &[Element],
    replaced: Option<NodeRevisionId>,
  ) -> Result<(), IndexRefusal> {
    let parent = *self.row(ancestors.last().ok_or(IndexRefusal::Corrupt)?.node)?;
    let id = self.next_id()?;
    let (row, shape) = match &change.copy_from {
      None => (self.fresh_row(change, &parent)?, Shape::Fresh),
      Some(source) => self.copied_row(change, source, id)?,
    };
    self.raise(ancestors, row.mergeinfo_count)?;
    // The row takes `id`: nothing was pushed since it was read.
    self.rows.push(row);
    let parent_id = ancestors.last().ok_or(IndexRefusal::Corrupt)?.node;
    self.set_child(parent_id, &change.path, id)?;
    let staged = self.staged.entry(change.path.clone()).or_default();
    staged.node = Some(id);
    staged.deleted = replaced;
    staged.shape = Some(shape);
    Ok(())
  }

  /// A new node without history: its parent's copy identity and copy root (`dag.c` `make_entry`).
  fn fresh_row(&mut self, change: &ChangedPath, parent: &Row) -> Result<Row, IndexRefusal> {
    let properties = change.properties.unwrap_or(PropertyList::Empty);
    check_mergeinfo_flag(change, false, properties.has_mergeinfo())?;
    Ok(Row {
      kind: change.kind,
      node_id: self.fresh_node()?,
      copy_id: parent.copy_id,
      created_path: self.intern(&change.path)?,
      created_revision: self.revision,
      predecessor: None,
      predecessor_count: 0,
      copy_from: None,
      copy_root: parent.copy_root,
      text: change.text,
      properties,
      mergeinfo_count: u64::from(properties.has_mergeinfo()),
      children: children::EMPTY,
    })
  }

  /// A copy: its source's node identity, a fresh copy identity, itself (`id`) as its copy root, its source as its
  /// predecessor (`dag.c` `svn_fs_fs__dag_copy`), and the row's own text and property list over its source's.
  fn copied_row(
    &mut self,
    change: &ChangedPath,
    source: &CopySource,
    id: NodeRevisionId,
  ) -> Result<(Row, Shape), IndexRefusal> {
    let (source_id, source_row) = self.copy_source(change, source)?;
    let properties = change.properties.unwrap_or(source_row.properties);
    let had = source_row.properties.has_mergeinfo();
    check_mergeinfo_flag(change, had, properties.has_mergeinfo())?;
    let from = self.intern(&source.path)?;
    let row = Row {
      kind: source_row.kind,
      node_id: source_row.node_id,
      copy_id: self.fresh_copy()?,
      created_path: self.intern(&change.path)?,
      created_revision: self.revision,
      predecessor: Some(source_id),
      predecessor_count: next_count(source_row.predecessor_count)?,
      copy_from: Some((from, source.revision)),
      copy_root: id,
      text: change.text.or(source_row.text),
      properties,
      mergeinfo_count: recounted(source_row.mergeinfo_count, had, properties.has_mergeinfo())?,
      // A copy's children are its source's, the very node-revisions resolution finds beneath it (A-759).
      children: source_row.children,
    };
    Ok((
      row,
      Shape::Copied {
        from,
        revision: source.revision,
      },
    ))
  }

  /// A copy's source: in a revision before this one, present there, and of the row's kind; the very node-revision the
  /// copy replaces among them, since the replacement's deletion comes first (A-652).
  fn copy_source(
    &self,
    change: &ChangedPath,
    source: &CopySource,
  ) -> Result<(NodeRevisionId, Row), IndexRefusal> {
    // A copy reads any committed revision (`svn_fs_copy` takes any revision root): past the youngest is none. A record's
    // base is the youngest, so a record's source is at most the previous revision; a work's on an older base may be
    // newer than its base ([`Index::trial`]).
    if source.revision > self.index.youngest {
      return Err(IndexRefusal::NoSuchRevision {
        revision: source.revision,
      });
    }
    let source_id = self
      .index
      .resolve(&source.path, source.revision)?
      .ok_or_else(|| IndexRefusal::NotFound {
        path: source.path.clone(),
        revision: source.revision,
      })?;
    let source_row = *self.row(source_id)?;
    if source_row.kind != change.kind {
      return Err(IndexRefusal::KindMismatch {
        path: change.path.clone(),
      });
    }
    Ok((source_id, source_row))
  }

  /// Sets a mutable node's text and property list from the row; answers whether it held `svn:mergeinfo` before and
  /// after.
  fn set_content(
    &mut self,
    node: NodeRevisionId,
    change: &ChangedPath,
  ) -> Result<(bool, bool), IndexRefusal> {
    let row = self.row_mut(node)?;
    let had = row.properties.has_mergeinfo();
    if let Some(text) = change.text {
      row.text = Some(text);
    }
    if let Some(properties) = change.properties {
      row.properties = properties;
    }
    Ok((had, row.properties.has_mergeinfo()))
  }

  /// Refuses a row whose kind differs from the node it names.
  fn check_kind(&self, change: &ChangedPath, node: NodeRevisionId) -> Result<(), IndexRefusal> {
    if self.row(node)?.kind == change.kind {
      Ok(())
    } else {
      Err(IndexRefusal::KindMismatch {
        path: change.path.clone(),
      })
    }
  }

  /// The refusal for a row naming a path this revision leaves absent.
  fn absent(&self, path: &str) -> IndexRefusal {
    IndexRefusal::NotFound {
      path: path.to_owned(),
      revision: self.revision,
    }
  }

  /// Opens `path` from the root: each directory present and a directory, each one's inheritance decided against the
  /// one above it as that one stands now (`tree.c` `open_path`, `get_copy_inheritance`).
  fn open(&self, path: &str) -> Result<Opened, IndexRefusal> {
    let root_node = self
      .staged
      .get(ROOT)
      .and_then(|staged| staged.node)
      .ok_or(IndexRefusal::Corrupt)?;
    let root = Element {
      end: ROOT.len(),
      node: root_node,
      mutable: true,
      inherit: Inherit::Itself,
    };
    if path == ROOT {
      return Ok(Opened {
        ancestors: Vec::new(),
        own: Some(root),
      });
    }
    let mut ancestors = vec![root];
    for (directory, end) in inner_ancestors(path) {
      let element = self.open_directory(directory, end, &ancestors)?;
      ancestors.push(element);
    }
    let own = self.open_element(path, path.len(), &ancestors)?;
    Ok(Opened { ancestors, own })
  }

  /// Opens a directory of a row's path: refused when absent or a file.
  fn open_directory(
    &self,
    directory: &str,
    end: usize,
    above: &[Element],
  ) -> Result<Element, IndexRefusal> {
    let element = self
      .open_element(directory, end, above)?
      .ok_or_else(|| self.absent(directory))?;
    if self.row(element.node)?.kind == NodeKind::Directory {
      Ok(element)
    } else {
      Err(IndexRefusal::NotDirectory {
        path: directory.to_owned(),
      })
    }
  }

  /// Opens the node at `path` beneath the last of `above`, if present.
  fn open_element(
    &self,
    path: &str,
    end: usize,
    above: &[Element],
  ) -> Result<Option<Element>, IndexRefusal> {
    let Some(current) = self.current(path)? else {
      return Ok(None);
    };
    let parent = above.last().ok_or(IndexRefusal::Corrupt)?.node;
    let inherit = self.inheritance(current, parent, path)?;
    Ok(Some(Element {
      end,
      node: current.node,
      mutable: current.mutable,
      inherit,
    }))
  }

  /// How `child`, at `path`, takes its copy identity when made mutable (`tree.c` `get_copy_inheritance`).
  fn inheritance(
    &self,
    child: Current,
    parent: NodeRevisionId,
    path: &str,
  ) -> Result<Inherit, IndexRefusal> {
    if child.mutable {
      return Ok(Inherit::Itself);
    }
    let child_row = self.row(child.node)?;
    if child_row.copy_id == ROOT_COPY || child_row.copy_id == self.row(parent)?.copy_id {
      return Ok(Inherit::Parent);
    }
    if self.row(child_row.copy_root)?.node_id != child_row.node_id {
      return Ok(Inherit::Parent);
    }
    if self.path_name(child_row.created_path)? == path {
      Ok(Inherit::Itself)
    } else {
      Ok(Inherit::Fresh)
    }
  }

  /// Makes every element of `chain` mutable from the root down, each a successor of its old node-revision
  /// (`tree.c` `make_path_mutable`).
  fn make_mutable(&mut self, path: &str, chain: &mut [Element]) -> Result<(), IndexRefusal> {
    let mut parent: Option<NodeRevisionId> = None;
    for element in chain.iter_mut() {
      if !element.mutable {
        let above = parent.ok_or(IndexRefusal::Corrupt)?;
        let place = path.get(..element.end).ok_or(IndexRefusal::Corrupt)?;
        element.node = self.clone_child(element.node, above, element.inherit, place)?;
        element.mutable = true;
      }
      parent = Some(element.node);
    }
    Ok(())
  }

  /// The successor of `old` at `path` under the mutable `parent` (`dag.c` `svn_fs_fs__dag_clone_child`): its copy
  /// identity by `inherit`, its copy root its parent's unless it is a node-revision of its own copy root's node.
  fn clone_child(
    &mut self,
    old: NodeRevisionId,
    parent: NodeRevisionId,
    inherit: Inherit,
    path: &str,
  ) -> Result<NodeRevisionId, IndexRefusal> {
    let old_row = *self.row(old)?;
    let parent_row = *self.row(parent)?;
    let copy_id = match inherit {
      Inherit::Itself => old_row.copy_id,
      Inherit::Parent => parent_row.copy_id,
      Inherit::Fresh => self.fresh_copy()?,
    };
    let copy_root = if self.row(old_row.copy_root)?.node_id == old_row.node_id {
      old_row.copy_root
    } else {
      parent_row.copy_root
    };
    let row = Row {
      copy_id,
      copy_root,
      created_path: self.intern(path)?,
      created_revision: self.revision,
      predecessor: Some(old),
      predecessor_count: next_count(old_row.predecessor_count)?,
      copy_from: None,
      ..old_row
    };
    let id = self.push(row)?;
    self.staged.entry(path.to_owned()).or_default().node = Some(id);
    self.set_child(parent, path, id)?;
    Ok(id)
  }

  /// The mutable directory `parent`'s map with `path`'s name set to `id` (A-759).
  fn set_child(
    &mut self,
    parent: NodeRevisionId,
    path: &str,
    id: NodeRevisionId,
  ) -> Result<(), IndexRefusal> {
    let key = self.intern(path)?;
    let root = self.row(parent)?.children;
    let made = children::set(self, root, key, id).ok_or(IndexRefusal::Corrupt)?;
    self.row_mut(parent)?.children = made;
    Ok(())
  }

  /// The mutable directory `parent`'s map with `path`'s name removed (A-759).
  fn unset_child(&mut self, parent: NodeRevisionId, path: &str) -> Result<(), IndexRefusal> {
    let name = path.rsplit(SEPARATOR).next().ok_or(IndexRefusal::Corrupt)?;
    let root = self.row(parent)?.children;
    let made = children::remove(self, root, name).ok_or(IndexRefusal::Corrupt)?;
    self.row_mut(parent)?.children = made;
    Ok(())
  }

  /// The node at `path` in the revision as built so far: made or deleted by this revision, beneath a shape change this
  /// revision made above it, or as the previous revision left it.
  fn current(&self, path: &str) -> Result<Option<Current>, IndexRefusal> {
    if let Some(staged) = self.staged.get(path) {
      if let Some(node) = staged.node {
        return Ok(Some(Current {
          node,
          mutable: true,
        }));
      }
      if staged.deleted.is_some() {
        return Ok(None);
      }
    }
    let shaped = inner_ancestors(path).rev().find_map(|(ancestor, end)| {
      self
        .staged
        .get(ancestor)
        .and_then(|staged| staged.shape)
        .map(|shape| (shape, end))
    });
    let found = match shaped {
      Some((Shape::Copied { from, revision }, end)) => {
        let source = beneath(self.path_name(from)?, path, end)?;
        self.index.resolve(&source, revision)?
      }
      Some((Shape::Fresh | Shape::Deleted, _)) => None,
      None => self.index.resolve(path, self.previous)?,
    };
    Ok(found.map(|node| Current {
      node,
      mutable: false,
    }))
  }

  /// Adds `count` to the mergeinfo count of every element of `chain`, each mutable.
  fn raise(&mut self, chain: &[Element], count: u64) -> Result<(), IndexRefusal> {
    for element in chain {
      let row = self.row_mut(element.node)?;
      row.mergeinfo_count = row
        .mergeinfo_count
        .checked_add(count)
        .ok_or(IndexRefusal::Corrupt)?;
    }
    Ok(())
  }

  /// Takes `count` from the mergeinfo count of every element of `chain`, each mutable.
  fn lower(&mut self, chain: &[Element], count: u64) -> Result<(), IndexRefusal> {
    for element in chain {
      let row = self.row_mut(element.node)?;
      row.mergeinfo_count = row
        .mergeinfo_count
        .checked_sub(count)
        .ok_or(IndexRefusal::Corrupt)?;
    }
    Ok(())
  }

  /// The row of `id`, the index's or this revision's.
  fn row(&self, id: NodeRevisionId) -> Result<&Row, IndexRefusal> {
    match id.0.checked_sub(self.index.rows.len()) {
      None => self.index.rows.get(id.0),
      Some(made) => self.rows.get(made),
    }
    .ok_or(IndexRefusal::Corrupt)
  }

  /// The row of `id`, which this revision made.
  fn row_mut(&mut self, id: NodeRevisionId) -> Result<&mut Row, IndexRefusal> {
    let made = id
      .0
      .checked_sub(self.index.rows.len())
      .ok_or(IndexRefusal::Corrupt)?;
    self.rows.get_mut(made).ok_or(IndexRefusal::Corrupt)
  }

  /// The id the next row pushed takes.
  fn next_id(&self) -> Result<NodeRevisionId, IndexRefusal> {
    self
      .index
      .rows
      .len()
      .checked_add(self.rows.len())
      .map(NodeRevisionId)
      .ok_or(IndexRefusal::Corrupt)
  }

  /// Pushes a row and answers its id.
  fn push(&mut self, row: Row) -> Result<NodeRevisionId, IndexRefusal> {
    let id = self.next_id()?;
    self.rows.push(row);
    Ok(id)
  }

  /// A node identity no node-revision has had.
  fn fresh_node(&mut self) -> Result<NodeId, IndexRefusal> {
    let number = self.next_node;
    self.next_node = number.checked_add(1).ok_or(IndexRefusal::Corrupt)?;
    Ok(NodeId {
      revision: self.revision,
      number,
    })
  }

  /// A copy identity no node-revision has had.
  fn fresh_copy(&mut self) -> Result<CopyId, IndexRefusal> {
    let number = self.next_copy;
    self.next_copy = number.checked_add(1).ok_or(IndexRefusal::Corrupt)?;
    Ok(CopyId {
      revision: self.revision,
      number,
    })
  }

  /// The number of `path`, the index's or a new one.
  fn intern(&mut self, path: &str) -> Result<usize, IndexRefusal> {
    if let Some(&number) = self
      .index
      .numbers
      .get(path)
      .or_else(|| self.new_numbers.get(path))
    {
      return Ok(number);
    }
    let number = self
      .index
      .paths
      .len()
      .checked_add(self.new_paths.len())
      .ok_or(IndexRefusal::Corrupt)?;
    self.new_paths.push(path.into());
    self.new_numbers.insert(path.into(), number);
    Ok(number)
  }

  /// The path numbered `number`, the index's or a new one.
  fn path_name(&self, number: usize) -> Result<&str, IndexRefusal> {
    match number.checked_sub(self.index.paths.len()) {
      None => self.index.path_name(number),
      Some(new) => self
        .new_paths
        .get(new)
        .map(|path| &**path)
        .ok_or(IndexRefusal::Corrupt),
    }
  }
}

/// The room units `bytes` of directory maps take: [`Index::ENTRY_BYTES`] bytes a unit, rounded up (A-759).
fn map_units(bytes: usize) -> usize {
  bytes.div_ceil(Index::ENTRY_BYTES)
}

impl children::MapRead for Index {
  fn map_node(&self, handle: u32) -> Option<&children::MapNode> {
    self.maps.get(usize::try_from(handle).ok()?)
  }

  fn key_path(&self, key: usize) -> Option<&str> {
    self.paths.get(key).map(|entry| &*entry.path)
  }
}

impl children::MapRead for Builder<'_> {
  fn map_node(&self, handle: u32) -> Option<&children::MapNode> {
    let at = usize::try_from(handle).ok()?;
    match at.checked_sub(self.index.maps.len()) {
      None => self.index.maps.get(at),
      Some(made) => self.maps.get(made),
    }
  }

  fn key_path(&self, key: usize) -> Option<&str> {
    self.path_name(key).ok()
  }
}

impl children::MapWrite for Builder<'_> {
  fn push_map_node(&mut self, node: children::MapNode) -> Option<u32> {
    let handle = u32::try_from(self.index.maps.len().checked_add(self.maps.len())?).ok()?;
    if handle == children::EMPTY {
      return None;
    }
    self.maps.push(node);
    Some(handle)
  }

  fn staged(&self, handle: u32) -> bool {
    usize::try_from(handle)
      .ok()
      .and_then(|at| at.checked_sub(self.index.maps.len()))
      .is_some_and(|made| made < self.maps.len())
  }

  fn map_node_mut(&mut self, handle: u32) -> Option<&mut children::MapNode> {
    let made = usize::try_from(handle)
      .ok()?
      .checked_sub(self.index.maps.len())?;
    self.maps.get_mut(made)
  }
}

mod children;

#[cfg(test)]
pub(crate) mod tests;
