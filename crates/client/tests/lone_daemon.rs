//! A daemon nobody supervises ends its own process on its watchdog's stuck verdict (A-232, §4.14 `daemon.alive`), and a
//! client's call to it ends `DaemonGone`: neither `Stalled` nor waiting for good. The daemon runs in a child process,
//! this test binary re-invoked as [`a_lone_daemon_role`] (an environment variable selects the role), over a segment it
//! makes itself (`SegmentSource::Create`, as `slates daemon` run alone does), so its end is a real process's exit and the
//! client sees it as it sees a kill: the socket's close on Linux, the exit watch on macOS and Windows.
//!
//! The parent connects, tells the child its client id, and the child holds the shard that serves that client in one
//! poll: asleep for good (wedged) or on the CPU for good (spinning). The parent then submits a call, which the client
//! waits for while its daemon lives (A-232's one rule), so the call is outstanding behind the hold, and drives the
//! driver as an SDK's loop does while it watches the child. The bound is the rule's, from its named terms (A-232 step 5):
//! the watchdog sees the shard's last movement at its first look after the hold began, at most a beat and that look's
//! lateness after it, and finds it stuck at its first look a whole window after that. So the verdict comes within
//! `W + B + L` of the hold, where `W` is the liveness budget, `B` the beat and `L` the lateness of the looks the verdict
//! rests on, summed, which its end line names (`slates_server::watchdog`'s `Lateness`). The end then takes its own
//! time, `end`: from the verdict's line to the client's liveness answering gone, measured. The hold's time `t0` is the
//! host's monotonic clock read inside the held poll, which every process on the host shares.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use slates_client::driver::{Driver, Event};
use slates_client::{Client, ClientError, Deadlines};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::watchdog::FAILED_EXIT_CODE;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the environment variable that selects the role, and the one naming its instance.
const ROLE: &str = "SLATES_LONE_DAEMON_ROLE";
const ROLE_INSTANCE: &str = "SLATES_LONE_DAEMON_INSTANCE";
/// Format: the role's tags, looked for anywhere in a line (libtest's banner is glued to a role's first line).
const READY_TAG: &str = "LONE-DAEMON-READY";
const HELD_TAG: &str = "LONE-DAEMON-HELD";
/// Format: the words the watchdog's end line begins its cause with, and the ones that name its lateness.
const END_WORDS: &str = "so the watchdog ends the daemon's process";
const LATE_WORDS: &str = "the looks since it last moved ran ";
/// Format: the held line's word for the hold's time on the host's monotonic clock.
const AT_WORD: &str = "at_ns=";
/// Shape: the probe budget of the quick profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards in the child's daemon: two, so the held shard is one of several.
const TEST_SHARDS: u16 = 2;
/// Shape: the window, the beat.
const WINDOW: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
const BEAT: Duration = Duration::from_nanos(HEARTBEAT_NS);
/// Shape: how long the child may take to start and the client to connect: twenty windows, a busy machine's start.
const START_WAIT: Duration = Duration::from_nanos(20 * LIVENESS_BUDGET_NS);
/// Shape: the test's own bound on the whole exchange once the hold has begun: ten windows, far past the rule's bound and
/// the reconnect budget after it, so a daemon that never ends itself fails here with the call still pending.
const GIVE_UP: Duration = Duration::from_nanos(10 * LIVENESS_BUDGET_NS);
/// Shape: the loop's pause between steps: a hundredth of a beat, so the exit is seen within it.
const STEP_PAUSE: Duration = Duration::from_nanos(HEARTBEAT_NS / 100);

/// How the child holds the shard serving the parent's client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
  /// Asleep for good in one poll: no CPU, never runnable (a deadlock, a host call that never returns).
  Wedged,
  /// On the CPU for good in one poll: a task that never yields.
  Spinning,
  /// Not held at all: the role lowers its own priority as far as it goes, and the parent starves it with spinners.
  Starved,
}

