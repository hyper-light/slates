//! What a volume's journal is asked to hold against what it holds (§4.5 "Journal"; GAPS, "the journal's budget is a
//! fixed share"). The design sizes the journal as the mutation rate times the longest subscriber lag times the record
//! size; the daemon holds a fixed 10‰ of the quota instead. Every mount round records what waited for it
//! (`OpLog::note_round`), and by Little's law that backlog is the formula's product, measured. This bench runs real
//! workloads against FUSE mounts and reads the volume's journal report after each: the largest backlog any round found
//! (records, bytes, and how long its oldest record waited), the longest wait, the mean record charge, the records
//! appended and dropped, and the overruns, beside the budget the volume holds.
//!
//! A mount's own changes are behind its cursor, so its rounds find only another path's changes waiting. Each workload
//! therefore changes the volume through one path while a second mount of it watches:
//! - `git_clone`: `git clone --depth 1` of this checkout into a writer mount (3,791 files, 113 MiB checked out at
//!   2026-10-23);
//! - `cargo_build`: `cargo build --offline` of `slates-wire-derive` (three dependencies and a build script), its target
//!   directory in a writer mount;
//! - `sdk_ring`: the SDK client sending whole-file replacements a ring's worth at a time without waiting, the pattern
//!   of A-141's reproduction, over files the watcher has read.
//!
//! Rows are `journal\t<workload>\t<run>\t<seconds>\t<appended>\t<dropped>\t<rounds>\t<overruns>\t<largest_records>\t`
//! `<largest_bytes>\t<largest_lag_ns>\t<longest_lag_ns>\t<mean_record_bytes>\t<budget_bytes>\t<load>`. Runs interleave
//! the workloads, so drift in the machine's load falls on all three, and every run is printed, never a best one.
//! Nothing here gates; `docs/wip/BENCHMARKS.md` records the runs.
//!
//! The workloads write into the volumes, in RAM. The host directories are the mount points, made beside the binary
//! under the build output and removed at the end (R1; never `/tmp`). Linux only, since the rounds measured are FUSE's;
//! elsewhere it says so and exits.
//!
//! Run from the checkout (Linux, `fusermount3` and `git` present, the cargo registry populated):
//! `cargo build --release -p slates-cli --bin slates --example journal_bench && target/release/examples/journal_bench`
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(target_os = "linux")]
fn main() {
  linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("SKIP journal_bench: the rounds it measures are a Linux FUSE mount's");
}

#[cfg(target_os = "linux")]
mod linux {
  use std::path::{Path, PathBuf};
  use std::process::Command;
  use std::time::{Duration, Instant};

  use slates_client::{
    Client, ClientError, CreateSpec, Deadlines, Intent, JournalReport, RequestId, VolumeId,
  };
  use slates_db::replay::RECOVERY_BUDGET_NS;
  use slates_ipc::protocol::{NamePolicy, ReplyBody, RequestBody, SizeClass};
  use slates_machine::{MachineProfile, ProfileOptions};
  use slates_server::daemon::LIVENESS_BUDGET_NS;
  use slates_server::{Daemon, DaemonConfig, SegmentSource};

  /// Shape: runs of each workload, interleaved, every one printed.
  const RUNS: usize = 3;
  /// Shape: every volume's quota — room for the clone (113 MiB checked out, its pack beside it) and the build's target
  /// directory, with headroom. The fixed journal share is a per mille of it, so one quota holds the budget equal across
  /// the workloads.
  const VOLUME_BYTES: u64 = 1 << 30;
  /// Shape: the whole-file replacements the SDK workload sends: enough rings' worth that the watcher runs many rounds,
  /// and at two records or more each, more records than the fixed share of [`VOLUME_BYTES`] keeps.
  const RING_WRITES: usize = 64_000;
  /// Shape: the files the SDK workload replaces in turn, each read through the watcher first, as a reader would.
  const RING_FILES: usize = 64;
  /// Shape: each replacement's length — small enough that a whole replacement fits one ring slot (A-141's reproduction).
  const RING_FILE_BYTES: usize = 64;
  /// Format: a regular file's permission bits, owner read-write and everyone else read.
  const FILE_MODE: u32 = 0o644;
  /// Shape: the crate the build workload compiles, relative to the checkout.
  const BUILD_MANIFEST: &str = "crates/wire-derive/Cargo.toml";
  /// Shape: the machine profile's budget per probe, as `vfs_tails` takes it.
  const PROBE_BUDGET: Duration = Duration::from_millis(5);
  /// Shape: how long the daemon may take to accept the first client, a reply to come, an unmount to stop being busy, or
  /// the watcher's rounds to settle.
  const WAIT_FOR: Duration = Duration::from_secs(120);
  /// Shape: the pause between polls of the journal report while the watcher's last round lands.
  const POLL: Duration = Duration::from_millis(50);
  /// Shape: the polls in a row that find no new round or record before the report counts as settled.
  const SETTLED_POLLS: usize = 4;

