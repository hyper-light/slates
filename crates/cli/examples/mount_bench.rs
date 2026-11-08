//! The mount benchmark (T-4.8, per-operation latency against each platform's own file system; §4.6; step 2 of the VFS
//! and DAX plan, 2026-11-01): one workload, timed call by call, over every arm it is given, quiet and then with every
//! core loaded, so a slates mount and the file systems beside it are measured like for like on each platform.
//!
//! An arm is a directory: a slates mount (FUSE on Linux, NFS on macOS, a WinFsp drive on Windows) or a native file
//! system (tmpfs and overlayfs on Linux, APFS on macOS, NTFS on Windows). The scripts in `docs/wip/bench/fsbench/`
//! make the arms and run this program over them. Each arm's work goes in `<arm>/d`, made if missing and kept if it was
//! there (an overlay's lower directory, a volume's base), so a base's entries stand beside the files the run makes.
//!
//! The workload, for each round and each arm, over [`FILES_PER_ROUND`] files of [`FILE_BYTES`] each, every call timed
//! on its own:
//! - create (an exclusive create), write (the file's bytes in one call), fsync, close;
//! - stat;
//! - open, then read (the bytes back, compared, so a fast wrong answer is a failed run, not a result);
//! - rename (within the directory);
//! - unlink.
//!
//! Each phase runs over all of a round's files before the next, so the directory holds them from the creates to the
//! unlinks, as a tree being written does. Rounds interleave the arms and rotate which goes first, so a load that
//! drifts during a run lands on every arm alike (the A/B rule: interleaved, fresh state, load recorded). One untimed
//! round per arm runs first, so first-use costs are in no pass's tail.
//!
//! Two passes:
//! - **Quiet:** nothing added.
//! - **Contended:** [`SPINNERS_PER_CPU`] spinner threads per logical CPU (`--spinners-per-cpu` asks for more), each
//!   sweeping a buffer of twice the largest L2 the machine profile reports, one cache line at a time, as `vfs_tails.rs`
//!   loads a machine; cores, caches and memory bandwidth are all contended.
//!
//! Each pass records the load it ran under: the load average where the OS keeps one, the spinners, and a wake-up
//! probe. The probe is two threads trading a message every [`PROBE_EVERY`] for the whole pass. Its round trip is the
//! scheduling delay that an out-of-kernel file system's server pays per request, measured the same way on every
//! platform.
//!
//! Why these measures: the 2026-10-19 condition-12 runs (`docs/wip/BENCHMARKS.md`) found every FUSE operation within
//! one round trip of its floor, and a build beside a run raised slates' p50 4× while the in-kernel file systems barely
//! moved. A busy machine is the normal case (Ada, 2026-10-04), so the contended pass is primary evidence, and every row
//! reports p50, p99, p999 and max.
//!
//! Output: `# ` lines for the host and each pass; rows
//! `fsbench\t<pass>\t<arm>\t<op>\t<samples>\t<p50>\t<p99>\t<p999>\t<max>` in nanoseconds; then a Markdown table per
//! pass, an operation per row and an arm per column, each cell p50 · p99 · p999 · max in microseconds.
//!
//! The program writes only inside the arms it is given. A slates arm's writes land in its RAM volume. A disk-backed
//! arm (APFS or NTFS on a runner's disk) is the comparison's own subject, made by the script on a CI runner.
//!
//! Run: `cargo build --release -p slates-cli --bin slates --example mount_bench`, then
//! `target/release/examples/mount_bench --arm tmpfs=/mnt/fsb-tmpfs --arm slates=/mnt/fsb-slates`.
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use slates_machine::{MachineProfile, ProfileOptions};

