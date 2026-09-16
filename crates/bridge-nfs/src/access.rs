//! POSIX discretionary access control for the NFS export (§4.6 "POSIX and transparency acceptance";
//! `docs/wip/EQUIVALENCE.md` §8: a mounted filesystem may not ignore ownership or modes). The FUSE
//! mount gets these checks from the kernel (`default_permissions`, `crates/bridge-fuse/src/mount.rs`);
//! an NFS server is the one bridge that must make them itself, because its client checks nothing but
//! what `ACCESS` reports and the server sees every operation. Before 2026-09-15 the export answered
//! `ACCESS` from the owner's permission bits alone and enforced nothing on any other procedure, so a
//! caller other than the owner could read, write, remove and `chmod` whatever it could name — 5,336 of
//! pjdfstest's 8,686 cases failed on the macOS lane for exactly this (`docs/bugs/2026-09-15-nfs-export-
//! enforces-no-posix-permissions.md`).
//!
//! This module is the pure decision logic: no I/O, no transport types, unit-tested on every host. The
//! rules are POSIX.1-2017 §4.5 "File Access Permissions" and the `chown`/`chmod`/`unlink`/`rename`
//! ownership clauses, with the superuser exemptions every Unix applies:
//!
//! - **The class rule.** Exactly one class of permission bits applies to a caller — the owner class if
//!   the caller owns the object, else the group class if the object's group is the caller's primary or
//!   any supplementary group, else the other class. A caller in the owner class is judged by the owner
//!   bits even when the group or other bits would allow more.
//! - **The superuser.** uid 0 may read, write and search anything; it may *execute* a file only if some
//!   execute bit is set (Linux and the BSDs alike).
//! - **Ownership operations.** `chmod` needs ownership or the superuser. `chown` is restricted
//!   (`_POSIX_CHOWN_RESTRICTED`): only the superuser changes an owner; the owner may change the group to
//!   one of the caller's own groups. Setting explicit times needs ownership; setting them to "now" needs
//!   ownership or write permission.
//! - **The sticky bit.** In a directory with `S_ISVTX`, an entry is removed or renamed only by the
//!   entry's owner, the directory's owner, or the superuser.
//! - **Set-id hygiene.** A non-superuser's write or `chown` clears the set-user-id and set-group-id
//!   bits of a file, and a non-superuser's `chmod` cannot set `S_ISGID` on an object whose group is not
//!   one of the caller's.
//!
//! **The owner override for I/O** ([`permits_io`]). An NFS client checks `open(2)` against `ACCESS`
//! and sends `READ`/`WRITE` only after a successful open; a file opened for writing and then
//! `chmod 0444` must keep accepting the writes of its open descriptor, which a stateless server cannot
//! tell from a fresh open. Linux `nfsd` resolves this by letting the *owner* read and write regardless
//! of the mode bits (`NFSD_MAY_OWNER_OVERRIDE`); this export does the same for `READ`, `WRITE` and a
//! `SETATTR` of the size, while `ACCESS` reports the exact class verdict so the client's own `open`
//! check is right.

use slates_bridge_core::{NodeAttr, SetAttr};
use slates_vfs::inode::Kind;

/// Format: the superuser's uid (POSIX: "appropriate privileges" is uid 0 on every Unix here).
pub const ROOT_UID: u32 = 0;
/// Format: `(uid_t)-1`, the sentinel no real user holds — the uid of a caller with no Unix identity
/// (a `Principal` that is not a uid), which owns nothing and belongs to no group.
pub const INVALID_UID: u32 = u32::MAX;
/// Format: `S_ISUID`, the set-user-id bit of `st_mode`.
pub const S_ISUID: u32 = 0o4000;
/// Format: `S_ISGID`, the set-group-id bit of `st_mode`.
pub const S_ISGID: u32 = 0o2000;
/// Format: `S_ISVTX`, the sticky bit of `st_mode`.
pub const S_ISVTX: u32 = 0o1000;
/// Format: the read bit of the "other" class; the group and owner classes are this shifted by
/// [`GROUP_SHIFT`] and [`OWNER_SHIFT`].
const OTHER_READ: u32 = 0o4;
/// Format: the write bit of the "other" class.
const OTHER_WRITE: u32 = 0o2;
/// Format: the execute (search) bit of the "other" class.
const OTHER_EXECUTE: u32 = 0o1;
/// Format: the bit shift from the other class to the group class of `st_mode`.
const GROUP_SHIFT: u32 = 3;
/// Format: the bit shift from the other class to the owner class of `st_mode`.
const OWNER_SHIFT: u32 = 6;
/// Format: every execute bit of `st_mode` — what the superuser needs at least one of to execute a file.
const ANY_EXECUTE: u32 =
  OTHER_EXECUTE | (OTHER_EXECUTE << GROUP_SHIFT) | (OTHER_EXECUTE << OWNER_SHIFT);

