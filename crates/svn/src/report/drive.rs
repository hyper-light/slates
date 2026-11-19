//! The editor drive that answers a finished report (§4.17.4 "4. Reads", the reporter, piece 7's
//! third part; A-616, its second part): the calls of Subversion's delta editor (`svn_delta.h`'s
//! `svn_delta_editor_t`) in the order `reporter.c` makes them, one value a call, and [`Drive`],
//! which makes them.
//!
//! One drive feeds every protocol that answers a report: ra_svn writes each call as `editorp.c`'s
//! command ([`super::editor`]), and HTTP writes it as `mod_dav_svn`'s `reports/update.c` does
//! (A-627's `dav::Drive`). So a call carries the editor's own arguments and the raw facts of the
//! node it names, never one protocol's words.
//!
//! The order a drive keeps, `reporter.c`'s (`drive`, `delta_dirs`, `update_entry`,
//! `delta_proplists`, `delta_files`; Subversion's `1.14.x` at `d434dc14`, 1.15.0 alike):
//! - [`Call::TargetRevision`], then [`Call::OpenRoot`];
//! - inside an open directory: [`Call::Delete`] and [`Call::Absent`] of its entries, and
//!   [`Call::Open`] or [`Call::Add`] of one, which opens it until its close;
//! - a node's properties ([`Call::ChangeProperty`]) right after its open or add, before anything
//!   inside it: the entry properties first, then the lock token's removal, then its own;
//! - a file's text last before its close: [`Call::ApplyTextDelta`], each [`Call::Window`] in order,
//!   then [`Call::TextDeltaEnd`];
//! - [`Call::CloseFile`] and [`Call::CloseDirectory`], innermost first, then [`Call::CloseEdit`];
//!   or [`Call::AbortEdit`] wherever the drive meets a refusal.
//!
//! So the nodes open at any moment are a stack, and a property change applies to the innermost one
//! still open.
//!
//! **The drive.** [`Drive`] walks the report and the two revisions as `reporter.c` does, over the
//! index (A-553) for every node's identity, kind, created revision, text and property list, the
//! green ([`Repository`]) for directories' entries, property lists, revision properties and texts,
//! the session's [`Guards`] for its locks, and the rules its principal reads by ([`Sources::rules`],
//! the session's one read rule, which its read commands ask too). Its walk is an explicit stack of
//! frames, never recursion, so the depth of a path is data, not stack: the
//! anchor, each directory being compared (`delta_dirs`), each file being sent (`delta_files`). The
//! report's paths are read as `reporter.c` reads its spill buffer: one ahead, each consumed by the
//! directory it falls beneath (`fetch_path_info`, `skip_path_info`, `any_path_info`).
//!
//! **Bounded work.** A step does one bounded thing: one directory's entries read and its properties
//! compared, one entry of a directory decided, one deletion found, one node's properties, one
//! window of a text. [`Drive::next`] takes the steps its caller allows and pauses when they are
//! spent, so a large unchanged directory never holds the shard.
//!
//! **Where it differs from `svnserve`**, each in what a client reads alike:
//! - a directory's deletions and a node's own properties come in name order, where `svnserve`'s
//!   come in APR's per-table seeded hash order (three recordings of one repository gave three);
//! - property lists and texts are compared by their content identity, where FSFS compares a
//!   representation's place unless asked strictly (`svn_fs_props_changed`): a property list changed
//!   and changed back is the same list here, as rep-sharing makes a text.
//!
//! **Texts.** A changed text's windows are Subversion's deltifier's against the client's base
//! (A-652's [`deltas`], `txdelta_next_window` and `xdelta.c`), a window of up to 102,400 bytes at a
//! time, the very windows a stock server sends, stored or computed; an added text's are new data.
//!
//! No I/O, no clock, no randomness, no panic.

#[cfg(test)]
pub(crate) mod green;
pub(crate) mod history;
#[cfg(test)]
mod tests;

use super::{Depth, PathInfo, Report, join_fspath, join_relpath};
use crate::authz::{PrincipalRules, may_read};
use crate::index::{
  ContentIdentity, Index, IndexRefusal, Node, NodeId, NodeKind, NodeRevisionId, PropertyList,
  Revision, Text,
};
use crate::reads::Repository;
use crate::svndiff::{Instruction, WINDOW_LIMIT, Window, deltas};
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::ops::Bound;

/// The facts of the node an [`Call::OpenRoot`], [`Call::Open`] or [`Call::Add`] names, as the
/// repository holds them in the drive's target revision: what a protocol derives its own words from
/// (`update.c`'s checked-in href from the path and the created revision, an added directory's
/// `bc-url` from the path and the target revision, an added file's `sha1-checksum`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts<'a> {
  /// The node's path in the target revision, as the repository names it (`/trunk/a.txt`).
  pub repository_path: &'a str,
  /// The revision the node at that path was created in (`svn_fs_node_created_rev`).
  pub created_revision: Revision,
  /// A file's SHA-1 (`svn_fs_file_checksum`); none for a directory.
  pub sha1: Option<[u8; 20]>,
}

