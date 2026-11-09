//! A daemon keeps its beat while it recovers (§2.6 boot step 2, §4.8 "Recovery", §4.14 `daemon.alive`, A-228): the test
//! plays the anchor, holds one segment across restarts of an in-process daemon, grows real merge state over this crate's
//! own source tree until a recovery needs more CPU than it could spend in one poll and still beat, and watches the anchor's
//! heartbeat word through that recovery. It drives the client verbs and no mount, so it runs on every platform, Windows
//! included; the rest of the recovery oracle, in `recovery.rs`, drives the NFS loopback and rustix syscalls and is Unix-only.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, SizeClass, SnapshotId, VolumeId};
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::anchor::{Anchor, held_anchor, source_of};
use common::client::{connect, scratch};

/// Shape: shards per test daemon: two, so the control shard and the shards it waits on are distinct.
const TEST_SHARDS: u16 = 2;

/// Derived: the least recovery CPU a daemon whose recovery runs as one poll cannot beat through. The watchdog judges a shard
/// spinning once it has run one liveness window of CPU without completing a step, and withholds the beat until the step
/// ends, so such a recovery leaves a beat gap of its CPU less one window. That gap passes the bound the test holds the beat
/// to (one window plus one beat) once the recovery runs past two windows and a beat; one beat more covers the watchdog
/// looking once a beat.
const UNBEATABLE_RECOVERY_NS: u64 =
  2 * slates_server::daemon::LIVENESS_BUDGET_NS + 2 * slates_server::daemon::HEARTBEAT_NS;

/// A source tree held as greens' origin: an overlay over the host directory `dir`, pinned whole and snapshotted, the base
/// every green of the test is created over (A-217: a green over a snapshot holds its origin as a reference and walks it
/// again at every recovery). Bounded by what one shard's content reserve holds, the most any volume there can. Returns
/// the overlay and its snapshot.
fn held_source_tree(
  client: &mut Client,
  config: &DaemonConfig,
  dir: &str,
) -> (VolumeId, SnapshotId) {
  let reserve = u64::try_from(config.shard_content_layout().arena.1).unwrap();
  let overlay = client
    .create(&CreateSpec {
      size: SizeClass::Dynamic { max: reserve },
      base: Some(dir.to_owned()),
      ..scratch("held-source-tree")
    })
    .unwrap();
  assert!(
    client.pin(overlay, None).unwrap() > 0,
    "the tree's entries pinned"
  );
  let snapshot = client.snapshot(overlay).unwrap();
  (overlay, snapshot)
}

/// Every file of `green` at version 0, by path, with its bytes: what the green was created over.
fn files_at_origin(client: &mut Client, green: VolumeId) -> Vec<(String, Vec<u8>)> {
  let at = slates_client::ReadAt::Version { version: 0 };
  let mut files = Vec::new();
  let mut dirs = vec![String::new()];
  while let Some(dir) = dirs.pop() {
    for entry in client.list_dir(green, &dir, at).unwrap() {
      let path = if dir.is_empty() {
        entry.name.clone()
      } else {
        format!("{dir}/{}", entry.name)
      };
      match entry.kind {
        slates_client::EntryKind::Dir => dirs.push(path),
        slates_client::EntryKind::File => {
          let bytes = client.read(green, &path, at).unwrap();
          files.push((path, bytes));
        }
        _ => {}
      }
    }
  }
  files.sort();
  files
}

/// Grows `greens` over `snapshot` of `base` to `target`, remembering in `acknowledged` every file of the first, what each
/// green's origin must read back after every restart.
fn grow_greens(
  client: &mut Client,
  (base, snapshot): (VolumeId, SnapshotId),
  greens: &mut Vec<VolumeId>,
  target: usize,
  acknowledged: &mut Vec<(String, Vec<u8>)>,
) {
  while greens.len() < target {
    let green = client
      .create_green_over(
        &format!("held-tree-{}", greens.len()),
        false,
        slates_client::GreenBase {
          volume: base,
          snapshot,
        },
      )
      .unwrap();
    if acknowledged.is_empty() {
      *acknowledged = files_at_origin(client, green);
      assert!(!acknowledged.is_empty(), "the tree holds files");
    }
    greens.push(green);
  }
}

/// Format: the file a version accepted on the first green adds.
const ACCEPTED_FILE: &str = "accepted-through-recovery";
/// Format: what that file holds.
const ACCEPTED_BYTES: &[u8] = b"accepted as version 1\n";
/// Format: the file the work left open edits.
const OPEN_WORK_FILE: &str = "open-through-recovery";
/// Format: what the open work's edit writes.
const OPEN_WORK_BYTES: &[u8] = b"an open work's edit\n";

