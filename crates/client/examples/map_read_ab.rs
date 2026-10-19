//! What the confined-descriptor rule costs the local zero-copy read (§4.6, A-135).
//!
//! Since 2026-10-18 the daemon refuses to hand a peer a descriptor that authorizes more than the read
//! (`Control::send_mapping` → `confined_for_a_peer`; the escape it closes is in
//! `docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md`). No backing satisfies the rule yet,
//! so a `MapRead` takes its copy path. This prices that: the same reads, through the same client API, against a
//! real one-shard daemon — one binary with the rule in force, one with it lifted — so the backing decision has a
//! number rather than an intuition.
//!
//! Command: `cargo run --release -p slates-client --example map_read_ab -- <file_kib> <samples>`
//! (defaults 1024 and 200). Prints the path actually taken, so an arm that silently fell back is visible, and
//! `/proc/loadavg` where it exists, since the figures are load-sensitive.
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the file read per sample, in kibibytes, unless the first argument says otherwise.
const DEFAULT_FILE_KIB: usize = 1024;
/// Shape: reads sampled, unless the second argument says otherwise.
const DEFAULT_SAMPLES: usize = 200;
/// Shape: the machine profile's budget per probe.
const PROBE_MS: u64 = 5;
/// Shape: how long a client retries the rendezvous while the daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);

fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT => {}
      Err(e) => panic!("{e}"),
    }
  }
}

fn percentile(sorted: &[u128], per_thousand: u64) -> u128 {
  if sorted.is_empty() {
    return 0;
  }
  let last = sorted.len() - 1;
  let at = (last as u128 * u128::from(per_thousand)) / 1000;
  sorted[usize::try_from(at).unwrap_or(last).min(last)]
}

fn main() {
  let mut args = std::env::args().skip(1);
  let file_kib = args
    .next()
    .and_then(|a| a.parse::<usize>().ok())
    .unwrap_or(DEFAULT_FILE_KIB);
  let samples = args
    .next()
    .and_then(|a| a.parse::<usize>().ok())
    .unwrap_or(DEFAULT_SAMPLES);

  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .unwrap();
  let instance = format!("mapab-{}", std::process::id());
  // One shard, so the volume's owner is the client's home shard: the cross-shard hand-off is a separate question.
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-mapab-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let mut client = connect(&instance);

  let bytes: Vec<u8> = (0..(file_kib << 10))
    .map(|i| u8::try_from(i % 251).unwrap_or(0))
    .collect();
  let id = client
    .create(&CreateSpec {
      name: "mapab".to_owned(),
      size: SizeClass::Bounded {
        limit: u64::try_from(bytes.len().saturating_mul(8)).unwrap_or(1 << 30),
      },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let attached = client.attach(id, None, Intent::Write).unwrap();
  client
    .fs_write((id, attached.attachment), "/big.bin", &bytes, 0o644)
    .unwrap();
  client.detach(attached.attachment).unwrap();

  // One read outside the sample, so the first-touch cost is not in the figures. What it returned is reported
  // rather than asserted: the two paths are being compared, and a path that returns something else is the most
  // interesting thing this can find — not a reason to abort before printing it.
  let warm = client.map_read(id, "/big.bin").unwrap();
  let zero_copy = warm.is_zero_copy();
  let got = warm.bytes().len();
  let equal = warm.bytes() == bytes.as_slice();
  println!(
    "warm read: path={} returned_bytes={got} file_bytes={} equal={equal}",
    if zero_copy { "zero-copy" } else { "copy" },
    bytes.len()
  );
  drop(warm);

  let mut taken: Vec<u128> = Vec::with_capacity(samples);
  let mut checksum = 0u64;
  for _ in 0..samples {
    let started = Instant::now();
    let mapped = client.map_read(id, "/big.bin").unwrap();
    let first = mapped.bytes().first().copied().unwrap_or(0);
    taken.push(started.elapsed().as_nanos());
    checksum = checksum.wrapping_add(u64::from(first));
    drop(mapped);
  }
  taken.sort_unstable();

  #[allow(clippy::disallowed_methods)] // reading the kernel's load average, where it exists
  let load = std::fs::read_to_string("/proc/loadavg")
    .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
    .unwrap_or_else(|_| "unknown".to_owned());
  let p50 = percentile(&taken, 500);
  println!(
    "map_read path={} file_kib={file_kib} samples={samples} load=\"{load}\"",
    if zero_copy { "zero-copy" } else { "copy" }
  );
  // Latency only, deliberately. A MiB/s computed from the *requested* size is a lie on any path that returns
  // less than it, and this bench exists precisely because one path did: see the `warm read:` line above for what
  // each arm actually handed back.
  println!(
    "  p50={} us  p99={} us  max={} us  checksum={checksum}",
    p50 / 1_000,
    percentile(&taken, 990) / 1_000,
    taken.last().copied().unwrap_or(0) / 1_000,
  );

  client.destroy(id).unwrap();
}
