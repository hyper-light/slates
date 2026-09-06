//! Inode lifetime under references (§4.6, the inode-addressed-io design): a file unlinked while a
//! transport holds it open keeps its content until the last reference drops (POSIX
//! unlink-while-open), and its content is reclaimed exactly at that last reference. Driven against
//! an in-memory scratch volume on every host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{store, volume};
use slates_vfs::ids::InodeNo;

/// An inode unlinked while a reference is held keeps its content until the last reference drops,
/// then is reclaimed. Without the deferral this fails: `drop_link` releases the content at
/// `nlink == 0` regardless of open references (the gap Ada identified at `drop_link`).
#[test]
fn an_unlinked_referenced_inode_survives_until_the_last_reference() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"hello").unwrap();

  // A transport holds the file open.
  vol.reference(&store, file).unwrap();
  // It is unlinked from the namespace.
  vol.unlink_no(&mut store, root, "f").unwrap();
  assert!(
    vol.lookup_no(&store, root, "f").is_err(),
    "the name is gone after unlink"
  );

  // The open object still serves its content.
  let mut buf = [0u8; 5];
  let read = vol.read(&store, file, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"hello",
    "an unlinked-but-open file keeps its content"
  );

  // The last close reclaims it.
  vol.unreference(&mut store, file).unwrap();
  assert!(
    vol.read(&store, file, 0, &mut buf).is_err(),
    "the content is reclaimed after the last reference"
  );
}

/// A file replaced by a rename while it is held open keeps its own content until the last
/// reference drops; the name resolves to the renamed file.
#[test]
fn rename_over_an_open_file_preserves_it() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let a = vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  vol.write(&mut store, a, 0, b"aaa").unwrap();
  let b = vol.create_file_no(&mut store, root, "b", 0o644).unwrap();
  vol.write(&mut store, b, 0, b"bbb").unwrap();

  // Hold b open, then rename a over b, so b's inode leaves the namespace.
  vol.reference(&store, b).unwrap();
  vol.rename_no(&mut store, root, "a", root, "b").unwrap();

  // b's replaced inode still serves its own content.
  let mut buf = [0u8; 3];
  let read = vol.read(&store, b, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"bbb",
    "the replaced-but-open file keeps its content"
  );
  // The name now resolves to the renamed file.
  assert_eq!(
    vol.lookup_no(&store, root, "b").unwrap().inode,
    a,
    "the name resolves to the renamed file"
  );

  // The last close reclaims the replaced inode.
  vol.unreference(&mut store, b).unwrap();
  assert!(
    vol.read(&store, b, 0, &mut buf).is_err(),
    "the replaced inode is reclaimed after the last reference"
  );
}

/// With several references, the content is reclaimed only at the last unreference, not the first.
#[test]
fn content_reclaims_only_at_the_last_of_several_references() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"data").unwrap();

  vol.reference(&store, file).unwrap();
  vol.reference(&store, file).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();

  let mut buf = [0u8; 4];
  vol.unreference(&mut store, file).unwrap();
  assert_eq!(
    vol.read(&store, file, 0, &mut buf).unwrap(),
    4,
    "still alive with one reference remaining"
  );
  vol.unreference(&mut store, file).unwrap();
  assert!(
    vol.read(&store, file, 0, &mut buf).is_err(),
    "reclaimed at the last reference"
  );
}

/// Writes reach an unlinked-but-open file, not just reads (POSIX unlink-while-open covers both).
#[test]
fn writes_reach_an_unlinked_open_file() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"hello").unwrap();
  vol.reference(&store, file).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();

  vol.write(&mut store, file, 5, b" world").unwrap();
  let mut buf = [0u8; 11];
  let read = vol.read(&store, file, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"hello world",
    "a write reaches an unlinked open file"
  );

  vol.unreference(&mut store, file).unwrap();
  assert!(
    vol.read(&store, file, 0, &mut buf).is_err(),
    "reclaimed after the last reference"
  );
}

/// A reference to an inode number no inode holds is refused, so the reference map cannot grow past
/// the inode table's cap (hardening: references are validated, not arbitrary).
#[test]
fn a_reference_to_an_absent_inode_is_refused() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  assert!(
    vol.reference(&store, InodeNo(999_999)).is_err(),
    "cannot reference an inode that does not exist"
  );
}