  /// A workload that changes a volume through one path while a second mount of it watches.
  #[derive(Clone, Copy)]
  enum Workload {
    /// `git clone --depth 1` of the checkout through a writer mount.
    GitClone,
    /// A cargo build whose target directory is in a writer mount.
    CargoBuild,
    /// The SDK client's pipelined whole-file replacements.
    SdkRing,
  }

  impl Workload {
    /// The workload's name in the rows.
    fn name(self) -> &'static str {
      match self {
        Workload::GitClone => "git_clone",
        Workload::CargoBuild => "cargo_build",
        Workload::SdkRing => "sdk_ring",
      }
    }
  }

  /// The daemon's client, the binary that mounts, and where the mount points and the checkout are.
  struct Bench {
    client: Client,
    instance: String,
    slates: PathBuf,
    examples: PathBuf,
    checkout: PathBuf,
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

  /// Waits `duration`: the bench paces itself between polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  fn pause(duration: Duration) {
    std::thread::sleep(duration);
  }

  /// Runs `command` to its end, panicking with its output when it fails.
  fn finish(command: &mut Command, what: &str) {
    let output = command.output().unwrap();
    assert!(
      output.status.success(),
      "{what}: {}\n{}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }

  /// The checkout the bench runs from: the git top level of the working directory.
  fn checkout() -> PathBuf {
    let output = Command::new("git")
      .args(["rev-parse", "--show-toplevel"])
      .output()
      .unwrap();
    assert!(output.status.success(), "run the bench from the checkout");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
  }

  /// One row: the workload, the run, how long the workload took, the journal report and the machine's load.
  fn row(workload: Workload, run: usize, elapsed: Duration, journal: &JournalReport) -> String {
    format!(
      "journal\t{}\t{run}\t{:.3}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
      workload.name(),
      elapsed.as_secs_f64(),
      journal.appended,
      journal.dropped,
      journal.rounds,
      journal.overruns,
      journal.largest_records,
      journal.largest_bytes,
      journal.largest_lag_ns,
      journal.longest_lag_ns,
      journal.mean_record_bytes,
      journal.budget_bytes,
      load()
    )
  }

  impl Bench {
    /// A fresh volume of [`VOLUME_BYTES`] named `name`.
    fn create(&mut self, name: &str) -> VolumeId {
      self
        .client
        .create(&CreateSpec {
          name: name.to_owned(),
          size: SizeClass::Bounded {
            limit: VOLUME_BYTES,
          },
          names: NamePolicy::Exact,
          require_locked: false,
          base: None,
        })
        .unwrap()
    }

    /// A mount point named `name` beside the binary, made empty.
    fn point(&self, name: &str) -> String {
      let point = format!("{}/{name}-{}", self.examples.display(), std::process::id());
      finish(Command::new("mkdir").args(["-p", &point]), "mkdir");
      point
    }

    /// Mounts volume `id` at `point` through the real `slates mount`.
    fn mount(&self, id: &str, point: &str) {
      finish(
        Command::new(&self.slates).args(["mount", id, point, "--instance", &self.instance]),
        "slates mount",
      );
    }

    /// Unmounts `point` and removes it, asking again while the mount is busy (a workload's last child can hold it for
    /// a moment after it exits).
    fn unmount(&self, point: &str) {
      let started = Instant::now();
      loop {
        let output = Command::new("fusermount3")
          .args(["-u", point])
          .output()
          .unwrap();
        if output.status.success() {
          break;
        }
        assert!(
          started.elapsed() < WAIT_FOR,
          "fusermount3 -u {point}: {}",
          String::from_utf8_lossy(&output.stderr)
        );
        pause(POLL);
      }
      finish(Command::new("rmdir").arg(point), "rmdir");
    }

    /// The volume's journal report once the watcher's rounds have settled: [`SETTLED_POLLS`] polls in a row with no
    /// new round and no new record.
    fn settled(&mut self, volume: VolumeId) -> JournalReport {
      let started = Instant::now();
      let mut last = self.client.status(volume).unwrap().journal;
      let mut quiet = 0;
      while quiet < SETTLED_POLLS {
        assert!(
          started.elapsed() < WAIT_FOR,
          "the rounds never settled: {last:?}"
        );
        pause(POLL);
        let now = self.client.status(volume).unwrap().journal;
        if now.rounds == last.rounds && now.appended == last.appended {
          quiet += 1;
        } else {
          quiet = 0;
        }
        last = now;
      }
      last
    }

    /// Runs `workload` once on a fresh volume, with a second mount watching, and prints its row.
    fn measure(&mut self, workload: Workload, run: usize) {
      let name = format!("jb-{}-{run}", workload.name());
      let volume = self.create(&name);
      let id: String = volume
        .bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
      let watcher = self.point(&format!("{name}-watcher"));
      self.mount(&id, &watcher);
      let (elapsed, writer) = match workload {
        Workload::GitClone => {
          let writer = self.point(&format!("{name}-writer"));
          self.mount(&id, &writer);
          let source = format!("file://{}", self.checkout.display());
          let started = Instant::now();
          finish(
            Command::new("git").args([
              "clone",
              "--quiet",
              "--depth",
              "1",
              &source,
              &format!("{writer}/clone"),
            ]),
            "git clone",
          );
          (started.elapsed(), Some(writer))
        }
        Workload::CargoBuild => {
          let writer = self.point(&format!("{name}-writer"));
          self.mount(&id, &writer);
          let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
          let manifest = self.checkout.join(BUILD_MANIFEST);
          let started = Instant::now();
          finish(
            Command::new(cargo)
              .current_dir(&self.checkout)
              .env_remove("CARGO_TARGET_DIR")
              .arg("build")
              .arg("--quiet")
              .arg("--offline")
              .arg("--locked")
              .arg("--manifest-path")
              .arg(&manifest)
              .arg("--target-dir")
              .arg(format!("{writer}/target")),
            "cargo build",
          );
          (started.elapsed(), Some(writer))
        }
        Workload::SdkRing => (self.ring(volume, &watcher), None),
      };
      let journal = self.settled(volume);
      println!("{}", row(workload, run, elapsed, &journal));
      if let Some(writer) = writer {
        self.unmount(&writer);
      }
      self.unmount(&watcher);
      self.client.destroy(volume).unwrap();
    }

    /// The SDK workload: writes [`RING_FILES`] files, reads each through the watcher, then sends [`RING_WRITES`]
    /// replacements of them a ring's worth at a time, collecting every reply before the next ring. Returns how long the
    /// replacements took.
    fn ring(&mut self, volume: VolumeId, watcher: &str) -> Duration {
      let attachment = self
        .client
        .attach(volume, None, Intent::Write)
        .unwrap()
        .attachment;
      let target = (volume, attachment);
      for index in 0..RING_FILES {
        self
          .client
          .fs_write(target, &ring_path(index), &[0; RING_FILE_BYTES], FILE_MODE)
          .unwrap();
        #[allow(clippy::disallowed_methods)] // a read through the slates mount: RAM, not disk
        let read = std::fs::read(format!("{watcher}/{}", ring_path(index))).unwrap();
        assert_eq!(read.len(), RING_FILE_BYTES);
      }
      let started = Instant::now();
      let mut done = 0;
      while done < RING_WRITES {
        let in_flight = self.send_a_ring(target, done);
        assert!(
          !in_flight.is_empty() || started.elapsed() < WAIT_FOR,
          "the ring stayed full with nothing of ours in it"
        );
        for request in in_flight {
          self.collect(request);
          done += 1;
        }
        // A request sent with `begin` carries no periodic acknowledgement, and the daemon refuses past its bound on
        // completion records retained (AC-2.3), so each ring's completions are released before the next.
        self.client.acknowledge_all().unwrap();
      }
      let elapsed = started.elapsed();
      self.client.detach(attachment).unwrap();
      elapsed
    }

    /// Sends replacements from the `done`th on without waiting, as many as the ring takes before it is full (or
    /// [`RING_WRITES`] are out), and returns their request ids.
    fn send_a_ring(
      &mut self,
      (volume, attachment): (VolumeId, u64),
      done: usize,
    ) -> Vec<RequestId> {
      let mut in_flight = Vec::new();
      while done + in_flight.len() < RING_WRITES {
        let index = done + in_flight.len();
        let body = RequestBody::FsWrite {
          volume,
          attachment,
          path: ring_path(index),
          bytes: vec![index.to_le_bytes().first().copied().unwrap(); RING_FILE_BYTES],
          mode: FILE_MODE,
        };
        match self.client.begin(&body) {
          Ok(request) => in_flight.push(request),
          Err(ClientError::RingFull | ClientError::TooManyOutstanding { .. }) => break,
          Err(e) => panic!("begin: {e}"),
        }
      }
      in_flight
    }

    /// Takes `request`'s reply within [`WAIT_FOR`], which must be the end of a write.
    fn collect(&mut self, request: RequestId) {
      let asked = Instant::now();
      let reply = loop {
        let spin = self.client.published_spin_ns();
        if let Some(reply) = self.client.spin_reply(request, spin).unwrap() {
          break reply;
        }
        assert!(asked.elapsed() < WAIT_FOR, "no reply to {request:?}");
      };
      assert!(matches!(reply, ReplyBody::FsDone { .. }), "{reply:?}");
    }
  }

  /// The file the `index`th replacement of the SDK workload rewrites.
  fn ring_path(index: usize) -> String {
    format!("f{}.bin", index % RING_FILES)
  }

  pub(super) fn run() {
    let checkout = checkout();
    let profile = MachineProfile::measure_patiently(&ProfileOptions {
      budget_per_probe: PROBE_BUDGET,
      codecs: false,
      core_matrix: false,
    })
    .expect("the machine profile measures");
    let instance = format!("slates-jb-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance, None);
    let daemon = Daemon::start(
      &profile,
      config,
      SegmentSource::Create {
        name: format!("slates-seg-jb-{}", std::process::id()),
      },
    )
    .unwrap();
    daemon.bootstrap(true).unwrap();
    let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
    let started = Instant::now();
    let client = loop {
      match Client::connect(&instance, deadlines) {
        Ok(client) => break client,
        Err(e) => assert!(started.elapsed() < WAIT_FOR, "connect: {e}"),
      }
    };
    let exe = std::env::current_exe().unwrap();
    let examples = exe.parent().unwrap().to_path_buf();
    let slates = examples
      .parent()
      .map(|release| release.join("slates"))
      .unwrap();
    assert!(
      Path::new(&slates).exists(),
      "build the slates binary beside the example: {}",
      slates.display()
    );
    let mut bench = Bench {
      client,
      instance,
      slates,
      examples,
      checkout,
    };
    println!(
      "journal\tworkload\trun\tseconds\tappended\tdropped\trounds\toverruns\tlargest_records\tlargest_bytes\t\
       largest_lag_ns\tlongest_lag_ns\tmean_record_bytes\tbudget_bytes\tload"
    );
    for run in 0..RUNS {
      for workload in [Workload::GitClone, Workload::CargoBuild, Workload::SdkRing] {
        bench.measure(workload, run);
      }
    }
    drop(bench);
    drop(daemon);
  }
}
