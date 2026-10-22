//! What a FUSE mount on a volume costs every write through another path (§4.6 "Cache posture"; A-142): each such
//! change runs an invalidation round on the mount's serve task, on the volume's owner shard, which writes its
//! notifications on the mount's notify thread. An in-process daemon serves one volume, and an SDK client times
//! sequential whole-file writes of one small file. Phases alternate between no mount and a mount made by the real
//! `slates mount`, which reads the file first, as a reader of it would. A bare thread start and join is timed alone for
//! comparison, since a thread per round was what A-142 removed.
//!
//! Rows are `write_round\t<phase>\t<mounted>\t<p50>\t<p90>\t<p99>\t<mean>\t<load>` in nanoseconds, and
//! `spawn_join\t<p50>\t<p90>\t<p99>\t<mean>`. Nothing here gates; the A/B in `docs/wip/BENCHMARKS.md` is two builds of
//! this example, before and after, run interleaved.
//!
//! The writes land in the volume, in RAM. The only host directory is the mount point, made by `mkdir` beside the
//! binary under the build output and removed at the end (R1; never `/tmp`). Linux only, since the rounds measured are
//! FUSE's; elsewhere it says so and exits.
//!
//! Run (Linux, `fusermount3` present): `cargo build --release -p slates-cli --bin slates --example write_round_ab &&
//! target/release/examples/write_round_ab`
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(target_os = "linux")]
fn main() {
  linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("SKIP write_round_ab: the rounds it measures are a Linux FUSE mount's");
}

#[cfg(target_os = "linux")]
mod linux {
  use std::process::Command;
  use std::time::{Duration, Instant};

  use slates_client::{Client, CreateSpec, Deadlines, Intent};
  use slates_db::replay::RECOVERY_BUDGET_NS;
  use slates_ipc::protocol::{NamePolicy, SizeClass};
  use slates_machine::{MachineProfile, ProfileOptions};
  use slates_server::daemon::LIVENESS_BUDGET_NS;
  use slates_server::{Daemon, DaemonConfig, SegmentSource};

  /// Shape: writes timed per phase — 2,000 samples put a p90 on 200 of them.
  const WRITES: usize = 2000;
  /// Shape: phases each way, alternating, so drift in the machine's load falls on both.
  const PHASES: usize = 4;
  /// Shape: thread starts timed alone.
  const SPAWNS: usize = 2000;
  /// Shape: the file's length — small, so a write is the round trip and the round, not the copy.
  const FILE_BYTES: usize = 64;
  /// Shape: the volume's quota — far above the one file.
  const VOLUME_BYTES: u64 = 64 << 20;
  /// Shape: the machine profile's budget per probe, as `vfs_tails` takes it.
  const PROBE_BUDGET: Duration = Duration::from_millis(5);
  /// Shape: how long the daemon may take to accept the first client.
  const CONNECT_WAIT: Duration = Duration::from_secs(30);
  /// Shape: the pause after an unmount, so its teardown is not in the next phase.
  const SETTLE: Duration = Duration::from_millis(200);

  /// The 50th, 90th and 99th percentiles and the mean of `samples`, nanoseconds, tab-separated.
  fn quantiles(mut samples: Vec<u64>) -> String {
    samples.sort_unstable();
    let at = |permille: usize| {
      let last = samples.len().saturating_sub(1);
      samples.get(last * permille / 1000).copied().unwrap_or(0)
    };
    let mean = samples.iter().sum::<u64>() / u64::try_from(samples.len().max(1)).unwrap();
    format!("{}\t{}\t{}\t{mean}", at(500), at(900), at(990))
  }

  /// The machine's load averages, as `/proc/loadavg` gives them.
  #[allow(clippy::disallowed_methods)] // a read of procfs, not a write
  fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
      .unwrap_or_default()
      .split_whitespace()
      .take(3)
      .collect::<Vec<_>>()
      .join(" ")
  }

  /// Times [`WRITES`] whole-file writes of `f.bin`, each a different byte.
  fn writes(client: &mut Client, target: (slates_client::VolumeId, u64), phase: usize) -> Vec<u64> {
    (0..WRITES)
      .map(|write| {
        let fill = u8::try_from((phase * WRITES + write) % 256).unwrap();
        let started = Instant::now();
        client
          .fs_write(target, "f.bin", &[fill; FILE_BYTES], 0o644)
          .unwrap();
        u64::try_from(started.elapsed().as_nanos()).unwrap()
      })
      .collect()
  }

  pub(super) fn run() {
    let profile = MachineProfile::measure_patiently(&ProfileOptions {
      budget_per_probe: PROBE_BUDGET,
      codecs: false,
      core_matrix: false,
    })
    .expect("the machine profile measures");
    let instance = format!("slates-wrab-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance, None);
    let daemon = Daemon::start(
      &profile,
      config,
      SegmentSource::Create {
        name: format!("slates-seg-wrab-{}", std::process::id()),
      },
    )
    .unwrap();
    daemon.bootstrap(true).unwrap();
    let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
    let started = Instant::now();
    let mut client = loop {
      match Client::connect(&instance, deadlines) {
        Ok(client) => break client,
        Err(e) => assert!(started.elapsed() < CONNECT_WAIT, "connect: {e}"),
      }
    };
    let volume = client
      .create(&CreateSpec {
        name: "wrab".to_owned(),
        size: SizeClass::Bounded {
          limit: VOLUME_BYTES,
        },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    let attachment = client
      .attach(volume, None, Intent::Write)
      .unwrap()
      .attachment;
    let target = (volume, attachment);
    client
      .fs_write(target, "f.bin", &[0; FILE_BYTES], 0o644)
      .unwrap();
    let exe = std::env::current_exe().unwrap();
    let examples = exe.parent().unwrap();
    let slates = examples.parent().unwrap().join("slates");
    let id: String = volume
      .bytes
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    let point = format!("{}/wrab-{}", examples.display(), std::process::id());
    assert!(
      Command::new("mkdir")
        .args(["-p", &point])
        .status()
        .unwrap()
        .success()
    );
    for phase in 0..PHASES {
      let unmounted = writes(&mut client, target, 2 * phase);
      println!(
        "write_round\t{phase}\tno\t{}\t{}",
        quantiles(unmounted),
        load()
      );
      let mounted = Command::new(&slates)
        .args(["mount", &id, &point, "--instance", &instance])
        .output()
        .unwrap();
      assert!(
        mounted.status.success(),
        "slates mount: {}",
        String::from_utf8_lossy(&mounted.stderr)
      );
      // The mount's kernel caches the file, as a reader of it would.
      #[allow(clippy::disallowed_methods)] // a read through the slates mount: RAM, not disk
      let _ = std::fs::read(format!("{point}/f.bin")).unwrap();
      let timed = writes(&mut client, target, 2 * phase + 1);
      println!(
        "write_round\t{phase}\tyes\t{}\t{}",
        quantiles(timed),
        load()
      );
      let _ = Command::new("fusermount3").args(["-u", &point]).output();
      // The bench paces itself between phases; shipped code parks on its driver (D-9).
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(SETTLE);
    }
    let spawns = (0..SPAWNS)
      .map(|_| {
        let started = Instant::now();
        std::thread::Builder::new()
          .name("slates-wrab-probe".to_owned())
          .spawn(|| {})
          .unwrap()
          .join()
          .unwrap();
        u64::try_from(started.elapsed().as_nanos()).unwrap()
      })
      .collect();
    println!("spawn_join\t{}", quantiles(spawns));
    let _ = Command::new("rmdir").arg(&point).output();
    drop(daemon);
  }
}
