//! Presenting a large landing does not monopolize its owner shard (§4.15 steps 1-3, the plan and its preliminary
//! verdicts; §4.3 "Bounded work everywhere", R9): an ungranted `land` plans the volume's diverged entries, reading and
//! hashing every diverged file, and checks each against the target, user-scaled work that must run in slices of the
//! shard's step quantum with the shard serving its other clients between them, as a pin, a green's creation and a
//! granted landing's writes do. It was one synchronous call: on a loaded Mac a presentation held its shard for 1,991 ms
//! (2026-11-07, `landing_fairness.rs` under load), past the anchor's 1,000 ms liveness budget.
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

use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, Filter, SizeClass, VolumeId};
use slates_ipc::protocol::{Intent, ReplyBody, RequestBody};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::target::target_dir;

/// Shape: one shard, so the presentation and the probing client share it — the shard is the subject.
const TEST_SHARDS: u16 = 1;
/// Shape: the files the volume holds, all its own (so all diverged): enough that planning them spans many of the
/// shard's step quanta on any machine, few enough that the sliced plan ends within [`PRESENT_WAIT`] on a box several
/// times oversubscribed.
const FILES: usize = 1_000;
/// Shape: each file's bytes: large enough that reading and hashing them is most of the plan's work.
const FILE_BYTES: usize = 16 * 1024;
/// Shape: the volume's bound over its files' bytes, room for their copies (the harness's own factor).
const BOUND_FACTOR: u64 = 8;
/// Shape: the probes of each kind taken before the presentation, for their service time on this machine.
const BASELINE_PROBES: usize = 32;
/// Shape: how long the presentation may take before the test gives up on it: two minutes, past a sliced plan of
/// this volume on a box three times oversubscribed.
const PRESENT_WAIT: Duration = Duration::from_secs(120);

/// The kinds of work the shard serves beside the plan: a status read and a provisioning (a volume created and
/// destroyed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
  Status,
  Provision,
}

/// Format: the probe kinds, in the order the prober cycles through them.
const PROBES: [Probe; 2] = [Probe::Status, Probe::Provision];

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-present-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("present-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// One probe's latency.
fn probe(client: &mut Client, kind: Probe, round: usize, status_of: VolumeId) -> Duration {
  let at = Instant::now();
  match kind {
    Probe::Status => {
      client.status(status_of).unwrap();
    }
    Probe::Provision => {
      let made = client
        .create(&scratch(&format!("present-probe-{round}")))
        .unwrap();
      client.destroy(made).unwrap();
    }
  }
  at.elapsed()
}

/// Sends an ungranted `Land` of `volume` into `target` and waits for its reply, however long the plan takes (within
/// [`PRESENT_WAIT`]): the reply and how long it took. A reply the client gives up on is the defect, so it is the reply.
fn present_and_wait(
  client: &mut Client,
  volume: VolumeId,
  target: String,
) -> (ReplyBody, Duration) {
  let began = Instant::now();
  let word = client
    .begin(&RequestBody::Land {
      volume,
      snapshot: None,
      target,
      filter: Filter::default(),
      grant: None,
    })
    .unwrap()
    .word();
  loop {
    match client.poll_reply_word(word) {
      Ok(Some(reply)) => return (reply, began.elapsed()),
      Ok(None) => {}
      Err(slates_client::ClientError::Refused(refusal)) => {
        return (ReplyBody::Refused { refusal }, began.elapsed());
      }
      Err(e) => panic!("the presentation's reply: {e}"),
    }
    assert!(
      began.elapsed() < PRESENT_WAIT,
      "the presentation answered within {PRESENT_WAIT:?}"
    );
    std::thread::yield_now();
  }
}

fn quantile(samples: &mut [Duration], ppm: u64) -> Duration {
  /// Format: parts per million.
  const PPM: u64 = 1_000_000;
  if samples.is_empty() {
    return Duration::ZERO;
  }
  samples.sort_unstable();
  let n = u64::try_from(samples.len()).unwrap();
  let rank = (n * ppm).div_ceil(PPM).max(1);
  samples[usize::try_from(rank).unwrap() - 1]
}

/// §4.15 steps 1-3, §4.3, R9. Do: on a one-shard daemon, write 1,000 files of 16 KiB into a scratch volume; take each
/// probe's baseline latency; then present a landing of the volume into an empty target on one thread while another
/// client keeps probing the same shard, pausing its baseline median between probes. Expect: the presentation answers
/// `GrantRequired` naming every file; the shard answered probes while it was planned, and none waited for half the
/// presentation or more.
#[test]
fn presenting_a_large_landing_never_holds_its_shard() {
  let (daemon, instance) = daemon("fair");
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      size: SizeClass::Dynamic {
        max: (FILES * FILE_BYTES) as u64 * BOUND_FACTOR,
      },
      ..scratch("present-volume")
    })
    .unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  for f in 0..FILES {
    let mut bytes = format!("file {f}\n").into_bytes();
    bytes.resize(FILE_BYTES, b'x');
    client
      .fs_write((volume, attachment), &format!("f{f:04}.c"), &bytes, 0o644)
      .unwrap();
  }
  let mut prober = connect(&instance);
  let status_of = prober.create(&scratch("present-status")).unwrap();
  let mut round = 0usize;
  let mut baseline = Vec::new();
  for _ in 0..BASELINE_PROBES {
    for kind in PROBES {
      baseline.push(probe(&mut prober, kind, round, status_of));
      round += 1;
    }
  }
  let target_path = target.path.clone();
  let presenting = std::thread::spawn(move || {
    let began = Instant::now();
    let (reply, took) = present_and_wait(&mut client, volume, target_path);
    (client, began, took, reply)
  });
  // The prober pauses its baseline median between probes, so the shard serves probes about half the time.
  let pace = quantile(&mut baseline.clone(), 500_000);
  let mut during = Vec::new();
  while !presenting.is_finished() {
    let kind = PROBES[round % PROBES.len()];
    let at = Instant::now();
    let took = probe(&mut prober, kind, round, status_of);
    during.push((at, took));
    round += 1;
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(pace);
  }
  let (client, began, took, reply) = presenting.join().unwrap();
  drop(prober);
  drop(client);
  daemon.stop();
  let mut inside: Vec<Duration> = during
    .iter()
    .filter(|(at, probe)| *at >= began && *at + *probe <= began + took)
    .map(|(_, probe)| *probe)
    .collect();
  let probes_inside = inside.len();
  let longest = during
    .iter()
    .map(|(_, probe)| *probe)
    .max()
    .unwrap_or_default();
  let entries = match &reply {
    ReplyBody::GrantRequired { summary, .. } => {
      summary.by_action.iter().map(|a| a.count).sum::<u64>()
    }
    _ => 0,
  };
  eprintln!(
    "presentation of {FILES} files of {FILE_BYTES} bytes: {took:?}, {entries} entries; {probes_inside} probes inside \
     it — p50 {:?}, p99 {:?}, longest of all {longest:?}; before it the longest {:?}",
    quantile(&mut inside.clone(), 500_000),
    quantile(&mut inside, 990_000),
    baseline.iter().max().copied().unwrap_or_default()
  );
  assert_eq!(
    entries, FILES as u64,
    "the presentation names every file: {reply:?}"
  );
  assert!(
    probes_inside > 1,
    "the shard answered probes while the plan ran ({probes_inside} inside its {took:?})"
  );
  assert!(
    longest * 2 < took,
    "no probe waited for half the presentation or more ({longest:?} of {took:?})"
  );
}