/// Shape: files per round: the directory's population between a round's creates and its unlinks, half the 2,000 a
/// 2026-10-19 run created per arm, so two rounds cover that run.
const FILES_PER_ROUND: usize = 1_000;
/// Shape: rounds per pass. Times [`FILES_PER_ROUND`], 10,000 samples of each call per arm, so ten samples lie beyond
/// the p999 and it is not the max under another name.
const ROUNDS: usize = 10;
/// Shape: a file's bytes: a small source file (`vfs_tails.rs`' `SMALL_BYTES`).
const FILE_BYTES: usize = 4 << 10;
/// Format: the directory under each arm the work goes in.
const WORK_DIR: &str = "d";
/// Shape: the pause between two wake-up probes, about 200 a second: a few thousand per pass, too few to load anything.
const PROBE_EVERY: Duration = Duration::from_millis(5);
/// Shape: spinner threads per logical CPU in the contended pass, unless `--spinners-per-cpu` names another count: one
/// fills every core, so a thread that must be woken waits its turn behind a CPU-bound one.
const SPINNERS_PER_CPU: usize = 1;
/// Shape: the L2 assumed when the profile reports none (the largest per-core L2 on the supported targets).
const FALLBACK_L2_BYTES: u64 = 16 << 20;
/// Derived: a spinner's sweep is this many times the largest L2, so every pass of it spills its core's cache.
const SWEEP_PER_L2: u64 = 2;
/// Format: the percentiles reported, in parts per thousand.
const PERCENTILES: &[u64] = &[500, 990, 999];
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Format: nanoseconds per microsecond, and tenths of a microsecond per nanosecond's hundred.
const NS_PER_US: u64 = 1000;
/// Format: see [`NS_PER_US`].
const NS_PER_TENTH_US: u64 = 100;
/// Format: the operations, in the order a round makes them.
const OPERATIONS: [&str; 9] = [
  "create", "write", "fsync", "close", "stat", "open", "read", "rename", "unlink",
];

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

/// `nanoseconds` as microseconds to a tenth.
fn micros(nanoseconds: u64) -> String {
  format!(
    "{}.{}",
    nanoseconds / NS_PER_US,
    (nanoseconds % NS_PER_US) / NS_PER_TENTH_US
  )
}

/// p50, p99, p999 and max of `samples`, sorted in place.
fn summary(samples: &mut [u64]) -> [u64; 4] {
  samples.sort_unstable();
  let mut ranks = [0u64; 4];
  for (slot, permille) in ranks.iter_mut().zip(PERCENTILES) {
    *slot = rank(samples, *permille);
  }
  if let Some(last) = ranks.last_mut() {
    *last = samples.last().copied().unwrap_or(0);
  }
  ranks
}

/// One arm: its name in the tables and its directory.
struct Arm {
  name: String,
  root: PathBuf,
}

impl Arm {
  fn work(&self) -> PathBuf {
    self.root.join(WORK_DIR)
  }
}

/// One arm's samples of each operation, in [`OPERATIONS`] order.
#[derive(Default)]
struct Samples {
  by_operation: [Vec<u64>; 9],
}

impl Samples {
  fn push(&mut self, operation: usize, started: Instant, ended: Instant) {
    if let Some(samples) = self.by_operation.get_mut(operation) {
      samples.push(ns(ended.saturating_duration_since(started)));
    }
  }
}

/// A file's bytes: its round and index in the first sixteen, then a pattern of both, so a read of another file's bytes
/// is caught.
fn content(round: usize, index: usize) -> Vec<u8> {
  let mut bytes = vec![0u8; FILE_BYTES];
  let tag = [round.to_le_bytes(), index.to_le_bytes()].concat();
  for (position, byte) in bytes.iter_mut().enumerate() {
    let pattern = position.wrapping_mul(31) ^ index ^ round;
    let low = u8::try_from(pattern & usize::from(u8::MAX)).unwrap_or(0);
    *byte = tag.get(position).copied().unwrap_or(low);
  }
  bytes
}