impl Hold {
  fn word(self) -> &'static str {
    match self {
      Hold::Wedged => "wedged",
      Hold::Spinning => "spinning",
      Hold::Starved => "starved",
    }
  }

  fn from_word(word: &str) -> Hold {
    match word {
      "wedged" => Hold::Wedged,
      "spinning" => Hold::Spinning,
      "starved" => Hold::Starved,
      other => panic!("no hold named {other:?}"),
    }
  }
}

/// The product's own deadlines.
fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn profile() -> MachineProfile {
  MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures")
}

fn pause(span: Duration) {
  // The harness paces its loop; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(span);
}

/// Which of `daemon`'s shards (its index in `Daemon::shards`) serves the client `client_id`, once its connection is
/// seated.
fn serving(daemon: &Daemon, client_id: u32) -> usize {
  let started = Instant::now();
  loop {
    let found = daemon.shards().iter().position(|shard| {
      daemon
        .observation(
          Some(*shard),
          OBSERVE_BUDGET_NS,
          move |state: &mut ShardState| {
            state
              .clients
              .iter()
              .any(|(_, slot)| slot.client_id == client_id)
          },
        )
        .unwrap()
        .wait()
        .unwrap_or(false)
    });
    if let Some(shard) = found {
      return shard;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "client {client_id} was seated on a shard"
    );
    pause(BEAT);
  }
}

/// When the held poll last read the host's monotonic clock, right before it holds: zero until then. Written once by the
/// held shard, read by the thread that reports it.
static HELD_AT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Holds `daemon`'s shard at `index` in one poll for good, as `hold` says, and says so on standard output, with the
/// host's monotonic clock read inside the poll as the last thing before the hold. The line is printed by another thread,
/// so the shard does nothing between its reading and its hold that a lost CPU could stretch (A-232 step 5).
fn hold_for_good(daemon: &Daemon, index: usize, hold: Hold) {
  let shard = daemon.shards()[index];
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      // A host call that never returns (the watchdog's own example of a wedge): a read of a pipe whose write end is
      // kept open and never written.
      let (never, _kept) = rustix::pipe::pipe().unwrap();
      let mut byte = [0_u8; 1];
      HELD_AT_NS.store(
        slates_machine::clock::monotonic_ns(),
        std::sync::atomic::Ordering::Release,
      );
      match hold {
        Hold::Wedged => {
          let _ = rustix::io::read(&never, &mut byte);
        }
        Hold::Spinning => loop {
          std::hint::spin_loop();
        },
        Hold::Starved => {}
      }
    })
    .unwrap();
  std::thread::spawn(move || {
    let _ = observation.wait();
  });
  loop {
    let at = HELD_AT_NS.load(std::sync::atomic::Ordering::Acquire);
    if at != 0 {
      println!("{HELD_TAG} shard={index} {} {AT_WORD}{at}", hold.word());
      return;
    }
    pause(STEP_PAUSE);
  }
}

/// The role [`a_lone_daemons_wedged_shard_ends_its_process_and_the_call_ends_daemon_gone`] and its spinning sibling run
/// in a child process: a daemon over a segment of its own, a client id read from standard input, that client's shard
/// held for good. It never returns: the watchdog ends the process, or the parent kills it at its bound.
#[test]
#[ignore = "a role: run by this file's tests in a child process"]
fn a_lone_daemon_role() {
  let Ok(hold) = std::env::var(ROLE) else {
    return;
  };
  let hold = Hold::from_word(&hold);
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let profile = profile();
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{instance}"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  if hold == Hold::Starved {
    lower_this_process();
  }
  println!("{READY_TAG}");
  if hold == Hold::Starved {
    loop {
      std::thread::park();
    }
  }
  let mut line = String::new();
  std::io::stdin().read_line(&mut line).unwrap();
  let client_id: u32 = line.trim().parse().unwrap();
  hold_for_good(&daemon, serving(&daemon, client_id), hold);
  loop {
    std::thread::park();
  }
}

