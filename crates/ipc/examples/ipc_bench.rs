//! The IPC baselines (Phase 2 task 3; §4.7): one ring round trip with both ends spinning on
//! two threads (the fast path's floor: two cache-line transfers), and one parked round trip
//! (the client parks on the wake word, the daemon wakes it: the cost the spin window is
//! measured against). Rows are `ratchet\t<key>\t<lower>\t<median>\t<upper>` in nanoseconds.
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Instant;

use slates_ipc::region::{ClientRegion, RegionGeometry};
use slates_ipc::slot::Slot;
use slates_ipc::{ClientEnd, DaemonEnd};

/// Shape: runs per row.
const RUNS: usize = 5;
/// Shape: round trips per run.
const TRIPS: u64 = 20_000;
/// Shape: parked round trips per run (each pays a wake).
const PARKED_TRIPS: u64 = 2_000;
/// Shape: slots per ring.
const SLOTS: u32 = 64;

fn pair(name: &str, spin_ns: u32) -> (DaemonEnd, ClientEnd) {
  let region = ClientRegion::create(
    name,
    1,
    0,
    RegionGeometry {
      slots: SLOTS,
      spin_ns,
      bulk_bytes: 4096,
      page: 4096,
    },
  )
  .unwrap();
  let (handoff, len) = region.handoff().unwrap();
  let client = ClientRegion::open(&handoff, len).unwrap();
  (DaemonEnd::new(region), ClientEnd::new(client))
}

fn row(key: &str, mut samples: Vec<u64>) {
  samples.sort_unstable();
  let lower = samples[0];
  let median = samples[samples.len() / 2];
  let upper = samples[samples.len() - 1];
  println!("ratchet\t{key}\t{lower}\t{median}\t{upper}");
  println!(
    "  {key}: {median} ns [{lower}, {upper}] over {} runs: {samples:?}",
    samples.len()
  );
}

fn ns(elapsed: std::time::Duration) -> u64 {
  u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

/// The daemon thread: echoes `trips` requests, spinning; with `late`, it waits until the
/// client has parked before replying, so every trip pays a wake.
fn serve(mut daemon: DaemonEnd, trips: u64, late: bool) -> std::thread::JoinHandle<u64> {
  std::thread::spawn(move || {
    let mut served = 0;
    while served < trips {
      if let Some(req) = daemon.try_take().unwrap() {
        if late {
          while daemon
            .region()
            .client_parked()
            .unwrap()
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
          {
            std::hint::spin_loop();
          }
        }
        daemon
          .reply(&Slot::inline(req.request, &req.payload).unwrap())
          .unwrap();
        served += 1;
      } else {
        std::hint::spin_loop();
      }
    }
    daemon.wakes()
  })
}

fn main() {
  let mut spinning = Vec::new();
  let mut parked = Vec::new();
  for run in 0..RUNS {
    // Spinning: a spin window far past any reply, so the client never parks.
    let (daemon, mut client) = pair(&format!("slates-ipc-bench-spin-{run}"), u32::MAX);
    let handle = serve(daemon, TRIPS, false);
    let started = Instant::now();
    for n in 0..TRIPS {
      client
        .send(&Slot::inline(n, &[1, 2, 3, 4]).unwrap())
        .unwrap();
      let reply = client.wait(None).unwrap();
      assert_eq!(reply.request, n);
    }
    spinning.push(ns(started.elapsed()) / TRIPS);
    assert_eq!(handle.join().unwrap(), 0);
    // Parked: a zero spin window, and the daemon replies only once the client parked.
    let (daemon, mut client) = pair(&format!("slates-ipc-bench-park-{run}"), 0);
    let handle = serve(daemon, PARKED_TRIPS, true);
    let started = Instant::now();
    for n in 0..PARKED_TRIPS {
      client
        .send(&Slot::inline(n, &[1, 2, 3, 4]).unwrap())
        .unwrap();
      let reply = client.wait(None).unwrap();
      assert_eq!(reply.request, n);
    }
    parked.push(ns(started.elapsed()) / PARKED_TRIPS);
    assert_eq!(
      handle.join().unwrap(),
      PARKED_TRIPS,
      "every trip woke the client"
    );
    assert_eq!(client.park_ratio().0, PARKED_TRIPS);
  }
  row("ipc.ring_round_trip_spinning", spinning);
  row("ipc.ring_round_trip_parked_and_woken", parked);
}
