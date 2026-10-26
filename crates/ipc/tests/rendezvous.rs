//! The rendezvous across real processes (Phase 2 task 3; §4.7 "Rendezvous", §4.13): the test
//! binary re-invoked as the client connects to the daemon end running in the test process,
//! receives its region through the platform's handoff (an abstract socket with `SCM_RIGHTS`
//! on Linux; the bootstrap object with claim slots on macOS), sends a request, and gets the
//! reply; a client with no daemon is refused `DaemonUnavailable` and creates nothing.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use slates_ipc::region::{ClientRegion, RegionGeometry};
use slates_ipc::rendezvous::{Adoption, ENV_RENDEZVOUS, HeldRendezvous};
use slates_ipc::slot::Slot;
use slates_ipc::{
  CLAIM_WAIT_NS, ClientEnd, DaemonEnd, IpcError, Listener, Prepared, begin_connect_as, connect,
};

/// The regions this test process has made, which numbers the next one's name.
static REGIONS_MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Format: the environment variable that turns this binary into the client.
const CHILD_ROLE: &str = "SLATES_IPC_TEST_CLIENT";
/// Format: the environment variable naming the instance a re-invoked child connects to expecting the
/// typed capacity refusal.
const CHILD_REFUSED_ROLE: &str = "SLATES_RENDEZVOUS_REFUSED_CHILD";
/// Format: the environment variable naming the instance whose held rendezvous a re-invoked child restarts a daemon
/// under, with a claim crossing the restart.
const CHILD_RESTART_ROLE: &str = "SLATES_RENDEZVOUS_RESTART_CHILD";
/// Shape: how long the daemon side serves before giving up (milliseconds).
const SERVE_MS: u64 = 5_000;
/// Shape: slots per ring here.
const SLOTS: u32 = 16;

fn geometry() -> RegionGeometry {
  RegionGeometry {
    slots: SLOTS,
    spin_ns: 100_000,
    spin_shift: 4,
    bulk_bytes: 4096,
    page: 4096,
  }
}

/// The client: connects to the instance in the environment, sends one request, expects the
/// reply to be the payload reversed, exits 0. Passes trivially when not the child.
#[test]
fn rendezvous_client() {
  let Ok(instance) = std::env::var(CHILD_ROLE) else {
    return;
  };
  let connected = connect(&instance).unwrap();
  let mut client = ClientEnd::connected(connected);
  client
    .send(&Slot::inline(0x0007_0000_0000_0001, b"hello").unwrap())
    .unwrap();
  let reply = client.wait(Some(2_000_000_000)).unwrap();
  assert_eq!(reply.payload, b"olleh");
  assert_eq!(reply.request, 0x0007_0000_0000_0001);
  std::process::exit(0);
}

/// Shape: the client bound the refusing daemon of the test below reports; any value the child can check
/// for, distinct from the ids and lengths that flow through a served handoff.
const REFUSED_LIMIT: usize = 3;

/// The client refused at the bound: connects to the instance in the environment and expects the typed
/// refusal `TooManyClients { limit: REFUSED_LIMIT }` — where before it read a short handoff (Linux) or
/// waited out its claim (macOS, Windows). Exits 0 only on the typed refusal. Passes trivially when not
/// the child.
#[test]
fn rendezvous_client_refused() {
  let Ok(instance) = std::env::var(CHILD_REFUSED_ROLE) else {
    return;
  };
  let started = Instant::now();
  let outcome = connect(&instance).map(|_| ());
  assert!(
    matches!(outcome, Err(IpcError::TooManyClients { limit }) if limit == REFUSED_LIMIT),
    "the connect is refused typed with the bound: {outcome:?} after {:?}",
    started.elapsed()
  );
  std::process::exit(0);
}

