//! Content placement over authenticated sessions on the simulated fabric (§4.8, §4.10,
//! AC-8.12). The virtual clock puts valid replies inside a collector's final sleep, so
//! deadline handling cannot discard an already-delivered offer or acknowledgement.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};
use slates_cluster::CommitBudget;
use slates_cluster::content::{ContentHold, HoldSpace, put_content};
use slates_db::register::{HostId, ObjectId, Placement, Quorum};
use slates_mem::arena::ChunkArena;
use slates_mem::budget::{MetadataBudget, ShardBudget};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::{Endpoint, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;

/// Shape: distinct owner and holder identities at the smallest remote quorum, f=1.
/// Shape: the receive ceiling the content test's sessions may auto-tune to — sixty-four initial windows at
/// the minimum datagram, enough for an archive to flow at its path's rate.
const CONTENT_RECEIVE_CEILING: u64 =
  64 * (slates_transport::conn::REORDER_THRESHOLD + 1) * MIN_DATAGRAM_BYTES as u64;

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
    wake_tracking: None,
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
      // The canonical form: the file's recorded size is the length its extents tile.
      meta: NodeMeta {
        size: chunk.raw_len,
        ..NodeMeta::default()
      },
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
    slates_rt::futures::sleep(config().timer_tick_ns)
      .await
      .unwrap();
  }
}

/// Settles `endpoint`'s unacknowledged replies ([`Endpoint::settle`]), or gives up after `within_ns` (a
/// peer that already gave up never acknowledges them): `true` when every reply was acknowledged in time.
async fn settle_within(endpoint: &mut Endpoint, within_ns: u64) -> bool {
  let mut settle = std::pin::pin!(endpoint.settle());
  let mut deadline = std::pin::pin!(slates_rt::futures::sleep(within_ns));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(settled) = std::future::Future::poll(settle.as_mut(), cx) {
      return std::task::Poll::Ready(settled.is_ok());
    }
    if std::future::Future::poll(deadline.as_mut(), cx)
      .map(Result::unwrap)
      .is_ready()
    {
      return std::task::Poll::Ready(false);
    }
    std::task::Poll::Pending
  })
  .await
}

/// The holder's shard memory (§4.2; AUD-29-43): an arena over whole pages, a byte budget over its usable
/// capacity and an unbounded metadata ledger.
struct Room {
  arena: ChunkArena,
  budget: ShardBudget,
  metadata: MetadataBudget,
}

impl Room {
  /// Shape: the blocks the test archive needs — its two chunks, its manifest and one verification scratch
  /// block — rounded up to the buddy's power of two.
  const BLOCKS: usize = 4;

  fn new() -> Room {
    let page = rustix::param::page_size();
    let mut arena = ChunkArena::new(page);
    arena
      .add_region(slates_mem::region::Region::map(page * Self::BLOCKS, page, false).unwrap())
      .unwrap();
    let capacity = u64::try_from(arena.capacity()).unwrap();
    Room {
      arena,
      budget: ShardBudget::new(capacity, 0),
      metadata: MetadataBudget::new(u64::MAX),
    }
  }
}