/// The groups a Unix caller belongs to: its primary group and its supplementary groups (bounded by
/// the `AUTH_SYS` credential's `gids<16>`, [`crate::rpc::AUTH_SYS_MAX_GIDS`]). A created object takes
/// the primary group; the group class of a permission check matches any of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixGroups {
  /// The primary group.
  pub gid: u32,
  /// The supplementary groups.
  pub supplementary: Vec<u32>,
}

/// The Unix identity an `AUTH_SYS` credential names: the uid a request runs as, and its groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixIdentity {
  /// The user id.
  pub uid: u32,
  /// The primary and supplementary groups.
  pub groups: UnixGroups,
}

/// The caller of a request, as the access rules see it: a uid and the groups it belongs to. A caller
/// with no Unix identity ([`INVALID_UID`], no groups) is judged by the other class of every object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
  /// The user id.
  pub uid: u32,
  /// The groups, or `None` for a caller whose credential named none.
  pub groups: Option<UnixGroups>,
}

impl Caller {
  /// The superuser, belonging to no group — what a request with no credential (`AUTH_NONE`) runs as
  /// on a loopback mount (§4.13), and what a test's default export runs as.
  pub fn root() -> Caller {
    Caller {
      uid: ROOT_UID,
      groups: None,
    }
  }

  /// Whether the caller is the superuser.
  pub fn is_root(&self) -> bool {
    self.uid == ROOT_UID
  }

  /// Whether `gid` is the caller's primary or one of its supplementary groups.
  pub fn in_group(&self, gid: u32) -> bool {
    self
      .groups
      .as_ref()
      .is_some_and(|groups| groups.gid == gid || groups.supplementary.contains(&gid))
  }

  /// Whether the caller owns `node`.
  pub fn owns(&self, node: &NodeAttr) -> bool {
    self.uid == node.uid
  }
}

/// What a request wants from an object: to read it (list a directory), write it (add or remove an
/// entry), or execute it (search a directory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
  /// Read a file's bytes or list a directory.
  Read,
  /// Write a file's bytes, or add, remove or rename an entry of a directory.
  Write,
  /// Execute a file, or search (traverse) a directory.
  Search,
}

/// Why an ownership operation is refused: the errno class each maps to at the transport edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
  /// The caller is not the owner (nor the superuser): `EPERM`.
  NotOwner,
  /// The caller lacks the permission bit the operation needs: `EACCES`.
  NoAccess,
}

/// The one permission bit that decides `want` for `caller` on `node` — the class rule.
fn class_bit(caller: &Caller, node: &NodeAttr, want: Want) -> u32 {
  let shift = if caller.owns(node) {
    OWNER_SHIFT
  } else if caller.in_group(node.gid) {
    GROUP_SHIFT
  } else {
    0
  };
  let bit = match want {
    Want::Read => OTHER_READ,
    Want::Write => OTHER_WRITE,
    Want::Search => OTHER_EXECUTE,
  };
  bit << shift
}

/// Whether `caller` may `want` `node` by the POSIX class rule, with the superuser's exemptions.
pub fn permits(caller: &Caller, node: &NodeAttr, want: Want) -> bool {
  if caller.is_root() {
    // The superuser reads, writes and searches anything, and executes a file that some class may.
    return want != Want::Search || node.kind == Kind::Dir || node.mode & ANY_EXECUTE != 0;
  }
  node.mode & class_bit(caller, node, want) != 0
}

/// [`permits`] for a file's data — `READ`, `WRITE`, a truncate — with the owner override an NFS server
/// applies (the module doc): the owner reads and writes its own file whatever the mode bits say, so an
/// open descriptor survives a later `chmod`; every other caller is judged by the class rule.
pub fn permits_io(caller: &Caller, node: &NodeAttr, want: Want) -> bool {
  caller.owns(node) || permits(caller, node, want)
}

