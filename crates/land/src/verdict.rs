//! The per-entry verdict (§4.15 step 4; AC-1.12): a pure function of three inputs, the
//! witnessed base, the disk now, and the overlay; the table is the specification and the test
//! enumerates it. No verdict ever writes; a conflict refuses the whole landing.
//!
//! An entry of a captured clone's landing is judged by content alone ([`content_verdict`]; §4.15 `capture_base`,
//! A-291): the witness is the identity of what the capture held, and the disk's bytes are what is compared with it.

use slates_vfs::inode::{Fingerprint, Witness};

use crate::manifest::{Action, FileMeta, OverlayIdentity};

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
    Action::Chmod { from, to } => chmod_verdict(*from, *to, disk, modified(witnessed, overlay)),
    Action::ChmodDir { from, to } => chmod_dir_verdict(*from, *to, disk),
  }
}

/// A file's modification time as its witness found it on the disk and as the volume holds it (A-501): the time field an
/// overlay's metadata entry judges; none without either (a captured clone's entry, whose capture read another folder,
/// judges no time).
fn modified(witnessed: Option<&Witness>, overlay: Option<&OverlayIdentity>) -> Option<(i64, i64)> {
  Some((witnessed?.fingerprint.mtime_ns, overlay?.mtime_ns))
}

/// What landing a file's metadata does to a disk file holding `disk` (A-458): each field — the permission bits, the
/// Windows bits — judged on its own by the rule §4.15 step 4 holds bytes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaVerdict {
  /// Set the fields named: each one the volume changed and the disk still holds as the base did. A field not named
  /// stays as the disk holds it.
  Set {
    /// The permission bits to set.
    permissions: Option<u32>,
    /// The Windows bits to set.
    windows: Option<u16>,
    /// The modification time to set (A-501: an overlay's file; nothing else judges a time).
    modified: Option<i64>,
  },
  /// Nothing to set: the disk already holds every field the volume changed.
  Held,
  /// A field the volume changed holds a third value on the disk, neither the base's nor the volume's.
  Conflict,
}

/// One field of a file's metadata, judged (A-458): the volume's value where the disk still holds the base's; nothing
/// where the volume left the base's value (the disk's stands, as a change the disk made to bytes the volume left does)
/// or where the disk already holds the volume's; a conflict where the disk holds a third. Never a value made of two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field<T> {
  /// Nothing to set.
  Kept,
  /// The volume's value, set.
  Set(T),
  /// The disk holds a third value.
  Conflict,
}

impl<T: Copy> Field<T> {
  fn set(self) -> Option<T> {
    match self {
      Field::Set(value) => Some(value),
      Field::Kept | Field::Conflict => None,
    }
  }
}

fn field<T: PartialEq + Copy>(base: T, disk: T, volume: T) -> Field<T> {
  if volume == base || disk == volume {
    Field::Kept
  } else if disk == base {
    Field::Set(volume)
  } else {
    Field::Conflict
  }
}

/// The metadata verdict (A-458) of a file the volume changed from `from` to `to`, against the disk's `disk`: a conflict
/// in any field is the file's, and otherwise each field the disk still holds as the base did is set.
pub fn meta_verdict(from: FileMeta, disk: FileMeta, to: FileMeta) -> MetaVerdict {
  let permissions = field(from.permissions, disk.permissions, to.permissions);
  let windows = field(from.windows, disk.windows, to.windows);
  match (permissions, windows) {
    (Field::Conflict, _) | (_, Field::Conflict) => MetaVerdict::Conflict,
    (Field::Kept, Field::Kept) => MetaVerdict::Held,
    (permissions, windows) => MetaVerdict::Set {
      permissions: permissions.set(),
      windows: windows.set(),
      modified: None,
    },
  }
}

/// [`meta_verdict`] with a file's modification time judged beside its other fields by the same rule (A-501): `base`,
/// `disk` and `volume` its time as the witness found it, as the disk holds it now and as the volume holds it.
pub fn with_modified(meta: MetaVerdict, base: i64, disk: i64, volume: i64) -> MetaVerdict {
  match (meta, field(base, disk, volume)) {
    (MetaVerdict::Conflict, _) | (_, Field::Conflict) => MetaVerdict::Conflict,
    (MetaVerdict::Held, Field::Kept) => MetaVerdict::Held,
    (MetaVerdict::Held, Field::Set(time)) => MetaVerdict::Set {
      permissions: None,
      windows: None,
      modified: Some(time),
    },
    (
      MetaVerdict::Set {
        permissions,
        windows,
        ..
      },
      time,
    ) => MetaVerdict::Set {
      permissions,
      windows,
      modified: time.set(),
    },
  }
}

