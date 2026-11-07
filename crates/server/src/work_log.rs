//! A work volume's log in the partition (A-207; §4.16; D-18: "Local acknowledgement promises daemon-restart survival only
//! when bytes, roots, witnesses, accounting and completion records are recoverable from anchor-owned RAM"). Every
//! verb that changes a work records what it did in the work's log, in the same database transaction as its
//! completion record: an edit or a declaration appends one entry ([`slates_db::op::Op::WorkLogged`]), and an accepted
//! submit or a rebase restates the work whole ([`slates_db::op::Op::WorkRestated`]: its base, the files it holds of
//! its own and its journal). Recovery replays the log onto the work over its green, recovered first, so the work comes
//! back exactly as acknowledged.
//!
//! The change itself is applied by one code path for the verb and for the replay ([`apply_edit`], [`apply_declare`]),
//! so a replayed work cannot drift from the live one. The entries are encoded by the wire codec (`Wire`), whose
//! decoder refuses a truncated, overlong or trailing body; the journal's operations are carried by a mirror of the
//! merge crate's [`VolumeOp`], which stays free of any codec (a pure core).
//!
//! Evidence (2026-11-04, `crates/server/tests/merge_load.rs`,
//! `an_acknowledged_work_edit_survives_a_daemon_restart_or_is_refused`): before the log, a work was rebuilt at
//! recovery as a fresh clone of its green's head, so an edit answered `Edited` read back as the green's bytes after a
//! daemon restart, and the next submit would have been accepted without it, silently.

use slates_merge::engine::Green;
use slates_merge::increment::VolumeOp;
use slates_merge::special::SpecialNode;
use slates_wire::Wire;

use std::collections::BTreeMap;

use crate::work_files::{Base, BaseGone, WorkFiles};

/// One journal operation as the log carries it: [`VolumeOp`], field for field, with an IPC node as its own encoding
/// ([`SpecialNode::encode`]).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoggedOp {
  /// [`VolumeOp::Overwrite`].
  Overwrite {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The length.
    len: u64,
  },
  /// [`VolumeOp::Extend`].
  Extend {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The length.
    len: u64,
  },
  /// [`VolumeOp::Truncate`].
  Truncate {
    /// The file.
    path: String,
    /// The new length.
    len: u64,
  },
  /// [`VolumeOp::Insert`].
  Insert {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The length.
    len: u64,
  },
  /// [`VolumeOp::Delete`].
  Delete {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The length.
    len: u64,
  },
  /// [`VolumeOp::Create`].
  Create {
    /// The file.
    path: String,
  },
  /// [`VolumeOp::Mknod`].
  Mknod {
    /// The name.
    path: String,
    /// The node, as [`SpecialNode::encode`] writes it.
    node: Vec<u8>,
    /// The mode.
    mode: u32,
  },
  /// [`VolumeOp::Unlink`].
  Unlink {
    /// The name.
    path: String,
  },
  /// [`VolumeOp::Rename`].
  Rename {
    /// The old name.
    from: String,
    /// The new name.
    to: String,
  },
  /// [`VolumeOp::Mkdir`].
  Mkdir {
    /// The directory.
    path: String,
  },
  /// [`VolumeOp::Rmdir`].
  Rmdir {
    /// The directory.
    path: String,
  },
  /// [`VolumeOp::SetMode`].
  SetMode {
    /// The name.
    path: String,
    /// The mode.
    mode: u32,
  },
  /// [`VolumeOp::Symlink`].
  Symlink {
    /// The link.
    path: String,
    /// Its target.
    target: String,
  },
  /// [`VolumeOp::SetXattr`].
  SetXattr {
    /// The name.
    path: String,
    /// The attribute.
    name: String,
    /// Its value.
    value: Vec<u8>,
  },
  /// [`VolumeOp::RemoveXattr`].
  RemoveXattr {
    /// The name.
    path: String,
    /// The attribute.
    name: String,
  },
  /// [`VolumeOp::Link`].
  Link {
    /// The new name.
    path: String,
    /// The name it links to.
    target: String,
  },
}

/// A file a work holds of its own, in a restatement: its bytes, or `None` for a file it removed.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnFile {
  /// The file.
  pub(crate) path: String,
  /// Its bytes, or none for a removal.
  pub(crate) bytes: Option<Vec<u8>>,
}