/// Whether `caller` owns `node` or is the superuser — what `chmod`, explicit `utimes` and a `chown`
/// of the group require.
pub fn owner_or_root(caller: &Caller, node: &NodeAttr) -> bool {
  caller.is_root() || caller.owns(node)
}

/// Whether the sticky bit of `directory` forbids `caller` removing or renaming `entry` out of it: the
/// directory is sticky and the caller is neither the entry's owner, the directory's owner, nor the
/// superuser.
pub fn sticky_forbids(caller: &Caller, directory: &NodeAttr, entry: &NodeAttr) -> bool {
  directory.mode & S_ISVTX != 0
    && !caller.is_root()
    && !caller.owns(directory)
    && !caller.owns(entry)
}

/// Whether `caller` may change `node`'s owner to `uid` and/or group to `gid` (`None` leaves each
/// unchanged), under `_POSIX_CHOWN_RESTRICTED`: the superuser may do anything; the owner may leave the
/// owner as is and set the group to one of the caller's own groups; anyone else may do nothing.
pub fn may_chown(caller: &Caller, node: &NodeAttr, uid: Option<u32>, gid: Option<u32>) -> bool {
  if caller.is_root() {
    return true;
  }
  if !caller.owns(node) {
    return false;
  }
  if uid.is_some_and(|new| new != node.uid) {
    return false;
  }
  !gid.is_some_and(|new| new != node.gid && !caller.in_group(new))
}

/// Why a `SETATTR` of `changes` on `node` by `caller` is refused, or `None` when every field it sets
/// is allowed: the mode needs ownership; the owner and group follow [`may_chown`]; the size needs
/// write permission (with the I/O owner override); explicit times need ownership and "now" needs
/// ownership or write permission (`explicit_times` says which the request asked for).
pub fn setattr_denial(
  caller: &Caller,
  node: &NodeAttr,
  changes: &SetAttr,
  explicit_times: bool,
) -> Option<Denial> {
  if changes.mode.is_some() && !owner_or_root(caller, node) {
    return Some(Denial::NotOwner);
  }
  if (changes.uid.is_some() || changes.gid.is_some())
    && !may_chown(caller, node, changes.uid, changes.gid)
  {
    return Some(Denial::NotOwner);
  }
  if changes.size.is_some() && !permits_io(caller, node, Want::Write) {
    return Some(Denial::NoAccess);
  }
  if changes.atime.is_some() || changes.mtime.is_some() {
    if explicit_times {
      if !owner_or_root(caller, node) {
        return Some(Denial::NotOwner);
      }
    } else if !owner_or_root(caller, node) && !permits(caller, node, Want::Write) {
      return Some(Denial::NoAccess);
    }
  }
  None
}

/// The set-id side effects POSIX attaches to an allowed `SETATTR` by a non-superuser, applied to
/// `changes` before it reaches the volume: a `chown` of a file clears its set-user-id and set-group-id
/// bits, and a `chmod` may not set `S_ISGID` on an object whose group is not one of the caller's (the
/// bit is dropped, the rest of the mode applied). The superuser's changes are applied as asked.
pub fn with_setid_side_effects(caller: &Caller, node: &NodeAttr, changes: SetAttr) -> SetAttr {
  if caller.is_root() {
    return changes;
  }
  let mut changes = changes;
  if let Some(mode) = changes.mode
    && mode & S_ISGID != 0
    && !caller.in_group(node.gid)
  {
    changes.mode = Some(mode & !S_ISGID);
  }
  if (changes.uid.is_some() || changes.gid.is_some()) && node.kind != Kind::Dir {
    let mode = changes.mode.unwrap_or(node.mode);
    if mode & (S_ISUID | S_ISGID) != 0 {
      changes.mode = Some(mode & !(S_ISUID | S_ISGID));
    }
  }
  changes
}

