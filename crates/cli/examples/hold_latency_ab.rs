//! What a WinFsp mount and unmount wait for the anchor (A-213; A-198's hold, A-206's release): each mount's hold and
//! each unmount's release is a round trip to the anchor over its hold channel, which the Windows anchor looked at only
//! between supervision steps, up to half the liveness budget (500 ms) apart, until it learned to wake on the channel.
//! This program times the real `slates mount` and `slates unmount` of builds given to it, interleaved, so the
//! difference is the anchor's answer and nothing else.
//!
//! Each arm is a `slates` binary. The program starts that binary's anchor (`slates anchor --quick`) on an instance of
//! its own, bootstraps it and creates one volume. Then, for [`ROUNDS`] rounds, every arm mounts the volume at a free
//! drive letter, the program waits until the drive answers, unmounts it, and waits until the letter is gone. Each
//! `slates` command is timed from its start to its exit. Rounds rotate which arm goes first, so a load that drifts lands
//! on every arm alike, and one untimed mount and unmount per arm runs first.
//!
//! Output: rows `hold_latency\t<arm>\t<round>\t<mount ns>\t<unmount ns>`; then, per arm and verb, every round's time in
//! milliseconds, sorted, with the best and the median. Nothing here gates; the A/B in `docs/wip/BENCHMARKS.md` is two
//! builds of `slates`, before and after, given to one run of this program.
//!
//! The volume is in RAM; nothing is written to disk (R1). Windows with WinFsp installed only; elsewhere it says so and
//! exits.
//!
//! Run (Windows, WinFsp installed): `cargo build --release -p slates-cli --bin slates --example hold_latency_ab`, copy
//! `target/release/slates.exe` of each build aside, then
//! `target/release/examples/hold_latency_ab --arm before=C:\before\slates.exe --arm after=C:\after\slates.exe`.
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

#[cfg(windows)]
fn main() {
  windows::run();
}

#[cfg(not(windows))]
fn main() {
  eprintln!(
    "SKIP hold_latency_ab: the hold it times is the Windows anchor's, around a WinFsp drive"
  );
}

#[cfg(windows)]
mod windows {
  use std::io::BufRead;
  use std::process::{Child, Command, Stdio};
  use std::time::{Duration, Instant};

  use slates_server::daemon::LIVENESS_BUDGET_NS;

