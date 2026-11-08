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
      name: common::fixture_object("slates-seg", &format!("merge-load-{name}")),
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

/// The bytes every shard has committed against its budget (§4.2), from the daemon's status.
fn committed(client: &mut Client) -> u64 {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .map(|shard| shard.committed_bytes)
    .sum()
}

/// A-207, §4.16 ("a work volume is only ever a clone of a green version") and D-5 (clones by reference), measured by §4.2's
/// all-cost admission (found 2026-11-04 by the load test: one client's `CreateWork` took 4.2 ms over 16 MB of the
/// Linux kernel, and a hundred clients' median 441 ms, since each work was a copy of its green). Do: a green holding
/// `FILES` files of `FILE_LEN` bytes; create `WORKS` works over it, edit one slot of one file in each, and submit
/// each. Expect the shards' committed bytes to grow by less than one copy of the green's files across all the works
/// (each work holds the one file it changed), the edits to be accepted at versions 2 through `WORKS + 1`, and every
/// work after its accepted submit to read the green's bytes of a file it never changed. Before: each work was
/// charged a whole copy at create and again at every reset, `WORKS` copies in all.
#[test]
fn works_over_a_green_share_its_files() {
  /// Shape: the green's files.
  const FILES: usize = 32;
  /// Shape: each file's length, so the green holds one mebibyte.
  const FILE_LEN: usize = 32 * 1024;
  /// Shape: the works created over it.
  const WORKS: usize = 64;
  let (daemon, instance) = daemon("share");
  let mut client = common::client::connect(&instance);
  let green = client.create_green("g", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  for file in 0..FILES {
    client
      .edit(
        seed,
        &format!("f{file}"),
        0,
        0,
        &vec![b'0' + u8::try_from(file % 10).unwrap(); FILE_LEN],
      )
      .unwrap();
  }
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let before = committed(&mut client);
  let mut works = Vec::new();
  for n in 0..WORKS {
    let (work, base) = client.create_work(green, &format!("w{n}")).unwrap();
    assert_eq!(base, 1);
    works.push(work);
  }
  let grown = committed(&mut client).saturating_sub(before);
  let one_copy = u64::try_from(FILES * FILE_LEN).unwrap();
  assert!(
    grown < one_copy,
    "{WORKS} works grew the committed bytes by {grown}, at least one copy of the green's {one_copy}"
  );
  for (n, work) in works.iter().enumerate() {
    let at = u64::try_from(n).unwrap() * SLOT_LEN;
    client.edit(*work, "f0", at, SLOT_LEN, &slot(b'w')).unwrap();
    assert_eq!(
      client.submit(*work).unwrap(),
      Submitted::Accepted(u64::try_from(n).unwrap() + 2)
    );
  }
  reads_after_resets(&mut client, &works, FILE_LEN);
  drop(client);
  daemon.stop();
}

/// After `works_over_a_green_share_its_files`' submits: the last work was reset to the head, every work's slot merged
/// into it; an earlier work stays at its own version.
fn reads_after_resets(client: &mut Client, works: &[VolumeId], file_len: usize) {
  let newest = works[works.len() - 1];
  let unchanged = client.read(newest, "f7", ReadAt::Head).unwrap();
  assert_eq!(
    unchanged,
    vec![b'7'; file_len],
    "a work reads the green's bytes of a file it never changed"
  );
  let changed = client.read(newest, "f0", ReadAt::Head).unwrap();
  let slots = usize::try_from(u64::try_from(works.len()).unwrap() * SLOT_LEN).unwrap();
  assert!(
    changed[..slots].iter().all(|byte| *byte == b'w'),
    "and the head's, every work's slot, after its reset"
  );
  let first = client.read(works[0], "f0", ReadAt::Head).unwrap();
  assert_eq!(
    first[usize::try_from(SLOT_LEN).unwrap()],
    b'0',
    "the first work stays at its own version, 2"
  );
}

/// A-207, §4.16 and D-18 ("Local acknowledgement promises daemon-restart survival only when bytes … and completion records are
/// recoverable from anchor-owned RAM"; BUG-11 closed that for a plain volume's content; found 2026-11-04 by the load
/// test's chaos plan). Do: a green holding `f`; work A overwrites a slot and is accepted (its log restated at version
/// 2), then overwrites another slot (logged); work B overwrites A's first slot, conflicts, writes version 2's bytes
/// over the window and rebases (restated at 2, its journal restated), then overwrites its own slot; stop the daemon and
/// start another over the same anchor segment. Expect each work's bytes after the restart as acknowledged before it,
/// and A then B submitted and accepted at 3 and 4 with exactly those bytes. Before: a work was rebuilt as a fresh clone
/// of the green's head, so A and B read back the green's bytes and would have been accepted without their edits.
#[cfg(unix)]
#[test]
fn acknowledged_work_edits_survive_a_daemon_restart() {
  let profile = common::machine_profile();
  let instance = format!("merge-load-restart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let anchor = common::anchor::held_anchor("merge-load-restart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = common::client::connect(&instance);
  let green = green_with_file(&mut client);
  let (a, b) = edits_before_a_restart(&mut client, green);
  let before = (
    client.read(a, "f", ReadAt::Head).unwrap(),
    client.read(b, "f", ReadAt::Head).unwrap(),
  );
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  let after = (
    client.read(a, "f", ReadAt::Head).unwrap(),
    client.read(b, "f", ReadAt::Head).unwrap(),
  );
  assert_eq!(
    after.0, before.0,
    "work A's bytes after the restart are what it acknowledged before it"
  );
  assert_eq!(after.1, before.1, "and work B's, rebased, too");
  assert_eq!(
    client.submit(a).unwrap(),
    Submitted::Accepted(3),
    "A's base survived: no conflict with its own version"
  );
  assert_eq!(
    client.submit(b).unwrap(),
    Submitted::Accepted(4),
    "B's rebase survived: disjoint from A's"
  );
  let merged = client
    .read(green, "f", ReadAt::Version { version: 4 })
    .unwrap();
  for (at, fill) in [(0, b'a'), (SLOT_AT, b'A'), (SLOT_AT + 4 * SLOT_LEN, b'b')] {
    let start = usize::try_from(at).unwrap();
    let end = start + usize::try_from(SLOT_LEN).unwrap();
    assert_eq!(
      &merged[start..end],
      &slot(fill)[..],
      "version 4 holds the slot at {at}"
    );
  }
  drop(client);
  second.stop();
  drop(anchor);
}

/// The acknowledged state `acknowledged_work_edits_survive_a_daemon_restart` restarts over: work A accepted at 2 and
/// then edited again; work B conflicted, resolved by rebasing onto 2, and edited again. Returns `(A, B)`. Built where
/// its one caller is.
#[cfg(unix)]
fn edits_before_a_restart(client: &mut Client, green: VolumeId) -> (VolumeId, VolumeId) {
  let (a, _) = client.create_work(green, "a").unwrap();
  let (b, _) = client.create_work(green, "b").unwrap();
  client.edit(a, "f", SLOT_AT, SLOT_LEN, &slot(b'A')).unwrap();
  client.edit(b, "f", SLOT_AT, SLOT_LEN, &slot(b'B')).unwrap();
  assert_eq!(client.submit(a).unwrap(), Submitted::Accepted(2));
  client.edit(a, "f", 0, SLOT_LEN, &slot(b'a')).unwrap();
  assert!(matches!(client.submit(b).unwrap(), Submitted::Conflict(_)));
  let theirs = client
    .read(green, "f", ReadAt::Version { version: 2 })
    .unwrap();
  let start = usize::try_from(SLOT_AT).unwrap();
  let window = &theirs[start..start + usize::try_from(SLOT_LEN).unwrap()];
  client.edit(b, "f", SLOT_AT, SLOT_LEN, window).unwrap();
  assert_eq!(client.rebase(b).unwrap(), Rebased::Rebased(2));
  client
    .edit(b, "f", SLOT_AT + 4 * SLOT_LEN, SLOT_LEN, &slot(b'b'))
    .unwrap();
  (a, b)
}