/// Reclamation frees the inode's content bytes for reuse; while the inode is orphaned (unlinked
/// but open) its bytes remain charged, since the content is still alive in RAM (Ada review point
/// 6: reusable capacity after reclamation).
#[test]
fn reclamation_frees_capacity_for_reuse() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, &[7u8; 4096]).unwrap();
  let used_before = vol.accounting().referenced_bytes;
  assert!(used_before >= 4096, "the write is accounted");

  vol.reference(&store, file).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();
  // While the inode is orphaned its content is alive, so its bytes are still charged.
  assert_eq!(
    vol.accounting().referenced_bytes,
    used_before,
    "an unlinked-but-open file still occupies its capacity"
  );

  // The last reference reclaims the content, freeing the capacity.
  vol.unreference(&mut store, file).unwrap();
  assert!(
    vol.accounting().referenced_bytes < used_before,
    "reclamation frees the capacity for reuse"
  );
}

/// A teardown sweep releases exactly the references one attachment holds and reclaims the inodes
/// that reach zero references and no links — a whole attachment's references dropped in one batch,
/// without a per-inode forget (FUSE does not guarantee one at unmount, §3).
#[test]
fn a_sweep_releases_an_attachments_references_and_reclaims() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"data").unwrap();

  // Attachment 7 references the file, then it is unlinked (deferred — the reference holds it).
  vol.reference_for(&store, f, 7).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();
  assert!(vol.stat(&store, f).is_ok(), "held open across unlink");

  // Tearing down attachment 7 sweeps its reference; the unlinked inode is reclaimed.
  vol.sweep_attachment(&mut store, 7).unwrap();
  assert!(
    vol.stat(&store, f).is_err(),
    "reclaimed at the attachment teardown sweep"
  );
  // Sweeping an attachment that holds nothing is a no-op.
  vol.sweep_attachment(&mut store, 7).unwrap();
}

/// The corruption guard: two attachments reference the same inode; sweeping one must NOT reclaim it
/// while the other still holds it. Per-attachment accounting (global == sum of per-attachment
/// counts) makes a sweep drop only its own share. A naive per-inode sweep would reclaim a live
/// object another attachment is still serving — data loss for that holder.
#[test]
fn a_sweep_does_not_reclaim_an_inode_another_attachment_holds() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol
    .create_file_no(&mut store, root, "shared", 0o644)
    .unwrap();
  vol.write(&mut store, f, 0, b"shared").unwrap();

  // Two attachments (e.g. two mounts of the volume) both reference the same inode.
  vol.reference_for(&store, f, 100).unwrap();
  vol.reference_for(&store, f, 200).unwrap();
  vol.unlink_no(&mut store, root, "shared").unwrap();

  // Attachment 100 tears down: its share is released, but 200 still holds the inode alive.
  vol.sweep_attachment(&mut store, 100).unwrap();
  let mut buf = [0u8; 6];
  let read = vol.read(&store, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"shared",
    "the inode survives one attachment's teardown — attachment 200 still holds it"
  );

  // Attachment 200 tears down: now no reference remains, so it is reclaimed.
  vol.sweep_attachment(&mut store, 200).unwrap();
  assert!(
    vol.read(&store, f, 0, &mut buf).is_err(),
    "reclaimed at the last attachment's teardown"
  );
}

/// `forget_for` drops only the forgetting attachment's share: a FORGET from one attachment (even one
/// asking to drop more than it holds) leaves another attachment's references — and the object —
/// intact, until that attachment forgets too.
#[test]
fn a_forget_drops_only_the_owners_share() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();

  vol.reference_for(&store, f, 1).unwrap();
  vol.reference_for(&store, f, 2).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();

  // Attachment 1 forgets more than it holds — capped at its own one reference; 2's remains.
  vol.forget_for(&mut store, f, 1, 99).unwrap();
  assert!(vol.stat(&store, f).is_ok(), "attachment 2 still holds it");

  // Attachment 2 forgets its reference — now reclaimed.
  vol.forget_for(&mut store, f, 2, 1).unwrap();
  assert!(
    vol.stat(&store, f).is_err(),
    "reclaimed once both attachments forgot"
  );
}
