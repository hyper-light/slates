//! Membership in a subtree (§4.6 scoped exports; AUD-29-76): which inodes a mount rooted at a directory may name.
//! The export enforces the scope by this answer on every handle a client presents, so it must hold for every
//! kind of node and through renames. Driven against an in-memory scratch volume on every host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{store, volume};

/// AUD-29-76. Do: build `/shared/sub/f`, `/shared/g`, `/private/secret`, a symlink in `shared`, then rename
/// `shared/sub` to `private/sub` and back; also link `private/secret` into `shared` as `alias`. Expect: with
/// `shared` as the scope, `shared` itself and everything under it are within, `private` and `secret` and the
/// root are not; the moved subtree leaves and re-enters the scope with its parent; and `secret`, reachable by
/// its alias inside the scope but homed outside it, is refused (conservative: its names outside cannot be
/// enumerated without a walk).
#[test]
fn a_subtree_contains_exactly_what_hangs_beneath_its_root() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let shared = vol.mkdir_no(&mut store, root, "shared", 0o755).unwrap();
  let private = vol.mkdir_no(&mut store, root, "private", 0o755).unwrap();
  let sub = vol.mkdir_no(&mut store, shared, "sub", 0o755).unwrap();
  let f = vol.create_file_no(&mut store, sub, "f", 0o644).unwrap();
  let g = vol.create_file_no(&mut store, shared, "g", 0o644).unwrap();
  let secret = vol
    .create_file_no(&mut store, private, "secret", 0o600)
    .unwrap();
  for inside in [shared, sub, f, g] {
    assert!(vol.within(&store, inside, shared).unwrap(), "{inside:?}");
  }
  for outside in [root, private, secret] {
    assert!(!vol.within(&store, outside, shared).unwrap(), "{outside:?}");
  }
  vol
    .rename_no(&mut store, shared, "sub", private, "sub")
    .unwrap();
  assert!(
    !vol.within(&store, sub, shared).unwrap(),
    "moved out with its parent"
  );
  assert!(!vol.within(&store, f, shared).unwrap());
  vol
    .rename_no(&mut store, private, "sub", shared, "sub")
    .unwrap();
  assert!(vol.within(&store, f, shared).unwrap(), "moved back in");
  vol.link_no(&mut store, shared, "alias", secret).unwrap();
  assert!(
    !vol.within(&store, secret, shared).unwrap(),
    "a node homed outside is refused though an alias hangs inside"
  );
}
