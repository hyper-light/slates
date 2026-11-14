//! A-232 step 5, §4.7 and §4.14: the client's one rule — a call waits while its daemon lives — and its liveness bound,
//! proved against the anchor that ships. The test is this crate's own anchor: a child process of this test binary runs
//! [`crate::anchor::supervise`], the `slates anchor` path after its arguments, so the lapse, the lateness it measures and
//! the kill are the product's, not a copy's. The daemon it supervises is this binary again, re-invoked as
//! [`a_daemon_role`]: [`crate::daemon::start`] over the anchor's segment from the environment (`SegmentSource::FromEnv`,
//! a supervised daemon), then [`crate::daemon::serve`], as `slates daemon` runs. The daemon inherits the anchor's
//! standard streams, so the parent reads both processes' lines on one pipe and tells the first daemon, on standard
//! input, which client's shard to hold.
//!
//! Two shapes are proved here; the third, a daemon nobody supervises, is `crates/client/tests/lone_daemon.rs`.
//! - **Stuck, supervised.** The shard serving the parent's client is wedged at `t0` (one poll asleep for good), read
//!   inside the poll from the host's monotonic clock, which every process on the host shares. The watchdog first sees
//!   it still at its next look `a1`, within a beat and that look's lateness, and withholds the beat from its first look
//!   a whole window after `a1`, so the last beat comes within `W + B + late(a1)` of `t0` (`slates_server::watchdog`'s
//!   `Lateness`, which the wedged line names). The anchor kills once the beat is older than the window and its own
//!   lateness p99, at the look its own lateness lets it make; its kill line logs that age. So the client's liveness
//!   answers the daemon gone within `t0 + 2·W + B + L + kill`: `W` the liveness budget, `B` the beat, `L` the lateness
//!   of the two judges — the watchdog's first look and the anchor's age past the window (its p99 and its kill look's
//!   overshoot) — each read from its own line, and `kill` the time from the kill line to the client seeing the daemon
//!   gone, measured. Without the watchdog's term (`L` the anchor's p99 alone, the plan's first wording), the bound failed
//!   on a loaded Mac by 65.7 ms, the watchdog's looks 724 ms late in all (2026-11-13). The call itself then reconnects
//!   to the daemon the anchor restarts and is answered under its own id.
//! - **Starved, supervised** (heavy, gated): the anchor and its daemon held off their CPUs — on Linux CI in a cgroup of
//!   `cpu.weight` 1 beside default-weight spinners (`.github/diag-a232-step5.sh` moves them), elsewhere at nice 19 beside
//!   two spinners a CPU — while the parent calls for a span of windows. No call may fail, and the anchor may kill
//!   nothing: a starved daemon is alive.
// Unix only (`crate`'s declaration says why); the module is `cfg(test)` there, so clippy reads it as test code.
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use slates_client::driver::{Driver, Event};
use slates_client::{Client, ClientError, CreateSpec, Deadlines};
use slates_ipc::protocol::{NamePolicy, SizeClass};
use slates_server::Daemon;
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;

use crate::args::ProcessOptions;

