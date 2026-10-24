//! The differential against e2fsprogs (A-160): every image the writer makes is one `e2fsck -fn` passes, and one in
//! which `debugfs` reads every file's bytes and sees each node's kind, links, owner and target as written. e2fsprogs
//! is the reference implementation of the format, and the kernel's driver is held to the same structures. The image
//! reaches it through a memory file the child inherits (`/proc/self/fd/N`), so nothing touches disk. Linux only, and
//! skipped loudly where e2fsprogs is absent.
#![cfg(target_os = "linux")]
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Command;

use common::{Contents, node, sample, write};
use slates_image::layout::{BLOCK_BYTES, POINTERS};
use slates_image::{Kind, NodeId, Tree, TreeBuilder};

/// Whether `tool` is on the path.
fn present(tool: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {tool}")])
    .output()
    .is_ok_and(|out| out.status.success())
}

/// `bytes` in a memory file a child process inherits (no close-on-exec), and the path a child opens it by.
fn memory_file(bytes: &[u8]) -> (OwnedFd, String) {
  let fd = rustix::fs::memfd_create("slates-image-test", rustix::fs::MemfdFlags::empty()).unwrap();
  let mut written = 0;
  while written < bytes.len() {
    written += rustix::io::write(&fd, &bytes[written..]).unwrap();
  }
  let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
  (fd, path)
}

/// Shape: how long one e2fsprogs run may take before the test gives up on it: a clean check of these images takes
/// well under a second. Measured 2026-10-23: e2fsck 1.47.4 deadlocks on its own cache lock in pass 5 when it meets an
/// error it would fix in an image opened read-only (`pthread_mutex_lock` under `io_channel_set_options`, from
/// `ext2fs_rw_bitmaps`), so a corrupt image would otherwise hang the suite rather than fail it.
const TOOL_WAIT_SECONDS: &str = "120";
/// Shape: how long after [`TOOL_WAIT_SECONDS`]'s `SIGTERM` an e2fsprogs run is killed outright. e2fsck catches
/// `SIGTERM` to cancel cleanly, and a deadlocked e2fsck never reaches its cancel, so the terminate alone left it
/// running (a 15-minute hang, 2026-10-23).
const TOOL_KILL_AFTER_SECONDS: &str = "10";

/// `tool` with `args`, run under coreutils' `timeout` so a hung run fails the test with what it printed.
fn bounded(tool: &str, args: &[&str]) -> std::process::Output {
  Command::new("timeout")
    .args([
      "--kill-after",
      TOOL_KILL_AFTER_SECONDS,
      TOOL_WAIT_SECONDS,
      tool,
    ])
    .args(args)
    .output()
    .unwrap()
}