/// Shape: the nice value the starved role takes, the lowest priority a process may give itself.
#[cfg(unix)]
const LOWEST_NICE: i32 = 19;

/// Lowers every thread of this process to the lowest priority the platform lets a process give itself: the daemon then
/// competes for a CPU as a background job beside the parent's spinners. On Linux a nice value is per thread, so each
/// thread is lowered; elsewhere the process's value is.
fn lower_this_process() {
  #[cfg(target_os = "linux")]
  {
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
      if let Some(tid) = task
        .file_name()
        .to_str()
        .and_then(|name| name.parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
      {
        let _ = rustix::process::setpriority_process(Some(tid), LOWEST_NICE);
      }
    }
  }
  #[cfg(all(unix, not(target_os = "linux")))]
  rustix::process::setpriority_process(None, LOWEST_NICE).unwrap();
}

/// The child, killed and reaped on drop, so a failed assertion leaves no daemon behind.
struct LoneDaemon {
  child: Child,
  stdin: Option<ChildStdin>,
  /// Every line of its standard output, with when the parent read it on the host's monotonic clock.
  stdout: Receiver<(u64, String)>,
  /// Every line of its standard error, the same way.
  stderr: Receiver<(u64, String)>,
}

impl Drop for LoneDaemon {
  fn drop(&mut self) {
    if matches!(self.child.try_wait(), Ok(None)) {
      let _ = self.child.kill();
    }
    let _ = self.child.wait();
  }
}

/// Reads `from` line by line on a thread of its own, each line stamped with when it arrived (the host's monotonic clock).
fn lines_of(from: impl std::io::Read + Send + 'static) -> Receiver<(u64, String)> {
  let (said, lines) = channel();
  std::thread::spawn(move || {
    for line in BufReader::new(from).lines() {
      let Ok(line) = line else {
        return;
      };
      if said
        .send((slates_machine::clock::monotonic_ns(), line))
        .is_err()
      {
        return;
      }
    }
  });
  lines
}

fn start_lone_daemon(instance: &str, hold: Hold) -> LoneDaemon {
  let mut child = Command::new(std::env::current_exe().unwrap())
    .args(["--ignored", "--exact", "a_lone_daemon_role", "--nocapture"])
    .env(ROLE, hold.word())
    .env(ROLE_INSTANCE, instance)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let stdin = child.stdin.take();
  let stdout = lines_of(child.stdout.take().unwrap());
  let stderr = lines_of(child.stderr.take().unwrap());
  LoneDaemon {
    child,
    stdin,
    stdout,
    stderr,
  }
}

/// Waits for a line of `lines` carrying `tag`, within `within`; the line.
fn await_tag(lines: &Receiver<(u64, String)>, tag: &str, within: Duration) -> String {
  let started = Instant::now();
  loop {
    let left = within.saturating_sub(started.elapsed());
    match lines.recv_timeout(left) {
      Ok((_, line)) if line.contains(tag) => return line,
      Ok(_) => {}
      Err(error) => panic!("no {tag} line within {within:?}: {error}"),
    }
  }
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        pause(BEAT);
      }
      Err(refused) => panic!("the lone daemon refused a client: {refused}"),
    }
  }
}

/// The hold's time on the host's monotonic clock, from the held line.
fn held_at_ns(line: &str) -> u64 {
  line
    .split(AT_WORD)
    .nth(1)
    .and_then(|rest| rest.split(' ').next())
    .and_then(|number| number.parse().ok())
    .unwrap_or_else(|| panic!("the held line names its time: {line}"))
}

/// What one exchange measured.
struct Ended {
  /// How the call ended, if it did within the test's bound.
  call: Option<Result<(), ClientError>>,
  /// From the hold to the call's end.
  call_after: Option<Duration>,
  /// The child's exit and how long after the hold it was seen.
  exit: Option<(ExitStatus, Duration)>,
  /// When the client's liveness first answered the daemon gone, on the host's monotonic clock.
  gone_at_ns: Option<u64>,
}

