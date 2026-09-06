//! The client's tests (Phase 2 task 5; §4.4, §4.7, §4.9, AC-2.3's idempotency half): the
//! typed verbs over an in-process daemon, and a session that outlives a daemon restart: the
//! same client reconnects under its id and its retry meets the completion record; the
//! volume is rebuilt from the recovered catalog; a second client cannot take a live session.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_anchor::AnchorSegment;
use slates_client::{Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass};
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
/// restarted daemon, which rebuilt the volume; the local-only snapshot was reconciled away.
fn assert_served_after_restart(
  client: &mut Client,
  kept: slates_client::VolumeId,
  session: slates_client::Session,
) {
  let report = client.status(kept).unwrap();
  assert_eq!(report.name, "kept");
  assert_eq!(client.reconnects(), 1, "one reconnect, under the old id");
  assert_eq!(client.client_id(), session.client_id);
  assert_eq!(
    report.snapshots, 0,
    "a snapshot held only in the old process's memory is reconciled out of the catalog"
  );
  assert_eq!(report.head.value, 0, "and the head is reset");
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
  wait_until_listed(client, &["kept"]);
}

/// A session outlives a daemon restart over the same segment (the test plays the anchor):
/// the client's next call finds the daemon gone, reconnects under its id and is served by
/// the restarted daemon, which rebuilt the volume from the recovered catalog; its retry of
/// the create it made before meets the completion record (the same id, no second volume);
/// a local-only snapshot is reconciled away; a second client cannot take the live session.
#[test]
fn a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record() {
  let profile = profile();
  let instance = format!("cl-resume-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  // The test plays the anchor: it holds the segment and its content object across both daemons, so
  // anchor-owned volume storage survives the restart (§4.8). The content object is one shard's
  // reserve times the partitions; it is lazily backed, so its unused tail costs no RAM.
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
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
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  let create_id = client.last_request();
  let _snapshot = client.snapshot(kept).unwrap();
  assert_eq!(client.status(kept).unwrap().snapshots, 1);
  let session = client.session();
  first.stop();
  let second = Daemon::start(&profile, config, source()).unwrap();
  assert_served_after_restart(&mut client, kept, session);
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
