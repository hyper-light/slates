//! The rendezvous across real processes (Phase 2 task 3; §4.7 "Rendezvous", §4.13): the test
//! binary re-invoked as the client connects to the daemon end running in the test process,
//! receives its region through the platform's handoff (an abstract socket with `SCM_RIGHTS`
//! on Linux; the bootstrap object with claim slots on macOS), sends a request, and gets the
//! reply; a client with no daemon is refused `DaemonUnavailable` and creates nothing.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::time::{Duration, Instant};

use slates_ipc::region::{ClientRegion, RegionGeometry};
use slates_ipc::slot::Slot;
use slates_ipc::{
  CLAIM_WAIT_NS, ClientEnd, DaemonEnd, IpcError, Listener, Prepared, begin_connect_as, connect,
};

/// Format: the environment variable that turns this binary into the client.
const CHILD_ROLE: &str = "SLATES_IPC_TEST_CLIENT";
/// Format: the environment variable naming the instance a re-invoked child connects to expecting the
/// typed capacity refusal.
const CHILD_REFUSED_ROLE: &str = "SLATES_RENDEZVOUS_REFUSED_CHILD";
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
  let mut listener = Listener::open(&instance).unwrap();
  let exe = std::env::current_exe().unwrap();
  let mut child = Command::new(exe)
    .args(["--exact", "rendezvous_client_refused", "--nocapture"])
    .env(CHILD_REFUSED_ROLE, &instance)
    .spawn()
    .unwrap();
  let started = Instant::now();
  while started.elapsed() < Duration::from_millis(SERVE_MS) && listener.capacity_refused() == 0 {
    listener
      .accept_pending(&|_| false, &mut |_client_id| {
        Err(IpcError::TooManyClients {
          limit: REFUSED_LIMIT,
        })
      })
      .unwrap();
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
  let mut listener = Listener::open(&instance).unwrap();
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
    let accepted = listener
      .accept_pending(&|_| false, &mut |client_id| {
        Ok(Prepared {
          region: ClientRegion::create(
            &format!("slates-cr-{pid}-{client_id}"),
            client_id,
            0,
            geometry(),
          )?,
          kick_fd: None,
        })
      })
      .unwrap();
    for a in accepted {
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
  let mut listener = Listener::open(&instance).unwrap();
  let started = Instant::now();
  let (refusal, polls, longest) = poll_until_refused(begin_connect_as(&instance, 0).unwrap());
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
    assert!(
      longest < poll_bound(),
      "the longest poll was {longest:?}, bound {:?}",
      poll_bound()
    );
  }
}

/// Polls `claim` until it ends, which must be a refusal: the refusal, the polls made, the longest poll.
fn poll_until_refused(mut claim: slates_ipc::Claim) -> (IpcError, u64, Duration) {
  let mut polls = 0u64;
  let mut longest = Duration::ZERO;
  loop {
    let at = Instant::now();
    let polled = claim.poll();
    longest = longest.max(at.elapsed());
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
fn poll_while_serving(
  listener: &mut Listener,
  instance: &str,
) -> (slates_ipc::Connected, Duration) {
  let pid = std::process::id();
  let mut claim = begin_connect_as(instance, 0).unwrap();
  let started = Instant::now();
  let mut longest = Duration::ZERO;
  // The daemon's side of each accepted claim, kept: its region lives while the daemon holds it.
  let mut accepted = Vec::new();
  loop {
    let served = listener.accept_pending(&|_| false, &mut |client_id| {
      Ok(Prepared {
        region: ClientRegion::create(
          &format!("slates-claim-{pid}-{client_id}"),
          client_id,
          0,
          geometry(),
        )?,
        kick_fd: None,
      })
    });
    accepted.extend(served.unwrap_or_default());
    let at = Instant::now();
    let polled = claim.poll().unwrap();
    longest = longest.max(at.elapsed());
    if let Some(connected) = polled {
      // The daemon's ends go with the test; the client's mapping keeps the region it opened.
      drop(accepted);
      return (connected, longest);
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
  let listener = Listener::open(&instance).unwrap();
  let claim = begin_connect_as(&instance, 0).unwrap();
  drop(listener);
  let (refusal, _, _) = poll_until_refused(claim);
  assert!(
    matches!(refusal, IpcError::DaemonUnavailable { .. }),
    "{refusal:?}"
  );
}