/// The metadata a replaced file lands with (A-466), each field by [`meta_verdict`]'s rule: the volume's value where it
/// changed the field and the disk still holds the base's; the disk's where the volume left the field as the base had it
/// (an outsider's change kept, as bytes the volume did not change are) or already holds the volume's. None where a
/// field the volume changed holds a third value on the disk: the file's conflict. A captured clone's replacement is
/// staged with it, its bytes judged by their own content verdict; never a value made of two (D-27).
pub fn landed_meta(from: FileMeta, disk: FileMeta, to: FileMeta) -> Option<FileMeta> {
  let permissions = field(from.permissions, disk.permissions, to.permissions);
  let windows = field(from.windows, disk.windows, to.windows);
  match (permissions, windows) {
    (Field::Conflict, _) | (_, Field::Conflict) => None,
    (permissions, windows) => Some(FileMeta {
      permissions: permissions.set().unwrap_or(disk.permissions),
      windows: windows.set().unwrap_or(disk.windows),
    }),
  }
}

/// The verdict for a file whose metadata alone the volume changed (`Action::Chmod`, A-458): no file is the disk's
/// removal (`ModifyDelete`) and anything else a type change; a file is judged field by field ([`meta_verdict`]). Its
/// bytes are the disk's, whatever they are now, since the volume kept the base's, so the file is judged by its
/// metadata alone, whichever object holds the name; an overlay's entry and a captured clone's are judged alike.
fn chmod_verdict(
  from: FileMeta,
  to: FileMeta,
  disk: DiskState,
  modified: Option<(i64, i64)>,
) -> Verdict {
  match disk {
    DiskState::Absent => Verdict::Conflict(ConflictClass::ModifyDelete),
    DiskState::Dir { .. } | DiskState::Symlink | DiskState::AtDestination(_) => {
      Verdict::Conflict(ConflictClass::TypeChanged)
    }
    DiskState::File { fingerprint, .. } => {
      let meta = meta_verdict(from, FileMeta::of_fingerprint(&fingerprint), to);
      let meta = match modified {
        Some((base, volume)) => with_modified(meta, base, fingerprint.mtime_ns, volume),
        None => meta,
      };
      let unchanged = from == to && modified.is_none_or(|(base, volume)| base == volume);
      meta_outcome(meta, unchanged)
    }
  }
}

/// The verdict for a base directory whose metadata alone the volume changed (`Action::ChmodDir`, A-501): judged as a
/// file's metadata entry is, field by field, against a directory; no directory is the disk's removal (`ModifyDelete`)
/// and anything else a type change. Its time is no field: a directory's moves with every entry made or removed in it.
fn chmod_dir_verdict(from: FileMeta, to: FileMeta, disk: DiskState) -> Verdict {
  match disk {
    DiskState::Absent => Verdict::Conflict(ConflictClass::ModifyDelete),
    DiskState::File { .. } | DiskState::Symlink | DiskState::AtDestination(_) => {
      Verdict::Conflict(ConflictClass::TypeChanged)
    }
    DiskState::Dir { fingerprint, .. } => meta_outcome(
      meta_verdict(from, FileMeta::of_dir_fingerprint(&fingerprint), to),
      from == to,
    ),
  }
}

/// A metadata entry's verdict from its fields' (A-458): applied where a field is to be set, identical where the disk
/// holds every change already, a conflict where a field is held a third way; skipped where the volume changed nothing.
fn meta_outcome(meta: MetaVerdict, unchanged: bool) -> Verdict {
  match meta {
    MetaVerdict::Set { .. } => Verdict::Apply,
    MetaVerdict::Held if unchanged => Verdict::Skip,
    MetaVerdict::Held => Verdict::AcceptIdentical,
    MetaVerdict::Conflict => Verdict::Conflict(ConflictClass::ModifyModify),
  }
}

