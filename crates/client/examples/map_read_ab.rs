//! Copy-once-map-many against copy-per-read (§4.6, A-171, A-173, A-175; G-direct step 3), through the client API against a real
//! one-shard daemon.
//!
//! A direct map pays once: the request, the daemon's fill of a copy of the file, the hand-off and the `mmap` (its
//! setup), then the page faults of the first read through the map. After that a read is loads from memory the daemon
//! keeps equal to the file. The copy read verb pays on every read: a request per page and a copy of every byte. This
//! measures, for each file size, the setup, the first mapped read, a steady mapped read and a copy read, interleaved
//! sample by sample on one daemon so the machine's load falls on every arm alike, and from the best of each the reads
//! past which the map's one-off cost is repaid:
//! `(setup + first mapped read − steady mapped read) / (copy read − steady mapped read)`.
//!
//! Command: `cargo run --release -p slates-client --example map_read_ab -- <sizes_kib> <samples>` (defaults
//! `4,1024,65536` and 15). Prints every sample of every arm, the best of each, the break-even read count, the path
//! the map took and the load (none on Windows, which reports no load average). A map that read by copy is reported as
//! such and never priced as a direct one.
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

#[cfg(any(unix, windows))]
use std::time::{Duration, Instant};

#[cfg(any(unix, windows))]
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, ReadAt, SizeClass, VolumeId,
};
#[cfg(any(unix, windows))]
use slates_machine::{MachineProfile, ProfileOptions};
#[cfg(any(unix, windows))]
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the file sizes read, in kibibytes, unless the first argument lists others: a page, a chunk's run of a
/// mebibyte, and a file of 64 MiB, where a read's cost is its bytes.
#[cfg(any(unix, windows))]
const DEFAULT_SIZES_KIB: &[usize] = &[4, 1024, 65536];
/// Shape: the samples a size takes of every arm, unless the second argument says otherwise.
#[cfg(any(unix, windows))]
const DEFAULT_SAMPLES: usize = 15;
/// Shape: the machine profile's budget per probe.
#[cfg(any(unix, windows))]
const PROBE_MS: u64 = 5;
/// Shape: how long a client retries the rendezvous while the daemon starts.
#[cfg(any(unix, windows))]
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the volume a file lives in holds this many times its bytes, room for its records and its rewrites.
#[cfg(any(unix, windows))]
const ROOM_FACTOR: u64 = 8;
/// Shape: the least room a volume is given, for a small file's records.
#[cfg(any(unix, windows))]
const LEAST_ROOM: u64 = 1 << 20;

#[cfg(any(unix, windows))]
fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

#[cfg(any(unix, windows))]
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

/// The machine's load average, where the OS reports one: `/proc/loadavg` on Linux, `vm.loadavg` on macOS.
#[cfg(unix)]
fn load() -> String {
  #[allow(clippy::disallowed_methods)] // reading the kernel's load average, where it exists
  if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
    return text
      .split_whitespace()
      .take(3)
      .collect::<Vec<_>>()
      .join(" ");
  }
  std::process::Command::new("sysctl")
    .args(["-n", "vm.loadavg"])
    .output()
    .ok()
    .and_then(|out| String::from_utf8(out.stdout).ok())
    .map(|text| text.trim().trim_matches(['{', '}']).trim().to_owned())
    .unwrap_or_else(|| "unknown".to_owned())
}

/// The paired arm: Windows reports no load average, so the run's record carries the processor's load measured beside
/// it instead.
#[cfg(windows)]
fn load() -> String {
  "not reported by Windows".to_owned()
}

/// One arm's samples, in nanoseconds, in the order taken.
#[cfg(any(unix, windows))]
#[derive(Default)]
struct Arm(Vec<u128>);

#[cfg(any(unix, windows))]
impl Arm {
  fn take<T>(&mut self, work: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let done = work();
    self.0.push(started.elapsed().as_nanos());
    done
  }

  fn best(&self) -> u128 {
    self.0.iter().copied().min().unwrap_or(0)
  }

  fn all(&self) -> String {
    self
      .0
      .iter()
      .map(|ns| format!("{:.1}", *ns as f64 / 1000.0))
      .collect::<Vec<_>>()
      .join(" ")
  }
}

