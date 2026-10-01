//! A large granted landing does not monopolize its owner shard (AUD-29-25; §4.15, §4.3, R9): the landing runs
//! as an owned run stepped in slices of half the shard's step quantum, and the shard serves its other clients
//! between them. Before the sliced run, the landing was one synchronous call on the shard, and a verb that
//! arrived while it ran waited for all of it.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The daemon's NFS loopback transport and a real landing target: macOS and Linux.
#![cfg(unix)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_client::driver::{Begin, Driver, Event, Ticket};
use slates_client::{
  Client, ClientError, CreateSpec, Filter, Intent, Landing, SizeClass, VolumeId,
};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{approve, connect, deadlines, scratch};
use common::nfs::{create, mount, write};
use common::target::target_dir;

/// Shape: one shard, so the landing and the probing client share it — the shard is the subject.
const TEST_SHARDS: u16 = 1;
/// Shape: the volume's name.
const VOLUME: &str = "fair-landing";
/// Shape: the volume the lease probe takes and gives back its write lease on.
const LEASED: &str = "fair-leased";
/// Shape: files in the landing — each a temporary created, written, synced and linked on the disk, so the
/// landing's host work spans many slices on any disk.
const FILES: usize = 600;
/// Shape: the probes of each kind taken before the landing, for their service time on this machine.
const BASELINE_PROBES: usize = 32;

/// The kinds of work the shard serves beside the landing: a read, a provisioning (a volume created and
/// destroyed), and a write lease taken and given back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
  Read,
  Provision,
  Lease,
}

/// Format: the probe kinds, in the order the prober cycles through them.
const PROBES: [Probe; 3] = [Probe::Read, Probe::Provision, Probe::Lease];

/// One probe's latency: its round trips through the shard.
fn probe(
  client: &mut slates_client::Client,
  kind: Probe,
  round: usize,
  leased: slates_client::VolumeId,
) -> Duration {
  let began = Instant::now();
  match kind {
    Probe::Read => {
      client.list().unwrap();
    }
    Probe::Provision => {
      let id = client
        .create(&scratch(&format!("fair-probe-{round}")))
        .unwrap();
      client.destroy(id).unwrap();
    }
    Probe::Lease => {
      let attached = client.attach(leased, None, Intent::Write).unwrap();
      client.detach(attached.attachment).unwrap();
    }
  }
  began.elapsed()
}

/// The `ppm`-th quantile (parts per million) of `samples`, exactly: the `⌈n × ppm / 10⁶⌉`-th smallest.
fn quantile(samples: &mut [Duration], ppm: u64) -> Duration {
  /// Format: parts per million.
  const PPM: u64 = 1_000_000;
  samples.sort_unstable();
  let n = u64::try_from(samples.len()).unwrap();
  let rank = (n * ppm).div_ceil(PPM).max(1);
  samples[usize::try_from(rank).unwrap() - 1]
}

/// Writes `count` small files into the volume `volume` through the daemon's NFS transport.
fn write_files(daemon: &Daemon, volume: &str, count: usize) {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability(volume).unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  let mut xid = 2u32;
  for n in 0..count {
    let file = create(&mut stream, &root, &format!("f{n:04}"), xid);
    write(&mut stream, &file, format!("file {n}").as_bytes(), xid + 1);
    xid += 2;
  }
}