/// A version accepted on `green` from a work that adds a file (so the green's chain is replayed at recovery), and a work
/// left open with an edit of its own (so its log is), returned.
fn accept_a_version_and_leave_a_work_open(client: &mut Client, green: VolumeId) -> VolumeId {
  let (work, _) = client.create_work(green, "accepted-work").unwrap();
  client
    .edit(work, ACCEPTED_FILE, 0, 0, ACCEPTED_BYTES)
    .unwrap();
  assert_eq!(
    client.submit(work).unwrap(),
    slates_client::Submitted::Accepted(1)
  );
  let (open, _) = client.create_work(green, "open-work").unwrap();
  client
    .edit(open, OPEN_WORK_FILE, 0, 0, OPEN_WORK_BYTES)
    .unwrap();
  open
}

/// Everything recovered as acknowledged: each green held at its origin at version 0 with every file of the origin's
/// bytes, and the merging green at its accepted version with that version's file, and the open work's edit.
fn assert_recovered_whole(
  client: &mut Client,
  greens: &[VolumeId],
  (merging, open): (VolumeId, VolumeId),
  acknowledged: &[(String, Vec<u8>)],
) {
  for (index, green) in greens.iter().enumerate() {
    assert_eq!(
      client.versions(*green).unwrap(),
      0,
      "green {index} recovered at its origin"
    );
    assert_eq!(
      files_at_origin(client, *green),
      acknowledged,
      "green {index}'s every file reads back what it was created over"
    );
  }
  assert_eq!(
    client.versions(merging).unwrap(),
    1,
    "the merging green at its accepted version"
  );
  assert_eq!(
    client
      .read(
        merging,
        ACCEPTED_FILE,
        slates_client::ReadAt::Version { version: 1 }
      )
      .unwrap(),
    ACCEPTED_BYTES,
    "the accepted version's file"
  );
  assert_eq!(
    client
      .read(open, OPEN_WORK_FILE, slates_client::ReadAt::Head)
      .unwrap(),
    OPEN_WORK_BYTES,
    "the open work's edit"
  );
}

/// A restarted daemon's recovery, watched: the CPU the slowest shard ran from the start's return until an observation of
/// it was answered (its start's poll ended), and the largest gap between the anchor's beats, from this daemon's first beat
/// (the word holds the last daemon's until then) until a window past every shard's start.
#[allow(clippy::disallowed_methods)] // the test plays the anchor, which watches the heartbeat from its own thread
fn watch_recovery(daemon: &Daemon, anchor: &Anchor) -> (Duration, Duration) {
  let shards: Vec<slates_rt::ShardId> = daemon.shards().to_vec();
  let cpu_of = |shard: slates_rt::ShardId| {
    slates_rt::registry::holder_of(shard.0)
      .and_then(slates_rt::registry::shard_cpu)
      .map_or(0, |reading| reading.spent_ns)
  };
  let started_cpu: Vec<u64> = shards.iter().map(|shard| cpu_of(*shard)).collect();
  let (said, done) = std::sync::mpsc::channel();
  let mut waiters = Vec::new();
  for shard in &shards {
    let observation = daemon
      .observation(
        Some(*shard),
        slates_server::daemon::OBSERVE_BUDGET_NS,
        |_: &mut slates_server::state::ShardState| (),
      )
      .unwrap();
    let said = said.clone();
    waiters.push(std::thread::spawn(move || {
      let _ = observation.wait();
      let _ = said.send(());
    }));
  }
  drop(said);
  let read = || anchor.segment.supervision().unwrap().heartbeat_ns();
  let every = Duration::from_nanos(slates_server::daemon::HEARTBEAT_NS);
  let after = Duration::from_nanos(slates_server::daemon::LIVENESS_BUDGET_NS);
  let watching = Instant::now();
  let mut answered = 0;
  let mut ended: Option<Instant> = None;
  let mut last = read();
  let mut last_seen = watching.elapsed();
  let mut beating = false;
  let mut largest = Duration::ZERO;
  while ended.is_none_or(|at| at.elapsed() < after) {
    std::thread::sleep(every);
    while done.try_recv().is_ok() {
      answered += 1;
    }
    if ended.is_none() && answered == shards.len() {
      ended = Some(Instant::now());
    }
    let now = read();
    if now != last {
      if beating {
        largest = largest.max(Duration::from_nanos(now.saturating_sub(last)));
      }
      beating = true;
      last = now;
      last_seen = watching.elapsed();
    }
  }
  if beating {
    largest = largest.max(watching.elapsed().saturating_sub(last_seen));
  }
  for waiter in waiters {
    let _ = waiter.join();
  }
  let recovered_cpu = shards
    .iter()
    .zip(&started_cpu)
    .map(|(shard, from)| cpu_of(*shard).saturating_sub(*from))
    .max()
    .unwrap_or(0);
  (Duration::from_nanos(recovered_cpu), largest)
}

