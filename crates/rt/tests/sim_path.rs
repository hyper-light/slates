//! The simulated fabric's network model, driven through the runtime's own `UdpSocket` (§4.10a; the
//! constrained-link design, `docs/wip/research/nfs-transport-constrained-links.md` §5 and §9): a
//! bottleneck serializes at its rate into a bounded drop-tail queue shared by the flows through it; a
//! path loses datagrams at random or in bursts, drops those above its MTU; a receiver's buffer is
//! bounded; a NAT's mapping expires and rebinds. Every expectation below is the closed form of the
//! model's rule (the serialization time, the queue arithmetic, a binomial bound), not a replay of the
//! implementation — the session plane's congestion, loss-recovery, path-MTU and migration work is only
//! as honest as this network is.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, Sender, channel};

use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::{
  PPM, SimFabricStats, SimLink, SimLoss, SimNat, SimPath, SimRuntime, sim_udp_add_link,
  sim_udp_set_nat, sim_udp_set_pair_path, sim_udp_stats,
};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 1_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// Shape: a datagram of 1000 bytes on an 8 Mbit/s link occupies it for exactly one millisecond, so every
/// expected time below is a whole number of milliseconds.
const DATAGRAM: usize = 1000;
/// Shape: the link rate that makes one [`DATAGRAM`] take one millisecond.
const RATE: u64 = 8_000_000;
/// Shape: one millisecond, the serialization time of one datagram at [`RATE`].
const MS: u64 = 1_000_000;
/// Shape: the propagation delay after the link, ten milliseconds.
const PROPAGATION: u64 = 10 * MS;

/// One datagram as the receiver saw it: the virtual arrival time, the sequence number the sender wrote
/// in its first eight bytes, and the source port it arrived from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Arrival {
  at: u64,
  sequence: u64,
  from: u16,
}

/// A datagram whose first eight bytes are `sequence`, padded to `len`.
fn datagram(sequence: u64, len: usize) -> Vec<u8> {
  let mut bytes = vec![0u8; len.max(8)];
  bytes[..8].copy_from_slice(&sequence.to_le_bytes());
  bytes
}

/// Spawns a receiver that binds, reports its port, and forwards every datagram it reads (never stopping;
/// the simulation ends when nothing is left to happen). With `reads` false it binds and never reads — a
/// receiver whose buffer fills.
fn spawn_receiver(sim: &mut SimRuntime, reads: bool) -> (Receiver<u16>, Receiver<Arrival>) {
  let id = sim.shard_ids()[0];
  let (port_tx, port_rx) = channel();
  let (arrival_tx, arrival_rx) = channel();
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = port_tx.send(socket.local_addr().unwrap().port());
      if !reads {
        // Hold the socket open, unread, until the simulation has nothing else to do.
        core::future::pending::<()>().await;
      }
      let mut buf = vec![0u8; 64 * 1024];
      loop {
        let (n, from) = socket.recv_from(&mut buf).await.unwrap();
        assert!(n >= 8, "a stamped datagram arrived whole");
        let sequence = u64::from_le_bytes(buf[..8].try_into().unwrap());
        let _ = arrival_tx.send(Arrival {
          at: slates_rt::futures::now_ns(),
          sequence,
          from: from.port(),
        });
      }
    })
    .unwrap();
  (port_rx, arrival_rx)
}

/// Spawns a sender that binds, configures the paths through `setup(own port, receiver port)`, then sends
/// each `(at, len)` of `schedule` at virtual time `at` with the next sequence number.
fn spawn_sender(
  sim: &mut SimRuntime,
  receiver: u16,
  setup: impl FnOnce(u16, u16) + Send + 'static,
  schedule: Vec<(u64, usize)>,
  first_sequence: u64,
) {
  let id = sim.shard_ids()[0];
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      setup(socket.local_addr().unwrap().port(), receiver);
      let dest = SocketAddrV4::new(Ipv4Addr::LOCALHOST, receiver);
      for (index, (at, len)) in schedule.into_iter().enumerate() {
        let now = slates_rt::futures::now_ns();
        if at > now {
          slates_rt::futures::sleep(at - now).await;
        }
        let sequence = first_sequence + index as u64;
        let _ = socket.send_to(&datagram(sequence, len), dest).unwrap();
      }
    })
    .unwrap();
}