/// Prints each probe kind's quantiles inside the landing (`began`, lasting `took`) beside its baseline, asserts
/// every kind was answered more than once while the landing ran, and returns the longest probe inside it.
fn report_probes(
  during: &[(Probe, Instant, Duration)],
  baseline: &[(Probe, Duration)],
  began: Instant,
  took: Duration,
) -> Duration {
  let mut longest_probe = Duration::ZERO;
  for kind in PROBES {
    let mut inside: Vec<Duration> = during
      .iter()
      .filter(|(k, at, probe)| *k == kind && *at >= began && *at + *probe <= began + took)
      .map(|(_, _, probe)| *probe)
      .collect();
    let mut before: Vec<Duration> = baseline
      .iter()
      .filter(|(k, _)| *k == kind)
      .map(|(_, probe)| *probe)
      .collect();
    assert!(
      inside.len() > 1,
      "the shard answered {kind:?} probes while the landing ran: {}",
      inside.len()
    );
    let max = quantile(&mut inside, 1_000_000);
    eprintln!(
      "{kind:?}: {} probes inside the landing — p50 {:?}, p99 {:?}, p999 {:?}, max {max:?}; before it max {:?}",
      inside.len(),
      quantile(&mut inside, 500_000),
      quantile(&mut inside, 990_000),
      quantile(&mut inside, 999_000),
      quantile(&mut before, 1_000_000)
    );
    longest_probe = longest_probe.max(max);
  }
  longest_probe
}

