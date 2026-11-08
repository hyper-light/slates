//! The rings and the wake word (Phase 2 task 3; §4.7): a client end and a daemon end over two
//! mappings of one region, the queued-reply fast path, the armed wait and wake when
//! the reply is late, the client's wake estimate learning from the parks a reply ended, the
//! ring's credit (`RingFull`, never a drop), a hostile slot released without wedging the ring,
//! the deadline, and the doorbell when the daemon is parked.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use slates_ipc::region::{ClientRegion, PARKED_STAMP_SLOT_ONE, RegionGeometry};
use slates_ipc::slot::{PAYLOAD_BYTES, Slot, SlotKind};
use slates_ipc::{ClientEnd, DaemonEnd, IpcError};

/// Shape: slots per ring in these tests.
const SLOTS: u32 = 8;
/// Shape: the spin window the daemon publishes here, microseconds' worth of nanoseconds.
const SPIN_NS: u32 = 200_000;
/// Shape: the client's wake estimate weighs about sixteen wakes here, so a handful of parks moves it.
const SPIN_SHIFT: u32 = 4;
/// Shape: the existing five-second reply deadline also bounds the peer's wait for the client
/// to arm. It is a test hang guard, not a latency assertion or a production tuning value.
const REPLY_DEADLINE_NS: u64 = 5_000_000_000;

fn geometry() -> RegionGeometry {
  RegionGeometry {
    slots: SLOTS,
    spin_ns: SPIN_NS,
    spin_shift: SPIN_SHIFT,
    bulk_bytes: 4096,
    page: 4096,
  }
}

/// A daemon end over a fresh region and a client end over a second mapping of it. The region's name carries the
/// process id: on macOS a region is opened by name, so two test processes on one machine (two CI jobs, a
/// parallel runner) must never map each other's (2026-10-01: concurrent runs of one test met each other's
/// region — `shm_open` EEXIST, and a client receiving another process's reply while its own request went unread).
fn pair(name: &str) -> (DaemonEnd, ClientEnd) {
  let name = format!("{name}-{}", std::process::id());
  let region = ClientRegion::create(&name, 7, 0, geometry()).unwrap();
  let (handoff, len) = region.handoff().unwrap();
  let client = ClientRegion::open(&handoff, len).unwrap();
  assert_eq!(client.client_id(), 7);
  assert_eq!(client.spin_ns(), SPIN_NS);
  (DaemonEnd::new(region), ClientEnd::new(client))
}

/// AC-2.1/T-2.6: replies already available take the no-park path with intact ids and payloads.
/// Performance is measured by the IPC/provisioning benchmarks; a new thread need not be
/// scheduled within this fixture's spin window.
#[test]
fn queued_replies_complete_without_parking_the_client() {
  let (mut daemon, mut client) = pair("slates-ipc-rings-spin");
  for request in 0..3u64 {
    client
      .send(&Slot::inline(request, &[1, 2, 3]).unwrap())
      .unwrap();
    let received = daemon.try_take().unwrap().unwrap();
    let mut payload = received.payload;
    payload.reverse();
    daemon
      .reply(&Slot::inline(received.request, &payload).unwrap())
      .unwrap();
  }
  for request in 0..3u64 {
    let reply = client.wait(Some(1_000_000_000)).unwrap();
    assert_eq!(reply.request, request);
    assert_eq!(reply.payload, vec![3, 2, 1]);
    assert_eq!(reply.kind, SlotKind::Inline);
  }
  assert_eq!(daemon.wakes(), 0, "queued replies need no wake");
  assert_eq!(client.park_ratio(), (0, 3));
  assert_eq!(
    (client.wake_samples(), client.wake_estimate_ns()),
    (0, u64::from(SPIN_NS)),
    "a client that never parked learned nothing and spins for the published window"
  );
}