/// Format: the environment variables that select the anchor role, name the instance, and say what the first daemon
/// holds (`wedged`, or `none` to serve).
const ROLE: &str = "SLATES_A232_ROLE";
const ANCHOR_ROLE: &str = "anchor";
const INSTANCE: &str = "SLATES_A232_INSTANCE";
const HOLD: &str = "SLATES_A232_HOLD";
const WEDGED: &str = "wedged";
/// Format: the daemon role's test path, which the anchor re-invokes this binary with.
const DAEMON_ROLE: &str = "liveness_tests::a_daemon_role";
/// Format: the roles' tags, looked for anywhere in a line (libtest's banner is glued to a role's first line).
const DAEMON_READY: &str = "A232-DAEMON-READY";
const HELD_TAG: &str = "A232-HELD";
/// Format: the words of the lines read, and the words that carry their numbers.
const PID_WORD: &str = "pid=";
const RESTARTS_WORD: &str = "restarts=";
const AT_WORD: &str = "at_ns=";
const LAPSED_WORDS: &str = "slates anchor: the daemon's heartbeat lapsed (";
const P99_WORDS: &str = "lateness p99 of ";
const RESTARTED_WORDS: &str = "slates anchor: daemon exited (";
const PROCESS_STUCK_WORDS: &str = "; its process ";
const SPARED_WORDS: &str = "so its process is judged in its own CPU time";
const WITHHELD_WORDS: &str = "the watchdog withholds the beat";
const LATE_WORDS: &str = "the looks since it last moved ran ";
const FIRST_LATE_WORDS: &str = "the first of them ";
const AGE_WORDS: &str = "heartbeat lapsed (";
const STARVED_WORDS: &str = "is starved";
/// Format: the environment variables that run the starved case, and that name the cgroup a caller moves the anchor and
/// its daemon into (Linux CI); without the second, the test lowers their priority itself.
const STARVE_ENV: &str = "SLATES_A232_STARVE";
const STARVED_CGROUP_ENV: &str = "SLATES_A232_STARVED_CGROUP";

/// Shape: shards in the daemon: two, so the held shard is one of several.
const TEST_SHARDS: u16 = 2;
/// Shape: the window and the beat.
const WINDOW: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
const BEAT: Duration = Duration::from_nanos(HEARTBEAT_NS);
/// Shape: how long the anchor and its daemon may take to start and a client to connect: twenty windows, a busy
/// machine's start.
const START_WAIT: Duration = Duration::from_nanos(20 * LIVENESS_BUDGET_NS);
/// Shape: the loop's pause between steps: a hundredth of a beat, so the client's view of the daemon is read within it.
const STEP_PAUSE: Duration = Duration::from_nanos(HEARTBEAT_NS / 100);
/// Shape: how long the starved case calls for: forty-five windows, the span A-232's red ran (45 s), many chances for a
/// starved shard to miss a reply deadline.
const CALLING: Duration = Duration::from_nanos(45 * LIVENESS_BUDGET_NS);
/// Shape: the test's own bound on the stuck exchange, in windows past the reply deadline and the reconnect budget:
/// ten windows past the rule's two, so an anchor that never ends the daemon fails here with the call still pending.
const GIVE_UP_WINDOWS: u32 = 12;
/// Shape: the starved case's calls come in turns of this many kinds (a synchronous `list`, a synchronous `status`, an
/// async `list`).
const CALL_KINDS: u64 = 3;
/// Shape: spinners per logical CPU beside a starved daemon when the test starves it itself (the busy-machine bar's two).
const SPINNERS_PER_CPU: usize = 2;
/// Shape: the nice value a starved daemon is lowered to, the lowest a process may give itself.
const LOWEST_NICE: i32 = 19;

/// The product's own deadlines.
fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

/// The options both roles run under: the quick profile and [`TEST_SHARDS`] shards.
fn options(instance: String) -> ProcessOptions {
  ProcessOptions {
    instance,
    quick: true,
    shards: Some(TEST_SHARDS),
    fleet: None,
  }
}

fn pause(span: Duration) {
  // The harness paces its loop; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(span);
}

/// The anchor role: `slates anchor`'s own supervision over this binary's [`a_daemon_role`], until a stop is requested.
#[test]
#[ignore = "a role: run by this module's tests in a child process"]
fn an_anchor_role() {
  if std::env::var(ROLE).ok().as_deref() != Some(ANCHOR_ROLE)
    || std::env::var_os(slates_anchor::supervise::ENV_ANCHOR_PID).is_some()
  {
    return;
  }
  let instance = std::env::var(INSTANCE).unwrap();
  let daemon_args: Vec<String> = ["--ignored", "--exact", DAEMON_ROLE, "--nocapture"]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
  crate::anchor::supervise(&options(instance), &daemon_args).unwrap();
}

