//! The recall gate (RFC 8881 §10.2; A-79): a change to a delegated file's inode is refused before anything is touched,
//! whatever the path, and its recall is queued once; a read is not refused; once the inode leaves the gate the change
//! proceeds.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_vfs::error::VfsError;

mod common;
use common::{store, volume};

/// Writes, truncates, chmods, renames and unlinks `file` (named `held` in `dir`), expecting each refused `Delegated`.
fn every_change_is_refused(
  vol: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
  file: slates_vfs::ids::InodeNo,
  dir: slates_mem::handle::Handle<slates_vfs::dir::DirNode>,
) {
  assert_eq!(
    vol.write(store, file, 0, b"after!"),
    Err(VfsError::Delegated)
  );
  assert_eq!(vol.truncate(store, file, 0), Err(VfsError::Delegated));
  assert_eq!(vol.chmod(store, file, 0o600), Err(VfsError::Delegated));
  assert_eq!(
    vol.rename(store, dir, "held", dir, "moved"),
    Err(VfsError::Delegated)
  );
  assert_eq!(vol.unlink(store, dir, "held"), Err(VfsError::Delegated));
}

/// A-79: do delegate a file's inode, then write, truncate, chmod, rename and unlink it; expect every change refused
/// `Delegated` with the file's bytes, mode and name unchanged and one recall queued for it. Read it; expect its bytes.
/// Release it; expect the write to succeed.
#[test]
fn a_delegated_inode_refuses_every_change_and_queues_one_recall() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "held", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"before").unwrap();
  let dir = vol.root();
  store.recall_gate.delegate(file.0);
  every_change_is_refused(&mut vol, &mut store, file, dir);
  let mut buf = [0u8; 6];
  assert_eq!(
    vol.read(&store, file, 0, &mut buf),
    Ok(6),
    "a read is not a change"
  );
  assert_eq!(&buf, b"before", "nothing changed");
  assert!(
    vol.lookup_no(&store, root, "held").is_ok(),
    "the name is still there"
  );
  assert_eq!(
    store.recall_gate.take_requested(),
    vec![file.0],
    "one recall, however many refusals"
  );
  store.recall_gate.release(file.0);
  assert_eq!(vol.write(&mut store, file, 0, b"after!"), Ok(6));
}

/// A-79: do refuse a change at the gate, then park on it; expect the refusal counted (so a caller can tell the gate's
/// refusal from any other), the park to wait while the inode stays delegated, and to end once it is released.
#[test]
fn a_parked_caller_waits_until_the_inode_is_released() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "held", 0o644).unwrap();
  store.recall_gate.delegate(file.0);
  let before = store.recall_gate.refusals();
  assert_eq!(
    vol.write(&mut store, file, 0, b"x"),
    Err(VfsError::Delegated)
  );
  assert_eq!(
    store.recall_gate.refusals(),
    before + 1,
    "the gate's refusal"
  );
  let seen = store.recall_gate.generation();
  let waker = std::task::Waker::noop();
  assert!(
    !store.recall_gate.wait_for_release(seen, waker),
    "still delegated: wait"
  );
  assert!(
    !store.recall_gate.wait_for_release(seen, waker),
    "a second park replaces the first"
  );
  store.recall_gate.release(file.0);
  assert!(
    store.recall_gate.wait_for_release(seen, waker),
    "released: go"
  );
  assert_eq!(vol.write(&mut store, file, 0, b"x"), Ok(1));
}