/// Drives `driver` as an SDK's loop does until the call `ticket` ends or [`GIVE_UP`] passes, watching `daemon`'s process.
fn drive(
  client: &mut Client,
  driver: &mut Driver,
  ticket: u64,
  daemon: &mut LoneDaemon,
  held_at: Instant,
) -> Ended {
  let mut ended = Ended {
    call: None,
    call_after: None,
    exit: None,
    gone_at_ns: None,
  };
  let mut next_tick = Instant::now();
  while ended.call.is_none() && held_at.elapsed() < GIVE_UP {
    let mut events = driver.pump(client);
    if Instant::now() >= next_tick {
      events.extend(driver.tick(client));
    }
    for event in events {
      let outcome = match event {
        Event::Ready {
          ticket: ready,
          word,
        } if ready == ticket => {
          let reply = client.poll_reply_word(word);
          driver.finish(ticket);
          reply.map(|_| ())
        }
        Event::Failed {
          ticket: failed,
          error,
        } if failed == ticket => Err(error),
        _ => continue,
      };
      ended.call = Some(outcome);
      ended.call_after = Some(held_at.elapsed());
    }
    if ended.gone_at_ns.is_none() && client.daemon_gone() {
      ended.gone_at_ns = Some(slates_machine::clock::monotonic_ns());
    }
    if ended.exit.is_none()
      && let Ok(Some(status)) = daemon.child.try_wait()
    {
      ended.exit = Some((status, held_at.elapsed()));
    }
    next_tick = Instant::now()
      + Duration::from_nanos(driver.next_wake_ns(client).unwrap_or(deadlines().reply_ns));
    step_wait(client, ended.gone_at_ns.is_some());
  }
  ended
}

/// The loop's wait between steps: [`STEP_PAUSE`], or less on Linux while the daemon is not yet seen gone, since there it
/// wakes the moment the daemon's control socket closes (`Client::daemon_watch`), so the gone time is read when it
/// happens rather than at the next step. A step's sleep made the gone time up to a step late: "DaemonGone time" is the
/// test's measurement, and a 1 ms sample was error added to it (A-232 step 5, round 2). Once gone, a closed socket
/// would wake every wait at once, so the loop sleeps its step.
fn step_wait(client: &Client, gone_seen: bool) {
  #[cfg(target_os = "linux")]
  if !gone_seen && let Some(watch) = client.daemon_watch() {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut interest = [PollFd::new(&watch, PollFlags::RDHUP | PollFlags::HUP)];
    let timeout = Timespec::try_from(STEP_PAUSE).ok();
    let _ = poll(&mut interest, timeout.as_ref());
    return;
  }
  let _ = (client, gone_seen);
  pause(STEP_PAUSE);
}

/// The lateness the watchdog's end line names, when the parent read the line (the host's monotonic clock), and the
/// line, from everything the child said on standard error.
fn end_line(stderr: &Receiver<(u64, String)>) -> Option<(Duration, u64, String)> {
  let mut found = None;
  while let Ok((read_at_ns, line)) = stderr.recv_timeout(BEAT) {
    if line.contains(END_WORDS) {
      let late_us: u64 = line
        .split(LATE_WORDS)
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("the end line names its lateness: {line}"));
      found = Some((Duration::from_micros(late_us), read_at_ns, line));
    }
  }
  found
}

/// The milliseconds of CPU a spinning verdict names in the end line.
fn spun_for(line: &str) -> Duration {
  let ms: u64 = line
    .split("is spinning, ")
    .nth(1)
    .and_then(|rest| rest.split(' ').next())
    .and_then(|number| number.parse().ok())
    .unwrap_or_else(|| panic!("the end line names the CPU spun: {line}"));
  Duration::from_millis(ms)
}

