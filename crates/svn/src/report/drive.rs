//! The editor drive that answers a finished report (§4.17.4 "4. Reads", the reporter, piece 7's
//! third part; A-616, its second part): the calls of Subversion's delta editor
//! (`svn_delta.h`'s `svn_delta_editor_t`) in the order `reporter.c` makes them, one value a call.
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
//! - [`Call::CloseFile`] and [`Call::CloseDirectory`], innermost first, then
//!   [`Call::CloseEdit`]; or [`Call::AbortEdit`] wherever the drive meets a refusal.
//!
//! So the nodes open at any moment are a stack, and a property change applies to the innermost one
//! still open. No I/O, no clock, no randomness, no panic.

use crate::index::{NodeKind, Revision};
use crate::svndiff::Window;

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
