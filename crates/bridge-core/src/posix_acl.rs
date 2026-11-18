//! A POSIX ACL as Linux keeps one (§4.5 "Extended attributes"; A-302, A-314): the value of `system.posix_acl_access`
//! or `system.posix_acl_default`, which a guest's `setfacl` sets and its `getfacl` reads through the FUSE dispatch of a
//! guest kernel's route, and the rules by which an ACL, a file's mode and a directory's new entries hold together.
//!
//! **What it holds.** The format is Linux's own (`include/uapi/linux/posix_acl_xattr.h`): a little-endian version word
//! (2), then eight-byte entries, each a tag, a permission set and an id. Tags run in a fixed order: the owner
//! (`ACL_USER_OBJ`), named users by uid (`ACL_USER`), the owning group (`ACL_GROUP_OBJ`), named groups by gid
//! (`ACL_GROUP`), the mask (`ACL_MASK`, required once a named entry exists) and everyone else (`ACL_OTHER`).
//!
//! **Who enforces it (A-314).** A guest kernel's route asks its kernel to check every access against the ACLs it reads
//! (`FUSE_POSIX_ACL`), as a Linux bind mount enforces a container's `setfacl`; Shards' coordinator asked for exactly
//! that (2026-11-18). The kernel then reads a node's access ACL with GETXATTR and judges with it, but it leaves the rest
//! to its server (`fs/fuse/acl.c`: "Fuse userspace is responsible for updating access permissions in the inode";
//! `fs/fuse/dir.c` drops its cached ACLs after a chmod because the server "may have updated acl xattrs"; Linux
//! v7.2-rc4). This module is that rest, as Linux's own `fs/posix_acl.c` does it for a disk file system:
//! - **The mode is the access ACL's owner, mask and other entries** (the owning group's where there is no mask), on
//!   every route. Setting an access ACL sets those bits of the mode, and keeps the ACL only when it says more than a
//!   mode can, a named entry or a mask ([`Acl::permission_bits`], [`Acl::exceeds_a_mode`]: `posix_acl_equiv_mode` and
//!   `posix_acl_update_mode`). A chmod through any route changes those three entries by changing the mode, so a
//!   reader is shown them from the mode at the moment it reads ([`Acl::showing`]: what `__posix_acl_chmod_masq` does
//!   to a stored ACL at each chmod). Kept this way, no route needs to know an ACL exists to keep it right: a host's
//!   mount, NFS, WinFsp and the typed channel chmod a file as they always did.
//! - **A directory's default ACL is what it gives what is made in it** ([`Acl::given`]: `posix_acl_create_masq`). The
//!   creator's umask is not applied under one; the new node's mode keeps only what the default grants, and it takes the
//!   default's entries as its access ACL when they say more than its mode, and a new directory takes the default too.
//!   Applied by the shared seam at every create, whatever the route.
//!
//! **What was weighed.** Server-side enforcement was rejected: a FUSE request names its caller's uid and primary gid
//! and none of its other groups (`struct fuse_in_header`), so a server cannot judge a named group's entry, which the
//! guest's own kernel can. Rewriting the stored ACL at every chmod was rejected for showing it from the mode: every
//! route's chmod would have to parse and rewrite an attribute it cannot otherwise see (a host's mount reaches no ACL,
//! A-302), where showing costs one parse at a read.
//!
//! **Hostile input.** A value is a guest's bytes. It is refused whole as Linux's `posix_acl_from_xattr` and
//! `posix_acl_valid` refuse one (`fs/posix_acl.c`): shorter than its header, of another version
//! ([`AclRefusal::Version`]), not a whole number of entries, an unknown tag, a permission outside `rwx`, an id that maps
//! to no one (`u32::MAX`, Linux's `ACL_UNDEFINED_ID`) on a named entry, or tags out of their order, repeated where they
//! may not be, or a named entry without a mask. Nothing is allocated past the value's own length.

use crate::OwnerView;

