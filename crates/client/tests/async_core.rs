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
/// Shape: the reply deadline (nanoseconds): a fifth of a second, far past any served verb.
const REPLY_NS: u64 = 200_000_000;
/// Shape: the reconnect budget (nanoseconds): five seconds.
const RECONNECT_NS: u64 = 5_000_000_000;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the fast-path spin budget (nanoseconds): a tenth of a second, well past a live daemon's
/// microsecond reply, so the fast path always catches it here.
const SPIN_NS: u64 = 100_000_000;
/// Shape: how long the slow path waits for the completion fd to signal (nanoseconds): two seconds,
/// far past a live daemon's reply.
const FD_WAIT_NS: u64 = 2_000_000_000;

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

fn deadlines() -> Deadlines {
  Deadlines {
    reply_ns: REPLY_NS,
    reconnect_ns: RECONNECT_NS,
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
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let _daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: "slates-seg-cl-async".to_owned(),
    },
  )
  .unwrap();
  let mut client = connect(&instance);

  // Fast path: begin a create and take its reply within the spin window — no event loop, no fd.
  let create_id = client.begin(&create("async-fast")).unwrap();
  let volume = match client
    .spin_reply(create_id, SPIN_NS)
    .unwrap()
    .expect("the create replied within the spin window")
  {
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
      u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX) < REPLY_NS,
      "a reply for {id:?} arrived within the deadline"
    );
    std::hint::spin_loop();
  }
}
