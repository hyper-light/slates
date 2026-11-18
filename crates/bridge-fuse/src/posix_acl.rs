//! A POSIX ACL as a Linux guest's kernel hands it to its file server (§4.5 "Extended attributes", §4.6 the Shards
//! form; A-302): the value of `system.posix_acl_access` or `system.posix_acl_default`, which a guest's `setfacl` sets
//! and its `getfacl` reads through the FUSE dispatch of a guest kernel's route.
//!
//! **What it holds.** The format is Linux's own (`include/uapi/linux/posix_acl_xattr.h`): a little-endian version word
//! (2), then eight-byte entries, each a tag, a permission set and an id. Tags run in a fixed order: the owner
//! (`ACL_USER_OBJ`), named users by uid (`ACL_USER`), the owning group (`ACL_GROUP_OBJ`), named groups by gid
//! (`ACL_GROUP`), the mask (`ACL_MASK`, required once a named entry exists) and everyone else (`ACL_OTHER`).
//!
//! **Why slates reads it at all.** slates stores the guest's ACLs as its data and enforces none of them, as Shards'
//! own server does: neither negotiates `FUSE_POSIX_ACL`, so the guest's kernel checks the mode bits it is shown and
//! hands the ACL through (`fs/fuse/acl.c`, the path kept for servers without that flag). But a named entry's id is an
//! owner, and a Shards stream shows its guest owners through its [`OwnerView`] (A-281): an ACL naming the run's uid
//! must be kept naming the id the volume keeps for it, as a chown to the run's uid is, or the next stream's guest
//! (or the same guest after its owner word changes) reads an ACL naming someone else. So a value is translated each
//! way through the view, and with its ids changed, its named entries are put back in id order, the order Linux's
//! own ACLs keep and the one `libacl` expects.
//!
//! **Hostile input.** The value is a guest's bytes. It is refused whole as Linux's `posix_acl_from_xattr` and
//! `posix_acl_valid` refuse one (`fs/posix_acl.c`): shorter than its header, of another version (`EOPNOTSUPP`), not a
//! whole number of entries, an unknown tag, a permission outside `rwx`, an id that maps to no one (`u32::MAX`, Linux's
//! `ACL_UNDEFINED_ID`) on a named entry, or tags out of their order, repeated where they may not be, or a named entry
//! without a mask. Nothing is allocated before its length is checked against the request it came in.

use slates_bridge_core::OwnerView;

use crate::bridge::{EINVAL, EOPNOTSUPP};

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
/// Format: the permission bits an entry may hold (`ACL_READ | ACL_WRITE | ACL_EXECUTE`).
const PERMISSIONS: u16 = 0x07;
/// Format: the id that names no one (`ACL_UNDEFINED_ID`), which a named entry may not carry.
const UNDEFINED_ID: u32 = u32::MAX;

/// Why a value is no ACL Linux would take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclRefusal {
  /// Another format version: refused `EOPNOTSUPP`, as `posix_acl_from_xattr` refuses one.
  Version,
  /// Malformed or invalid: refused `EINVAL`.
  Invalid,
}