/// AC-2.1/T-2.6: a reply withheld until the client arms its wait is delivered without a lost
/// wake. It may arrive during the mandatory recheck or after a kernel wait; both are correct.
#[test]
fn a_reply_after_the_client_arms_its_wait_is_not_lost() {
  let (mut daemon, mut client) = pair("slates-ipc-rings-park");
  client.send(&Slot::inline(1, b"?").unwrap()).unwrap();
  let request = daemon.try_take().unwrap().unwrap();
  let handle = std::thread::spawn(move || {
    let started = Instant::now();
    while daemon
      .region()
      .client_parked()
      .unwrap()
      .load(Ordering::Acquire)
      == 0
    {
      assert!(
        started.elapsed() < Duration::from_nanos(REPLY_DEADLINE_NS),
        "the client did not arm its wait"
      );
      std::thread::yield_now();
    }
    daemon
      .reply(&Slot::inline(request.request, b"late").unwrap())
      .unwrap();
    daemon.wakes()
  });
  let reply = client.wait(Some(REPLY_DEADLINE_NS));
  let wakes = handle.join().unwrap();
  let reply = reply.unwrap();
  assert_eq!(reply.request, 1);
  assert_eq!(reply.payload, b"late");
  assert_eq!(client.park_ratio().1, 1, "one reply delivered");
  assert!(wakes <= 1, "one reply can issue at most one wake");
  assert_eq!(
    client
      .region()
      .client_parked()
      .unwrap()
      .load(Ordering::Acquire),
    0,
    "delivery disarms the wait"
  );
}

/// Shape: parked trips the learning test drives — past the estimate's half-life (eleven wakes at a
/// shift of four) with room for trips a reply raced.
const LEARNING_TRIPS: u64 = 32;
/// Shape: the prior the learning client starts from, the most a header can publish (about 4.3 s) —
/// unmistakably above any real wake, so an estimate that learned has left it.
const LEARNING_PRIOR_NS: u32 = u32::MAX;
/// Shape: how long the daemon holds a reply after the client raised its parked flag, so the client
/// is asleep in its wait when the reply wakes it (a millisecond: hundreds of wakes).
const ASLEEP_AFTER: Duration = Duration::from_millis(1);

/// §4.7, §4.3 (A-31): a client parked in its wait and woken by a reply learns that wake — from the
/// daemon's reply stamp to its own return — and its estimate, seeded with the published window, leaves
/// that prior for the wakes it measured. Do: a client that always parks (a chosen zero spin) and a daemon
/// that replies a millisecond after the client raised its flag, for 32 trips. Expect: most trips taught
/// the estimate (the non-vacuity count), and the estimate fell below half its 4.3-second prior.
#[test]
fn a_client_learns_its_wake_from_the_parks_a_reply_ended() {
  let region = ClientRegion::create(
    &format!("slates-ipc-rings-learn-{}", std::process::id()),
    9,
    0,
    RegionGeometry {
      spin_ns: LEARNING_PRIOR_NS,
      ..geometry()
    },
  )
  .unwrap();
  let (handoff, len) = region.handoff().unwrap();
  let mut daemon = DaemonEnd::new(region);
  let mut client = ClientEnd::new(ClientRegion::open(&handoff, len).unwrap());
  client.set_spin_ns(Some(0));
  let server = std::thread::spawn(move || {
    for _ in 0..LEARNING_TRIPS {
      let request = take_within(|| daemon.try_take().unwrap(), "a request");
      let flagged = Instant::now();
      while daemon
        .region()
        .client_parked()
        .unwrap()
        .load(Ordering::Acquire)
        == 0
      {
        assert!(
          flagged.elapsed() < Duration::from_nanos(REPLY_DEADLINE_NS),
          "the client did not arm its wait"
        );
        std::thread::yield_now();
      }
      let raised = Instant::now();
      while raised.elapsed() < ASLEEP_AFTER {
        std::thread::yield_now();
      }
      daemon
        .reply(&Slot::inline(request.request, b"late").unwrap())
        .unwrap();
    }
  });
  for trip in 0..LEARNING_TRIPS {
    client.send(&Slot::inline(trip, b"?").unwrap()).unwrap();
    let reply = client.wait(Some(REPLY_DEADLINE_NS)).unwrap();
    assert_eq!(reply.request, trip);
  }
  server.join().unwrap();
  assert!(
    client.wake_samples() >= LEARNING_TRIPS / 2,
    "most parked trips taught the estimate: {} of {LEARNING_TRIPS} (parks {:?})",
    client.wake_samples(),
    client.park_ratio()
  );
  assert!(
    client.wake_estimate_ns() < u64::from(LEARNING_PRIOR_NS) / 2,
    "the estimate left its prior for the measured wakes: {} ns",
    client.wake_estimate_ns()
  );
}