/// AUD-29-25 acceptance and its percentile lane. Do: write 600 files through the daemon's NFS transport into
/// a one-shard volume, take each probe's baseline latency, then land the volume into a fresh directory under
/// a grant on one thread while another client keeps probing the same shard — reads, provisioning (create and
/// destroy) and a write lease taken and given back, in turn. Expect: the landing is done with every file
/// written and ran in many slices; every kind of probe was answered while it ran; no probe waited for the
/// whole landing; no slice ran past its budget by more than its own last unit (the daemon counts any that
/// did); and the run publishes, per probe kind and for the slices, p50/p99/p999/max, with the shard's
/// longest step — the recorded lane (`docs/wip/BENCHMARKS.md`, "The landing's percentile lane").
#[test]
fn a_large_landing_leaves_its_shard_serving_between_its_slices() {
  let profile = common::machine_profile();
  let instance = format!("srv-fairland-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-fairland-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = target_dir();
  // The product's own deadlines: a granted landing longer than the reply deadline is waited for while the
  // daemon lives (until 2026-10-01 the client answered `Stalled` after one second of it).
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let volume = client.create(&scratch(VOLUME)).unwrap();
  write_files(&daemon, VOLUME, FILES);

  let mut prober = connect(&instance);
  let leased = prober.create(&scratch(LEASED)).unwrap();
  let mut round = 0usize;
  let mut baseline = Vec::new();
  for _ in 0..BASELINE_PROBES {
    for kind in PROBES {
      baseline.push((kind, probe(&mut prober, kind, round, leased)));
      round += 1;
    }
  }
  let grant = approve(&mut client, &secret, volume, None, &target.path);
  let target_path = target.path.clone();
  let landing = std::thread::spawn(move || {
    let began = Instant::now();
    let landed = client.land(volume, None, &target_path, Filter::default(), Some(grant));
    (began, began.elapsed(), landed)
  });
  let mut during = Vec::new();
  while !landing.is_finished() {
    let kind = PROBES[round % PROBES.len()];
    let at = Instant::now();
    let took = probe(&mut prober, kind, round, leased);
    during.push((kind, at, took));
    round += 1;
  }
  let (began, took, landed) = landing.join().unwrap();
  let slices = daemon.landing_slices().unwrap();
  let longest_step = daemon
    .shard_pulses()
    .iter()
    .map(|pulse| pulse.longest_step_ns)
    .max()
    .unwrap_or(0);
  drop(prober);
  daemon.stop();

  let Ok(Landing::Landed(outcome)) = landed else {
    panic!("the granted landing: {landed:?}");
  };
  assert_eq!(outcome.state, "done");
  assert_eq!(outcome.written, u64::try_from(FILES).unwrap());
  eprintln!(
    "landing of {FILES} files: {took:?} in {} slices of a {} ns budget: p50 {} ns, p99 {} ns, p999 {} ns, \
     max {} ns; {} past budget; the shard's longest step {longest_step} ns",
    slices.slices,
    slices.budget_ns,
    slices.p50_ns,
    slices.p99_ns,
    slices.p999_ns,
    slices.max_ns,
    slices.past_budget
  );
  let longest_probe = report_probes(&during, &baseline, began, took);
  assert!(
    slices.slices > 1,
    "the landing ran in slices: {}",
    slices.slices
  );
  assert_eq!(
    slices.past_budget, 0,
    "every slice ended within its own last unit of its budget"
  );
  assert!(
    longest_probe < took,
    "no probe waited for the whole landing ({longest_probe:?} of {took:?})"
  );
  assert!(
    Duration::from_nanos(slices.max_ns) * 2 <= took,
    "no slice was more than half the landing ({} ns of {took:?})",
    slices.max_ns
  );
}

/// Shape: the files of the calibration landing that measures this machine's per-file landing cost — enough
/// that the mean is many units, not one unit's noise.
const CALIBRATION_FILES: usize = 500;
/// Shape: how many times the span it must outlast a sized landing is made to last, so a disk faster than the
/// calibration measured still outlasts it.
const OUTLAST_FACTOR: u32 = 2;
/// Shape: the most files a sized landing is given — sixteen calibrations; a disk so fast that more would be
/// needed skips the test loudly rather than writing without bound.
const MOST_FILES: usize = CALIBRATION_FILES * 16;
/// Shape: the most a sized landing's volume may grow to — a gibibyte, far above the most files' small
/// contents; it is dynamic, so it reserves nothing up front and takes only what its files use.
const SIZED_VOLUME_BYTES: u64 = 1 << 30;

/// A volume for a sized landing: grown as its files arrive, up to a bound far above what it will hold (a
/// bounded volume would reserve the whole bound at creation, more than a small machine's shard holds).
fn sized(name: &str) -> CreateSpec {
  CreateSpec {
    size: SizeClass::Dynamic {
      max: SIZED_VOLUME_BYTES,
    },
    ..scratch(name)
  }
}

/// Creates a sized volume, or reports a loud skip when the shard's budget cannot hold it (a typed
/// `BudgetExceeded`: a machine too small for the landing this disk's speed calls for).
fn create_sized(client: &mut Client, name: &str, files: usize) -> Option<VolumeId> {
  match client.create(&sized(name)) {
    Ok(volume) => Some(volume),
    Err(ClientError::Refused(slates_client::Refusal::BudgetExceeded { available })) => {
      eprintln!(
        "SKIP: the shard's budget ({available} bytes available) cannot hold a volume for {files} files"
      );
      None
    }
    Err(e) => panic!("creating {name}: {e:?}"),
  }
}

/// How many files a landing needs to last [`OUTLAST_FACTOR`] times `span` on this machine: a calibration
/// landing of [`CALIBRATION_FILES`] files (its own volume, grant and target) measures the per-file cost.
/// `None` when more than [`MOST_FILES`] would be needed — the disk outruns the test's bound.
fn files_to_outlast(
  daemon: &Daemon,
  client: &mut Client,
  secret: &[u8; 32],
  span: Duration,
) -> Option<usize> {
  /// Shape: the calibration volume's name.
  const CALIBRATION: &str = "calibration-landing";
  let volume = client.create(&sized(CALIBRATION)).unwrap();
  write_files(daemon, CALIBRATION, CALIBRATION_FILES);
  let target = target_dir();
  let grant = approve(client, secret, volume, None, &target.path);
  let began = Instant::now();
  let landed = client.land(volume, None, &target.path, Filter::default(), Some(grant));
  let took = began.elapsed();
  assert!(
    matches!(landed, Ok(Landing::Landed(_))),
    "the calibration landing: {landed:?}"
  );
  // Its charge goes back to the shard for the landing it sizes.
  client.destroy(volume).unwrap();
  let per_file = took / u32::try_from(CALIBRATION_FILES).unwrap();
  let wanted = span * OUTLAST_FACTOR;
  let files = usize::try_from(wanted.as_nanos().div_ceil(per_file.as_nanos().max(1))).unwrap();
  eprintln!(
    "calibration: {CALIBRATION_FILES} files landed in {took:?} ({per_file:?} each); {files} files to last {wanted:?}"
  );
  (files <= MOST_FILES).then_some(files.max(CALIBRATION_FILES))
}
/// Shape: the keepalive test's lease term (the daemon's failover bound): a second, which the landing outlasts,
/// with a half-term renewal margin of 500 ms — past the longest single unit measured here (a 205 ms `fsync`
/// stall, `docs/wip/BENCHMARKS.md`, 2026-10-01).
const SHORT_TERM_NS: u64 = 1_000_000_000;

/// AUD-29-25 (the lease keepalive). Do: on a daemon whose landing lease term is one second, land enough files
/// to last twice the term on this machine (a calibration landing measures the per-file cost) under a grant. Expect: the landing is done with every file written, having renewed
/// its lease between slices at least once; without the renewal its entries past the term were skipped
/// (`LeaseEnded`) and it ended partial.
#[test]
fn a_landing_longer_than_its_lease_term_renews_it_and_lands_everything() {
  let profile = common::machine_profile();
  let instance = format!("srv-renewland-{}", std::process::id());
  let config =
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)).with_failover_slo(SHORT_TERM_NS);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-renewland-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = target_dir();
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let Some(files) = files_to_outlast(
    &daemon,
    &mut client,
    &secret,
    Duration::from_nanos(SHORT_TERM_NS),
  ) else {
    daemon.stop();
    eprintln!(
      "SKIP: this disk lands faster than {MOST_FILES} files can outlast a {SHORT_TERM_NS} ns term"
    );
    return;
  };
  let Some(volume) = create_sized(&mut client, VOLUME, files) else {
    daemon.stop();
    return;
  };
  write_files(&daemon, VOLUME, files);
  let grant = approve(&mut client, &secret, volume, None, &target.path);
  let began = Instant::now();
  let landed = client.land(volume, None, &target.path, Filter::default(), Some(grant));
  let took = began.elapsed();
  let counters = daemon.fleet_refusals().unwrap();
  daemon.stop();
  let renewed = counters.get("landing.lease_renewed").copied().unwrap_or(0);
  let Ok(Landing::Landed(outcome)) = landed else {
    panic!("the granted landing: {landed:?}");
  };
  eprintln!(
    "landing of {files} files: {took:?} under a {SHORT_TERM_NS} ns term, {renewed} renewals: {} written, {}",
    outcome.written, outcome.state
  );
  assert!(
    took > std::time::Duration::from_nanos(SHORT_TERM_NS),
    "the landing outlasted its term ({took:?}), or the test proves nothing"
  );
  assert_eq!(outcome.state, "done");
  assert_eq!(outcome.written, u64::try_from(files).unwrap());
  assert!(renewed >= 1, "the lease was renewed between slices");
}