/// A volume holding `/f.bin` of `kib` KiB of a pattern.
#[cfg(any(unix, windows))]
fn volume_with_file(client: &mut Client, kib: usize) -> (VolumeId, Vec<u8>) {
  let bytes: Vec<u8> = (0..(kib << 10))
    .map(|index| u8::try_from(index % 251).unwrap() | 1)
    .collect();
  let room = u64::try_from(bytes.len())
    .unwrap()
    .saturating_mul(ROOM_FACTOR)
    .max(LEAST_ROOM);
  let id = client
    .create(&CreateSpec {
      name: format!("mapab-{kib}"),
      size: SizeClass::Bounded { limit: room },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let attached = client.attach(id, None, Intent::Write).unwrap();
  client
    .fs_write((id, attached.attachment), "/f.bin", &bytes, 0o644)
    .unwrap();
  client.detach(attached.attachment).unwrap();
  (id, bytes)
}

/// Every arm of one size: the copy read, the map's setup, its first read and a steady read.
#[cfg(any(unix, windows))]
#[derive(Default)]
struct Arms {
  copy: Arm,
  setup: Arm,
  first: Arm,
  steady: Arm,
}

/// Samples every arm `samples` times over `/f.bin` of `id`, interleaved; whether every map was a direct one.
#[cfg(any(unix, windows))]
fn sample(client: &mut Client, id: VolumeId, bytes: &[u8], samples: usize) -> (Arms, bool) {
  let mut arms = Arms::default();
  let mut direct = true;
  let mut checksum = 0u64;
  // One of each outside the samples, so a first touch is not in the figures.
  assert_eq!(
    client.read(id, "/f.bin", ReadAt::Head).unwrap().len(),
    bytes.len()
  );
  let warm = client.map_read(id, "/f.bin").unwrap();
  client.release_map(warm).unwrap();
  for _ in 0..samples {
    let copied = arms
      .copy
      .take(|| client.read(id, "/f.bin", ReadAt::Head).unwrap());
    assert!(copied == bytes, "the copy read returned the file");
    let mapped = arms.setup.take(|| client.map_read(id, "/f.bin").unwrap());
    direct &= mapped.is_zero_copy();
    let mut buffer = vec![0u8; usize::try_from(mapped.len()).unwrap()];
    let read = arms.first.take(|| mapped.read_at(0, &mut buffer).unwrap());
    assert!(
      read == bytes.len() && buffer == bytes,
      "the first mapped read returned the file"
    );
    arms.steady.take(|| mapped.read_at(0, &mut buffer).unwrap());
    checksum = checksum.wrapping_add(u64::from(buffer[buffer.len() / 2]));
    client.release_map(mapped).unwrap();
  }
  println!("  checksum {checksum}");
  (arms, direct)
}

/// Reports one size's arms and the reads past which the map is repaid.
#[cfg(any(unix, windows))]
fn report(kib: usize, arms: &Arms, direct: bool) {
  println!(
    "{kib} KiB, map path={}",
    if direct {
      "direct"
    } else {
      "copy (not priced)"
    }
  );
  for (name, arm) in [
    ("copy read", &arms.copy),
    ("map setup", &arms.setup),
    ("first mapped read", &arms.first),
    ("steady mapped read", &arms.steady),
  ] {
    println!(
      "  {name:<19} best {:>10.1} us  all: {}",
      arm.best() as f64 / 1000.0,
      arm.all()
    );
  }
  if !direct {
    return;
  }
  let (copy, setup, first, steady) = (
    arms.copy.best(),
    arms.setup.best(),
    arms.first.best(),
    arms.steady.best(),
  );
  println!(
    "  steady mapped read / copy read = {:.4}",
    steady as f64 / copy.max(1) as f64
  );
  match copy.checked_sub(steady).filter(|saved| *saved > 0) {
    Some(saved) => {
      let once = setup + first.saturating_sub(steady);
      println!(
        "  break-even after {} reads (one-off {:.1} us, saved {:.1} us a read)",
        once.div_ceil(saved),
        once as f64 / 1000.0,
        saved as f64 / 1000.0
      );
    }
    None => println!("  never repaid: a mapped read costs no less than a copy read"),
  }
}

/// The paired arm: `Client::map_read` exists on Unix and Windows only, so there is nothing to price elsewhere; it skips
/// loudly.
#[cfg(not(any(unix, windows)))]
fn main() {
  eprintln!(
    "skip: the copy-once-map-many bench needs `Client::map_read`, built for Unix and Windows"
  );
}

#[cfg(any(unix, windows))]
fn main() {
  let mut args = std::env::args().skip(1);
  let sizes: Vec<usize> = args
    .next()
    .map(|list| list.split(',').filter_map(|kib| kib.parse().ok()).collect())
    .unwrap_or_else(|| DEFAULT_SIZES_KIB.to_vec());
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
  // One shard, so the volume's owner is the client's home shard, the only one a descriptor is handed from.
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
  println!("samples={samples} load before=\"{}\"", load());
  for kib in sizes {
    let (id, bytes) = volume_with_file(&mut client, kib);
    let (arms, direct) = sample(&mut client, id, &bytes, samples);
    report(kib, &arms, direct);
    client.destroy(id).unwrap();
  }
  println!("load after=\"{}\"", load());
}
