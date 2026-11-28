//! The simulated host's open descriptors keep what a real host's keep alive, and copy nothing at the open (CLAUDE.md's
//! oracle-host gotchas: an oracle's own cost stays flat, and it models what the real host keeps alive).
//!
//! A-920 (main's ruling of 2026-11-27 17:29): `SimHost::open_file` cloned the whole file, its bytes and all, as the
//! open's snapshot. Release probe 18's wide-move test spent its longest step (1.4 ms, CPU) in the import's `begin`,
//! which is that copy of the test's dump: an oracle's cost inside the product's poll. A real open copies nothing and
//! keeps the old inode alive only once a rename or an unlink displaces it.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{HostFs, LandFs};

/// Shape: a file of a mebibyte, so a copy at the open shows in the bytes the opens hold.
const LARGE: usize = 1 << 20;

/// The bytes of a file of `len`, each its offset's residue mod 251.
fn patterned(len: usize) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from(at % 251).unwrap_or(u8::MAX))
    .collect()
}

/// Every byte `file` reads from its start.
fn read_whole(host: &mut SimHost, file: slates_vfs::host::HostFile, len: usize) -> Vec<u8> {
  let mut buf = vec![0u8; len];
  let read = host.read_at(file, 0, &mut buf).unwrap();
  buf.truncate(read);
  buf
}

/// Do: open a mebibyte file on a simulated host. Expect: the opens hold no copy of a file's bytes
/// (`SimHost::open_bytes_held`), as a real descriptor holds none. Red before the cut: the open's snapshot held the
/// file's mebibyte.
#[test]
fn a_sim_hosts_open_copies_none_of_its_files_bytes() {
  let mut host = SimHost::new();
  host.replace_file("/large", &patterned(LARGE));
  let root = host.root();
  let file = host.open_file(root, "large").unwrap();
  assert_eq!(
    host.open_bytes_held(),
    0,
    "the open copied the file's bytes"
  );
  assert_eq!(read_whole(&mut host, file, LARGE), patterned(LARGE));
}

/// Do: open files, then displace each as a real host's descriptors survive it: the host's `unlink`, a rename over the
/// file (the outsider's), a removal of the directory holding one with everything beneath, and an exchange; and open a
/// directory to change its mode, then `rmdir` it. Expect: each open file still reads its bytes from before, as the
/// descriptor keeps its inode alive, and the directory's descriptor still answers its `fstat`.
#[test]
fn a_sim_hosts_open_reads_its_bytes_after_an_unlink_a_rename_over_a_removal_and_an_exchange() {
  let mut host = SimHost::new();
  host.replace_file("/unlinked", b"unlinked before");
  host.replace_file("/renamed-over", b"renamed over before");
  host.replace_file("/other", b"the other file");
  host.mkdir("/tree");
  host.replace_file("/tree/beneath", b"beneath before");
  host.replace_file("/a", b"a before");
  host.replace_file("/b", b"b before");
  host.mkdir("/empty");
  let root = host.root();
  let tree = host.open_dir(root, "tree").unwrap();
  let opened: Vec<_> = [
    (root, "unlinked"),
    (root, "renamed-over"),
    (tree, "beneath"),
    (root, "a"),
  ]
  .into_iter()
  .map(|(dir, name)| host.open_file(dir, name).unwrap())
  .collect();
  let empty = host.open_dir_to_chmod(root, "empty").unwrap();
  host.unlink(root, "unlinked").unwrap();
  host.rename("/other", "/renamed-over");
  host.remove("/tree");
  host.exchange(root, "a", "b").unwrap();
  host.rmdir(root, "empty").unwrap();
  let read: Vec<Vec<u8>> = opened
    .into_iter()
    .map(|file| read_whole(&mut host, file, 64))
    .collect();
  assert_eq!(
    read,
    [
      b"unlinked before".to_vec(),
      b"renamed over before".to_vec(),
      b"beneath before".to_vec(),
      b"a before".to_vec(),
    ]
  );
  assert!(
    host.fstat(empty).is_ok(),
    "the removed directory's descriptor answers"
  );
}