/// The ring's credit: with every slot holding an untaken request the next send refuses
/// `RingFull`; once the daemon takes one, the send goes through; nothing was dropped.
#[test]
fn a_full_ring_refuses_and_never_drops() {
  let (mut daemon, mut client) = pair("slates-ipc-rings-full");
  for n in 0..u64::from(SLOTS) {
    client.send(&Slot::inline(n, &[]).unwrap()).unwrap();
  }
  assert_eq!(
    client.send(&Slot::inline(99, &[]).unwrap()),
    Err(IpcError::RingFull)
  );
  assert_eq!(
    client
      .region()
      .cmd()
      .depth(client.region().object())
      .unwrap(),
    u64::from(SLOTS)
  );
  let first = daemon.try_take().unwrap().unwrap();
  assert_eq!(first.request, 0);
  client.send(&Slot::inline(99, &[]).unwrap()).unwrap();
  let mut seen = vec![];
  while let Some(req) = daemon.try_take().unwrap() {
    seen.push(req.request);
  }
  assert_eq!(seen, (1..u64::from(SLOTS)).chain([99]).collect::<Vec<_>>());
}

/// Format: where slot 1 of the command ring sits in the region (header 128, words 320 — five
/// cache-line words since layout version 2 added the reply stamp — ring header 128, one slot 64).
const SLOT_ONE_AT: usize = 128 + 320 + 128 + 64;

/// A hostile slot (an unknown kind, a length past the payload) is refused as `BadSlot`, the
/// slot is released, and the next slot flows; an oversized payload is refused before the
/// ring is touched.
#[test]
fn a_hostile_slot_is_refused_and_the_ring_keeps_flowing() {
  let (mut daemon, mut client) = pair("slates-ipc-rings-hostile");
  client.send(&Slot::inline(1, b"ok").unwrap()).unwrap();
  // Poison slot 1 by hand: kind 0xffff and a length past the payload, then publish it.
  {
    let object = client.region_mut().object_mut();
    assert_eq!(
      object
        .atomic_u64(SLOT_ONE_AT)
        .unwrap()
        .load(Ordering::Acquire),
      1,
      "slot 1 is free"
    );
    object
      .write(SLOT_ONE_AT + 8, &0xffffu16.to_le_bytes())
      .unwrap();
    object
      .write(SLOT_ONE_AT + 10, &200u16.to_le_bytes())
      .unwrap();
    object
      .atomic_u64(SLOT_ONE_AT)
      .unwrap()
      .store(2, Ordering::Release);
  }
  assert_eq!(daemon.try_take().unwrap().unwrap().payload, b"ok");
  assert!(matches!(daemon.try_take(), Err(IpcError::BadSlot { .. })));
  assert!(
    client
      .region()
      .cmd()
      .can_push(client.region().object(), 1 + u64::from(SLOTS))
      .unwrap(),
    "the poisoned slot was released to the producer"
  );
  let mut oversized = Slot::inline(3, &[]).unwrap();
  oversized.payload = vec![0; PAYLOAD_BYTES + 1];
  assert!(matches!(
    client.send(&oversized),
    Err(IpcError::PayloadTooLarge { .. })
  ));
  // A producer that poisons its own slot desynchronises only itself: the hand-written slot
  // consumed the client's cursor position, so a valid slot written by hand at the next index
  // is what the daemon takes.
  {
    let object = client.region_mut().object_mut();
    let at = SLOT_ONE_AT + 64;
    object.write(at + 8, &1u16.to_le_bytes()).unwrap();
    object.write(at + 10, &4u16.to_le_bytes()).unwrap();
    object.write(at + 20, b"next").unwrap();
    object.atomic_u64(at).unwrap().store(3, Ordering::Release);
  }
  assert_eq!(daemon.try_take().unwrap().unwrap().payload, b"next");
}

