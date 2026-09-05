//! Kernel cache invalidations (§4.6 "Cache posture": explicit invalidation on every mutation
//! of a path visible through another attachment). slates caches entries and attributes forever
//! in the kernel, so it must tell the kernel to drop them when the daemon changes something a
//! second reader could see. A notification is an unsolicited message the daemon writes to
//! `/dev/fuse`: the `fuse_out_header` with a zero unique id and the notification code in the
//! `error` field, then the notification body.
//!
//! These are pure encoders (like the reply encoders), so they carry golden-vector tests and run
//! on every host; the driver writes them through the transport on a mutation before it
//! acknowledges the mutating request, so a second process never reads stale attributes after
//! the mutating call returns (AC-3.3).

use crate::abi::OUT_HEADER_LEN;
use crate::error::FuseError;
use crate::wire::Writer;

/// The FUSE notification codes slates sends (`enum fuse_notify_code`). They ride in the reply
/// header's `error` field, positive (unlike an errno, which is negated).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Notify {
  /// Format: FUSE_NOTIFY_INVAL_INODE — drop an inode's cached attributes and a data range.
  InvalInode = 2,
  /// Format: FUSE_NOTIFY_INVAL_ENTRY — drop a cached name → node mapping in a directory.
  InvalEntry = 3,
  /// Format: FUSE_NOTIFY_DELETE — like InvalEntry, and the child was deleted.
  Delete = 6,
}

impl Notify {
  /// The code's wire value.
  pub fn code(self) -> i32 {
    self as i32
  }
}

/// Writes a notification header (`fuse_out_header` with unique 0 and the code in `error`) then
/// `body` into `out`; returns the bytes written, or a refusal when the buffer is too small.
fn write_notification(code: i32, body: &[u8], out: &mut [u8]) -> Result<usize, FuseError> {
  let total = OUT_HEADER_LEN.saturating_add(body.len());
  if out.len() < total {
    return Err(FuseError::ReplyTooSmall {
      have: out.len(),
      need: total,
    });
  }
  let mut w = Writer::new();
  w.u32(u32::try_from(total).unwrap_or(u32::MAX));
  w.u32(code.cast_unsigned());
  w.u64(0); // unique: zero marks an unsolicited notification
  w.bytes(body);
  out[..total].copy_from_slice(w.as_bytes());
  Ok(total)
}

/// Encodes a `FUSE_NOTIFY_INVAL_INODE`: drop `ino`'s cached attributes and the data range
/// `[off, off + len)` (a negative `off` — passed as `i64` — drops attributes only, no data).
/// `fuse_notify_inval_inode_out`: ino (8), off (8), len (8).
pub fn inval_inode(ino: u64, off: i64, len: i64, out: &mut [u8]) -> Result<usize, FuseError> {
  let mut w = Writer::new();
  w.u64(ino);
  w.u64(off.cast_unsigned());
  w.u64(len.cast_unsigned());
  write_notification(Notify::InvalInode.code(), &w.into_bytes(), out)
}

/// Encodes a `FUSE_NOTIFY_INVAL_ENTRY`: drop the cached mapping of `name` in directory
/// `parent`. `fuse_notify_inval_entry_out`: parent (8), namelen (4), flags (4), then the name
/// and a terminating NUL.
pub fn inval_entry(parent: u64, name: &str, out: &mut [u8]) -> Result<usize, FuseError> {
  let mut w = Writer::new();
  w.u64(parent);
  w.u32(u32::try_from(name.len()).unwrap_or(u32::MAX));
  w.u32(0); // flags
  w.bytes(name.as_bytes());
  w.bytes(&[0]); // the name is NUL-terminated on the wire
  write_notification(Notify::InvalEntry.code(), &w.into_bytes(), out)
}

/// Encodes a `FUSE_NOTIFY_DELETE`: like `inval_entry`, and `child` was removed.
/// `fuse_notify_delete_out`: parent (8), child (8), namelen (4), padding (4), then the name.
pub fn delete(parent: u64, child: u64, name: &str, out: &mut [u8]) -> Result<usize, FuseError> {
  let mut w = Writer::new();
  w.u64(parent);
  w.u64(child);
  w.u32(u32::try_from(name.len()).unwrap_or(u32::MAX));
  w.u32(0); // padding
  w.bytes(name.as_bytes());
  w.bytes(&[0]);
  write_notification(Notify::Delete.code(), &w.into_bytes(), out)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: the offset of the notification code in the header, and of the body.
  const AT_ERROR: usize = 4;

  fn code_of(out: &[u8]) -> i32 {
    i32::from_le_bytes(out[AT_ERROR..AT_ERROR + 4].try_into().unwrap())
  }

  fn unique_of(out: &[u8]) -> u64 {
    u64::from_le_bytes(out[8..16].try_into().unwrap())
  }

  /// An inode invalidation carries the code, a zero unique, and the ino/off/len body.
  #[test]
  fn inval_inode_encodes_the_code_and_body() {
    let mut out = [0u8; 64];
    let n = inval_inode(7, 0, -1, &mut out).unwrap();
    assert_eq!(n, OUT_HEADER_LEN + 24);
    assert_eq!(code_of(&out), Notify::InvalInode.code());
    assert_eq!(unique_of(&out), 0, "a notification's unique is zero");
    assert_eq!(
      u64::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap()),
      7
    );
    // len -1 (attributes only) rides as an all-ones u64.
    let len_at = OUT_HEADER_LEN + 16;
    assert_eq!(
      u64::from_le_bytes(out[len_at..len_at + 8].try_into().unwrap()),
      u64::MAX
    );
  }

  /// An entry invalidation carries the parent, the name length, and the NUL-terminated name.
  #[test]
  fn inval_entry_encodes_the_name() {
    let mut out = [0u8; 64];
    let n = inval_entry(1, "stale.txt", &mut out).unwrap();
    assert_eq!(code_of(&out), Notify::InvalEntry.code());
    assert_eq!(
      u64::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap()),
      1
    );
    let namelen_at = OUT_HEADER_LEN + 8;
    assert_eq!(
      u32::from_le_bytes(out[namelen_at..namelen_at + 4].try_into().unwrap()),
      9
    );
    let name_at = OUT_HEADER_LEN + 16;
    assert_eq!(&out[name_at..name_at + 9], b"stale.txt");
    assert_eq!(out[name_at + 9], 0, "NUL-terminated");
    assert_eq!(n, name_at + 10);
  }

  /// A delete notification carries the parent, the child, and the name.
  #[test]
  fn delete_encodes_parent_child_and_name() {
    let mut out = [0u8; 64];
    let n = delete(1, 5, "gone", &mut out).unwrap();
    assert_eq!(code_of(&out), Notify::Delete.code());
    assert_eq!(
      u64::from_le_bytes(
        out[OUT_HEADER_LEN + 8..OUT_HEADER_LEN + 16]
          .try_into()
          .unwrap()
      ),
      5
    );
    assert!(n > OUT_HEADER_LEN);
  }

  /// An undersized buffer is refused, not written out of bounds.
  #[test]
  fn a_small_buffer_is_refused() {
    let mut tiny = [0u8; 8];
    assert!(matches!(
      inval_inode(1, 0, 0, &mut tiny),
      Err(FuseError::ReplyTooSmall { .. })
    ));
  }
}
