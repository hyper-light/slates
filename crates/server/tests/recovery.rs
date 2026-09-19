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
// These integration tests drive the daemon's NFS-loopback transport, the fleet's TCP
// transport and rustix syscalls — all macOS/Linux; on Windows the daemon mounts through WinFsp and
// the fleet transport is QUIC-over-UDP, so these particular tests are unix (as `virtiofs.rs` is).
#![cfg(unix)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_anchor::{AnchorSegment, RegionKind};
use slates_client::{Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass, VolumeId};
use slates_ipc::protocol::{ReplyBody, RequestBody};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;
use common::nfs::{create, fsstat, lookup, mount, read, write};

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

/// AC-2.3 (§4.8 transactions; AUD-06): a verb whose record cannot be made durable is refused **typed**
/// (`Unpublished`), its effects and its completion record are rolled back together, a retry under the
/// same id **re-executes** (it is never answered a success from memory — there is none), and a restart
/// over the same segment agrees with the live daemon: the volume the retry created is there, once, and
/// the retried id still meets its completion record. Non-vacuous: the shard's rollback counter moved,
/// and the refusal is the typed one. The failure is injected at the database's publication (the
/// segment refusing the record) so the whole recorded-verb path above it — dispatch, the completion
/// record, the commit, the reply — runs exactly as it would on a segment that cannot publish.
#[test]
fn an_unpublished_verb_is_refused_typed_a_retry_re_executes_and_a_restart_agrees() {
  let profile = profile();
  let instance = format!("srv-unpublished-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let segment = anchor_segment("unpublished", &profile, &config);

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let durable = client.create(&scratch("durable")).unwrap();

  let create_id = refused_unpublished_create(&first, &mut client);

  // The retry under the same id re-executes — the volume is created now, durably — rather than
  // reading a success that was never published.
  let retried = retry_create(&mut client, create_id, "unpublished");
  let ReplyBody::Created { id: created } = retried else {
    panic!("the retry re-executes the create: {retried:?}");
  };
  assert_ne!(created, durable, "a distinct volume");
  assert_eq!(
    names_listed(&mut client),
    vec!["durable".to_owned(), "unpublished".to_owned()]
  );

  // The restart over the same segment agrees: the effect and its completion survived together.
  let member = first.member_identity().unwrap();
  first.stop();
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  assert_eq!(second.member_identity(), Ok(member), "a warm restart");
  assert_eq!(
    names_listed(&mut client),
    vec!["durable".to_owned(), "unpublished".to_owned()],
    "the retried create is durable: present once after the restart"
  );
  assert_eq!(
    retry_create(&mut client, create_id, "unpublished"),
    ReplyBody::Created { id: created },
    "after the restart the id meets the completion record the retry published — the same volume, no \
     second create"
  );
  assert_eq!(
    rollbacks_of(&second),
    0,
    "the restarted daemon rolled nothing back"
  );
  second.stop();
}

/// The refused phase: the next publication on whichever shard runs the create is refused before its
/// record is appended, so the create of "unpublished" comes back typed `Unpublished`, the shard
/// counts one rollback, and nothing of the verb is listed. Returns the refused request's id, for the
/// retry. Clears the faults still pending on the other shards.
fn refused_unpublished_create(daemon: &Daemon, client: &mut Client) -> RequestId {
  daemon
    .inject_publication_fault(Some(slates_db::replay::PublicationFault::BeforeAppend))
    .expect("the fault is installed");
  let refused = client.create(&scratch("unpublished"));
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(
        slates_ipc::protocol::Refusal::Unpublished { .. }
      ))
    ),
    "the verb is refused typed as unpublished, not served: {refused:?}"
  );
  let create_id = client.last_request();
  assert_eq!(
    rollbacks_of(daemon),
    1,
    "the shard rolled the transaction back (counted)"
  );
  daemon
    .inject_publication_fault(None)
    .expect("the faults still pending on the other shards are cleared");
  assert_eq!(
    names_listed(client),
    vec!["durable".to_owned()],
    "nothing of the refused verb is visible: the rolled-back volume is not listed"
  );
  create_id
}

/// The volume names the daemon lists, sorted.
fn names_listed(client: &mut Client) -> Vec<String> {
  let mut names: Vec<String> = client
    .list()
    .unwrap()
    .iter()
    .map(|v| v.name.clone())
    .collect();
  names.sort();
  names
}

