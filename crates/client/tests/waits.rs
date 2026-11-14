//! The synchronous client's waits (§4.7, A-232, A-56's every call ends): a call waits while its daemon lives, and ends
//! `DaemonGone` once its daemon is gone and no successor answers within the reconnect budget. Both waits are driven: the
//! reply wait of a round trip, and the wait for a slot on a full command ring before the request can go. A daemon held
//! off its CPU past several reply deadlines is the live case. A daemon killed in a child process (this test binary
//! re-invoked, an environment variable selecting the role) is the gone case, a real process death the client learns of
//! as it would in the field: the peer's close on Linux, the exit watch on macOS and Windows.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the probe budget of the quick profile these tests measure (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the part of the watchdog's window (`LIVENESS_BUDGET_NS`) a held daemon's shard is kept off the CPU: half.
/// The watchdog calls a shard stuck only after a whole window in which it did not move (A-228,
/// `crates/server/src/watchdog.rs`), so the daemon is alive by its own judgment throughout the hold.
const HOLD_PARTS_OF_WINDOW: u64 = 2;
/// Shape: the reply deadlines a hold spans: a held daemon's client waits a fifth of the hold for a reply before it asks
/// the daemon's liveness, so its call is overdue several times over before the daemon can answer.
const DEADLINES_HELD: u64 = 5;
/// Format: the environment variable selecting the child daemon's role, and the one naming its instance.
const ROLE: &str = "SLATES_WAITS_TEST_ROLE";
const ROLE_INSTANCE: &str = "SLATES_WAITS_TEST_INSTANCE";
/// Format: the tag on the line the child daemon prints once it serves (libtest prints its own banner first, and may
/// leave its test line unended, so the tag is looked for anywhere in a line).
const READY: &str = "waits-daemon-ready";

/// The machine profile this binary's daemons derive from, measured once per process (§4.1)
/// (`docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`).
fn profile() -> MachineProfile {
  static PROFILE: std::sync::OnceLock<Result<MachineProfile, slates_machine::MachineError>> =
    std::sync::OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures")
}

/// The product's own deadlines.
fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

/// How long a held daemon's shard is kept off the CPU.
fn hold_ns() -> u64 {
  LIVENESS_BUDGET_NS / HOLD_PARTS_OF_WINDOW
}

/// A held daemon's client's deadlines: the product's reconnect budget, and a reply deadline of [`DEADLINES_HELD`] parts
/// of the hold.
fn held_deadlines() -> Deadlines {
  Deadlines {
    reply_ns: hold_ns() / DEADLINES_HELD,
    reconnect_ns: deadlines().reconnect_ns,
  }
}