/// Wakes an actual sleeper without a reply. The parked flag includes preparation before the wake-word snapshot;
/// an empty wake then can be coalesced, with no reply for the mandatory ring recheck to find (§4.7, A-31).
#[cfg(not(windows))]
fn wake_without_reply(region: &ClientRegion) {
  let word = region.wake_word().unwrap();
  take_within(
    || slates_ipc::wake::wake_one(word).unwrap().then_some(()),
    "a client asleep on its wake word",
  );
  eprintln!("IPC EMPTY WAKE: the kernel confirmed a sleeper");
}

/// The auto-reset Event retains an early signal, so Windows needs no wake-word snapshot or sleeper confirmation.
#[cfg(windows)]
fn wake_without_reply(region: &ClientRegion) {
  region.wake_signal().unwrap();
}

/// §4.7 "Wake strategy": a wait whose budget is already spent answers at once, as a timeout or as the word's move, never
/// as an error. Do: over a real region's wake word (shared memory, as the client waits on it), wait with no time left
/// for the word to leave the value it holds, then for it to leave a value it does not hold. Expect: `Ok(false)`, timed
/// out, then `Ok(true)`, the word differs. macOS refuses a zero timeout (`os_sync_wait_on_address_with_timeout`:
/// "Passing zero for the @timeout_ns results in an error being returned", `EINVAL`, its SDK header), and a client whose
/// spin had spent its whole reply deadline met that as `OsRefused` (A-186's one-millisecond deadline test on macOS,
/// 2026-10-28).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_wait_with_no_time_left_answers_at_once() {
  let (daemon, _client) = pair("zero-wait");
  let word = daemon.region().wake_word().unwrap();
  let holds = word.load(Ordering::Acquire);
  assert_eq!(
    slates_ipc::wake::wait(word, holds, Some(0)),
    Ok(false),
    "timed out"
  );
  assert_eq!(
    slates_ipc::wake::wait(word, holds.wrapping_add(1), Some(0)),
    Ok(true),
    "the word differs from the value waited on"
  );
}

/// Takes the next item `take` yields, bounded by the reply deadline (a hang guard: one that never arrives
/// fails the test with `what`, never spins it forever).
fn take_within<T>(mut take: impl FnMut() -> Option<T>, what: &str) -> T {
  let started = Instant::now();
  loop {
    if let Some(slot) = take() {
      return slot;
    }
    assert!(
      started.elapsed() < Duration::from_nanos(REPLY_DEADLINE_NS),
      "{what} never arrived"
    );
    std::thread::yield_now();
  }
}

/// Waits (bounded by the reply deadline, a hang guard) until `holds` is true of the region.
fn await_region(region: &ClientRegion, what: &str, holds: impl Fn(&ClientRegion) -> bool) {
  let started = Instant::now();
  while !holds(region) {
    assert!(
      started.elapsed() < Duration::from_nanos(REPLY_DEADLINE_NS),
      "{what}"
    );
    std::thread::yield_now();
  }
}