/// One call of the editor drive, with the editor's own arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call<'a> {
  /// `set_target_revision`: the revision the edit brings the working copy to.
  TargetRevision(Revision),
  /// `open_root`: the edit's root, the report's anchor, at the revision the client reported.
  OpenRoot {
    /// The anchor's revision in the working copy; none when the editor's caller gives none.
    base: Option<Revision>,
    /// The anchor in the target revision.
    node: Facts<'a>,
  },
  /// `open_directory` or `open_file`: an entry of the innermost open directory, changed.
  Open {
    /// Which of the two.
    kind: NodeKind,
    /// The editor path from the edit's root (`dir/b.txt`), as ra_svn sends it.
    path: &'a str,
    /// The entry's revision in the working copy.
    base: Option<Revision>,
    /// The entry in the target revision.
    node: Facts<'a>,
  },
  /// `add_directory` or `add_file`: an entry of the innermost open directory, new to the working
  /// copy.
  Add {
    /// Which of the two.
    kind: NodeKind,
    /// The editor path from the edit's root.
    path: &'a str,
    /// The copy source the client may fetch the entry's base from, when the client asked for
    /// copy sources and the entry has one it may read.
    copy_from: Option<(&'a str, Revision)>,
    /// The entry in the target revision.
    node: Facts<'a>,
  },
  /// `delete_entry`: an entry of the innermost open directory, gone.
  Delete {
    /// The editor path from the edit's root.
    path: &'a str,
    /// The revision it was deleted in (`svn_repos_deleted_rev`), when the drive found one.
    revision: Option<Revision>,
  },
  /// `absent_directory` or `absent_file`: an entry of the innermost open directory the principal
  /// may not read.
  Absent {
    /// Which of the two.
    kind: NodeKind,
    /// The editor path from the edit's root.
    path: &'a str,
  },
  /// `change_dir_prop` or `change_file_prop` on the innermost open node.
  ChangeProperty {
    /// The property's name.
    name: &'a str,
    /// Its new value; none removes it.
    value: Option<&'a [u8]>,
  },
  /// `apply_textdelta` on the innermost open node, a file: its windows follow.
  ApplyTextDelta {
    /// The MD5 of the text the delta applies to; none for an added file's first text.
    base_md5: Option<[u8; 16]>,
  },
  /// One window of the open delta, in order. The consumer encodes it in the svndiff version it
  /// negotiated.
  Window(&'a Window),
  /// The end of the open delta: the window handler's last call, with no window.
  TextDeltaEnd,
  /// `close_file` on the innermost open node, a file.
  CloseFile {
    /// The MD5 of the file's text in the target revision.
    md5: [u8; 16],
  },
  /// `close_directory` on the innermost open node, a directory.
  CloseDirectory,
  /// `close_edit`: every node closed, the edit complete.
  CloseEdit,
  /// `abort_edit`: the drive met a refusal, which its caller answers after this.
  AbortEdit,
}

/// `svn:entry:committed-rev`, the revision a node was created in.
/// Format: `svn_props.h` `SVN_PROP_ENTRY_COMMITTED_REV`.
const COMMITTED_REV: &str = "svn:entry:committed-rev";

/// `svn:entry:committed-date`, that revision's date.
/// Format: `svn_props.h` `SVN_PROP_ENTRY_COMMITTED_DATE`.
const COMMITTED_DATE: &str = "svn:entry:committed-date";

/// `svn:entry:last-author`, that revision's author.
/// Format: `svn_props.h` `SVN_PROP_ENTRY_LAST_AUTHOR`.
const LAST_AUTHOR: &str = "svn:entry:last-author";

/// `svn:entry:uuid`, the repository's identity.
/// Format: `svn_props.h` `SVN_PROP_ENTRY_UUID`.
const ENTRY_UUID: &str = "svn:entry:uuid";

/// `svn:entry:lock-token`, removed from a file whose lock is gone.
/// Format: `svn_props.h` `SVN_PROP_ENTRY_LOCK_TOKEN`.
const LOCK_TOKEN: &str = "svn:entry:lock-token";

/// The revision property holding a revision's date.
/// Format: `svn_props.h` `SVN_PROP_REVISION_DATE`.
const REVISION_DATE: &[u8] = b"svn:date";

/// The revision property holding a revision's author.
/// Format: `svn_props.h` `SVN_PROP_REVISION_AUTHOR`.
const REVISION_AUTHOR: &[u8] = b"svn:author";

/// The MD5 of no bytes: the checksum of a file without a text.
/// Format: RFC 1321 A.5, `MD5 ("") = d41d8cd98f00b204e9800998ecf8427e`.
const EMPTY_MD5: [u8; 16] = *b"\xd4\x1d\x8c\xd9\x8f\x00\xb2\x04\xe9\x80\x09\x98\xec\xf8\x42\x7e";

/// `SVN_ERR_FS_CORRUPT`, for a green and an index that disagree.
/// Format: the filesystem category's start, 160,000, plus 4 (`svn_error_codes.h`).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;

/// `SVN_ERR_FS_PATH_SYNTAX`, for a target that names no directory to edit.
/// Format: the filesystem category's start plus 5 (`svn_error_codes.h`).
const SVN_ERR_FS_PATH_SYNTAX: u32 = 160_005;

/// `SVN_ERR_FS_NO_SUCH_REVISION`, for a revision past the youngest.
/// Format: the filesystem category's start plus 6 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;

/// `SVN_ERR_FS_NOT_FOUND`, for a reported path the repository does not hold.
/// Format: the filesystem category's start plus 13 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_FOUND: u32 = 160_013;

/// `SVN_ERR_AUTHZ_ROOT_UNREADABLE`, for an edit whose root the principal may not read.
/// Format: the authorization category's start, 220,000, plus 0 (`svn_error_codes.h`).
const SVN_ERR_AUTHZ_ROOT_UNREADABLE: u32 = 220_000;

/// What a drive asks of the session beyond the repository and its read rules: the locks held now
/// (§4.17.4 item 10).
pub trait Guards {
  /// The token of the lock held on `path` now (`svn_fs_get_lock`); none when it is not locked.
  fn lock_token(&self, path: &str) -> Option<&[u8]>;
}

/// A refusal the drive met, answered after its [`Call::AbortEdit`]: Subversion's error number and
/// its message as the C code formats it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveFailure {
  /// The error number.
  pub code: u32,
  /// The message.
  pub message: String,
}

impl DriveFailure {
  /// A green and an index that disagree.
  fn corrupt() -> Self {
    Self {
      code: SVN_ERR_FS_CORRUPT,
      message: String::from("Filesystem is corrupt"),
    }
  }

  /// The refusal of an index refusal, in FSFS's words where FSFS has them.
  fn of_index(refusal: &IndexRefusal) -> Self {
    match refusal {
      IndexRefusal::NoSuchRevision { revision } => Self {
        code: SVN_ERR_FS_NO_SUCH_REVISION,
        message: format!("No such revision {revision}"),
      },
      IndexRefusal::NotFound { path, revision } => Self {
        code: SVN_ERR_FS_NOT_FOUND,
        message: format!("File not found: revision {revision}, path '{path}'"),
      },
      _ => Self::corrupt(),
    }
  }
}

impl From<IndexRefusal> for DriveFailure {
  fn from(refusal: IndexRefusal) -> Self {
    Self::of_index(&refusal)
  }
}

/// What [`Drive::next`] made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next<'a> {
  /// The next call.
  Call(Call<'a>),
  /// The steps given were spent before a call was ready: call again.
  Paused,
  /// The drive has ended: closed, or aborted with [`Drive::failure`].
  Ended,
}

/// The facts of a node, owned while its call waits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedFacts {
  /// The node's repository path.
  repository_path: String,
  /// Its created revision.
  created_revision: Revision,
  /// A file's SHA-1.
  sha1: Option<[u8; 20]>,
}

impl OwnedFacts {
  /// The facts lent to a call.
  fn lent(&self) -> Facts<'_> {
    Facts {
      repository_path: &self.repository_path,
      created_revision: self.created_revision,
      sha1: self.sha1,
    }
  }
}

/// A call made and not yet lent, holding what it names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Owned {
  /// [`Call::TargetRevision`].
  TargetRevision(Revision),
  /// [`Call::OpenRoot`].
  OpenRoot(Option<Revision>, OwnedFacts),
  /// [`Call::Open`].
  Open(NodeKind, String, Option<Revision>, OwnedFacts),
  /// [`Call::Add`].
  Add(NodeKind, String, Option<(String, Revision)>, OwnedFacts),
  /// [`Call::Delete`].
  Delete(String, Option<Revision>),
  /// [`Call::Absent`].
  Absent(NodeKind, String),
  /// [`Call::ChangeProperty`].
  ChangeProperty(String, Option<Vec<u8>>),
  /// [`Call::ApplyTextDelta`].
  ApplyTextDelta(Option<[u8; 16]>),
  /// [`Call::Window`].
  Window(Window),
  /// [`Call::TextDeltaEnd`].
  TextDeltaEnd,
  /// [`Call::CloseFile`].
  CloseFile([u8; 16]),
  /// [`Call::CloseDirectory`].
  CloseDirectory,
  /// [`Call::CloseEdit`].
  CloseEdit,
  /// [`Call::AbortEdit`].
  AbortEdit,
}