/// Format: the version word every Linux ACL value starts with (`POSIX_ACL_XATTR_VERSION`).
const VERSION: u32 = 2;
/// Format: the version word's bytes (`struct posix_acl_xattr_header`).
const HEADER_BYTES: usize = 4;
/// Format: one entry's bytes (`struct posix_acl_xattr_entry`: `e_tag` le16, `e_perm` le16, `e_id` le32).
const ENTRY_BYTES: usize = 8;
/// Format: the file owner's entry (`ACL_USER_OBJ`).
const USER_OBJ: u16 = 0x01;
/// Format: a named user's entry (`ACL_USER`), its id a uid.
const USER: u16 = 0x02;
/// Format: the owning group's entry (`ACL_GROUP_OBJ`).
const GROUP_OBJ: u16 = 0x04;
/// Format: a named group's entry (`ACL_GROUP`), its id a gid.
const GROUP: u16 = 0x08;
/// Format: the mask entry (`ACL_MASK`), the most a named entry or the owning group grants.
const MASK: u16 = 0x10;
/// Format: everyone else's entry (`ACL_OTHER`).
const OTHER: u16 = 0x20;
/// Format: the permission bits an entry may hold (`ACL_READ | ACL_WRITE | ACL_EXECUTE`), which are also one class's
/// bits of a mode (`S_IRWXO`).
const PERMISSIONS: u16 = 0x07;
/// Format: the id that names no one (`ACL_UNDEFINED_ID`), which a named entry may not carry.
const UNDEFINED_ID: u32 = u32::MAX;
/// Format: where the owner's class lies in a mode (`S_IRWXU` is `PERMISSIONS` shifted by this).
const OWNER_SHIFT: u32 = 6;
/// Format: where the group's class lies in a mode (`S_IRWXG`).
const GROUP_SHIFT: u32 = 3;
/// Format: the three classes' bits of a mode (`S_IRWXUGO`), the part an ACL speaks for; the set-id and sticky bits
/// above them are the mode's alone.
pub const MODE_PERMISSION_BITS: u32 = 0o777;

/// Why a value is no ACL Linux would take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclRefusal {
  /// Another format version: Linux's `posix_acl_from_xattr` refuses it `EOPNOTSUPP`.
  Version,
  /// Malformed or invalid: refused `EINVAL`.
  Invalid,
}

/// Which way a value's ids are turned through an [`OwnerView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toward {
  /// From the ids the guest names to those the volume keeps: a value the guest sets.
  Kept,
  /// From the ids the volume keeps to those the guest is shown: a value the guest reads.
  Shown,
}

/// One entry: its tag, permission set and id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
  tag: u16,
  perm: u16,
  id: u32,
}

/// A valid ACL: its entries, in the order Linux keeps them. An ACL of no entries is the empty one, which a kernel reads
/// as none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acl {
  entries: Vec<Entry>,
}

impl Acl {
  /// The ACL `value` holds, or why Linux would refuse it (see the module's notes).
  pub fn parse(value: &[u8]) -> Result<Acl, AclRefusal> {
    let header = value.get(..HEADER_BYTES).ok_or(AclRefusal::Invalid)?;
    let version =
      u32::from_le_bytes(<[u8; HEADER_BYTES]>::try_from(header).map_err(|_| AclRefusal::Invalid)?);
    if version != VERSION {
      return Err(AclRefusal::Version);
    }
    let body = value.get(HEADER_BYTES..).unwrap_or_default();
    let (whole, rest) = body.as_chunks::<ENTRY_BYTES>();
    if !rest.is_empty() {
      return Err(AclRefusal::Invalid);
    }
    let entries = whole
      .iter()
      .map(entry_of)
      .collect::<Result<Vec<Entry>, AclRefusal>>()?;
    valid(&entries)?;
    Ok(Acl { entries })
  }