/// AC-2.6 (admission refused typed), across real processes: the daemon end refuses the child's connect at
/// its client bound — `make_region` answers `TooManyClients` — and the child receives that refusal typed,
/// with the bound, instead of an uninterpretable handoff or a claim that times out. Do: the listener
/// refuses every region; the child connects. Expect: the child exits 0 (it saw `TooManyClients { limit }`
/// with the listener's bound), the listener counts one capacity refusal and no cross-uid one, and the
/// round ends in well under the claim wait (a reply, not a timeout).
#[test]
fn a_client_process_refused_at_the_bound_is_told_so_typed() {
  let instance = format!("test-refused-{}", std::process::id());
  let mut listener = Listener::open(&instance, None).unwrap();
  let exe = std::env::current_exe().unwrap();
  let mut child = Command::new(exe)
    .args(["--exact", "rendezvous_client_refused", "--nocapture"])
    .env(CHILD_REFUSED_ROLE, &instance)
    .spawn()
    .unwrap();
  let started = Instant::now();
  while started.elapsed() < Duration::from_millis(SERVE_MS) && listener.capacity_refused() == 0 {
    let round = listener.accept_pending(
      &|_| false,
      &mut |_client_id| {
        Err(IpcError::TooManyClients {
          limit: REFUSED_LIMIT,
        })
      },
      &mut |refusal| panic!("only the bound refuses here, and it is counted: {refusal}"),
    );
    assert!(round.ended.is_none(), "{:?}", round.ended);
    std::hint::spin_loop();
  }
  let status = child.wait().unwrap();
  assert!(
    status.success(),
    "the child saw the typed refusal: {status:?}"
  );
  assert_eq!(
    listener.capacity_refused(),
    1,
    "one connect refused at the bound"
  );
  assert_eq!(listener.refused(), 0, "no cross-uid refusal");
}

/// The daemon end accepts the child through the real rendezvous, hands it a region, serves
/// its request, and the child exits 0; no cross-uid refusals were counted.
#[test]
fn a_client_process_connects_and_completes_a_round_trip() {
  let instance = format!("test-{}", std::process::id());
  let mut listener = Listener::open(&instance, None).unwrap();
  let exe = std::env::current_exe().unwrap();
  let mut child = Command::new(exe)
    .args(["--exact", "rendezvous_client", "--nocapture"])
    .env(CHILD_ROLE, &instance)
    .spawn()
    .unwrap();
  let started = Instant::now();
  let mut ends: Vec<DaemonEnd> = Vec::new();
  let mut served = false;
  let pid = std::process::id();
  while started.elapsed() < Duration::from_millis(SERVE_MS) && !served {
    let round = listener.accept_pending(
      &|_| false,
      &mut |client_id| {
        Ok(Prepared {
          region: ClientRegion::create(
            &format!("slates-cr-{pid}-{client_id}"),
            client_id,
            0,
            geometry(),
          )?,
          kick_fd: None,
        })
      },
      &mut |refusal| panic!("the one client is served: {refusal}"),
    );
    assert!(round.ended.is_none(), "{:?}", round.ended);
    for a in round.accepted {
      assert_eq!(a.client_id, 1);
      ends.push(DaemonEnd::new(a.region));
    }
    for end in &mut ends {
      if let Some(req) = end.try_take().unwrap() {
        let mut reply = req.payload.clone();
        reply.reverse();
        end
          .reply(&Slot::inline(req.request, &reply).unwrap())
          .unwrap();
        served = true;
      }
    }
    std::hint::spin_loop();
  }
  assert!(served, "the client's request arrived");
  let status = child.wait().unwrap();
  assert!(status.success(), "the client saw its reply: {status:?}");
  assert_eq!(listener.refused(), 0);
}

/// No daemon: the connect is refused with the endpoint named, quickly, and nothing is left
/// behind.
#[test]
fn no_daemon_means_daemon_unavailable() {
  let started = Instant::now();
  let refused = connect("slates-test-no-such-daemon").err().unwrap();
  assert!(
    matches!(refused, IpcError::DaemonUnavailable { .. }),
    "{refused:?}"
  );
  assert!(started.elapsed() < Duration::from_secs(2));
}

/// The longest poll seen, and whether the scheduler took the thread away during **that** poll. The bound
/// below judges a poll by the clock; this attributes it, so a machine that preempted the thread is not read
/// as a claim that waited (measured 2026-10-18: a 129 ms poll against the 100 ms bound under a full
/// `cargo test --workspace`, where every poll of the run had been preempted several times).
#[derive(Clone, Copy, Default)]
struct Longest {
  took: Duration,
  /// `None` where the platform keeps no such counter (Windows), which is not the same as zero: the bound
  /// cannot be attributed there and so is not enforced, loudly.
  switches: Option<u64>,
}