impl Owned {
  /// The call, lent.
  fn lent(&self) -> Call<'_> {
    match self {
      Self::TargetRevision(revision) => Call::TargetRevision(*revision),
      Self::OpenRoot(base, node) => Call::OpenRoot {
        base: *base,
        node: node.lent(),
      },
      Self::Open(kind, path, base, node) => Call::Open {
        kind: *kind,
        path,
        base: *base,
        node: node.lent(),
      },
      Self::Add(kind, path, copy_from, node) => Call::Add {
        kind: *kind,
        path,
        copy_from: copy_from
          .as_ref()
          .map(|(source, revision)| (source.as_str(), *revision)),
        node: node.lent(),
      },
      Self::Delete(path, revision) => Call::Delete {
        path,
        revision: *revision,
      },
      Self::Absent(kind, path) => Call::Absent { kind: *kind, path },
      Self::ChangeProperty(name, value) => Call::ChangeProperty {
        name,
        value: value.as_deref(),
      },
      Self::ApplyTextDelta(base_md5) => Call::ApplyTextDelta {
        base_md5: *base_md5,
      },
      Self::Window(window) => Call::Window(window),
      Self::TextDeltaEnd => Call::TextDeltaEnd,
      Self::CloseFile(md5) => Call::CloseFile { md5: *md5 },
      Self::CloseDirectory => Call::CloseDirectory,
      Self::CloseEdit => Call::CloseEdit,
      Self::AbortEdit => Call::AbortEdit,
    }
  }
}

/// A node as a directory's entry names it (`svn_fs_dirent_t`), with the facts the drive compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dirent {
  /// Its kind.
  kind: NodeKind,
  /// Its node-revision.
  id: NodeRevisionId,
  /// Its line of history.
  line: NodeId,
  /// Its property list.
  properties: PropertyList,
  /// Its text.
  text: Option<Text>,
  /// The revision it was created in.
  created: Revision,
}

impl Dirent {
  /// The entry of a node.
  fn of(node: &Node<'_>) -> Self {
    Self {
      kind: node.kind(),
      id: node.id(),
      line: node.node_id(),
      properties: node.properties(),
      text: node.text(),
      created: node.created_revision(),
    }
  }

  /// The facts a call carries of this node at `path`.
  fn facts(&self, path: &str) -> OwnedFacts {
    OwnedFacts {
      repository_path: path.to_owned(),
      created_revision: self.created,
      sha1: self.text.map(|text| text.sha1),
    }
  }

  /// The MD5 of its text.
  fn md5(&self) -> [u8; 16] {
    self.text.map_or(EMPTY_MD5, |text| text.md5)
  }

  /// Its text's content identity, none for no bytes.
  fn content(&self) -> Option<ContentIdentity> {
    self
      .text
      .filter(|text| text.size > 0)
      .map(|text| text.identity)
  }
}

/// How two entries of one kind are related (`svn_fs_compare_ids`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Distance {
  /// The same node-revision: 0.
  Same,
  /// One line of history: 1.
  Related,
  /// Different lines: -1.
  Unrelated,
}

impl Distance {
  /// The distance from `source` to `target`.
  fn between(source: &Dirent, target: &Dirent) -> Self {
    if source.id == target.id {
      Self::Same
    } else if source.line == target.line {
      Self::Related
    } else {
      Self::Unrelated
    }
  }
}

/// One entry to update, `update_entry`'s parameters.
#[derive(Clone, Debug)]
struct EntryUpdate {
  /// The revision the source is read at.
  s_rev: Revision,
  /// The source's repository path, none when the working copy holds nothing there.
  s_path: Option<String>,
  /// The source's node.
  s_entry: Option<Dirent>,
  /// The target's repository path.
  t_path: String,
  /// The target's node, none when the target revision holds nothing there.
  t_entry: Option<Dirent>,
  /// The editor path from the edit's root.
  e_path: String,
  /// The report's path for the entry, if the client reported it.
  info: Option<PathInfo>,
  /// The entry's depth in the working copy.
  wc_depth: Depth,
  /// The depth asked below the entry's directory.
  requested_depth: Depth,
}

/// Where a directory's comparison stands (`delta_dirs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryPhase {
  /// Its properties are next.
  Properties,
  /// The report's paths beneath it are next.
  Report,
  /// Its source entries gone from the target are next.
  Deletions,
  /// Its target entries the report did not name are next.
  Targets,
  /// Its close is next.
  Close,
}

/// A directory being compared.
#[derive(Clone, Debug)]
struct DirectoryFrame {
  /// The revision its source is read at.
  s_rev: Revision,
  /// Its source path.
  s_path: Option<String>,
  /// Its target path.
  t_path: String,
  /// Its editor path.
  e_path: String,
  /// Whether the working copy holds none of its entries.
  start_empty: bool,
  /// Its depth in the working copy.
  wc_depth: Depth,
  /// The depth asked of it.
  requested_depth: Depth,
  /// Where it stands.
  phase: DirectoryPhase,
  /// Its source entries, by name.
  s_entries: BTreeMap<String, Dirent>,
  /// Its target entries not yet updated, by name.
  t_entries: BTreeMap<String, Dirent>,
  /// The last source entry the deletions looked at.
  deleted_after: Option<String>,
}

impl DirectoryFrame {
  /// A directory about to be compared from its properties.
  fn new(entry: &EntryUpdate, start_empty: bool) -> Self {
    Self {
      s_rev: entry.s_rev,
      s_path: entry.s_path.clone(),
      t_path: entry.t_path.clone(),
      e_path: entry.e_path.clone(),
      start_empty,
      wc_depth: entry.wc_depth,
      requested_depth: entry.requested_depth,
      phase: DirectoryPhase::Properties,
      s_entries: BTreeMap::new(),
      t_entries: BTreeMap::new(),
      deleted_after: None,
    }
  }
}

/// Where a file's sending stands (`delta_files`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilePhase {
  /// Its properties are next.
  Properties,
  /// Whether its text is sent is next.
  Text,
  /// Its next window is next.
  Windows,
  /// The end of its delta is next.
  End,
  /// Its close is next.
  Close,
}

/// A file being sent.
#[derive(Clone, Debug)]
struct FileFrame {
  /// The revision its source is read at.
  s_rev: Revision,
  /// Its source path: what the working copy holds, or its copy source.
  s_path: Option<String>,
  /// Its target path.
  t_path: String,
  /// Its target node.
  target: Dirent,
  /// The lock token the working copy holds for it.
  lock_token: Option<Vec<u8>>,
  /// Where it stands.
  phase: FilePhase,
  /// The text its delta applies to, once the delta is begun against one.
  source: Option<Text>,
  /// How many bytes of its text the windows have sent.
  sent: u64,
}

/// One frame of the walk.
#[derive(Clone, Debug)]
enum Frame {
  /// The edit's root when the report has an operand: its close follows the operand's update.
  Anchor,
  /// A directory being compared.
  Directory(Box<DirectoryFrame>),
  /// A file being sent.
  File(Box<FileFrame>),
}