/// AC-2.1 (§4.7 "Wake strategy"; docs/bugs/2026-09-28-the-client-tests-judged-a-live-daemon-by-a-shorter-clock.md):
/// a wake that brings no reply sends the client back to park and is counted, and the reply that follows
/// still ends the wait. The peer delivers an empty wake, waits until the client
/// has parked again (its reply stamp cleared over the peer's marker), then replies: exactly one unanswered
/// wake, the reply delivered, and every park accounted for by a reply or an unanswered wake. Non-vacuous
/// for the client tests' park assertion, which counted a spurious wake as a broken rule before.
#[test]
fn a_wake_without_a_reply_is_counted_and_the_client_parks_again() {
  let (mut daemon, mut client) = pair("slates-ipc-rings-unanswered");
  client.set_spin_ns(Some(0));
  let server = std::thread::spawn(move || {
    let request = take_within(|| daemon.try_take().unwrap(), "the request");
    await_region(daemon.region(), "the client did not park", |region| {
      region.client_parked().unwrap().load(Ordering::Acquire) != 0
    });
    // A marker the client overwrites with zero only when it parks again, after the unanswered wake: in the
    // reply stamp slot that next park names, the other one from this park's (its flag's value names it).
    let parked = daemon
      .region()
      .client_parked()
      .unwrap()
      .load(Ordering::Acquire);
    let next_slot = usize::from(parked != PARKED_STAMP_SLOT_ONE);
    daemon
      .region()
      .reply_stamp(next_slot)
      .unwrap()
      .store(1, Ordering::Release);
    wake_without_reply(daemon.region());
    await_region(daemon.region(), "the client did not park again", |region| {
      region
        .reply_stamp(next_slot)
        .unwrap()
        .load(Ordering::Acquire)
        == 0
        && region.client_parked().unwrap().load(Ordering::Acquire) != 0
    });
    daemon
      .reply(&Slot::inline(request.request, b"after").unwrap())
      .unwrap();
  });
  client.send(&Slot::inline(9, b"?").unwrap()).unwrap();
  let reply = client.wait(Some(REPLY_DEADLINE_NS));
  let joined = server.join();
  assert!(
    joined.is_ok(),
    "peer {joined:?}; client error {:?}",
    reply.as_ref().err()
  );
  let reply = reply.unwrap();
  assert_eq!(reply.request, 9);
  assert_eq!(reply.payload, b"after");
  assert_eq!(client.unanswered_wakes(), 1, "the empty wake was counted");
  let (parks, replies) = client.park_ratio();
  assert!(
    parks <= replies + client.unanswered_wakes(),
    "every park ended in a reply or an unanswered wake: {parks} parks, {replies} replies"
  );
}

/// No reply within the deadline: `DeadlineExceeded`, and the client is not left parked.
#[test]
fn a_missing_reply_ends_at_the_deadline() {
  let (_daemon, mut client) = pair("slates-ipc-rings-deadline");
  client.send(&Slot::inline(1, &[]).unwrap()).unwrap();
  assert_eq!(
    client.wait(Some(5_000_000)),
    Err(IpcError::DeadlineExceeded)
  );
  assert_eq!(
    client
      .region()
      .client_parked()
      .unwrap()
      .load(Ordering::Acquire),
    0
  );
  assert_eq!(client.park_ratio().0, 1);
}

/// While the daemon's shard is parked the client rings the doorbell per request.
#[test]
fn the_doorbell_rings_only_while_the_daemon_is_parked() {
  let (daemon, mut client) = pair("slates-ipc-rings-doorbell");
  client.send(&Slot::inline(1, &[]).unwrap()).unwrap();
  assert_eq!(daemon.doorbell().unwrap(), 0);
  daemon.set_parked(true).unwrap();
  client.send(&Slot::inline(2, &[]).unwrap()).unwrap();
  client.send(&Slot::inline(3, &[]).unwrap()).unwrap();
  assert_eq!(daemon.doorbell().unwrap(), 2);
  daemon.set_parked(false).unwrap();
  client.send(&Slot::inline(4, &[]).unwrap()).unwrap();
  assert_eq!(daemon.doorbell().unwrap(), 2);
}

/// Whether the loopback completion socket `sock` is readable within `timeout` — a `WSAPoll` for
/// `POLLRDNORM`, the readiness a Windows async runtime (`uv_poll`, a Python selector) waits on. It
/// borrows the socket by value (`WSAPoll` takes no ownership), so it neither closes nor races the
/// bridge's own handle.
#[cfg(windows)]
fn readable_within(sock: std::os::windows::io::RawSocket, timeout: Duration) -> bool {
  use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAPOLLFD, WSAPoll};
  // `RawSocket` is a `u64`; a Windows `SOCKET` is pointer-width. The handle fits (a socket is within
  // the pointer range on its own target), so the conversion is exact, not a truncating cast.
  let mut fds = [WSAPOLLFD {
    fd: usize::try_from(sock).unwrap(),
    events: POLLRDNORM,
    revents: 0,
  }];
  let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
  // SAFETY: `fds` is one well-formed pollfd naming the live completion socket; `WSAPoll` reads the
  // array of length one and writes `revents`. It takes no ownership of the socket.
  let n = unsafe { WSAPoll(fds.as_mut_ptr(), 1, ms) };
  n > 0 && (fds[0].revents & POLLRDNORM) != 0
}