impl Longest {
  fn note(&mut self, took: Duration, switches: Option<u64>) {
    if took > self.took {
      self.took = took;
      self.switches = switches;
    }
  }
}

/// Derived: the longest one poll of a claim may take — a tenth of the claim wait. A poll that waited for
/// the daemon would take the whole claim wait; one that only reads the answer's state takes microseconds,
/// so the tenth separates the two with room for scheduling noise.
fn poll_bound() -> Duration {
  Duration::from_nanos(CLAIM_WAIT_NS / 10)
}

/// AUD-29-19 (§4.7 "Rendezvous", R6): a connect never holds its caller. Do: open a daemon end that does
/// not accept, begin a claim on it and poll until it ends, timing every poll; then begin a second claim
/// and let the daemon end accept it while the claim is polled. Expect: every poll returns at once (within
/// [`poll_bound`]) and the claim is polled more than once (non-vacuous: its answer was outstanding); the
/// unanswered claim refused `DaemonUnavailable` no sooner than the claim wait; the second claim connected
/// once the daemon answered it. Before this change the connect waited for the answer inside one call — up
/// to the claim wait on macOS and Windows, and without any bound on Linux.
#[test]
fn a_claim_is_polled_without_waiting_and_refused_at_the_claim_wait() {
  let instance = format!("test-claim-{}", std::process::id());
  let mut listener = Listener::open(&instance, None).unwrap();
  let started = Instant::now();
  let (refusal, polls, longest) =
    poll_until_refused(begin_connect_as(&instance, 0, slates_ipc::CLAIM_WAIT_NS).unwrap());
  assert!(
    matches!(refusal, IpcError::DaemonUnavailable { .. }),
    "{refusal:?}"
  );
  assert!(
    started.elapsed() >= Duration::from_nanos(CLAIM_WAIT_NS),
    "refused only at the claim wait"
  );
  assert!(polls > 1, "non-vacuous: the answer was outstanding");
  // The unanswered claim may still sit in the daemon's queue (Linux: a closed connection in the
  // backlog); the daemon end drains it, and the second claim is answered.
  let (connected, served_longest) = poll_while_serving(&mut listener, &instance);
  assert!(connected.region.client_id() > 0);
  for longest in [longest, served_longest] {
    println!(
      "claim poll: longest {:?}, bound {:?}, involuntary switches during it {:?}",
      longest.took,
      poll_bound(),
      longest.switches
    );
    // Over the bound is the claim's doing only if the scheduler did not take the thread away during that
    // poll. A poll that merely read the answer's state and was then preempted is not a poll that waited.
    match longest.switches {
      Some(switches) => assert!(
        longest.took < poll_bound() || switches > 0,
        "the longest poll was {:?}, bound {:?}, and the scheduler took the thread away {switches} times \
         during it (zero: the claim waited)",
        longest.took,
        poll_bound()
      ),
      // No counter here, so a long poll cannot be told from a preempted one. Say so and do not assert a
      // bound this platform gives no way to defend.
      None => println!(
        "claim poll: the bound was not enforced — this platform has no involuntary-switch counter, so a \
         {:?} poll cannot be attributed to the claim rather than to the scheduler",
        longest.took
      ),
    }
  }
}

/// Polls `claim` until it ends, which must be a refusal: the refusal, the polls made, the longest poll.
fn poll_until_refused(mut claim: slates_ipc::Claim) -> (IpcError, u64, Longest) {
  let mut polls = 0u64;
  let mut longest = Longest::default();
  loop {
    let at = Instant::now();
    let switches = slates_machine::bench::involuntary_switches();
    let polled = claim.poll();
    longest.note(
      at.elapsed(),
      slates_machine::bench::involuntary_switches_since(switches),
    );
    polls += 1;
    match polled {
      Ok(None) => std::hint::spin_loop(),
      Ok(Some(_)) => panic!("no daemon answered the claim"),
      Err(refusal) => return (refusal, polls, longest),
    }
  }
}