/// The daemon role, started by the anchor role: the first daemon (no restart before it) makes the local group and, when
/// told to, wedges the shard serving the client whose id comes on standard input; every daemon then serves until it is
/// stopped or killed.
#[test]
#[ignore = "a role: run by this module's anchor role"]
fn a_daemon_role() {
  if std::env::var_os(slates_anchor::supervise::ENV_ANCHOR_PID).is_none() {
    return;
  }
  let Ok(instance) = std::env::var(INSTANCE) else {
    return;
  };
  let started = crate::daemon::start(&options(instance)).unwrap();
  let restarts = started
    .daemon
    .segment()
    .supervision()
    .map(|supervision| supervision.restarts())
    .unwrap();
  if restarts == 0 {
    started
      .daemon
      .bootstrap(true)
      .expect("the fixture explicitly creates its local consensus group");
  }
  println!(
    "{DAEMON_READY} {PID_WORD}{} {RESTARTS_WORD}{restarts}",
    std::process::id()
  );
  if restarts == 0 && std::env::var(HOLD).ok().as_deref() == Some(WEDGED) {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let client_id: u32 = line.trim().parse().unwrap();
    let daemon = &started.daemon;
    wedge_for_good(daemon, serving(daemon, client_id));
  }
  crate::daemon::serve(started).unwrap();
}

/// Which of `daemon`'s shards serves the client `client_id`, once its connection is seated.
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

/// When the held poll last read the host's monotonic clock, right before it blocks: zero until then. Written once by the
/// held shard, read by the thread that reports it.
static HELD_AT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Wedges `daemon`'s shard at `index` in one poll for good — asleep on a channel nobody sends on — and says so on
/// standard output, with the host's monotonic clock read inside the poll as the last thing before it blocks. The line is
/// printed by another thread: a shard that wrote it itself could lose its CPU after the write and block a scheduling
/// delay after the time it named, which on a loaded Mac put the shard's last movement 51-55 ms after `t0` in 2 of 3
/// runs (2026-11-13).
fn wedge_for_good(daemon: &Daemon, index: usize) {
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
      let _ = rustix::io::read(&never, &mut byte);
    })
    .unwrap();
  std::thread::spawn(move || {
    let _ = observation.wait();
  });
  loop {
    let at = HELD_AT_NS.load(std::sync::atomic::Ordering::Acquire);
    if at != 0 {
      println!("{HELD_TAG} shard={index} {AT_WORD}{at}");
      return;
    }
    pause(STEP_PAUSE);
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

/// The anchor role, stopped and reaped on drop: asked to stop first (its stop stops its daemon and unlinks what it
/// named), killed if it has not ended within a window and the recovery budget, so a failed assertion leaves no daemon
/// behind.
struct AnchorRole {
  child: Child,
  stdin: Option<ChildStdin>,
  stdout: Receiver<(u64, String)>,
  stderr: Receiver<(u64, String)>,
}

impl AnchorRole {
  fn start(instance: &str, hold: &str) -> AnchorRole {
    let mut child = Command::new(std::env::current_exe().unwrap())
      .args([
        "--ignored",
        "--exact",
        "liveness_tests::an_anchor_role",
        "--nocapture",
      ])
      .env(ROLE, ANCHOR_ROLE)
      .env(INSTANCE, instance)
      .env(HOLD, hold)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let stdin = child.stdin.take();
    let stdout = lines_of(child.stdout.take().unwrap());
    let stderr = lines_of(child.stderr.take().unwrap());
    AnchorRole {
      child,
      stdin,
      stdout,
      stderr,
    }
  }

  /// Asks the anchor to stop and waits for its exit, within a window and the recovery budget; whether it ended so.
  fn stop(&mut self) -> bool {
    if let Some(pid) = rustix::process::Pid::from_raw(i32::try_from(self.child.id()).unwrap()) {
      let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
    let bound = WINDOW + Duration::from_nanos(slates_db::replay::RECOVERY_BUDGET_NS);
    let asked = Instant::now();
    while asked.elapsed() < bound {
      if matches!(self.child.try_wait(), Ok(Some(_))) {
        return true;
      }
      pause(STEP_PAUSE);
    }
    false
  }
}

impl Drop for AnchorRole {
  fn drop(&mut self) {
    if matches!(self.child.try_wait(), Ok(None)) && !self.stop() {
      let _ = self.child.kill();
    }
    let _ = self.child.wait();
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

/// The number after `word` in `line`.
fn number_after(line: &str, word: &str) -> u64 {
  line
    .split(word)
    .nth(1)
    .and_then(|rest| {
      rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|number| number.parse().ok())
    })
    .unwrap_or_else(|| panic!("the line names a number after {word:?}: {line}"))
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
      Err(refused) => panic!("the anchored daemon refused a client: {refused}"),
    }
  }
}

/// Every line `lines` holds now and brings until a beat passes with none.
fn drain(lines: &Receiver<(u64, String)>) -> Vec<(u64, String)> {
  let mut all = Vec::new();
  while let Ok(line) = lines.recv_timeout(BEAT) {
    all.push(line);
  }
  all
}

/// Drives `driver` as an SDK's loop does until the call `ticket` ends or `give_up` passes: how it ended, when (the host's
/// monotonic clock), and when the client's liveness first answered the daemon gone.
fn drive(
  client: &mut Client,
  driver: &mut Driver,
  ticket: u64,
  give_up: Duration,
) -> (Option<Result<(), ClientError>>, u64, Option<u64>) {
  let began = Instant::now();
  let mut gone_at_ns = None;
  let mut next_tick = Instant::now();
  while began.elapsed() < give_up {
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
      return (
        Some(outcome),
        slates_machine::clock::monotonic_ns(),
        gone_at_ns,
      );
    }
    if gone_at_ns.is_none() && client.daemon_gone() {
      gone_at_ns = Some(slates_machine::clock::monotonic_ns());
    }
    next_tick = Instant::now()
      + Duration::from_nanos(driver.next_wake_ns(client).unwrap_or(deadlines().reply_ns));
    pause(STEP_PAUSE);
  }
  (None, slates_machine::clock::monotonic_ns(), gone_at_ns)
}

