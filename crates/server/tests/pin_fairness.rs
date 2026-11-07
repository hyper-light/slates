//! A pin of a large base does not monopolize its owner shard (§4.4 `pin`, §4.3 "Bounded work everywhere", R9): a
//! whole-base pin reads every file of the tree an overlay covers into the store, user-scaled work that must run in
//! slices of the shard's step quantum, the shard serving its other clients between them, as a granted landing and an
//! image build do. It was one synchronous call: the kernel's `Documentation/` (11,608 files) held its shard for one
//! poll of 9,194 ms on a Mac (2026-11-06), past the anchor's 1,000 ms liveness budget, so an anchored daemon pinning it
//! is killed as dead, and every verb on that shard waited for the whole pin.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]
// A real overlay base on the host's disk: macOS and Linux.
#![cfg(unix)]

use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, SizeClass, VolumeId};
use slates_ipc::protocol::{ReplyBody, RequestBody};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::target::{TargetDir, target_dir};

/// Shape: one shard, so the pin and the probing client share it — the shard is the subject.
const TEST_SHARDS: u16 = 1;
/// Shape: the files of the base, spread over [`DIRS`] directories: enough that reading them from the disk spans many
/// of the shard's step quanta on any machine (a pin costs 0.26 to 0.43 ms a file over a real disk on a Mac, measured
/// 2026-11-06, so one whole pin of them took 0.25 s in a debug build), and few enough that a pin stepped a file a slice
/// ends within [`PIN_WAIT`] on a box three times oversubscribed, where a slice's yield costs the shard a scheduler
/// quantum (about 25 ms a slice at load 63 on 18 CPUs; GAPS, A-218's `kevent` nap).
const FILES: usize = 1_000;
/// Shape: the directories the files are spread over, so the pin walks a tree, not one directory.
const DIRS: usize = 20;
/// Shape: each file's bytes: small, so the time is the per-file work, not the bytes.
const FILE_BYTES: usize = 512;
/// Shape: the overlay's bound over the base's bytes, room for the pin's copy (the harness's own factor).
const BOUND_FACTOR: u64 = 8;
/// Shape: the probes of each kind taken before the pin, for their service time on this machine.
const BASELINE_PROBES: usize = 32;
/// Shape: how long the pin may take before the test gives up on it: two minutes, past a sliced pin of this base on a
/// box three times oversubscribed ([`FILES`]).
const PIN_WAIT: Duration = Duration::from_secs(120);

/// The kinds of work the shard serves beside the pin: a status read and a provisioning (a volume created and
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
  let instance = format!("srv-pin-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-pin-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The base on disk: [`FILES`] files of [`FILE_BYTES`] over [`DIRS`] directories.
fn seed(target: &TargetDir) {
  for d in 0..DIRS {
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir(format!("{}/d{d}", target.path)).unwrap();
  }
  for f in 0..FILES {
    target.seed(&format!("d{}/f{f}.c", f % DIRS), &vec![b'x'; FILE_BYTES]);
  }
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
        .create(&scratch(&format!("pin-probe-{round}")))
        .unwrap();
      client.destroy(made).unwrap();
    }
  }
  at.elapsed()
}

/// Pins `overlay` whole and waits for its reply, however long the pin takes (within [`PIN_WAIT`]): the reply and
/// how long it took.
fn pin_and_wait(client: &mut Client, overlay: VolumeId) -> (ReplyBody, Duration) {
  let began = Instant::now();
  let word = client
    .begin(&RequestBody::Pin {
      volume: overlay,
      paths: None,
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
      Err(e) => panic!("the pin's reply: {e}"),
    }
    assert!(
      began.elapsed() < PIN_WAIT,
      "the pin answered within {PIN_WAIT:?}"
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

/// §4.4 `pin`, §4.3, R9. Do: on a one-shard daemon, overlay a base of 1,000 files in 20 directories, take each
/// probe's baseline latency, then pin the overlay whole on one thread while another client keeps probing the same
/// shard, pausing its baseline median between probes: a status read, and a volume created and destroyed. Expect: the pin answers `Pinned` with every file; the
/// shard answered probes while the pin ran, and none waited for half the pin or more; and the snapshot taken after
/// it is a complete immutable base (a green is created over it), so the sliced pin pinned the whole tree.
#[test]
fn a_pin_of_a_large_base_never_holds_its_shard() {
  let (daemon, instance) = daemon("fair");
  let target = target_dir();
  seed(&target);
  let mut client = connect(&instance);
  let overlay = client
    .create(&CreateSpec {
      base: Some(target.path.clone()),
      size: SizeClass::Bounded {
        limit: (FILES * FILE_BYTES) as u64 * BOUND_FACTOR,
      },
      ..scratch("pin-base")
    })
    .unwrap();
  let mut prober = connect(&instance);
  let status_of = prober.create(&scratch("pin-status")).unwrap();
  let mut round = 0usize;
  let mut baseline = Vec::new();
  for _ in 0..BASELINE_PROBES {
    for kind in PROBES {
      baseline.push(probe(&mut prober, kind, round, status_of));
      round += 1;
    }
  }
  let pinning = std::thread::spawn(move || {
    let began = Instant::now();
    let (reply, took) = pin_and_wait(&mut client, overlay);
    (client, began, took, reply)
  });
  // The prober pauses its baseline median between probes, so the shard serves probes about half the time: a busy
  // shard, not one the prober alone fills. A pin that never ended under that load would be the defect.
  let pace = quantile(&mut baseline.clone(), 500_000);
  let mut during = Vec::new();
  while !pinning.is_finished() {
    let kind = PROBES[round % PROBES.len()];
    let at = Instant::now();
    let took = probe(&mut prober, kind, round, status_of);
    during.push((at, took));
    round += 1;
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(pace);
  }
  let (mut client, began, took, reply) = pinning.join().unwrap();
  let snapshot = client.snapshot(overlay).unwrap();
  let green = client.create_green_over(
    "pin-green",
    false,
    slates_ipc::protocol::GreenBase {
      volume: overlay,
      snapshot,
    },
  );
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
  eprintln!(
    "pin of {FILES} files: {took:?}, reply {reply:?}; {probes_inside} probes inside it — p50 {:?}, p99 {:?}, longest \
     of all {longest:?}; before it the longest {:?}",
    quantile(&mut inside.clone(), 500_000),
    quantile(&mut inside, 990_000),
    baseline.iter().max().copied().unwrap_or_default()
  );
  assert!(
    matches!(reply, ReplyBody::Pinned { entries } if entries == FILES as u64),
    "the pin pinned every file: {reply:?}"
  );
  assert!(
    probes_inside > 1,
    "the shard answered probes while the pin ran ({probes_inside} inside its {took:?})"
  );
  assert!(
    longest * 2 < took,
    "no probe waited for half the pin or more ({longest:?} of {took:?})"
  );
  assert!(
    green.is_ok(),
    "the snapshot after the pin is a complete base: {green:?}"
  );
}