/// One lone daemon held `hold` with a call outstanding: the daemon must end itself within the rule's bound, with the
/// failed exit code, and the call must end `DaemonGone`.
///
/// The bound is the rule's own measure. A wedge is judged on the wall clock, so its verdict comes within `W + B + L` of
/// the hold `t0` (the module doc). The verdict's line is written before the end, so the parent reading it bounds the
/// verdict's time from above, and the test asserts that bound: the end line read by `t0 + W + B + L`. Then `end`, the
/// time from that line to the client's liveness answering gone, is measured, and the client's gone time is reported
/// against `t0 + W + B + L + end` and against the plan's `t0 + W + B + L` without it. A spin is judged in the shard's
/// own CPU time, which a busy machine hands out slower than the wall runs, so its verdict must name at least a window
/// of CPU and at most a window and one look's (`B + L`). The call then ends at its next deadline after the exit, past
/// the reconnect budget: within `exit + reply + reconnect` and one of the reconnect's paced attempts (`reconnect / 10`,
/// the driver's pacing).
fn a_lone_daemons_stuck_shard_ends_it(name: &str, hold: Hold) {
  let instance = format!("lone-{name}-{}", std::process::id());
  let mut daemon = start_lone_daemon(&instance, hold);
  await_tag(&daemon.stdout, READY_TAG, START_WAIT);
  let mut client = connect(&instance);
  let stdin = daemon.stdin.as_mut().unwrap();
  writeln!(stdin, "{}", client.client_id()).unwrap();
  stdin.flush().unwrap();
  let t0_ns = held_at_ns(&await_tag(&daemon.stdout, HELD_TAG, START_WAIT));
  let held_at = Instant::now();
  let mut driver = Driver::new(&client);
  let ticket = driver
    .submit(
      &mut client,
      Box::new(|client: &mut Client| client.list_begin()),
    )
    .unwrap();
  let ended = drive(&mut client, &mut driver, ticket, &mut daemon, held_at);
  let (status, exited_after) = ended.exit.unwrap_or_else(|| {
    panic!(
      "{name}: the lone daemon was still running {GIVE_UP:?} after its shard was held; the call: {:?} (None: still \
       pending)",
      ended.call
    )
  });
  let (late, line_read_ns, line) = end_line(&daemon.stderr).unwrap_or_else(|| {
    panic!("{name}: the lone daemon exited ({status}) without the watchdog's end line")
  });
  let gone_ns = ended
    .gone_at_ns
    .unwrap_or_else(|| panic!("{name}: the client's liveness never answered the daemon gone"));
  let rule = WINDOW + BEAT + late;
  let line_after = Duration::from_nanos(line_read_ns.saturating_sub(t0_ns));
  let gone_after = Duration::from_nanos(gone_ns.saturating_sub(t0_ns));
  let end = Duration::from_nanos(gone_ns.saturating_sub(line_read_ns));
  let deadlines = deadlines();
  let call_bound = exited_after
    + Duration::from_nanos(
      deadlines.reply_ns + deadlines.reconnect_ns + deadlines.reconnect_ns / 10,
    );
  let signed_ns = |after: Duration, bound: Duration| -> i128 {
    i128::try_from(after.as_nanos()).unwrap() - i128::try_from(bound.as_nanos()).unwrap()
  };
  println!(
    "A232-UNSUPERVISED {name}: W = {WINDOW:?}, B = {BEAT:?}, L = {late:?}, so W + B + L = {rule:?}; the verdict's line \
     read {line_after:?} after t0 (margin {} ns); end = {end:?}; DaemonGone (the client's liveness) {gone_after:?} \
     after t0: minus (W + B + L + end) = {} ns, minus (W + B + L) = {} ns; exit {status} seen {exited_after:?} after \
     the hold was read; the call ended {:?} after {:?}, bound {call_bound:?}; the watchdog said: {line}",
    signed_ns(line_after, rule),
    signed_ns(gone_after, rule + end),
    signed_ns(gone_after, rule),
    ended.call,
    ended.call_after
  );
  assert_eq!(
    status.code(),
    Some(i32::from(FAILED_EXIT_CODE)),
    "{name}: the watchdog's exit code"
  );
  match hold {
    Hold::Wedged => assert!(
      line_after <= rule,
      "{name}: the watchdog's verdict was read {line_after:?} after the wedge, past the rule's W + B + L = {rule:?}"
    ),
    Hold::Starved => {
      panic!("{name}: a starved daemon is not a stuck one; this case is not run here")
    }
    Hold::Spinning => {
      let look = BEAT + late;
      let spun = spun_for(&line);
      assert!(
        spun >= WINDOW && spun <= WINDOW + look,
        "{name}: the verdict named {spun:?} of CPU, outside a window to a window and a look ({:?})",
        WINDOW + look
      );
    }
  }
  assert!(
    matches!(ended.call, Some(Err(ClientError::DaemonGone { .. }))),
    "{name}: the outstanding call ended DaemonGone, not {:?}",
    ended.call
  );
  assert!(
    ended.call_after.is_some_and(|after| after <= call_bound),
    "{name}: the call ended {:?} after the hold, past {call_bound:?}",
    ended.call_after
  );
}

