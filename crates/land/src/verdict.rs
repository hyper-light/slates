//! The per-entry verdict (§4.15 step 4; AC-1.12): a pure function of three inputs, the
//! witnessed base, the disk now, and the overlay; the table is the specification and the test
//! enumerates it. No verdict ever writes; a conflict refuses the whole landing.

use slates_vfs::inode::{Fingerprint, Witness};

use crate::manifest::{Action, OverlayIdentity};

/// The verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// Write the entry.
  Apply,
  /// The disk already holds what the overlay would write, or nothing is to land.
  Skip,
  /// The disk changed to the same bytes as the overlay: accept by identity, write nothing.
  AcceptIdentical,
  /// A conflict of the named class: nothing is written.
  Conflict(ConflictClass),
}

/// The conflict classes of §4.15.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictClass {
  /// Both sides changed the entry differently.
  ModifyModify,
  /// The overlay changed it; the disk deleted it.
  ModifyDelete,
  /// The overlay deleted it; the disk changed it.
  DeleteModify,
  /// The overlay renamed the directory; the disk moved its origin.
  RenameRename,
  /// Both sides created the entry, with different bytes.
  CreateCreate,
  /// A file became a directory or the reverse.
  TypeChanged,
  /// An outsider replaced the file between validation and the exchange.
  TargetInUse,
}

/// What the disk holds at the entry's path right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskState {
  /// Nothing.
  Absent,
  /// A regular file with this fingerprint and, when the fingerprint moved from the witness,
  /// the BLAKE3 of its bytes (hashed only then).
  File {
    /// The fingerprint.
    fingerprint: Fingerprint,
    /// The identity, when hashed.
    identity: Option<[u8; 32]>,
  },
  /// A directory with this fingerprint.
  Dir {
    /// The fingerprint.
    fingerprint: Fingerprint,
    /// Whether every name inside is one this landing's manifest creates beneath the path (an
    /// empty directory counts): a resumed clear finds its own fresh directory this way.
    holds_only_ours: bool,
  },
  /// A symlink.
  Symlink,
  /// For a rename only: the origin is gone and the destination holds this directory (a landed
  /// rename met again on resume).
  AtDestination(Fingerprint),
}

/// The verdict for one entry (§4.15's table).
pub fn verdict(
  action: &Action,
  witnessed: Option<&Witness>,
  disk: DiskState,
  overlay: Option<&OverlayIdentity>,
) -> Verdict {
  match action {
    Action::Create | Action::Symlink { .. } => created(action, disk, overlay),
    Action::Mkdir => match disk {
      DiskState::Absent => Verdict::Apply,
      DiskState::Dir { .. } | DiskState::AtDestination(_) => Verdict::Skip,
      DiskState::File { .. } | DiskState::Symlink => Verdict::Conflict(ConflictClass::TypeChanged),
    },
    Action::Replace => replaced(witnessed, disk, overlay),
    Action::Delete => match (witnessed, disk) {
      (_, DiskState::Absent) => Verdict::Skip,
      (Some(w), DiskState::File { fingerprint, .. }) if fingerprint == w.fingerprint => {
        Verdict::Apply
      }
      (Some(_), DiskState::File { .. }) => Verdict::Conflict(ConflictClass::DeleteModify),
      (Some(_), DiskState::Dir { .. } | DiskState::AtDestination(_)) => {
        Verdict::Conflict(ConflictClass::TypeChanged)
      }
      (_, DiskState::Symlink) => Verdict::Apply,
      (None, _) => Verdict::Conflict(ConflictClass::DeleteModify),
    },
    Action::Clear | Action::Rmdir => removed_dir(action, witnessed, disk),
    Action::Rename { .. } => renamed(witnessed, disk),
  }
}