/// What a drive reads: the index, the green, the session's guards and its read rules.
#[derive(Debug)]
pub struct Sources<'s, R: ?Sized, G: ?Sized> {
  /// The index (A-553).
  pub index: &'s Index,
  /// The green: entries, property lists, revision properties and texts.
  pub repository: &'s R,
  /// The session's locks.
  pub guards: &'s G,
  /// The rules the session's principal reads by (A-606), the ones its read commands ask, compiled once
  /// for the principal by the connection: [`may_read`] judges every path the drive reads by them
  /// (`authz_check_access_cb`). None where the repository names no authorization file, and then
  /// every path is readable.
  pub rules: Option<&'s PrincipalRules>,
}

/// A drive answering one finished report (module documentation).
#[derive(Clone, Debug)]
pub struct Drive {
  /// The repository path of the working copy's anchor.
  anchor: String,
  /// The anchor-relative target.
  operand: String,
  /// The repository path the edit brings the target to.
  target_path: String,
  /// The revision the edit brings the working copy to.
  target_revision: Revision,
  /// The revision the working copy's anchor is at.
  source_revision: Revision,
  /// The depth the command asked for.
  requested_depth: Depth,
  /// Whether file texts are sent.
  text_deltas: bool,
  /// Whether an added file names what it was copied from.
  send_copyfrom_args: bool,
  /// Whether two nodes of one kind are related whatever their history.
  ignore_ancestry: bool,
  /// Whether the edit brings the target to another path.
  is_switch: bool,
  /// The report of the target itself, until the drive starts.
  target_info: Option<PathInfo>,
  /// The report's other paths not yet consumed, the next first.
  lookahead: VecDeque<PathInfo>,
  /// The repository's UUID.
  uuid: Vec<u8>,
  /// The walk.
  frames: Vec<Frame>,
  /// The calls made and not yet lent.
  pending: VecDeque<Owned>,
  /// The call lent last.
  current: Option<Owned>,
  /// Whether the walk has begun.
  started: bool,
  /// Whether the walk has made its last call.
  ended: bool,
  /// The refusal the drive met.
  failure: Option<DriveFailure>,
}

impl Drive {
  /// A drive answering `report` for a repository whose UUID is `uuid`.
  pub fn new(report: Report, uuid: &[u8]) -> Self {
    Self {
      anchor: report.anchor,
      operand: report.operand,
      target_path: report.target_path,
      target_revision: report.target_revision,
      source_revision: report.source_revision,
      requested_depth: report.requested_depth,
      text_deltas: report.text_deltas,
      send_copyfrom_args: report.send_copyfrom_args,
      ignore_ancestry: report.ignore_ancestry,
      is_switch: report.is_switch,
      target_info: Some(report.target_info),
      lookahead: report.paths.into(),
      uuid: uuid.to_vec(),
      frames: Vec::new(),
      pending: VecDeque::new(),
      current: None,
      started: false,
      ended: false,
      failure: None,
    }
  }