/// One entry of a work's log.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkLogEntry {
  /// An edit, as the verb applied it.
  Edit {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The bytes removed there.
    delete_len: u64,
    /// The bytes inserted there.
    bytes: Vec<u8>,
  },
  /// A namespace or metadata declaration, as the verb applied it.
  Declared {
    /// The operation.
    op: LoggedOp,
  },
  /// The work restated whole: its base version, what it holds of its own, and its journal.
  Restated {
    /// The green version the work is based on.
    base: u64,
    /// The files it holds of its own.
    files: Vec<OwnFile>,
    /// Its journal.
    journal: Vec<LoggedOp>,
  },
}

/// Why a work's log could not be replayed: a refusal the recovery counts, leaving the work unbuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplayError {
  /// An entry's bytes did not decode.
  Undecodable,
  /// A logged IPC node did not decode.
  BadNode,
  /// The work's green is gone, so a file it had not changed cannot be read.
  BaseGone,
}

impl From<BaseGone> for ReplayError {
  fn from(_: BaseGone) -> ReplayError {
    ReplayError::BaseGone
  }
}

impl LoggedOp {
  /// The log's form of `op`.
  pub(crate) fn of(op: &VolumeOp) -> LoggedOp {
    match op.clone() {
      VolumeOp::Overwrite { path, at, len } => LoggedOp::Overwrite { path, at, len },
      VolumeOp::Extend { path, at, len } => LoggedOp::Extend { path, at, len },
      VolumeOp::Truncate { path, len } => LoggedOp::Truncate { path, len },
      VolumeOp::Insert { path, at, len } => LoggedOp::Insert { path, at, len },
      VolumeOp::Delete { path, at, len } => LoggedOp::Delete { path, at, len },
      VolumeOp::Create { path } => LoggedOp::Create { path },
      VolumeOp::Mknod { path, node, mode } => LoggedOp::Mknod {
        path,
        node: node.encode(),
        mode,
      },
      VolumeOp::Unlink { path } => LoggedOp::Unlink { path },
      VolumeOp::Rename { from, to } => LoggedOp::Rename { from, to },
      VolumeOp::Mkdir { path } => LoggedOp::Mkdir { path },
      VolumeOp::Rmdir { path } => LoggedOp::Rmdir { path },
      VolumeOp::SetMode { path, mode } => LoggedOp::SetMode { path, mode },
      VolumeOp::Symlink { path, target } => LoggedOp::Symlink { path, target },
      VolumeOp::SetXattr { path, name, value } => LoggedOp::SetXattr { path, name, value },
      VolumeOp::RemoveXattr { path, name } => LoggedOp::RemoveXattr { path, name },
      VolumeOp::Link { path, target } => LoggedOp::Link { path, target },
    }
  }

  /// The journal operation this entry names, or `BadNode` for an IPC node that does not decode.
  pub(crate) fn op(self) -> Result<VolumeOp, ReplayError> {
    Ok(match self {
      LoggedOp::Overwrite { path, at, len } => VolumeOp::Overwrite { path, at, len },
      LoggedOp::Extend { path, at, len } => VolumeOp::Extend { path, at, len },
      LoggedOp::Truncate { path, len } => VolumeOp::Truncate { path, len },
      LoggedOp::Insert { path, at, len } => VolumeOp::Insert { path, at, len },
      LoggedOp::Delete { path, at, len } => VolumeOp::Delete { path, at, len },
      LoggedOp::Create { path } => VolumeOp::Create { path },
      LoggedOp::Mknod { path, node, mode } => VolumeOp::Mknod {
        path,
        node: SpecialNode::decode(&node).map_err(|_| ReplayError::BadNode)?,
        mode,
      },
      LoggedOp::Unlink { path } => VolumeOp::Unlink { path },
      LoggedOp::Rename { from, to } => VolumeOp::Rename { from, to },
      LoggedOp::Mkdir { path } => VolumeOp::Mkdir { path },
      LoggedOp::Rmdir { path } => VolumeOp::Rmdir { path },
      LoggedOp::SetMode { path, mode } => VolumeOp::SetMode { path, mode },
      LoggedOp::Symlink { path, target } => VolumeOp::Symlink { path, target },
      LoggedOp::SetXattr { path, name, value } => VolumeOp::SetXattr { path, name, value },
      LoggedOp::RemoveXattr { path, name } => VolumeOp::RemoveXattr { path, name },
      LoggedOp::Link { path, target } => VolumeOp::Link { path, target },
    })
  }
}