/// Runs the simulation to rest and collects what the receiver saw, with the fabric's counters.
fn finish(sim: &mut SimRuntime, arrivals: &Receiver<Arrival>) -> (Vec<Arrival>, SimFabricStats) {
  sim.run_until_idle();
  (arrivals.try_iter().collect(), sim_udp_stats())
}

/// A receiver's port once its task has run far enough to bind.
fn port(sim: &mut SimRuntime, ports: &Receiver<u16>) -> u16 {
  sim.run_until_idle();
  ports.try_recv().expect("the receiver bound")
}

/// A burst of `count` datagrams of `len` bytes all sent at time zero.
fn burst(count: usize, len: usize) -> Vec<(u64, usize)> {
  vec![(0, len); count]
}

/// AC (research §9, the bottleneck): datagrams sent back to back through a link are serialized at its
/// rate — the k-th of a burst (from zero) leaves the link after `(k+1)` serialization times and arrives
/// one propagation delay later, exactly.
#[test]
fn a_bottleneck_serializes_a_burst_at_its_rate() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let (ports, arrivals) = spawn_receiver(&mut sim, true);
  let receiver = port(&mut sim, &ports);
  let setup = move |from: u16, to: u16| {
    let link = sim_udp_add_link(SimLink {
      rate_bits_per_second: RATE,
      queue_bytes: 1_000_000,
    });
    sim_udp_set_pair_path(from, to, SimPath::in_order(PROPAGATION, 0).through(link));
  };
  spawn_sender(&mut sim, receiver, setup, burst(20, DATAGRAM), 0);
  let (seen, stats) = finish(&mut sim, &arrivals);
  assert_eq!(seen.len(), 20, "every datagram fit the queue");
  for (k, arrival) in seen.iter().enumerate() {
    assert_eq!(arrival.sequence, k as u64, "in send order");
    assert_eq!(
      arrival.at,
      (k as u64 + 1) * MS + PROPAGATION,
      "datagram {k} left after {} serialization times",
      k + 1
    );
  }
  assert_eq!(stats.dropped_queue, 0);
  assert_eq!(
    stats.peak_queue_bytes,
    19 * DATAGRAM as u64,
    "the last of the burst waited behind the other nineteen"
  );
}

/// AC (research §2.1, drop-tail congestion loss): a burst larger than the queue keeps exactly what fits —
/// a datagram is admitted while the backlog ahead of it plus itself is within the queue — and drops the
/// tail, counted as queue drops.
#[test]
fn a_full_queue_drops_the_tail_of_a_burst() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let (ports, arrivals) = spawn_receiver(&mut sim, true);
  let receiver = port(&mut sim, &ports);
  let setup = move |from: u16, to: u16| {
    let link = sim_udp_add_link(SimLink {
      rate_bits_per_second: RATE,
      queue_bytes: 10 * DATAGRAM as u64,
    });
    sim_udp_set_pair_path(from, to, SimPath::in_order(PROPAGATION, 0).through(link));
  };
  spawn_sender(&mut sim, receiver, setup, burst(30, DATAGRAM), 0);
  let (seen, stats) = finish(&mut sim, &arrivals);
  let sequences: Vec<u64> = seen.iter().map(|a| a.sequence).collect();
  assert_eq!(
    sequences,
    (0..10).collect::<Vec<u64>>(),
    "the ten that fit arrived"
  );
  assert_eq!(stats.dropped_queue, 20, "the twenty past the queue dropped");
}