/// The verdict for an entry judged by content alone ([`crate::manifest::Judged::Content`]; §4.15 `capture_base`,
/// A-291's landing): the witness's identity is the BLAKE3 of what a capture held there, its fingerprint is ignored (the
/// target is another folder than the one the capture read), and the disk's bytes are hashed for every entry with a
/// witness. A file is overwritten only while the disk still holds the captured bytes (or accepted when it already holds
/// the volume's), removed only while it still holds them, and a directory is removed only once nothing is left in it
/// that this landing does not remove itself (`holds_only_ours` over the manifest's removals), never with what an
/// outsider put there. Creations hold no witness and are judged as the witness table judges them; a content manifest
/// makes no clear and no rename (a moved directory is its paths removed and made).
pub fn content_verdict(
  action: &Action,
  witnessed: Option<&Witness>,
  disk: DiskState,
  overlay: Option<&OverlayIdentity>,
) -> Verdict {
  match action {
    Action::Replace => content_replaced(witnessed, disk, overlay),
    Action::Delete => match (witnessed, disk) {
      (_, DiskState::Absent) => Verdict::Skip,
      (
        Some(w),
        DiskState::File {
          identity: Some(id), ..
        },
      ) if id == w.identity => Verdict::Apply,
      (_, DiskState::File { .. }) => Verdict::Conflict(ConflictClass::DeleteModify),
      (_, DiskState::Symlink) => Verdict::Apply,
      (_, DiskState::Dir { .. } | DiskState::AtDestination(_)) => {
        Verdict::Conflict(ConflictClass::TypeChanged)
      }
    },
    Action::Rmdir => match disk {
      DiskState::Absent => Verdict::Skip,
      DiskState::Dir {
        holds_only_ours: true,
        ..
      } => Verdict::Apply,
      DiskState::Dir { .. } | DiskState::AtDestination(_) => {
        Verdict::Conflict(ConflictClass::DeleteModify)
      }
      DiskState::File { .. } | DiskState::Symlink => Verdict::Conflict(ConflictClass::TypeChanged),
    },
    Action::Create
    | Action::Symlink { .. }
    | Action::Mkdir
    | Action::Clear
    | Action::Rename { .. }
    | Action::ChmodDir { .. } => verdict(action, witnessed, disk, overlay),
    // A captured clone's file metadata entry judges no time: its capture read another folder (A-501).
    Action::Chmod { from, to } => chmod_verdict(*from, *to, disk, None),
  }
}

