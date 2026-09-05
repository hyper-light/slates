//! The rings and the wake word (Phase 2 task 3; §4.7): a client end and a daemon end over two
//! mappings of one region on two threads, the round trip spinning, the park and the wake when
//! the reply is late, the ring's credit (`RingFull`, never a drop), a hostile slot released
//! without wedging the ring, the deadline, and the doorbell when the daemon is parked.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use slates_ipc::region::{ClientRegion, RegionGeometry};
use slates_ipc::slot::{PAYLOAD_BYTES, Slot, SlotKind};
use slates_ipc::{ClientEnd, DaemonEnd, IpcError};

/// Shape: slots per ring in these tests.
const SLOTS: u32 = 8;
/// Shape: the spin window the daemon publishes here, microseconds' worth of nanoseconds.
const SPIN_NS: u32 = 200_000;
/// Shape: the pause the slow daemon takes before replying, well past the spin window.
const LATE_MS: u64 = 20;

fn geometry() -> RegionGeometry {
  RegionGeometry {
    slots: SLOTS,
    spin_ns: SPIN_NS,
    bulk_bytes: 4096,
    page: 4096,
  }
}

/// A daemon end over a fresh region and a client end over a second mapping of it.
fn pair(name: &str) -> (DaemonEnd, ClientEnd) {
  let region = ClientRegion::create(name, 7, 0, geometry()).unwrap();
  let (handoff, len) = region.handoff().unwrap();
  let client = ClientRegion::open(&handoff, len).unwrap();
  assert_eq!(client.client_id(), 7);
  assert_eq!(client.spin_ns(), SPIN_NS);
  (DaemonEnd::new(region), ClientEnd::new(client))
}

/// A request is answered while the client spins; the request id and payload come back.
#[test]
fn a_round_trip_completes_while_the_client_spins() {
  let (mut daemon, mut client) = pair("slates-ipc-rings-spin");
  let handle = std::thread::spawn(move || {
    let mut served = 0;
    while served < 3 {
      if let Some(req) = daemon.try_take().unwrap() {
        let mut reply = req.payload.clone();
        reply.reverse();
        daemon
          .reply(&Slot::inline(req.request, &reply).unwrap())
          .unwrap();
        served += 1;
      } else {
        std::hint::spin_loop();
      }
    }
    daemon.wakes()
  });
  for n in 0..3u64 {
    client.send(&Slot::inline(n, &[1, 2, 3]).unwrap()).unwrap();
    let reply = client.wait(Some(1_000_000_000)).unwrap();
    assert_eq!(reply.request, n);
    assert_eq!(reply.payload, vec![3, 2, 1]);
    assert_eq!(reply.kind, SlotKind::Inline);
  }
  let wakes = handle.join().unwrap();
  assert_eq!(wakes, 0, "the client never parked");
  assert_eq!(client.park_ratio(), (0, 3));
}

/// A late reply: the client spins its window, parks on the wake word, and is woken by the
/// daemon's wake once the reply is written.
#[test]
fn a_late_reply_parks_the_client_and_the_daemon_wakes_it() {
  let (mut daemon, mut client) = pair("slates-ipc-rings-park");
  let handle = std::thread::spawn(move || {
    loop {
      if let Some(req) = daemon.try_take().unwrap() {
        // The test harness delays the reply past the spin window (D-9: shipped code never
        // sleeps).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(LATE_MS));
        daemon
          .reply(&Slot::inline(req.request, b"late").unwrap())
          .unwrap();
        return daemon.wakes();
      }
      std::hint::spin_loop();
    }
  });
  client.send(&Slot::inline(1, b"?").unwrap()).unwrap();
  let reply = client.wait(Some(5_000_000_000)).unwrap();
  assert_eq!(reply.payload, b"late");
  assert_eq!(client.park_ratio(), (1, 1), "one park for one reply");
  assert_eq!(
    handle.join().unwrap(),
    1,
    "one wake issued for the parked client"
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

/// Format: where slot 1 of the command ring sits in the region (header 128, words 256, ring
/// header 128, one slot 64).
const SLOT_ONE_AT: usize = 128 + 256 + 128 + 64;

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
    let bytes = object.bytes_mut();
    bytes[SLOT_ONE_AT + 8..SLOT_ONE_AT + 10].copy_from_slice(&0xffffu16.to_le_bytes());
    bytes[SLOT_ONE_AT + 10..SLOT_ONE_AT + 12].copy_from_slice(&200u16.to_le_bytes());
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
    let bytes = object.bytes_mut();
    bytes[at + 8..at + 10].copy_from_slice(&1u16.to_le_bytes());
    bytes[at + 10..at + 12].copy_from_slice(&4u16.to_le_bytes());
    bytes[at + 20..at + 24].copy_from_slice(b"next");
    object.atomic_u64(at).unwrap().store(3, Ordering::Release);
  }
  assert_eq!(daemon.try_take().unwrap().unwrap().payload, b"next");
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