/// Begins a claim and polls it while the daemon end serves, until it connects within the claim wait: the
/// connection and the longest poll.
fn poll_while_serving(listener: &mut Listener, instance: &str) -> (slates_ipc::Connected, Longest) {
  let claim = begin_connect_as(instance, 0, slates_ipc::CLAIM_WAIT_NS).unwrap();
  let (connected, longest, accepted) = serve_until_connected(listener, claim);
  // The daemon's ends go with the test; the client's mapping keeps the region it opened.
  drop(accepted);
  (connected, longest)
}

/// Polls `claim`, already begun, while the daemon end `listener` serves, until it connects within the claim wait:
/// the connection, the longest poll, and the daemon's side of each claim it accepted. The caller keeps that side for
/// as long as the daemon is meant to live: on Linux the client's liveness is the peer of its control socket, which
/// closes with it.
fn serve_until_connected(
  listener: &mut Listener,
  mut claim: slates_ipc::Claim,
) -> (slates_ipc::Connected, Longest, Vec<slates_ipc::Accepted>) {
  let pid = std::process::id();
  let started = Instant::now();
  let mut longest = Longest::default();
  // The daemon's side of each accepted claim, kept: its region lives while the daemon holds it.
  let mut accepted = Vec::new();
  loop {
    // A claim refused or a round ended early shows as the claim never connecting, which the bound below judges.
    let round = listener.accept_pending(
      &|_| false,
      &mut |client_id| {
        // Named once per region, as the daemon names them (A-176): a client still holding a dead listener's region
        // keeps that name alive on Windows, and a fresh listener counts its client ids from one again.
        let region = REGIONS_MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Prepared {
          region: ClientRegion::create(
            &format!("slates-claim-{pid}-{region}"),
            client_id,
            0,
            geometry(),
          )?,
          kick_fd: None,
        })
      },
      &mut |_refusal| {},
    );
    accepted.extend(round.accepted);
    let at = Instant::now();
    let switches = slates_machine::bench::involuntary_switches();
    let polled = claim.poll().unwrap();
    longest.note(
      at.elapsed(),
      slates_machine::bench::involuntary_switches_since(switches),
    );
    if let Some(connected) = polled {
      return (connected, longest, accepted);
    }
    assert!(
      started.elapsed() < Duration::from_nanos(CLAIM_WAIT_NS),
      "the answered claim connects within the claim wait"
    );
  }
}

/// AUD-29-20 (found on CI 2026-10-01: a Node SDK recovery failed every call `OsRefused { call: "recvmsg",
/// code: Some(104) }`). Do: begin a claim on a daemon end, then close the daemon end before it answers — as a
/// daemon killed with the claim in its backlog — and poll the claim until it ends. Expect: `DaemonUnavailable`,
/// which a reconnect retries, never an `OsRefused` that ends every call. Linux resets the queued connection
/// (`ECONNRESET`); macOS and Windows leave the claim unanswered until the claim wait.
#[test]
fn a_claim_whose_daemon_died_before_answering_is_unavailable_not_refused() {
  let instance = format!("test-claim-reset-{}", std::process::id());
  let listener = Listener::open(&instance, None).unwrap();
  let claim = begin_connect_as(&instance, 0, slates_ipc::CLAIM_WAIT_NS).unwrap();
  drop(listener);
  let (refusal, _, _) = poll_until_refused(claim);
  assert!(
    matches!(refusal, IpcError::DaemonUnavailable { .. }),
    "{refusal:?}"
  );
}