/// AC (research §5.3, a shared bottleneck): two flows sending at once through one link share its capacity
/// — together they drain at the link's rate, one datagram per serialization time, so the last arrives
/// after all forty have been serialized, not after twenty as either alone would.
#[test]
fn two_flows_through_one_link_share_its_capacity() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let (ports, arrivals) = spawn_receiver(&mut sim, true);
  let receiver = port(&mut sim, &ports);
  let link = std::sync::OnceLock::<slates_rt::sim::SimLinkId>::new();
  let link: &'static _ = Box::leak(Box::new(link));
  let setup_first = move |from: u16, to: u16| {
    let id = *link.get_or_init(|| {
      sim_udp_add_link(SimLink {
        rate_bits_per_second: RATE,
        queue_bytes: 1_000_000,
      })
    });
    sim_udp_set_pair_path(from, to, SimPath::in_order(PROPAGATION, 0).through(id));
  };
  spawn_sender(&mut sim, receiver, setup_first, burst(20, DATAGRAM), 0);
  spawn_sender(&mut sim, receiver, setup_first, burst(20, DATAGRAM), 1000);
  let (seen, _) = finish(&mut sim, &arrivals);
  assert_eq!(seen.len(), 40);
  let last = seen.iter().map(|a| a.at).max().unwrap();
  assert_eq!(
    last,
    40 * MS + PROPAGATION,
    "forty datagrams took forty serialization times"
  );
  let mut gaps: Vec<u64> = seen.windows(2).map(|w| w[1].at - w[0].at).collect();
  gaps.dedup();
  assert_eq!(
    gaps,
    vec![MS],
    "the link emitted one datagram per millisecond"
  );
}

/// Sends `count` small datagrams through `loss` (no bottleneck) and returns which sequence numbers were
/// lost, in order, with the counters.
fn lost_sequences(seed: u64, loss: SimLoss, count: usize) -> (Vec<u64>, SimFabricStats) {
  let mut sim = SimRuntime::new(&config(), seed).unwrap();
  let (ports, arrivals) = spawn_receiver(&mut sim, true);
  let receiver = port(&mut sim, &ports);
  let setup = move |from: u16, to: u16| {
    sim_udp_set_pair_path(from, to, SimPath::in_order(MS, 0).with_loss(loss));
  };
  // Spaced so the receiver drains between sends and its buffer never drops anything.
  let schedule: Vec<(u64, usize)> = (0..count).map(|k| (k as u64 * MS, 16)).collect();
  spawn_sender(&mut sim, receiver, setup, schedule, 0);
  let (seen, stats) = finish(&mut sim, &arrivals);
  let delivered: std::collections::BTreeSet<u64> = seen.iter().map(|a| a.sequence).collect();
  let lost = (0..count as u64)
    .filter(|s| !delivered.contains(s))
    .collect();
  (lost, stats)
}

/// The mean length of the runs of consecutive lost sequence numbers.
fn mean_run(lost: &[u64]) -> f64 {
  if lost.is_empty() {
    return 0.0;
  }
  let runs = 1 + lost.windows(2).filter(|w| w[1] != w[0] + 1).count();
  lost.len() as f64 / runs as f64
}

/// Shape: datagrams per loss scenario — enough that a one-percent process loses about a hundred, so the
/// binomial bound below is tight and a burst process forms dozens of bursts.
const LOSS_SAMPLE: usize = 10_000;

/// AC (research §2.1, random loss): an independent loss process of 1% loses a binomial count — within
/// five standard deviations of `n·p` (σ = √(n·p·(1−p)) ≈ 9.95 for n = 10,000) — every loss counted, and
/// its losses are scattered: the mean run of consecutive losses is close to one.
#[test]
fn random_loss_loses_its_rate_scattered() {
  let (lost, stats) = lost_sequences(3, SimLoss::random(PPM / 100), LOSS_SAMPLE);
  let expected = LOSS_SAMPLE as f64 * 0.01;
  let sigma = (LOSS_SAMPLE as f64 * 0.01 * 0.99).sqrt();
  assert!(
    (lost.len() as f64 - expected).abs() <= 5.0 * sigma,
    "{} lost of {LOSS_SAMPLE}; expected {expected} ± {}",
    lost.len(),
    5.0 * sigma
  );
  assert_eq!(stats.dropped_loss, lost.len() as u64, "every loss counted");
  assert!(
    mean_run(&lost) < 1.5,
    "random losses are scattered (mean run {})",
    mean_run(&lost)
  );
}