/// `after - bound` in signed nanoseconds: at most zero when `after` is within `bound`.
fn margin_ns(after: Duration, bound: Duration) -> i128 {
  i128::try_from(after.as_nanos()).unwrap() - i128::try_from(bound.as_nanos()).unwrap()
}

/// A-232 step 5, §4.7, §4.14. Do: under the anchor that ships ([`crate::anchor::supervise`]), wedge the shard that
/// serves a client at `t0` (one poll asleep for good) and keep a call of that client outstanding. Expect: the
/// watchdog's wedged line says it withholds the beat; the anchor's kill line comes within `2·W + B + L` of `t0`, `L` the
/// watchdog's first look's lateness and the anchor's logged age past the window (the module doc); so the client's
/// liveness answers the daemon gone within `t0 + 2·W + B + L + kill`, `kill` the measured time from the kill line to
/// it; and the call is then answered by the daemon the anchor restarts,
/// after one reconnect, within the reply deadline and the reconnect budget of the daemon's going. Before A-232 the
/// call failed `Stalled` at its first deadline, a second after `t0`, while the daemon still lived.
#[test]
fn an_anchored_daemons_wedged_shard_is_ended_within_the_liveness_bound_and_its_call_answered_after()
{
  let instance = format!("a232-stuck-{}", std::process::id());
  let mut anchor = AnchorRole::start(&instance, WEDGED);
  await_tag(&anchor.stdout, DAEMON_READY, START_WAIT);
  let mut client = connect(&instance);
  let stdin = anchor.stdin.as_mut().unwrap();
  writeln!(stdin, "{}", client.client_id()).unwrap();
  stdin.flush().unwrap();
  let t0_ns = number_after(&await_tag(&anchor.stdout, HELD_TAG, START_WAIT), AT_WORD);
  let mut driver = Driver::new(&client);
  let ticket = driver
    .submit(
      &mut client,
      Box::new(|client: &mut Client| client.list_begin()),
    )
    .unwrap();
  let deadlines = deadlines();
  let give_up =
    GIVE_UP_WINDOWS * WINDOW + Duration::from_nanos(deadlines.reply_ns + deadlines.reconnect_ns);
  let (call, call_end_ns, gone_at_ns) = drive(&mut client, &mut driver, ticket, give_up);
  let lines = drain(&anchor.stderr);
  let found = |words: &str| lines.iter().find(|(_, line)| line.contains(words)).cloned();
  let withheld = lines
    .iter()
    .find(|(_, line)| line.contains("is wedged") && line.contains(WITHHELD_WORDS))
    .cloned();
  let (killed_read_ns, killed) = found(LAPSED_WORDS).unwrap_or_else(|| {
    panic!(
      "the anchor never found the beat lapsed within {give_up:?}; the call: {call:?} (None: still pending); the lines: \
       {lines:?}"
    )
  });
  let restarted = found(RESTARTED_WORDS);
  let gone_ns = gone_at_ns.unwrap_or_else(|| {
    panic!("the client's liveness never answered the daemon gone; the call: {call:?}")
  });
  let withheld = withheld.expect("the watchdog said it withholds the beat for the wedged shard");
  let p99 = Duration::from_millis(number_after(&killed, P99_WORDS));
  // The age the kill line logs, in whole milliseconds rounded down; one more bounds it from above.
  let age = Duration::from_millis(number_after(&killed, AGE_WORDS) + 1);
  let watchdog_first = Duration::from_micros(number_after(&withheld.1, FIRST_LATE_WORDS));
  // Where the verdict and the shard's last movement fall after t0: the verdict's line read, and the span it names
  // before it (`for N ms`, in whole milliseconds), as the watchdog saw them.
  let verdict_after = Duration::from_nanos(withheld.0.saturating_sub(t0_ns));
  let unmoved = Duration::from_millis(number_after(&withheld.1, "a CPU for "));
  let moved_after = verdict_after.checked_sub(unmoved);
  let late = watchdog_first + age.saturating_sub(WINDOW);
  let rule = 2 * WINDOW + BEAT + late;
  let plan_rule = 2 * WINDOW + BEAT + p99;
  let killed_after = Duration::from_nanos(killed_read_ns.saturating_sub(t0_ns));
  let gone_after = Duration::from_nanos(gone_ns.saturating_sub(t0_ns));
  let kill = Duration::from_nanos(gone_ns.saturating_sub(killed_read_ns));
  let reaped = restarted
    .as_ref()
    .map(|(at, _)| Duration::from_nanos(at.saturating_sub(killed_read_ns)));
  let call_after = Duration::from_nanos(call_end_ns.saturating_sub(gone_ns));
  let call_bound = Duration::from_nanos(deadlines.reply_ns + deadlines.reconnect_ns);
  println!(
    "A232-SUPERVISED: W = {WINDOW:?}, B = {BEAT:?}, L = {late:?} (the watchdog's first look {watchdog_first:?}, the \
     anchor's age past the window {:?}, of it the p99 {p99:?}), so 2W + B + L = {rule:?}; the kill line read \
     {killed_after:?} after t0 (margin {} ns; against the plan's 2W + B + p99 = {plan_rule:?}, {} ns); kill = {kill:?} \
     (to the client's liveness; to the reap {reaped:?}); the verdict read {verdict_after:?} after t0, the shard last \
     moved {moved_after:?} after t0 by it; DaemonGone {gone_after:?} after t0, minus (2W + B + L + kill) \
     = {} ns, minus (2W + B + p99 + kill) = {} ns; the call ended {call:?} {call_after:?} after it, bound \
     {call_bound:?}, after {} reconnect(s); the watchdog: {}; the anchor: {killed}",
    age.saturating_sub(WINDOW),
    margin_ns(killed_after, rule),
    margin_ns(killed_after, plan_rule),
    margin_ns(gone_after, rule + kill),
    margin_ns(gone_after, plan_rule + kill),
    client.reconnects(),
    withheld.1,
  );
  assert!(
    withheld.1.contains(LATE_WORDS),
    "the withheld line names its looks' lateness: {}",
    withheld.1
  );
  assert!(
    killed_after <= rule,
    "the anchor's kill line came {killed_after:?} after the wedge, past 2W + B + L = {rule:?}"
  );
  assert!(
    matches!(call, Some(Ok(()))),
    "the outstanding call was answered by the restarted daemon, not {call:?}"
  );
  assert!(
    client.reconnects() >= 1,
    "the call was answered after a reconnect"
  );
  assert!(
    call_after <= call_bound,
    "the call was answered {call_after:?} after the daemon went, past the reply deadline and the reconnect budget \
     {call_bound:?}"
  );
  assert!(anchor.stop(), "the anchor stopped when asked");
}

