//! The async round-trip core (§4.7 "Wake strategy", R6, D-19): the client's `begin` / `spin_reply`
//! / `poll_reply` primitives drive a real daemon without a blocking wait — the foundation the Python
//! `asyncio` and Node `uv_poll` SDKs build on. Three by-use paths over an in-process daemon: the
//! fast path (a reply taken within the spin, no completion fd), the slow path (armed, the reply
//! taken after the completion fd an event loop would poll signals), and out-of-order matching (two
//! requests in flight, each reply routed to its own request by id).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::os::fd::{BorrowedFd, RawFd};
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, RequestId};
use slates_ipc::protocol::{NamePolicy, ReplyBody, RequestBody, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the probe budget of the quick profile these tests measure (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon: two, so a client's shard and the control shard differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the fast-path spin budget (nanoseconds): a tenth of a second, well past a live daemon's
/// microsecond reply, so the fast path always catches it here.
const SPIN_NS: u64 = 100_000_000;
/// Shape: how long the fast path is retried window after window before the test calls the reply lost: far past a
/// daemon slowed by a sanitizer or a loaded runner.
const REPLY_WAIT: Duration = Duration::from_secs(30);
/// Shape: how long the slow path waits for the completion fd to signal (nanoseconds): two seconds,
/// far past a live daemon's reply.
const FD_WAIT_NS: u64 = 2_000_000_000;

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures")
}

/// The product's own deadlines (`Deadlines::derive` over the anchor's liveness budget and the recovery
/// budget), never a shorter hand-picked reply clock that calls a live daemon stalled
/// (`docs/bugs/2026-09-28-the-client-tests-judged-a-live-daemon-by-a-shorter-clock.md`).
fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// A bounded scratch volume request under `name`.
fn create(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// Waits up to `timeout_ns` for `fd` to become readable without consuming it (a poll, not a read).
fn wait_readable(fd: RawFd, timeout_ns: u64) -> bool {
  use rustix::event::{PollFd, PollFlags, Timespec};
  // SAFETY: `fd` is the client's live completion fd, borrowed for one poll within the test.
  let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
  let mut fds = [PollFd::new(&borrowed, PollFlags::IN)];
  let ts = Timespec {
    tv_sec: i64::try_from(timeout_ns / 1_000_000_000).unwrap_or(0),
    tv_nsec: i64::try_from(timeout_ns % 1_000_000_000).unwrap_or(0),
  };
  rustix::event::poll(&mut fds, Some(&ts)).is_ok_and(|n| n > 0)
}

/// The async core drives a real daemon three ways. Do: spawn an in-process daemon; take a create's
/// reply by the fast spin path; take a snapshot's reply by the completion fd (armed before the send,
/// so the daemon signals the fd exactly as it would for a request parked on the event loop); and
/// prove two in-flight requests' replies each route to their own request. Expect: every reply comes
/// back typed, the completion fd fires for the parked request, and the ids match.
#[test]
fn the_async_core_drives_a_daemon_by_spin_and_completion_fd() {
  let profile = profile();
  let instance = format!("cl-async-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let _daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-async-{}", std::process::id()),
    },
  )
  .unwrap();
  _daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);

  // Fast path: begin a create and take its reply within the spin window — no event loop, no fd.
  // Window after window, never the fd: what is proved is the path the reply takes, not how fast a slowed daemon (TSan,
  // a loaded runner) commits its first create. One 100 ms window failed on CI's TSan lane, 2026-10-06.
  let create_id = client.begin(&create("async-fast")).unwrap();
  let began = std::time::Instant::now();
  let fast = loop {
    if let Some(reply) = client.spin_reply(create_id, SPIN_NS).unwrap() {
      break reply;
    }
    assert!(
      began.elapsed() < REPLY_WAIT,
      "the create replied through the spin path within {REPLY_WAIT:?}"
    );
  };
  let volume = match fast {
    ReplyBody::Created { id } => id,
    other => panic!("the create's reply is Created, got {other:?}"),
  };

  // Slow path: the completion fd an async event loop would poll. Arm before the send, so the daemon
  // sees the client parked when it replies and signals the fd — the exact mechanism a real slow path
  // uses once its spin has lapsed and it has armed and yielded.
  let fd = client.enable_async_completion().unwrap();
  client.arm_async().unwrap();
  let snapshot_id = client.begin(&RequestBody::Snapshot { volume }).unwrap();
  assert!(
    wait_readable(fd, FD_WAIT_NS),
    "the completion fd signals the reply to the armed (parked) request"
  );
  client.drain_completion();
  match client
    .poll_reply(snapshot_id)
    .unwrap()
    .expect("the snapshot reply is on the ring once the completion fd signals")
  {
    ReplyBody::Snapshotted { .. } => {}
    other => panic!("the snapshot's reply is Snapshotted, got {other:?}"),
  }
  client.disarm_async().unwrap();

  // Out-of-order matching: two requests in flight, each reply routed to its own request by id. Poll
  // the second's id first (buffering the first's reply if it arrived), then the first's from the
  // buffer — the multiplexing an async pump relies on.
  let a = client.begin(&create("async-a")).unwrap();
  let b = client.begin(&create("async-b")).unwrap();
  let reply_b = poll_until(&mut client, b);
  let reply_a = poll_until(&mut client, a);
  assert!(
    matches!(reply_a, ReplyBody::Created { .. }) && matches!(reply_b, ReplyBody::Created { .. }),
    "each of two in-flight requests got its own Created reply: a={reply_a:?} b={reply_b:?}"
  );
}

