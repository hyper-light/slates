//! A green's head reaches the disk (§4.16, §4.15, A-201): merged increments are checked out into the volume the green
//! was created from, and that volume lands under a grant, the landing's verdict judging the merge's files against the
//! disk. Each test drives a real daemon over a real directory in the build output and reads the disk back.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// A real landing target: macOS and Linux.
#![cfg(unix)]

use slates_client::{Client, ClientError, Landing, Submitted, VolumeId};
use slates_ipc::protocol::{CheckoutReport, Filter, GreenBase, Intent, Refusal};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::landing::approve;
use common::target::{TargetDir, target_dir};

/// Shape: one shard: the merge and landing planes are the subject, not placement.
const TEST_SHARDS: u16 = 1;

/// A daemon with its consensus group, the client, and the issuer secret a human's grant is proven under.
fn daemon(tag: &str) -> (Daemon, Client, [u8; 32]) {
  let profile = common::machine_profile();
  let instance = format!("srv-checkout-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("checkout-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  (daemon, client, secret)
}

/// The project a server holds on its disk before any agent works on it.
fn seed_project(target: &TargetDir) {
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir(format!("{}/src", target.path)).unwrap();
  target.seed("README", b"readme original");
  target.seed("src/one.txt", b"one: original line");
  target.seed("src/two.txt", b"two: original line");
}

/// The server's overlay of `target`, pinned and snapshotted, and a green over that snapshot: the volume, its base
/// and the green.
fn green_over(client: &mut Client, target: &TargetDir) -> (VolumeId, GreenBase, VolumeId) {
  let volume = client
    .create(&slates_client::CreateSpec {
      base: Some(target.path.clone()),
      ..scratch("project")
    })
    .unwrap();
  client.pin(volume, None).unwrap();
  let snapshot = client.snapshot(volume).unwrap();
  let base = GreenBase { volume, snapshot };
  let green = client
    .create_green_over("project-green", false, base)
    .unwrap();
  (volume, base, green)
}

/// A work over `green` with `edits` (path, at, delete, bytes), submitted: the outcome.
fn push(
  client: &mut Client,
  green: VolumeId,
  name: &str,
  edits: &[(&str, u64, u64, &[u8])],
) -> Submitted {
  let (work, _) = client.create_work(green, name).unwrap();
  for (path, at, delete, bytes) in edits {
    client.edit(work, path, *at, *delete, bytes).unwrap();
  }
  client.submit(work).unwrap()
}

/// Two agents' pushes, each to its own files, both accepted; the head is version 2.
fn two_agents_push(client: &mut Client, green: VolumeId) {
  let first = push(
    client,
    green,
    "agent-a",
    &[
      ("src/one.txt", 0, 3, b"ONE"),
      ("src/new.txt", 0, 0, b"created by a"),
    ],
  );
  assert_eq!(first, Submitted::Accepted(1));
  let second = push(
    client,
    green,
    "agent-b",
    &[("README", 15, 0, b" + b's line")],
  );
  assert_eq!(second, Submitted::Accepted(2));
}

/// A third agent starts from version 0 and edits what the first agent edits; the two others push; its submit is then a
/// conflict with one byte window, never an inferred merge.
fn two_agents_push_and_a_late_overlap_conflicts(client: &mut Client, green: VolumeId) {
  let (late_work, late_base) = client.create_work(green, "agent-c").unwrap();
  assert_eq!(late_base, 0);
  client.edit(late_work, "src/one.txt", 0, 3, b"Uno").unwrap();
  two_agents_push(client, green);
  let late = client.submit(late_work).unwrap();
  let Submitted::Conflict(windows) = late else {
    panic!("an overlapping edit based on version 0 conflicts, got {late:?}");
  };
  assert_eq!(windows.len(), 1, "one window: {windows:?}");
}

#[allow(clippy::disallowed_methods)]
fn disk(target: &TargetDir, path: &str) -> Vec<u8> {
  std::fs::read(format!("{}/{path}", target.path)).unwrap()
}

/// A-201. Do: a server's overlay of its project directory, a green over its snapshot, two agents' accepted pushes
/// (one edits a file and adds one, the other edits another), a third whose edit overlaps the first's; check the head
/// out into the overlay and land it under a grant. Expect: the overlap a conflict with its byte window (no inferred
/// merge); the checkout reporting version 2 with two files changed and one added; the landing `done`; the disk holding
/// every merged byte and the untouched file as it was.
#[test]
fn merged_pushes_are_checked_out_and_land_on_the_servers_disk() {
  let (daemon, mut client, secret) = daemon("land");
  let target = target_dir();
  seed_project(&target);
  let (volume, base, green) = green_over(&mut client, &target);
  two_agents_push_and_a_late_overlap_conflicts(&mut client, green);
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  let (version, report) = client.checkout(green, base, attachment).unwrap();
  assert_eq!(version, 2);
  assert_eq!(
    report,
    CheckoutReport {
      written: 3,
      made: 0,
      removed: 0,
      moded: 0
    }
  );
  let grant = approve(&mut client, &secret, volume, None, &target.path);
  match client.land(volume, None, &target.path, Filter::default(), Some(grant)) {
    Ok(Landing::Landed(outcome)) => assert_eq!(outcome.state, "done", "{outcome:?}"),
    other => panic!("the granted landing: {other:?}"),
  }
  assert_eq!(disk(&target, "src/one.txt"), b"ONE: original line");
  assert_eq!(disk(&target, "src/new.txt"), b"created by a");
  assert_eq!(disk(&target, "README"), b"readme original + b's line");
  assert_eq!(disk(&target, "src/two.txt"), b"two: original line");
  drop(client);
  daemon.stop();
}

/// A-201 with §4.15's verdict. Do: as above, but an outsider rewrites `src/one.txt` on the disk after the base was
/// witnessed and before the landing. Expect: the presentation names `/src/one.txt` as a conflict and nothing else, and
/// the outsider's bytes are what the disk still holds.
#[test]
fn an_outsiders_edit_of_a_merged_file_is_a_conflict_at_the_landing() {
  let (daemon, mut client, _) = daemon("outsider");
  let target = target_dir();
  seed_project(&target);
  let (volume, base, green) = green_over(&mut client, &target);
  two_agents_push(&mut client, green);
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client.checkout(green, base, attachment).unwrap();
  // Past the base's timestamp granularity, so the fingerprint moves.
  #[allow(clippy::disallowed_methods)] // a test may sleep (the lint's stated exception)
  std::thread::sleep(std::time::Duration::from_millis(1100));
  target.seed("src/one.txt", b"the outsider's line");
  match client.land(volume, None, &target.path, Filter::default(), None) {
    Ok(Landing::GrantRequired { conflicts, .. }) => assert_eq!(
      conflicts,
      vec!["/src/one.txt (ModifyModify)".to_owned()],
      "the presentation names the outsider's file alone"
    ),
    other => panic!("the presentation: {other:?}"),
  }
  assert_eq!(disk(&target, "src/one.txt"), b"the outsider's line");
  drop(client);
  daemon.stop();
}

/// The refusal a call met, or a panic naming what came instead.
fn refusal<T: std::fmt::Debug>(outcome: Result<T, ClientError>) -> Refusal {
  match outcome {
    Err(ClientError::Refused(refusal)) => refusal,
    other => panic!("expected a refusal, got {other:?}"),
  }
}

/// A-201's checks. Do: name a snapshot whose content is not the green's origin; then write the volume after its
/// snapshot and name the right one. Expect: `ConsistentBaseUnavailable`, then `Unsupported` (the volume changed since
/// the base), and in neither case a byte of the head in the volume's file.
#[test]
fn a_checkout_is_refused_for_a_base_that_is_not_the_greens_or_has_moved() {
  let (daemon, mut client, _) = daemon("refused");
  let target = target_dir();
  seed_project(&target);
  let (volume, base, green) = green_over(&mut client, &target);
  two_agents_push(&mut client, green);
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write(
      (volume, attachment),
      "README",
      b"the server's own edit",
      0o644,
    )
    .unwrap();
  let moved = client.snapshot(volume).unwrap();
  let other = GreenBase {
    volume,
    snapshot: moved,
  };
  assert_eq!(
    refusal(client.checkout(green, other, attachment)),
    Refusal::ConsistentBaseUnavailable
  );
  assert!(matches!(
    refusal(client.checkout(green, base, attachment)),
    Refusal::Unsupported { .. }
  ));
  assert_eq!(
    client
      .read(volume, "README", slates_ipc::protocol::ReadAt::Head)
      .unwrap(),
    b"the server's own edit"
  );
  drop(client);
  daemon.stop();
}