/// A-232, §4.14. Do: in a lone daemon (a child process, `SegmentSource::Create`), wedge the shard that serves a client —
/// one poll asleep for good — and keep a call of that client outstanding. Expect: the daemon's process exits with the
/// failed exit code, its watchdog's verdict read within `W + B + L` of the wedge (the module doc), and the call ends
/// `DaemonGone`. Before A-232 the watchdog withheld a beat nobody read, the process lived on, and the call waited until
/// this test's own bound.
#[test]
fn a_lone_daemons_wedged_shard_ends_its_process_and_the_call_ends_daemon_gone() {
  a_lone_daemons_stuck_shard_ends_it("wedged", Hold::Wedged);
}

/// A-232, §4.14. Do: as the wedged case, with the shard on the CPU for good in one poll (a task that never yields).
/// Expect: the process exits with the failed exit code on a spinning verdict that names a window of CPU to a window and
/// one look's, and the call ends `DaemonGone`.
#[test]
fn a_lone_daemons_spinning_shard_ends_its_process_and_the_call_ends_daemon_gone() {
  a_lone_daemons_stuck_shard_ends_it("spinning", Hold::Spinning);
}

/// Format: the environment variable that runs the starved case, and the words the watchdog's starved line carries.
#[cfg(unix)]
const STARVE_ENV: &str = "SLATES_STARVE_LONE_DAEMON";
#[cfg(unix)]
const STARVED_WORDS: &str = "is starved";
/// Shape: how long the starved case starves the daemon: ten windows, many chances to misjudge it.
#[cfg(unix)]
const STARVE_FOR: Duration = Duration::from_nanos(10 * LIVENESS_BUDGET_NS);
/// Shape: spinners per logical CPU beside the starved daemon (the busy-machine bar's two a CPU).
#[cfg(unix)]
const SPINNERS_PER_CPU: usize = 2;

/// Spinners at the parent's priority, two a logical CPU, until `running` is cleared.
#[cfg(unix)]
fn spinners(running: &'static std::sync::atomic::AtomicBool) -> Vec<std::thread::JoinHandle<()>> {
  let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
  (0..cpus * SPINNERS_PER_CPU)
    .map(|_| {
      std::thread::spawn(move || {
        while running.load(std::sync::atomic::Ordering::Relaxed) {
          std::hint::spin_loop();
        }
      })
    })
    .collect()
}

/// What the starved case saw of its calls.
#[derive(Default)]
#[cfg(unix)]
struct Calls {
  answered: u64,
  longest: Duration,
  failed: Vec<Option<Result<(), ClientError>>>,
}

