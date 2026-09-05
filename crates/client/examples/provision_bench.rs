//! The provisioning histogram (R9, AC-2.1, T-2.6; §4.7 "the provisioning fast path"): a
//! volume created from the Rust client through the real rendezvous and rings against an
//! in-process daemon, its round trip sampled thousands of times, reported as p50, p99, p999
//! and max, in the spinning form (the client's spin window covers the reply, the shard is
//! polling) and the parked form (requests paced past the shard's park, so each pays the
//! doorbell, the shard's wake and the client's wake), with 1, 8 and 64 concurrent clients.
//! Rows are `ratchet\t<key>\t<lower>\t<median>\t<upper>` in nanoseconds over the runs; the
//! max rows are informational (a scheduler's worst case is not a code property). The
//! `ac-2.1` lines gate the spinning p99 against the floor.
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: runs per row.
const RUNS: usize = 3;
/// Shape: creates sampled per client per run in the spinning form.
const SPINNING_SAMPLES: usize = 2_000;
/// Shape: creates sampled per run in the parked form (each pays two wakes and a pause).
const PARKED_SAMPLES: usize = 500;
/// Shape: the pause between parked-form requests: long past the runtime's spin-before-park
/// window (microseconds), so the shard is parked when the request lands.
const PARKED_PAUSE: Duration = Duration::from_millis(1);
/// Shape: the concurrent client counts of T-2.6.
const CLIENT_COUNTS: &[usize] = &[1, 8, 64];
/// Shape: the initial floor of AC-2.1 (R9): fifty microseconds; the ratchet's ceiling
/// tightens below it.
const PROVISION_FLOOR_NS: u64 = 50_000;
/// Shape: the bounded volume created per sample.
const VOLUME_BYTES: u64 = 1 << 20;
/// Format: the percentiles reported, in parts per thousand.
const PERCENTILES: &[(&str, u64)] = &[("p50", 500), ("p99", 990), ("p999", 999)];
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;