/// A-232, §4.14, A-228's "a frozen process is still killed". Do: under the anchor that ships, stop the daemon's whole
/// process (SIGSTOP) with a call of a client outstanding, so its watchdog stops looking as well as beating. Expect: the
/// anchor does not take the silent watchdog for a starved one: it judges the process by the start's rule (no CPU
/// progress, never runnable), kills it a window after the lapse, and says so ("; its process ... killing it"); the call
/// is then answered by the daemon it restarts, after one reconnect. The time from the stop to the kill is reported.
#[test]
fn an_anchored_daemon_stopped_whole_is_killed_by_its_process_judgment_and_its_call_answered_after()
{
  let instance = format!("a232-frozen-{}", std::process::id());
  let anchor = AnchorRole::start(&instance, "none");
  let daemon_pid = u32::try_from(number_after(
    &await_tag(&anchor.stdout, DAEMON_READY, START_WAIT),
    PID_WORD,
  ))
  .unwrap();
  let mut client = connect(&instance);
  // Stopped only once the anchor holds it as running, not starting: a start is judged by the start's rule, which kills
  // a stopped start too, and is not the path this tests (the first run here was killed as "the starting daemon"). The
  // watchdog beats within a beat of its start and the anchor sees it at its next look, a beat later; one answered call,
  // then a window, leaves ten beats of slack.
  client
    .list()
    .expect("the daemon answers before it is stopped");
  pause(WINDOW);
  let stopped_ns = slates_machine::clock::monotonic_ns();
  rustix::process::kill_process(
    rustix::process::Pid::from_raw(i32::try_from(daemon_pid).unwrap()).unwrap(),
    rustix::process::Signal::STOP,
  )
  .unwrap();
  let mut driver = Driver::new(&client);
  let ticket = driver
    .submit(
      &mut client,
      Box::new(|client: &mut Client| client.list_begin()),
    )
    .unwrap();
  let deadlines = deadlines();
  let give_up =
    GIVE_UP_WINDOWS * WINDOW + Duration::from_nanos(deadlines.reply_ns + deadlines.reconnect_ns);
  let (call, _, gone_at_ns) = drive(&mut client, &mut driver, ticket, give_up);
  let lines = drain(&anchor.stderr);
  let killed = lines
    .iter()
    .find(|(_, line)| line.contains(PROCESS_STUCK_WORDS))
    .cloned();
  let withheld_kill = lines.iter().any(|(_, line)| line.contains(LAPSED_WORDS));
  println!(
    "A232-FROZEN: the anchor's kill line {:?} after the stop; DaemonGone {:?} after it; the call ended {call:?} after {} \
     reconnect(s); the anchor: {:?}",
    killed
      .as_ref()
      .map(|(at, _)| Duration::from_nanos(at.saturating_sub(stopped_ns))),
    gone_at_ns.map(|at| Duration::from_nanos(at.saturating_sub(stopped_ns))),
    client.reconnects(),
    killed.as_ref().map(|(_, line)| line.as_str()),
  );
  assert!(
    killed.is_some() && !withheld_kill,
    "the anchor killed the stopped daemon by its process judgment, not as a withheld beat: {lines:?}"
  );
  assert!(
    matches!(call, Some(Ok(()))),
    "the outstanding call was answered by the restarted daemon, not {call:?}"
  );
  assert!(client.reconnects() >= 1, "after a reconnect");
}

