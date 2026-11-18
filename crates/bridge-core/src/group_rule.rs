//! The group a new object takes, by the rule of the kernel a route serves (§4.6 "POSIX and transparency
//! acceptance", §4.13; A-328). POSIX leaves it to the implementation (`mkdir(2)`, `open(2)`: "the group ID of the
//! parent directory or ... the effective group ID of the process"), and the two kernels slates serves answer
//! differently:
//!
//! - **System V's rule, Linux's** (`inode_init_owner` in `fs/inode.c`): the creator's group, unless the parent
//!   directory is set-group-ID; then the parent's group, and a directory made there takes the bit too, so the whole
//!   tree under it keeps the group.
//! - **BSD's rule, macOS's** (XNU's `vnode_authattr_new`; measured on APFS, macOS 26, 2026-11-18: a parent `2755`, a
//!   directory made in it `755`): always the parent directory's group, and no directory takes the bit from its parent.
//!
//! A route gives a new object what a native file system under the kernel it serves would. FUSE and virtio-fs serve
//! Linux kernels, so System V's rule; the NFS export gives its host's rule, as a native NFS server does, since its
//! host's file system makes the object; so do the routes without a kernel (the typed channel). Before A-328 every
//! route took System V's rule (AUD-29-81), and on macOS a Subversion repository's `db/revprops`, made in the
//! set-group-ID `db`, read `2755` over the mount and `755` on the host (CI run 44260193629).
//!
//! This module is the pure decision ([`GroupRule::created`]), unit-tested on every host; the one platform choice is
//! [`GroupRule::of_this_host`], paired `#[cfg]` functions.

use crate::access::S_ISGID;

/// How a new object takes its group (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupRule {
  /// Linux's: the creator's group, unless the parent is set-group-ID, whose group a new object takes and whose bit a
  /// new directory takes too.
  SystemV,
  /// macOS's and the BSDs': the parent's group always, and no inherited bit.
  Bsd,
}

/// What a new object takes from its rule: its group, and whether it takes the set-group-ID bit (a directory alone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreatedGroup {
  /// The object's group.
  pub gid: u32,
  /// Whether the object takes the set-group-ID bit beside the mode it was asked for.
  pub set_group_id: bool,
}

impl GroupRule {
  /// The group and the bit a new object takes in a parent directory of mode `parent_mode` and group `parent_gid`, made
  /// by a creator whose credential names `creator_gid` (`None`: it names none, and the parent's group stands), a
  /// directory when `directory`.
  pub fn created(
    self,
    parent_mode: u32,
    parent_gid: u32,
    creator_gid: Option<u32>,
    directory: bool,
  ) -> CreatedGroup {
    match self {
      GroupRule::SystemV => {
        let set_group_id_parent = parent_mode & S_ISGID != 0;
        CreatedGroup {
          gid: match creator_gid {
            Some(gid) if !set_group_id_parent => gid,
            _ => parent_gid,
          },
          set_group_id: set_group_id_parent && directory,
        }
      }
      GroupRule::Bsd => CreatedGroup {
        gid: parent_gid,
        set_group_id: false,
      },
    }
  }

  /// The rule of this host's own file systems, which its NFS export and its routes without a kernel give: BSD's on
  /// macOS.
  #[cfg(target_os = "macos")]
  pub const fn of_this_host() -> GroupRule {
    GroupRule::Bsd
  }

  /// The rule of this host's own file systems, which its NFS export and its routes without a kernel give: System V's
  /// on Linux, and on Windows, whose own file systems keep no POSIX group (its drive stamps no Unix owner, and its NFS
  /// export answers as Linux's does).
  #[cfg(not(target_os = "macos"))]
  pub const fn of_this_host() -> GroupRule {
    GroupRule::SystemV
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: a parent's group in these cases, other than the creator's.
  const PARENT_GID: u32 = 7;
  /// Format: the group a creator's credential names.
  const CREATOR_GID: u32 = 20;
  /// Format: a plain directory's mode.
  const PLAIN: u32 = 0o755;
  /// Format: a set-group-ID directory's mode.
  const SET_GROUP_ID: u32 = 0o2755;

  /// A-328 / AUD-29-81, System V's rule. Do: make a file and a directory, with and without a creator's group, in a
  /// plain parent and in a set-group-ID one. Expect: in the plain parent the creator's group (the parent's when the
  /// credential names none) and no bit; in the set-group-ID parent the parent's group, the directory alone taking the
  /// bit.
  #[test]
  fn system_vs_rule_gives_the_creators_group_unless_the_parent_is_set_group_id() {
    let rule = GroupRule::SystemV;
    for directory in [false, true] {
      assert_eq!(
        rule.created(PLAIN, PARENT_GID, Some(CREATOR_GID), directory),
        CreatedGroup {
          gid: CREATOR_GID,
          set_group_id: false
        }
      );
      assert_eq!(
        rule.created(PLAIN, PARENT_GID, None, directory),
        CreatedGroup {
          gid: PARENT_GID,
          set_group_id: false
        }
      );
      for creator in [Some(CREATOR_GID), None] {
        assert_eq!(
          rule.created(SET_GROUP_ID, PARENT_GID, creator, directory),
          CreatedGroup {
            gid: PARENT_GID,
            set_group_id: directory
          }
        );
      }
    }
  }

  /// A-328, BSD's rule (macOS: APFS measured, a directory made in a `2755` parent reading `755`). Do: make a file and
  /// a directory, with and without a creator's group, in a plain parent and in a set-group-ID one. Expect: the
  /// parent's group every time, and no bit.
  #[test]
  fn bsds_rule_gives_the_parents_group_and_no_inherited_bit() {
    let rule = GroupRule::Bsd;
    for directory in [false, true] {
      for parent_mode in [PLAIN, SET_GROUP_ID] {
        for creator in [Some(CREATOR_GID), None] {
          assert_eq!(
            rule.created(parent_mode, PARENT_GID, creator, directory),
            CreatedGroup {
              gid: PARENT_GID,
              set_group_id: false
            }
          );
        }
      }
    }
  }
}