/// A file the capture held, which the volume changed: written while the disk still holds the captured bytes.
fn content_replaced(
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
    DiskState::File { identity, .. } => {
      let ours = overlay.map(|o| o.hash);
      if ours == Some(w.identity) {
        return Verdict::Skip;
      }
      match identity {
        Some(id) if id == w.identity => Verdict::Apply,
        Some(id) if Some(id) == ours => Verdict::AcceptIdentical,
        _ => Verdict::Conflict(ConflictClass::ModifyModify),
      }
    }
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
      uid: 0,
      gid: 0,
      birth_ns: 1,
      windows: 0,
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

  /// AC-1.12 for a captured clone's landing (A-291): each row of the content table. The fingerprints differ from the
  /// witness's in every row (the target is another folder than the capture read), so only the bytes decide: an
  /// untouched target is written, one already holding the volume's bytes is accepted, one an outsider changed is a
  /// conflict, a deletion is carried only over the captured bytes, and a directory is removed only once emptied.
  #[test]
  fn the_content_table() {
    let captured = [1u8; 32];
    let ours = [2u8; 32];
    let outsider = [3u8; 32];
    let w = Witness {
      fingerprint: Fingerprint::default(),
      ..witness(7, captured)
    };
    let disk = |id: [u8; 32]| file(fp(99, 50), Some(id));
    let rows: Vec<Row> = vec![
      (
        Action::Replace,
        Some(w),
        disk(captured),
        Some(overlay(ours)),
        Verdict::Apply,
      ),
      (
        Action::Replace,
        Some(w),
        disk(ours),
        Some(overlay(ours)),
        Verdict::AcceptIdentical,
      ),
      (
        Action::Replace,
        Some(w),
        disk(outsider),
        Some(overlay(ours)),
        Verdict::Conflict(ConflictClass::ModifyModify),
      ),
      (
        Action::Replace,
        Some(w),
        file(fp(99, 50), None),
        Some(overlay(ours)),
        Verdict::Conflict(ConflictClass::ModifyModify),
      ),
      (
        Action::Replace,
        Some(w),
        DiskState::Absent,
        Some(overlay(ours)),
        Verdict::Conflict(ConflictClass::ModifyDelete),
      ),
      (
        Action::Replace,
        Some(w),
        dir(dir_fp(99)),
        Some(overlay(ours)),
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        Action::Replace,
        Some(w),
        disk(outsider),
        Some(overlay(captured)),
        Verdict::Skip,
      ),
      (
        Action::Delete,
        Some(w),
        disk(captured),
        None,
        Verdict::Apply,
      ),
      (
        Action::Delete,
        Some(w),
        disk(outsider),
        None,
        Verdict::Conflict(ConflictClass::DeleteModify),
      ),
      (
        Action::Delete,
        Some(w),
        DiskState::Absent,
        None,
        Verdict::Skip,
      ),
      (
        Action::Delete,
        Some(w),
        dir(dir_fp(99)),
        None,
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        Action::Rmdir,
        Some(w),
        DiskState::Absent,
        None,
        Verdict::Skip,
      ),
      (
        Action::Rmdir,
        Some(w),
        DiskState::Dir {
          fingerprint: dir_fp(99),
          holds_only_ours: true,
        },
        None,
        Verdict::Apply,
      ),
      (
        Action::Rmdir,
        Some(w),
        dir(dir_fp(99)),
        None,
        Verdict::Conflict(ConflictClass::DeleteModify),
      ),
      (
        Action::Rmdir,
        Some(w),
        disk(captured),
        None,
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        Action::Create,
        None,
        DiskState::Absent,
        Some(overlay(ours)),
        Verdict::Apply,
      ),
      (
        Action::Create,
        None,
        disk(ours),
        Some(overlay(ours)),
        Verdict::Skip,
      ),
      (
        Action::Create,
        None,
        disk(outsider),
        Some(overlay(ours)),
        Verdict::Conflict(ConflictClass::CreateCreate),
      ),
      (Action::Mkdir, None, DiskState::Absent, None, Verdict::Apply),
    ];
    for (action, witnessed, disk, overlay, expected) in rows {
      let got = content_verdict(&action, witnessed.as_ref(), disk, overlay.as_ref());
      assert_eq!(
        got, expected,
        "{action:?} disk={disk:?} overlay={overlay:?}"
      );
    }
    // The witness table would have taken the moved fingerprint for an outsider's change: the same untouched target
    // is a conflict there, which is why a captured clone's entries are judged by content.
    assert_eq!(
      verdict(
        &Action::Replace,
        Some(&w),
        disk(captured),
        Some(&overlay(ours))
      ),
      Verdict::Conflict(ConflictClass::ModifyModify)
    );
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

  /// A-458 (§4.15 step 4's rule, held field by field for a file's metadata). Do: judge a file whose mode the volume
  /// changed from 0644 to 0755 against a disk that kept 0644, already holds 0755, or holds 0600; one whose Windows bits
  /// alone the volume changed against a disk whose mode an outsider changed; and the volume's change against a missing
  /// file, a directory and a symlink. Expect: set where the disk kept the base's, identical where it holds the
  /// volume's, a conflict where it holds a third; the bits set and the outsider's mode kept; and the disk's removal and
  /// a type change as conflicts. The same rule judges a captured clone's entry.
  #[test]
  fn a_files_metadata_is_judged_field_by_field() {
    /// Format: `FILE_ATTRIBUTE_HIDDEN`.
    const HIDDEN: u16 = 0x2;
    let meta = |permissions: u32, windows: u16| FileMeta {
      permissions,
      windows,
    };
    let on_disk = |permissions: u32, windows: u16| {
      file(
        Fingerprint {
          mode: 0o100_000 | permissions,
          windows,
          ..fp(7, 3)
        },
        None,
      )
    };
    let executable = Action::Chmod {
      from: meta(0o644, 0),
      to: meta(0o755, 0),
    };
    let hidden = Action::Chmod {
      from: meta(0o644, 0),
      to: meta(0o644, HIDDEN),
    };
    let rows = [
      (&executable, on_disk(0o644, 0), Verdict::Apply),
      (&executable, on_disk(0o755, 0), Verdict::AcceptIdentical),
      (
        &executable,
        on_disk(0o600, 0),
        Verdict::Conflict(ConflictClass::ModifyModify),
      ),
      (&hidden, on_disk(0o600, 0), Verdict::Apply),
      (&hidden, on_disk(0o600, HIDDEN), Verdict::AcceptIdentical),
      (
        &executable,
        DiskState::Absent,
        Verdict::Conflict(ConflictClass::ModifyDelete),
      ),
      (
        &executable,
        DiskState::Dir {
          fingerprint: dir_fp(7),
          holds_only_ours: false,
        },
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        &executable,
        DiskState::Symlink,
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
    ];
    for (action, disk, expected) in rows {
      assert_eq!(
        verdict(action, None, disk, None),
        expected,
        "{action:?} {disk:?}"
      );
      assert_eq!(
        content_verdict(action, None, disk, None),
        expected,
        "judged by content: {action:?} {disk:?}"
      );
    }
    assert_eq!(
      meta_verdict(meta(0o644, 0), meta(0o600, 0), meta(0o644, HIDDEN)),
      MetaVerdict::Set {
        permissions: None,
        windows: Some(HIDDEN),
        modified: None,
      },
      "the bits set, the outsider's mode left"
    );
    assert_eq!(
      meta_verdict(meta(0o644, 0), meta(0o644, 0), meta(0o755, HIDDEN)),
      MetaVerdict::Set {
        permissions: Some(0o755),
        windows: Some(HIDDEN),
        modified: None,
      }
    );
    assert_eq!(
      meta_verdict(meta(0o644, 0), meta(0o755, HIDDEN), meta(0o755, 0)),
      MetaVerdict::Held,
      "the volume's mode already held, and the bits an outsider set, which the volume never changed, left as theirs"
    );
  }

  /// A-501 (a `touch` alone, a directory's mode alone). Do: judge a file whose time alone the volume changed against a
  /// disk that kept the witnessed time, already holds the volume's, or holds a third; the same entry judged by content,
  /// which judges no time; and a directory whose mode the volume changed against a directory that kept it, already holds
  /// it, holds a third, and against a file. Expect the time set, identical or a conflict by the same rule, no time judged
  /// for a captured clone's entry, and the directory's mode judged as a file's is, a file in its place a type change.
  #[test]
  fn a_files_time_and_a_directorys_mode_are_judged_by_field() {
    /// Shape: a file's times: as its witness found it, as the volume holds it, and an outsider's.
    const WITNESSED_NS: i64 = 3;
    /// Shape: see `WITNESSED_NS`.
    const TOUCHED_NS: i64 = 30;
    /// Shape: see `WITNESSED_NS`.
    const OUTSIDER_NS: i64 = 40;
    let meta = FileMeta {
      permissions: 0o644,
      windows: 0,
    };
    let touch = Action::Chmod {
      from: meta,
      to: meta,
    };
    let witnessed = witness(7, [1; 32]);
    let touched = OverlayIdentity {
      mtime_ns: TOUCHED_NS,
      ..overlay([1; 32])
    };
    let at = |mtime_ns: i64| file(Fingerprint { mtime_ns, ..fp(7, 3) }, None);
    assert_eq!(witnessed.fingerprint.mtime_ns, WITNESSED_NS);
    let judged = |disk| verdict(&touch, Some(&witnessed), disk, Some(&touched));
    assert_eq!(judged(at(WITNESSED_NS)), Verdict::Apply);
    assert_eq!(judged(at(TOUCHED_NS)), Verdict::AcceptIdentical);
    assert_eq!(
      judged(at(OUTSIDER_NS)),
      Verdict::Conflict(ConflictClass::ModifyModify)
    );
    assert_eq!(
      content_verdict(&touch, Some(&witnessed), at(OUTSIDER_NS), Some(&touched)),
      Verdict::Skip,
      "a captured clone's entry judges no time"
    );
    let private = Action::ChmodDir {
      from: FileMeta {
        permissions: 0o755,
        windows: 0,
      },
      to: FileMeta {
        permissions: 0o700,
        windows: 0,
      },
    };
    let directory = |mode: u32| DiskState::Dir {
      fingerprint: Fingerprint {
        mode,
        ..dir_fp(7)
      },
      holds_only_ours: false,
    };
    let rows = [
      (directory(0o040_755), Verdict::Apply),
      (directory(0o040_700), Verdict::AcceptIdentical),
      (
        directory(0o040_750),
        Verdict::Conflict(ConflictClass::ModifyModify),
      ),
      (
        at(WITNESSED_NS),
        Verdict::Conflict(ConflictClass::TypeChanged),
      ),
      (
        DiskState::Absent,
        Verdict::Conflict(ConflictClass::ModifyDelete),
      ),
    ];
    for (disk, expected) in rows {
      assert_eq!(verdict(&private, None, disk, None), expected, "{disk:?}");
    }
  }
}