/// Lowers every thread of the process `pid` to the lowest priority a process may give itself: on Linux a nice value is
/// per thread, so each thread is lowered; elsewhere the process's value is.
fn lower(pid: u32) {
  #[cfg(target_os = "linux")]
  {
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))
      .unwrap()
      .flatten()
    {
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
  #[cfg(not(target_os = "linux"))]
  rustix::process::setpriority_process(
    rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()),
    LOWEST_NICE,
  )
  .unwrap();
}

/// Waits until the processes `pids` sit in the cgroup `cgroup` (`/proc/<pid>/cgroup`'s v2 line), within `within`.
#[cfg(target_os = "linux")]
fn await_cgroup(pids: &[u32], cgroup: &str, within: Duration) {
  let started = Instant::now();
  let inside = |pid: &u32| {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
      .is_ok_and(|line| line.trim_end().ends_with(&format!(":{cgroup}")))
  };
  while !pids.iter().all(inside) {
    assert!(
      started.elapsed() < within,
      "the caller moved {pids:?} into {cgroup} within {within:?}"
    );
    pause(STEP_PAUSE);
  }
}

#[cfg(not(target_os = "linux"))]
fn await_cgroup(_pids: &[u32], cgroup: &str, _within: Duration) {
  panic!("{STARVED_CGROUP_ENV}={cgroup}: a cgroup is Linux's");
}

