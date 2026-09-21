//! The client's tests (Phase 2 task 5; §4.4, §4.7, §4.9, AC-2.3's idempotency half): the
//! typed verbs over an in-process daemon, and a session that outlives a daemon restart: the
//! same client reconnects under its id and its retry meets the completion record; the
//! volume and its snapshot are recovered from anchor-owned RAM; a second client cannot take a
//! live session.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_anchor::AnchorSegment;
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass, Submitted,
};
use slates_ipc::protocol::{Refusal, RequestBody};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the probe budget of the quick profile these tests measure (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon: two, so a client's shard and the control shard differ.
const TEST_SHARDS: u16 = 2;
/// Shape: the reply deadline of the test client (nanoseconds): a fifth of a second, far past
/// any served verb and short enough that a dead daemon is found quickly.
const REPLY_NS: u64 = 200_000_000;
/// Shape: the reconnect budget of the test client (nanoseconds): five seconds.
const RECONNECT_NS: u64 = 5_000_000_000;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);

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

fn wait_until_listed(client: &mut Client, names: &[&str]) {
  let started = Instant::now();
  loop {
    let mut listed: Vec<String> = client
      .list()
      .unwrap()
      .iter()
      .map(|v| v.name.clone())
      .collect();
    listed.sort();
    if listed == names {
      return;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "listing settles to {names:?}: {listed:?}"
    );
  }
}

/// Create, the duplicate refused, snapshot, clone; the origin's id.
fn create_snapshot_clone(client: &mut Client) -> slates_client::VolumeId {
  let id = client.create(&scratch("one")).unwrap();
  assert_eq!(
    client.create(&scratch("one")),
    Err(ClientError::Refused(Refusal::AlreadyExists {
      existing: id
    }))
  );
  let snapshot = client.snapshot(id).unwrap();
  let clone = client.clone_snapshot(id, snapshot, "one-clone").unwrap();
  assert_ne!(clone, id);
  id
}

/// The register at f=0 (§4.8 task 7): the head is placed on the local append, the host epoch
/// is the first, no mirror exists; `await placed(region)` returns and the mirror is refused.
fn assert_register_at_f0(
  client: &mut Client,
  id: slates_client::VolumeId,
  report: &slates_client::StatusReport,
) {
  assert!(report.placed.region, "the head is placed at f=0");
  assert_eq!(report.placed.host_epoch, 1);
  assert_eq!(report.placed.mirror_age_ns, None);
  assert_eq!(
    client
      .await_placed(id, None, slates_client::Scope::Region)
      .unwrap(),
    (true, None),
    "await placed(region) is the local append"
  );
  assert_eq!(
    client.await_placed(id, None, slates_client::Scope::Mirror),
    Err(slates_client::ClientError::Refused(Refusal::Unsupported {
      feature: "mirror".to_owned()
    })),
    "no mirror on a laptop"
  );
}

/// Attach for writing takes the lease; status shows it; detach releases it.
fn attach_status_detach(client: &mut Client, id: slates_client::VolumeId) {
  let attached = client.attach(id, None, Intent::Write).unwrap();
  assert_eq!(attached.lease_epoch, Some(1));
  let report = client.status(id).unwrap();
  assert_eq!(report.name, "one");
  assert_eq!(report.attachments, 1);
  assert_eq!(report.snapshots, 1);
  assert_register_at_f0(client, id, &report);
  wait_until_listed(client, &["one", "one-clone"]);
  client.detach(attached.attachment).unwrap();
  assert_eq!(client.status(id).unwrap().lease_epoch, None);
}

