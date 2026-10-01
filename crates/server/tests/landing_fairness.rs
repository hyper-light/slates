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

use slates_client::{Filter, Intent, Landing};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{approve, connect, scratch};
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

/// Writes `count` small files into the volume [`VOLUME`] through the daemon's NFS transport.
fn write_files(daemon: &Daemon, count: usize) {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability(VOLUME).unwrap().unwrap();
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
  write_files(&daemon, FILES);

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

/// Shape: the files in the keepalive test's landing — enough that it outlasts its lease's term on any disk.
const FILES_PAST_TERM: usize = 1_500;
/// Shape: the keepalive test's lease term (the daemon's failover bound): a second, which the landing outlasts,
/// with a half-term renewal margin of 500 ms — past the longest single unit measured here (a 205 ms `fsync`
/// stall, `docs/wip/BENCHMARKS.md`, 2026-10-01).
const SHORT_TERM_NS: u64 = 1_000_000_000;

/// AUD-29-25 (the lease keepalive). Do: on a daemon whose landing lease term is one second, land 1,500 files —
/// longer than the term — under a grant. Expect: the landing is done with every file written, having renewed
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
  let volume = client.create(&scratch(VOLUME)).unwrap();
  write_files(&daemon, FILES_PAST_TERM);
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
    "landing of {FILES_PAST_TERM} files: {took:?} under a {SHORT_TERM_NS} ns term, {renewed} renewals: {} written, {}",
    outcome.written, outcome.state
  );
  assert!(
    took > std::time::Duration::from_nanos(SHORT_TERM_NS),
    "the landing outlasted its term ({took:?}), or the test proves nothing"
  );
  assert_eq!(outcome.state, "done");
  assert_eq!(outcome.written, u64::try_from(FILES_PAST_TERM).unwrap());
  assert!(renewed >= 1, "the lease was renewed between slices");
}