/// Serves one exchange with a virtual deadline, so a missing put fails instead of
/// keeping the simulation alive forever after the collector incorrectly drops its offer.
async fn serve_bounded(endpoint: &mut Endpoint, held: &mut ContentHold, room: &mut Room) -> bool {
  use std::future::Future;
  use std::task::Poll;
  let mut serving = std::pin::pin!(endpoint.serve_once(|_, request| {
    let mut space = HoldSpace {
      arena: &mut room.arena,
      budget: &mut room.budget,
      metadata: &mut room.metadata,
    };
    held
      .serve(&mut space, HOLDER, &request, |_, _| true, |_, _, _| true)
      .0
  }));
  let mut deadline = std::pin::pin!(slates_rt::futures::sleep(COLLECTION_NS * 2));
  std::future::poll_fn(|context| {
    if let Poll::Ready(result) = serving.as_mut().poll(context) {
      return Poll::Ready(result.is_ok());
    }
    if deadline
      .as_mut()
      .poll(context)
      .map(Result::unwrap)
      .is_ready()
    {
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
        slates_transport::connection::ConnectionShape::for_frame_cap(
          slates_transport::endpoint::MAX_PACKET_PAYLOAD,
          CONTENT_RECEIVE_CEILING,
        ),
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      slates_rt::futures::sleep(offer_delay_ns).await.unwrap();
      let mut held = ContentHold::new();
      let mut room = Room::new();
      for _ in ["offer", "put"] {
        if !serve_bounded(&mut endpoint, &mut held, &mut room).await {
          break;
        }
      }
      // The put's reply is in flight when the serve returns; settle it before the session drops.
      let _ = settle_within(&mut endpoint, COLLECTION_NS * 2).await;
      held_tx
        .send(held.holds_manifest_for_any_object(&archive().manifest_identity()))
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
        slates_transport::connection::ConnectionShape::for_frame_cap(
          slates_transport::endpoint::MAX_PACKET_PAYLOAD,
          CONTENT_RECEIVE_CEILING,
        ),
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
      slates_rt::futures::sleep(budget.max_deadline_ns())
        .await
        .unwrap();
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

/// Shape: the collector polls the barrier test makes per collection span — fine enough that an offer, a
/// chunk and their replies (a few polls) fit inside one offer span, so the poll quantum does not decide
/// whether the fast holder beat the slow offer's timeout.
const POLLS_PER_SPAN: u64 = 4;

/// Shape: the stalled holder of the barrier test, distinct from [`HOLDER`] (the fast one).
const SLOW: HostId = HostId(3);

/// Runs a holder at `delay_ns` before it serves: an offer then the chunk, each bounded; reports whether it
/// held the archive.
fn spawn_holder(
  simulation: &mut SimRuntime,
  shard: slates_rt::ShardId,
  (holder_identity, owner_certificate): (Identity, rustls::pki_types::CertificateDer<'static>),
  (holder_tx, owner_rx): (
    std::sync::mpsc::Sender<SocketAddrV4>,
    Receiver<SocketAddrV4>,
  ),
  delay_ns: u64,
  held_tx: std::sync::mpsc::Sender<bool>,
) {
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
        slates_transport::connection::ConnectionShape::for_frame_cap(
          slates_transport::endpoint::MAX_PACKET_PAYLOAD,
          CONTENT_RECEIVE_CEILING,
        ),
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      slates_rt::futures::sleep(delay_ns).await.unwrap();
      let mut held = ContentHold::new();
      let mut room = Room::new();
      for _ in ["offer", "chunk"] {
        if !serve_bounded(&mut endpoint, &mut held, &mut room).await {
          break;
        }
      }
      let _ = settle_within(&mut endpoint, COLLECTION_NS * 2).await;
      held_tx
        .send(held.holds_manifest_for_any_object(&archive().manifest_identity()))
        .unwrap();
    })
    .unwrap();
}

/// An owner endpoint dialled to the holder whose address arrives on `holder_rx`, its own address sent on
/// `owner_tx`.
async fn dial(
  owner_identity: &Identity,
  holder_certificate: &rustls::pki_types::CertificateDer<'static>,
  owner_tx: std::sync::mpsc::Sender<SocketAddrV4>,
  holder_rx: Receiver<SocketAddrV4>,
) -> Endpoint {
  let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  owner_tx.send(socket.local_addr().unwrap()).unwrap();
  let holder_address = address_from(holder_rx).await;
  let mut endpoint = Endpoint::client(
    socket,
    holder_address,
    owner_identity,
    holder_certificate,
    NAME,
    slates_transport::connection::ConnectionShape::for_frame_cap(
      slates_transport::endpoint::MAX_PACKET_PAYLOAD,
      CONTENT_RECEIVE_CEILING,
    ),
  )
  .unwrap();
  endpoint.establish().await.unwrap();
  endpoint
}

/// AUD-29-58 (§4.8 hedged placement): one acknowledgement still needed, two offers outstanding, one holder
/// stalled. Do: put to a fast holder and to one that waits four collection spans before answering anything,
/// under a hard budget whose span is two collection spans. Expect the placement to complete with the fast
/// holder before the slow offer times out (inside the offer span) — the fast holder's chunk went out and came
/// back while the slow offer was still unanswered — and the slow holder never counted. Until 2026-10-01 every
/// put waited for every offer, so the fast holder's put left only once the slow offer had timed out, at the
/// end of the offer span.
#[test]
fn a_fast_holder_places_while_a_slow_offer_is_outstanding() {
  let stall_ns = COLLECTION_NS * 4;
  let budget = CommitBudget::hard(COLLECTION_NS * 2, COLLECTION_NS / POLLS_PER_SPAN);
  let mut simulation = SimRuntime::new(&config(), 1).unwrap();
  let shard = simulation.shard_ids()[0];
  let owner_identity = identity();
  let (fast_identity, slow_identity) = (identity(), identity());
  let (fast_certificate, slow_certificate) =
    (fast_identity.certificate(), slow_identity.certificate());
  let (fast_to_owner, owner_from_fast) = channel();
  let (owner_to_fast, fast_from_owner) = channel();
  let (slow_to_owner, owner_from_slow) = channel();
  let (owner_to_slow, slow_from_owner) = channel();
  let (fast_held_tx, fast_held_rx) = channel();
  let (slow_held_tx, slow_held_rx) = channel();
  spawn_holder(
    &mut simulation,
    shard,
    (fast_identity, owner_identity.certificate()),
    (fast_to_owner, fast_from_owner),
    0,
    fast_held_tx,
  );
  spawn_holder(
    &mut simulation,
    shard,
    (slow_identity, owner_identity.certificate()),
    (slow_to_owner, slow_from_owner),
    stall_ns,
    slow_held_tx,
  );
  let (placed_tx, placed_rx) = channel();
  simulation
    .spawn_on(shard, async move {
      let fast = dial(
        &owner_identity,
        &fast_certificate,
        owner_to_fast,
        owner_from_fast,
      )
      .await;
      let slow = dial(
        &owner_identity,
        &slow_certificate,
        owner_to_slow,
        owner_from_slow,
      )
      .await;
      let started = slates_rt::futures::now_ns();
      let placed = put_content(
        OWNER,
        &archive(),
        ObjectId::new(OWNER, 1),
        1,
        &[OWNER, HOLDER, SLOW],
        Quorum { f: 1 },
        vec![(HOLDER, fast), (SLOW, slow)],
        budget,
      )
      .await;
      let returned_after = slates_rt::futures::now_ns().saturating_sub(started);
      let mut placed = placed;
      slates_rt::futures::sleep(stall_ns + budget.max_deadline_ns() * 2)
        .await
        .unwrap();
      let _ = placed.stragglers.recover();
      placed_tx
        .send((
          placed.outcome.map_err(|error| error.to_string()),
          placed.latencies_ns,
          returned_after,
        ))
        .unwrap();
    })
    .unwrap();
  simulation.run_until_idle();
  let (outcome, latencies, returned_after) = placed_rx.try_recv().unwrap();
  let placement = outcome.expect("the fast holder places the content");
  assert!(placement.placed(Quorum { f: 1 }));
  assert!(placement.acked.contains(&HOLDER));
  assert!(
    !placement.acked.contains(&SLOW),
    "the stalled holder never counted"
  );
  let fast_latency = latencies
    .iter()
    .find(|(host, _)| *host == HOLDER)
    .map(|(_, latency)| *latency)
    .expect("the fast holder's latency is recorded");
  let offer_span_ns = budget.max_deadline_ns();
  assert!(
    fast_latency < offer_span_ns && returned_after < offer_span_ns,
    "placed before the slow offer timed out: fast ack at {fast_latency} ns, returned at {returned_after} ns, \
     offer span {offer_span_ns} ns"
  );
  assert_eq!(fast_held_rx.try_recv(), Ok(true));
  let _ = slow_held_rx.try_recv();
}
