//! The control plane end to end over the runtime's UDP substrate (§4.10a): a node derives its keys
//! from its control secret, seals a control datagram, and sends it over a `UdpSocket`; the peer
//! receives the bytes and runs the acceptance enforcement order (`accept`) with its keyring,
//! recovering the authentic datagram. This is every built piece — the key schedule, the AES-256-GCM
//! seal, the wire codec, and the acceptance order — wired over the real datagram path and proven
//! deterministically at N=1 on the simulation UDP fabric (no OS network). Test by use (R5).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::mpsc::channel;

// The address types come through `rustix::net` (the standard types re-exported), honouring the
// host-path wall's `std::net` guard, as the runtime's own UDP test does.
use rustix::net::{Ipv4Addr, SocketAddrV4};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::accept::{Keyring, Refusal, accept};
use slates_transport::schedule::{Direction, KeySchedule};
use slates_transport::seal::Opener;
use slates_transport::{ControlDatagram, Envelope};

/// The control secret both ends share (enrollment mints and distributes this; injected here). Both
/// derive the same schedule from it, so the sender's sealer and the receiver's opener match.
const SECRET: [u8; 32] = [0x33; 32];
const SENDER: u64 = 0xABCD_1234_5678_9A00;
const EPOCH: u32 = 2;
const CHANNEL: u32 = 1;
/// The datagram cap for the test (a real cap is derived from the path MTU).
const MAX_LEN: usize = 2048;

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

/// A keyring keyed for one peer, as enrollment will eventually populate.
struct MapKeyring {
  openers: BTreeMap<(u64, u32), Opener>,
}

impl Keyring for MapKeyring {
  fn opener(&mut self, sender: u64, key_epoch: u32) -> Option<&mut Opener> {
    self.openers.get_mut(&(sender, key_epoch))
  }
}

fn sample() -> ControlDatagram {
  ControlDatagram {
    sender: SENDER,
    key_epoch: EPOCH,
    envelope: Envelope {
      kind: 3,
      class: 1,
      flags: 0,
      epoch: 11,
      hlc: 0xFEED,
      request_id: 21,
    },
    body: b"a control datagram over the wire".to_vec(),
  }
}

/// A sealed control datagram travels over the (simulated) UDP fabric and is accepted authentic: the
/// receiver, keyed for the sender through the shared schedule, recovers exactly what was sent.
#[test]
fn a_sealed_control_datagram_travels_over_udp_and_is_accepted() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (port_tx, port_rx) = channel();
  let (result_tx, result_rx) = channel();

  // The receiver: bind, tell the sender the port, receive one datagram, run the acceptance order.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = port_tx.send(socket.local_addr().unwrap().port());
      let mut buf = [0u8; MAX_LEN];
      let outcome = match socket.recv_from(&mut buf).await {
        Ok((n, _from)) => {
          let schedule = KeySchedule::from_control_secret(&SECRET);
          let mut keyring = MapKeyring {
            openers: BTreeMap::new(),
          };
          keyring.openers.insert(
            (SENDER, EPOCH),
            schedule.opener(SENDER, EPOCH, Direction::Initiator, CHANNEL),
          );
          accept(&buf[..n], MAX_LEN, &mut keyring).map_err(|e: Refusal| e.to_string())
        }
        Err(e) => Err(format!("recv failed: {e:?}")),
      };
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  // The sender: wait for the receiver's port, seal a datagram, send it.
  sim
    .spawn_on(id, async move {
      let port = loop {
        if let Ok(p) = port_rx.try_recv() {
          break p;
        }
        slates_rt::futures::sleep(1_000).await;
      };
      let schedule = KeySchedule::from_control_secret(&SECRET);
      let mut sealer = schedule.sealer(SENDER, EPOCH, Direction::Initiator, CHANNEL);
      let wire = sample().encode_sealed(&mut sealer).unwrap();
      let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = sender.send_to(&wire, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok(datagram)) => assert_eq!(datagram, sample(), "the authentic datagram arrived intact"),
    other => panic!("the control datagram was not accepted over UDP: {other:?}"),
  }
}
