//! The store's directory under its standing grant, on the host's real disk (A-767): files created and opened relative to
//! the granted directory, other directories and names that leave it refused, symlinks not followed. The directories are
//! real ones in the build output (`CARGO_TARGET_TMPDIR`), never `/tmp` (R1).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

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
  std::os::unix::fs::symlink(outside.join("victim"), dir.join("link")).unwrap();
  let root = StoreRoot::open(&dir, &grant_for(&dir)).unwrap();
  assert!(root.open_file("link").is_err());
  assert!(!outside.join("victim").exists());
}
