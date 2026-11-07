//! Creating a green over a large base does not monopolize its owner shard (§4.16 "Green creation", A-9, A-217; §4.3
//! "Bounded work everywhere", R9): `CreateGreen { base }` walks the base snapshot into the green's origin, reading every
//! file of the tree, user-scaled work that must run in slices of the shard's step quantum with the shard serving its
//! other clients between them, as a pin, a granted landing and an image build do. It was one synchronous call: the
//! whole Linux kernel (97,229 files) held its shard for one poll of 69,028 ms on a Mac (2026-11-06, loadtest-scale),
//! past the anchor's 1,000 ms liveness budget, so an anchored daemon creating that green is killed as dead, and every
//! verb on the shard waited for the whole walk.
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

use slates_client::{Client, CreateSpec, ReadAt, SizeClass, VolumeId};
use slates_ipc::protocol::{GreenBase, ReplyBody, RequestBody};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::target::{TargetDir, target_dir};

/// Shape: one shard, so the green's walk and the probing client share it — the shard is the subject.
const TEST_SHARDS: u16 = 1;
/// Shape: the files of the base, spread over [`DIRS`] directories: enough that walking them into an origin spans many
/// of the shard's step quanta on any machine, and few enough that the pin that makes the base complete stays short
/// (a pin costs 0.26 to 0.43 ms a file over a real disk on a Mac, 2026-11-06).
const FILES: usize = 4_000;
/// Shape: the directories the files are spread over, so the walk walks a tree, not one directory.
const DIRS: usize = 40;
/// Shape: each file's bytes: large enough that copying and hashing the tree's bytes is most of the walk's work, as it
/// is over a real source tree (the kernel's `fs/` averages about 16 KB a file).
const FILE_BYTES: usize = 16 * 1024;
/// Shape: the overlay's bound over the base's bytes, room for the pin's copy (the harness's own factor).
const BOUND_FACTOR: u64 = 8;
/// Shape: the probes of each kind taken before the walk, for their service time on this machine.
const BASELINE_PROBES: usize = 32;
/// Shape: how long the green's creation may take before the test gives up on it: two minutes, past a sliced walk of
/// this base on a box three times oversubscribed.
const CREATE_WAIT: Duration = Duration::from_secs(120);

/// The kinds of work the shard serves beside the walk: a status read and a provisioning (a volume created and
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
  let instance = format!("srv-green-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-green-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The base on disk: [`FILES`] files of [`FILE_BYTES`] over [`DIRS`] directories, each file's bytes naming it.
fn seed(target: &TargetDir) {
  for d in 0..DIRS {
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir(format!("{}/d{d}", target.path)).unwrap();
  }
  for f in 0..FILES {
    target.seed(&file_path(f), &file_bytes(f));
  }
}

fn file_path(f: usize) -> String {
  format!("d{}/f{f}.c", f % DIRS)
}

fn file_bytes(f: usize) -> Vec<u8> {
  let mut bytes = format!("file {f}\n").into_bytes();
  bytes.resize(FILE_BYTES, b'x');
  bytes
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
        .create(&scratch(&format!("green-probe-{round}")))
        .unwrap();
      client.destroy(made).unwrap();
    }
  }
  at.elapsed()
}

/// Sends `CreateGreen` over `base` and waits for its reply, however long the walk takes (within [`CREATE_WAIT`]): the
/// reply and how long it took. A reply the client gives up on is the defect this file is about, so it is the reply.
fn create_and_wait(client: &mut Client, base: GreenBase) -> (ReplyBody, Duration) {
  let began = Instant::now();
  let word = client
    .begin(&RequestBody::CreateGreen {
      name: "big-green".to_owned(),
      require_evidence: false,
      base: Some(base),
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
      Err(e) => panic!("the green's reply: {e}"),
    }
    assert!(
      began.elapsed() < CREATE_WAIT,
      "the green answered within {CREATE_WAIT:?}"
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

/// §4.16 "Green creation", §4.3, R9. Do: on a one-shard daemon, overlay a base of 4,000 files of 16 KiB in 40
/// directories, pin it whole and snapshot it; take each probe's baseline latency; then create a green over the snapshot
/// on one thread while another client keeps probing the same shard, pausing its baseline median between probes: a
/// status read, and a volume created and destroyed. Expect: the green is created; the shard answered probes while the
/// walk ran, and none waited for half the walk or more; and the green's version 0 reads every file's bytes, so the
/// sliced walk took the whole tree.
#[test]
fn creating_a_green_over_a_large_base_never_holds_its_shard() {
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
      ..scratch("green-base")
    })
    .unwrap();
  client.pin(overlay, None).unwrap();
  let snapshot = client.snapshot(overlay).unwrap();
  let base = GreenBase {
    volume: overlay,
    snapshot,
  };
  let mut prober = connect(&instance);
  let status_of = prober.create(&scratch("green-status")).unwrap();
  let mut round = 0usize;
  let mut baseline = Vec::new();
  for _ in 0..BASELINE_PROBES {
    for kind in PROBES {
      baseline.push(probe(&mut prober, kind, round, status_of));
      round += 1;
    }
  }
  let creating = std::thread::spawn(move || {
    let began = Instant::now();
    let (reply, took) = create_and_wait(&mut client, base);
    (client, began, took, reply)
  });
  // The prober pauses its baseline median between probes, so the shard serves probes about half the time: a busy
  // shard, not one the prober alone fills.
  let pace = quantile(&mut baseline.clone(), 500_000);
  let mut during = Vec::new();
  while !creating.is_finished() {
    let kind = PROBES[round % PROBES.len()];
    let at = Instant::now();
    let took = probe(&mut prober, kind, round, status_of);
    during.push((at, took));
    round += 1;
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(pace);
  }
  let (mut client, began, took, reply) = creating.join().unwrap();
  let read_back = match &reply {
    ReplyBody::GreenCreated { id } => [0, FILES / 2, FILES - 1]
      .into_iter()
      .map(|f| {
        client
          .read(*id, &file_path(f), ReadAt::Version { version: 0 })
          .map(|bytes| bytes == file_bytes(f))
      })
      .collect::<Result<Vec<bool>, _>>(),
    _ => Ok(Vec::new()),
  };
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
    "green over {FILES} files of {FILE_BYTES} bytes: {took:?}, reply {reply:?}; {probes_inside} probes inside it — \
     p50 {:?}, p99 {:?}, longest of all {longest:?}; before it the longest {:?}",
    quantile(&mut inside.clone(), 500_000),
    quantile(&mut inside, 990_000),
    baseline.iter().max().copied().unwrap_or_default()
  );
  assert!(
    matches!(reply, ReplyBody::GreenCreated { .. }),
    "the green was created: {reply:?}"
  );
  assert_eq!(
    read_back.as_deref(),
    Ok(&[true, true, true][..]),
    "version 0 reads every sampled file's bytes"
  );
  assert!(
    probes_inside > 1,
    "the shard answered probes while the walk ran ({probes_inside} inside its {took:?})"
  );
  assert!(
    longest * 2 < took,
    "no probe waited for half the walk or more ({longest:?} of {took:?})"
  );
}