/// §2.6 boot step 2, §4.14 `daemon.alive`, §4.16 A-207, A-217 (the recovery-slicing plan): a daemon whose recovery needs
/// more CPU than it could spend in one poll and still beat keeps its beat while it recovers, and recovers everything. Do:
/// hold this crate's own source tree as a snapshot and create greens over it (each holds its origin as a reference, walked
/// again from the snapshot at every recovery); on one more, accept a version and leave a work open with an edit; double
/// the greens and restart the daemon over the same segment until one recovery's CPU on its slowest shard reaches
/// [`UNBEATABLE_RECOVERY_NS`], watching the anchor's heartbeat word every beat through it. Expect: the beat never older
/// than one liveness window plus a beat; every green at its origin with every file reading back the bytes it was
/// created over, the merging green at its accepted version with that version's file, and the open work's edit. With recovery as one poll the watchdog finds
/// the shard spinning after a window of CPU and withholds the beat until it ends, and an anchor would kill the daemon,
/// whose successor repeats the same recovery.
#[test]
fn a_daemon_keeps_its_beat_through_a_recovery_longer_than_a_window_and_recovers_everything() {
  let profile = common::machine_profile();
  let instance = format!("srv-long-recovery-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let anchor = held_anchor("long-recovery", &profile, &config);
  let mut daemon = Daemon::start(&profile, config.clone(), source_of(&anchor)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let tree = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
  let (base, snapshot) = held_source_tree(&mut client, &config, tree);
  let window = Duration::from_nanos(slates_server::daemon::LIVENESS_BUDGET_NS);
  let beat = Duration::from_nanos(slates_server::daemon::HEARTBEAT_NS);
  let unbeatable = Duration::from_nanos(UNBEATABLE_RECOVERY_NS);
  let mut greens: Vec<VolumeId> = Vec::new();
  let mut acknowledged = Vec::new();
  grow_greens(
    &mut client,
    (base, snapshot),
    &mut greens,
    1,
    &mut acknowledged,
  );
  // A green of its own takes the accepted version: once a version is accepted and no work holds version 0, recovery may
  // fold version 0 away (the retention rule), and the others are read back at their origin.
  let merging = client
    .create_green_over(
      "held-tree-merging",
      false,
      slates_client::GreenBase {
        volume: base,
        snapshot,
      },
    )
    .unwrap();
  let open = accept_a_version_and_leave_a_work_open(&mut client, merging);
  let gap = loop {
    daemon.stop();
    daemon = Daemon::start(&profile, config.clone(), source_of(&anchor)).unwrap();
    let (cpu, gap) = watch_recovery(&daemon, &anchor);
    eprintln!(
      "long recovery: {} greens over {} files recovered in {cpu:?} of CPU, the beat's largest gap {gap:?}",
      greens.len(),
      acknowledged.len()
    );
    if cpu >= unbeatable {
      break gap;
    }
    // A restarted daemon's overlay holds its base listings no longer, so a snapshot taken before the restart no longer
    // counts as a complete base for a new green (refused `ConsistentBaseUnavailable`; reported as its own gap): each
    // round's greens are made over a fresh snapshot of the re-pinned tree, the same files.
    assert!(
      client.pin(base, None).unwrap() > 0,
      "the tree's entries pinned again"
    );
    let snapshot = client.snapshot(base).unwrap();
    let target = greens.len() * 2;
    grow_greens(
      &mut client,
      (base, snapshot),
      &mut greens,
      target,
      &mut acknowledged,
    );
  };
  assert_recovered_whole(&mut client, &greens, (merging, open), &acknowledged);
  drop(client);
  daemon.stop();
  drop(anchor);
  assert!(
    gap <= window + beat,
    "the beat went {gap:?} without advancing while the daemon recovered {} greens, past the {window:?} window plus a \
     {beat:?} beat: the watchdog withheld it",
    greens.len()
  );
}