/// The async completion socket becomes readable when a reply lands for an armed (parked) client —
/// the readiness a Windows async SDK's event loop waits on (`uv_poll` / a selector, D-19). Windows
/// passes no shared completion fd (Mach and named sockets are refused, D-10), so the client-local
/// bridge parks on the region's named auto-reset Event — the daemon signals *that* on a reply, not
/// the process-local wake word — and nudges its loopback socket only while armed, so a fast-path
/// reply (parked == 0) wakes no loop. This drives that whole path over a real region and a real
/// reply; the socket goes from quiet to readable exactly at the armed reply.
#[cfg(windows)]
#[test]
fn the_completion_socket_becomes_readable_on_an_armed_reply() {
  let (mut daemon, mut client) = pair("slates-ipc-completion-armed");
  let sock = client.enable_async_completion().unwrap();
  // No reply yet: an event loop polling the socket would find nothing to wake on.
  assert!(
    !readable_within(sock, Duration::from_millis(50)),
    "the completion socket is quiet until a reply lands"
  );

  // Arm before the reply (the SDK sets parked before it yields to its loop), then send a request.
  client.arm_async().unwrap();
  client.send(&Slot::inline(0, &[1, 2, 3]).unwrap()).unwrap();

  // The daemon answers on its own thread: it reverses the payload and replies, which bumps the wake
  // word and signals the Event because the client is parked.
  let server = std::thread::spawn(move || {
    let req = take_within(|| daemon.try_take().unwrap(), "the request");
    let mut reply = req.payload.clone();
    reply.reverse();
    daemon
      .reply(&Slot::inline(req.request, &reply).unwrap())
      .unwrap();
    daemon.wakes()
  });

  // The bridge wakes on the Event and nudges the socket: the loop's poll now fires.
  assert!(
    readable_within(sock, Duration::from_secs(2)),
    "the completion socket became readable after the armed reply"
  );
  client.drain_completion();

  // And the reply is on the ring to take, id-matched with its payload reversed.
  let reply = take_within(|| client.try_take().unwrap(), "the reply");
  assert_eq!(reply.request, 0);
  assert_eq!(reply.payload, vec![3, 2, 1]);
  assert_eq!(reply.kind, SlotKind::Inline);
  client.disarm_async().unwrap();

  let wakes = server.join().unwrap();
  assert_eq!(
    wakes, 1,
    "the daemon issued exactly one wake for the parked client"
  );
}