  /// The next call, made within `steps` steps of the walk; [`Next::Paused`] when they are spent
  /// first, [`Next::Ended`] after the last call.
  pub fn next<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    sources: &Sources<'_, R, G>,
    steps: usize,
  ) -> Next<'_> {
    self.current = None;
    let mut left = steps;
    while self.pending.is_empty() && !self.ended {
      let Some(rest) = left.checked_sub(1) else {
        return Next::Paused;
      };
      left = rest;
      if let Err(failure) = self.step(sources) {
        self.fail(failure);
      }
    }
    self.current = self.pending.pop_front();
    self
      .current
      .as_ref()
      .map_or(Next::Ended, |owned| Next::Call(owned.lent()))
  }

  /// The refusal the drive met, once it has made its [`Call::AbortEdit`].
  pub fn failure(&self) -> Option<&DriveFailure> {
    self.failure.as_ref()
  }

  /// Ends the walk with a refusal: the calls already made, then the abort.
  fn fail(&mut self, failure: DriveFailure) {
    self.frames.clear();
    self.pending.push_back(Owned::AbortEdit);
    self.failure = Some(failure);
    self.ended = true;
  }

  /// One step of the walk.
  fn step<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    match self.frames.pop() {
      None if !self.started => self.start(sources),
      None => {
        self.pending.push_back(Owned::CloseEdit);
        self.ended = true;
        Ok(())
      }
      Some(Frame::Anchor) => {
        self.pending.push_back(Owned::CloseDirectory);
        Ok(())
      }
      Some(Frame::Directory(directory)) => self.step_directory(*directory, sources),
      Some(Frame::File(file)) => self.step_file(*file, sources),
    }
  }

  /// `drive`: the anchor checked, the target revision and the root, then the target compared.
  fn start<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    self.started = true;
    let info = self.target_info.take().ok_or_else(DriveFailure::corrupt)?;
    let t_anchor = if self.operand.is_empty() {
      self.target_path.clone()
    } else {
      dirname(&self.target_path).to_owned()
    };
    if !may_read(sources.rules, &t_anchor) {
      return Err(DriveFailure {
        code: SVN_ERR_AUTHZ_ROOT_UNREADABLE,
        message: String::from("Not authorized to open root of edit operation"),
      });
    }
    let s_fullpath = join_fspath(&self.anchor, &self.operand);
    let s_entry = dirent(sources, &s_fullpath, self.source_revision)?;
    let t_entry = dirent(sources, &self.target_path, self.target_revision)?;
    let set_path = info.revision.is_some() && info.link_path.is_none();
    let s_path = (!set_path || s_entry.is_some()).then_some(s_fullpath);
    self.check_operand(s_entry, t_entry)?;
    let anchor = dirent(sources, &t_anchor, self.target_revision)?;
    self.open_root(&t_anchor, anchor);
    let entry = EntryUpdate {
      s_rev: self.source_revision,
      s_path,
      s_entry,
      t_path: self.target_path.clone(),
      t_entry,
      e_path: self.operand.clone(),
      wc_depth: info.depth,
      info: Some(info),
      requested_depth: self.requested_depth,
    };
    self.enter_target(entry, sources)
  }

  /// `drive`'s refusals of a report without an operand: the target must be a directory in both
  /// revisions.
  fn check_operand(
    &self,
    s_entry: Option<Dirent>,
    t_entry: Option<Dirent>,
  ) -> Result<(), DriveFailure> {
    if !self.operand.is_empty() {
      return Ok(());
    }
    let Some(t_entry) = t_entry else {
      return Err(DriveFailure {
        code: SVN_ERR_FS_PATH_SYNTAX,
        message: format!("Target path '{}' does not exist", self.target_path),
      });
    };
    let directories = s_entry.is_some_and(|entry| entry.kind == NodeKind::Directory)
      && t_entry.kind == NodeKind::Directory;
    if directories {
      Ok(())
    } else {
      Err(DriveFailure {
        code: SVN_ERR_FS_PATH_SYNTAX,
        message: String::from("Cannot replace a directory from within"),
      })
    }
  }

  /// `set_target_revision` and `open_root`, with the anchor's facts in the target revision.
  fn open_root(&mut self, t_anchor: &str, anchor: Option<Dirent>) {
    let facts = anchor.map_or_else(
      || OwnedFacts {
        repository_path: t_anchor.to_owned(),
        created_revision: self.target_revision,
        sha1: None,
      },
      |anchor| anchor.facts(t_anchor),
    );
    self
      .pending
      .push_back(Owned::TargetRevision(self.target_revision));
    self
      .pending
      .push_back(Owned::OpenRoot(Some(self.source_revision), facts));
  }

  /// The target compared within the root: as a directory when the anchor is the target, else as
  /// one entry of the anchor, whose close follows it.
  fn enter_target<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    entry: EntryUpdate,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    if self.operand.is_empty() {
      let start_empty = entry.info.as_ref().is_some_and(|info| info.start_empty);
      self
        .frames
        .push(Frame::Directory(Box::new(DirectoryFrame::new(
          &entry,
          start_empty,
        ))));
      Ok(())
    } else {
      self.frames.push(Frame::Anchor);
      self.update_entry(entry, sources)
    }
  }

  /// One step of a directory's comparison.
  fn step_directory<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    directory: DirectoryFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    match directory.phase {
      DirectoryPhase::Properties => self.directory_properties(directory, sources),
      DirectoryPhase::Report => self.report_entry(directory, sources),
      DirectoryPhase::Deletions => self.deletion(directory, sources),
      DirectoryPhase::Targets => self.target_entry(directory, sources),
      DirectoryPhase::Close => {
        self.pending.push_back(Owned::CloseDirectory);
        Ok(())
      }
    }
  }

  /// `delta_dirs`' start: the properties compared, all of them when the directory starts empty,
  /// then both revisions' entries read when the depth asked reaches them.
  fn directory_properties<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut directory: DirectoryFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let source = (!directory.start_empty)
      .then_some(directory.s_path.as_deref())
      .flatten();
    self.properties(sources, directory.s_rev, source, &directory.t_path, None)?;
    let deeper = directory.requested_depth > Depth::Empty;
    if deeper || directory.requested_depth == Depth::Unknown {
      if let (Some(s_path), false) = (&directory.s_path, directory.start_empty) {
        directory.s_entries = entries(sources, s_path, directory.s_rev)?;
      }
      directory.t_entries = entries(sources, &directory.t_path, self.target_revision)?;
      directory.phase = DirectoryPhase::Report;
    } else {
      directory.phase = DirectoryPhase::Close;
    }
    self.frames.push(Frame::Directory(Box::new(directory)));
    Ok(())
  }

  /// `delta_dirs`' report loop, one path beneath the directory at a time: a reported deletion only
  /// noted, every other path updated, its name then done in both revisions' entries.
  fn report_entry<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut directory: DirectoryFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let Some((name, info)) = self.fetch_path_info(&directory.e_path) else {
      directory.phase = DirectoryPhase::Deletions;
      self.frames.push(Frame::Directory(Box::new(directory)));
      return Ok(());
    };
    let deleted = info
      .as_ref()
      .is_some_and(|info| info.revision.is_none() && info.depth != Depth::Exclude);
    if deleted {
      directory.s_entries.remove(&name);
      self.frames.push(Frame::Directory(Box::new(directory)));
      return Ok(());
    }
    let entry = reported_entry(&mut directory, name, info);
    self.frames.push(Frame::Directory(Box::new(directory)));
    match entry {
      Some(entry) => self.update_entry(entry, sources),
      None => Ok(()),
    }
  }

  /// `delta_dirs`' deletions, one source entry at a time in name order: each the target no longer
  /// holds, within the working copy's depth, deleted with the revision it was deleted in.
  fn deletion<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut directory: DirectoryFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let next = match &directory.deleted_after {
      None => directory.s_entries.iter().next(),
      Some(after) => directory
        .s_entries
        .range::<str, _>((Bound::Excluded(after.as_str()), Bound::Unbounded))
        .next(),
    }
    .map(|(name, entry)| (name.clone(), *entry));
    let Some((name, s_entry)) = next else {
      directory.phase = DirectoryPhase::Targets;
      self.frames.push(Frame::Directory(Box::new(directory)));
      return Ok(());
    };
    let doomed = !directory.t_entries.contains_key(&name)
      && within_depth(s_entry.kind, directory.wc_depth, directory.requested_depth);
    let deleted = doomed.then(|| {
      (
        join_relpath(&directory.e_path, &name),
        join_fspath(&directory.t_path, &name),
        directory.s_rev,
      )
    });
    directory.deleted_after = Some(name);
    self.frames.push(Frame::Directory(Box::new(directory)));
    if let Some((e_path, t_path, s_rev)) = deleted {
      let revision =
        history::deleted_revision(sources.index, &t_path, s_rev, self.target_revision)?;
      self.pending.push_back(Owned::Delete(e_path, revision));
    }
    Ok(())
  }

  /// `delta_dirs`' target loop, one entry the report did not name at a time in name order: each
  /// within the depths updated, its source forgotten when the working copy is being made deeper.
  fn target_entry<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut directory: DirectoryFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let Some((name, t_entry)) = directory.t_entries.pop_first() else {
      directory.phase = DirectoryPhase::Close;
      self.frames.push(Frame::Directory(Box::new(directory)));
      return Ok(());
    };
    let entry = target_update(&directory, &name, t_entry);
    self.frames.push(Frame::Directory(Box::new(directory)));
    match entry {
      Some(entry) => self.update_entry(entry, sources),
      None => Ok(()),
    }
  }

  /// `update_entry`: the source and target followed where the report sends them, the entry left
  /// alone when nothing changed, deleted when unrelated, then made absent, opened or added.
  fn update_entry<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut entry: EntryUpdate,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    self.follow_report(&mut entry, sources)?;
    if entry.s_path.is_some() && entry.s_entry.is_none() {
      return Err(DriveFailure {
        code: SVN_ERR_FS_NOT_FOUND,
        message: format!(
          "Working copy path '{}' does not exist in repository",
          entry.e_path
        ),
      });
    }
    let Some(related) = self.relate(&entry, sources) else {
      return Ok(());
    };
    if entry.s_entry.is_some() && !related {
      self.delete_unrelated(&entry, sources)?;
      entry.s_path = None;
    }
    let Some(t_entry) = entry.t_entry else {
      self.skip_path_info(&entry.e_path);
      return Ok(());
    };
    if !may_read(sources.rules, &entry.t_path) {
      self
        .pending
        .push_back(Owned::Absent(t_entry.kind, entry.e_path.clone()));
      self.skip_path_info(&entry.e_path);
      return Ok(());
    }
    match t_entry.kind {
      NodeKind::Directory => {
        self.enter_directory(&entry, t_entry, related);
        Ok(())
      }
      NodeKind::File => self.enter_file(entry, t_entry, related, sources),
    }
  }

  /// `update_entry`'s following of the report: a link's target outside a switch, and the reported
  /// revision and link for the source.
  fn follow_report<R: Repository + ?Sized, G: Guards + ?Sized>(
    &self,
    entry: &mut EntryUpdate,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let Some(info) = entry.info.as_ref() else {
      return Ok(());
    };
    if let (Some(link), false) = (&info.link_path, self.is_switch) {
      entry.t_path = link.clone();
      entry.t_entry = dirent(sources, link, self.target_revision)?;
    }
    match (info.revision, entry.s_path.is_some()) {
      (None, _) => {
        entry.s_path = None;
        entry.s_entry = None;
      }
      (Some(revision), true) => {
        if let Some(link) = &info.link_path {
          entry.s_path = Some(link.clone());
        }
        entry.s_rev = revision;
        let s_path = entry.s_path.as_deref().unwrap_or_default();
        entry.s_entry = dirent(sources, s_path, revision)?;
      }
      (Some(_), false) => {}
    }
    Ok(())
  }

  /// `update_entry`'s relation of source and target: none when the entry needs nothing, else
  /// whether the two are related.
  fn relate<R: Repository + ?Sized, G: Guards + ?Sized>(
    &self,
    entry: &EntryUpdate,
    sources: &Sources<'_, R, G>,
  ) -> Option<bool> {
    let (Some(source), Some(target)) = (entry.s_entry, entry.t_entry) else {
      return Some(false);
    };
    if source.kind != target.kind {
      return Some(false);
    }
    let distance = Distance::between(&source, &target);
    let changed = distance != Distance::Related
      || self.ignore_ancestry
      || target.kind != NodeKind::File
      || source.properties != target.properties
      || source.content() != target.content();
    let quiet = (distance == Distance::Same || !changed)
      && !self.any_path_info(&entry.e_path)
      && (entry.requested_depth <= entry.wc_depth || target.kind == NodeKind::File);
    if quiet && self.left_alone(entry, sources) {
      return None;
    }
    Some(distance != Distance::Unrelated || self.ignore_ancestry)
  }

  /// Whether an unchanged entry needs nothing: unreported, or reported with its entries and with no
  /// lock token, or with the token of the lock held now.
  fn left_alone<R: Repository + ?Sized, G: Guards + ?Sized>(
    &self,
    entry: &EntryUpdate,
    sources: &Sources<'_, R, G>,
  ) -> bool {
    match &entry.info {
      None => true,
      Some(info) if info.start_empty => false,
      Some(info) => info
        .lock_token
        .as_deref()
        .is_none_or(|token| sources.guards.lock_token(&entry.t_path) == Some(token)),
    }
  }

  /// `update_entry`'s deletion of a source unrelated to its target, with the revision it was
  /// deleted in, or the one before the target's when the path still holds a node.
  fn delete_unrelated<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    entry: &EntryUpdate,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let deleted = history::deleted_revision(
      sources.index,
      &entry.t_path,
      entry.s_rev,
      self.target_revision,
    )?;
    let revision = match deleted {
      Some(revision) => Some(revision),
      None => dirent(sources, &entry.t_path, self.target_revision)?
        .and_then(|_| self.target_revision.checked_sub(1)),
    };
    self
      .pending
      .push_back(Owned::Delete(entry.e_path.clone(), revision));
    Ok(())
  }

  /// A directory opened when related, else added, then compared.
  fn enter_directory(&mut self, entry: &EntryUpdate, t_entry: Dirent, related: bool) {
    let facts = t_entry.facts(&entry.t_path);
    let call = if related {
      Owned::Open(
        NodeKind::Directory,
        entry.e_path.clone(),
        Some(entry.s_rev),
        facts,
      )
    } else {
      Owned::Add(NodeKind::Directory, entry.e_path.clone(), None, facts)
    };
    self.pending.push_back(call);
    let start_empty = entry.info.as_ref().is_some_and(|info| info.start_empty);
    self
      .frames
      .push(Frame::Directory(Box::new(DirectoryFrame::new(
        entry,
        start_empty,
      ))));
  }

  /// A file opened when related, else added (`add_file_smartly`, with its copy source when the
  /// client asked for them), then sent against its source or its copy source.
  fn enter_file<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    entry: EntryUpdate,
    t_entry: Dirent,
    related: bool,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let facts = t_entry.facts(&entry.t_path);
    let (s_rev, s_path) = if related {
      self.pending.push_back(Owned::Open(
        NodeKind::File,
        entry.e_path.clone(),
        Some(entry.s_rev),
        facts,
      ));
      (entry.s_rev, entry.s_path)
    } else {
      let copy = self.copy_source(&entry.t_path, sources)?;
      self.pending.push_back(Owned::Add(
        NodeKind::File,
        entry.e_path.clone(),
        copy.clone(),
        facts,
      ));
      copy.map_or((entry.s_rev, entry.s_path), |(path, revision)| {
        (revision, Some(path))
      })
    };
    self.frames.push(Frame::File(Box::new(FileFrame {
      s_rev,
      s_path,
      t_path: entry.t_path,
      target: t_entry,
      lock_token: entry.info.and_then(|info| info.lock_token),
      phase: FilePhase::Properties,
      source: None,
      sent: 0,
    })));
    Ok(())
  }

  /// `add_file_smartly`'s copy source: the file's own copy, when the client asked for copy sources
  /// and may read the source.
  fn copy_source<R: Repository + ?Sized, G: Guards + ?Sized>(
    &self,
    t_path: &str,
    sources: &Sources<'_, R, G>,
  ) -> Result<Option<(String, Revision)>, DriveFailure> {
    if !self.send_copyfrom_args {
      return Ok(None);
    }
    let Some((copied_in, copy_path)) =
      history::closest_copy(sources.index, t_path, self.target_revision)?
    else {
      return Ok(None);
    };
    if copy_path != t_path {
      return Ok(None);
    }
    let copy = history::present(sources.index, &copy_path, copied_in)?;
    let source = copy.and_then(|node| {
      let (path, revision) = node.copy_from()?;
      Some((path.to_owned(), revision))
    });
    Ok(source.filter(|(path, _)| may_read(sources.rules, path)))
  }

  /// One step of a file's sending.
  fn step_file<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut file: FileFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    match file.phase {
      FilePhase::Properties => {
        let token = file.lock_token.as_deref();
        self.properties(
          sources,
          file.s_rev,
          file.s_path.as_deref(),
          &file.t_path,
          token,
        )?;
        file.phase = FilePhase::Text;
      }
      FilePhase::Text => self.text(&mut file, sources)?,
      FilePhase::Windows => self.window(&mut file, sources)?,
      FilePhase::End => {
        self.pending.push_back(Owned::TextDeltaEnd);
        file.phase = FilePhase::Close;
      }
      FilePhase::Close => {
        self.pending.push_back(Owned::CloseFile(file.target.md5()));
        return Ok(());
      }
    }
    self.frames.push(Frame::File(Box::new(file)));
    Ok(())
  }

  /// `delta_files`' text: none when the source's is the same, else the delta begun against the
  /// source's MD5, its windows next when the client asked for texts.
  fn text<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    file: &mut FileFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let base = match file.s_path.as_deref() {
      Some(s_path) => {
        let source = dirent(sources, s_path, file.s_rev)?.ok_or_else(DriveFailure::corrupt)?;
        if source.content() == file.target.content() {
          file.phase = FilePhase::Close;
          return Ok(());
        }
        file.source = source.text;
        Some(source.md5())
      }
      None => None,
    };
    self.pending.push_back(Owned::ApplyTextDelta(base));
    file.phase = if self.text_deltas {
      FilePhase::Windows
    } else {
      FilePhase::End
    };
    Ok(())
  }

  /// The delta's next window, `txdelta_next_window`'s: the target's next piece of up to
  /// [`WINDOW_LIMIT`] bytes made from the source's piece at the same place, by Subversion's
  /// deltifier ([`deltas`]), or as new data once the source has run out, its view where the source
  /// ended; at the text's end, the delta's end next.
  fn window<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    file: &mut FileFrame,
    sources: &Sources<'_, R, G>,
  ) -> Result<(), DriveFailure> {
    let Some(text) = file.target.text.filter(|text| file.sent < text.size) else {
      file.phase = FilePhase::End;
      return Ok(());
    };
    let target = read_window(sources.repository, &text, file.sent)?;
    let source_size = file.source.map_or(0, |source| source.size);
    let view = file.sent.min(source_size);
    let source = match file.source {
      Some(source) if view < source.size => read_window(sources.repository, &source, view)?,
      _ => Vec::new(),
    };
    let mut window = window_of(&source, &target)?;
    window.source_offset = view;
    let length = u64::try_from(target.len()).map_err(|_| DriveFailure::corrupt())?;
    file.sent = file.sent.saturating_add(length);
    self.pending.push_back(Owned::Window(window));
    Ok(())
  }

  /// `delta_proplists`: the entry properties, the lock token's removal when the lock is gone, then
  /// the node's own properties against the source's, or all of them when there is no source.
  fn properties<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    sources: &Sources<'_, R, G>,
    s_rev: Revision,
    s_path: Option<&str>,
    t_path: &str,
    lock_token: Option<&[u8]>,
  ) -> Result<(), DriveFailure> {
    let target =
      dirent(sources, t_path, self.target_revision)?.ok_or_else(DriveFailure::corrupt)?;
    self.entry_properties(sources, target.created, s_path.is_some())?;
    if let Some(token) = lock_token
      && sources.guards.lock_token(t_path) != Some(token)
    {
      self
        .pending
        .push_back(Owned::ChangeProperty(LOCK_TOKEN.to_owned(), None));
    }
    let source = match s_path {
      Some(s_path) => {
        let source = dirent(sources, s_path, s_rev)?.ok_or_else(DriveFailure::corrupt)?;
        if source.properties == target.properties {
          return Ok(());
        }
        property_list(sources.repository, source.properties)?
      }
      None => Vec::new(),
    };
    let target = property_list(sources.repository, target.properties)?;
    for (name, value) in property_changes(&source, &target) {
      self.pending.push_back(Owned::ChangeProperty(name, value));
    }
    Ok(())
  }

  /// The entry properties of a node created in `created`: its revision, then that revision's date
  /// and author (each removed when the revision has none and a source holds one), then the UUID.
  fn entry_properties<R: Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    sources: &Sources<'_, R, G>,
    created: Revision,
    has_source: bool,
  ) -> Result<(), DriveFailure> {
    let properties = sources
      .repository
      .revision_properties(created)
      .ok_or_else(DriveFailure::corrupt)?;
    let value = |wanted: &[u8]| {
      properties
        .iter()
        .find(|(name, _)| *name == wanted)
        .map(|(_, value)| value.to_vec())
    };
    let (date, author) = (value(REVISION_DATE), value(REVISION_AUTHOR));
    self.pending.push_back(Owned::ChangeProperty(
      COMMITTED_REV.to_owned(),
      Some(created.to_string().into_bytes()),
    ));
    if date.is_some() || has_source {
      self
        .pending
        .push_back(Owned::ChangeProperty(COMMITTED_DATE.to_owned(), date));
    }
    if author.is_some() || has_source {
      self
        .pending
        .push_back(Owned::ChangeProperty(LAST_AUTHOR.to_owned(), author));
    }
    self.pending.push_back(Owned::ChangeProperty(
      ENTRY_UUID.to_owned(),
      Some(self.uuid.clone()),
    ));
    Ok(())
  }

  /// `fetch_path_info`: the next report path beneath `prefix`, as its child's name and, when it is
  /// the child itself, the path, consumed.
  fn fetch_path_info(&mut self, prefix: &str) -> Option<(String, Option<PathInfo>)> {
    let rest = beneath(&self.lookahead.front()?.path, prefix)?;
    match rest.split_once('/') {
      Some((child, _)) => Some((child.to_owned(), None)),
      None => {
        let name = rest.to_owned();
        Some((name, self.lookahead.pop_front()))
      }
    }
  }

  /// `skip_path_info`: every report path beneath `prefix` consumed.
  fn skip_path_info(&mut self, prefix: &str) {
    while self.any_path_info(prefix) {
      self.lookahead.pop_front();
    }
  }

  /// `any_path_info`: whether the next report path is beneath `prefix`.
  fn any_path_info(&self, prefix: &str) -> bool {
    self
      .lookahead
      .front()
      .is_some_and(|next| beneath(&next.path, prefix).is_some())
  }
}

