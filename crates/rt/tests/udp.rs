//! A UDP datagram received asynchronously through the runtime's driver (§4.10a): a task awaits
//! `recv_from`, which registers read-readiness with the shard's driver and yields; a second task
//! sends after a short runtime sleep, the socket becomes readable, the driver wakes the receiver, and
//! the datagram arrives. This is the fleet transport's substrate proven end to end on the
//! readiness-native driver (kqueue here on macOS; epoll on Linux) — no foreign runtime.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

// The address types come from the runtime's own re-export (`core::net`'s, the same ones `rustix::net`
// re-exports) so the test builds on every platform — including Windows, where a real datagram round
// trip here exercises the IOCP driver's AFD readiness reactor (`crate::afd`).
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
  }
}

/// A task's `recv_from` awaits through the driver until a datagram arrives; a second task sends it.
#[test]
fn a_udp_datagram_is_received_through_the_driver() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];

  // The receiver is bound before the tasks so the sender knows where to send.
  let receiver = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let target = receiver.local_addr().unwrap();
  assert_ne!(target.port(), 0, "the OS assigned a port");

  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let mut buf = [0u8; 64];
    let outcome = receiver
      .recv_from(&mut buf)
      .await
      .map(|(n, from)| (buf[..n].to_vec(), from));
    let _ = tx.send(outcome);
  })
  .unwrap();

  // The sender waits a runtime tick (letting the receiver register read-readiness on the driver),
  // then sends from outside the runtime's socket set: the receiver's socket becomes readable, the
  // driver wakes it, and recv_from returns.
  rt.spawn_on(id, async move {
    slates_rt::futures::sleep(5_000_000).await;
    let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let _ = sender.send_to(b"ping", target);
  })
  .unwrap();

  match rx.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok((bytes, from))) => {
      assert_eq!(bytes, b"ping", "the datagram's bytes arrived");
      assert_eq!(from.ip(), &Ipv4Addr::LOCALHOST, "from a loopback sender");
    }
    Ok(Err(e)) => panic!("recv_from failed: {e:?}"),
    Err(e) => {
      let counters = rt.shutdown();
      panic!("timed out ({e}); counters {counters:#?}");
    }
  }
  rt.shutdown();
}

/// The simulated UDP fabric delivers deterministically at N=1 (§4.10a "sim arm first"): a receiver
/// awaits recv_from (registering fabric interest), a sender delivers, and the datagram arrives — the
/// whole plane with no OS network, driven to idle. Uses `slates_rt::sim::SimRuntime`.
#[test]
fn a_simulated_udp_datagram_is_received() {
  use slates_rt::sim::SimRuntime;

  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (port_tx, port_rx) = channel();
  let (result_tx, result_rx) = channel();

  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      // The receiver's bound port, told to the sender before awaiting.
      let _ = port_tx.send(socket.local_addr().unwrap().port());
      let mut buf = [0u8; 64];
      let outcome = socket
        .recv_from(&mut buf)
        .await
        .map(|(n, from)| (buf[..n].to_vec(), from));
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim
    .spawn_on(id, async move {
      // The receiver runs first (spawn order) and sends its port before awaiting, so it is ready.
      let port = loop {
        if let Ok(p) = port_rx.try_recv() {
          break p;
        }
        slates_rt::futures::sleep(1_000).await;
      };
      let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = sender.send_to(b"simping", SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok((bytes, from))) => {
      assert_eq!(bytes, b"simping", "the simulated datagram arrived");
      assert_ne!(from.port(), 0, "with the sender's fabric port");
    }
    other => panic!("the simulated recv did not complete: {other:?}"),
  }
}

// ── The fabric's latency model (§4.8 A-9: "independently delayed … messages") ────────────────────────

/// Shape: the one-way delay of the modelled inter-region path — 80 ms, half the 162 ms P50 round trip
/// Microsoft publishes for Japan East → East US (30 days ending 2026-07-30; `docs/wip/wan-timeout.md`).
const ONE_WAY_NS: u64 = 80_000_000;
/// Shape: the path's jitter, ± 20 ms around the one-way delay — a quarter of it, the spread the
/// WAN proof was asked for.
const JITTER_NS: u64 = 20_000_000;
/// Shape: how far apart the sender spaces its datagrams — 5 ms, well inside the ± 20 ms jitter, so a
/// model that allowed reordering would overtake often and one that keeps a flow in order is provably
/// clamping rather than merely lucky.
const SEND_GAP_NS: u64 = 5_000_000;
/// Shape: the datagrams one flow sends — enough for the seeded jitter to draw an overtake when the model
/// allows it (a run at this count showed several), few enough to keep the run short.
const FLOW_LENGTH: usize = 32;

/// Sends `FLOW_LENGTH` datagrams `SEND_GAP_NS` apart, each stamped with its virtual send time, and returns
/// each datagram's (arrival, stamp) pair as the receiver saw them, in arrival order.
fn run_stamped_flow(seed: u64, delay: slates_rt::sim::SimDelay) -> Vec<(u64, u64)> {
  use slates_rt::sim::{SimRuntime, sim_udp_set_delay};

  let mut sim = SimRuntime::new(&config(), seed).unwrap();
  sim_udp_set_delay(delay);
  let id = sim.shard_ids()[0];
  let (port_tx, port_rx) = channel();
  let (result_tx, result_rx) = channel();

  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = port_tx.send(socket.local_addr().unwrap().port());
      let mut arrivals = Vec::with_capacity(FLOW_LENGTH);
      let mut buf = [0u8; 8];
      for _ in 0..FLOW_LENGTH {
        let (n, _) = socket.recv_from(&mut buf).await.unwrap();
        assert_eq!(n, 8, "a whole stamp arrived");
        let stamp = u64::from_le_bytes(buf);
        arrivals.push((slates_rt::futures::now_ns(), stamp));
      }
      let _ = result_tx.send(arrivals);
    })
    .unwrap();

  sim
    .spawn_on(id, async move {
      let port = loop {
        if let Ok(p) = port_rx.try_recv() {
          break p;
        }
        slates_rt::futures::sleep(1_000).await;
      };
      let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let dest = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
      for _ in 0..FLOW_LENGTH {
        let stamp = slates_rt::futures::now_ns();
        let _ = sender.send_to(&stamp.to_le_bytes(), dest);
        slates_rt::futures::sleep(SEND_GAP_NS).await;
      }
    })
    .unwrap();

  sim.run_until_idle();
  result_rx
    .try_recv()
    .expect("the receiver saw the whole flow")
}