/// The two daemons and the client of the restart test below, in one process under the parent's held rendezvous.
/// Daemon A opens it; the client's claim begins while A is published; A is dropped unanswered; daemon B opens it and
/// answers the claim. Exits 0 when the client judges B, the daemon that answered it, alive. Passes trivially when not
/// the child.
#[test]
fn rendezvous_restart_child() {
  let Ok(instance) = std::env::var(CHILD_RESTART_ROLE) else {
    return;
  };
  // The parent's held rendezvous, handed across the spawn as an anchor hands it to the daemons it starts.
  let adoption = Adoption::from_env().unwrap();
  assert!(adoption.is_some(), "the parent hands its held rendezvous");
  let first = Listener::open(&instance, adoption.as_ref()).unwrap();
  let first_opened = SystemTime::now();
  let claim = begin_connect_as(&instance, 0, slates_ipc::CLAIM_WAIT_NS).unwrap();
  // A daemon killed with the claim unanswered: nothing it published is taken back, its start stamp included.
  drop(first);
  // A daemon's start stamp is the wall clock when it opens. Wait until the clock has passed the first daemon's
  // opening, so the second's stamp differs and the test cannot pass by the two coinciding.
  while SystemTime::now() <= first_opened {
    std::hint::spin_loop();
  }
  let mut second = Listener::open(&instance, adoption.as_ref()).unwrap();
  // The second daemon lives on with its ends of what it accepted, as a running daemon does.
  let (connected, _, _accepted) = serve_until_connected(&mut second, claim);
  assert!(
    !connected.liveness.daemon_gone(),
    "the daemon that answered the claim is alive and published, and the client judged it gone"
  );
  std::process::exit(0);
}

/// GAPS §8a-sdk-stamp, A-143 (found 2026-10-22: on macOS an async SDK call to a stopped daemon, restarted once,
/// ended `DaemonGone` where it must end `Stalled`). Do: hold the rendezvous as an anchor does, and run in a child
/// under it: daemon A, a client's claim begun while A is published, A dropped unanswered (a killed daemon), then
/// daemon B, which answers the claim. Expect: the client judges B alive. Before A-143 the client kept the start stamp
/// it saw when its claim began, A's, while it watched B's process, and so judged the live B gone. Linux passes by
/// construction: its liveness is the control socket's peer, the daemon that accepted the connection.
#[test]
fn a_claim_begun_before_a_restart_judges_the_daemon_that_answered_it_alive() {
  let instance = format!("test-claim-restart-{}", std::process::id());
  let held = HeldRendezvous::hold(&instance).unwrap();
  let exe = std::env::current_exe().unwrap();
  let status = Command::new(exe)
    .args(["--exact", "rendezvous_restart_child", "--nocapture"])
    .env(CHILD_RESTART_ROLE, &instance)
    .env(ENV_RENDEZVOUS, held.adoption().env_value())
    .status()
    .unwrap();
  assert!(
    status.success(),
    "the client judged the daemon that answered its claim alive: {status:?}"
  );
  drop(held);
}

/// A-176: do let two clients claim before one accept round, and refuse the second one's region (as Windows refused a
/// reconnecting client's region by its name); expect the round to keep the first client, which it accepted and answered,
/// the first client to connect, and the second to be told it was refused. Before A-176 the round answered the second
/// with its refusal and then failed whole, dropping the first client's region and control with it: that client was
/// told it had connected to a region nobody served, and on Linux the daemon's end of its control socket closed under it.
#[test]
fn a_refused_claim_does_not_cost_its_round_the_client_it_accepted() {
  let instance = format!("test-round-{}", std::process::id());
  let mut listener = Listener::open(&instance, None).unwrap();
  let mut first = begin_connect_as(&instance, 0, CLAIM_WAIT_NS).unwrap();
  let mut second = begin_connect_as(&instance, 0, CLAIM_WAIT_NS).unwrap();
  let pid = std::process::id();
  let mut regions_asked = 0;
  let mut refusals = Vec::new();
  let round = listener.accept_pending(
    &|_| false,
    &mut |client_id| {
      regions_asked += 1;
      if regions_asked > 1 {
        return Err(IpcError::OsRefused {
          call: "the test refuses every region after the first",
          code: None,
        });
      }
      Ok(Prepared {
        region: ClientRegion::create(
          &format!("slates-round-{pid}-{client_id}"),
          client_id,
          0,
          geometry(),
        )?,
        kick_fd: None,
      })
    },
    &mut |refusal| refusals.push(refusal),
  );
  assert_eq!(regions_asked, 2, "both claims reached the round");
  assert!(
    round.ended.is_none(),
    "a refused claim does not end the round that refused it: {:?}",
    round.ended
  );
  assert_eq!(
    refusals.len(),
    1,
    "the refused claim is handed back once: {refusals:?}"
  );
  let accepted = round.accepted;
  assert_eq!(
    accepted.len(),
    1,
    "the round keeps the client it accepted before the refusal"
  );
  let Answer::Connected(connected) = answer_within_claim_wait(&mut first) else {
    panic!("the accepted client connects");
  };
  assert!(
    !connected.liveness.daemon_gone(),
    "the accepted client's daemon end lives on in the round's result"
  );
  let refused = answer_within_claim_wait(&mut second);
  assert!(
    matches!(refused, Answer::Refused(_)),
    "the second client is told the daemon refused it: {refused:?}"
  );
  drop(accepted);
}