/// `relevant`: what of `path` lies beneath `prefix` (all of it beneath the anchor's `""`); none
/// when `path` is not beneath it.
fn beneath<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
  if prefix.is_empty() {
    Some(path)
  } else {
    path.strip_prefix(prefix)?.strip_prefix('/')
  }
}

/// The parent of a repository path, `svn_fspath__dirname`'s: `/` for the root and its children.
fn dirname(path: &str) -> &str {
  match path.rsplit_once('/') {
    Some(("", _)) | None => "/",
    Some((parent, _)) => parent,
  }
}

/// `DEPTH_BELOW_HERE`: the depth one level down, `empty` below `immediates`.
fn below(depth: Depth) -> Depth {
  if depth == Depth::Immediates {
    Depth::Empty
  } else {
    depth
  }
}

/// Whether a source entry of `kind` lies within the working copy's depth and the depth asked, as
/// `delta_dirs`' deletions test it.
fn within_depth(kind: NodeKind, wc_depth: Depth, requested_depth: Depth) -> bool {
  match kind {
    NodeKind::File => wc_depth >= Depth::Files,
    NodeKind::Directory => wc_depth >= Depth::Immediates && requested_depth != Depth::Files,
  }
}

/// `is_depth_upgrade`: whether the entry is new to a working copy being made deeper, so its source
/// is ignored.
fn is_depth_upgrade(wc_depth: Depth, requested_depth: Depth, kind: NodeKind) -> bool {
  let no_deeper = requested_depth == Depth::Unknown
    || requested_depth <= wc_depth
    || wc_depth == Depth::Immediates;
  let files_held = kind == NodeKind::File && wc_depth == Depth::Files;
  let files_only =
    kind == NodeKind::Directory && wc_depth == Depth::Empty && requested_depth == Depth::Files;
  !(no_deeper || files_held || files_only)
}

