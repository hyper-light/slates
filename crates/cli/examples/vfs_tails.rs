//! The VFS tail lane: what an agent's file calls cost through a real kernel mount of a slates volume, on a
//! busy machine (§4.6, R4, R9; Ada 2026-10-04: "a busy machine is the norm", and the p99 tails under the worst
//! conditions are the target). An in-process daemon serves a volume; the real `slates mount` command (the
//! release binary beside this example's directory) mounts it, so the mount is production's (on macOS,
//! `mount(2)` with the root handle and the derived attribute cache, A-34); each operation is a direct syscall sequence on the mount
//! (no shell, so a sample is the call, not a fork), timed per call and reported as p50, p99, p999 and max.
//!
//! The sweep is the load the machine carries and the concurrency of the callers:
//! - load: 0, 1 and 2 background spinner threads per core, each sweeping its own buffer of twice the largest
//!   L2 the profile reports (so it spills its core's cache and contends for the shared cache and memory
//!   bandwidth as well as for cores);
//! - workers: 1 caller, and one caller per four cores, each in its own directory.
//!
//! The provisioning round trip (a volume created through the real client) is sampled under the same loads.
//! Rows are `tails\t<load per core>\t<workers>\t<op>\t<p50>\t<p99>\t<p999>\t<max>\t<samples>` in nanoseconds.
//! Nothing here gates: this lane finds the tails; a gate follows each fix (docs/wip/BENCHMARKS.md).
//!
//! The writes land in the slates volume behind the mount, which lives in RAM; the only host directory is the
//! mount point, made by `mkdir` beside the binary under `target/` and removed at the end, as `slates_mount.rs`
//! does (R1; never `/tmp`).
//!
//! Run (macOS/BSD): `cargo build --release -p slates-cli --bin slates --example vfs_tails &&
//! target/release/examples/vfs_tails`
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use slates_client::{Client, CreateSpec, Deadlines};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_ipc::protocol::{NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: background spinner threads per core in each sweep step: none, as many as cores, and twice that
/// (an oversubscribed machine).
const LOADS_PER_CORE: &[usize] = &[0, 1, 2];
/// Shape: cores per concurrent caller in the wide step (the narrow step is one caller).
const CORES_PER_WORKER: usize = 4;
/// Shape: samples of each operation per caller: enough that p999 is a measured rank (a thousand or more
/// samples per row at one caller).
const SAMPLES: usize = 2_000;
/// Shape: the small file an agent writes or reads (a source file).
const SMALL_BYTES: usize = 4 << 10;
/// Shape: the large file (a build artifact).
const LARGE_BYTES: usize = 1 << 20;
/// Shape: large-file samples per caller: each moves a mebibyte, so fewer.
const LARGE_SAMPLES: usize = 200;
/// Shape: the entries in the listed directory (a source tree's directory).
const LISTED_ENTRIES: usize = 256;
/// Derived: the volume's bound: a caller's peak is its listed directory and one large file (256 × 4 KiB +
/// 1 MiB, about 2 MiB); 64 MiB holds the widest sweep's callers several times over without claiming a shard's
/// budget the provisioning samples need.
const VOLUME_BYTES: u64 = 64 << 20;
/// Shape: the provisioning volume's bound.
const PROVISION_BYTES: u64 = 1 << 20;
/// Shape: the L2 assumed when the profile reports none (the largest per-core L2 on the supported targets).
const FALLBACK_L2_BYTES: u64 = 16 << 20;
/// Derived: a spinner's sweep is this many times the largest L2, so every pass spills its core's cache.
const SWEEP_PER_L2: u64 = 2;
/// Format: the percentiles reported, in parts per thousand.
const PERCENTILES: &[u64] = &[500, 990, 999];
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: how long to wait for the daemon's rendezvous to answer.
const CONNECT_WAIT: Duration = Duration::from_secs(10);

fn ns(elapsed: Duration) -> u64 {
  u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

/// The nearest-rank percentile `permille` of sorted `samples`.
fn rank(samples: &[u64], permille: u64) -> u64 {
  let count = u64::try_from(samples.len()).unwrap_or(u64::MAX);
  let position = count.saturating_mul(permille).div_ceil(PERMILLE).max(1);
  let index = usize::try_from(position.saturating_sub(1)).unwrap_or(usize::MAX);
  samples.get(index).copied().unwrap_or(0)
}

fn report(load: usize, workers: usize, op: &str, mut samples: Vec<u64>) {
  samples.sort_unstable();
  let ranks: Vec<String> = PERCENTILES
    .iter()
    .map(|permille| rank(&samples, *permille).to_string())
    .collect();
  println!(
    "tails\t{load}\t{workers}\t{op}\t{}\t{}\t{}",
    ranks.join("\t"),
    samples.last().copied().unwrap_or(0),
    samples.len()
  );
}

fn quick_profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(5),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures")
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
    match Client::connect(instance, deadlines) {
      Ok(client) => return client,
      Err(e) => {
        assert!(started.elapsed() < CONNECT_WAIT, "connect: {e}");
        std::hint::spin_loop();
      }
    }
  }
}