/// The typed verbs over the rings: create, the duplicate refused, snapshot, clone, attach
/// with the lease, status, list, detach, resize, destroy, acknowledge.
#[test]
fn the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed() {
  let profile = profile();
  let instance = format!("cl-life-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: "slates-seg-cl-life".to_owned(),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let id = create_snapshot_clone(&mut client);
  attach_status_detach(&mut client, id);
  client
    .resize(id, SizeClass::Bounded { limit: 2 << 20 })
    .unwrap();
  client.destroy(id).unwrap();
  wait_until_listed(&mut client, &["one-clone"]);
  client.acknowledge_all().unwrap();
  let report = client.daemon_status().unwrap();
  assert_eq!(report.pid, std::process::id(), "the daemon is this process");
  assert_eq!(report.shards.len(), usize::from(TEST_SHARDS));
  assert_eq!(
    report.shards.iter().map(|s| s.clients).sum::<u32>(),
    1,
    "one live client"
  );
  assert!(
    report.shards.iter().map(|s| s.served).sum::<u64>() >= 10,
    "the calls were served"
  );
  assert!(
    report
      .shards
      .iter()
      .flat_map(|s| s.refusals.iter())
      .any(|r| r.kind == "already_exists" && r.count == 1),
    "the duplicate was counted: {:?}",
    report.shards
  );
  assert_eq!(client.reconnects(), 0, "nothing made the client reconnect");
  let (parks, replies) = client.park_ratio();
  assert!(replies >= 10, "every call was a reply: {replies}");
  assert!(
    parks <= replies,
    "parks never exceed replies: {parks}/{replies}"
  );
  daemon.stop();
}

/// After the restart: the client's next call reconnects under its id and is served by the
/// restarted daemon, which rebuilt the volume and recovered its snapshot from anchor-owned RAM.
fn assert_served_after_restart(
  client: &mut Client,
  kept: slates_client::VolumeId,
  session: slates_client::Session,
  snapshot: slates_client::SnapshotId,
) {
  let report = client.status(kept).unwrap();
  assert_eq!(report.name, "kept");
  assert_eq!(client.reconnects(), 1, "one reconnect, under the old id");
  assert_eq!(client.client_id(), session.client_id);
  assert_eq!(
    report.snapshots, 1,
    "the snapshot's content was recovered from anchor-owned RAM (§4.8), not reconciled away"
  );
  assert_eq!(
    report.head.value, snapshot.value,
    "and the head still points at the recovered snapshot"
  );
}

/// Exactly-once across the restart: the retry answers from the replayed completion record.
fn assert_retry_meets_record(
  client: &mut Client,
  kept: slates_client::VolumeId,
  create_id: slates_wire::request::RequestId,
) {
  let retried = client
    .retry(
      create_id,
      &RequestBody::Create {
        name: "kept".to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    )
    .unwrap();
  assert_eq!(
    retried,
    slates_ipc::protocol::ReplyBody::Created { id: kept },
    "the original reply, not a second create"
  );
  wait_until_listed(client, &["before-restart", "kept"]);
}

/// A session outlives a daemon restart over the same segment (the test plays the anchor):
/// the client's next call finds the daemon gone, reconnects under its id and is served by
/// the restarted daemon, which recovered the volume and its snapshot from anchor-owned RAM; its
/// retry of the create it made before meets the completion record (the same id, no second
/// volume); a second client cannot take the live session.
#[test]
fn a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record() {
  let profile = profile();
  let instance = format!("cl-resume-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  // The test plays the anchor: it holds the segment and its content object across both daemons, so
  // anchor-owned volume storage survives the restart (§4.8). The content object is two reserve-sized
  // slots per shard (the recovery image is a double buffer — the committed image and the one being
  // published — so a torn publish preserves the committed one) times the partitions; it is lazily
  // backed, so its unused tail costs no RAM.
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
    * 2
    * usize::from(config.geometry.partitions.max(1));
  let segment = AnchorSegment::create(
    "slates-seg-cl-resume",
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content("slates-con-cl-resume", content_bytes)
  .unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  let create_id = client.last_request();
  let snapshot = client.snapshot(kept).unwrap();
  assert_eq!(client.status(kept).unwrap().snapshots, 1);
  let session = client.session();
  let mut exited = connect(&instance);
  let before_restart = exited.create(&scratch("before-restart")).unwrap();
  let silent = connect(&instance);
  let highest_issued = silent.client_id();
  drop(exited);
  drop(silent);
  first.stop();
  let second = Daemon::start(&profile, config, source()).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut fresh = connect(&instance);
  assert_eq!(
    fresh.status(kept).unwrap().snapshots,
    1,
    "the new status must not replay an old client's create completion"
  );
  assert_eq!(fresh.status(before_restart).unwrap().snapshots, 0);
  assert!(
    fresh.client_id() > highest_issued,
    "even a client that ran no verb reserved its id before the restart"
  );
  assert_served_after_restart(&mut client, kept, session, snapshot);
  assert_retry_meets_record(&mut client, kept, create_id);
  // New work continues under the session's sequence.
  let more = client.create(&scratch("more")).unwrap();
  assert_ne!(more, kept);
  // A second client cannot take a live session.
  assert!(matches!(
    Client::resume(&instance, session, deadlines()),
    Err(ClientError::SessionTaken { .. })
  ));
  second.stop();
  drop(segment);
}

/// AC-2.3 / AC-2.6: use the last fresh identity, refuse the next admission, restart, and
/// expect the existing session to keep working while fresh admission remains refused.
#[test]
fn exhausted_client_id_space_refuses_fresh_callers_but_preserves_a_resuming_session() {
  let profile = profile();
  let instance = format!("cl-id-end-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(1);
  // The anchor keeps both content publication slots as well as metadata across the restart.
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap() * 2;
  let mut segment = AnchorSegment::create(
    "slates-seg-cl-id-end",
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content("slates-con-cl-id-end", content_bytes)
  .unwrap();
  // Build the retained admission history at the format boundary without billions of connects.
  let (mut db, _) = slates_db::replay::recover(&mut segment, 0, config.caps, 0).unwrap();
  db.mutate(
    &mut segment,
    &slates_db::Op::ClientIdReserved {
      client: u32::MAX - 1,
    },
    0,
  )
  .unwrap();
  db.snapshot(&mut segment).unwrap();
  drop(db);
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content: segment.content_handoff().unwrap(),
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  assert_eq!(client.client_id(), u32::MAX);
  let kept = client.create(&scratch("last-identity")).unwrap();
  assert_fresh_identity_refused(&instance);
  assert_eq!(client.status(kept).unwrap().name, "last-identity");
  first.stop();
  let second = Daemon::start(&profile, config, source()).unwrap();
  second.bootstrap(true).unwrap();
  assert_eq!(client.status(kept).unwrap().name, "last-identity");
  assert_eq!(client.client_id(), u32::MAX);
  assert!(
    client.reconnects() > 0,
    "the existing session really resumed"
  );
  assert_fresh_identity_refused(&instance);
  assert_eq!(client.status(kept).unwrap().name, "last-identity");
  second.stop();
}

fn assert_fresh_identity_refused(instance: &str) {
  assert!(matches!(
    Client::connect(instance, deadlines()),
    Err(ClientError::Ipc(
      slates_ipc::IpcError::DaemonUnavailable { .. }
    ))
  ));
}

/// Commits two versions on a green: create `f`, then modify it, so the chain has versions 1 and 2.
fn seed_green_two_versions(client: &mut Client, green: slates_client::VolumeId) {
  let (w0, _) = client.create_work(green, "w0").unwrap();
  client.edit(w0, "f", 0, 0, b"hello").unwrap();
  assert!(matches!(client.submit(w0).unwrap(), Submitted::Accepted(1)));
  let (w1, _) = client.create_work(green, "w1").unwrap();
  client.edit(w1, "f", 0, 5, b"world").unwrap();
  assert!(matches!(client.submit(w1).unwrap(), Submitted::Accepted(2)));
  assert_eq!(client.versions(green).unwrap(), 2);
}

/// After the restart the green's chain recovered: the head is 2, the last-changed index still names
/// f after version 0, and a new increment continues from the recovered head (version 3, not 1).
fn assert_green_recovered(client: &mut Client, green: slates_client::VolumeId) {
  assert_eq!(
    client.versions(green).unwrap(),
    2,
    "the green's chain survived the restart"
  );
  assert_eq!(
    client.changed_since(green, 0).unwrap(),
    vec!["f".to_owned()]
  );
  assert!(client.changed_since(green, 2).unwrap().is_empty());
  let (w2, base) = client.create_work(green, "w2").unwrap();
  assert_eq!(base, 2, "a new work is based on the recovered head");
  client.edit(w2, "g", 0, 0, b"new").unwrap();
  match client.submit(w2).unwrap() {
    Submitted::Accepted(v) => assert_eq!(v, 3, "the recovered green advances to version 3"),
    other => panic!("expected accept, got {other:?}"),
  }
}

/// A green's version chain is durable (§4.16, §4.8): versions committed before a restart are
/// recovered from the partition log, so the head returns, the last-changed index is intact, and a
/// new increment continues the chain. Non-vacuous: without the persisted chain the recovered green
/// would be empty (head 0) and the next submit would be version 1, not 3.
#[test]
fn a_green_chain_survives_a_daemon_restart() {
  let profile = profile();
  let instance = format!("cl-green-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
    * 2
    * usize::from(config.geometry.partitions.max(1));
  let segment = AnchorSegment::create(
    "slates-seg-cl-green",
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content("slates-con-cl-green", content_bytes)
  .unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("g", false).unwrap();
  seed_green_two_versions(&mut client, green);
  first.stop();

  let second = Daemon::start(&profile, config, source()).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  assert_green_recovered(&mut client, green);
  second.stop();
  drop(segment);
}

/// A host directory a test writes into, removed when dropped (a test's own disk, outside slates).
struct HostDir {
  path: String,
}

impl Drop for HostDir {
  fn drop(&mut self) {
    let _ = std::process::Command::new("rm")
      .args(["-rf", &self.path])
      .output();
  }
}

/// A fresh host directory named with the process id, holding `f.txt` = `disk bytes`.
fn host_dir_with_file() -> HostDir {
  // The template needs trailing `X`s: GNU mktemp (Linux) refuses one without them ("too few X's"),
  // while BSD mktemp (macOS) tolerates their absence — so the bare prefix passed on macOS and failed
  // the Linux lane. `<prefix>.XXXXXX` is the form the conformance harness already uses on both.
  let out = std::process::Command::new("mktemp")
    .args([
      "-d",
      "-t",
      &format!("slates-origin-{}.XXXXXX", std::process::id()),
    ])
    .output()
    .unwrap();
  assert!(out.status.success(), "mktemp -d");
  let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
  let wrote = std::process::Command::new("sh")
    .arg("-c")
    .arg(format!("printf '%s' 'disk bytes' > '{path}/f.txt'"))
    .output()
    .unwrap();
  assert!(wrote.status.success(), "host write");
  HostDir { path }
}

/// A green over a complete immutable base: an overlay over the host directory, pinned whole and
/// snapshotted, then the green created over that snapshot and advanced once (`f.txt` prefixed).
fn seed_green_over_base(client: &mut Client, dir: &str) -> slates_client::VolumeId {
  let overlay = client
    .create(&CreateSpec {
      name: "over".to_owned(),
      size: slates_client::SizeClass::Dynamic { max: 1 << 24 },
      names: slates_client::NamePolicy::Exact,
      require_locked: false,
      base: Some(dir.to_owned()),
    })
    .unwrap();
  assert_eq!(
    client.pin(overlay, None).unwrap(),
    1,
    "the one base file pinned"
  );
  let snapshot = client.snapshot(overlay).unwrap();
  let green = client
    .create_green_over(
      "g-base",
      false,
      slates_client::GreenBase {
        volume: overlay,
        snapshot,
      },
    )
    .unwrap();
  let (work, base) = client.create_work(green, "w0").unwrap();
  assert_eq!(base, 0);
  client.edit(work, "f.txt", 0, 0, b"agent: ").unwrap();
  assert!(matches!(
    client.submit(work).unwrap(),
    Submitted::Accepted(1)
  ));
  green
}

/// After the restart the origin is version 0 again — re-seeded from its durable record before the
/// chain replays over it — so version 0 reads the base bytes, version 1 the increment over them, and a
/// new increment continues the chain at 2.
fn assert_origin_recovered(client: &mut Client, green: slates_client::VolumeId) {
  use slates_client::ReadAt;
  assert_eq!(client.versions(green).unwrap(), 1, "the chain survived");
  assert_eq!(
    client
      .read(green, "f.txt", ReadAt::Version { version: 0 })
      .unwrap(),
    b"disk bytes",
    "version 0 is the origin, re-seeded before the chain replayed"
  );
  assert_eq!(
    client
      .read(green, "f.txt", ReadAt::Version { version: 1 })
      .unwrap(),
    b"agent: disk bytes"
  );
  let (work, base) = client.create_work(green, "w1").unwrap();
  assert_eq!(base, 1);
  client.edit(work, "f.txt", 0, 0, b"again: ").unwrap();
  assert!(matches!(
    client.submit(work).unwrap(),
    Submitted::Accepted(2)
  ));
  assert_eq!(
    client.read(green, "f.txt", ReadAt::Head).unwrap(),
    b"again: agent: disk bytes"
  );
}

/// A base-seeded green's origin is durable (§4.16 "the origin version from a snapshot"; A-9: a chain
/// from "a complete immutable base"): the green is created over a pinned overlay snapshot and advanced
/// once; after a daemon restart over the same segment, version 0 reads the origin's bytes and the chain
/// replays over it. Non-vacuous: without the persisted origin the recovered green would replay its one
/// increment over an empty version 0 — `f.txt` at version 0 would be `NotFound`, not the base bytes.
#[test]
fn a_base_seeded_greens_origin_survives_a_daemon_restart() {
  let profile = profile();
  let instance = format!("cl-origin-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
    * 2
    * usize::from(config.geometry.partitions.max(1));
  let segment = AnchorSegment::create(
    "slates-seg-cl-origin",
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content("slates-con-cl-origin", content_bytes)
  .unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    }
  };
  let dir = host_dir_with_file();
  // Captured by the harness and shown only on failure: the CI macOS lane refused the base-seeded
  // overlay `NoSpace` on 2026-09-16 where this host admits it, and the daemon's derived budget is the
  // first thing that failure has to explain (§4.2 D-12 derives it from the runner's memory).
  eprintln!(
    "the fixture's daemon: reserve_per_shard {} bytes, content segment {content_bytes} bytes, {} shards; derivations: {:#?}",
    config.reserve_per_shard, TEST_SHARDS, config.derivations
  );
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = seed_green_over_base(&mut client, &dir.path);
  first.stop();

  let second = Daemon::start(&profile, config, source()).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  assert_origin_recovered(&mut client, green);
  second.stop();
  drop(segment);
  drop(dir);
}