fn ns(elapsed: Duration) -> u64 {
  u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

/// Shape: how long a client retries the rendezvous while the daemon comes up or its claim
/// slots clear under a burst of concurrent connects.
const CONNECT_WAIT: Duration = Duration::from_secs(10);

/// Connects, retrying the rendezvous while the daemon is coming up.
fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      // The daemon coming up, or every bootstrap claim slot contended under a burst of
      // concurrent connects: both clear as the control shard drains.
      Err(ClientError::Ipc(
        slates_ipc::IpcError::DaemonUnavailable { .. } | slates_ipc::IpcError::RingFull,
      )) if started.elapsed() < CONNECT_WAIT => {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

/// One client's samples: `count` creates (each followed by a destroy that is not timed).
fn sample(instance: &str, tag: &str, count: usize, pause: Option<Duration>) -> Vec<u64> {
  let mut client = connect(instance);
  // The spinning form spins for the floor (the reply lands without a wake when the daemon
  // meets it); the parked form keeps the daemon's published window and parks.
  client.spin_for(if pause.is_none() {
    Some(PROVISION_FLOOR_NS)
  } else {
    None
  });
  let mut samples = Vec::with_capacity(count);
  for n in 0..count {
    if let Some(pause) = pause {
      std::thread::park_timeout(pause);
    }
    let spec = CreateSpec {
      name: format!("{tag}-{n}"),
      size: SizeClass::Bounded {
        limit: VOLUME_BYTES,
      },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    };
    let started = Instant::now();
    let id = client.create(&spec).unwrap();
    samples.push(ns(started.elapsed()));
    client.destroy(id).unwrap();
  }
  samples
}

/// The percentiles of one run's samples.
fn percentiles(mut samples: Vec<u64>) -> Vec<(&'static str, u64)> {
  samples.sort_unstable();
  let last = samples.len().saturating_sub(1);
  let mut out: Vec<(&'static str, u64)> = PERCENTILES
    .iter()
    .map(|(name, permille)| {
      let index = usize::try_from(u64::try_from(last).unwrap_or(u64::MAX) * permille / PERMILLE)
        .unwrap_or(last);
      (*name, samples[index.min(last)])
    })
    .collect();
  out.push(("max", samples[last]));
  out
}

/// One form over the runs; per percentile, the per-run values.
fn form(
  instance: &str,
  key: &str,
  clients: usize,
  count: usize,
  pause: Option<Duration>,
) -> Vec<(&'static str, Vec<u64>)> {
  let mut per_run: Vec<Vec<(&'static str, u64)>> = Vec::new();
  for run in 0..RUNS {
    let handles: Vec<_> = (0..clients)
      .map(|c| {
        let instance = instance.to_owned();
        let tag = format!("{key}-{run}-{c}");
        std::thread::spawn(move || sample(&instance, &tag, count, pause))
      })
      .collect();
    let mut all = Vec::new();
    for handle in handles {
      all.extend(handle.join().unwrap());
    }
    per_run.push(percentiles(all));
  }
  let mut rows = Vec::new();
  for (index, (name, _)) in per_run[0].iter().enumerate() {
    let values: Vec<u64> = per_run.iter().map(|r| r[index].1).collect();
    rows.push((*name, values));
  }
  rows
}

/// One client's status round trips over the runs: the channel plus the completion record.
fn status_form(instance: &str) -> Vec<(&'static str, Vec<u64>)> {
  let mut client = connect(instance);
  client.spin_for(Some(PROVISION_FLOOR_NS));
  let id = client
    .create(&CreateSpec {
      name: "status-target".to_owned(),
      size: SizeClass::Bounded {
        limit: VOLUME_BYTES,
      },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let mut per_run = Vec::new();
  for _ in 0..RUNS {
    let mut samples = Vec::with_capacity(SPINNING_SAMPLES);
    for _ in 0..SPINNING_SAMPLES {
      let started = Instant::now();
      client.status(id).unwrap();
      samples.push(ns(started.elapsed()));
    }
    per_run.push(percentiles(samples));
  }
  client.destroy(id).unwrap();
  let mut rows = Vec::new();
  for (index, (name, _)) in per_run[0].iter().enumerate() {
    rows.push((*name, per_run.iter().map(|r| r[index].1).collect()));
  }
  rows
}

/// Prints the rows; the max row and every row of an oversubscribed run are informational.
fn print_rows_gated(key: &str, rows: &[(&'static str, Vec<u64>)], gated: bool) {
  for (name, values) in rows {
    let mut sorted = values.clone();
    sorted.sort_unstable();
    let (lower, median, upper) = (
      sorted[0],
      sorted[sorted.len() / 2],
      sorted[sorted.len() - 1],
    );
    let kind = if gated && *name != "max" {
      "ratchet"
    } else {
      "ratchet-info"
    };
    println!("{kind}\tprovision.{key}_{name}\t{lower}\t{median}\t{upper}");
    println!(
      "  provision.{key}_{name}: {median} ns [{lower}, {upper}] over {RUNS} runs: {values:?}"
    );
  }
}

fn p99_median(rows: &[(&'static str, Vec<u64>)]) -> u64 {
  let values = &rows.iter().find(|(n, _)| *n == "p99").unwrap().1;
  let mut sorted = values.clone();
  sorted.sort_unstable();
  sorted[sorted.len() / 2]
}

fn main() {
  let profile = MachineProfile::measure(ProfileOptions {
    budget_per_probe: ProfileOptions::default().budget_per_probe,
    codecs: false,
    core_matrix: false,
  });
  let instance = format!("provision-bench-{}", std::process::id());
  // The daemon as derived for this machine (its shard count and pinning), since AC-2.1 is
  // measured against the reference machine's own daemon.
  let config = DaemonConfig::derive(&profile, &instance);
  let shards = config.runtime.shards;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: "slates-seg-provision-bench".to_owned(),
    },
  )
  .unwrap();
  println!(
    "provisioning histogram: {shards} shards, spin window {} ns, floor {PROVISION_FLOOR_NS} ns",
    daemon.config().region.spin_ns
  );
  // A client thread spins for its reply while the shards spin serving; a run with more of
  // those than the machine has cores past the shards measures the scheduler oversubscribing,
  // not the provisioning path, so its row is informational and its p99 is not gated (AC-2.1
  // is measured on the reference machine, whose cores hold the concurrency).
  let cores = profile.facts.cores.len();
  let runnable = cores.saturating_sub(usize::from(shards)).max(1);
  let mut failed = false;
  for clients in CLIENT_COUNTS {
    let key = format!("spinning_{clients}");
    let rows = form(&instance, &key, *clients, SPINNING_SAMPLES, None);
    // The AC-2.1 floor is a latency claim: one client's provisioning p99, no cross-client
    // contention. The 1/8/64 sweep is the T-2.6 histogram; every runnable level's rows are
    // recorded so the ratchet catches regressions, but only the single-client p99 is held to
    // the 50 µs floor here (a concurrency level's tail is contention on shared owner shards,
    // reported and ratcheted, not floored).
    let ratchet_gated = *clients <= runnable;
    print_rows_gated(&key, &rows, ratchet_gated);
    let p99 = p99_median(&rows);
    let floor_gated = *clients == 1;
    if floor_gated {
      let ok = p99 <= PROVISION_FLOOR_NS;
      failed |= !ok;
      println!(
        "ac-2.1: {clients} client spinning ({cores} cores, {shards} shards), provisioning p99 {p99} ns vs floor {PROVISION_FLOOR_NS} ns: {}",
        if ok { "ok" } else { "FAIL" }
      );
    } else if ratchet_gated {
      println!(
        "ac-2.1: {clients} clients spinning, p99 {p99} ns (T-2.6 histogram; recorded and ratcheted, not floored)"
      );
    } else {
      println!(
        "ac-2.1: {clients} clients oversubscribes {runnable} runnable core(s); p99 {p99} ns reported, not gated here"
      );
    }
  }
  // The channel and the completion record alone: a status of one volume, one client, so the
  // create's own cost is the difference (informational).
  let rows = status_form(&instance);
  for (name, values) in &rows {
    let mut sorted = values.clone();
    sorted.sort_unstable();
    println!(
      "ratchet-info\tprovision.status_1_{name}\t{}\t{}\t{}",
      sorted[0],
      sorted[sorted.len() / 2],
      sorted[sorted.len() - 1]
    );
    println!("  provision.status_1_{name}: {values:?}");
  }
  // The parked form is reported separately (AC-2.1), not gated: its latency is a wake, which
  // the scheduler's park granularity dominates.
  let rows = form(&instance, "parked_1", 1, PARKED_SAMPLES, Some(PARKED_PAUSE));
  print_rows_gated("parked_1", &rows, false);
  println!(
    "ac-2.1: 1 client parked (paced {} µs), create p99 {} ns: reported, not gated",
    PARKED_PAUSE.as_micros(),
    p99_median(&rows)
  );
  daemon.stop();
  if failed {
    std::process::exit(1);
  }
}