/// One round in `arm`: every phase over `files` files, each call timed into `samples`.
// The arms are the comparison's directories: a slates mount, whose writes land in its RAM volume, or the native file
// system the script made for the comparison (R1 governs slates' own code, not this stand-in for a user's program).
#[allow(clippy::disallowed_methods)]
fn round(
  arm: &Arm,
  pass: &str,
  round: usize,
  files: usize,
  samples: &mut Samples,
) -> Result<(), String> {
  let work = arm.work();
  let made = |index: usize| work.join(format!("{pass}-r{round}-f{index}"));
  let moved = |index: usize| work.join(format!("{pass}-r{round}-m{index}"));
  let fail = |what: &str, path: &Path, error: std::io::Error| {
    format!("{}: {what} {}: {error}", arm.name, path.display())
  };
  for index in 0..files {
    let path = made(index);
    let bytes = content(round, index);
    let started = Instant::now();
    let mut file = OpenOptions::new()
      .write(true)
      .create_new(true)
      .open(&path)
      .map_err(|e| fail("create", &path, e))?;
    let created = Instant::now();
    file
      .write_all(&bytes)
      .map_err(|e| fail("write", &path, e))?;
    let written = Instant::now();
    file.sync_all().map_err(|e| fail("fsync", &path, e))?;
    let synced = Instant::now();
    drop(file);
    let closed = Instant::now();
    samples.push(0, started, created);
    samples.push(1, created, written);
    samples.push(2, written, synced);
    samples.push(3, synced, closed);
  }
  for index in 0..files {
    let path = made(index);
    let started = Instant::now();
    let metadata = std::fs::metadata(&path).map_err(|e| fail("stat", &path, e))?;
    samples.push(4, started, Instant::now());
    if usize::try_from(metadata.len()).ok() != Some(FILE_BYTES) {
      return Err(format!(
        "{}: {} has {} bytes, not {FILE_BYTES}",
        arm.name,
        path.display(),
        metadata.len()
      ));
    }
  }
  let mut buffer = vec![0u8; FILE_BYTES];
  for index in 0..files {
    let path = made(index);
    let started = Instant::now();
    let mut file = std::fs::File::open(&path).map_err(|e| fail("open", &path, e))?;
    let opened = Instant::now();
    file
      .read_exact(&mut buffer)
      .map_err(|e| fail("read", &path, e))?;
    let read = Instant::now();
    drop(file);
    samples.push(5, started, opened);
    samples.push(6, opened, read);
    if buffer != content(round, index) {
      return Err(format!(
        "{}: {} read back other bytes than were written",
        arm.name,
        path.display()
      ));
    }
  }
  for index in 0..files {
    let (from, to) = (made(index), moved(index));
    let started = Instant::now();
    std::fs::rename(&from, &to).map_err(|e| fail("rename", &from, e))?;
    samples.push(7, started, Instant::now());
  }
  for index in 0..files {
    let path = moved(index);
    let started = Instant::now();
    std::fs::remove_file(&path).map_err(|e| fail("unlink", &path, e))?;
    samples.push(8, started, Instant::now());
  }
  Ok(())
}

/// Background load until `stop`: sweeps its own `sweep_bytes` buffer a `line_bytes` line at a time, so the core, the
/// caches and memory bandwidth are all contended.
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

/// The wake-up probe until `stop`: the round trip of a message to a thread blocked on a channel and back, one every
/// [`PROBE_EVERY`].
fn probe(stop: &AtomicBool) -> Vec<u64> {
  let (to_echo, echo_inbox) = mpsc::channel::<()>();
  let (to_probe, probe_inbox) = mpsc::channel::<()>();
  std::thread::scope(|scope| {
    scope.spawn(move || {
      while echo_inbox.recv().is_ok() {
        if to_probe.send(()).is_err() {
          break;
        }
      }
    });
    let mut round_trips = Vec::new();
    while !stop.load(Ordering::Relaxed) {
      let sent = Instant::now();
      if to_echo.send(()).is_err() || probe_inbox.recv().is_err() {
        break;
      }
      round_trips.push(ns(sent.elapsed()));
      // The pause between probes, on a harness thread: the probe times the message's round trip, not this sleep, so a
      // timer that comes back late (macOS under a launchd job's policy) costs probes, not accuracy.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(PROBE_EVERY);
    }
    drop(to_echo);
    round_trips
  })
}

/// The OS's load average, where it keeps one.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // a read of procfs, not a write
fn load_average() -> String {
  std::fs::read_to_string("/proc/loadavg").map_or_else(
    |e| format!("unread ({e})"),
    |text| {
      text
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
    },
  )
}

/// The OS's load average, where it keeps one.
#[cfg(target_os = "macos")]
fn load_average() -> String {
  std::process::Command::new("sysctl")
    .args(["-n", "vm.loadavg"])
    .output()
    .map_or_else(
      |e| format!("unread ({e})"),
      |out| {
        String::from_utf8_lossy(&out.stdout)
          .trim()
          .trim_start_matches('{')
          .trim_end_matches('}')
          .trim()
          .to_owned()
      },
    )
}

/// The OS's load average, where it keeps one.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn load_average() -> String {
  "none (this OS keeps no load average)".to_owned()
}

/// What one pass measured, and the load it ran under.
struct Pass {
  name: &'static str,
  spinners: usize,
  load_before: String,
  load_after: String,
  probe: Vec<u64>,
  wall: Duration,
  samples: Vec<Samples>,
}