/// Calls `daemon`, one call after another, for [`STARVE_FOR`] or until a call fails or the process exits.
#[cfg(unix)]
fn call_for_the_span(client: &mut Client, daemon: &mut LoneDaemon) -> Calls {
  let mut driver = Driver::new(client);
  let mut calls = Calls::default();
  let started = Instant::now();
  while started.elapsed() < STARVE_FOR && calls.failed.is_empty() {
    let asked = Instant::now();
    let ticket = driver
      .submit(client, Box::new(|client: &mut Client| client.list_begin()))
      .unwrap();
    let ended = drive(client, &mut driver, ticket, daemon, asked);
    calls.longest = calls.longest.max(asked.elapsed());
    match ended.call {
      Some(Ok(())) if ended.exit.is_none() => calls.answered += 1,
      other => calls.failed.push(other),
    }
  }
  calls
}

/// How many times the child's standard error said a shard was starved, and its end line if it has one.
#[cfg(unix)]
fn starved_and_ended(stderr: &Receiver<(u64, String)>) -> (usize, Option<String>) {
  let mut said_starved = 0;
  let mut ended_line = None;
  while let Ok((_, line)) = stderr.recv_timeout(BEAT) {
    said_starved += usize::from(line.contains(STARVED_WORDS));
    if line.contains(END_WORDS) {
      ended_line = Some(line);
    }
  }
  (said_starved, ended_line)
}

/// A-232, §4.14 (heavy, so run only with `SLATES_STARVE_LONE_DAEMON=1`; it skips loudly otherwise). Do: run a lone daemon
/// at the lowest priority a process may give itself (nice 19 on Unix) beside two spinners a CPU at the parent's, and keep
/// calling it, one call after another, for ten windows. Expect: every call answered, the daemon's process still
/// running at the end, and no end line: a starved shard is not a stuck one, so it never ends a daemon. Non-vacuous only
/// if the daemon was starved: a call waited past the reply deadline, or the watchdog said a shard was starved; a run that
/// starved nothing fails, since it proves nothing.
#[cfg(unix)]
#[test]
fn a_lone_daemon_starved_beside_spinners_is_never_ended() {
  if std::env::var_os(STARVE_ENV).is_none() {
    println!(
      "SKIPPED: set {STARVE_ENV}=1 to starve a lone daemon beside two spinners a CPU (heavy)"
    );
    return;
  }
  let instance = format!("lone-starved-{}", std::process::id());
  let mut daemon = start_lone_daemon(&instance, Hold::Starved);
  await_tag(&daemon.stdout, READY_TAG, START_WAIT);
  let mut client = connect(&instance);
  let running: &'static std::sync::atomic::AtomicBool =
    Box::leak(Box::new(std::sync::atomic::AtomicBool::new(true)));
  let spun = spinners(running);
  let calls = call_for_the_span(&mut client, &mut daemon);
  running.store(false, std::sync::atomic::Ordering::Relaxed);
  for spinner in spun {
    spinner.join().unwrap();
  }
  let still_running = matches!(daemon.child.try_wait(), Ok(None));
  let (said_starved, ended_line) = starved_and_ended(&daemon.stderr);
  let reply = Duration::from_nanos(deadlines().reply_ns);
  println!(
    "starved: {} calls answered, the longest {:?} against a {reply:?} reply deadline; the watchdog said a shard was \
     starved {said_starved} times; still running: {still_running}; failed: {:?}",
    calls.answered, calls.longest, calls.failed
  );
  assert!(
    ended_line.is_none() && still_running,
    "a starved lone daemon was ended: {ended_line:?}"
  );
  assert!(
    calls.failed.is_empty(),
    "every call to the starved daemon was answered: {:?}",
    calls.failed
  );
  assert!(
    calls.longest > reply || said_starved > 0,
    "the run starved nothing (longest call {:?}, no starved verdict), so it proves nothing",
    calls.longest
  );
}