/// AC (research §5.4, burst loss): a Gilbert–Elliott process that enters a burst with 0.25% per datagram
/// and leaves with 25% loses in runs averaging about four — `1 / 0.25` — where independent loss at the
/// same overall rate loses one at a time.
#[test]
fn burst_loss_loses_in_runs() {
  let (lost, _) = lost_sequences(3, SimLoss::bursty(PPM / 400, PPM / 4, PPM), LOSS_SAMPLE);
  let run = mean_run(&lost);
  assert!(lost.len() > 20, "bursts happened ({} lost)", lost.len());
  assert!(
    (2.5..=6.0).contains(&run),
    "burst losses run about four long (mean run {run})"
  );
}

/// AC (RFC 8899's black hole): a path with an MTU delivers a datagram at or below it and silently drops
/// one above it, counted.
#[test]
fn a_datagram_above_the_path_mtu_is_dropped() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let (ports, arrivals) = spawn_receiver(&mut sim, true);
  let receiver = port(&mut sim, &ports);
  let setup = move |from: u16, to: u16| {
    sim_udp_set_pair_path(from, to, SimPath::in_order(MS, 0).with_mtu(1400));
  };
  spawn_sender(
    &mut sim,
    receiver,
    setup,
    vec![(0, 1200), (MS, 1500), (2 * MS, 1400)],
    0,
  );
  let (seen, stats) = finish(&mut sim, &arrivals);
  let sequences: Vec<u64> = seen.iter().map(|a| a.sequence).collect();
  assert_eq!(sequences, vec![0, 2], "the 1500-byte datagram was dropped");
  assert_eq!(stats.dropped_mtu, 1);
}

/// AC (a kernel socket's bounded buffer; Banned #8): a receiver that does not read holds at most its
/// buffer's worth of datagrams and the fabric drops the rest, counted — the mailbox never grows without
/// bound.
#[test]
fn an_unread_receiver_buffers_at_most_its_bound() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let (ports, arrivals) = spawn_receiver(&mut sim, false);
  let receiver = port(&mut sim, &ports);
  spawn_sender(&mut sim, receiver, |_, _| {}, burst(300, DATAGRAM), 0);
  let (_, stats) = finish(&mut sim, &arrivals);
  let fit = (slates_rt::sim::SIM_RECV_BUFFER_BYTES / DATAGRAM) as u64;
  assert_eq!(stats.delivered, fit, "the buffer held what fits");
  assert_eq!(
    stats.dropped_receive_buffer,
    300 - fit,
    "and dropped the rest"
  );
}

/// AC (RFC 4787 mapping lifetime; RFC 9000 §9.3 rebinding): a socket behind a NAT is reached at the
/// external port its datagrams come from while the mapping is refreshed; after the mapping idles past its
/// timeout, a datagram to that port is dropped at the NAT, and the socket's next datagram arrives from a
/// fresh external port.
#[test]
fn a_nat_mapping_expires_and_the_next_datagram_rebinds() {
  /// Shape: the NAT's idle timeout, 30 ms of virtual time.
  const IDLE: u64 = 30 * MS;
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (ports, arrivals) = spawn_receiver(&mut sim, true);
  let peer = port(&mut sim, &ports);
  let (inside_tx, inside_rx): (Sender<(u64, u64)>, _) = channel();
  sim
    .spawn_on(id, async move {
      let inside = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      sim_udp_set_nat(
        inside.local_addr().unwrap().port(),
        SimNat {
          idle_timeout_ns: IDLE,
        },
      );
      let peer_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, peer);
      inside.send_to(&datagram(1, 16), peer_addr).unwrap();
      // Idle past the timeout, then speak again.
      slates_rt::futures::sleep(2 * IDLE).await;
      inside.send_to(&datagram(2, 16), peer_addr).unwrap();
      let mut buf = [0u8; 64];
      let (_, _) = inside.recv_from(&mut buf).await.unwrap();
      let _ = inside_tx.send((
        u64::from_le_bytes(buf[..8].try_into().unwrap()),
        slates_rt::futures::now_ns(),
      ));
    })
    .unwrap();
  sim.run_until_idle();
  let seen: Vec<Arrival> = arrivals.try_iter().collect();
  assert_eq!(seen.len(), 2);
  let (first, second) = (seen[0], seen[1]);
  assert_ne!(
    first.from, second.from,
    "the datagram after the idle timeout came from a rebound port"
  );
  // The peer answers both observed addresses: the stale one is dropped at the NAT, the fresh one arrives.
  let (answer_tx, answer_rx) = channel();
  answer_tx.send((first.from, second.from)).unwrap();
  sim
    .spawn_on(id, async move {
      let (stale, fresh) = answer_rx.try_recv().unwrap();
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      socket
        .send_to(
          &datagram(10, 16),
          SocketAddrV4::new(Ipv4Addr::LOCALHOST, stale),
        )
        .unwrap();
      socket
        .send_to(
          &datagram(11, 16),
          SocketAddrV4::new(Ipv4Addr::LOCALHOST, fresh),
        )
        .unwrap();
    })
    .unwrap();
  sim.run_until_idle();
  let (answered, _) = inside_rx
    .try_recv()
    .expect("the inside socket was answered");
  assert_eq!(answered, 11, "only the answer to the live mapping arrived");
  assert_eq!(
    sim_udp_stats().dropped_nat,
    1,
    "the stale mapping's answer was dropped"
  );
}