impl WorkLogEntry {
  /// The restatement of a work at `base` holding `files` of its own, with `journal`.
  pub(crate) fn restated(base: u64, files: &WorkFiles, journal: &[VolumeOp]) -> WorkLogEntry {
    WorkLogEntry::Restated {
      base,
      files: files
        .changed_entries()
        .map(|(path, bytes)| OwnFile {
          path: path.to_owned(),
          bytes: bytes.map(<[u8]>::to_vec),
        })
        .collect(),
      journal: journal.iter().map(LoggedOp::of).collect(),
    }
  }
}

/// The journal operations an edit of `path` declares: a create when the file is new (`old` is none), the removed
/// range, and the inserted bytes as an extend past the file's end or an insert within it.
pub(crate) fn edit_ops(
  path: &str,
  old: Option<&[u8]>,
  (at, delete_len, inserted): (u64, u64, u64),
) -> Vec<VolumeOp> {
  let old_len = old.map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX));
  let mut ops = Vec::new();
  if old.is_none() {
    ops.push(VolumeOp::Create {
      path: path.to_owned(),
    });
  }
  if delete_len > 0 {
    ops.push(VolumeOp::Delete {
      path: path.to_owned(),
      at,
      len: delete_len,
    });
  }
  if inserted > 0 {
    ops.push(if at >= old_len {
      VolumeOp::Extend {
        path: path.to_owned(),
        at,
        len: inserted,
      }
    } else {
      VolumeOp::Insert {
        path: path.to_owned(),
        at,
        len: inserted,
      }
    });
  }
  ops
}

/// Applies an edit to a work's `files` and `journal` over its `base` (`WorkFiles::splice`: the file becomes the work's
/// own on its first change), and journals `ops`. The one change path of the verb and of the replay.
pub(crate) fn apply_edit(
  (files, journal): (&mut WorkFiles, &mut Vec<VolumeOp>),
  base: Base<'_>,
  (path, at, delete_len, bytes): (&str, u64, u64, &[u8]),
  ops: Vec<VolumeOp>,
) -> Result<(), BaseGone> {
  files.splice(base, path, (at, delete_len), bytes)?;
  journal.extend(ops);
  Ok(())
}

/// Applies a declaration to a work's `files` and `journal` over its `base` (`WorkFiles::declare`), and journals it.
/// The one change path of the verb and of the replay.
pub(crate) fn apply_declare(
  (files, journal): (&mut WorkFiles, &mut Vec<VolumeOp>),
  base: Base<'_>,
  op: VolumeOp,
) -> Result<(), BaseGone> {
  files.declare(base, &op)?;
  journal.push(op);
  Ok(())
}

/// A work rebuilt from its log over `green`: its base (`created_at` unless a restatement names another), what it holds
/// of its own, and its journal, each entry applied by the verbs' own change path.
pub(crate) fn replay(
  green: Option<&Green>,
  created_at: u64,
  entries: &[Vec<u8>],
) -> Result<(u64, WorkFiles, Vec<VolumeOp>), ReplayError> {
  let mut version = created_at;
  let mut files = WorkFiles::default();
  let mut journal: Vec<VolumeOp> = Vec::new();
  for bytes in entries {
    match WorkLogEntry::from_bytes(bytes).map_err(|_| ReplayError::Undecodable)? {
      WorkLogEntry::Restated {
        base: restated,
        files: own,
        journal: logged,
      } => {
        version = restated;
        files = WorkFiles::from_changed(
          own
            .into_iter()
            .map(|file| (file.path, file.bytes))
            .collect::<BTreeMap<_, _>>(),
        );
        journal = logged
          .into_iter()
          .map(LoggedOp::op)
          .collect::<Result<_, _>>()?;
      }
      WorkLogEntry::Edit {
        path,
        at,
        delete_len,
        bytes,
      } => {
        let base = Base { green, version };
        let old = files.get(base, &path)?.map(std::borrow::Cow::into_owned);
        let inserted = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let ops = edit_ops(&path, old.as_deref(), (at, delete_len, inserted));
        apply_edit(
          (&mut files, &mut journal),
          base,
          (&path, at, delete_len, &bytes),
          ops,
        )?;
      }
      WorkLogEntry::Declared { op } => {
        apply_declare(
          (&mut files, &mut journal),
          Base { green, version },
          op.op()?,
        )?;
      }
    }
  }
  Ok((version, files, journal))
}

