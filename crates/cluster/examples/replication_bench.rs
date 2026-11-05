//! The replication latency benchmark (§4.8 "a record is sent to all candidates and commits at f+1
//! acknowledgements"; §4.10 live shipping): what one cross-node commit costs today, over real endpoints — TLS 1.3,
//! packet protection, the clocked connection — on the real runtime and real loopback sockets, one shard per node.
//!
//! Three rows, each a histogram of `samples` sequential operations after a warm-up:
//!
//! - `round-trip`: one request and its reply on a warm session to one holder (the holder answers at once). This is
//!   the floor any per-write replication over the fleet transport sits on.
//! - `commit poll=<ns>`: `commit_record` of a fresh register record (owner `1`, holders `2` and `3`, `f = 1`),
//!   placed at the owner's hold plus the first holder's binding acknowledgement, with the collection loop parking
//!   `<ns>` between checks unless a reply wakes it. The daemon parks a tenth of its 100 ms heartbeat
//!   (`fleet::POLL_PER_PERIOD`), 10 ms; the second poll is a hundredth of that.
//!
<<<<<<< HEAD
//! - `live ship <n> B`: one live-shipped write (A-200) of an `n`-byte publication: `Shipper::append`, then `ship` to
//!   both holders, each applying it through `serve_entry`, until it is durable (the owner and one holder hold it).
//!   One write at a time (a credit of one), at the daemon's poll.
//!
=======
>>>>>>> origin/main
//! The daemon ships a record only on its record period (100 ms), so a head waits up to one period before this
//! commit starts: that wait is not in these rows.
//!
//! `cargo run --release -p slates-cluster --example replication_bench [samples] [load-threads]` prints each row's
//! p50, p99, p999 and max in microseconds. `load-threads` spinning threads run beside the shards for the loaded
//! rows (default none). **Failures** (the process exits non-zero): a commit that does not place.

// Benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::too_many_lines,
  clippy::cast_precision_loss
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;

use rustls::pki_types::PrivateKeyDer;
<<<<<<< HEAD
use slates_cluster::live::{LIVE_ENTRY_STREAM, fold_late, serve_entry, ship};
use slates_cluster::{CommitBudget, RECORD_STREAM, commit_record};
use slates_db::live::{Replica, Shipper};
=======
use slates_cluster::{CommitBudget, RECORD_STREAM, commit_record};
>>>>>>> origin/main
use slates_db::register::{
  Acceptor, Authority, HostEpoch, HostId, ObjectId, Placement, Quorum, Record,
};
use slates_machine::profile::{MachineProfile, ProfileOptions};
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::connection::{ConnectionShape, Priority};
use slates_transport::endpoint::{Endpoint, MAX_PACKET_PAYLOAD};
use slates_transport::handshake::Identity;

/// Format: the TLS name every node presents.
const NAME: &str = "slates-node";
/// Shape: the default sample count per row: enough that a p999 rests on ten samples.
const DEFAULT_SAMPLES: usize = 10_000;
/// Shape: operations run before each row's timed samples, so handshake-era congestion state and cold caches are
/// behind it.
const WARM_UP: usize = 200;
/// Shape: the receive ceiling of each session: far above anything a record exchange holds.
const RECEIVE_CEILING: u64 = 4 << 20;
/// Shape: a commit's deadline: a second, far above any loopback round trip, so the deadline never decides a
/// sample; a commit not placed within it fails the run.
const DEADLINE_NS: u64 = 1_000_000_000;
/// Shape: the daemon's record commit poll: a tenth of its 100 ms heartbeat (`slates_server::fleet::HEARTBEAT_NS /
/// POLL_PER_PERIOD`).
const DAEMON_POLL_NS: u64 = 10_000_000;
/// Shape: the finer poll: a tenth of a millisecond, a hundredth of the daemon's, to show what the poll costs.
const FINE_POLL_NS: u64 = 100_000;
/// Shape: the tasks and timers each shard admits: far above the handful a node runs here.
const TASKS: usize = 256;
/// Shape: the daemon's latency budget its batch bound is calibrated against (`slates_server::config`, R9's 50 µs).
const LATENCY_BUDGET_NS: u64 = 50_000;
<<<<<<< HEAD
/// Shape: the write sizes a live-shipped entry is measured at: a metadata-sized delta, a page, a chunk window.
const LIVE_PAYLOADS: [usize; 3] = [64, 4096, 65_536];
/// Shape: the largest entry a holder decodes here: the largest payload with room for its header.
const LIVE_CAP: usize = 1 << 17;
=======
>>>>>>> origin/main
/// Format: the request kind of the bare round trip, distinct from [`RECORD_STREAM`].
const ECHO_KIND: u64 = 2;
/// Format: nanoseconds per microsecond.
const NS_PER_US: f64 = 1_000.0;
/// Format: the owner and its two candidate holders.
const OWNER: HostId = HostId(1);
const HOLDERS: [HostId; 2] = [HostId(2), HostId(3)];
/// Format: the one object every record names.
const OBJECT: ObjectId = ObjectId::new(OWNER, 7);