/// A live kernel mount, unmounted and its mount point removed on drop.
struct Mounted {
  path: String,
}

impl Drop for Mounted {
  fn drop(&mut self) {
    let _ = Command::new("umount").arg(&self.path).output();
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// Mounts `volume` with the real `slates mount` against the in-process daemon `instance`.
fn mount(instance: &str, volume: slates_client::VolumeId) -> Mounted {
  let exe = std::env::current_exe().unwrap();
  let examples = exe.parent().unwrap();
  let path = format!("{}/slates-tails-{}", examples.display(), std::process::id());
  let made = Command::new("mkdir").args(["-p", &path]).status().unwrap();
  assert!(made.success(), "mkdir -p {path}");
  let slates = examples.parent().unwrap().join("slates");
  let id: String = volume
    .bytes
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let mounted = Command::new(&slates)
    .args(["mount", &id, &path, "--instance", instance])
    .output()
    .unwrap();
  assert!(
    mounted.status.success(),
    "slates mount ({}): {}",
    slates.display(),
    String::from_utf8_lossy(&mounted.stderr)
  );
  Mounted { path }
}

/// Background load: `threads` spinners, each sweeping its own `sweep_bytes` buffer a line at a time until
/// `stop`, so the machine's cores, caches and memory bandwidth are all contended.
fn spin(stop: &AtomicBool, sweep_bytes: usize, line_bytes: usize) {
  let mut buffer = vec![0u8; sweep_bytes];
  let mut value = 0u8;
  while !stop.load(Ordering::Relaxed) {
    for line in buffer.chunks_mut(line_bytes) {
      if let Some(first) = line.first_mut() {
        *first = first.wrapping_add(value);
      }
    }
    value = value.wrapping_add(1);
  }
  std::hint::black_box(&buffer);
}

/// Times `call` once.
fn timed(call: impl FnOnce()) -> u64 {
  let started = Instant::now();
  call();
  ns(started.elapsed())
}

/// One caller's samples of each operation, in its own directory under the mount.
#[derive(Default)]
struct Samples {
  create: Vec<u64>,
  open_read: Vec<u64>,
  stat: Vec<u64>,
  list: Vec<u64>,
  rename: Vec<u64>,
  unlink: Vec<u64>,
  write_large: Vec<u64>,
}

impl Samples {
  fn absorb(&mut self, other: Samples) {
    self.create.extend(other.create);
    self.open_read.extend(other.open_read);
    self.stat.extend(other.stat);
    self.list.extend(other.list);
    self.rename.extend(other.rename);
    self.unlink.extend(other.unlink);
    self.write_large.extend(other.write_large);
  }
}

fn create_file(path: &str, bytes: &[u8]) {
  let fd = rustix::fs::open(
    path,
    OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC,
    Mode::from_raw_mode(0o644),
  )
  .unwrap();
  let mut written = 0;
  while written < bytes.len() {
    written += rustix::io::write(&fd, bytes.get(written..).unwrap()).unwrap();
  }
}

fn read_file(path: &str, buffer: &mut [u8]) {
  let fd = rustix::fs::open(path, OFlags::RDONLY, Mode::empty()).unwrap();
  let mut read = 0;
  while read < buffer.len() {
    let got = rustix::io::read(&fd, buffer.get_mut(read..).unwrap()).unwrap();
    if got == 0 {
      break;
    }
    read += got;
  }
}

fn list_dir(path: &str) -> usize {
  let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
  let dir = rustix::fs::Dir::read_from(&fd).unwrap();
  let mut entries = 0;
  for entry in dir {
    entry.unwrap();
    entries += 1;
  }
  entries
}

/// One caller's run: creates, reads, stats, lists, renames, unlinks and large writes, each sampled.
// The writes, renames and unlinks are through the slates mount, into the RAM volume behind it: never a host
// path's bytes (R1).
#[allow(clippy::disallowed_methods)]
fn caller(root: &str, index: usize) -> Samples {
  let dir = format!("{root}/w{index}");
  rustix::fs::mkdir(dir.as_str(), Mode::from_raw_mode(0o755)).unwrap();
  let listed = format!("{dir}/listed");
  rustix::fs::mkdir(listed.as_str(), Mode::from_raw_mode(0o755)).unwrap();
  let small = vec![7u8; SMALL_BYTES];
  let large = vec![9u8; LARGE_BYTES];
  let mut buffer = vec![0u8; SMALL_BYTES];
  for entry in 0..LISTED_ENTRIES {
    create_file(&format!("{listed}/e{entry}"), &small);
  }
  let mut samples = Samples::default();
  for sample in 0..SAMPLES {
    let file = format!("{dir}/f{sample}");
    let moved = format!("{dir}/m{sample}");
    samples.create.push(timed(|| create_file(&file, &small)));
    samples
      .open_read
      .push(timed(|| read_file(&file, &mut buffer)));
    samples.stat.push(timed(|| {
      rustix::fs::stat(file.as_str()).unwrap();
    }));
    samples.list.push(timed(|| {
      assert_eq!(list_dir(&listed), LISTED_ENTRIES + 2);
    }));
    samples.rename.push(timed(|| {
      rustix::fs::rename(file.as_str(), moved.as_str()).unwrap()
    }));
    samples
      .unlink
      .push(timed(|| rustix::fs::unlink(moved.as_str()).unwrap()));
  }
  for sample in 0..LARGE_SAMPLES {
    let file = format!("{dir}/large{sample}");
    samples
      .write_large
      .push(timed(|| create_file(&file, &large)));
    rustix::fs::unlink(file.as_str()).unwrap();
  }
  samples
}

fn provision(client: &mut Client, round: usize) -> Vec<u64> {
  (0..SAMPLES)
    .map(|sample| {
      let spec = CreateSpec {
        name: format!("p{round}-{sample}"),
        size: SizeClass::Bounded {
          limit: PROVISION_BYTES,
        },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      };
      let mut id = None;
      let elapsed = timed(|| id = Some(client.create(&spec).unwrap()));
      client.destroy(id.unwrap()).unwrap();
      elapsed
    })
    .collect()
}

/// `VFS_TAILS_FOCUS=<seconds>:<load per core>`: renames one file back and forth for that long under that load
/// and prints the rate, so a profiler can sample the process during one operation's loop.
// The renames are through the slates mount, into the RAM volume behind it (R1).
#[allow(clippy::disallowed_methods)]
fn focus_loop(
  daemon: &Daemon,
  root: &str,
  focus: &str,
  cores: usize,
  sweep_bytes: usize,
  line_bytes: usize,
) {
  let (seconds, per_core) = focus.split_once(':').unwrap();
  let span = Duration::from_secs(seconds.parse().unwrap());
  let per_core: usize = per_core.parse().unwrap();
  let (first, second) = (format!("{root}/focus-a"), format!("{root}/focus-b"));
  create_file(&first, &[1u8; SMALL_BYTES]);
  let stop = AtomicBool::new(false);
  std::thread::scope(|scope| {
    for _ in 0..per_core.saturating_mul(cores) {
      scope.spawn(|| spin(&stop, sweep_bytes, line_bytes));
    }
    println!(
      "# focus: pid {} renaming for {span:?} at {per_core} spinner(s) per core",
      std::process::id()
    );
    let started = Instant::now();
    let mut renames = 0u64;
    let mut samples = Vec::new();
    while started.elapsed() < span {
      let (from, to) = if renames.is_multiple_of(2) {
        (&first, &second)
      } else {
        (&second, &first)
      };
      samples.push(timed(|| {
        rustix::fs::rename(from.as_str(), to.as_str()).unwrap()
      }));
      renames += 1;
    }
    stop.store(true, Ordering::Relaxed);
    report(per_core, 1, "focus_rename", samples);
  });
  let (local, forwarded) = daemon.nfs_service_times().unwrap();
  for (kind, served) in [("served_local", local), ("served_forwarded", forwarded)] {
    println!(
      "tails\t{per_core}\t1\t{kind}\t{}\t{}\t{}\t{}\t{}",
      served.p50_ns, served.p99_ns, served.p999_ns, served.max_ns, served.count
    );
  }
}

fn main() {
  let cores = std::thread::available_parallelism().map_or(1, usize::from);
  let profile = quick_profile();
  let largest_l2 = profile
    .facts
    .cores
    .iter()
    .map(|core| core.l2_bytes)
    .max()
    .filter(|bytes| *bytes > 0)
    .unwrap_or(FALLBACK_L2_BYTES);
  let sweep_bytes = usize::try_from(largest_l2.saturating_mul(SWEEP_PER_L2)).unwrap();
  let line_bytes = usize::try_from(profile.facts.cache_line).unwrap().max(1);
  let instance = format!("slates-tails-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, None);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-tails-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: "tails".to_owned(),
      size: SizeClass::Bounded {
        limit: VOLUME_BYTES,
      },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let mounted = mount(&instance, volume);
  // Release builds abort on a panic, so `Mounted`'s drop never runs: the hook unmounts first.
  let mount_path = mounted.path.clone();
  let previous = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |info| {
    let _ = Command::new("umount").arg(&mount_path).output();
    let _ = Command::new("rmdir").arg(&mount_path).output();
    previous(info);
  }));
  println!(
    "# {cores} cores, spinner sweep {sweep_bytes} B, {} shard(s); {}",
    daemon.shards().len(),
    std::env::consts::OS
  );
  if let Ok(focus) = std::env::var("VFS_TAILS_FOCUS") {
    focus_loop(
      &daemon,
      &mounted.path,
      &focus,
      cores,
      sweep_bytes,
      line_bytes,
    );
    return;
  }
  let wide = (cores / CORES_PER_WORKER).max(1);
  let mut round = 0usize;
  for &per_core in LOADS_PER_CORE {
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
      for _ in 0..per_core.saturating_mul(cores) {
        scope.spawn(|| spin(&stop, sweep_bytes, line_bytes));
      }
      report(per_core, 1, "provision", provision(&mut client, round));
      for workers in [1, wide] {
        round += 1;
        let root = format!("{}/r{round}", mounted.path);
        #[allow(clippy::disallowed_methods)]
        rustix::fs::mkdir(root.as_str(), Mode::from_raw_mode(0o755)).unwrap();
        let mut all = Samples::default();
        std::thread::scope(|callers| {
          let handles: Vec<_> = (0..workers)
            .map(|index| {
              let root = root.as_str();
              callers.spawn(move || caller(root, index))
            })
            .collect();
          for handle in handles {
            all.absorb(handle.join().unwrap());
          }
        });
        report(per_core, workers, "create_4k", all.create);
        report(per_core, workers, "open_read_4k", all.open_read);
        report(per_core, workers, "stat", all.stat);
        report(per_core, workers, "list_256", all.list);
        report(per_core, workers, "rename", all.rename);
        report(per_core, workers, "unlink", all.unlink);
        report(per_core, workers, "write_1m", all.write_large);
      }
      stop.store(true, Ordering::Relaxed);
    });
  }
  drop(mounted);
  drop(client);
  drop(daemon);
}