/// Runs `debugfs -R request` on the image at `path`; its standard output.
fn debugfs(path: &str, request: &str) -> Vec<u8> {
  let out = bounded("debugfs", &["-R", request, path]);
  assert!(
    out.status.success(),
    "debugfs {request}: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  out.stdout
}

/// Shape: the files of one image `debugfs cat` reads back byte for byte, spread across it: every file's name, mode,
/// owner and size are checked by listing, and each `cat` is a process, which on a loaded box costs tens of
/// milliseconds.
const CAT_SAMPLE: usize = 12;

/// One line of `debugfs ls -p`: `/inode/mode/uid/gid/name/size/`, the size empty for a directory.
#[derive(Debug, PartialEq, Eq)]
struct Listed {
  mode: u32,
  uid: u32,
  gid: u32,
  name: Vec<u8>,
  size: Option<u64>,
}

/// The entries `debugfs ls -p` lists for directory `at` (`.` and `..` left out).
fn listing(path: &str, at: &str) -> Vec<Listed> {
  let out = debugfs(path, &format!("ls -p \"{at}\""));
  let mut listed = Vec::new();
  for line in out
    .split(|byte| *byte == b'\n')
    .filter(|line| !line.is_empty())
  {
    let fields: Vec<&[u8]> = line.split(|byte| *byte == b'/').collect();
    let text = |index: usize| String::from_utf8(fields[index].to_vec()).unwrap();
    let name = fields[5].to_vec();
    if name == b"." || name == b".." {
      continue;
    }
    listed.push(Listed {
      mode: u32::from_str_radix(&text(2), 8).unwrap(),
      uid: text(3).parse().unwrap(),
      gid: text(4).parse().unwrap(),
      name,
      size: text(6).parse().ok(),
    });
  }
  listed.sort_by(|left, right| left.name.cmp(&right.name));
  listed
}

/// What `debugfs ls -p` should list for `child`, named `name`.
fn expected_listing(tree: &Tree, name: &[u8], child: NodeId) -> Listed {
  let node = tree.node(child).unwrap();
  let (type_bits, size) = match &node.kind {
    Kind::Directory => (0o040_000, None),
    Kind::File { size } => (0o100_000, Some(*size)),
    Kind::Symlink { target } => (0o120_000, Some(u64::try_from(target.len()).unwrap())),
    Kind::CharDevice { .. } => (0o020_000, Some(0)),
    Kind::BlockDevice { .. } => (0o060_000, Some(0)),
    Kind::Fifo => (0o010_000, Some(0)),
    Kind::Socket => (0o140_000, Some(0)),
  };
  Listed {
    mode: type_bits | u32::from(node.permissions),
    uid: node.uid,
    gid: node.gid,
    name: name.to_vec(),
    size,
  }
}

/// Checks the image of `tree` with `contents` at `capacity`: e2fsck passes it, debugfs lists every directory's entries
/// with their modes, owners and sizes as written, and reads a spread of its files byte for byte.
fn check(what: &str, tree: &Tree, contents: &Contents, capacity: u64) {
  let (image, written, _) = write(tree, contents, capacity, u64::MAX);
  let (_fd, path) = memory_file(&image.bytes);
  let fsck = bounded("e2fsck", &["-fn", &path]);
  assert!(
    fsck.status.success(),
    "{what}: e2fsck -fn exit {:?}\n{}{}",
    fsck.status.code(),
    String::from_utf8_lossy(&fsck.stdout),
    String::from_utf8_lossy(&fsck.stderr)
  );
  let mut stack = vec![(String::new(), NodeId::ROOT)];
  let mut files = Vec::new();
  while let Some((prefix, directory)) = stack.pop() {
    let at = if prefix.is_empty() {
      "/".to_owned()
    } else {
      prefix.clone()
    };
    let expected: Vec<Listed> = tree
      .entries(directory)
      .map(|(name, child)| expected_listing(tree, name, child))
      .collect();
    assert_eq!(
      listing(&path, &at),
      expected,
      "{what}: {at} lists as written"
    );
    for (name, child) in tree.entries(directory) {
      let child_at = format!("{prefix}/{}", String::from_utf8(name.to_vec()).unwrap());
      match &tree.node(child).unwrap().kind {
        Kind::Directory => stack.push((child_at, child)),
        Kind::File { .. } => files.push((child_at, child)),
        _ => {}
      }
    }
  }
  let step = files.len().div_ceil(CAT_SAMPLE).max(1);
  for (at, child) in files.iter().step_by(step) {
    let expected = contents
      .files
      .get(&child.index())
      .cloned()
      .unwrap_or_default();
    assert!(
      debugfs(&path, &format!("cat \"{at}\"")) == expected,
      "{what}: {at} reads as written"
    );
  }
  assert!(
    !files.is_empty() && written.blocks_used > 0,
    "{what}: files were checked"
  );
}

/// A-160 (the differential). Do: write the sample tree (every kind, a hard link, a fast and a slow symlink), a tree of
/// files at every block-map boundary through the double-indirect level, a directory of 600 entries (several blocks),
/// and the sample in an image spanning two groups (a superblock copy in group 1); run `e2fsck -fn` on each, list every
/// directory with `debugfs`, and read a spread of files. Expect e2fsck to pass every image, every entry listed with its
/// mode, owner and size as written, and every file read byte for byte as written.
#[test]
fn every_image_passes_e2fsck_and_debugfs_reads_every_file_as_written() {
  if !present("e2fsck") || !present("debugfs") || !present("timeout") {
    eprintln!(
      "SKIP: e2fsprogs (e2fsck, debugfs) or coreutils' timeout is not installed, so the images are not checked against it"
    );
    return;
  }
  let (tree, contents) = sample();
  check("sample", &tree, &contents, 4 << 20);
  check("two groups", &tree, &contents, 130 << 20);
  let (tree, contents) = boundaries();
  check("block-map boundaries", &tree, &contents, 64 << 20);
  let (tree, contents) = wide();
  check("a wide directory", &tree, &contents, 8 << 20);
}

/// A-160 (what debugfs sees of each node). Do: write the sample and ask debugfs for the hard-linked file's links and
/// owner, the fast symlink's target, the device's number, and the root's listing. Expect each as written.
#[test]
fn debugfs_sees_each_nodes_kind_links_owner_and_target() {
  if !present("debugfs") || !present("timeout") {
    eprintln!("SKIP: debugfs is not installed, so the image's nodes are not checked against it");
    return;
  }
  let (tree, contents) = sample();
  let (image, _, _) = write(&tree, &contents, 4 << 20, u64::MAX);
  let (_fd, path) = memory_file(&image.bytes);
  let readme = String::from_utf8(debugfs(&path, "stat /README")).unwrap();
  assert!(readme.contains("Links: 2"), "{readme}");
  assert!(
    readme.contains("User:  1000") && readme.contains("Group:  1000"),
    "{readme}"
  );
  let fast = String::from_utf8(debugfs(&path, "stat /docs/fast")).unwrap();
  assert!(fast.contains("Fast link dest: \"../README\""), "{fast}");
  let null = String::from_utf8(debugfs(&path, "stat /null")).unwrap();
  assert!(
    null.contains("Type: character special") && null.contains("01:03"),
    "{null}"
  );
  let listing = String::from_utf8(debugfs(&path, "ls -l /")).unwrap();
  for name in ["docs", "README", "null"] {
    assert!(listing.contains(name), "{name} in {listing}");
  }
}

/// Files whose block counts sit at each boundary of an inode's map through the double-indirect level.
fn boundaries() -> (Tree, Contents) {
  let block = usize::try_from(BLOCK_BYTES).unwrap();
  let pointers = usize::try_from(POINTERS).unwrap();
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let mut contents = Contents::default();
  for (index, count) in [
    1,
    12,
    13,
    12 + pointers,
    12 + pointers + 1,
    12 + 2 * pointers + 1,
  ]
  .iter()
  .enumerate()
  {
    let length = count * block + 3;
    let bytes: Vec<u8> = (0..length)
      .map(|at| u8::try_from((at * 7 + index) % 253).unwrap())
      .collect();
    let id = builder.add_node(node(Kind::File {
      size: u64::try_from(length).unwrap(),
    }));
    contents.files.insert(id.index(), bytes);
    builder.add_entry(NodeId::ROOT, format!("f{index}").into_bytes(), id);
  }
  (builder.build().unwrap(), contents)
}

/// A directory of 600 entries, so its records fill several blocks.
fn wide() -> (Tree, Contents) {
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let mut contents = Contents::default();
  let directory = builder.add_node(node(Kind::Directory));
  builder.add_entry(NodeId::ROOT, b"wide".to_vec(), directory);
  for index in 0..600 {
    let bytes = format!("entry {index}\n").into_bytes();
    let id = builder.add_node(node(Kind::File {
      size: u64::try_from(bytes.len()).unwrap(),
    }));
    contents.files.insert(id.index(), bytes);
    builder.add_entry(
      directory,
      format!("a-rather-long-file-name-number-{index:04}").into_bytes(),
      id,
    );
  }
  (builder.build().unwrap(), contents)
}