/// Polls `id`'s reply until it lands within the reply deadline (the async core is non-blocking, so
/// the test drives the ring itself here rather than through an event loop).
fn poll_until(client: &mut Client, id: RequestId) -> ReplyBody {
  let started = Instant::now();
  loop {
    if let Some(reply) = client.poll_reply(id).unwrap() {
      return reply;
    }
    assert!(
      u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX) < deadlines().reply_ns,
      "a reply for {id:?} arrived within the deadline"
    );
    std::hint::spin_loop();
  }
}

/// Begins `List` and takes its reply, driving the ring until it lands.
fn list_round_trip(client: &mut Client) {
  let id = client.begin(&RequestBody::List).unwrap();
  client.begin_ack_if_due().unwrap();
  let _ = poll_until(client, id);
}

/// AUD-29-22 (§4.9, banned item 8): an awaited reply is never evicted to bound the buffer. Do: begin a call
/// and let its reply be drained into the buffer, then begin and finish three bounds' worth of further calls
/// (acknowledgements going out as they fall due); then begin calls up to the admitted bound; abandon one
/// whose reply is buffered. Expect: the early reply still taken afterwards, the call past the bound refused
/// `TooManyOutstanding` with nothing sent, a call admitted again once one is abandoned, and the abandoned
/// reply dropped on arrival and counted. Until 2026-10-01 the buffer dropped its oldest reply past twice the
/// ring's slots, and the early reply's later poll returned nothing for good.
#[test]
fn an_awaited_reply_survives_any_drain_and_admission_refuses_at_the_bound() {
  let profile = profile();
  let instance = format!("cl-await-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-await-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);

  let early = client.begin(&RequestBody::List).unwrap();
  drain_until_ready(&mut client, early);
  let limit = fill_to_the_bound(&mut client);
  for _ in 0..limit * 3 {
    list_round_trip(&mut client);
  }
  assert!(
    matches!(
      client.poll_reply(early).unwrap(),
      Some(ReplyBody::Listed { .. })
    ),
    "the early reply is still there after three bounds of drained traffic"
  );
  assert_abandoned_replies_are_dropped(&mut client);
  assert!(
    client.outstanding().is_empty(),
    "nothing is left outstanding"
  );
  daemon.stop();
}

/// Drains the completion ring until `id`'s reply is in the client's buffer.
fn drain_until_ready(client: &mut Client, id: RequestId) {
  let started = Instant::now();
  while !client.take_ready().unwrap().contains(&id.word()) {
    assert!(started.elapsed() < START_WAIT, "{id:?}'s reply is drained");
    std::hint::spin_loop();
  }
}

/// Begins calls until admission refuses, checks the bound counts the one already outstanding, then takes
/// every reply back; returns the bound.
fn fill_to_the_bound(client: &mut Client) -> usize {
  let mut held = Vec::new();
  let limit = loop {
    match client.begin(&RequestBody::List) {
      Ok(id) => held.push(id),
      Err(ClientError::TooManyOutstanding { limit }) => break limit,
      Err(e) => panic!("{e}"),
    }
  };
  assert_eq!(
    held.len() + 1,
    limit,
    "the early call holds one of the slots"
  );
  for id in held {
    let _ = poll_until(client, id);
  }
  limit
}

/// An abandoned call's buffered reply is gone, and a late reply to an abandoned call is dropped and counted.
fn assert_abandoned_replies_are_dropped(client: &mut Client) {
  let abandoned = client.begin(&RequestBody::List).unwrap();
  drain_until_ready(client, abandoned);
  let dropped_before = client.unawaited_dropped();
  assert!(client.abandon(abandoned.word()));
  assert!(
    client.poll_reply(abandoned).unwrap().is_none(),
    "an abandoned reply is gone"
  );
  let late = client.begin(&RequestBody::List).unwrap();
  client.abandon(late.word());
  let started = Instant::now();
  while client.unawaited_dropped() == dropped_before {
    let _ = client.take_ready().unwrap();
    assert!(
      started.elapsed() < START_WAIT,
      "the late reply arrives and is dropped"
    );
    std::hint::spin_loop();
  }
}