/// The verdict for a directory rename: `disk` is the origin, which must still be the witnessed
/// directory; an origin that is gone while the destination holds the witnessed directory is a
/// rename already made (a resumed landing).
fn renamed(witnessed: Option<&Witness>, disk: DiskState) -> Verdict {
  match (witnessed, disk) {
    (Some(w), DiskState::Dir { fingerprint, .. }) if fingerprint.ino == w.fingerprint.ino => {
      Verdict::Apply
    }
    (Some(w), DiskState::AtDestination(fp)) if fp.ino == w.fingerprint.ino => Verdict::Skip,
    (_, DiskState::Absent | DiskState::Dir { .. } | DiskState::AtDestination(_)) => {
      Verdict::Conflict(ConflictClass::RenameRename)
    }
    (_, DiskState::File { .. } | DiskState::Symlink) => {
      Verdict::Conflict(ConflictClass::TypeChanged)
    }
  }
}

/// The verdict for a directory the overlay removed (`Rmdir`) or removed and recreated
/// (`Clear`): the disk's directory must still be the witnessed inode; a clear of an absent
/// directory recreates it; a clear that finds a directory holding only what this manifest
/// creates beneath it is already the new state (a resumed clear meets its own fresh
/// directory).
fn removed_dir(action: &Action, witnessed: Option<&Witness>, disk: DiskState) -> Verdict {
  match (action, witnessed, disk) {
    (Action::Clear, _, DiskState::Absent) => Verdict::Apply,
    (_, _, DiskState::Absent) => Verdict::Skip,
    (_, Some(w), DiskState::Dir { fingerprint, .. }) if fingerprint.ino == w.fingerprint.ino => {
      Verdict::Apply
    }
    (
      Action::Clear,
      _,
      DiskState::Dir {
        holds_only_ours: true,
        ..
      },
    ) => Verdict::Skip,
    (_, _, DiskState::Dir { .. } | DiskState::AtDestination(_)) => {
      Verdict::Conflict(ConflictClass::DeleteModify)
    }
    (_, _, DiskState::File { .. } | DiskState::Symlink) => {
      Verdict::Conflict(ConflictClass::TypeChanged)
    }
  }
}

fn created(action: &Action, disk: DiskState, overlay: Option<&OverlayIdentity>) -> Verdict {
  match (action, disk) {
    (_, DiskState::Absent) => Verdict::Apply,
    (Action::Symlink { .. }, DiskState::Symlink) => Verdict::Skip,
    (Action::Symlink { .. }, _) => Verdict::Conflict(ConflictClass::TypeChanged),
    (_, DiskState::Dir { .. } | DiskState::Symlink | DiskState::AtDestination(_)) => {
      Verdict::Conflict(ConflictClass::TypeChanged)
    }
    (_, DiskState::File { identity, .. }) => match (identity, overlay) {
      (Some(id), Some(o)) if id == o.hash => Verdict::Skip,
      _ => Verdict::Conflict(ConflictClass::CreateCreate),
    },
  }
}

