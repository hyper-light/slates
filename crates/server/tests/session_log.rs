//! The session log (`slates log`, A-sessionlog): a Smartlog-sized view of what a person supervising many agents must
//! decide. Ten works over one green, one of them collided and one landing presented and awaiting its grant: the view
//! names exactly those two as needing the human, and every work's base, the head it falls behind and its last
//! submission beside them. Before it, the supervisor cross-referenced volume ids, `versions`, a conflict's JSON and the
//! audit to find them.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use slates_client::{
  Client, Filter, GrantScope, Landing, NeedsYou, SessionLog, Submitted, VolumeId, WorkMark,
};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::target::target_dir;

/// Shape: two shards, so the log is gathered across shards.
const TEST_SHARDS: u16 = 2;
/// Shape: the works a supervisor watches over one green.
const WORKS: usize = 10;

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-session-log-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-session-log-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// [`WORKS`] works made on `green` at version 1: all but the last two submit disjoint new files, the ninth rewrites
/// `/shared.txt` and the tenth the same span differently, every one accepted but the tenth, which collides. The works,
/// and the tenth.
fn works_submitted(client: &mut Client, green: VolumeId) -> (Vec<VolumeId>, VolumeId) {
  let works: Vec<VolumeId> = (0..WORKS)
    .map(|i| {
      let (work, base) = client.create_work(green, &format!("agent-{i}")).unwrap();
      assert_eq!(base, 1, "every work starts from version 1");
      work
    })
    .collect();
  for (i, work) in works.iter().take(WORKS - 2).enumerate() {
    client
      .edit(*work, &format!("/f{i}.txt"), 0, 0, b"mine\n")
      .unwrap();
    assert!(matches!(
      client.submit(*work).unwrap(),
      Submitted::Accepted(_)
    ));
  }
  let ours = works[WORKS - 2];
  let theirs = works[WORKS - 1];
  client.edit(ours, "/shared.txt", 0, 4, b"ours").unwrap();
  assert!(matches!(
    client.submit(ours).unwrap(),
    Submitted::Accepted(_)
  ));
  client.edit(theirs, "/shared.txt", 0, 4, b"them").unwrap();
  assert!(matches!(
    client.submit(theirs).unwrap(),
    Submitted::Conflict(_)
  ));
  (works, theirs)
}

/// Every one of `works` listed once on `green`, with where it started and how its last submission ended: `theirs`
/// collided on `shared.txt` at version 1, every other accepted and reset onto its version.
fn each_work_listed(log: &SessionLog, works: &[VolumeId], green: VolumeId, theirs: VolumeId) {
  let listed: Vec<_> = log.works.iter().filter(|w| works.contains(&w.id)).collect();
  assert_eq!(listed.len(), WORKS, "every work once: {:?}", log.works);
  for line in &listed {
    assert_eq!(line.green, green);
    if line.id == theirs {
      assert_eq!(line.base, 1, "the collided work still stands on version 1");
      let WorkMark::Collided { first, windows, .. } = &line.mark else {
        panic!("the tenth work collided: {line:?}");
      };
      assert!(*windows >= 1);
      assert_eq!(first.as_ref().map(|w| w.path.as_str()), Some("shared.txt"));
    } else {
      let WorkMark::Accepted { version } = line.mark else {
        panic!("an accepted work: {line:?}");
      };
      assert_eq!(
        line.base, version,
        "an accepted work is reset onto its version"
      );
    }
  }
}

/// The log names exactly two items as needing the human: `theirs`, collided on `shared.txt` with its green `head - 1`
/// past it, and the plain volume's landing awaiting its grant by manifest.
fn needs_exactly_two(
  log: &SessionLog,
  (theirs, head): (VolumeId, u64),
  (landing, manifest, plain): (u64, [u8; 32], VolumeId),
) {
  let needs = log.needs_you();
  assert_eq!(
    needs.len(),
    2,
    "exactly the collision and the landing: {needs:?}"
  );
  assert!(
    needs.iter().any(
      |n| matches!(n, NeedsYou::Collided { work, path, behind, .. }
    if *work == theirs && path == "shared.txt" && *behind == head - 1)
    ),
    "the collided work, its path and how far its green moved past it: {needs:?}"
  );
  assert!(
    needs.iter().any(
      |n| matches!(n, NeedsYou::AwaitingGrant { landing: l, manifest: m, volume, .. }
    if *l == landing && *m == manifest && *volume == plain)
    ),
    "the landing awaiting its grant, by manifest: {needs:?}"
  );
}

/// The landing granted under the issuer's proof and run into `path`: it no longer awaits.
fn granted_and_run(
  daemon: &Daemon,
  client: &mut Client,
  (landing, manifest, plain): (u64, [u8; 32], VolumeId),
  path: &str,
) {
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::grant_proof(
    &secret,
    landing,
    &manifest,
    GrantScope::Once,
    common::landing::GRANT_TERM_NS,
  );
  let granted = client
    .grant(
      landing,
      manifest,
      GrantScope::Once,
      common::landing::GRANT_TERM_NS,
      proof,
    )
    .unwrap();
  client
    .land(plain, None, path, Filter::default(), Some(granted))
    .unwrap();
  let after = client.session_log().unwrap().needs_you();
  assert!(
    !after
      .iter()
      .any(|n| matches!(n, NeedsYou::AwaitingGrant { landing: l, .. } if *l == landing)),
    "a landing granted and run no longer awaits: {after:?}"
  );
}

/// Do: on a two-shard daemon, seed a green with `/shared.txt` (version 1); make ten works at version 1; eight submit
/// disjoint new files and the ninth rewrites `/shared.txt`, all accepted; the tenth rewrites the same span differently
/// and collides. Present a landing of a plain volume and leave it ungranted. Expect: the log lists all ten works with
/// base and outcome, the green's head, and names exactly two items as needing the human: the collided work (its path)
/// and the landing awaiting its grant (its manifest).
#[test]
fn ten_works_one_collision_one_pending_landing_names_exactly_those_two() {
  let (daemon, instance) = daemon("ten");
  let mut client = connect(&instance);
  let green = client.create_green("watched", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  client.edit(seed, "/shared.txt", 0, 0, b"base\n").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));

  let (works, theirs) = works_submitted(&mut client, green);

  let target = target_dir();
  let plain = client.create(&scratch("landed")).unwrap();
  let Landing::GrantRequired {
    landing, manifest, ..
  } = client
    .land(plain, None, &target.path, Filter::default(), None)
    .unwrap()
  else {
    panic!("the landing is presented, awaiting a grant");
  };

  let log = client.session_log().expect("the session log");
  let head = client.versions(green).unwrap();
  assert_eq!(head, 1 + WORKS as u64 - 1, "seed and nine accepted works");
  assert_eq!(
    log.head_of(green),
    Some(head),
    "the log carries the green's head"
  );

  // Every work is listed once, with where it started and how its last submission ended.
  each_work_listed(&log, &works, green, theirs);

  needs_exactly_two(&log, (theirs, head), (landing, manifest, plain));

  // Rebasing does not resolve the collision (the same span changed), granting is not this view's verb (R10): the log
  // only shows them. Once the landing is granted and run it no longer needs the human.
  granted_and_run(
    &daemon,
    &mut client,
    (landing, manifest, plain),
    &target.path,
  );
  drop(client);
  daemon.stop();
}