/// One pass: every round over every arm, with `spinners` spinner threads running for its length.
fn pass(
  name: &'static str,
  spinners: usize,
  sweep: (usize, usize),
  arms: &[Arm],
  rounds: usize,
  files: usize,
) -> Result<Pass, String> {
  let stop = AtomicBool::new(false);
  let load_before = load_average();
  let started = Instant::now();
  let (result, probe_samples, samples) = std::thread::scope(|scope| {
    for _ in 0..spinners {
      scope.spawn(|| spin(&stop, sweep.0, sweep.1));
    }
    let prober = scope.spawn(|| probe(&stop));
    let mut samples: Vec<Samples> = arms.iter().map(|_| Samples::default()).collect();
    let result = (|| {
      for number in 0..rounds {
        for offset in 0..arms.len() {
          let index = number.wrapping_add(offset) % arms.len();
          let (Some(arm), Some(into)) = (arms.get(index), samples.get_mut(index)) else {
            continue;
          };
          // Outside every timed call: the lane's stall detector reads this line's arrival as the run's sign of life, and
          // names the round a stalled run was in by it (`docs/wip/bench/fsbench/windows-ab.sh`).
          eprintln!(
            "progress: {name} round {} of {rounds}, arm {}, {:.1} s into the pass",
            number.saturating_add(1),
            arm.name,
            started.elapsed().as_secs_f64()
          );
          round(arm, name, number, files, into)?;
        }
      }
      Ok::<(), String>(())
    })();
    stop.store(true, Ordering::Relaxed);
    let probe_samples = prober.join().unwrap_or_default();
    (result, probe_samples, samples)
  });
  result?;
  Ok(Pass {
    name,
    spinners,
    load_before,
    load_after: load_average(),
    probe: probe_samples,
    wall: started.elapsed(),
    samples,
  })
}

/// Prints a pass: its load, its rows, and its table.
fn print_pass(arms: &[Arm], mut measured: Pass) {
  let probe = summary(&mut measured.probe);
  let load = format!(
    "spinners {} (none when quiet; a count per logical CPU when contended); load average {} at the start, {} at the \
     end; wake-up probe \
     p50 {} µs, p99 {} µs, p999 {} µs, max {} µs over {} probes; {:.1} s",
    measured.spinners,
    measured.load_before,
    measured.load_after,
    micros(probe[0]),
    micros(probe[1]),
    micros(probe[2]),
    micros(probe[3]),
    measured.probe.len(),
    measured.wall.as_secs_f64()
  );
  println!("# pass {}: {load}", measured.name);
  let mut cells: Vec<Vec<String>> = OPERATIONS.iter().map(|_| Vec::new()).collect();
  for (arm, samples) in arms.iter().zip(measured.samples.iter_mut()) {
    for ((operation, column), samples) in OPERATIONS
      .iter()
      .zip(cells.iter_mut())
      .zip(samples.by_operation.iter_mut())
    {
      let ranks = summary(samples);
      println!(
        "fsbench\t{}\t{}\t{operation}\t{}\t{}\t{}\t{}\t{}",
        measured.name,
        arm.name,
        samples.len(),
        ranks[0],
        ranks[1],
        ranks[2],
        ranks[3]
      );
      column.push(
        ranks
          .iter()
          .map(|value| micros(*value))
          .collect::<Vec<_>>()
          .join(" · "),
      );
    }
  }
  println!();
  println!(
    "**{}** ({load}). Each cell: p50 · p99 · p999 · max, µs.",
    measured.name
  );
  println!();
  let names: Vec<&str> = arms.iter().map(|arm| arm.name.as_str()).collect();
  println!("| op | {} |", names.join(" | "));
  println!("|---|{}", "---|".repeat(arms.len()));
  for (operation, row) in OPERATIONS.iter().zip(cells) {
    println!("| {operation} | {} |", row.join(" | "));
  }
  println!();
}

/// The arms and the counts the command line names.
struct Options {
  arms: Vec<Arm>,
  rounds: usize,
  files: usize,
  spinners_per_cpu: usize,
  passes: Vec<&'static str>,
}