#[cfg(test)]
mod tests {
  #![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
  )]
  use super::*;

  /// One of every journal operation, so a round trip covers each variant.
  fn every_op() -> Vec<VolumeOp> {
    let node = SpecialNode {
      kind: slates_merge::special::SpecialKind::Fifo,
      uid: 7,
      gid: 8,
      atime: 1,
      mtime: 2,
      ctime: 3,
      btime: 4,
    };
    let p = || "dir/f".to_owned();
    vec![
      VolumeOp::Overwrite {
        path: p(),
        at: 1,
        len: 2,
      },
      VolumeOp::Extend {
        path: p(),
        at: 3,
        len: 4,
      },
      VolumeOp::Truncate { path: p(), len: 5 },
      VolumeOp::Insert {
        path: p(),
        at: 6,
        len: 7,
      },
      VolumeOp::Delete {
        path: p(),
        at: 8,
        len: 9,
      },
      VolumeOp::Create { path: p() },
      VolumeOp::Mknod {
        path: p(),
        node,
        mode: 0o640,
      },
      VolumeOp::Unlink { path: p() },
      VolumeOp::Rename {
        from: p(),
        to: "g".to_owned(),
      },
      VolumeOp::Mkdir {
        path: "d".to_owned(),
      },
      VolumeOp::Rmdir {
        path: "d".to_owned(),
      },
      VolumeOp::SetMode {
        path: p(),
        mode: 0o600,
      },
      VolumeOp::Symlink {
        path: "l".to_owned(),
        target: "t".to_owned(),
      },
      VolumeOp::SetXattr {
        path: p(),
        name: "user.a".to_owned(),
        value: vec![1, 2],
      },
      VolumeOp::RemoveXattr {
        path: p(),
        name: "user.a".to_owned(),
      },
      VolumeOp::Link {
        path: "h".to_owned(),
        target: p(),
      },
    ]
  }

  /// §4.16, D-18: do restate a work holding a file and a removal with every journal operation, encode it, and decode
  /// it; expect the same entry, and the replay of it alone to give back the base, the files and the journal.
  #[test]
  fn a_restatement_round_trips_every_journal_operation() {
    let files = WorkFiles::from_changed(
      [
        ("kept".to_owned(), Some(b"own bytes".to_vec())),
        ("gone".to_owned(), None),
      ]
      .into_iter()
      .collect(),
    );
    let journal = every_op();
    let entry = WorkLogEntry::restated(9, &files, &journal);
    let bytes = entry.to_bytes();
    assert_eq!(WorkLogEntry::from_bytes(&bytes).unwrap(), entry);
    let (base, replayed_files, replayed_journal) = replay(None, 0, &[bytes]).unwrap();
    assert_eq!(base, 9);
    assert_eq!(
      replayed_files.changed_entries().collect::<Vec<_>>(),
      files.changed_entries().collect::<Vec<_>>()
    );
    assert_eq!(replayed_journal, journal);
  }

  /// Hostile input (CLAUDE.md §4): do decode every truncation of an encoded entry and the entry with a trailing byte;
  /// expect each refused `Undecodable` by the replay, never a panic or a partial work.
  #[test]
  fn a_truncated_or_overlong_entry_is_refused() {
    let files = WorkFiles::from_changed(
      [("kept".to_owned(), Some(b"own bytes".to_vec()))]
        .into_iter()
        .collect(),
    );
    let bytes = WorkLogEntry::restated(3, &files, &every_op()).to_bytes();
    for cut in 0..bytes.len() {
      assert_eq!(
        replay(None, 0, &[bytes[..cut].to_vec()]).err(),
        Some(ReplayError::Undecodable),
        "cut at {cut}"
      );
    }
    let mut long = bytes;
    long.push(0);
    assert_eq!(
      replay(None, 0, &[long]).err(),
      Some(ReplayError::Undecodable)
    );
  }
}