/// Shape: the volume an impatient call lands beside the patient one.
const IMPATIENT: &str = "impatient-landing";
/// Shape: how long the loop waits for both landings to end: a minute, far past two landings measured at
/// about 1.5 s each (`docs/wip/BENCHMARKS.md`, "The landing's percentile lane"), so a lost reply fails the test
/// rather than hanging it.
const BOTH_END_WITHIN: Duration = Duration::from_secs(60);

/// How each call ended and when, on the loop's clock.
type Ended = std::collections::BTreeMap<Ticket, (Duration, Result<Option<Landing>, ClientError>)>;

/// A granted landing's begin, for the driver to send (and resend after a reconnect).
fn land_begin(volume: VolumeId, target: String, grant: u64) -> Begin {
  Box::new(move |client: &mut Client| {
    client.land_begin(volume, None, &target, Filter::default(), Some(grant))
  })
}

/// Runs an event loop over `driver` — pump, tick when the driver asks, end each call by its event — until
/// every one of `tickets` has ended or [`BOTH_END_WITHIN`] passes, as an SDK's loop does.
fn drive(client: &mut Client, driver: &mut Driver, tickets: &[Ticket]) -> Ended {
  let started = Instant::now();
  let mut ended = Ended::new();
  let mut next_tick = Instant::now();
  while tickets.iter().any(|ticket| !ended.contains_key(ticket))
    && started.elapsed() < BOTH_END_WITHIN
  {
    let mut events = driver.pump(client);
    if Instant::now() >= next_tick {
      events.extend(driver.tick(client));
    }
    for event in events {
      match event {
        Event::Ready { ticket, word } => {
          let landed = client.land_poll(word);
          driver.finish(ticket);
          ended.insert(ticket, (started.elapsed(), landed));
        }
        Event::Failed { ticket, error } => {
          ended.insert(ticket, (started.elapsed(), Err(error)));
        }
      }
    }
    next_tick = Instant::now()
      + Duration::from_nanos(driver.next_wake_ns(client).unwrap_or(deadlines().reply_ns));
    std::hint::spin_loop();
  }
  ended
}