/// Retries the scratch create of `name` under `id` — the daemon answers from its completion record
/// when it served the id, else serves it now.
fn retry_create(client: &mut Client, id: RequestId, name: &str) -> ReplyBody {
  client
    .retry(
      id,
      &RequestBody::Create {
        name: name.to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    )
    .unwrap()
}

/// Transactions rolled back across every shard of `daemon` (`Daemon::db_publication_counters`).
fn rollbacks_of(daemon: &Daemon) -> u64 {
  daemon
    .db_publication_counters()
    .unwrap()
    .iter()
    .map(|c| c.rollbacks)
    .sum()
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

/// The NFS mount path carrying an owner mount capability for the volume named `name` on `daemon`
/// (§4.13; AUD-01): a name alone reaches nothing.
fn capability_path(daemon: &Daemon, name: &str) -> String {
  daemon
    .mount_capability(name)
    .expect("the name's owner shard answers")
    .expect("a volume by that name is served there")
}

/// The bytes of `/<volume>/<file>` read through a fresh NFS connection to `daemon`.
fn read_file(daemon: &Daemon, volume: &str, file: &str) -> Vec<u8> {
  let mut stream = nfs_stream(daemon);
  let root = mount(&mut stream, &capability_path(daemon, volume), 1);
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
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  {
    let mut stream = nfs_stream(&first);
    let root = mount(&mut stream, &capability_path(&first, "kept"), 1);
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
  let member = first.member_identity().unwrap();
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  assert_eq!(
    second.member_identity(),
    Ok(member),
    "retained Raft state preserves the voter; a warm restart needs no bootstrap"
  );
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

// -------------------------------------------- crash injection at every durable step (AC-2.3, AC-2.12)
//
// A scenario of single-transaction steps, so each crash point is a real state a kill can leave. Two
// crash states need no surgery — stop the first daemon *before* the step (nothing durable) or *after*
// it (both the content publish and the log record durable); the same segment and content object carry
// through to the second daemon (the client-restart pattern). The third state — published, but crashed
// before the log record committed — is produced by rolling the log ring's tail back to before the
// step (the content object keeps the step's published image, since a control verb publishes in its
// dispatch before the completion transaction commits). Rolling back the tail is a 64-byte header
// write; the log region itself is many gigabytes (a whole recovery budget of records) and is never
// copied.

/// Format: the width of a log record's body-length field and where it sits — a `u32` at offset 4 of
/// the 32-byte record header (`crates/db/src/record.rs`: magic, then the little-endian body length).
/// The oracle reads it to find one record's end so it can roll the ring back to exactly there.
const RECORD_LEN_AT: usize = 4;

/// Shape: the volume's size class at creation, and after the scenario's resize (bytes).
const CREATED_LIMIT: u64 = 1 << 20;
const RESIZED_LIMIT: u64 = 2 << 20;
/// Shape: the scenario's steps (the numbered arms of [`step`]).
const STEPS: usize = 6;
/// Shape: the steps that are control verbs — a publish then a completion record, so three crash states
/// each; the rest are mount-transport mutations — a publish only, so two.
const CONTROL_STEPS: [usize; 3] = [1, 4, 6];
/// Shape: the crash points the sweep yields — three per control step, two per mount mutation.
const CRASH_POINTS: usize = CONTROL_STEPS.len() * 3 + (STEPS - CONTROL_STEPS.len()) * 2;

/// Where a crash falls inside one step's durable writes, in the daemon's order — the shard image is
/// published, then (for a control verb) the completion record commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
  /// Before the publish: nothing of the step reached anchor-owned RAM (the first daemon stops before
  /// running the step).
  BeforePublish,
  /// After the publish, before the record: the content image is ahead of the log (the log ring's tail
  /// is rolled back over the step's record). A control verb only — a mount mutation has no record.
  BetweenPublishAndRecord,
  /// After both: the step is acknowledged; a retry meets its completion record.
  AfterRecord,
}

/// Reads a ring's tail (its monotonic byte write offset) from the log region's header word.
fn log_tail(segment: &AnchorSegment, partition: u16) -> u64 {
  let ring = segment.region_bytes(RegionKind::Log(partition)).unwrap();
  let mut word = [0u8; size_of::<u64>()];
  word
    .copy_from_slice(&ring[slates_anchor::layout::RING_TAIL..slates_anchor::layout::RING_TAIL + 8]);
  u64::from_le_bytes(word)
}

/// The 64-byte header words (head, tail, capacity, sequence base) of each partition's log ring — what
/// a crash-before-the-record rolls back. Copying the header, never the many-gigabyte data ring.
fn log_headers(segment: &AnchorSegment, partitions: u16) -> Vec<(u16, Vec<u8>)> {
  (0..partitions)
    .map(|p| {
      let ring = segment.region_bytes(RegionKind::Log(p)).unwrap();
      (p, ring[..slates_anchor::layout::RING_BYTES].to_vec())
    })
    .collect()
}

/// Puts each ring's header back, rolling its tail (and head/sequence) to the captured state — so a
/// record appended after the capture is beyond the tail and ignored on replay, exactly as a crash
/// before that record's durable commit would leave it.
fn restore_log_headers(segment: &mut AnchorSegment, headers: &[(u16, Vec<u8>)]) {
  for (p, header) in headers {
    let ring = segment.region_bytes_mut(RegionKind::Log(*p)).unwrap();
    ring[..header.len()].copy_from_slice(header);
  }
}

/// One scenario in flight: the client, what it holds, and the request id of each control step so a
/// resume of an acknowledged step retries under its original id (the exactly-once path).
struct Run {
  client: Client,
  kept: Option<VolumeId>,
  head_file: Option<Vec<u8>>,
  requests: Vec<Option<RequestId>>,
  xid: u32,
}

impl Run {
  fn new(client: Client) -> Run {
    Run {
      client,
      kept: None,
      head_file: None,
      requests: vec![None; STEPS + 1],
      xid: 0,
    }
  }

  fn xid(&mut self) -> u32 {
    self.xid += 1;
    self.xid
  }

  /// Runs a control verb. `retry` replays it under the id it first ran with (an acknowledged step,
  /// whose completion record answers); otherwise it is a fresh call under the next sequence — which is
  /// what a step that never committed (before-publish, or a rolled-back between-state) resumes as.
  fn control(&mut self, k: usize, retry: bool, body: &RequestBody) -> ReplyBody {
    let reply = if retry {
      let id = self.requests[k].expect("the step ran before");
      self.client.retry(id, body).unwrap()
    } else {
      let reply = self.client.call(body).unwrap();
      self.requests[k] = Some(self.client.last_request());
      reply
    };
    if let ReplyBody::Created { id } = reply {
      self.kept = Some(id);
    }
    reply
  }
}

/// The NFS root handle of `/<volume>` on `daemon`, with its connection.
fn mount_volume(run: &mut Run, daemon: &Daemon, volume: &str) -> (TcpStream, Vec<u8>) {
  let mut stream = nfs_stream(daemon);
  let xid = run.xid();
  let root = mount(&mut stream, &capability_path(daemon, volume), xid);
  (stream, root)
}

/// The scenario's step `k` on `daemon`: a control verb through the client (replayed under its original
/// id when `retry`), or a mutation over the mount transport (re-issued as is — a CREATE is UNCHECKED
/// and a WRITE at an offset is idempotent, which is how an NFS client resumes after a server restart).
fn step(run: &mut Run, daemon: &Daemon, k: usize, retry: bool) {
  match k {
    1 => {
      let body = RequestBody::Create {
        name: "kept".to_owned(),
        size: SizeClass::Bounded {
          limit: CREATED_LIMIT,
        },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      };
      match run.control(k, retry, &body) {
        ReplyBody::Created { .. } => {}
        other => panic!("create: {other:?}"),
      }
    }
    2 => {
      let (mut stream, root) = mount_volume(run, daemon, "kept");
      let xid = run.xid();
      run.head_file = Some(create(&mut stream, &root, "f", xid));
    }
    3 | 5 => {
      let bytes = if k == 3 { BEFORE } else { AFTER };
      let mut stream = nfs_stream(daemon);
      let file = run.head_file.clone().expect("f was created");
      let xid = run.xid();
      write(&mut stream, &file, bytes, xid);
    }
    4 => {
      let body = RequestBody::Snapshot {
        volume: run.kept.expect("kept exists"),
      };
      match run.control(k, retry, &body) {
        ReplyBody::Snapshotted { .. } => {}
        other => panic!("snapshot: {other:?}"),
      }
    }
    6 => {
      let body = RequestBody::Resize {
        volume: run.kept.expect("kept exists"),
        size: SizeClass::Bounded {
          limit: RESIZED_LIMIT,
        },
      };
      assert_eq!(run.control(k, retry, &body), ReplyBody::Resized);
    }
    _ => panic!("no step {k}"),
  }
}

/// The observable end state, compared whole against the reference: the kept volume's snapshot count,
/// its file's bytes, and its acknowledged capacity (the quota the mount reports).
#[derive(Debug, PartialEq, Eq)]
struct Observed {
  snapshots: u32,
  file: Vec<u8>,
  capacity: u64,
}

fn observe(run: &mut Run, daemon: &Daemon) -> Observed {
  let kept = run.kept.expect("kept exists");
  let snapshots = run.client.status(kept).unwrap().snapshots;
  let (mut stream, root) = mount_volume(run, daemon, "kept");
  let xid = run.xid();
  let (capacity, _) = fsstat(&mut stream, &root, xid);
  Observed {
    snapshots,
    file: read_file(daemon, "kept", "f"),
    capacity,
  }
}

/// What the recovered daemon must show for the crash state itself, before the resume: the catalog's
/// acknowledged state and nothing of an unacknowledged effect. The snapshot check uses D-13's exact
/// counters — an image-only snapshot the trim removed would leave a version sharing the head's bytes
/// (unique < referenced); its absence proves the trim. The capacity check proves the acknowledged
/// quota was restored over the image's.
fn assert_crash_state(run: &mut Run, daemon: &Daemon, k: usize, done: bool) {
  match k {
    1 => {
      let has_kept = run.client.list().unwrap().iter().any(|v| v.name == "kept");
      assert_eq!(
        has_kept, done,
        "crash at step 1: kept exists iff acknowledged"
      );
    }
    4 => {
      let report = run.client.status(run.kept.unwrap()).unwrap();
      if done {
        assert_eq!(report.snapshots, 1, "crash at step 4 after record");
      } else {
        assert_eq!(
          report.snapshots, 0,
          "crash at step 4: no unrecorded snapshot"
        );
        assert!(report.referenced_bytes > 0, "the head has content");
        assert_eq!(
          report.unique_bytes, report.referenced_bytes,
          "crash at step 4: no hidden snapshot shares the head's bytes (image-only snapshot trimmed)"
        );
      }
    }
    6 => {
      let (mut stream, root) = mount_volume(run, daemon, "kept");
      let xid = run.xid();
      let (capacity, _) = fsstat(&mut stream, &root, xid);
      assert_eq!(
        capacity,
        if done { RESIZED_LIMIT } else { CREATED_LIMIT },
        "crash at step 6: the acknowledged size policy, not the image's"
      );
    }
    _ => {}
  }
}

/// The whole scenario once on a fresh daemon with no crash — the reference every resume must reach.
/// Built from its own clean run, never from a crashed state.
fn reference(profile: &MachineProfile) -> Observed {
  let instance = format!("srv-crash-ref-{}", std::process::id());
  let config = DaemonConfig::derive(profile, &instance).with_shards(TEST_SHARDS);
  let segment = anchor_segment("crash-ref", profile, &config);
  let daemon = Daemon::start(profile, config, source_of(&segment)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut run = Run::new(connect(&instance));
  for k in 1..=STEPS {
    step(&mut run, &daemon, k, false);
  }
  let observed = observe(&mut run, &daemon);
  daemon.stop();
  drop(segment);
  observed
}

/// AC-2.3 / AC-2.12 (T-2.14): crash injection at every durable step of the recovery path with a resume
/// that must reach the reference. Each step writes into anchor-owned RAM in the daemon's order — the
/// shard image published, then (a control verb) the completion record committed. The test, as the
/// anchor, starts a second daemon over each crash state a kill at that step can leave: before the
/// publish (stop before the step), between the publish and the record (roll the log's tail back over
/// the step's record, the content image kept), and after both (stop after the step). A mount mutation
/// has no record, so two states. The recovered daemon must show the catalog's acknowledged state and
/// nothing of an unacknowledged effect; then the client resumes — an acknowledged step replayed under
/// its original id, an unacknowledged one re-issued — and finishes the scenario, whose end state must
/// equal a separate clean run's. A file handle minted before a crash must still resolve after it.
/// Non-vacuous: without the image-only snapshot trim, the between-state at step 4 shows a hidden
/// snapshot (unique ≠ referenced bytes); without the acknowledged-quota revert, the between-state at
/// step 6 shows the image's larger capacity.
#[test]
fn a_crash_at_every_durable_step_recovers_and_the_resume_reaches_the_reference() {
  let profile = profile();
  let reference = reference(&profile);
  let mut points = 0;
  for k in 1..=STEPS {
    let crashes: &[Crash] = if CONTROL_STEPS.contains(&k) {
      &[
        Crash::BeforePublish,
        Crash::BetweenPublishAndRecord,
        Crash::AfterRecord,
      ]
    } else {
      &[Crash::BeforePublish, Crash::AfterRecord]
    };
    for &crash in crashes {
      points += 1;
      let started = Instant::now();
      run_crash_point(&profile, &reference, k, crash);
      eprintln!(
        "recovery oracle: crash point {points}/{CRASH_POINTS} (step {k}, {crash:?}) in {} ms",
        started.elapsed().as_millis()
      );
    }
  }
  assert_eq!(points, CRASH_POINTS, "every crash point ran");
}

/// One crash point: run the scenario to step `k` on a first daemon, leave the crash state a kill at
/// that step would (before the publish, between the publish and the record, or after both), start a
/// second daemon over it, assert the recovered state, resume, and require the end state to equal the
/// clean `reference`.
fn run_crash_point(profile: &MachineProfile, reference: &Observed, k: usize, crash: Crash) {
  let name = format!("crash-{k}-{crash:?}");
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(profile, &instance).with_shards(TEST_SHARDS);
  let mut segment = anchor_segment(&name, profile, &config);
  let partitions = config.geometry.partitions;

  let first = Daemon::start(profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut run = Run::new(connect(&instance));
  for j in 1..k {
    step(&mut run, &first, j, false);
  }
  let before = log_headers(&segment, partitions);
  // Before-publish: the step never runs on the first daemon; the others run it, then the
  // between-state rolls the log back so only the content publish survives.
  if crash != Crash::BeforePublish {
    step(&mut run, &first, k, false);
  }
  first.stop();
  if crash == Crash::BetweenPublishAndRecord {
    restore_log_headers(&mut segment, &before);
  }

  let second = Daemon::start(profile, config, source_of(&segment)).unwrap();
  let done = crash == Crash::AfterRecord;
  assert_crash_state(&mut run, &second, k, done);
  step(&mut run, &second, k, done);
  for j in k + 1..=STEPS {
    step(&mut run, &second, j, false);
  }
  assert_eq!(
    observe(&mut run, &second),
    *reference,
    "crash {crash:?} at step {k}: the resume reaches the reference"
  );
  let mut stream = nfs_stream(&second);
  let xid = run.xid();
  assert_eq!(
    read(&mut stream, run.head_file.as_ref().unwrap(), xid),
    AFTER,
    "crash {crash:?} at step {k}: a handle minted before the crash resolves after it"
  );
  drop(stream);
  second.stop();
  drop(segment);
}

// ------------------------------------------------ clone pins and destroy completion across a restart

/// The partition that owns `volume` (`slates_server::verbs::owner_of` over its id's bytes).
fn owner_partition(volume: VolumeId) -> u16 {
  slates_server::verbs::owner_of(slates_ipc::protocol::VolumeId {
    bytes: volume.bytes,
  })
}

/// AC-2.12 (§4.8 recovery authority): a clone's pin on its origin snapshot, and a destroy still in
/// flight, are reconciled to the catalog across a restart. A clone is created (pinning the snapshot),
/// then the daemon is restarted; the recovered snapshot must still be pinned — its destroy refused —
/// because `reconcile_clone_pins` restored the pin the image carried against the recorded clone. Then
/// the clone's destroy verb is committed and the daemon is restarted *before its background
/// reclamation ran* — the state a crash leaves, reproduced by rolling the log's tail to just past the
/// destroy verb's record (excluding the later `VolumeDestroyed`), so the catalog says `Destroying`;
/// `complete_recovered_destroys` finishes it at boot, unpinning the origin, and the snapshot's destroy
/// then succeeds. Non-vacuous: without the pin reconciliation the first destroy-snapshot would succeed
/// (the image's pin lost); without the destroy completion the clone would stay `Destroying` forever
/// and the snapshot pinned.
#[test]
fn a_clone_pin_and_a_destroy_in_flight_reconcile_to_the_catalog_across_a_restart() {
  let profile = profile();
  let instance = format!("srv-pins-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let mut segment = anchor_segment("pins", &profile, &config);

  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  let snapshot = client.snapshot(kept).unwrap();
  let clone = client.clone_snapshot(kept, snapshot, "kept-clone").unwrap();
  first.stop();

  // Restart 1: the clone's pin on the snapshot survived in the image and is reconciled to the recorded
  // clone, so the snapshot's destroy is refused (a lost pin would be the only reason it were allowed).
  let second = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  assert!(
    matches!(
      client.destroy_snapshot(kept, snapshot),
      Err(ClientError::Refused(slates_ipc::protocol::Refusal::BadRequest { reason })) if reason == "Pinned"
    ),
    "the recovered snapshot is still pinned by the recovered clone (a lost pin would let it be destroyed): {:?}",
    client.destroy_snapshot(kept, snapshot)
  );

  // Destroy the clone; roll the log back to just past the destroy verb's record so the catalog shows
  // `Destroying` with the background `VolumeDestroyed` never durable — the crash a mid-reclamation kill
  // leaves. The destroy verb commits `Destroying` + the completion in one record; the next record is
  // the reclamation's `VolumeDestroyed`.
  let clone_partition = owner_partition(clone);
  let before_destroy = log_tail(&segment, clone_partition);
  client.destroy(clone).unwrap();
  // Wait for the background reclamation to run (so a `VolumeDestroyed` record exists after the destroy
  // verb's), then cut the log to before it.
  let started = Instant::now();
  loop {
    let listed: Vec<String> = client.list().unwrap().into_iter().map(|v| v.name).collect();
    if listed == ["kept"] {
      break;
    }
    assert!(
      started.elapsed() < Duration::from_secs(5),
      "the clone's destroy reclamation runs: {listed:?}"
    );
  }
  cut_log_after_one_record(&mut segment, clone_partition, before_destroy);
  second.stop();

  // Restart 2 over the `Destroying` state: recovery completes the destroy, unpinning the origin, so
  // the snapshot's destroy now succeeds.
  let third = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let started = Instant::now();
  loop {
    let listed: Vec<String> = client.list().unwrap().into_iter().map(|v| v.name).collect();
    if listed == ["kept"] {
      break;
    }
    assert!(
      started.elapsed() < Duration::from_secs(5),
      "the in-flight clone destroy completes on recovery: {listed:?}"
    );
  }
  client.destroy_snapshot(kept, snapshot).unwrap();
  assert_eq!(
    client.status(kept).unwrap().snapshots,
    0,
    "the snapshot's destroy succeeds once the recovered destroy unpinned it"
  );
  third.stop();
  drop(segment);
}

/// Rolls a partition's log ring back to just past the one record that begins at `from_tail`, dropping
/// every later record. Reads that record's body length from its header (a `u32` at [`RECORD_LEN_AT`]
/// in the 32-byte record header) to find its end — the records are unpadded, tail advancing by exactly
/// header + body — and writes the tail word back. `from_tail` is small in these tests, so the record
/// does not wrap the ring.
fn cut_log_after_one_record(segment: &mut AnchorSegment, partition: u16, from_tail: u64) {
  let capacity = {
    let ring = segment.region_bytes(RegionKind::Log(partition)).unwrap();
    let mut word = [0u8; size_of::<u64>()];
    word.copy_from_slice(
      &ring[slates_anchor::layout::RING_CAPACITY..slates_anchor::layout::RING_CAPACITY + 8],
    );
    u64::from_le_bytes(word)
  };
  let data_at = slates_anchor::layout::RING_BYTES
    + usize::try_from(from_tail % capacity.max(1)).unwrap_or(0)
    + RECORD_LEN_AT;
  let body_len = {
    let ring = segment.region_bytes(RegionKind::Log(partition)).unwrap();
    let mut word = [0u8; size_of::<u32>()];
    word.copy_from_slice(&ring[data_at..data_at + size_of::<u32>()]);
    u64::from(u32::from_le_bytes(word))
  };
  let record_end =
    from_tail + u64::try_from(slates_db::record::RECORD_HEADER).unwrap_or(0) + body_len;
  let ring = segment
    .region_bytes_mut(RegionKind::Log(partition))
    .unwrap();
  ring[slates_anchor::layout::RING_TAIL..slates_anchor::layout::RING_TAIL + 8]
    .copy_from_slice(&record_end.to_le_bytes());
}
