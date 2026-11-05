//! The merge plane under many clients (§4.16 "Submission", "Rebase, the only corrective path"; Phase 9): the cases the
//! many-client load test (`crates/client/examples/load_bench.rs`) found, each reduced to the fewest clients that show
//! it, driven through the public client against a daemon.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use slates_client::{Client, ReadAt, Rebased, Submitted, VolumeId};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the shards of the test daemon; the green and its works live on one owner shard whatever the count.
const TEST_SHARDS: u16 = 2;
/// Shape: the length of the one file the scenarios edit.
const FILE_BYTES: usize = 512;
/// Shape: the slot two clients overwrite, `[SLOT_AT, SLOT_AT + SLOT_LEN)`.
const SLOT_AT: u64 = 128;
/// Shape: the slot's length.
const SLOT_LEN: u64 = 64;

fn daemon(name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("merge-load-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-merge-load-{name}"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// A green whose version 1 holds `f`, `FILE_BYTES` of `-`.
fn green_with_file(client: &mut Client) -> VolumeId {
  let green = client.create_green("g", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  client.edit(seed, "f", 0, 0, &[b'-'; FILE_BYTES]).unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  green
}

fn slot(fill: u8) -> Vec<u8> {
  vec![fill; usize::try_from(SLOT_LEN).unwrap()]
}

/// §4.16 "Rebase, the only corrective path" (found 2026-11-04 by the load test at ten clients over the Linux kernel's
/// `kernel/` directory: 448 submits refused, seven clients starved). Do: two works over version 1 overwrite the same
/// slot of `f`; A submits and is accepted at 2; B's submit conflicts on the slot. B resolves as the design says: it
/// reads version 2's bytes of the window, writes them over the window in its work, rebases (onto 2), writes its own
/// bytes over the slot again, and submits. Expect the rebase to answer `Rebased(2)` and the submit `Accepted(3)` —
/// nothing was accepted after 2 — and version 3's file to hold B's slot.
#[test]
fn a_conflict_resolved_by_rebase_then_resubmitted_is_accepted() {
  let (daemon, instance) = daemon("resolve");
  let mut client = common::client::connect(&instance);
  let green = green_with_file(&mut client);
  let (a, _) = client.create_work(green, "a").unwrap();
  let (b, _) = client.create_work(green, "b").unwrap();
  client.edit(a, "f", SLOT_AT, SLOT_LEN, &slot(b'A')).unwrap();
  client.edit(b, "f", SLOT_AT, SLOT_LEN, &slot(b'B')).unwrap();
  assert_eq!(client.submit(a).unwrap(), Submitted::Accepted(2));
  let Submitted::Conflict(windows) = client.submit(b).unwrap() else {
    panic!("B's overlapping slot conflicts with A's");
  };
  assert_eq!(windows.len(), 1, "one window: {windows:?}");
  let window = &windows[0];
  assert_eq!(
    (window.path.as_str(), window.at, window.len),
    ("f", SLOT_AT, SLOT_LEN)
  );
  let theirs = client
    .read(green, "f", ReadAt::Version { version: 2 })
    .unwrap();
  let start = usize::try_from(window.at).unwrap();
  let end = start + usize::try_from(window.len).unwrap();
  client
    .edit(b, "f", window.at, window.len, &theirs[start..end])
    .unwrap();
  assert_eq!(
    client.rebase(b).unwrap(),
    Rebased::Rebased(2),
    "B's window now equals the head's"
  );
  client.edit(b, "f", SLOT_AT, SLOT_LEN, &slot(b'B')).unwrap();
  assert_eq!(
    client.submit(b).unwrap(),
    Submitted::Accepted(3),
    "nothing was accepted after B's base: the resubmit merges"
  );
  let merged = client
    .read(green, "f", ReadAt::Version { version: 3 })
    .unwrap();
  assert_eq!(
    &merged[start..end],
    &slot(b'B')[..],
    "version 3 holds B's slot"
  );
  drop(client);
  daemon.stop();
}

/// §4.16 data model (the increment identity names its work volume and base; a sibling of the resubmit case above).
/// Do: two works over version 1 overwrite the same slot of `f` with the same bytes; A submits, then B. Expect A
/// `Accepted(2)` and B judged as its own increment: its overlap is identical to the head's bytes (pass two), so it is
/// accepted as a version of its own, `Accepted(3)`, and the head is 3. Before: the identity hashed only the ops
/// document and the post-state, so B's increment was A's, and B was answered from A's idempotency record —
/// `Accepted(2)`, a version B's submit did not make.
#[test]
fn two_works_declaring_the_same_change_are_two_increments() {
  let (daemon, instance) = daemon("same-change");
  let mut client = common::client::connect(&instance);
  let green = green_with_file(&mut client);
  let (a, _) = client.create_work(green, "a").unwrap();
  let (b, _) = client.create_work(green, "b").unwrap();
  client.edit(a, "f", SLOT_AT, SLOT_LEN, &slot(b'X')).unwrap();
  client.edit(b, "f", SLOT_AT, SLOT_LEN, &slot(b'X')).unwrap();
  assert_eq!(client.submit(a).unwrap(), Submitted::Accepted(2));
  assert_eq!(
    client.submit(b).unwrap(),
    Submitted::Accepted(3),
    "B's identical overlap is its own increment, accepted as its own version"
  );
  assert_eq!(client.versions(green).unwrap(), 3);
  drop(client);
  daemon.stop();
}

/// §4.16 "The verdict" and "Commit" (the reply is `Conflict{windows}`, the windows the agent rebases against; found
/// 2026-11-04 by the load test at a hundred clients: every one of 19 unexplained conflicts named a strict subset of
/// the overlaps, and the clients needed 572 extra rebases to find the rest one at a time). Do: A overwrites two
/// non-adjacent slots of `f` and is accepted; B overwrites the same two slots with other bytes and a third slot A never
/// touched, and submits. Expect one conflict reply naming both overlapping slots, in file order, and not the third.
/// Before: the content verdict returned at the first differing span of a file, so B was told of one window.
#[test]
fn a_conflict_names_every_overlapping_window_of_a_file() {
  let (daemon, instance) = daemon("every-window");
  let mut client = common::client::connect(&instance);
  let green = green_with_file(&mut client);
  let (a, _) = client.create_work(green, "a").unwrap();
  let (b, _) = client.create_work(green, "b").unwrap();
  let slots = [0, SLOT_AT];
  for at in slots {
    client.edit(a, "f", at, SLOT_LEN, &slot(b'A')).unwrap();
    client.edit(b, "f", at, SLOT_LEN, &slot(b'B')).unwrap();
  }
  // Two slots past A's second, so the deriver keeps it apart from B's own overlap (adjacent overwrites compose into
  // one operation and one window).
  let untouched = SLOT_AT + 4 * SLOT_LEN;
  client
    .edit(b, "f", untouched, SLOT_LEN, &slot(b'b'))
    .unwrap();
  assert_eq!(client.submit(a).unwrap(), Submitted::Accepted(2));
  let Submitted::Conflict(windows) = client.submit(b).unwrap() else {
    panic!("B overlaps A twice");
  };
  let named: Vec<(&str, u64, u64)> = windows
    .iter()
    .map(|w| (w.path.as_str(), w.at, w.len))
    .collect();
  assert_eq!(
    named,
    vec![("f", 0, SLOT_LEN), ("f", SLOT_AT, SLOT_LEN)],
    "both overlaps, and not the slot A never touched"
  );
  drop(client);
  daemon.stop();
}