  /// The ACL's value, its version word first.
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut value = Vec::with_capacity(
      HEADER_BYTES.saturating_add(self.entries.len().saturating_mul(ENTRY_BYTES)),
    );
    value.extend_from_slice(&VERSION.to_le_bytes());
    for entry in &self.entries {
      value.extend_from_slice(&entry.tag.to_le_bytes());
      value.extend_from_slice(&entry.perm.to_le_bytes());
      value.extend_from_slice(&entry.id.to_le_bytes());
    }
    value
  }

  /// Whether it is the empty ACL, which a kernel reads as none (`posix_acl_from_xattr` of no entries).
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// The three classes' bits of a mode this ACL makes, [`MODE_PERMISSION_BITS`] alone: the owner's from its owner
  /// entry, the group's from its mask (the owning group's where it has none), everyone else's from its other entry
  /// (`posix_acl_equiv_mode`). Zero for the empty ACL, which sets no mode.
  pub fn permission_bits(&self) -> u32 {
    let class = |tag: u16| {
      self
        .entries
        .iter()
        .find(|entry| entry.tag == tag)
        .map(|entry| u32::from(entry.perm & PERMISSIONS))
    };
    let group = class(MASK).or_else(|| class(GROUP_OBJ)).unwrap_or(0);
    (class(USER_OBJ).unwrap_or(0) << OWNER_SHIFT)
      | (group << GROUP_SHIFT)
      | class(OTHER).unwrap_or(0)
  }

  /// Whether the ACL says more than a mode can: a named user or group, or a mask (`posix_acl_equiv_mode`'s "not
  /// equivalent"). One that does not is the mode alone and is kept as none (`posix_acl_update_mode`).
  pub fn exceeds_a_mode(&self) -> bool {
    self
      .entries
      .iter()
      .any(|entry| matches!(entry.tag, USER | GROUP | MASK))
  }

  /// Sets the entries a mode speaks for from `mode`: the owner's from its owner class, the mask's (the owning group's
  /// where there is no mask) from its group class, everyone else's from its other class (`__posix_acl_chmod_masq`).
  /// The access ACL a reader is shown is the kept one with these taken from the mode then, so a chmod through any route
  /// is the ACL's change too.
  pub fn showing(&mut self, mode: u32) {
    let has_mask = self.entries.iter().any(|entry| entry.tag == MASK);
    let class = |shift: u32| u16::try_from((mode >> shift) & u32::from(PERMISSIONS)).unwrap_or(0);
    for entry in &mut self.entries {
      match entry.tag {
        USER_OBJ => entry.perm = class(OWNER_SHIFT),
        MASK => entry.perm = class(GROUP_SHIFT),
        GROUP_OBJ if !has_mask => entry.perm = class(GROUP_SHIFT),
        OTHER => entry.perm = class(0),
        _ => {}
      }
    }
  }

  /// What a node made with `mode` in a directory whose default ACL this is takes (`posix_acl_create_masq`, applied by
  /// `posix_acl_create` in place of the umask): its mode, keeping of the three classes only what the default's owner,
  /// mask (or owning group) and other entries grant, its set-id and sticky bits as asked; and its access ACL, the
  /// default's entries with those three cut to the mode, when they say more than the mode can.
  pub fn given(&self, mode: u32) -> (u32, Option<Acl>) {
    let mut access = self.clone();
    let mut kept = mode & MODE_PERMISSION_BITS;
    let class =
      |bits: u32, shift: u32| u16::try_from((bits >> shift) & u32::from(PERMISSIONS)).unwrap_or(0);
    let cut = |kept: &mut u32, perm: u16, shift: u32| {
      *kept &= (u32::from(perm) << shift) | !(u32::from(PERMISSIONS) << shift);
    };
    for entry in &mut access.entries {
      match entry.tag {
        USER_OBJ => {
          entry.perm &= class(kept, OWNER_SHIFT);
          cut(&mut kept, entry.perm, OWNER_SHIFT);
        }
        OTHER => {
          entry.perm &= class(kept, 0);
          cut(&mut kept, entry.perm, 0);
        }
        _ => {}
      }
    }
    let has_mask = access.entries.iter().any(|entry| entry.tag == MASK);
    let group_tag = if has_mask { MASK } else { GROUP_OBJ };
    if let Some(group) = access
      .entries
      .iter_mut()
      .find(|entry| entry.tag == group_tag)
    {
      group.perm &= class(kept, GROUP_SHIFT);
      cut(&mut kept, group.perm, GROUP_SHIFT);
    }
    let made = (mode & !MODE_PERMISSION_BITS) | kept;
    (made, access.exceeds_a_mode().then_some(access))
  }
}