fn options() -> Result<Options, String> {
  let mut parsed = Options {
    arms: Vec::new(),
    rounds: ROUNDS,
    files: FILES_PER_ROUND,
    spinners_per_cpu: SPINNERS_PER_CPU,
    passes: vec!["quiet", "contended"],
  };
  let mut arguments = std::env::args().skip(1);
  while let Some(argument) = arguments.next() {
    let mut value = || {
      arguments
        .next()
        .ok_or_else(|| format!("{argument} needs a value"))
    };
    match argument.as_str() {
      "--arm" => {
        let given = value()?;
        let (name, root) = given
          .split_once('=')
          .ok_or_else(|| format!("--arm {given}: expected NAME=DIR"))?;
        parsed.arms.push(Arm {
          name: name.to_owned(),
          root: PathBuf::from(root),
        });
      }
      "--rounds" => parsed.rounds = value()?.parse().map_err(|e| format!("--rounds: {e}"))?,
      "--files" => parsed.files = value()?.parse().map_err(|e| format!("--files: {e}"))?,
      "--spinners-per-cpu" => {
        parsed.spinners_per_cpu = value()?
          .parse()
          .map_err(|e| format!("--spinners-per-cpu: {e}"))?;
      }
      "--pass" => {
        parsed.passes = match value()?.as_str() {
          "quiet" => vec!["quiet"],
          "contended" => vec!["contended"],
          "both" => vec!["quiet", "contended"],
          other => return Err(format!("--pass {other}: quiet, contended or both")),
        }
      }
      other => return Err(format!("unknown argument {other}")),
    }
  }
  if parsed.arms.is_empty() {
    return Err("no arms: give --arm NAME=DIR at least once".to_owned());
  }
  Ok(parsed)
}

/// Makes each arm's work directory where it is missing; returns the ones made here, to remove at the end.
// The work directories are inside the arms the script made (see `round`).
#[allow(clippy::disallowed_methods)]
fn make_work_directories(arms: &[Arm]) -> Result<Vec<PathBuf>, String> {
  let mut made = Vec::new();
  for arm in arms {
    let work = arm.work();
    println!("# arm {}: {}", arm.name, work.display());
    if !work.is_dir() {
      std::fs::create_dir(&work)
        .map_err(|error| format!("{}: make {}: {error}", arm.name, work.display()))?;
      made.push(work);
    }
  }
  Ok(made)
}

/// Removes the work directories made here.
#[allow(clippy::disallowed_methods)]
fn remove_work_directories(made: Vec<PathBuf>) {
  for work in made {
    if let Err(error) = std::fs::remove_dir(&work) {
      eprintln!("mount_bench: remove {}: {error}", work.display());
    }
  }
}

/// Runs and prints each pass after one untimed round per arm, so first-use costs (a daemon's first page faults, cold
/// caches) land in no pass's tail; the first failure ends the run.
fn run_passes(options: &Options, cores: usize, sweep: (usize, usize)) -> Result<(), String> {
  for arm in &options.arms {
    round(arm, "warm", 0, options.files, &mut Samples::default())?;
  }
  println!(
    "# warm-up: one untimed round per arm ({} files each)",
    options.files
  );
  for name in &options.passes {
    let spinners = if *name == "contended" {
      cores.saturating_mul(options.spinners_per_cpu)
    } else {
      0
    };
    let measured = pass(
      name,
      spinners,
      sweep,
      &options.arms,
      options.rounds,
      options.files,
    )?;
    print_pass(&options.arms, measured);
  }
  Ok(())
}

fn main() {
  let options = match options() {
    Ok(options) => options,
    Err(error) => {
      eprintln!("mount_bench: {error}");
      std::process::exit(2);
    }
  };
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(5),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let cores = std::thread::available_parallelism().map_or(1, usize::from);
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
  let identity = &profile.facts.identity;
  println!(
    "# host: {} on {} ({}), {} logical CPUs ({cores} available to this process), {} GiB, page {} B; largest L2 \
     {largest_l2} B, cache line {line_bytes} B",
    identity.os,
    identity.cpu,
    identity.arch,
    identity.cores,
    identity.memory >> 30,
    identity.page
  );
  println!(
    "# workload: {} rounds of {} files of {FILE_BYTES} B per arm and pass, arms interleaved, first arm rotating",
    options.rounds, options.files
  );
  let made = match make_work_directories(&options.arms) {
    Ok(made) => made,
    Err(error) => {
      eprintln!("mount_bench: {error}");
      std::process::exit(1);
    }
  };
  let outcome = run_passes(&options, cores, (sweep_bytes, line_bytes));
  remove_work_directories(made);
  if let Err(error) = outcome {
    eprintln!("mount_bench: {error}");
    std::process::exit(1);
  }
}