fn replaced(
  witnessed: Option<&Witness>,
  disk: DiskState,
  overlay: Option<&OverlayIdentity>,
) -> Verdict {
  let Some(w) = witnessed else {
    return created(&Action::Create, disk, overlay);
  };
  match disk {
    DiskState::Absent => Verdict::Conflict(ConflictClass::ModifyDelete),
    DiskState::Dir { .. } | DiskState::Symlink | DiskState::AtDestination(_) => {
      Verdict::Conflict(ConflictClass::TypeChanged)
    }
    DiskState::File {
      fingerprint,
      identity,
    } => {
      let unchanged =
        fingerprint == w.fingerprint && (!w.racy || identity.is_none_or(|id| id == w.identity));
      let overlay_changed = overlay.is_some_and(|o| o.hash != w.identity);
      match (unchanged, overlay_changed) {
        (true, true) => Verdict::Apply,
        (true, false) => Verdict::Skip,
        (false, false) => Verdict::Skip,
        (false, true) => match (identity, overlay) {
          (Some(id), Some(o)) if id == o.hash => Verdict::AcceptIdentical,
          _ => Verdict::Conflict(ConflictClass::ModifyModify),
        },
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fp(ino: u64, mtime_ns: i64) -> Fingerprint {
    Fingerprint {
      dev: 1,
      ino,
      size: 3,
      mtime_ns,
      ctime_ns: mtime_ns,
      mode: 0o100_644,
    }
  }

  fn dir_fp(ino: u64) -> Fingerprint {
    Fingerprint {
      mode: 0o040_755,
      ..fp(ino, 3)
    }
  }

  fn witness(ino: u64, identity: [u8; 32]) -> Witness {
    Witness {
      fingerprint: fp(ino, 3),
      identity,
      witnessed_at: 10,
      racy: false,
    }
  }

  fn overlay(hash: [u8; 32]) -> OverlayIdentity {
    OverlayIdentity {
      hash,
      size: 3,
      mode: 0o644,
      mtime_ns: 20,
    }
  }

  fn dir(fingerprint: Fingerprint) -> DiskState {
    DiskState::Dir {
      fingerprint,
      holds_only_ours: false,
    }
  }

  fn file(fingerprint: Fingerprint, identity: Option<[u8; 32]>) -> DiskState {
    DiskState::File {
      fingerprint,
      identity,
    }
  }

  /// One row: the action, whether a witness exists, the disk, the overlay, the expected verdict.
  type Row = (
    Action,
    Option<Witness>,
    DiskState,
    Option<OverlayIdentity>,
    Verdict,
  );

  /// AC-1.12: every row of §4.15's table and every conflict class, as the function decides.
  fn rows() -> Vec<Row> {
    let base_id = [1u8; 32];
    let new_id = [2u8; 32];
    let other_id = [3u8; 32];
    let w = witness(7, base_id);
    let same = file(fp(7, 3), None);
    let moved_new = file(fp(7, 4), Some(new_id));
    let moved_other = file(fp(7, 5), Some(other_id));
    let rename = Action::Rename { from: "/a".into() };
    let link = Action::Symlink { target: "t".into() };
    vec![
      // unchanged / changed → Apply
      (
        Action::Replace,
        Some(w),
        same,
        Some(overlay(new_id)),
        Verdict::Apply,
      ),
      // unchanged / unchanged → Skip (drift, nothing to land)
      (
        Action::Replace,
        Some(w),
        same,
        Some(overlay(base_id)),
        Verdict::Skip,
      ),
      // changed on disk / unchanged in the overlay → Skip
      (
        Action::Replace,
        Some(w),
        moved_other,
        Some(overlay(base_id)),
        Verdict::Skip,
      ),
      // changed to the same bytes / changed → AcceptIdentical
      (
        Action::Replace,
        Some(w),
        moved_new,
        Some(overlay(new_id)),
        Verdict::AcceptIdentical,
      ),
      // changed differently / changed → ModifyModify
      (
        Action::Replace,
        Some(w),
        moved_other,
        Some(overlay(new_id)),
        Verdict::Conflict(ConflictClass::ModifyModify),
      ),
      // deleted on disk / changed → ModifyDelete
      (
        Action::Replace,
        Some(w),
        DiskState::Absent,
        Some(overlay(new_id)),
        Verdict::Conflict(ConflictClass::ModifyDelete),
      ),
      // changed on disk / whiteout → DeleteModify
      (
        Action::Delete,
        Some(w),
        moved_other,
        None,
        Verdict::Conflict(ConflictClass::DeleteModify),
      ),
      // unchanged / whiteout → Apply
      (Action::Delete, Some(w), same, None, Verdict::Apply),
      // already gone / whiteout → Skip
      (
        Action::Delete,
        Some(w),
        DiskState::Absent,
        None,
        Verdict::Skip,
      ),
      // origin gone / redirect → RenameRename
      (
        rename.clone(),
        Some(witness(9, base_id)),
        DiskState::Absent,
        None,
        Verdict::Conflict(ConflictClass::RenameRename),
      ),
      // origin in place / redirect → Apply
      (
        rename,
        Some(witness(9, base_id)),
        dir(dir_fp(9)),
        None,
        Verdict::Apply,
      ),
      // origin gone, the destination holds the witnessed directory / redirect → Skip (resumed)
      (
        Action::Rename { from: "/a".into() },
        Some(witness(9, base_id)),
        DiskState::AtDestination(dir_fp(9)),
        None,
        Verdict::Skip,
      ),
      (
        Action::Rename { from: "/a".into() },
        Some(witness(9, base_id)),
        DiskState::AtDestination(dir_fp(10)),
        None,
        Verdict::Conflict(ConflictClass::RenameRename),
      ),
      // no witness; disk has different bytes / created → CreateCreate
      (
        Action::Create,
        None,
        moved_other,
        Some(overlay(new_id)),
        Verdict::Conflict(ConflictClass::CreateCreate),
      ),
      // no witness; disk has the same bytes / created → Skip
      (
        Action::Create,
        None,
        moved_new,
        Some(overlay(new_id)),
        Verdict::Skip,
      ),
      // nothing on disk / created → Apply
      (
        Action::Create,
        None,
        DiskState::Absent,
        Some(overlay(new_id)),
        Verdict::Apply,
      ),
      // file became a directory → TypeChanged
      (
        Action::Replace,
        Some(w),
        dir(dir_fp(7)),
        Some(overlay(new_id)),
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        Action::Mkdir,
        None,
        same,
        None,
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (Action::Mkdir, None, dir(dir_fp(11)), None, Verdict::Skip),
      (Action::Mkdir, None, DiskState::Absent, None, Verdict::Apply),
      (
        Action::Rmdir,
        Some(witness(9, base_id)),
        dir(dir_fp(9)),
        None,
        Verdict::Apply,
      ),
      (
        Action::Rmdir,
        Some(witness(9, base_id)),
        dir(dir_fp(10)),
        None,
        Verdict::Conflict(ConflictClass::DeleteModify),
      ),
      (
        Action::Rmdir,
        Some(witness(9, base_id)),
        same,
        None,
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        Action::Clear,
        Some(witness(9, base_id)),
        DiskState::Absent,
        None,
        Verdict::Apply,
      ),
      (
        Action::Clear,
        Some(witness(9, base_id)),
        dir(dir_fp(9)),
        None,
        Verdict::Apply,
      ),
      (
        Action::Clear,
        Some(witness(9, base_id)),
        dir(dir_fp(10)),
        None,
        Verdict::Conflict(ConflictClass::DeleteModify),
      ),
      (
        Action::Clear,
        Some(witness(9, base_id)),
        same,
        None,
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        Action::Clear,
        Some(witness(9, base_id)),
        DiskState::Dir {
          fingerprint: dir_fp(10),
          holds_only_ours: true,
        },
        None,
        Verdict::Skip,
      ),
      (
        Action::Rmdir,
        Some(witness(9, base_id)),
        DiskState::Dir {
          fingerprint: dir_fp(10),
          holds_only_ours: true,
        },
        None,
        Verdict::Conflict(ConflictClass::DeleteModify),
      ),
      (link.clone(), None, DiskState::Absent, None, Verdict::Apply),
      (link.clone(), None, DiskState::Symlink, None, Verdict::Skip),
      (
        link,
        None,
        same,
        None,
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
    ]
  }

  #[test]
  fn the_table() {
    for (action, witnessed, disk, overlay, expected) in rows() {
      let got = verdict(&action, witnessed.as_ref(), disk, overlay.as_ref());
      assert_eq!(
        got, expected,
        "{action:?} witnessed={witnessed:?} disk={disk:?}"
      );
    }
  }

  /// A racy witness (fingerprint inside the timestamp window) is trusted only with the hash.
  #[test]
  fn a_racy_witness_needs_the_hash() {
    let base_id = [1u8; 32];
    let new_id = [2u8; 32];
    let racy = Witness {
      racy: true,
      ..witness(7, base_id)
    };
    let same_fp_other_bytes = file(fp(7, 3), Some([3u8; 32]));
    assert_eq!(
      verdict(
        &Action::Replace,
        Some(&racy),
        same_fp_other_bytes,
        Some(&overlay(new_id))
      ),
      Verdict::Conflict(ConflictClass::ModifyModify)
    );
    let same_fp_same_bytes = file(fp(7, 3), Some(base_id));
    assert_eq!(
      verdict(
        &Action::Replace,
        Some(&racy),
        same_fp_same_bytes,
        Some(&overlay(new_id))
      ),
      Verdict::Apply
    );
  }
}