/// How a claim whose round has run was answered (the connection boxed: it is many times a refusal's size).
enum Answer {
  Connected(Box<slates_ipc::Connected>),
  Refused(IpcError),
  /// Not within the claim wait.
  Unanswered,
}

impl std::fmt::Debug for Answer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Answer::Connected(_) => f.write_str("Connected"),
      Answer::Refused(refusal) => write!(f, "Refused({refusal:?})"),
      Answer::Unanswered => f.write_str("Unanswered"),
    }
  }
}

/// Polls `claim` until it is answered, within the claim wait.
fn answer_within_claim_wait(claim: &mut slates_ipc::Claim) -> Answer {
  let started = Instant::now();
  while started.elapsed() < Duration::from_nanos(CLAIM_WAIT_NS) {
    match claim.poll() {
      Ok(Some(connected)) => return Answer::Connected(Box::new(connected)),
      Ok(None) => {}
      Err(refusal) => return Answer::Refused(refusal),
    }
  }
  Answer::Unanswered
}

/// A-176: do open an instance's rendezvous with no anchor, connect a client and keep it (on macOS and Windows its
/// doorbell maps the bootstrap object), and let the daemon die with no anchor holding its rendezvous (its listener and
/// its ends of what it accepted dropped); then open a fresh rendezvous for the instance, as the next daemon run alone
/// or a restarted anchor does. Expect: the fresh rendezvous opens and connects a new client, and the old client judges
/// its daemon gone. Before A-176 Windows refused the fresh one `ERROR_ALREADY_EXISTS`: a section's name lives while any
/// process maps it, and the old client's doorbell did, so no daemon of the instance could start until every client of
/// the dead one had exited.
#[test]
fn a_fresh_rendezvous_opens_while_a_client_of_a_dead_daemon_lives() {
  let instance = format!("test-fresh-{}", std::process::id());
  let mut dead = Listener::open(&instance, None).unwrap();
  let claim = begin_connect_as(&instance, 0, CLAIM_WAIT_NS).unwrap();
  let (old_client, _, old_accepted) = serve_until_connected(&mut dead, claim);
  drop(old_accepted);
  drop(dead);
  let mut fresh = Listener::open(&instance, None)
    .expect("a fresh rendezvous opens while a client of the dead daemon still maps the old one");
  let claim = begin_connect_as(&instance, 0, CLAIM_WAIT_NS).unwrap();
  let (new_client, _, accepted) = serve_until_connected(&mut fresh, claim);
  assert!(
    old_client.liveness.daemon_gone(),
    "the dead daemon's client judges it gone"
  );
  assert!(
    !new_client.liveness.daemon_gone(),
    "the fresh daemon's client judges it alive"
  );
  drop(accepted);
}

/// A-176: do open an instance's rendezvous with no anchor and keep it, then try a second owner of it, as a second
/// daemon run alone or an anchor would; drop the first and hold it as an anchor, then try again. Expect: every second
/// owner refused while the first lives (Windows and macOS: the owner's token, `InstanceHeld`; Linux: the abstract
/// address in use), and the rendezvous free again once its owner is gone, so two owners never share one bootstrap
/// object.
#[test]
fn a_second_owner_of_a_live_instances_rendezvous_is_refused() {
  let instance = format!("test-owner-{}", std::process::id());
  let first = Listener::open(&instance, None).unwrap();
  assert_held(
    Listener::open(&instance, None).map(|_| ()),
    "a second daemon run alone",
  );
  assert_held(
    HeldRendezvous::hold(&instance).map(|_| ()),
    "an anchor beside a daemon run alone",
  );
  drop(first);
  let held =
    HeldRendezvous::hold(&instance).expect("the rendezvous is free once its owner is gone");
  assert_held(
    HeldRendezvous::hold(&instance).map(|_| ()),
    "a second anchor",
  );
  assert_held(
    Listener::open(&instance, None).map(|_| ()),
    "a daemon run alone beside an anchor",
  );
  drop(held);
}