/// AC (§4.3 the simulation driver; §4.8 A-9 "independently delayed … messages"): over a modelled path of
/// 80 ms ± 20 ms one way, every datagram arrives on the virtual clock between 60 ms and 100 ms after it was
/// sent — never before its delay, never past its jitter — and the flow arrives in the order it was sent
/// even though the sends are spaced closer than the jitter (the model keeps a flow in order unless told
/// otherwise). The zero-delay fabric is the test above, unchanged.
#[test]
fn a_delayed_simulated_datagram_arrives_within_its_jitter_and_in_order() {
  use slates_rt::sim::SimDelay;

  let arrivals = run_stamped_flow(7, SimDelay::in_order(ONE_WAY_NS, JITTER_NS));
  assert_eq!(arrivals.len(), FLOW_LENGTH, "every datagram arrived");
  let mut seen_below_delay_floor = false;
  for (arrived, stamp) in &arrivals {
    let flight = arrived.saturating_sub(*stamp);
    assert!(
      (ONE_WAY_NS - JITTER_NS..=ONE_WAY_NS + JITTER_NS).contains(&flight),
      "a datagram flew {flight} ns; the path is {ONE_WAY_NS} ± {JITTER_NS} ns"
    );
    seen_below_delay_floor |= flight < ONE_WAY_NS;
  }
  assert!(
    seen_below_delay_floor && arrivals.iter().any(|(a, s)| a - s > ONE_WAY_NS),
    "the jitter was drawn on both sides of the delay (non-vacuity: the model jittered)"
  );
  let stamps: Vec<u64> = arrivals.iter().map(|(_, stamp)| *stamp).collect();
  let mut sorted = stamps.clone();
  sorted.sort_unstable();
  assert_eq!(stamps, sorted, "the flow arrived in send order");
}

/// AC (§4.8 A-9 "reordered messages"): when the model says a path may reorder, the seeded jitter overtakes
/// at least once within the flow — the same sends, spaced closer than the jitter, now arrive out of order —
/// so a consumer's reorder handling can be exercised deterministically.
#[test]
fn a_reordering_path_overtakes_within_one_flow() {
  use slates_rt::sim::SimDelay;

  let arrivals = run_stamped_flow(7, SimDelay::reordering(ONE_WAY_NS, JITTER_NS));
  assert_eq!(arrivals.len(), FLOW_LENGTH, "every datagram arrived");
  let stamps: Vec<u64> = arrivals.iter().map(|(_, stamp)| *stamp).collect();
  let overtakes = stamps.windows(2).filter(|pair| pair[1] < pair[0]).count();
  assert!(
    overtakes > 0,
    "the reordering model let a later datagram overtake an earlier one at least once"
  );
}

/// AC (D-20, deterministic simulation): the jitter is drawn from the simulation's seeded generator, so two
/// runs with one seed produce the same arrival times to the nanosecond, and a different seed draws a
/// different sequence — a failure history replays exactly.
#[test]
fn the_jitter_is_seeded_so_a_run_replays_exactly() {
  use slates_rt::sim::SimDelay;

  let first = run_stamped_flow(7, SimDelay::in_order(ONE_WAY_NS, JITTER_NS));
  let again = run_stamped_flow(7, SimDelay::in_order(ONE_WAY_NS, JITTER_NS));
  assert_eq!(first, again, "one seed, one history");
  let other = run_stamped_flow(8, SimDelay::in_order(ONE_WAY_NS, JITTER_NS));
  assert_ne!(first, other, "another seed draws another jitter sequence");
}