impl AclRefusal {
  /// The errno a reply carries for it.
  pub fn errno(self) -> i32 {
    match self {
      AclRefusal::Version => EOPNOTSUPP,
      AclRefusal::Invalid => EINVAL,
    }
  }
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

/// `value`, an ACL, with each named user's and group's id turned through `view` toward `toward`, and its named entries
/// back in id order; refused whole if Linux would refuse it (see the module's notes).
pub fn translated(value: &[u8], view: OwnerView, toward: Toward) -> Result<Vec<u8>, AclRefusal> {
  let mut entries = parse(value)?;
  for entry in &mut entries {
    entry.id = match (entry.tag, toward) {
      (USER, Toward::Kept) => view.kept_uid(entry.id),
      (USER, Toward::Shown) => view.shown_uid(entry.id),
      (GROUP, Toward::Kept) => view.kept_gid(entry.id),
      (GROUP, Toward::Shown) => view.shown_gid(entry.id),
      _ => entry.id,
    };
  }
  // The tags' order holds whatever the ids are, so a stable sort by tag, then id, reorders only within the runs of
  // named users and named groups.
  entries.sort_by_key(|entry| (tag_rank(entry.tag), entry.id));
  Ok(encode(&entries))
}

/// The entries of a well-formed value.
fn parse(value: &[u8]) -> Result<Vec<Entry>, AclRefusal> {
  let header = value.get(..HEADER_BYTES).ok_or(AclRefusal::Invalid)?;
  let version =
    u32::from_le_bytes(<[u8; HEADER_BYTES]>::try_from(header).map_err(|_| AclRefusal::Invalid)?);
  if version != VERSION {
    return Err(AclRefusal::Version);
  }
  let body = value.get(HEADER_BYTES..).unwrap_or_default();
  if body.len() % ENTRY_BYTES != 0 {
    return Err(AclRefusal::Invalid);
  }
  let entries = body
    .chunks_exact(ENTRY_BYTES)
    .map(entry_of)
    .collect::<Result<Vec<Entry>, AclRefusal>>()?;
  valid(&entries)?;
  Ok(entries)
}

/// One entry from its eight bytes, refused for an unknown tag, a permission outside `rwx` or a named entry's id that
/// names no one.
fn entry_of(bytes: &[u8]) -> Result<Entry, AclRefusal> {
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

/// The value of `entries`, its version word first.
fn encode(entries: &[Entry]) -> Vec<u8> {
  let mut value =
    Vec::with_capacity(HEADER_BYTES.saturating_add(entries.len().saturating_mul(ENTRY_BYTES)));
  value.extend_from_slice(&VERSION.to_le_bytes());
  for entry in entries {
    value.extend_from_slice(&entry.tag.to_le_bytes());
    value.extend_from_slice(&entry.perm.to_le_bytes());
    value.extend_from_slice(&entry.id.to_le_bytes());
  }
  value
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

  /// A-302. Do: turn an ACL naming the run (1000:1001), a third user (4242) and the host (501) through a view that
  /// swaps the host's owner (501:20) with the run's, toward the volume and back. Expect the run's ids kept as the
  /// host's and the host's as the run's, the third untouched, the named users put back in id order each way, the
  /// owner, mask and other entries unchanged, and the way back the guest's value exactly.
  #[test]
  fn an_acls_named_ids_turn_through_the_owner_view_and_stay_in_id_order() {
    let view = OwnerView::swapping((501, 20), (1000, 1001));
    let guest = acl(&[
      (USER_OBJ, 6, UNDEFINED_ID),
      (USER, 4, 501),
      (USER, 6, 1000),
      (USER, 7, 4242),
      (GROUP_OBJ, 4, UNDEFINED_ID),
      (GROUP, 4, 1001),
      (MASK, 7, UNDEFINED_ID),
      (OTHER, 0, UNDEFINED_ID),
    ]);
    let kept = translated(&guest, view, Toward::Kept).unwrap();
    assert_eq!(
      kept,
      acl(&[
        (USER_OBJ, 6, UNDEFINED_ID),
        (USER, 6, 501),
        (USER, 4, 1000),
        (USER, 7, 4242),
        (GROUP_OBJ, 4, UNDEFINED_ID),
        (GROUP, 4, 20),
        (MASK, 7, UNDEFINED_ID),
        (OTHER, 0, UNDEFINED_ID),
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
    let minimal = acl(&[
      (USER_OBJ, 7, UNDEFINED_ID),
      (GROUP_OBJ, 5, UNDEFINED_ID),
      (OTHER, 5, UNDEFINED_ID),
    ]);
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
  /// flipped bits in a valid one's tags. Expect every one refused, `EOPNOTSUPP` for the version and `EINVAL` for the
  /// rest, and nothing kept.
  #[test]
  fn a_value_linux_refuses_is_refused_whole() {
    let o = UNDEFINED_ID;
    let valid_named = acl(&[
      (USER_OBJ, 6, o),
      (USER, 6, 1000),
      (GROUP_OBJ, 4, o),
      (MASK, 6, o),
      (OTHER, 0, o),
    ]);
    let mut other_version = valid_named.clone();
    other_version[0] = 1;
    assert_eq!(
      translated(&other_version, OwnerView::IDENTITY, Toward::Kept),
      Err(AclRefusal::Version)
    );
    assert_eq!(AclRefusal::Version.errno(), EOPNOTSUPP);
    let mut torn = valid_named.clone();
    torn.pop();
    let invalid: Vec<Vec<u8>> = vec![
      vec![],
      vec![2, 0, 0],
      torn,
      acl(&[(0xFFFF, 0xFFFF, u32::MAX)]),
      acl(&[
        (USER_OBJ, 6, o),
        (0x40, 6, o),
        (GROUP_OBJ, 4, o),
        (OTHER, 0, o),
      ]),
      acl(&[(USER_OBJ, 8, o), (GROUP_OBJ, 4, o), (OTHER, 0, o)]),
      acl(&[
        (USER_OBJ, 6, o),
        (USER, 6, UNDEFINED_ID),
        (GROUP_OBJ, 4, o),
        (MASK, 6, o),
        (OTHER, 0, o),
      ]),
      acl(&[
        (USER_OBJ, 6, o),
        (USER_OBJ, 6, o),
        (GROUP_OBJ, 4, o),
        (OTHER, 0, o),
      ]),
      acl(&[
        (USER, 6, 1000),
        (USER_OBJ, 6, o),
        (GROUP_OBJ, 4, o),
        (MASK, 6, o),
        (OTHER, 0, o),
      ]),
      acl(&[
        (USER_OBJ, 6, o),
        (USER, 6, 1000),
        (GROUP_OBJ, 4, o),
        (OTHER, 0, o),
      ]),
      acl(&[(USER_OBJ, 6, o), (GROUP_OBJ, 4, o)]),
      acl(&[
        (USER_OBJ, 6, o),
        (GROUP_OBJ, 4, o),
        (OTHER, 0, o),
        (MASK, 6, o),
      ]),
    ];
    for value in invalid {
      assert_eq!(
        translated(&value, OwnerView::IDENTITY, Toward::Kept),
        Err(AclRefusal::Invalid),
        "{value:?}"
      );
    }
    assert_eq!(AclRefusal::Invalid.errno(), EINVAL);
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
}