/// Spinners at this process's priority, [`SPINNERS_PER_CPU`] a logical CPU, until `running` is cleared.
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

/// One call of the starved case's turn `turn`: a synchronous `list`, a synchronous `status` of `volume`, or a `list`
/// through the async driver, in turn, so every wait a call makes is starved (A-232 step 3).
fn one_call(
  client: &mut Client,
  volume: slates_ipc::protocol::VolumeId,
  turn: u64,
) -> Result<(), ClientError> {
  match turn % CALL_KINDS {
    0 => client.list().map(|_| ()),
    1 => client.status(volume).map(|_| ()),
    _ => {
      let mut driver = Driver::new(client);
      let ticket = driver.submit(client, Box::new(|client: &mut Client| client.list_begin()))?;
      match drive(client, &mut driver, ticket, CALLING).0 {
        Some(outcome) => outcome,
        None => Err(ClientError::DaemonGone {
          after_ns: u64::try_from(CALLING.as_nanos()).unwrap_or(u64::MAX),
        }),
      }
    }
  }
}

/// A-232 step 5, §4.7, §4.14 (heavy, so run only with `SLATES_A232_STARVE=1`; it skips loudly otherwise). Do: under the
/// anchor that ships, with the anchor and its daemon held off their CPUs — moved by the caller into the cgroup
/// `SLATES_A232_STARVED_CGROUP` names (Linux CI: `cpu.weight` 1 beside default-weight spinners), or else lowered to nice
/// 19 beside two spinners a CPU here — call `list`, `status` and an async `list` in turn for forty-five windows.
/// Expect: every call answered; no reconnect; the anchor never finds the beat lapsed and restarts nothing; a starved
/// daemon is alive. Non-vacuous only if the daemon was starved: a call waited past the reply deadline, or the watchdog
/// said a shard was starved; a run that starved nothing fails. Before A-232 such calls failed `Stalled` in 10 of 10
/// runs on both Linux runners (`docs/bugs/2026-11-13-a-live-starved-daemons-call-was-failed-stalled.md`).
#[test]
fn an_anchored_daemon_starved_beside_spinners_fails_no_call_and_is_never_killed() {
  if std::env::var_os(STARVE_ENV).is_none() {
    println!(
      "SKIPPED: set {STARVE_ENV}=1 to starve an anchored daemon beside spinners (heavy; Linux CI sets \
       {STARVED_CGROUP_ENV} too)"
    );
    return;
  }
  let instance = format!("a232-starved-{}", std::process::id());
  let anchor = AnchorRole::start(&instance, "none");
  let daemon_pid = u32::try_from(number_after(
    &await_tag(&anchor.stdout, DAEMON_READY, START_WAIT),
    PID_WORD,
  ))
  .unwrap();
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: "starved".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .expect("the test's volume");
  let pids = [anchor.child.id(), daemon_pid];
  println!("A232-STARVE pids {} {}", pids[0], pids[1]);
  let running: &'static std::sync::atomic::AtomicBool =
    Box::leak(Box::new(std::sync::atomic::AtomicBool::new(true)));
  let mut spun = Vec::new();
  match std::env::var(STARVED_CGROUP_ENV) {
    Ok(cgroup) => await_cgroup(&pids, &cgroup, START_WAIT),
    Err(_) => {
      pids.iter().copied().for_each(lower);
      spun = spinners(running);
    }
  }
  let reply = Duration::from_nanos(deadlines().reply_ns);
  let began = Instant::now();
  let (mut calls, mut answered, mut longest) = (u64::MIN, u64::MIN, Duration::ZERO);
  let mut failed = Vec::new();
  while began.elapsed() < CALLING {
    let asked = Instant::now();
    let outcome = one_call(&mut client, volume, calls);
    longest = longest.max(asked.elapsed());
    calls += 1;
    match outcome {
      Ok(()) => answered += 1,
      Err(error) => {
        println!(
          "A232-FAILED call {calls} after {:?} at {:?}: {error}; daemon gone {}",
          asked.elapsed(),
          began.elapsed(),
          client.daemon_gone()
        );
        failed.push(error);
      }
    }
  }
  running.store(false, std::sync::atomic::Ordering::Relaxed);
  for spinner in spun {
    spinner.join().unwrap();
  }
  println!(
    "A232-SUMMARY calls {calls} answered {answered} failed {} longest {longest:?} (reply deadline {reply:?}) \
     reconnects {}",
    failed.len(),
    client.reconnects()
  );
  let lines = drain(&anchor.stderr);
  let said = |words: &str| {
    lines
      .iter()
      .filter(|(_, line)| line.contains(words))
      .map(|(_, line)| line.clone())
      .collect::<Vec<_>>()
  };
  let (starved, lapsed, restarted, spared, process_stuck) = (
    said(STARVED_WORDS),
    said(LAPSED_WORDS),
    said(RESTARTED_WORDS),
    said(SPARED_WORDS),
    said(PROCESS_STUCK_WORDS),
  );
  println!(
    "A232-STARVED-LINES starved {} spared {} lapsed {lapsed:?} process-stuck {process_stuck:?} restarted {restarted:?}",
    starved.len(),
    spared.len()
  );
  assert!(
    failed.is_empty(),
    "every call to the starved daemon was answered: {failed:?}"
  );
  assert!(
    lapsed.is_empty()
      && process_stuck.is_empty()
      && restarted.is_empty()
      && client.reconnects() == 0,
    "the anchor ended a starved daemon: {lapsed:?} {process_stuck:?} {restarted:?}, {} reconnect(s)",
    client.reconnects()
  );
  assert!(
    longest > reply || !starved.is_empty(),
    "the run starved nothing (longest call {longest:?}, no starved verdict), so it proves nothing"
  );
}