/// `value`, an ACL, with each named user's and group's id turned through `view` toward `toward`, and its named entries
/// back in id order (A-302): an ACL names owners, kept and shown through a stream's [`OwnerView`] as a chown's and a
/// stat's are, so an ACL naming the run's uid is kept naming the id the volume keeps for it. Refused whole if Linux
/// would refuse it.
pub fn translated(value: &[u8], view: OwnerView, toward: Toward) -> Result<Vec<u8>, AclRefusal> {
  let mut acl = Acl::parse(value)?;
  for entry in &mut acl.entries {
    entry.id = match (entry.tag, toward) {
      (USER, Toward::Kept) => view.kept_uid(entry.id),
      (USER, Toward::Shown) => view.shown_uid(entry.id),
      (GROUP, Toward::Kept) => view.kept_gid(entry.id),
      (GROUP, Toward::Shown) => view.shown_gid(entry.id),
      _ => entry.id,
    };
  }
  // The tags' order holds whatever the ids are, so a stable sort by tag, then id, reorders only within the runs of
  // named users and named groups, the order Linux's own ACLs keep and `libacl` expects.
  acl
    .entries
    .sort_by_key(|entry| (tag_rank(entry.tag), entry.id));
  Ok(acl.to_bytes())
}

/// One entry from its eight bytes, refused for an unknown tag, a permission outside `rwx` or a named entry's id that
/// names no one.
fn entry_of(bytes: &[u8; ENTRY_BYTES]) -> Result<Entry, AclRefusal> {
  let word = |at: usize| -> Result<u16, AclRefusal> {
    let pair = bytes
      .get(at..at.saturating_add(2))
      .ok_or(AclRefusal::Invalid)?;
    Ok(u16::from_le_bytes(
      <[u8; 2]>::try_from(pair).map_err(|_| AclRefusal::Invalid)?,
    ))
  };
  let id_bytes = bytes.get(4..ENTRY_BYTES).ok_or(AclRefusal::Invalid)?;
  let entry = Entry {
    tag: word(0)?,
    perm: word(2)?,
    id: u32::from_le_bytes(<[u8; 4]>::try_from(id_bytes).map_err(|_| AclRefusal::Invalid)?),
  };
  if tag_rank(entry.tag) == UNKNOWN_RANK || entry.perm & !PERMISSIONS != 0 {
    return Err(AclRefusal::Invalid);
  }
  if matches!(entry.tag, USER | GROUP) && entry.id == UNDEFINED_ID {
    return Err(AclRefusal::Invalid);
  }
  Ok(entry)
}

/// Format: the rank of a tag no ACL holds.
const UNKNOWN_RANK: u8 = u8::MAX;

/// A tag's place in the order every ACL keeps its entries in.
fn tag_rank(tag: u16) -> u8 {
  match tag {
    USER_OBJ => 0,
    USER => 1,
    GROUP_OBJ => 2,
    GROUP => 3,
    MASK => 4,
    OTHER => 5,
    _ => UNKNOWN_RANK,
  }
}

/// Refuses an ACL Linux's `posix_acl_valid` refuses: the owner first and once, named users after it, the owning group
/// once, named groups after it, a mask at most once and required by any named entry, everyone else last and once. An
/// ACL of no entries is the empty one a kernel reads as "none", and stands.
fn valid(entries: &[Entry]) -> Result<(), AclRefusal> {
  if entries.is_empty() {
    return Ok(());
  }
  let ordered = entries.windows(2).all(in_order);
  let has = |tag: u16| entries.iter().any(|entry| entry.tag == tag);
  let named = has(USER) || has(GROUP);
  let complete = has(USER_OBJ) && has(GROUP_OBJ) && has(OTHER) && (!named || has(MASK));
  if ordered && complete {
    Ok(())
  } else {
    Err(AclRefusal::Invalid)
  }
}

