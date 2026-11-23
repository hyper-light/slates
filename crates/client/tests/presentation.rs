//! A-874: R10's three-step flow across real processes (§4.15 steps 1-3; main 01a20182's ruling of 2026-11-23;
//! `docs/bugs/2026-11-23-a-landings-presentation-went-with-its-presenting-client.md`): a landing is presented by one
//! process, which then exits, and granted and landed by another, as the CLI runs `slates land`, `slates grant` and
//! `slates land --grant`. The test binary re-invoked is the presenter (an environment variable selects the role), so
//! its exit is a real process's, which the daemon reclaims at its hang-up (076746072): the end of its control socket's
//! stream on Linux, its process's exit on macOS. Until 2026-11-23 this file held AUD-29-07's opposite rule, a killed
//! presenter's landing abandoned with it (`a_killed_clients_presented_landing_is_abandoned_with_it`), which A-874
//! reverses.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slates_client::{
  Client, CreateSpec, Deadlines, Filter, Landing, NamePolicy, SizeClass, SnapshotId, VolumeId,
};
use slates_ipc::protocol::{GrantScope, Intent};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the variable that makes the re-invoked binary the presenter, and the ones that tell it what to present.
const ROLE: &str = "SLATES_PRESENTATION_TEST_ROLE";
const ROLE_INSTANCE: &str = "SLATES_PRESENTATION_TEST_INSTANCE";
const ROLE_VOLUME: &str = "SLATES_PRESENTATION_TEST_VOLUME";
const ROLE_SNAPSHOT: &str = "SLATES_PRESENTATION_TEST_SNAPSHOT";
const ROLE_TARGET: &str = "SLATES_PRESENTATION_TEST_TARGET";
/// Format: the tag of the presenter's report line, found anywhere in a line (libtest's banner precedes it).
const TAG: &str = "PRESENTED";
/// Shape: the machine profile's probe budget, as the client's other daemon tests take it.
const PROBE_MS: u64 = 5;
/// Shape: the grant's validity, far past the test's run.
const GRANT_TERM_NS: u64 = 600_000_000_000;
/// Shape: the bytes the volume's file holds, and then the target's.
const BYTES: &[u8] = b"landed after its presenter left";

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> [u8; N] {
  let mut bytes = [0u8; N];
  for (at, byte) in bytes.iter_mut().enumerate() {
    *byte = u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).unwrap();
  }
  bytes
}

/// The presenter: connects, presents the named snapshot's landing into the target, prints its id and manifest, and
/// exits, its client never retired by a verb.
fn presenter() {
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let volume = VolumeId {
    bytes: unhex(&std::env::var(ROLE_VOLUME).unwrap()),
  };
  let snapshot = SnapshotId {
    value: std::env::var(ROLE_SNAPSHOT).unwrap().parse().unwrap(),
  };
  let target = std::env::var(ROLE_TARGET).unwrap();
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  let Ok(Landing::GrantRequired {
    landing, manifest, ..
  }) = client.land(volume, Some(snapshot), &target, Filter::default(), None)
  else {
    panic!("the landing was not presented");
  };
  println!("{TAG} {landing} {}", hex(&manifest));
}

/// The presenter run to its exit: the landing it presented and its manifest.
fn presented_by_another_process(
  instance: &str,
  volume: VolumeId,
  snapshot: SnapshotId,
  target: &str,
) -> (u64, [u8; 32]) {
  let mut child = Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "a_presentation_outlives_the_process_that_presented_it",
      "--nocapture",
    ])
    .env(ROLE, "presenter")
    .env(ROLE_INSTANCE, instance)
    .env(ROLE_VOLUME, hex(&volume.bytes))
    .env(ROLE_SNAPSHOT, snapshot.value.to_string())
    .env(ROLE_TARGET, target)
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
  // Read to its end, so the child's later output (libtest's) never meets a closed pipe.
  let lines: Vec<String> = BufReader::new(child.stdout.take().unwrap())
    .lines()
    .map(|line| line.expect("read the presenter's report"))
    .collect();
  let reported = lines
    .into_iter()
    .find_map(|line| {
      let (_, rest) = line.split_once(&format!("{TAG} "))?;
      let (landing, manifest) = rest.trim().split_once(' ')?;
      Some((landing.parse().unwrap(), unhex(manifest)))
    })
    .expect("the presenter reported its landing");
  assert!(
    child.wait().unwrap().success(),
    "the presenter exited cleanly"
  );
  reported
}

/// Waits until the daemon has reaped a client more than `before` (its status's `clients_reaped`), within the
/// observation budget.
fn reaped_past(client: &mut Client, before: u64) {
  let began = Instant::now();
  while client.daemon_status().unwrap().clients_reaped <= before {
    assert!(
      began.elapsed() < Duration::from_nanos(OBSERVE_BUDGET_NS),
      "the presenter's client was reaped within the observation budget"
    );
    std::thread::yield_now();
  }
}

/// R10's flow (main 01a20182's ruling of 2026-11-23). Do: write a file into a volume and snapshot it through this
/// process's client; present the snapshot's landing from a second process, which exits; wait until the daemon has
/// reaped that process's client; then grant the manifest and land under the grant through this process's client.
/// Expect the grant taken, the landing `Landed` and the file's bytes on disk: a presentation describes a volume's changes
/// against a target, not the process that asked. Red before: the grant refused `NotFound` ("presentation 1 (replaced,
/// landed, or its client went away)"), the presentation abandoned with its client at its reap (AUD-29-07, met by
/// 076746072's reclaim at hang-up).
#[test]
fn a_presentation_outlives_the_process_that_presented_it() {
  if std::env::var_os(ROLE).is_some() {
    presenter();
    return;
  }
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("cl-presented-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(1)),
    SegmentSource::Create {
      name: format!("slates-seg-cl-presented-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  // A-50: the landing's target is a real host directory in the build output (`CARGO_TARGET_TMPDIR`), never `/tmp`.
  let target = format!(
    "{}/slates-presented-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  let made = Command::new("mkdir")
    .args(["-p", &target])
    .output()
    .unwrap();
  assert!(made.status.success(), "mkdir -p {target}");
  let mut owner = Client::connect(&instance, deadlines()).unwrap();
  let volume = owner
    .create(&CreateSpec {
      name: "presented".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let attachment = owner
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  owner
    .fs_write((volume, attachment), "a.txt", BYTES, 0o644)
    .unwrap();
  let snapshot = owner.snapshot(volume).unwrap();
  let before = owner.daemon_status().unwrap().clients_reaped;
  let (landing, manifest) = presented_by_another_process(&instance, volume, snapshot, &target);
  reaped_past(&mut owner, before);
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::grant_proof(
    &secret,
    landing,
    &manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let grant = owner
    .grant(landing, manifest, GrantScope::Once, GRANT_TERM_NS, proof)
    .expect("the presentation outlived the process that presented it and is granted");
  let landed = owner
    .land(
      volume,
      Some(snapshot),
      &target,
      Filter::default(),
      Some(grant),
    )
    .unwrap();
  assert!(matches!(landed, Landing::Landed(_)), "{landed:?}");
  let read = Command::new("cat")
    .arg(format!("{target}/a.txt"))
    .output()
    .unwrap();
  assert_eq!(read.stdout, BYTES, "the file landed");
  let _ = Command::new("rm").args(["-rf", &target]).output();
  drop(owner);
  daemon.stop();
}