/// A one-shard daemon run alone: its one shard serves every client and is the control shard a hold wedges.
fn one_shard_daemon(instance: &str) -> Daemon {
  let profile = profile();
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, instance, Some(1)),
    SegmentSource::Create {
      name: format!("slates-seg-{instance}"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  daemon
}

/// A client of `instance` with `deadlines`, retrying the rendezvous while the daemon starts.
fn connect(instance: &str, deadlines: Deadlines) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// Fills `client`'s command ring while its daemon drains nothing: list calls begun until the client admits no more (its
/// outstanding bound is the ring's slots), then one abandoned and begun again to prove the ring itself full. Returns the
/// refusal that proof met: `RingFull` while the daemon lives, `ChannelLost` once it is gone.
fn fill_the_ring(client: &mut Client) -> ClientError {
  let mut begun = Vec::new();
  loop {
    match client.list_begin() {
      Ok(id) => begun.push(id.word()),
      Err(ClientError::TooManyOutstanding { .. }) => break,
      Err(e) => panic!("filling the ring: {e}"),
    }
  }
  assert_eq!(
    begun.len(),
    client.ring_slots(),
    "a ring's worth of calls begun"
  );
  let first = *begun.first().expect("a call was begun");
  assert!(client.abandon(first), "one begun call abandoned");
  client
    .list_begin()
    .expect_err("the ring is full: nothing more is sent")
}

/// Holds `daemon`'s shard off its CPU, runs `call` on `client` meanwhile, and checks the A-232 rule for a live daemon:
/// the call answered (never `Stalled`), after more than two reply deadlines (non-vacuous: it was overdue while the
/// daemon was held), with the daemon's watchdog looking during the hold and withholding no beat (alive by its own
/// judgment throughout).
fn answered_while_held(
  daemon: &Daemon,
  client: &mut Client,
  before: impl FnOnce(&mut Client),
  what: &str,
) {
  let reply = Duration::from_nanos(held_deadlines().reply_ns);
  let (looks_before, withheld_before) = daemon.watchdog_turns().unwrap();
  let hold = daemon
    .wedge_control_shard(hold_ns())
    .expect("the hold is admitted");
  let started = Instant::now();
  before(client);
  let listed = client.list();
  let took = started.elapsed();
  let held_for = Duration::from_nanos(hold.answer().expect("the hold ran"));
  assert!(
    listed.is_ok(),
    "{what} to the held daemon was answered: {listed:?} after {took:?}"
  );
  assert!(
    took > reply * 2 && held_for > reply * 2,
    "non-vacuous: {what} was overdue more than twice (answered after {took:?}, held {held_for:?}, reply deadline \
     {reply:?})"
  );
  let (looks, withheld) = daemon.watchdog_turns().unwrap();
  assert!(looks > looks_before, "the watchdog looked during the hold");
  assert_eq!(
    withheld, withheld_before,
    "the watchdog judged the held daemon alive throughout ({looks} looks)"
  );
}

/// A-232 (§4.7), the reply wait. Do: on a one-shard daemon, hold the shard off its CPU for [`hold_ns`]
/// (`Daemon::wedge_control_shard`: its thread asleep, so it drains nothing) while a client whose reply deadline is a
/// fifth of the hold lists the volumes. Expect: the list answered once the hold ends, past more than two reply
/// deadlines, the daemon alive by its watchdog throughout (see [`answered_while_held`]). Before A-232 the list ended
/// `Stalled` at its first reply deadline.
#[test]
fn a_round_trip_to_a_held_daemon_waits_and_is_answered() {
  let instance = format!("cl-waits-reply-{}", std::process::id());
  let daemon = one_shard_daemon(&instance);
  let mut client = connect(&instance, held_deadlines());
  answered_while_held(&daemon, &mut client, |_| {}, "a round trip");
  daemon.stop();
}

/// A-232 (§4.7), the wait for a slot. Do: on a one-shard daemon, hold the shard off its CPU for [`hold_ns`], fill a
/// client's command ring with list calls begun on the async path (proved full: a call begun once one is abandoned is
/// refused `RingFull`), then list the volumes synchronously, the client's reply deadline a fifth of the hold. Expect: the
/// list's request waits for a slot while the daemon lives and is answered once the hold ends, past more than two reply
/// deadlines, the daemon alive by its watchdog throughout. Before A-232 the send ended `Stalled` at one reply deadline.
#[test]
fn a_send_to_a_held_daemons_full_ring_waits_for_a_slot_and_is_answered() {
  let instance = format!("cl-waits-ring-{}", std::process::id());
  let daemon = one_shard_daemon(&instance);
  let mut client = connect(&instance, held_deadlines());
  answered_while_held(
    &daemon,
    &mut client,
    |client| {
      let refused = fill_the_ring(client);
      assert_eq!(
        refused,
        ClientError::RingFull,
        "the ring is full and the daemon lives"
      );
    },
    "a send on a full ring",
  );
  daemon.stop();
}

/// The child's role: a one-shard daemon run alone, serving until it is killed.
fn serve_until_killed() {
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let _daemon = one_shard_daemon(&instance);
  println!("{READY}");
  loop {
    std::thread::park();
  }
}

/// The child daemon, killed and reaped on drop so a failed assertion leaves no daemon behind.
struct ChildDaemon(Child);

impl ChildDaemon {
  /// Spawns this binary as the child daemon of `instance` and waits for its ready line.
  fn start(instance: &str) -> ChildDaemon {
    let mut child = ChildDaemon(
      Command::new(std::env::current_exe().unwrap())
        .args([
          "--exact",
          "a_call_to_a_killed_daemon_ends_daemon_gone_past_the_reconnect_budget",
          "--nocapture",
        ])
        .env(ROLE, "daemon")
        .env(ROLE_INSTANCE, instance)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let ready = BufReader::new(child.0.stdout.take().unwrap())
      .lines()
      .map(|line| line.expect("read the child daemon's report"))
      .any(|line| line.contains(READY));
    assert!(ready, "the child daemon reported it serves");
    child
  }

  /// Kills the daemon and reaps it.
  fn kill(&mut self) {
    self.0.kill().expect("kill the child daemon");
    self.0.wait().expect("reap the child daemon");
  }
}

impl Drop for ChildDaemon {
  fn drop(&mut self) {
    if self
      .0
      .try_wait()
      .expect("inspect the child daemon")
      .is_none()
    {
      self.kill();
    }
  }
}

/// Runs `call` and checks it ended `DaemonGone` past the reconnect budget: the reconnect's verdict, never a stall
/// (non-vacuous: it took at least the budget, so the client tried to reconnect for it), and within twice the reply and
/// reconnect budgets together.
fn gone_past_the_budget(
  call: impl FnOnce() -> Result<Vec<slates_client::VolumeSummary>, ClientError>,
  what: &str,
) {
  let budgets = deadlines();
  let started = Instant::now();
  let ended = call();
  let took = started.elapsed();
  assert!(
    matches!(ended, Err(ClientError::DaemonGone { .. })),
    "{what} to the killed daemon ended DaemonGone: {ended:?} after {took:?}"
  );
  let budget = Duration::from_nanos(budgets.reconnect_ns);
  let bound = Duration::from_nanos(budgets.reply_ns.saturating_add(budgets.reconnect_ns)) * 2;
  assert!(
    took >= budget && took <= bound,
    "{what} ended after {took:?}: at least the reconnect budget {budget:?}, within {bound:?}"
  );
}

/// A-232 (§4.7), a gone daemon. Do: run a one-shard daemon in a child process, connect two clients to it, `SIGKILL` the
/// child (`TerminateProcess` on Windows), then list the volumes on the first client and, on the second, fill its
/// command ring (proved full: refused `ChannelLost`, the daemon gone) and list them. Expect: both lists end
/// `DaemonGone` once the reconnect budget has passed with no daemon answering, never `Stalled` (see
/// [`gone_past_the_budget`]), as before A-232.
#[test]
fn a_call_to_a_killed_daemon_ends_daemon_gone_past_the_reconnect_budget() {
  if std::env::var_os(ROLE).is_some() {
    serve_until_killed();
    return;
  }
  let instance = format!("cl-waits-gone-{}", std::process::id());
  let mut child = ChildDaemon::start(&instance);
  let mut replying = connect(&instance, deadlines());
  let mut sending = connect(&instance, deadlines());
  replying.list().expect("the child daemon answers");
  sending.list().expect("the child daemon answers");
  child.kill();
  gone_past_the_budget(|| replying.list(), "a round trip");
  let refused = fill_the_ring(&mut sending);
  assert_eq!(
    refused,
    ClientError::ChannelLost,
    "the ring is full and the daemon gone"
  );
  gone_past_the_budget(|| sending.list(), "a send on a full ring");
}