/// The update of a reported path's child, or none when the depth asked is `files` and either side
/// is a directory, or the path is excluded; the name is done in the directory's entries either way.
fn reported_entry(
  directory: &mut DirectoryFrame,
  name: String,
  info: Option<PathInfo>,
) -> Option<EntryUpdate> {
  let t_entry = directory.t_entries.remove(&name);
  let excluded = info
    .as_ref()
    .is_some_and(|info| info.depth == Depth::Exclude);
  let s_entry = if !excluded || t_entry.is_some() {
    directory.s_entries.remove(&name)
  } else {
    directory.s_entries.get(&name).copied()
  };
  let is_directory =
    |entry: Option<Dirent>| entry.is_some_and(|entry| entry.kind == NodeKind::Directory);
  let files_only =
    directory.requested_depth == Depth::Files && (is_directory(t_entry) || is_directory(s_entry));
  if files_only || excluded {
    return None;
  }
  Some(EntryUpdate {
    s_rev: directory.s_rev,
    s_path: directory
      .s_path
      .as_ref()
      .map(|s_path| join_fspath(s_path, &name)),
    s_entry,
    t_path: join_fspath(&directory.t_path, &name),
    t_entry,
    e_path: join_relpath(&directory.e_path, &name),
    wc_depth: info
      .as_ref()
      .map_or(below(directory.wc_depth), |info| info.depth),
    info,
    requested_depth: below(directory.requested_depth),
  })
}

