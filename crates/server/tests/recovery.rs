//! The recovery oracle (§2.6 boot step 2, §4.8 "Recovery" and the A-9 invariants, D-18; AC-2.12 /
//! T-2.14; GAP-A9-6, BUG-11): a daemon restart preserves acknowledged **content** and its atomic
//! completion, not only catalog identity.
//!
//! The test plays the anchor (the client restart test's pattern): it holds one anchor segment and its
//! content object across two in-process daemons. Content is written through the daemon's own NFS
//! transport — the hand-rolled ONC RPC client of `common::nfs`, so the bytes travel the same path a
//! kernel `mount_nfs` sends them down (client → NFS → the shard's real volume) with no mount and no
//! privilege — and the snapshot, clone, status and retry go through the typed client verbs. After the
//! restart, every byte the first daemon acknowledged must read back identically through the same
//! verbs, or the daemon must refuse explicitly; an empty or older file is the acknowledged data loss
//! the design forbids ("rebuilding a scratch volume from only a quota and id loses acknowledged
//! data", §4.8).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_anchor::AnchorSegment;
use slates_client::{Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::nfs::{create, lookup, mount, read, write};

/// Shape: the probe budget of the quick profile these tests measure (milliseconds); an input to
/// derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon: two, so the volume can live on a shard other than the control
/// shard that accepts the NFS connection, and the cross-shard bridge route is on the recovery path.
const TEST_SHARDS: u16 = 2;
/// Shape: the reply deadline of the test client (nanoseconds): a fifth of a second, far past any
/// served verb and short enough that a stopped daemon is found quickly.
const REPLY_NS: u64 = 200_000_000;
/// Shape: the reconnect budget of the test client (nanoseconds): five seconds, the restarted
/// daemon's start comfortably inside it.
const RECONNECT_NS: u64 = 5_000_000_000;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the content object's slots per shard — the recovery image is a double buffer (the committed
/// image and the one being published), so a torn publish preserves the committed one (§4.8).
const PUBLISH_SLOTS: usize = 2;
/// Shape: the bytes written before the snapshot — what the snapshot freezes.
const BEFORE: &[u8] = b"the bytes the snapshot froze: alpha bravo charlie delta echo foxtrot\n";
/// Shape: the bytes written over the same file after the snapshot — the diverged head. Longer than
/// [`BEFORE`] and written at offset zero, so the head's file is exactly these bytes.
const AFTER: &[u8] =
  b"the bytes the head holds after the snapshot: golf hotel india juliet kilo lima mike november\n";

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

fn deadlines() -> Deadlines {
  Deadlines {
    reply_ns: REPLY_NS,
    reconnect_ns: RECONNECT_NS,
  }
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

fn scratch(name: &str) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// The anchor's segment and content object for a test: the content object is [`PUBLISH_SLOTS`]
/// reserve-sized slots per shard times the partitions (lazily backed, so the unused tail costs no RAM).
fn anchor_segment(name: &str, profile: &MachineProfile, config: &DaemonConfig) -> AnchorSegment {
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
    * PUBLISH_SLOTS
    * usize::from(config.geometry.partitions.max(1));
  AnchorSegment::create(
    &format!("slates-seg-{name}"),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(&format!("slates-con-{name}"), content_bytes)
  .unwrap()
}

/// The handoff a daemon attaches by: the segment and its content object.
fn source_of(segment: &AnchorSegment) -> SegmentSource {
  let (handoff, len) = segment.handoff().unwrap();
  let content = segment.content_handoff().unwrap();
  SegmentSource::Handoff {
    handoff,
    len,
    content,
  }
}

/// An NFS connection to the daemon's loopback port.
fn nfs_stream(daemon: &Daemon) -> TcpStream {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  TcpStream::connect(("127.0.0.1", port)).unwrap()
}

/// The bytes of `/<volume>/<file>` read through a fresh NFS connection to `daemon`.
fn read_file(daemon: &Daemon, volume: &str, file: &str) -> Vec<u8> {
  let mut stream = nfs_stream(daemon);
  let root = mount(&mut stream, &format!("/{volume}"), 1);
  let file = lookup(&mut stream, &root, file, 2);
  read(&mut stream, &file, 3)
}

/// A clone name that routes, by name, to the partition its origin `origin_name` lives on. A clone
/// is created on its origin's partition (it shares the origin's copy-on-write tree, so it must live
/// in the same store), but the NFS host root resolves a name through `owner_of_name` — a hash — so
/// a clone whose name hashes to another partition is unreachable by name over the mount transport
/// (a pre-existing sibling outside the recovery path, reported, not this oracle's subject). The
/// name is chosen here so the oracle reads the snapshot's content through the route that exists.
fn clone_name_on_origin_partition(origin_name: &str, stem: &str) -> String {
  let partitions = usize::from(TEST_SHARDS);
  let origin = slates_server::verbs::owner_of_name(origin_name, partitions);
  (0..partitions)
    .map(|suffix| format!("{stem}-{suffix}"))
    .find(|candidate| slates_server::verbs::owner_of_name(candidate, partitions) == origin)
    .unwrap_or_else(|| stem.to_owned())
}

/// AC-2.12 / T-2.14 (GAP-A9-6): write bytes, snapshot, diverge the head, restart the daemon over the
/// same anchor-owned RAM; expect the head's acknowledged bytes and the snapshot's frozen bytes to read
/// back byte-identical through the same verbs, the snapshot still held and the head still pointing at
/// it. Non-vacuous: the head is written *after* the last control-plane publish (the snapshot), so a
/// recovery that only republishes on control verbs — or that rebuilds scratch content empty (BUG-11) —
/// reads back the frozen bytes or nothing, and fails.
#[test]
fn acknowledged_content_and_its_snapshot_survive_a_daemon_restart_byte_for_byte() {
  let profile = profile();
  let instance = format!("srv-recover-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let segment = anchor_segment("recover", &profile, &config);

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  {
    let mut stream = nfs_stream(&first);
    let root = mount(&mut stream, "/kept", 1);
    let file = create(&mut stream, &root, "f", 2);
    write(&mut stream, &file, BEFORE, 3);
    let snapshot = client.snapshot(kept).unwrap();
    assert_eq!(client.status(kept).unwrap().head.value, snapshot.value);
    // Diverge the head after the snapshot: acknowledged FILE_SYNC by the first daemon.
    write(&mut stream, &file, AFTER, 4);
    assert_eq!(
      read(&mut stream, &file, 5),
      AFTER,
      "the head diverged before the restart"
    );
  }
  let snapshot = client.status(kept).unwrap().head;
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let report = client.status(kept).unwrap();
  assert_eq!(
    client.reconnects(),
    1,
    "the session reconnected under its id"
  );
  assert_eq!(
    report.snapshots, 1,
    "the snapshot was recovered, not reconciled away"
  );
  assert_eq!(
    report.head.value, snapshot.value,
    "the head still points at the recovered snapshot"
  );
  assert_eq!(
    read_file(&second, "kept", "f"),
    AFTER,
    "the head's acknowledged bytes, written after the snapshot, survived the restart"
  );
  // The snapshot's frozen bytes, read through a clone of it — the only path that serves a snapshot's
  // content over the mount transport.
  let clone_name = clone_name_on_origin_partition("kept", "kept-at-snapshot");
  client.clone_snapshot(kept, snapshot, &clone_name).unwrap();
  assert_eq!(
    read_file(&second, &clone_name, "f"),
    BEFORE,
    "the snapshot's frozen bytes survived the restart"
  );
  second.stop();
  drop(segment);
}
