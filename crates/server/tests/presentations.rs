//! A landing's presentation as a client meets it (§4.15 steps 1–3, AUD-29-07, AUD-29-02, A-49): bounded per shard,
//! refused before it allocates anything past the bound, replaced by its client's re-presentation, and of a named
//! snapshot exactly as the snapshot froze. These ran inside the shard while a presentation was answered in the verb;
//! since 2026-11-08 it is planned in slices and its reply deferred (`presentation_fairness.rs`), so a client drives
//! them.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]
// A real landing target on the host's disk: macOS and Linux.
#![cfg(unix)]

use slates_client::{Client, ClientError, Filter, Landing, Refusal, SnapshotId, VolumeId};
use slates_ipc::protocol::{GrantScope, Intent};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::landing::GRANT_TERM_NS;
use common::target::target_dir;

/// Shape: one shard, so every presentation meets the same shard's bound.
const TEST_SHARDS: u16 = 1;

fn daemon(tag: &str, awaiting_bound: Option<usize>) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-presented-{tag}-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  if let Some(bound) = awaiting_bound {
    config.landings_awaiting_per_shard = bound;
  }
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("presented-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// A presentation of `volume` (its `snapshot`, or the head) into `target` by `client`.
fn present(
  client: &mut Client,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  target: &str,
) -> Result<Landing, ClientError> {
  client.land(volume, snapshot, target, Filter::default(), None)
}

/// The landings the daemon's shards hold awaiting a grant.
fn awaiting(client: &mut Client) -> u64 {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .map(|shard| shard.landings_awaiting)
    .sum()
}

/// The `landing_planned` records the audit lists.
fn planned_records(client: &mut Client) -> usize {
  client
    .audit(0)
    .unwrap()
    .iter()
    .filter(|record| record.kind == "landing_planned")
    .count()
}

/// §4.15 step 3 (AUD-29-07): the landings a shard holds awaiting a grant are bounded, and a presentation past the
/// bound is refused before it allocates anything, while a client re-presenting the same volume and target replaces its
/// own and needs no room. Do: with the bound at one, present volume A, then B, then A again (into an empty target:
/// nothing is granted, so nothing is written). Expect: B is refused `LandingsAwaitingFull` and changes nothing — one
/// presentation awaiting, one `landing_planned` record, and A's re-presentation takes the very next id, so none was
/// spent on B; A's second presentation replaces its first (still one), and the first's id is no longer grantable.
/// Before 2026-09-29 nothing bounded the presentations.
#[test]
fn a_presentation_past_the_bound_is_refused_and_changes_nothing() {
  let (daemon, instance) = daemon("bound", Some(1));
  let target = target_dir();
  let mut client = connect(&instance);
  let a = client.create(&scratch("presented-a")).unwrap();
  let b = client.create(&scratch("presented-b")).unwrap();
  let Ok(Landing::GrantRequired {
    landing: first,
    manifest: first_manifest,
    ..
  }) = present(&mut client, a, None, &target.path)
  else {
    panic!("A was not presented");
  };
  assert_eq!(awaiting(&mut client), 1);
  let planned = planned_records(&mut client);
  let refusal = present(&mut client, b, None, &target.path);
  assert!(
    matches!(
      refusal,
      Err(ClientError::Refused(Refusal::LandingsAwaitingFull))
    ),
    "{refusal:?}"
  );
  assert_eq!(awaiting(&mut client), 1, "the refusal presented nothing");
  assert_eq!(
    planned_records(&mut client),
    planned,
    "nor recorded anything"
  );
  let Ok(Landing::GrantRequired { landing: again, .. }) =
    present(&mut client, a, None, &target.path)
  else {
    panic!("A was not presented again");
  };
  assert_eq!(again, first + 1, "no id was spent on the refusal");
  assert_eq!(
    awaiting(&mut client),
    1,
    "the re-presentation replaced the first"
  );
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::grant_proof(
    &secret,
    first,
    &first_manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let replaced = client.grant(
    first,
    first_manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
    proof,
  );
  assert!(
    matches!(replaced, Err(ClientError::Refused(Refusal::NotFound))),
    "the replaced presentation is no longer grantable: {replaced:?}"
  );
  drop(client);
  daemon.stop();
}

/// §4.15 (AUD-29-02, A-49): a landing of a named snapshot presents that snapshot's state, whatever the head has become
/// since, and a snapshot the volume does not hold is `NotFound` before any host access. Do: snapshot a volume holding
/// "v1" and present the snapshot's landing (into an empty target: nothing is granted, so nothing is written); write
/// "v2" at the head and present the snapshot again, then the head; present a snapshot the volume never had. Expect: the
/// snapshot's second presentation carries its first's manifest and the head's another; the unknown one is `NotFound`.
/// Before 2026-09-30 the changed head was refused `Unsupported`; before 2026-09-29 the head was landed under the
/// snapshot's name.
#[test]
fn a_landing_of_a_named_snapshot_presents_that_snapshot_whatever_the_head_became() {
  let (daemon, instance) = daemon("snapshot", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = client.create(&scratch("landed")).unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "f", b"v1", 0o644)
    .unwrap();
  let snapshot = client.snapshot(volume).unwrap();
  let manifest_of = |presented: Result<Landing, ClientError>| match presented {
    Ok(Landing::GrantRequired { manifest, .. }) => manifest,
    other => panic!("not presented: {other:?}"),
  };
  let before = manifest_of(present(&mut client, volume, Some(snapshot), &target.path));
  client
    .fs_write((volume, attachment), "f", b"v2", 0o644)
    .unwrap();
  assert_eq!(
    manifest_of(present(&mut client, volume, Some(snapshot), &target.path)),
    before,
    "the snapshot presents what it froze"
  );
  assert_ne!(
    manifest_of(present(&mut client, volume, None, &target.path)),
    before,
    "the head presents what it became"
  );
  let never = SnapshotId {
    value: snapshot.value.wrapping_add(1 << u32::BITS),
  };
  let unknown = present(&mut client, volume, Some(never), &target.path);
  assert!(
    matches!(unknown, Err(ClientError::Refused(Refusal::NotFound))),
    "a snapshot the volume does not hold: {unknown:?}"
  );
  drop(client);
  daemon.stop();
}
