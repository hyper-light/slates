//! §4.6, the Linux cache posture: "Infinite cache lifetimes require proven invalidation delivery and recovery for every
//! mutation source." A volume wakes the tasks parked on it when it changes, so a transport whose kernel caches the
//! volume — a FUSE mount — is told when a change is made, not when its kernel next asks. Record:
//! `docs/bugs/2026-10-21-a-fuse-mount-hears-of-another-paths-change-only-on-its-next-request.md`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::{counting_waker, store, volume};

/// §4.6: do park a counting waker on a volume, read a file through it, then write the file. Expect no wake from the
/// read, exactly one from the write — a second park by the same task replaced the first rather than adding a second
/// entry — the generation moved, and a park on the old generation answered at once.
#[test]
fn a_parked_task_is_woken_by_a_change_and_not_by_a_read() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  let seen = vol.change_generation();
  let (counter, waker) = counting_waker();
  assert!(
    !vol.wait_for_change(seen, &waker),
    "nothing changed yet: park"
  );
  assert!(
    !vol.wait_for_change(seen, &waker),
    "a second park by the same task replaces the first"
  );
  let mut buf = [0u8; 1];
  let _ = vol.read(&store, file, 0, &mut buf);
  assert_eq!(
    counter.wakes(),
    0,
    "a read changes nothing and wakes no one"
  );
  assert_eq!(vol.write(&mut store, file, 0, b"x"), Ok(1));
  assert_eq!(
    counter.wakes(),
    1,
    "the write woke the parked task exactly once: one entry, not two"
  );
  assert_ne!(vol.change_generation(), seen, "the generation moved");
  assert!(
    vol.wait_for_change(seen, &waker),
    "a park on a generation already passed is answered at once"
  );
}
