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

/// Shape: how long a test waits for the runtime to shut down before it reports the shard's state and
/// fails, rather than hanging the binary. A shutdown cancels a handful of tasks: milliseconds.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);
/// Shape: the gap between the two pulse reads a stalled shutdown reports, long enough for a running
/// shard to step many times and a parked one to wake at least once for a timer.
const PULSE_GAP: Duration = Duration::from_millis(200);

/// One read of a shard's pulse: steps, waits, spawns, completions, and whether it has exited.
fn pulse(shard: u16) -> String {
  slates_rt::registry::entry(shard).map_or_else(
    || "no entry".to_owned(),
    |entry| {
      format!(
        "steps {} waits {} spawns {} completed {} exited {}",
        entry.pulse.steps(),
        entry.pulse.waits(),
        entry.pulse.spawns(),
        entry.pulse.completed(),
        entry.exited.load(std::sync::atomic::Ordering::Acquire)
      )
    },
  )
}

/// Shuts the runtime down, or reports the shard's pulse (read twice, [`PULSE_GAP`] apart: steps that
/// climb are a shard spinning, waits that climb a shard parking and waking, neither a shard held inside
/// one call) when the shutdown outlasts [`SHUTDOWN_WAIT`], instead of hanging the test binary.
fn shutdown_within(rt: Runtime, context: &str) -> Result<(), String> {
  let shard = rt.shard_ids()[0];
  let (done_tx, done_rx) = channel();
  let stopper = std::thread::spawn(move || {
    let counters = rt.shutdown();
    let _ = done_tx.send(counters);
  });
  if done_rx.recv_timeout(SHUTDOWN_WAIT).is_ok() {
    stopper.join().unwrap();
    return Ok(());
  }
  let first = pulse(shard.0);
  // The gap is a wait on the stopper, so a shutdown that ends inside it is still a shutdown.
  if done_rx.recv_timeout(PULSE_GAP).is_ok() {
    stopper.join().unwrap();
    return Ok(());
  }
  let second = pulse(shard.0);
  Err(format!(
    "{context}: the runtime did not shut down within {SHUTDOWN_WAIT:?}; pulse {first}, then {second}"
  ))
}

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
    wake_tracking: None,
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
  let (sent_tx, sent_rx) = channel();
  rt.spawn_on(id, async move {
    slates_rt::futures::sleep(5_000_000).await;
    let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let _ = sent_tx.send(sender.send_to(b"ping", target));
  })
  .unwrap();

  match rx.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok((bytes, from))) => {
      assert_eq!(bytes, b"ping", "the datagram's bytes arrived");
      assert_eq!(from.ip(), &Ipv4Addr::LOCALHOST, "from a loopback sender");
    }
    Ok(Err(e)) => panic!("recv_from failed: {e:?}"),
    Err(e) => {
      let sent = sent_rx.try_recv();
      let before = pulse(id.0);
      let shutdown = shutdown_within(rt, "after the receive timed out");
      panic!("timed out ({e}); the send: {sent:?}; pulse at the timeout: {before}; {shutdown:?}");
    }
  }
  shutdown_within(rt, "after the datagram arrived").unwrap();
}

/// AC (§4.3; docs/bugs/2026-09-14-epoll-readiness-re-add-eexist.md): a socket is awaited **again** after
/// its first datagram — the shape of every receive loop (the fleet's serve sockets, a mount's stream) —
/// and the driver re-arms its readiness rather than refusing the second registration. A receiver awaits
/// two datagrams on one socket, each sent only after it blocked (so each await registers with the
/// driver); the second must arrive as the first did. On the epoll driver (Linux with io_uring refused, as
/// under a container's default seccomp profile) the second registration was `EEXIST` before the fix, so
/// the second await failed and the loop ended; kqueue and io_uring re-arm per await and passed either way.
#[test]
fn a_second_receive_on_the_same_socket_registers_readiness_again() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let receiver = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let target = receiver.local_addr().unwrap();

  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let mut buf = [0u8; 64];
    let first = receiver
      .recv_from(&mut buf)
      .await
      .map(|(n, _)| buf[..n].to_vec());
    let _ = tx.send(first);
    let second = receiver
      .recv_from(&mut buf)
      .await
      .map(|(n, _)| buf[..n].to_vec());
    let _ = tx.send(second);
  })
  .unwrap();
  // Two sends, each after the receiver has blocked on its await (a runtime sleep apart).
  let (sent_tx, sent_rx) = channel();
  rt.spawn_on(id, async move {
    let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    slates_rt::futures::sleep(5_000_000).await;
    let _ = sent_tx.send(sender.send_to(b"first", target));
    slates_rt::futures::sleep(20_000_000).await;
    let _ = sent_tx.send(sender.send_to(b"second", target));
  })
  .unwrap();

  for expected in [&b"first"[..], &b"second"[..]] {
    match rx.recv_timeout(Duration::from_secs(5)) {
      Ok(Ok(bytes)) => assert_eq!(bytes, expected),
      Ok(Err(e)) => panic!("recv_from of {expected:?} failed: {e:?}"),
      Err(e) => {
        let sent: Vec<_> = sent_rx.try_iter().collect();
        let before = pulse(id.0);
        let shutdown = shutdown_within(rt, "after a receive timed out");
        panic!(
          "timed out waiting for {expected:?} ({e}); the sends: {sent:?}; pulse at the timeout: {before}; {shutdown:?}"
        );
      }
    }
  }
  shutdown_within(rt, "after both datagrams arrived").unwrap();
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