  /// Shape: timed rounds per arm, the count the goal's other Windows measures use (20 drives, 20 unmounts).
  const ROUNDS: usize = 20;
  /// Shape: how long a drive may take to answer or to go: ten liveness budgets, as the live WinFsp tests wait.
  const WAIT_FOR: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS * 10);
  /// Shape: the pause between polls of a drive (milliseconds).
  const POLL_MS: u64 = 5;
  /// Shape: consecutive answered verbs before a started daemon counts as settled, as the CLI tests wait.
  const STABLE_STREAK: u32 = 10;
  /// Shape: the most `volume list` attempts a start makes before the daemon counts as absent.
  const START_ATTEMPTS: u32 = 60;
  /// Format: the daemon's line once it serves (`crates/cli/src/daemon.rs`); the instance name follows.
  const SERVING_LINE: &str = "slates daemon: instance ";
  /// Format: milliseconds in a second, for the summary.
  const MS_PER_S: f64 = 1_000.0;

  /// One build under test: its name, its binary, its anchor (killed and reaped on drop), and its volume.
  struct Arm {
    name: String,
    binary: String,
    instance: String,
    anchor: Child,
    volume: String,
    mounts: Vec<u64>,
    unmounts: Vec<u64>,
  }

  impl Drop for Arm {
    fn drop(&mut self) {
      let _ = self.anchor.kill();
      let _ = self.anchor.wait();
    }
  }

  fn pause() {
    // The harness paces its polls; shipped code parks on its driver (D-9).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(POLL_MS));
  }

  /// Runs `binary --instance INSTANCE ARGS`: its exit code, stdout, stderr, and how long it took.
  fn invoke(binary: &str, instance: &str, args: &[&str]) -> (i32, String, String, Duration) {
    let started = Instant::now();
    let output = Command::new(binary)
      .arg("--instance")
      .arg(instance)
      .args(args)
      .output()
      .unwrap();
    let took = started.elapsed();
    (
      output.status.code().unwrap_or(-1),
      String::from_utf8_lossy(&output.stdout).into_owned(),
      String::from_utf8_lossy(&output.stderr).into_owned(),
      took,
    )
  }

  /// Whether the drive `letter:` answers a look-up of its root.
  fn answers(drive: &str) -> bool {
    std::fs::metadata(format!("{drive}\\")).is_ok()
  }

  /// The first letter from `E:` to `Z:` that names no drive.
  fn free_drive() -> String {
    ('E'..='Z')
      .map(|letter| format!("{letter}:"))
      .find(|drive| !answers(drive))
      .expect("a free drive letter")
  }

  /// Polls until `done` holds, within [`WAIT_FOR`].
  fn wait_until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
      assert!(started.elapsed() < WAIT_FOR, "{what} within {WAIT_FOR:?}");
      pause();
    }
  }

  /// Starts `binary`'s anchor on its own instance, waits for its daemon to serve and settle, bootstraps it and makes
  /// one volume.
  fn start(name: &str, binary: &str) -> Arm {
    let instance = format!("hl{name}{}", std::process::id());
    let mut anchor = Command::new(binary)
      .args(["--instance", &instance, "anchor", "--quick"])
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let mut stderr = std::io::BufReader::new(anchor.stderr.take().unwrap());
    let serving = format!("{SERVING_LINE}{instance} serving on ");
    let mut line = String::new();
    loop {
      line.clear();
      assert!(
        stderr.read_line(&mut line).unwrap() > 0,
        "{name}: the anchor ended before its daemon served"
      );
      if line.starts_with(&serving) {
        break;
      }
    }
    // The rest of the anchor's log is drained so it never blocks on a full pipe; the thread ends with the anchor.
    std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let mut streak = 0u32;
    for _ in 0..START_ATTEMPTS {
      let (code, ..) = invoke(binary, &instance, &["volume", "list"]);
      streak = if code == 0 {
        streak.saturating_add(1)
      } else {
        0
      };
      if streak >= STABLE_STREAK {
        break;
      }
      pause();
    }
    assert!(streak >= STABLE_STREAK, "{name}: the daemon never settled");
    let (code, _, error, _) = invoke(binary, &instance, &["bootstrap", "root"]);
    assert_eq!(code, 0, "{name}: bootstrap: {error}");
    let (code, out, error, _) = invoke(
      binary,
      &instance,
      &["volume", "create", "held", "--bounded", "64MiB"],
    );
    assert_eq!(code, 0, "{name}: create: {error}");
    let volume = out
      .lines()
      .find_map(|line| line.strip_prefix("id: "))
      .unwrap()
      .to_owned();
    Arm {
      name: name.to_owned(),
      binary: binary.to_owned(),
      instance,
      anchor,
      volume,
      mounts: Vec::new(),
      unmounts: Vec::new(),
    }
  }

  /// One mount and unmount of the arm's volume: the two commands' times, each waited out until the drive answers or
  /// is gone.
  fn round(arm: &Arm) -> (Duration, Duration) {
    let drive = free_drive();
    let (code, _, error, mount) =
      invoke(&arm.binary, &arm.instance, &["mount", &arm.volume, &drive]);
    assert_eq!(code, 0, "{}: mount: {error}", arm.name);
    wait_until("the drive answers", || answers(&drive));
    let (code, _, error, unmount) = invoke(&arm.binary, &arm.instance, &["unmount", &drive]);
    assert_eq!(code, 0, "{}: unmount: {error}", arm.name);
    wait_until("the drive goes", || !answers(&drive));
    (mount, unmount)
  }

  fn nanos(took: Duration) -> u64 {
    u64::try_from(took.as_nanos()).unwrap_or(u64::MAX)
  }

  /// `ns` in milliseconds.
  fn ms(ns: u64) -> f64 {
    Duration::from_nanos(ns).as_secs_f64() * MS_PER_S
  }

  /// Every time in milliseconds, sorted, with the best and the median.
  fn summary(arm: &str, verb: &str, times: &[u64]) {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    let shown: Vec<String> = sorted.iter().map(|ns| format!("{:.1}", ms(*ns))).collect();
    let best = ms(sorted.first().copied().unwrap_or(0));
    let median = ms(sorted.get(sorted.len() / 2).copied().unwrap_or(0));
    println!(
      "# {arm} {verb}: best {best:.1} ms, median {median:.1} ms, all {} (ms, sorted): {}",
      sorted.len(),
      shown.join(" ")
    );
  }

  fn run_arms(arms: &mut [Arm]) {
    for arm in arms.iter() {
      round(arm);
    }
    for round_number in 0..ROUNDS {
      let first = round_number % arms.len();
      for offset in 0..arms.len() {
        let index = (first + offset) % arms.len();
        let (mount, unmount) = round(&arms[index]);
        let arm = &mut arms[index];
        arm.mounts.push(nanos(mount));
        arm.unmounts.push(nanos(unmount));
        println!(
          "hold_latency\t{}\t{round_number}\t{}\t{}",
          arm.name,
          nanos(mount),
          nanos(unmount)
        );
      }
    }
    for arm in arms.iter() {
      summary(&arm.name, "mount", &arm.mounts);
      summary(&arm.name, "unmount", &arm.unmounts);
    }
  }

  pub(super) fn run() {
    let mut arms = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
      assert_eq!(flag, "--arm", "usage: --arm NAME=PATH (repeated)");
      let arm = args.next().expect("--arm NAME=PATH");
      let (name, binary) = arm.split_once('=').expect("--arm NAME=PATH");
      arms.push(start(name, binary));
    }
    assert!(!arms.is_empty(), "usage: --arm NAME=PATH (repeated)");
    println!(
      "# hold_latency_ab: {ROUNDS} rounds per arm, interleaved, the first arm rotating; host {}",
      std::env::var("COMPUTERNAME").unwrap_or_default()
    );
    run_arms(&mut arms);
  }
}
