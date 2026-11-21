//! The store's directory under its standing grant, on the host's real disk (A-767): files created and opened relative to
//! the granted directory, other directories and names that leave it refused, symlinks not followed. The directories are
//! real ones in the build output (`CARGO_TARGET_TMPDIR`), never `/tmp` (R1). On Unix and on Windows (the store's
//! Windows lane): each platform's link and refusal code behind a paired helper.
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing
)]

use std::fs::File;
use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot, StoreRootRefusal};

/// A directory of the build's own scratch (never `/tmp`, R1), made fresh for one test.
fn scratch(name: &str) -> PathBuf {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
    .join(format!("store-root-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

/// Writes all of `bytes` at `offset` of `file` (positioned I/O, as the store's crates write).
#[cfg(unix)]
fn write_at(file: &File, bytes: &[u8], offset: u64) {
  std::os::unix::fs::FileExt::write_all_at(file, bytes, offset).unwrap();
}

/// Writes all of `bytes` at `offset` of `file` (positioned I/O, as the store's crates write).
#[cfg(windows)]
fn write_at(file: &File, bytes: &[u8], offset: u64) {
  let mut done = 0;
  while done < bytes.len() {
    let at = offset + u64::try_from(done).unwrap();
    done += std::os::windows::fs::FileExt::seek_write(file, &bytes[done..], at).unwrap();
  }
}

/// Reads `len` bytes at `offset` of `file`.
#[cfg(unix)]
fn read_at(file: &File, len: usize, offset: u64) -> Vec<u8> {
  let mut bytes = vec![0; len];
  std::os::unix::fs::FileExt::read_exact_at(file, &mut bytes, offset).unwrap();
  bytes
}

/// Reads `len` bytes at `offset` of `file`.
#[cfg(windows)]
fn read_at(file: &File, len: usize, offset: u64) -> Vec<u8> {
  let mut bytes = vec![0; len];
  let mut done = 0;
  while done < len {
    let at = offset + u64::try_from(done).unwrap();
    let read = std::os::windows::fs::FileExt::seek_read(file, &mut bytes[done..], at).unwrap();
    assert!(read > 0, "the file ends before {len} bytes at {offset}");
    done += read;
  }
  bytes
}

/// A symbolic link at `link` to the file `target`; `false` where this account may not make one (Windows without
/// Developer Mode or the privilege), which the caller skips loudly.
#[cfg(unix)]
fn file_link(target: &Path, link: &Path) -> bool {
  std::os::unix::fs::symlink(target, link).unwrap();
  true
}

/// A symbolic link at `link` to the file `target`; `false` where this account may not make one (Windows without
/// Developer Mode or the privilege), which the caller skips loudly.
#[cfg(windows)]
fn file_link(target: &Path, link: &Path) -> bool {
  std::os::windows::fs::symlink_file(target, link).is_ok()
}

/// A symbolic link at `link` to the directory `target`, or `false` as [`file_link`].
#[cfg(unix)]
fn dir_link(target: &Path, link: &Path) -> bool {
  std::os::unix::fs::symlink(target, link).unwrap();
  true
}

/// A symbolic link at `link` to the directory `target`, or `false` as [`file_link`].
#[cfg(windows)]
fn dir_link(target: &Path, link: &Path) -> bool {
  std::os::windows::fs::symlink_dir(target, link).is_ok()
}

/// The code a mount point where a file or a link lies is refused with: `ENOTDIR` on Unix, `ERROR_DIRECTORY` (267) on
/// Windows.
#[cfg(unix)]
fn not_a_directory() -> i32 {
  rustix::io::Errno::NOTDIR.raw_os_error()
}

/// The code a mount point where a file or a link lies is refused with: `ENOTDIR` on Unix, `ERROR_DIRECTORY` (267) on
/// Windows.
#[cfg(windows)]
fn not_a_directory() -> i32 {
  267
}

fn grant_for(path: &Path) -> StoreGrant {
  StoreGrant {
    repository: 7,
    target: StoreRoot::identify(path).unwrap(),
    issued_ns: 0,
  }
}

/// Do: open a granted directory, create a file, write it, open it again. Expect: the bytes are there, a second create
/// of the name is refused, and the name is found.
#[test]
fn a_granted_directory_creates_and_opens_its_files() {
  use std::io::{Read, Write};
  let dir = scratch("files");
  let root = StoreRoot::open(&dir, &grant_for(&dir)).unwrap();
  let mut file = root.create("log-0").unwrap();
  file.write_all(b"changeset").unwrap();
  drop(file);
  assert!(matches!(
    root.create("log-0"),
    Err(StoreRootRefusal::Io { .. })
  ));
  assert!(root.exists("log-0").unwrap());
  let mut again = root.open_file("log-0").unwrap();
  let mut read = String::new();
  again.read_to_string(&mut read).unwrap();
  assert_eq!(read, "changeset");
  assert_eq!(root.path_of("log-0"), dir.join("log-0"));
}

/// The store's Windows lane. Do: create a file in a granted directory, write two runs at offsets (the second past the
/// first's end), read them back by offset through a second open, and ask the volume's free bytes. Expect: each run reads
/// back where it was written, the gap between them zeros, the length the second run's end, and some bytes free.
#[test]
fn a_store_file_is_written_and_read_in_place() {
  let dir = scratch("positioned");
  let root = StoreRoot::open(&dir, &grant_for(&dir)).unwrap();
  let file = root.create("section-0").unwrap();
  write_at(&file, b"header", 0);
  write_at(&file, b"trailer", 4096);
  drop(file);
  let again = root.open_file("section-0").unwrap();
  assert_eq!(read_at(&again, 6, 0), b"header");
  assert_eq!(read_at(&again, 7, 4096), b"trailer");
  assert_eq!(read_at(&again, 16, 6), vec![0; 16]);
  assert_eq!(again.metadata().unwrap().len(), 4096 + 7);
  assert!(root.free_bytes().unwrap() > 0);
}

/// Do: open a directory with another directory's grant, and name files with separators and dots. Expect: each is
/// refused, and nothing is created outside the directory.
#[test]
fn another_directory_and_names_that_leave_it_are_refused() {
  let dir = scratch("refuse");
  let other = scratch("other");
  assert!(matches!(
    StoreRoot::open(&dir, &grant_for(&other)),
    Err(StoreRootRefusal::NotTheGrantsDirectory)
  ));
  let root = StoreRoot::open(&dir, &grant_for(&dir)).unwrap();
  for name in ["", ".", "..", "../escape", "a/b", "nul\0"] {
    assert!(
      matches!(root.create(name), Err(StoreRootRefusal::BadName)),
      "{name:?}"
    );
  }
  assert!(!dir.parent().unwrap().join("escape").exists());
}

/// Do: put a symlink in the directory and open it by name. Expect: refused (`O_NOFOLLOW`), its target untouched.
#[test]
fn a_symlink_in_the_directory_is_not_followed() {
  let dir = scratch("link");
  let outside = scratch("link-outside");
  if !file_link(&outside.join("victim"), &dir.join("link")) {
    println!(
      "SKIP: this account may not make a symbolic link here (Windows: no Developer Mode, no privilege)"
    );
    return;
  }
  let root = StoreRoot::open(&dir, &grant_for(&dir)).unwrap();
  assert!(root.open_file("link").is_err());
  assert!(!outside.join("victim").exists());
}

/// A mount point where a file and a symlink lie in `dir` refused `ENOTDIR`, both left as they were (the symlink's
/// target `elsewhere` untouched).
fn file_and_link_refused(root: &StoreRoot, dir: &Path, elsewhere: &Path) {
  for name in ["a-file", "a-link"] {
    assert!(
      matches!(root.mount_point(name), Err(StoreRootRefusal::Io { code, .. }) if code == not_a_directory()),
      "{name}"
    );
  }
  assert!(
    std::fs::symlink_metadata(dir.join("a-link"))
      .unwrap()
      .file_type()
      .is_symlink()
  );
  assert_eq!(std::fs::read_dir(elsewhere).unwrap().count(), 0);
}

/// A-645 (a). Do: make a mount point in a granted directory, make it again, then name one where a file and a symlink
/// lie, and one with a separator. Expect: the directory made, empty, at the granted path's child, and kept on the second
/// make; the file and the symlink refused `ENOTDIR` and left as they were (the symlink's target untouched); the
/// separator refused `BadName`.
#[test]
fn a_mount_point_is_made_in_the_granted_directory_and_nowhere_else() {
  let dir = scratch("mount-point");
  let elsewhere = scratch("mount-point-elsewhere");
  let root = StoreRoot::open(&dir, &grant_for(&dir)).unwrap();
  let point = root.mount_point("project.svn").unwrap();
  assert_eq!(point, dir.join("project.svn"));
  assert!(std::fs::metadata(&point).unwrap().is_dir());
  assert_eq!(std::fs::read_dir(&point).unwrap().count(), 0);
  assert_eq!(root.mount_point("project.svn").unwrap(), point);
  drop(root.create("a-file").unwrap());
  if !dir_link(&elsewhere, &dir.join("a-link")) {
    println!("SKIP the link's leg: this account may not make a symbolic link here");
    return;
  }
  file_and_link_refused(&root, &dir, &elsewhere);
  assert!(matches!(
    root.mount_point("a/b"),
    Err(StoreRootRefusal::BadName)
  ));
}