/// A create whose framed body is larger than a slot's inline payload, so it travels through the bulk area. Every name
/// has the same length, so one request's body read as another's decodes cleanly: the silent case.
fn bulk_create(n: u64) -> slates_ipc::protocol::RequestBody {
  slates_ipc::protocol::RequestBody::Create {
    name: format!("bulk-request-{n:08}-padded-past-the-inline-payload"),
    size: slates_ipc::protocol::SizeClass::Bounded { limit: 1 << 20 },
    names: slates_ipc::protocol::NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// §4.7 `Ring`, the bulk area. Do: fill the command ring with bulk requests, let the daemon end take the first (its
/// slot is released to the client), let the client send one more (whose body takes the same bulk chunk, the ring's
/// position modulo its slots), and only then decode the first request. Expect: the first request's own body. The slot
/// was released before its body was read, so a client that kept sending wrote the next request's body over it: the
/// daemon then ran the next request's body under the first request's id (found by the multiplexing load harness as
/// `BadRequest { "body does not decode" }`, one `CreateWork` and one or two `Edit`s in every 300-client run at 32
/// clients a connection, 2026-11-06).
#[test]
fn a_request_taken_from_the_ring_keeps_its_bulk_body_when_the_client_sends_on() {
  use slates_ipc::protocol::{Direction, RequestBody, pack, unpack};
  let (mut daemon, mut client) = pair("slates-ipc-rings-bulk-req");
  let slots = u64::from(SLOTS);
  for n in 0..slots {
    let index = client.next_request_index();
    let slot = pack(
      client.region_mut(),
      Direction::Request,
      index,
      n,
      &bulk_create(n),
    )
    .unwrap();
    assert_eq!(
      slot.kind,
      SlotKind::Bulk,
      "the body travels through the bulk area"
    );
    client.send(&slot).unwrap();
  }
  let first = daemon.try_take().unwrap().unwrap();
  let index = client.next_request_index();
  let later = pack(
    client.region_mut(),
    Direction::Request,
    index,
    slots,
    &bulk_create(slots),
  )
  .unwrap();
  client.send(&later).unwrap();
  let read: RequestBody = unpack(daemon.region(), first.kind, &first.payload).unwrap();
  assert_eq!(read, bulk_create(0), "the first request decodes as itself");
}

/// §4.7 `Ring`, the bulk area, the reply direction. Do: fill the completion ring with bulk replies, let the client end
/// take the first, let the daemon end write one more (into the same bulk chunk), then decode the first. Expect: the
/// first reply's own body: a daemon writing a deferred reply as soon as a slot frees must never change a reply the
/// client has taken but not yet read.
#[test]
fn a_reply_taken_from_the_ring_keeps_its_bulk_body_when_the_daemon_replies_on() {
  use slates_ipc::protocol::{Direction, ReplyBody, pack, unpack};
  let (mut daemon, mut client) = pair("slates-ipc-rings-bulk-reply");
  let slots = u64::from(SLOTS);
  let reply = |n: u64| ReplyBody::Refused {
    refusal: slates_ipc::protocol::Refusal::BadRequest {
      reason: format!("bulk-reply-{n:08}-padded-past-the-inline-payload"),
    },
  };
  for n in 0..slots {
    let index = daemon.next_reply_index();
    let slot = pack(daemon.region_mut(), Direction::Reply, index, n, &reply(n)).unwrap();
    assert_eq!(
      slot.kind,
      SlotKind::Bulk,
      "the body travels through the bulk area"
    );
    daemon.reply(&slot).unwrap();
  }
  let first = client.try_take().unwrap().unwrap();
  let index = daemon.next_reply_index();
  let later = pack(
    daemon.region_mut(),
    Direction::Reply,
    index,
    slots,
    &reply(slots),
  )
  .unwrap();
  daemon.reply(&later).unwrap();
  let read: ReplyBody = unpack(client.region(), first.kind, &first.payload).unwrap();
  assert_eq!(read, reply(0), "the first reply decodes as itself");
}

/// §4.7 `Ring`, the bulk area, the producer's side. Do: fill the command ring with bulk requests through
/// `ClientEnd::send_message`, offer one more while the daemon has taken none, then let the daemon take and decode the
/// first. Expect: the offer is refused `RingFull`, and the first request decodes as itself. A producer that packed a
/// body before it knew the ring had room wrote it into the bulk chunk of its ring position, which belongs to the oldest
/// request still unread. A client below its in-flight bound reaches a full ring whenever unawaited acknowledgements share
/// it, and the multiplexing load harness met it in every 300-client run at 32 clients a connection.
#[test]
fn a_request_refused_ring_full_never_writes_over_an_unread_body() {
  use slates_ipc::protocol::{RequestBody, unpack};
  let (mut daemon, mut client) = pair("slates-ipc-rings-bulk-full");
  let slots = u64::from(SLOTS);
  for n in 0..slots {
    client.send_message(n, &bulk_create(n)).unwrap();
  }
  assert!(
    matches!(
      client.send_message(slots, &bulk_create(slots)),
      Err(IpcError::RingFull)
    ),
    "the ring is full"
  );
  let first = daemon.try_take().unwrap().unwrap();
  let read: RequestBody = unpack(daemon.region(), first.kind, &first.payload).unwrap();
  assert_eq!(read, bulk_create(0), "the first request decodes as itself");
}