/// AC (D-20): a scenario with jitter, a bottleneck and random loss replays exactly from its seed — the
/// same arrivals to the nanosecond and the same counters — and another seed draws another history.
#[test]
fn a_lossy_bottleneck_scenario_replays_from_its_seed() {
  let run = |seed: u64| {
    let mut sim = SimRuntime::new(&config(), seed).unwrap();
    let (ports, arrivals) = spawn_receiver(&mut sim, true);
    let receiver = port(&mut sim, &ports);
    let setup = move |from: u16, to: u16| {
      let link = sim_udp_add_link(SimLink {
        rate_bits_per_second: RATE,
        queue_bytes: 8 * DATAGRAM as u64,
      });
      sim_udp_set_pair_path(
        from,
        to,
        SimPath::reordering(PROPAGATION, 2 * MS)
          .through(link)
          .with_loss(SimLoss::random(PPM / 20)),
      );
    };
    let schedule: Vec<(u64, usize)> = (0..200u64).map(|k| (k * MS / 2, DATAGRAM)).collect();
    spawn_sender(&mut sim, receiver, setup, schedule, 0);
    finish(&mut sim, &arrivals)
  };
  let first = run(9);
  assert_eq!(first, run(9), "one seed, one history");
  assert_ne!(first, run(10), "another seed, another history");
  assert!(
    first.1.dropped_queue > 0 && first.1.dropped_loss > 0,
    "the scenario exercised both drop causes: {:?}",
    first.1
  );
}

/// RFC 8899 §4.4 (§4.10a path MTU discovery; `sim_udp_set_interface_mtu`): a modelled host interface
/// refuses at the send any datagram larger than its MTU, with the same too-large refusal a real host with
/// don't-fragment set returns — so a prober reads it identically — while a datagram within it is sent; with
/// the model removed, every size is sent again. Do X (an Ethernet interface, 1,500), expect Y (1,400 bytes
/// sent, 1,600 refused `is_message_too_large`, 1,600 sent once the model is removed).
#[test]
fn a_modelled_interface_refuses_a_datagram_past_its_mtu() {
  /// Shape: an Ethernet interface's MTU.
  const INTERFACE_MTU: usize = 1_500;
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, socket.local_addr().unwrap().port());
      slates_rt::sim::sim_udp_set_interface_mtu(Some(INTERFACE_MTU));
      let within = socket.send_to(&[0u8; 1_400], target).map(|_| ());
      let past = socket
        .send_to(&[0u8; 1_600], target)
        .map_err(|refusal| refusal.is_message_too_large());
      slates_rt::sim::sim_udp_set_interface_mtu(None);
      let unmodelled = socket.send_to(&[0u8; 1_600], target).map(|_| ());
      let _ = tx.send((within, past, unmodelled));
    })
    .unwrap();
  sim.run_until_idle();
  let (within, past, unmodelled) = rx.try_recv().unwrap();
  assert!(within.is_ok(), "a datagram within the interface is sent");
  assert_eq!(past, Err(true), "past the interface: refused as too large");
  assert!(unmodelled.is_ok(), "without the model every size is sent");
}