/// §4.15 steps 1–4, AUD-29-25, §4.3, R9 (a granted landing's plan). Do: on a one-shard daemon, write 1,000 files of 16
/// KiB into a scratch volume, approve its landing into an empty target, take the probes' baseline, then land it under
/// the grant on one thread while another client probes the same shard. Expect: the landing is done with every file
/// written; the shard answered probes while it ran, and none waited for half the landing or more. The run's writes were
/// sliced (AUD-29-25); until 2026-11-08 its plan was not, and the plan of these files (6.2 s on a Mac in a debug build)
/// was one poll longer than all its sliced writes together.
#[test]
fn a_granted_landing_never_holds_its_shard_while_it_is_planned() {
  let (daemon, instance) = daemon("granted");
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      size: SizeClass::Dynamic {
        max: (FILES * FILE_BYTES) as u64 * BOUND_FACTOR,
      },
      ..scratch("granted-volume")
    })
    .unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  for f in 0..FILES {
    let mut bytes = format!("file {f}\n").into_bytes();
    bytes.resize(FILE_BYTES, b'x');
    client
      .fs_write((volume, attachment), &format!("f{f:04}.c"), &bytes, 0o644)
      .unwrap();
  }
  let secret = daemon.segment().issuer_secret().unwrap();
  let grant = common::landing::approve(&mut client, &secret, volume, None, &target.path);
  let mut prober = connect(&instance);
  let status_of = prober.create(&scratch("granted-status")).unwrap();
  let mut round = 0usize;
  let mut baseline = Vec::new();
  for _ in 0..BASELINE_PROBES {
    for kind in PROBES {
      baseline.push(probe(&mut prober, kind, round, status_of));
      round += 1;
    }
  }
  let target_path = target.path.clone();
  let landing = std::thread::spawn(move || {
    let began = Instant::now();
    let landed = client.land(volume, None, &target_path, Filter::default(), Some(grant));
    (began, began.elapsed(), landed)
  });
  let pace = quantile(&mut baseline.clone(), 500_000);
  let mut during = Vec::new();
  while !landing.is_finished() {
    let kind = PROBES[round % PROBES.len()];
    let at = Instant::now();
    let took = probe(&mut prober, kind, round, status_of);
    during.push((at, took));
    round += 1;
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(pace);
  }
  let (began, took, landed) = landing.join().unwrap();
  drop(prober);
  daemon.stop();
  let probes_inside = during
    .iter()
    .filter(|(at, probe)| *at >= began && *at + *probe <= began + took)
    .count();
  let longest = during
    .iter()
    .map(|(_, probe)| *probe)
    .max()
    .unwrap_or_default();
  eprintln!(
    "granted landing of {FILES} files of {FILE_BYTES} bytes: {took:?}, {landed:?}; {probes_inside} probes inside it, \
     the longest of all {longest:?}"
  );
  let Ok(slates_client::Landing::Landed(outcome)) = landed else {
    panic!("the granted landing: {landed:?}");
  };
  assert_eq!(outcome.state, "done");
  assert_eq!(outcome.written, FILES as u64);
  assert!(
    probes_inside > 1,
    "the shard answered probes while the landing ran ({probes_inside} inside its {took:?})"
  );
  assert!(
    longest * 2 < took,
    "no probe waited for half the landing or more ({longest:?} of {took:?})"
  );
}