/// The update of a target entry the report did not name, or none outside the depths.
fn target_update(directory: &DirectoryFrame, name: &str, t_entry: Dirent) -> Option<EntryUpdate> {
  let upgrade = is_depth_upgrade(directory.wc_depth, directory.requested_depth, t_entry.kind);
  let outside = match t_entry.kind {
    NodeKind::File => {
      directory.requested_depth == Depth::Unknown && directory.wc_depth < Depth::Files
    }
    NodeKind::Directory => {
      directory.wc_depth < Depth::Immediates || directory.requested_depth == Depth::Files
    }
  };
  if !upgrade && outside {
    return None;
  }
  let s_entry = (!upgrade)
    .then(|| directory.s_entries.get(name).copied())
    .flatten();
  let s_path = s_entry
    .and(directory.s_path.as_ref())
    .map(|s_path| join_fspath(s_path, name));
  Some(EntryUpdate {
    s_rev: directory.s_rev,
    s_path,
    s_entry,
    t_path: join_fspath(&directory.t_path, name),
    t_entry: Some(t_entry),
    e_path: join_relpath(&directory.e_path, name),
    info: None,
    wc_depth: below(directory.wc_depth),
    requested_depth: below(directory.requested_depth),
  })
}

/// The node at `path` in `revision` as an entry; none when absent there or out of canonical form.
fn dirent<R: ?Sized, G: ?Sized>(
  sources: &Sources<'_, R, G>,
  path: &str,
  revision: Revision,
) -> Result<Option<Dirent>, DriveFailure> {
  Ok(history::present(sources.index, path, revision)?.map(|node| Dirent::of(&node)))
}

/// The entries of the directory at `path` in `revision`, by name (`svn_fs_dir_entries`).
fn entries<R: Repository + ?Sized, G: ?Sized>(
  sources: &Sources<'_, R, G>,
  path: &str,
  revision: Revision,
) -> Result<BTreeMap<String, Dirent>, DriveFailure> {
  let names = sources
    .repository
    .entries(path, revision)
    .ok_or_else(DriveFailure::corrupt)?;
  let mut entries = BTreeMap::new();
  for name in names {
    let entry =
      dirent(sources, &join_fspath(path, &name), revision)?.ok_or_else(DriveFailure::corrupt)?;
    entries.insert(name, entry);
  }
  Ok(entries)
}

/// A property list's properties in name order, each name UTF-8 as every property name is.
fn property_list<R: Repository + ?Sized>(
  repository: &R,
  list: PropertyList,
) -> Result<Vec<(String, Vec<u8>)>, DriveFailure> {
  let PropertyList::Held { identity, .. } = list else {
    return Ok(Vec::new());
  };
  let held = repository
    .properties(&identity)
    .ok_or_else(DriveFailure::corrupt)?;
  let mut properties = Vec::with_capacity(held.len());
  for (name, value) in held {
    let name = std::str::from_utf8(name).map_err(|_| DriveFailure::corrupt())?;
    properties.push((name.to_owned(), value.to_vec()));
  }
  properties.sort_unstable_by(|left, right| left.0.cmp(&right.0));
  Ok(properties)
}

/// `svn_prop_diffs` from `source` to `target`, both in name order: each source property removed or
/// changed, then each property new to the target.
fn property_changes(
  source: &[(String, Vec<u8>)],
  target: &[(String, Vec<u8>)],
) -> Vec<(String, Option<Vec<u8>>)> {
  let find = |list: &[(String, Vec<u8>)], wanted: &str| {
    list
      .binary_search_by(|(name, _)| name.as_str().cmp(wanted))
      .ok()
      .and_then(|at| list.get(at))
      .map(|(_, value)| value.clone())
  };
  let mut changes = Vec::new();
  for (name, value) in source {
    match find(target, name) {
      None => changes.push((name.clone(), None)),
      Some(now) if now != *value => changes.push((name.clone(), Some(now))),
      Some(_) => {}
    }
  }
  for (name, value) in target {
    if find(source, name).is_none() {
      changes.push((name.clone(), Some(value.clone())));
    }
  }
  changes
}

/// The one window that makes `target`, at most a window's room, from `source`, at most as much: the
/// target as new data when the source is empty (`compute_window`), else the deltifier's window, its
/// source view counted from the source piece's start.
fn window_of(source: &[u8], target: &[u8]) -> Result<Window, DriveFailure> {
  let length = u64::try_from(target.len()).map_err(|_| DriveFailure::corrupt())?;
  if source.is_empty() {
    return Ok(Window {
      source_offset: 0,
      source_len: 0,
      target_len: length,
      instructions: vec![Instruction::New { length }],
      new_data: target.to_vec(),
    });
  }
  let room = usize::try_from(WINDOW_LIMIT)
    .ok()
    .and_then(NonZeroUsize::new)
    .ok_or_else(DriveFailure::corrupt)?;
  deltas(source, target, room)
    .map_err(|_| DriveFailure::corrupt())?
    .next()
    .ok_or_else(DriveFailure::corrupt)
}

/// Up to [`WINDOW_LIMIT`] bytes of `text` from `offset`, gathered from the green's runs.
fn read_window<R: Repository + ?Sized>(
  repository: &R,
  text: &Text,
  offset: u64,
) -> Result<Vec<u8>, DriveFailure> {
  let left = text.size.saturating_sub(offset).min(WINDOW_LIMIT);
  let want = usize::try_from(left).map_err(|_| DriveFailure::corrupt())?;
  let mut window = Vec::with_capacity(want);
  while window.len() < want {
    let at = u64::try_from(window.len())
      .ok()
      .and_then(|read| offset.checked_add(read))
      .ok_or_else(DriveFailure::corrupt)?;
    let run = repository
      .text(&text.identity, at)
      .filter(|run| !run.is_empty())
      .ok_or_else(DriveFailure::corrupt)?;
    let take = run.len().min(want.saturating_sub(window.len()));
    window.extend_from_slice(run.get(..take).ok_or_else(DriveFailure::corrupt)?);
  }
  Ok(window)
}
