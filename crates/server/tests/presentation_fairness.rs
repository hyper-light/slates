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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, Filter, SizeClass, VolumeId};
use slates_ipc::protocol::{Intent, ReplyBody, RequestBody};
use slates_rt::{RunState, registry};
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
      name: format!("slates-seg-present-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// One probe's latency, or the error the client answered it with (a probe the client gives up on is a hold this file
/// names, not a panic that hides which one it was).
fn probe(
  client: &mut Client,
  kind: Probe,
  round: usize,
  status_of: VolumeId,
) -> Result<Duration, slates_client::ClientError> {
  let at = Instant::now();
  match kind {
    Probe::Status => {
      client.status(status_of)?;
    }
    Probe::Provision => {
      let made = client.create(&scratch(&format!("present-probe-{round}")))?;
      client.destroy(made)?;
    }
  }
  Ok(at.elapsed())
}

/// What the shard showed at one instant (one shard: the daemon's only): its own CPU time, the steps it has run, its
/// longest step so far, its scheduler overrun, its online wake estimate (its step quantum: a cooperative slice's budget
/// is half of it, so under load a slice runs as long as a wake then costs), and the run-state samples taken of it so
/// far.
#[derive(Clone, Copy, Debug)]
struct ShardView {
  at: Instant,
  cpu_ns: u64,
  steps: u64,
  longest_step_ns: u64,
  overrun_ns: u64,
  wake_ns: u64,
  states: StateCounts,
}

fn view(daemon: &Daemon, sampler: &Sampler) -> ShardView {
  let pulse = daemon.shard_pulses().into_iter().next();
  ShardView {
    at: Instant::now(),
    cpu_ns: daemon.shard_cpu_ns().first().map_or(0, |(_, ns)| *ns),
    steps: pulse.map_or(0, |p| p.steps),
    longest_step_ns: pulse.map_or(0, |p| p.longest_step_ns),
    overrun_ns: pulse.map_or(0, |p| p.scheduler_overrun_ns),
    wake_ns: pulse.map_or(0, |p| p.wake_cost_ns),
    states: sampler.counts(),
  }
}

/// Shape: how often the sampler reads the shard's run state. A millisecond resolves a half-second wait into hundreds
/// of samples, at one `thread_info` call each; under load the sampler is itself delayed, which thins the samples but
/// does not bias which state they find.
const SAMPLE_EVERY: Duration = Duration::from_millis(1);

/// The shard's run-state samples so far: runnable (on a CPU or waiting for one), blocked inside a step (off the CPU
/// and not parked: waiting in a call), and parked (waiting for work, as an idle shard should).
#[derive(Clone, Copy, Debug, Default)]
struct StateCounts {
  runnable: u64,
  blocked: u64,
  parked: u64,
}

impl StateCounts {
  fn since(self, before: StateCounts) -> StateCounts {
    StateCounts {
      runnable: self.runnable.saturating_sub(before.runnable),
      blocked: self.blocked.saturating_sub(before.blocked),
      parked: self.parked.saturating_sub(before.parked),
    }
  }

  fn total(self) -> u64 {
    self
      .runnable
      .saturating_add(self.blocked)
      .saturating_add(self.parked)
  }
}

/// The sampler's counters, shared with its thread (leaked: one small value a test).
#[derive(Default)]
struct Samples {
  stop: AtomicBool,
  runnable: AtomicU64,
  blocked: AtomicU64,
  parked: AtomicU64,
}

/// A thread that reads the shard's run state every [`SAMPLE_EVERY`] for the test's length
/// (`slates_rt::registry::shard_run_state`, macOS; elsewhere it counts nothing), so a long probe is named by where
/// the shard's thread was during it, which its CPU clock alone cannot tell: preempted and blocked both stop the clock.
struct Sampler {
  samples: &'static Samples,
  thread: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
  fn start(daemon: &Daemon) -> Sampler {
    let samples: &'static Samples = Box::leak(Box::default());
    let shard = daemon.shard_pulses().first().map(|pulse| pulse.shard);
    let thread = shard.map(|shard| {
      std::thread::spawn(move || {
        while !samples.stop.load(Ordering::Acquire) {
          let parked = registry::with_entry(shard, |entry| entry.parking.parked());
          let state = registry::holder_of(shard).and_then(registry::shard_run_state);
          let counter = match (state, parked) {
            (Some(RunState::Runnable), _) => Some(&samples.runnable),
            (Some(RunState::Blocked), Some(true)) => Some(&samples.parked),
            (Some(RunState::Blocked), _) => Some(&samples.blocked),
            (None, _) => None,
          };
          if let Some(counter) = counter {
            counter.fetch_add(1, Ordering::Relaxed);
          }
          #[allow(clippy::disallowed_methods)]
          // D-9's test exception: the sampler is the test's own thread.
          std::thread::sleep(SAMPLE_EVERY);
        }
      })
    });
    Sampler { samples, thread }
  }

  fn counts(&self) -> StateCounts {
    StateCounts {
      runnable: self.samples.runnable.load(Ordering::Relaxed),
      blocked: self.samples.blocked.load(Ordering::Relaxed),
      parked: self.samples.parked.load(Ordering::Relaxed),
    }
  }
}

impl Drop for Sampler {
  fn drop(&mut self) {
    self.samples.stop.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

/// A probe that waited long or was given up on, with the shard's own record from just before it to just after.
struct Hold {
  what: String,
  before: ShardView,
  after: ShardView,
}

impl Hold {
  /// Which kind of wait it was, by the shard's own record, as `destroy_rows` and the SDK's `Ticker` name theirs:
  /// - (a) on the CPU: the shard ran its own CPU for at least half the wait (in one long step, or over many);
  /// - (a) blocked in a call: off the CPU for most of the wait, and most run-state samples found it blocked inside a
  ///   step (not parked);
  /// - (b) starved: off the CPU for most of the wait, and most samples found it runnable: ready, and the operating
  ///   system ran other threads;
  /// - off the CPU, unsampled: where no run state is read (Linux, Windows), the CPU clock cannot tell the two apart.
  ///
  /// Whether one step spans the wait (the shard's longest step rose to at least half of it) is printed beside: a
  /// starved shard's step is as long as its wait. The longest step is a running maximum, so a step inside the wait that
  /// is no longer than one before it shows as no rise.
  fn named(&self) -> String {
    let wall = self.after.at.saturating_duration_since(self.before.at);
    let wall_ns = u64::try_from(wall.as_nanos()).unwrap_or(u64::MAX);
    let cpu = Duration::from_nanos(self.after.cpu_ns.saturating_sub(self.before.cpu_ns));
    let steps = self.after.steps.saturating_sub(self.before.steps);
    let longest = Duration::from_nanos(self.after.longest_step_ns);
    let overrun =
      Duration::from_nanos(self.after.overrun_ns.saturating_sub(self.before.overrun_ns));
    let rose = self.after.longest_step_ns > self.before.longest_step_ns;
    let one_step = rose && self.after.longest_step_ns.saturating_mul(2) >= wall_ns;
    let states = self.after.states.since(self.before.states);
    let on_cpu = u64::try_from(cpu.as_nanos())
      .unwrap_or(u64::MAX)
      .saturating_mul(2)
      >= wall_ns;
    let verdict = if on_cpu {
      "(a) on the CPU: the shard ran its own work most of the wait"
    } else if states.total() == 0 {
      "off the CPU, unsampled: blocked or starved, which this platform's clocks cannot tell"
    } else if states.blocked.saturating_mul(2) >= states.total() {
      "(a) blocked in a call: off the CPU and not parked most of the wait"
    } else if states.runnable.saturating_mul(2) >= states.total() {
      "(b) starved: runnable and off the CPU most of the wait, the operating system running other threads"
    } else {
      "off the CPU, mixed: no state held half the samples"
    };
    format!(
      "{}: {verdict} — over {wall:?} the shard ran {cpu:?} of its own CPU in {steps} steps, its longest step {longest:?} \
       ({}), its scheduler overrun +{overrun:?}, its wake estimate {:?}; sampled {} runnable, {} blocked in a step, {} \
       parked",
      self.what,
      match (rose, one_step) {
        (true, true) => "risen inside the wait, spanning most of it",
        (true, false) => "risen inside the wait",
        _ => "as before the wait",
      },
      Duration::from_nanos(self.after.wake_ns),
      states.runnable,
      states.blocked,
      states.parked,
    )
  }
}

/// Shape: a probe that waits at least this share of the client's reply deadline is named with the shard's record, in
/// parts per thousand: half, so a probe that nearly stalled is explained beside one that did.
const NAMED_AT_PERMILLE: u64 = 500;

/// The wait past which a probe is named ([`NAMED_AT_PERMILLE`] of the client's reply deadline).
fn named_after() -> Duration {
  Duration::from_nanos(
    common::client::deadlines()
      .reply_ns
      .saturating_mul(NAMED_AT_PERMILLE)
      / 1_000,
  )
}

/// Runs one probe bracketed by the shard's record: its latency, kept in `during` with its start; a long or refused
/// probe also named in `holds`.
fn bracketed(
  (daemon, sampler): (&Daemon, &Sampler),
  prober: &mut Client,
  (kind, round, status_of): (Probe, usize, VolumeId),
  during: &mut Vec<(Instant, Duration)>,
  holds: &mut Vec<Hold>,
) {
  let before = view(daemon, sampler);
  let outcome = probe(prober, kind, round, status_of);
  let after = view(daemon, sampler);
  match outcome {
    Ok(took) => {
      during.push((before.at, took));
      if took >= named_after() {
        holds.push(Hold {
          what: format!("probe {round} ({kind:?}) took {took:?}"),
          before,
          after,
        });
      }
    }
    Err(error) => holds.push(Hold {
      what: format!("probe {round} ({kind:?}) was given up: {error:?}"),
      before,
      after,
    }),
  }
}

/// Prints every named probe and fails on one the client gave up on, naming each.
fn report_holds(holds: &[Hold]) {
  for hold in holds {
    eprintln!("named: {}", hold.named());
  }
  let refused: Vec<String> = holds
    .iter()
    .filter(|hold| hold.what.contains("given up"))
    .map(Hold::named)
    .collect();
  assert!(
    refused.is_empty(),
    "a probe the client gave up on:\n{}",
    refused.join("\n")
  );
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
      baseline.push(probe(&mut prober, kind, round, status_of).unwrap());
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
  let mut holds = Vec::new();
  let sampler = Sampler::start(&daemon);
  while !presenting.is_finished() {
    let kind = PROBES[round % PROBES.len()];
    bracketed(
      (&daemon, &sampler),
      &mut prober,
      (kind, round, status_of),
      &mut during,
      &mut holds,
    );
    round += 1;
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(pace);
  }
  let (client, began, took, reply) = presenting.join().unwrap();
  drop(sampler);
  drop(prober);
  drop(client);
  daemon.stop();
  report_holds(&holds);
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
      baseline.push(probe(&mut prober, kind, round, status_of).unwrap());
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
  let mut holds = Vec::new();
  let sampler = Sampler::start(&daemon);
  while !landing.is_finished() {
    let kind = PROBES[round % PROBES.len()];
    bracketed(
      (&daemon, &sampler),
      &mut prober,
      (kind, round, status_of),
      &mut during,
      &mut holds,
    );
    round += 1;
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(pace);
  }
  let (began, took, landed) = landing.join().unwrap();
  drop(sampler);
  drop(prober);
  daemon.stop();
  report_holds(&holds);
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
