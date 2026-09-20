//! Content placement over authenticated sessions on the simulated fabric (§4.8, §4.10,
//! AC-8.12). The virtual clock puts valid replies inside a collector's final sleep, so
//! deadline handling cannot discard an already-delivered offer or acknowledgement.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};
use slates_cluster::CommitBudget;
use slates_cluster::content::{ContentHold, put_content};
use slates_db::register::{HostId, ObjectId, Placement, Quorum};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::{Endpoint, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;

/// Shape: distinct owner and holder identities at the smallest remote quorum, f=1.
const OWNER: HostId = HostId(1);
/// Shape: the only remote holder needed to join the owner's local acknowledgement.
const HOLDER: HostId = HostId(2);
/// Shape: one virtual poll spans the whole collection budget. The holder replies during
/// that sleep; a reply queued before the collector resumes must win over expiration.
const COLLECTION_NS: u64 = 20_000_000;
/// Shape: the enrolled server name shared by this pair of test certificates.
const NAME: &str = "slates-node";

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

fn archive() -> Archive {
  let chunk = Archive::raw_chunk(b"a reply inside the last poll".to_vec());
  Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 4096,
    created_unix: 1,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 0,
    unicode_version: 0,
    root_meta: NodeMeta::default(),
    manifest: Node::Directory(vec![Entry {
      name: "file".to_owned(),
      meta: NodeMeta::default(),
      node: Node::File(vec![Extent {
        offset: 0,
        len: chunk.raw_len,
        chunk: chunk.identity,
        chunk_offset: 0,
      }]),
    }]),
    chunks: vec![chunk],
  }
}

/// Receives the other socket's assigned address, yielding on the simulated timer tick.
async fn address_from(receiver: Receiver<SocketAddrV4>) -> SocketAddrV4 {
  loop {
    if let Ok(address) = receiver.try_recv() {
      return address;
    }
    slates_rt::futures::sleep(config().timer_tick_ns).await;
  }
}

/// Serves one exchange with a virtual deadline, so a missing put fails instead of
/// keeping the simulation alive forever after the collector incorrectly drops its offer.
async fn serve_bounded(endpoint: &mut Endpoint, held: &mut ContentHold) -> bool {
  use std::future::Future;
  use std::task::Poll;
  let mut serving = std::pin::pin!(endpoint.serve_once(|_, request| held.serve(HOLDER, &request)));
  let mut deadline = std::pin::pin!(slates_rt::futures::sleep(COLLECTION_NS * 2));
  std::future::poll_fn(|context| {
    if let Poll::Ready(result) = serving.as_mut().poll(context) {
      return Poll::Ready(result.is_ok());
    }
    if deadline.as_mut().poll(context).is_ready() {
      return Poll::Ready(false);
    }
    Poll::Pending
  })
  .await
}

/// The observable placement, content hold and returned sessions of a content attempt.
#[derive(Debug)]
struct PutObservation {
  outcome: Result<Placement, String>,
  reusable: usize,
  late: Vec<HostId>,
  complete: bool,
  held: bool,
}

fn run_put(offer_delay_ns: u64, budget: CommitBudget) -> PutObservation {
  let mut simulation = SimRuntime::new(&config(), 1).unwrap();
  let shard = simulation.shard_ids()[0];
  let owner_identity = identity();
  let holder_identity = identity();
  let owner_certificate = owner_identity.certificate();
  let holder_certificate = holder_identity.certificate();
  let (owner_tx, owner_rx) = channel();
  let (holder_tx, holder_rx) = channel();
  let (held_tx, held_rx) = channel();
  simulation
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      holder_tx.send(socket.local_addr().unwrap()).unwrap();
      let owner_address = address_from(owner_rx).await;
      let mut endpoint = Endpoint::server(
        socket,
        owner_address,
        &holder_identity,
        &[owner_certificate],
        MIN_DATAGRAM_BYTES,
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      slates_rt::futures::sleep(offer_delay_ns).await;
      let mut held = ContentHold::new();
      for _ in ["offer", "put"] {
        if !serve_bounded(&mut endpoint, &mut held).await {
          break;
        }
      }
      held_tx
        .send(held.holds_manifest(&archive().manifest_identity()))
        .unwrap();
    })
    .unwrap();
  let (placed_tx, placed_rx) = channel();
  simulation
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      owner_tx.send(socket.local_addr().unwrap()).unwrap();
      let holder_address = address_from(holder_rx).await;
      let mut endpoint = Endpoint::client(
        socket,
        holder_address,
        &owner_identity,
        &holder_certificate,
        NAME,
        MIN_DATAGRAM_BYTES,
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      let mut placed = put_content(
        OWNER,
        &archive(),
        ObjectId::new(OWNER, 1),
        1,
        &[OWNER, HOLDER],
        Quorum { f: 1 },
        vec![(HOLDER, endpoint)],
        budget,
      )
      .await;
      slates_rt::futures::sleep(budget.max_deadline_ns()).await;
      let (late, complete) = placed.stragglers.recover();
      placed_tx
        .send(PutObservation {
          outcome: placed.outcome.map_err(|error| error.to_string()),
          reusable: placed.reusable.len(),
          late: late.into_iter().map(|(host, _)| host).collect(),
          complete,
          held: false,
        })
        .unwrap();
    })
    .unwrap();
  simulation.run_until_idle();
  let mut observed = placed_rx.try_recv().unwrap();
  observed.held = held_rx.try_recv().unwrap();
  observed
}

/// AC-8.12: receive both the missing-set reply and the verified put acknowledgement
/// during the collector's final sleep; expect placement and reuse of the holder session.
#[test]
fn replies_delivered_during_the_final_poll_place_the_content() {
  let observed = run_put(0, CommitBudget::hard(COLLECTION_NS, COLLECTION_NS));
  let placement = observed
    .outcome
    .expect("queued replies place content despite the elapsed poll budget");
  assert!(placement.placed(Quorum { f: 1 }));
  assert_eq!(
    observed.reusable, 1,
    "the holder's session is returned for reuse"
  );
  assert!(observed.held, "the holder verified the archive");
}

/// AC-8.12 / §4.10: an offer answered after collection stops but inside the task's
/// full span returns its session through straggler recovery, without claiming placement.
#[test]
fn a_late_offer_returns_its_session_after_collection_stops() {
  let observed = run_put(
    COLLECTION_NS * 2,
    CommitBudget::with_extension(
      COLLECTION_NS,
      COLLECTION_NS,
      1,
      1,
      COLLECTION_NS * 2,
      1,
      COLLECTION_NS,
    ),
  );
  assert!(
    observed.outcome.is_err(),
    "a missing-set reply is not a content acknowledgement"
  );
  assert!(!observed.held);
  assert_eq!(
    observed.late,
    [HOLDER],
    "the late offer's session must be recovered"
  );
  assert!(observed.complete, "every dispatched session returned");
}

/// AC-8.12: a holder silent beyond the request's full span cannot place content;
/// expiration still returns its session and accounts for every dispatched task.
#[test]
fn a_silent_holder_expires_without_placing_and_returns_its_session() {
  let observed = run_put(
    COLLECTION_NS * 4,
    CommitBudget::hard(COLLECTION_NS, COLLECTION_NS),
  );
  assert!(observed.outcome.is_err());
  assert!(!observed.held);
  assert_eq!(observed.reusable + observed.late.len(), 1);
  assert!(observed.complete);
}