/// The daemon's runtime configuration for this machine (`DaemonConfig::derive`: `RuntimeConfig::from_profile` over a
/// full measured profile, the daemon's own latency budget), with one unpinned shard per node, as `--shards` sets it.
/// A hand-set configuration measured the harness: a one-second step budget let a busy shard harvest its sockets once a
/// second.
fn config() -> RuntimeConfig {
  let profile = MachineProfile::measure(ProfileOptions::default()).unwrap();
  let mut config = RuntimeConfig::from_profile(&profile, TASKS, TASKS, LATENCY_BUDGET_NS);
  config.shards = 3;
  config.pin = false;
  config.cores.clear();
  config
}

fn identity() -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let certificate = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    certificate.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

fn shape() -> ConnectionShape {
  ConnectionShape::for_frame_cap(MAX_PACKET_PAYLOAD, RECEIVE_CEILING)
}

fn authority() -> Authority {
  Authority {
    generation: 0,
    owner: OWNER,
  }
}

fn record(sequence: u64) -> Record {
  Record {
    owner: OWNER,
    object: OBJECT,
    sequence,
    epoch: HostEpoch(1),
    generation: 0,
    value: sequence.to_le_bytes().to_vec(),
  }
}

/// A row's nearest-rank percentiles, in microseconds.
fn row(name: &str, mut samples_ns: Vec<u64>) {
  samples_ns.sort_unstable();
  let rank = |per_mille: usize| {
    let at = (samples_ns.len() * per_mille).div_ceil(1000).max(1) - 1;
    samples_ns[at] as f64 / NS_PER_US
  };
  println!(
    "{name:<22} n={:<6} p50={:>9.1} p99={:>9.1} p999={:>9.1} max={:>9.1} us",
    samples_ns.len(),
    rank(500),
    rank(990),
    rank(999),
    samples_ns[samples_ns.len() - 1] as f64 / NS_PER_US,
  );
}