/// The mode a non-superuser's write leaves a file with: its set-user-id and set-group-id bits
/// cleared (POSIX `write(2)`), or `None` when nothing needs clearing (no bits set, or the superuser).
pub fn mode_after_write(caller: &Caller, node: &NodeAttr) -> Option<u32> {
  if caller.is_root() || node.mode & (S_ISUID | S_ISGID) == 0 {
    return None;
  }
  Some(node.mode & !(S_ISUID | S_ISGID))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn node(kind: Kind, mode: u32, uid: u32, gid: u32) -> NodeAttr {
    NodeAttr {
      ino: 7,
      generation: 0,
      kind,
      mode,
      nlink: 1,
      uid,
      gid,
      size: 0,
      atime: 0,
      mtime: 0,
      ctime: 0,
    }
  }

  fn user(uid: u32, gid: u32, supplementary: &[u32]) -> Caller {
    Caller {
      uid,
      groups: Some(UnixGroups {
        gid,
        supplementary: supplementary.to_vec(),
      }),
    }
  }

  /// The class rule: the owner is judged by the owner bits alone, a group member by the group bits,
  /// everyone else by the other bits — even where a "lower" class would allow more.
  #[test]
  fn exactly_one_class_applies() {
    // owner: ---, group: rw-, other: r--
    let file = node(Kind::File, 0o064, 1000, 2000);
    let owner = user(1000, 1000, &[]);
    let member = user(1001, 2000, &[]);
    let other = user(1002, 3000, &[]);
    assert!(
      !permits(&owner, &file, Want::Read),
      "the owner class is empty"
    );
    assert!(
      permits(&member, &file, Want::Write),
      "the group class allows write"
    );
    assert!(permits(&other, &file, Want::Read));
    assert!(!permits(&other, &file, Want::Write));
    assert!(!permits(&other, &file, Want::Search));
  }

  /// A supplementary group counts for the group class; a caller with no groups is "other".
  #[test]
  fn supplementary_groups_and_no_identity() {
    let file = node(Kind::File, 0o040, 1000, 2000);
    let supplementary = user(1001, 9, &[2000]);
    assert!(permits(&supplementary, &file, Want::Read));
    let nobody = Caller {
      uid: INVALID_UID,
      groups: None,
    };
    assert!(!permits(&nobody, &file, Want::Read));
    assert!(!nobody.in_group(2000));
  }

  /// The superuser reads, writes and searches anything, and executes a file only if some class may.
  #[test]
  fn the_superuser_is_exempt_except_for_execute() {
    let root = Caller::root();
    let locked = node(Kind::File, 0o000, 1000, 1000);
    assert!(permits(&root, &locked, Want::Read));
    assert!(permits(&root, &locked, Want::Write));
    assert!(
      !permits(&root, &locked, Want::Search),
      "no execute bit at all"
    );
    let executable = node(Kind::File, 0o001, 1000, 1000);
    assert!(permits(&root, &executable, Want::Search));
    let dir = node(Kind::Dir, 0o000, 1000, 1000);
    assert!(
      permits(&root, &dir, Want::Search),
      "a directory is always searchable by root"
    );
  }

  /// The I/O owner override: the owner reads and writes its file whatever the bits; others do not.
  #[test]
  fn the_owner_override_applies_to_io_only() {
    let file = node(Kind::File, 0o000, 1000, 1000);
    let owner = user(1000, 1000, &[]);
    let other = user(1001, 1001, &[]);
    assert!(permits_io(&owner, &file, Want::Read));
    assert!(permits_io(&owner, &file, Want::Write));
    assert!(
      !permits(&owner, &file, Want::Read),
      "ACCESS still reports the exact verdict"
    );
    assert!(!permits_io(&other, &file, Want::Read));
  }

  /// The sticky bit: only the entry's owner, the directory's owner or root removes an entry.
  #[test]
  fn the_sticky_bit_names_who_may_remove() {
    let sticky = node(Kind::Dir, 0o1777, 1000, 1000);
    let entry = node(Kind::File, 0o644, 1001, 1001);
    assert!(sticky_forbids(&user(1002, 1002, &[]), &sticky, &entry));
    assert!(
      !sticky_forbids(&user(1001, 1001, &[]), &sticky, &entry),
      "the entry's owner"
    );
    assert!(
      !sticky_forbids(&user(1000, 1000, &[]), &sticky, &entry),
      "the directory's owner"
    );
    assert!(!sticky_forbids(&Caller::root(), &sticky, &entry));
    let plain = node(Kind::Dir, 0o777, 1000, 1000);
    assert!(
      !sticky_forbids(&user(1002, 1002, &[]), &plain, &entry),
      "no sticky bit, no rule"
    );
  }

  /// chown is restricted: only root changes the owner; the owner may set the group to one of its own.
  #[test]
  fn chown_is_restricted() {
    let file = node(Kind::File, 0o644, 1000, 1000);
    let owner = user(1000, 1000, &[2000]);
    let other = user(1001, 1001, &[]);
    assert!(may_chown(&Caller::root(), &file, Some(5), Some(6)));
    assert!(
      !may_chown(&owner, &file, Some(1001), None),
      "the owner cannot give the file away"
    );
    assert!(
      may_chown(&owner, &file, Some(1000), None),
      "a no-op owner change is allowed"
    );
    assert!(
      may_chown(&owner, &file, None, Some(2000)),
      "to a supplementary group"
    );
    assert!(
      !may_chown(&owner, &file, None, Some(3000)),
      "not to a foreign group"
    );
    assert!(
      !may_chown(&other, &file, None, Some(1001)),
      "a non-owner changes nothing"
    );
    assert!(
      !may_chown(&other, &file, None, None),
      "even a no-op is refused to a non-owner"
    );
  }

  /// SETATTR: the mode and explicit times need ownership (EPERM); the size and "now" times need write
  /// permission (EACCES); root passes everything.
  #[test]
  fn setattr_denials_are_typed() {
    let file = node(Kind::File, 0o644, 1000, 1000);
    let other = user(1001, 1001, &[]);
    let chmod = SetAttr {
      mode: Some(0o600),
      ..SetAttr::default()
    };
    assert_eq!(
      setattr_denial(&other, &file, &chmod, false),
      Some(Denial::NotOwner)
    );
    assert_eq!(
      setattr_denial(&user(1000, 1000, &[]), &file, &chmod, false),
      None
    );
    assert_eq!(setattr_denial(&Caller::root(), &file, &chmod, false), None);
    let truncate = SetAttr {
      size: Some(0),
      ..SetAttr::default()
    };
    assert_eq!(
      setattr_denial(&other, &file, &truncate, false),
      Some(Denial::NoAccess)
    );
    let writable = node(Kind::File, 0o666, 1000, 1000);
    assert_eq!(setattr_denial(&other, &writable, &truncate, false), None);
    let times = SetAttr {
      mtime: Some(1),
      ..SetAttr::default()
    };
    assert_eq!(
      setattr_denial(&other, &writable, &times, true),
      Some(Denial::NotOwner)
    );
    assert_eq!(
      setattr_denial(&other, &writable, &times, false),
      None,
      "now needs only write"
    );
    assert_eq!(
      setattr_denial(&other, &file, &times, false),
      Some(Denial::NoAccess)
    );
  }

  /// A non-root chown of a file clears its set-id bits; a non-root chmod cannot set S_ISGID on an
  /// object of a foreign group; root's changes are applied as asked.
  #[test]
  fn setid_side_effects() {
    let setid = node(Kind::File, 0o6755, 1000, 1000);
    let owner = user(1000, 1000, &[]);
    let regroup = SetAttr {
      gid: Some(1000),
      ..SetAttr::default()
    };
    assert_eq!(
      with_setid_side_effects(&owner, &setid, regroup).mode,
      Some(0o755),
      "a non-root chown strips the set-id bits"
    );
    assert_eq!(
      with_setid_side_effects(&Caller::root(), &setid, regroup).mode,
      None
    );
    let dir = node(Kind::Dir, 0o2755, 1000, 1000);
    assert_eq!(
      with_setid_side_effects(&owner, &dir, regroup).mode,
      None,
      "a directory keeps its set-group-id bit through a chown"
    );
    let foreign_group = node(Kind::File, 0o644, 1000, 5000);
    let setgid = SetAttr {
      mode: Some(0o2755),
      ..SetAttr::default()
    };
    assert_eq!(
      with_setid_side_effects(&owner, &foreign_group, setgid).mode,
      Some(0o755),
      "S_ISGID is dropped for a group the caller is not in"
    );
    assert_eq!(mode_after_write(&owner, &setid), Some(0o755));
    assert_eq!(mode_after_write(&Caller::root(), &setid), None);
    assert_eq!(
      mode_after_write(&owner, &node(Kind::File, 0o644, 1000, 1000)),
      None
    );
  }
}