/// Asserts `outcome` is the refusal of a second owner of a live instance's rendezvous: the owner's token (Windows,
/// macOS).
#[cfg(any(windows, target_os = "macos"))]
fn assert_held(outcome: Result<(), IpcError>, who: &str) {
  assert!(
    matches!(outcome, Err(IpcError::InstanceHeld { .. })),
    "{who} is refused while the instance's owner lives: {outcome:?}"
  );
}

/// The paired arm: the abstract address in use (Linux).
#[cfg(target_os = "linux")]
fn assert_held(outcome: Result<(), IpcError>, who: &str) {
  assert!(
    matches!(outcome, Err(IpcError::OsRefused { call: "bind", .. })),
    "{who} is refused while the instance's owner lives: {outcome:?}"
  );
}

/// Format: the environment variable naming the instance a re-invoked child tries to own, as a daemon run alone and as
/// an anchor would.
const CHILD_OWNER_ROLE: &str = "SLATES_RENDEZVOUS_SECOND_OWNER_CHILD";
/// Format: the environment variable saying what that child must find: `held` (a live owner refuses it) or `owned`.
const CHILD_OWNER_EXPECT: &str = "SLATES_RENDEZVOUS_SECOND_OWNER_EXPECT";

/// A-176, A-178: the refusal across processes, where a second owner really runs, and where macOS's bootstrap server
/// answers a second registrant otherwise than a second registration of one process (1100 against 1101). Do: own an
/// instance's rendezvous here as a daemon run alone; from a second process try to own it as a daemon run alone and as
/// an anchor; drop this owner and let a third process try. Expect: the second refused both ways while this owner
/// lives, and the third owning it both ways once it is gone.
#[test]
fn a_second_process_is_refused_a_live_instances_rendezvous() {
  let instance = format!("test-owner-process-{}", std::process::id());
  let first = Listener::open(&instance, None).unwrap();
  second_owner(&instance, "held");
  drop(first);
  second_owner(&instance, "owned");
}

/// Runs this binary as the child that tries to own `instance`, which must find it `expect`.
fn second_owner(instance: &str, expect: &str) {
  let output = Command::new(std::env::current_exe().unwrap())
    .args(["--exact", "rendezvous_second_owner_child", "--nocapture"])
    .env(CHILD_OWNER_ROLE, instance)
    .env(CHILD_OWNER_EXPECT, expect)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "a second process expecting the rendezvous {expect}: {}",
    String::from_utf8_lossy(&output.stderr)
  );
}

/// The child: tries to own the instance in the environment as a daemon run alone and then as an anchor, each let go at
/// once, and exits 0 only when both found what the environment says (`held`: each refused as a live owner's; `owned`:
/// each owned it). Passes trivially when not the child.
#[test]
fn rendezvous_second_owner_child() {
  let (Ok(instance), Ok(expect)) = (
    std::env::var(CHILD_OWNER_ROLE),
    std::env::var(CHILD_OWNER_EXPECT),
  ) else {
    return;
  };
  let alone = Listener::open(&instance, None).map(|_| ());
  let anchor = HeldRendezvous::hold(&instance).map(|_| ());
  if expect == "held" {
    assert_held(alone, "a second process run alone");
    assert_held(
      anchor,
      "a second process holding the rendezvous as an anchor",
    );
  } else {
    assert!(
      alone.is_ok() && anchor.is_ok(),
      "the rendezvous is free once its owner is gone: alone {alone:?}, anchor {anchor:?}"
    );
  }
  std::process::exit(0);
}