/// Whether two neighbouring entries keep the order: a later tag, or the same tag where it may repeat (a named user or
/// group). Every other tag stands once, and none comes back after a later one.
fn in_order(pair: &[Entry]) -> bool {
  match pair {
    [first, second] => {
      let (before, after) = (tag_rank(first.tag), tag_rank(second.tag));
      before < after || (before == after && matches!(second.tag, USER | GROUP))
    }
    _ => true,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// An ACL value of `entries` (tag, permission set, id), as a guest's kernel sends it.
  fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut value = VERSION.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
      value.extend_from_slice(&tag.to_le_bytes());
      value.extend_from_slice(&perm.to_le_bytes());
      value.extend_from_slice(&id.to_le_bytes());
    }
    value
  }

  /// The id an entry naming no one carries, short for the tests' tables.
  const O: u32 = UNDEFINED_ID;

  /// A-302. Do: turn an ACL naming the run (1000:1001), a third user (4242) and the host (501) through a view that
  /// swaps the host's owner (501:20) with the run's, toward the volume and back. Expect the run's ids kept as the
  /// host's and the host's as the run's, the third untouched, the named users put back in id order each way, the
  /// owner, mask and other entries unchanged, and the way back the guest's value exactly.
  #[test]
  fn an_acls_named_ids_turn_through_the_owner_view_and_stay_in_id_order() {
    let view = OwnerView::swapping((501, 20), (1000, 1001));
    let guest = acl(&[
      (USER_OBJ, 6, O),
      (USER, 4, 501),
      (USER, 6, 1000),
      (USER, 7, 4242),
      (GROUP_OBJ, 4, O),
      (GROUP, 4, 1001),
      (MASK, 7, O),
      (OTHER, 0, O),
    ]);
    let kept = translated(&guest, view, Toward::Kept).unwrap();
    assert_eq!(
      kept,
      acl(&[
        (USER_OBJ, 6, O),
        (USER, 6, 501),
        (USER, 4, 1000),
        (USER, 7, 4242),
        (GROUP_OBJ, 4, O),
        (GROUP, 4, 20),
        (MASK, 7, O),
        (OTHER, 0, O),
      ]),
      "the run's ids kept as the host's, the host's as the run's, in id order"
    );
    assert_eq!(translated(&kept, view, Toward::Shown).unwrap(), guest);
    assert_eq!(
      translated(&guest, OwnerView::IDENTITY, Toward::Kept).unwrap(),
      guest,
      "the identity changes nothing"
    );
  }

  /// A-302. Do: hand the codec the ACLs Linux takes with no named entry, and the empty one. Expect each kept as it is.
  #[test]
  fn a_minimal_acl_and_the_empty_one_stand() {
    let minimal = acl(&[(USER_OBJ, 7, O), (GROUP_OBJ, 5, O), (OTHER, 5, O)]);
    let empty = acl(&[]);
    for value in [minimal, empty] {
      assert_eq!(
        translated(&value, OwnerView::IDENTITY, Toward::Kept).unwrap(),
        value
      );
    }
  }

  /// A-302, hostile input. Do: hand the codec values Linux refuses: shorter than the header, another version, a torn
  /// entry, an all-ones entry, an unknown tag, a permission past `rwx`, a named user of the undefined id, the
  /// owner twice, a named user before the owner, a named entry with no mask, no `other`, a mask after `other`, and
  /// flipped bits in a valid one's tags. Expect every one refused, `Version` for the version and `Invalid` for the
  /// rest, and nothing kept.
  #[test]
  fn a_value_linux_refuses_is_refused_whole() {
    let valid_named = acl(&[
      (USER_OBJ, 6, O),
      (USER, 6, 1000),
      (GROUP_OBJ, 4, O),
      (MASK, 6, O),
      (OTHER, 0, O),
    ]);
    let mut other_version = valid_named.clone();
    other_version[0] = 1;
    assert_eq!(Acl::parse(&other_version), Err(AclRefusal::Version));
    let mut torn = valid_named.clone();
    torn.pop();
    let invalid: Vec<Vec<u8>> = vec![
      vec![],
      vec![2, 0, 0],
      torn,
      acl(&[(0xFFFF, 0xFFFF, u32::MAX)]),
      acl(&[
        (USER_OBJ, 6, O),
        (0x40, 6, O),
        (GROUP_OBJ, 4, O),
        (OTHER, 0, O),
      ]),
      acl(&[(USER_OBJ, 8, O), (GROUP_OBJ, 4, O), (OTHER, 0, O)]),
      acl(&[
        (USER_OBJ, 6, O),
        (USER, 6, UNDEFINED_ID),
        (GROUP_OBJ, 4, O),
        (MASK, 6, O),
        (OTHER, 0, O),
      ]),
      acl(&[
        (USER_OBJ, 6, O),
        (USER_OBJ, 6, O),
        (GROUP_OBJ, 4, O),
        (OTHER, 0, O),
      ]),
      acl(&[
        (USER, 6, 1000),
        (USER_OBJ, 6, O),
        (GROUP_OBJ, 4, O),
        (MASK, 6, O),
        (OTHER, 0, O),
      ]),
      acl(&[
        (USER_OBJ, 6, O),
        (USER, 6, 1000),
        (GROUP_OBJ, 4, O),
        (OTHER, 0, O),
      ]),
      acl(&[(USER_OBJ, 6, O), (GROUP_OBJ, 4, O)]),
      acl(&[
        (USER_OBJ, 6, O),
        (GROUP_OBJ, 4, O),
        (OTHER, 0, O),
        (MASK, 6, O),
      ]),
    ];
    for value in invalid {
      assert_eq!(Acl::parse(&value), Err(AclRefusal::Invalid), "{value:?}");
    }
    // Every single-bit flip of a tag word in a valid ACL makes another tag or none: each either stands as a valid ACL
    // or is refused, and never panics.
    for byte in (HEADER_BYTES..valid_named.len()).step_by(ENTRY_BYTES) {
      for bit in 0..16 {
        let mut flipped = valid_named.clone();
        let at = byte + bit / 8;
        flipped[at] ^= 1 << (bit % 8);
        let _ = translated(&flipped, OwnerView::IDENTITY, Toward::Kept);
      }
    }
  }

  /// A-314 (`posix_acl_equiv_mode` and `posix_acl_update_mode`). Do: take the mode bits of an ACL with a named user and
  /// a mask, of one with a mask alone, of a minimal one and of the empty one, as `setfacl` sets each. Expect the mask
  /// in the group bits wherever there is one and the owning group's entry otherwise, the owner's and other's entries
  /// in their classes; the named one and the mask-alone one to say more than a mode, the minimal and empty ones not;
  /// and the empty one to make no mode bits.
  #[test]
  fn an_acl_makes_the_mode_its_owner_mask_and_other_entries_say() {
    let named = Acl::parse(&acl(&[
      (USER_OBJ, 6, O),
      (USER, 7, 4242),
      (GROUP_OBJ, 4, O),
      (MASK, 5, O),
      (OTHER, 1, O),
    ]))
    .unwrap();
    assert_eq!(
      (named.permission_bits(), named.exceeds_a_mode()),
      (0o651, true)
    );
    let mask_alone = Acl::parse(&acl(&[
      (USER_OBJ, 7, O),
      (GROUP_OBJ, 7, O),
      (MASK, 4, O),
      (OTHER, 0, O),
    ]))
    .unwrap();
    assert_eq!(
      (mask_alone.permission_bits(), mask_alone.exceeds_a_mode()),
      (0o740, true),
      "a mask alone is more than a mode: the owning group keeps rwx behind it"
    );
    let minimal = Acl::parse(&acl(&[(USER_OBJ, 7, O), (GROUP_OBJ, 5, O), (OTHER, 0, O)])).unwrap();
    assert_eq!(
      (minimal.permission_bits(), minimal.exceeds_a_mode()),
      (0o750, false)
    );
    let empty = Acl::parse(&acl(&[])).unwrap();
    assert_eq!(
      (
        empty.is_empty(),
        empty.permission_bits(),
        empty.exceeds_a_mode()
      ),
      (true, 0, false)
    );
  }

  /// A-314 (`__posix_acl_chmod_masq`). Do: show an ACL with a named group and a mask from mode 02750, and a minimal ACL
  /// from mode 0604. Expect the owner, mask and other entries taken from the mode's classes, the owning group's and
  /// the named entries untouched where there is a mask, and the owning group's taken from the group class where there
  /// is none; the set-group-id bit changing no entry.
  #[test]
  fn a_chmod_is_shown_as_the_access_acls_owner_mask_and_other_entries() {
    let mut named = Acl::parse(&acl(&[
      (USER_OBJ, 6, O),
      (GROUP_OBJ, 4, O),
      (GROUP, 6, 77),
      (MASK, 6, O),
      (OTHER, 4, O),
    ]))
    .unwrap();
    named.showing(0o2750);
    assert_eq!(
      named.to_bytes(),
      acl(&[
        (USER_OBJ, 7, O),
        (GROUP_OBJ, 4, O),
        (GROUP, 6, 77),
        (MASK, 5, O),
        (OTHER, 0, O),
      ])
    );
    let mut minimal =
      Acl::parse(&acl(&[(USER_OBJ, 7, O), (GROUP_OBJ, 5, O), (OTHER, 5, O)])).unwrap();
    minimal.showing(0o604);
    assert_eq!(
      minimal.to_bytes(),
      acl(&[(USER_OBJ, 6, O), (GROUP_OBJ, 0, O), (OTHER, 4, O)])
    );
  }

  /// A-314 (`posix_acl_create_masq`, worked by hand from `fs/posix_acl.c` at v7.2-rc4). Do: give what a directory's
  /// default ACL `u::rwx,u:4242:rwx,g::r-x,m::rwx,o::---` gives a file asked for as 0666, a directory as 0777, a FIFO as
  /// 0644 and a set-group-id file as 02755; and what the minimal default `u::rwx,g::r-x,o::r--` gives a file asked for
  /// as 0666. Expect the modes 0660, 0770, 0640 and 02750, each access ACL the default's entries with the owner, mask
  /// and other entries cut to its mode, the owning group's and 4242's left as the default has them; and from the
  /// minimal default the mode 0644 and no access ACL, since it says no more than the mode.
  #[test]
  fn a_default_acl_gives_what_is_made_its_entries_cut_to_the_asked_mode() {
    let default = Acl::parse(&acl(&[
      (USER_OBJ, 7, O),
      (USER, 7, 4242),
      (GROUP_OBJ, 5, O),
      (MASK, 7, O),
      (OTHER, 0, O),
    ]))
    .unwrap();
    let cut = |owner: u16, mask: u16| {
      acl(&[
        (USER_OBJ, owner, O),
        (USER, 7, 4242),
        (GROUP_OBJ, 5, O),
        (MASK, mask, O),
        (OTHER, 0, O),
      ])
    };
    for (asked, made, owner, mask) in [
      (0o666, 0o660, 6, 6),
      (0o777, 0o770, 7, 7),
      (0o644, 0o640, 6, 4),
      (0o2755, 0o2750, 7, 5),
    ] {
      let (mode, access) = default.given(asked);
      assert_eq!(mode, made, "asked {asked:o}");
      assert_eq!(
        access.map(|access| access.to_bytes()),
        Some(cut(owner, mask)),
        "asked {asked:o}"
      );
    }
    let minimal = Acl::parse(&acl(&[(USER_OBJ, 7, O), (GROUP_OBJ, 5, O), (OTHER, 4, O)])).unwrap();
    assert_eq!(minimal.given(0o666), (0o644, None));
  }
}