/// A holder: answers the echo kind at once and each record through its acceptor (as `serve_record` does), until
/// `stop`.
<<<<<<< HEAD
async fn hold(mut endpoint: Endpoint, host: HostId, stop: &'static AtomicBool, silent: bool) {
  endpoint.establish().await.unwrap();
  if silent {
    // A dead holder that kept its socket: it never answers again.
    while !stop.load(Ordering::Acquire) {
      slates_rt::futures::sleep(FINE_POLL_NS).await.unwrap();
    }
    return;
  }
  let mut acceptor = Acceptor::new(host, authority());
  // One replica per volume, found from the entry's object (its bytes after the owner word, `slates_db::live`'s
  // format), and the state it stands for.
  let mut replicas: std::collections::BTreeMap<Vec<u8>, (Replica, Vec<u8>)> =
    std::collections::BTreeMap::new();
=======
async fn hold(mut endpoint: Endpoint, host: HostId, stop: &'static AtomicBool) {
  endpoint.establish().await.unwrap();
  let mut acceptor = Acceptor::new(host, authority());
>>>>>>> origin/main
  while !stop.load(Ordering::Acquire) {
    let served = slates_rt::futures::within(
      FINE_POLL_NS,
      endpoint.serve_once(|kind, request| {
<<<<<<< HEAD
        if kind == LIVE_ENTRY_STREAM {
          let object = request.get(8..24).unwrap_or_default().to_vec();
          let id = ObjectId(object.as_slice().try_into().unwrap_or([0; 16]));
          let (replica, state) = replicas
            .entry(object)
            .or_insert_with(|| (Replica::new(OWNER, id), Vec::new()));
          serve_entry(replica, &request, LIVE_CAP, |entry| {
            state.clear();
            state.extend_from_slice(&entry.payload);
            true
          })
        } else if kind == RECORD_STREAM {
=======
        if kind == RECORD_STREAM {
>>>>>>> origin/main
          match Record::decode(&request) {
            Ok(record) => acceptor
              .accept(&record)
              .map(|ack| ack.encode())
              .unwrap_or_default(),
            Err(_) => Vec::new(),
          }
        } else {
          request
        }
      }),
    )
    .await;
    if matches!(served, Ok(Some(Err(_)))) {
      return;
    }
  }
}

fn main() {
  let samples: usize = std::env::args()
    .nth(1)
    .and_then(|value| value.parse().ok())
    .unwrap_or(DEFAULT_SAMPLES);
  let load_threads: usize = std::env::args()
    .nth(2)
    .and_then(|value| value.parse().ok())
    .unwrap_or(0);
<<<<<<< HEAD
  // `silent` as the third argument: holder 3 handshakes and never answers again (a dead replica), and only the rows
  // that need one holder are run.
  let silent = std::env::args().nth(3).is_some_and(|word| word == "silent");
=======
>>>>>>> origin/main
  static STOP: AtomicBool = AtomicBool::new(false);
  static LOAD_STOP: AtomicBool = AtomicBool::new(false);
  let load: Vec<_> = (0..load_threads)
    .map(|_| {
      std::thread::spawn(|| {
        let mut spin = 0u64;
        while !LOAD_STOP.load(Ordering::Relaxed) {
          spin = std::hint::black_box(spin.wrapping_add(1));
        }
      })
    })
    .collect();

  let rt = Runtime::start(&config()).unwrap();
  let shards = rt.shard_ids().to_vec();
  let owner_identity = identity();
  let owner_certificate = owner_identity.certificate();
  let (result_tx, result_rx) = channel();
  let mut dials = Vec::new();
  for (index, host) in HOLDERS.into_iter().enumerate() {
    let holder_identity = identity();
    let holder_certificate = holder_identity.certificate();
    let holder_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let owner_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let holder_address = holder_socket.local_addr().unwrap();
    let owner_address = owner_socket.local_addr().unwrap();
    let owner_certificate = owner_certificate.clone();
    rt.spawn_on(shards[index + 1], async move {
      let endpoint = Endpoint::server(
        holder_socket,
        owner_address,
        &holder_identity,
        std::slice::from_ref(&owner_certificate),
        shape(),
      )
      .unwrap();
<<<<<<< HEAD
      hold(endpoint, host, &STOP, silent && host == HOLDERS[1]).await;
=======
      hold(endpoint, host, &STOP).await;
>>>>>>> origin/main
    })
    .unwrap();
    dials.push((host, owner_socket, holder_address, holder_certificate));
  }
  rt.spawn_on(shards[0], async move {
    let mut sessions = Vec::new();
    for (host, socket, address, certificate) in dials {
      let mut endpoint = Endpoint::client(
        socket,
        address,
        &owner_identity,
        &certificate,
        NAME,
        shape(),
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      sessions.push((host, endpoint));
    }
    let mut rows: Vec<(String, Vec<u64>)> = Vec::new();

    let mut timed = Vec::with_capacity(samples);
    for at in 0..WARM_UP + samples {
      let started = slates_rt::futures::now_ns();
      let reply = sessions[0]
        .1
        .request(ECHO_KIND, Priority::Control, &[1u8; 64])
        .await
        .unwrap();
      let elapsed = slates_rt::futures::now_ns().saturating_sub(started);
      assert_eq!(reply.len(), 64, "the echo came back whole");
      if at >= WARM_UP {
        timed.push(elapsed);
      }
    }
    rows.push(("round-trip".to_owned(), timed));

    let mut owner_acceptor = Acceptor::new(OWNER, authority());
    let placement = Placement::of(&[OWNER, HOLDERS[0], HOLDERS[1]]);
    let mut sequence = 0u64;
<<<<<<< HEAD
    for poll_ns in [DAEMON_POLL_NS, FINE_POLL_NS]
      .into_iter()
      .filter(|_| !silent)
    {
=======
    for poll_ns in [DAEMON_POLL_NS, FINE_POLL_NS] {
>>>>>>> origin/main
      let mut timed = Vec::with_capacity(samples);
      let rounds = if poll_ns == DAEMON_POLL_NS {
        // A 10 ms poll makes each sample about 10 ms: a tenth of the samples tells the same story in a tenth of the
        // time.
        (samples / 10).max(1)
      } else {
        samples
      };
      for at in 0..WARM_UP.min(rounds) + rounds {
        sequence += 1;
        let started = slates_rt::futures::now_ns();
        let mut committed = commit_record(
          OWNER,
          &mut owner_acceptor,
          &placement,
          &record(sequence),
          Quorum { f: 1 },
          std::mem::take(&mut sessions),
          CommitBudget::hard(DEADLINE_NS, poll_ns),
        )
        .await;
        let elapsed = slates_rt::futures::now_ns().saturating_sub(started);
        assert!(committed.outcome.is_ok(), "a commit placed");
        owner_acceptor.compact_below(OBJECT, sequence);
        sessions = committed.reusable;
        // The straggler past the quorum hands its session back, waking this task as it does: wait for it outside the
        // timed span, parked (a spinning wait starves the shard's sockets of their harvest).
        loop {
          let (recovered, done) = committed.stragglers.recover();
          sessions.extend(recovered);
          if done {
            break;
          }
          slates_rt::futures::within(FINE_POLL_NS, slates_rt::futures::idle())
            .await
            .unwrap();
        }
        sessions.sort_by_key(|(host, _)| host.0);
        assert_eq!(sessions.len(), 2, "both sessions came back");
        if at >= WARM_UP.min(rounds) {
          timed.push(elapsed);
        }
      }
      rows.push((format!("commit poll={poll_ns}"), timed));
    }
<<<<<<< HEAD
    for (volume, payload_bytes) in (100u64..).zip(LIVE_PAYLOADS) {
      let mut shipper = Shipper::new(
        (OWNER, ObjectId::new(OWNER, volume), HostEpoch(1)),
        Quorum { f: 1 },
        &[OWNER, HOLDERS[0], HOLDERS[1]],
        None,
        (1, 1),
      );
      let payload = vec![7u8; payload_bytes];
      let mut late = Vec::new();
      let mut timed = Vec::with_capacity(samples);
      for at in 0..WARM_UP + samples {
        let started = slates_rt::futures::now_ns();
        let entry = shipper.append(payload.clone()).unwrap();
        let shipped = ship(
          &mut shipper,
          (LIVE_ENTRY_STREAM, &entry.encode()),
          Some(entry.position),
          std::mem::take(&mut sessions),
          CommitBudget::hard(DEADLINE_NS, DAEMON_POLL_NS),
        )
        .await;
        let elapsed = slates_rt::futures::now_ns().saturating_sub(started);
        assert!(!shipped.timed_out, "a write became durable");
        sessions = shipped.reusable;
        late.push(shipped.stragglers);
        // Fold what has come back; with both holders live, wait for the rest outside the timed span (parked). A
        // silent holder's exchange returns only at its deadline: ship on to the holder that answers.
        loop {
          let mut pending = Vec::new();
          for mut stragglers in late.drain(..) {
            let (recovered, done) = fold_late(&mut shipper, &mut stragglers);
            sessions.extend(recovered);
            if !done {
              pending.push(stragglers);
            }
          }
          late = pending;
          if late.is_empty() || silent {
            break;
          }
          slates_rt::futures::within(FINE_POLL_NS, slates_rt::futures::idle())
            .await
            .unwrap();
        }
        sessions.sort_by_key(|(host, _)| host.0);
        assert_eq!(
          shipper.durable(),
          Some(entry.position),
          "the write is durable"
        );
        if at >= WARM_UP {
          timed.push(elapsed);
        }
      }
      rows.push((format!("live ship {payload_bytes} B"), timed));
    }
=======
>>>>>>> origin/main
    let _ = result_tx.send(rows);
    STOP.store(true, Ordering::Release);
    drop(sessions);
  })
  .unwrap();

  let rows = result_rx.recv().unwrap();
  println!(
    "replication_bench: samples={samples} load_threads={load_threads} cores={}",
    std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
  );
  for (name, timed) in rows {
    row(&name, timed);
  }
  LOAD_STOP.store(true, Ordering::Relaxed);
  for thread in load {
    thread.join().unwrap();
  }
  rt.shutdown().unwrap();
}