/// The async driver's patient path (the sibling of AUD-29-25 fixed 2026-10-01: a client answered `Stalled`
/// to a landing still at work). Do: land two volumes, each sized by a calibration landing to last twice the reply deadline, under grants through one async
/// driver, driven by an event loop as an SDK drives it — one call submitted patient (`submit_patient`, as both
/// SDKs' `land` is), the other plain — both landings outlasting the reply deadline. Expect: the plain call
/// ends `Stalled` at its deadline (the control: the landings did outlast it, so the patient flag is what this
/// tests), and the patient call is waited for past it and ends with its landing done and every file written.
#[test]
fn an_async_patient_landing_outlasting_the_reply_deadline_is_waited_for() {
  let profile = common::machine_profile();
  let instance = format!("srv-patientland-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-patientland-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let reply = Duration::from_nanos(deadlines().reply_ns);
  let Some(files) = files_to_outlast(&daemon, &mut client, &secret, reply) else {
    daemon.stop();
    eprintln!(
      "SKIP: this disk lands faster than {MOST_FILES} files can outlast the {reply:?} reply deadline"
    );
    return;
  };
  let (Some(patient_volume), Some(impatient_volume)) = (
    create_sized(&mut client, VOLUME, files),
    create_sized(&mut client, IMPATIENT, files),
  ) else {
    daemon.stop();
    return;
  };
  write_files(&daemon, VOLUME, files);
  write_files(&daemon, IMPATIENT, files);
  let (patient_target, impatient_target) = (target_dir(), target_dir());
  let patient_grant = approve(
    &mut client,
    &secret,
    patient_volume,
    None,
    &patient_target.path,
  );
  let impatient_grant = approve(
    &mut client,
    &secret,
    impatient_volume,
    None,
    &impatient_target.path,
  );

  let mut driver = Driver::new(&client);
  let patient = driver
    .submit_patient(
      &mut client,
      land_begin(patient_volume, patient_target.path.clone(), patient_grant),
    )
    .unwrap();
  let impatient = driver
    .submit(
      &mut client,
      land_begin(
        impatient_volume,
        impatient_target.path.clone(),
        impatient_grant,
      ),
    )
    .unwrap();
  let mut ended = drive(&mut client, &mut driver, &[patient, impatient]);
  daemon.stop();

  let (impatient_at, impatient_end) = ended.remove(&impatient).expect("the plain call ended");
  let (patient_at, patient_end) = ended.remove(&patient).expect("the patient call ended");
  eprintln!(
    "reply deadline {reply:?}; plain call ended at {impatient_at:?}: {impatient_end:?}; patient call at {patient_at:?}"
  );
  assert!(
    matches!(impatient_end, Err(ClientError::Stalled { .. })),
    "the control: a plain call to a landing outlasting the deadline stalls: {impatient_end:?}"
  );
  assert!(
    patient_at > reply,
    "the patient landing outlasted the reply deadline ({patient_at:?})"
  );
  let Ok(Some(Landing::Landed(outcome))) = patient_end else {
    panic!("the patient landing: {patient_end:?}");
  };
  assert_eq!(outcome.state, "done");
  assert_eq!(outcome.written, u64::try_from(files).unwrap());
}
