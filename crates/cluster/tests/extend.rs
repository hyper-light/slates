//! The commit dispatch's **progress-based deadline extension** (§4.8 "late work"), live over the
//! simulated UDP fabric: a commit whose quorum is still filling as it passes its deadline is given a
//! bounded extension and commits, while a stalled one (no new acknowledgement within the stall window)
//! is left to time out and reported uncertain. This is the [`CommitBudget::with_extension`] policy the
//! [`commit_record`] collection loop consumes, exercised by use — real endpoints and holder logic over
//! simulated UDP, one process simulating several holders (still an `f = 2` protocol topology; real
//! multi-process deployment is a further gate). The extender's decision logic is unit-tested in
//! `progress.rs`; this proves the dispatch *consumes* it — a slow-but-progressing commit survives, a
//! stuck one does not. Test by use (R5).

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::future::Future;
use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::{ClusterError, CommitBudget, broadcast, commit_record, serve_record};
use slates_db::register::{
  Acceptor, Authority, HostEpoch, HostId, ObjectId, Placement, Quorum, Record,
};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
/// Shape: a small packet budget — sixteen bytes of stream data per frame — so a message spans several
/// packets and the reassembly, credit and loss paths all run.
const FRAME_CAP: usize = slates_transport::session::STREAM_FRAME_HEADER_BYTES + 16;
/// Shape: the receive ceiling the test sessions' windows may auto-tune to — sixty-four initial windows,
/// room for the tuning path to run without any test holding more than a few kilobytes.
const RECEIVE_CEILING_WINDOWS: u64 = 64;

/// The connection shape every test session is built with: the frame cap, a ceiling of
/// [`RECEIVE_CEILING_WINDOWS`] initial windows, and the session plane's controller.
fn shape() -> slates_transport::connection::ConnectionShape {
  slates_transport::connection::ConnectionShape::for_frame_cap(
    FRAME_CAP,
    RECEIVE_CEILING_WINDOWS * slates_transport::connection::initial_receive_window(FRAME_CAP),
  )
}
const GENERATION: u64 = 0;
const OWNER: HostId = HostId(1);
const OBJECT: ObjectId = ObjectId::new(OWNER, 7);
/// The quorum: `f = 2`, so a commit needs `f + 1 = 3` distinct acknowledgements — the owner and both
/// holders. That the last holder acknowledges only past the base deadline is what makes the extension
/// observable: a hard deadline would report the commit uncertain before that third acknowledgement.
const QUORUM: Quorum = Quorum { f: 2 };

// Test timing values (nanoseconds); a production caller derives these from a measured RTT budget.
/// The base deadline before extension — deliberately shorter than the second holder's acknowledgement,
/// so a hard budget would time the commit out before the quorum fills.
const BASE_DEADLINE_NS: u64 = 15_000_000;
/// The collection loop's poll interval.
const POLL_NS: u64 = 1_000_000;
/// When the first holder acknowledges (well within the base deadline) — the quorum starts filling.
const FIRST_ACK_NS: u64 = 10_000_000;
/// When the second holder acknowledges (past the base deadline) — the acknowledgement only an extension
/// lets the commit collect.
const SECOND_ACK_NS: u64 = 25_000_000;
/// How much each granted extension adds to the deadline.
const EXTENSION_NS: u64 = 15_000_000;
/// How many extensions a commit may be granted.
const MAX_EXTENSIONS: u32 = 3;
/// The lookahead ratio 3/4 (hyperscale's 0.75, AD-26): an extension is considered in the last quarter of
/// the current deadline.
const LOOKAHEAD_NUM: u64 = 3;
const LOOKAHEAD_DEN: u64 = 4;
/// The stall window — wider than the gap between the staggered acknowledgements, so a steadily filling
/// quorum reads as progressing while a quorum with no new acknowledgement for this long reads as stalled.
const STALL_WINDOW_NS: u64 = 20_000_000;
/// When a third holder — the straggler — acknowledges: past the quorum the owner and two prompt holders
/// form at [`FIRST_ACK_NS`], so the commit has already returned without it, and within the dispatch's full
/// span (`BASE_DEADLINE_NS + MAX_EXTENSIONS × EXTENSION_NS`), so its task finishes by acknowledging rather
/// than timing out.
const STRAGGLER_ACK_NS: u64 = 25_000_000;
/// How long the owner waits after a commit returns before recovering its stragglers' sessions: past
/// [`STRAGGLER_ACK_NS`], so the recovery finds the straggler finished.
const RECOVERY_WAIT_NS: u64 = 30_000_000;
/// How long a stalled holder keeps its connection open before exiting — past the commit's stall-driven
/// expiry, so the owner's request genuinely waits (and times out) rather than seeing the connection drop
/// (which would be reported "not placed", a different path). Bounded so the simulation reaches idle.
const KEEPALIVE_NS: u64 = 60_000_000;

/// A holder's plan: how many successive records it acknowledges (serving one commit each), and at what
/// delay after its handshake completes it starts.
#[derive(Clone, Copy)]
struct HolderPlan {
  /// How many successive commits the holder serves (acknowledges). A holder that serves none keeps its
  /// connection open for [`KEEPALIVE_NS`] and then exits, so the owner's request to it genuinely waits.
  serves: u32,
  /// The delay after the handshake before the holder acts (serves, or simply stays alive).
  at_ns: u64,
}

/// The commit's outcome (its placement or the `ClusterError` string) and the sim time the dispatch took,
/// so a test can assert the commit ran past its base deadline (the extension is observable) or expired;
/// the holders whose sessions came back at the return (`reusable`) and those recovered later from the
/// stragglers (`recovered`, after [`RECOVERY_WAIT_NS`]); and, when the run re-commits, the second commit's
/// placement over the returned **and recovered** sessions — the by-use proof a recovered session is live.
struct Outcome {
  placement: Result<Placement, String>,
  elapsed_ns: u64,
  reusable: Vec<HostId>,
  recovered: Vec<HostId>,
  recommitted: Option<Placement>,
}

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 2_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

fn self_signed(name: &str) -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

async fn recv_port(rx: Receiver<u16>) -> u16 {
  loop {
    if let Ok(p) = rx.try_recv() {
      return p;
    }
    slates_rt::futures::sleep(POLL_NS).await.unwrap();
  }
}

/// Settles `endpoint`'s unacknowledged replies ([`Endpoint::settle`]), or gives up after `within_ns`:
/// `Some` when every reply was acknowledged in time.
async fn settle_within(endpoint: &mut Endpoint, within_ns: u64) -> Option<()> {
  let mut settle = std::pin::pin!(endpoint.settle());
  let mut deadline = std::pin::pin!(slates_rt::futures::sleep(within_ns));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(result) = settle.as_mut().poll(cx) {
      return std::task::Poll::Ready(result.ok());
    }
    if deadline.as_mut().poll(cx).map(Result::unwrap).is_ready() {
      return std::task::Poll::Ready(None);
    }
    std::task::Poll::Pending
  })
  .await
}

fn authority() -> Authority {
  Authority {
    generation: GENERATION,
    owner: OWNER,
  }
}

fn record(sequence: u64, value: &[u8]) -> Record {
  Record {
    owner: OWNER,
    object: OBJECT,
    sequence,
    epoch: HostEpoch(1),
    generation: GENERATION,
    value: value.to_vec(),
  }
}

/// The host id of the holder at `index` among the plans: the owner is `1`, the holders `2`, `3`, ….
fn holder_id(index: usize) -> HostId {
  HostId(u64::try_from(index).unwrap_or(0) + 2)
}

/// Runs an `f = 2` commit — owner `1`, one candidate holder per plan (`2`, `3`, …) — over the sim fabric
/// with `budget`. Each serving holder acknowledges after its planned delay, so the owner's quorum fills
/// over time. After the commit returns, the owner waits [`RECOVERY_WAIT_NS`] and recovers the sessions of
/// the holders that were still in flight (the stragglers); with a `recommit` quorum, it then commits a
/// **second** record, under that quorum, over the returned and recovered sessions together, so a recovered
/// session is proven live by use. Returns the commit's [`Outcome`].
fn run_commit(budget: CommitBudget, plans: &[HolderPlan], recommit: Option<Quorum>) -> Outcome {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let owner_identity = self_signed(NAME);
  let owner_cert = owner_identity.certificate();
  let holder_identities: Vec<Identity> = plans.iter().map(|_| self_signed(NAME)).collect();
  let holder_certs: Vec<_> = holder_identities
    .iter()
    .map(Identity::certificate)
    .collect();

  // Port exchange, one pair of channels per holder: the owner sends the port of the socket it dials
  // from, and holder i sends the port it listens on.
  let mut owner_port_tx = Vec::new();
  let mut owner_port_rx = Vec::new();
  let mut holder_port_tx = Vec::new();
  let mut holder_port_rx = Vec::new();
  for _ in plans {
    let (otx, orx) = channel::<u16>();
    let (htx, hrx) = channel::<u16>();
    owner_port_tx.push(otx);
    owner_port_rx.push(orx);
    holder_port_tx.push(htx);
    holder_port_rx.push(hrx);
  }
  let (result_tx, result_rx) = channel::<Outcome>();

  // The holder tasks: each handshakes, waits its planned delay, then serves its planned number of records
  // (acknowledging each) or simply stays alive to its keepalive and exits.
  let holder_setup = holder_identities
    .into_iter()
    .zip(owner_port_rx)
    .zip(holder_port_tx)
    .zip(plans.iter().copied());
  for (index, (((holder_identity, orx), htx), plan)) in holder_setup.enumerate() {
    let owner_cert = owner_cert.clone();
    let holder = holder_id(index);
    sim
      .spawn_on(id, async move {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = htx.send(socket.local_addr().unwrap().port());
        let owner_port = recv_port(orx).await;
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, owner_port);
        let mut endpoint = Endpoint::server(
          socket,
          peer,
          &holder_identity,
          std::slice::from_ref(&owner_cert),
          shape(),
        )
        .unwrap();
        endpoint.establish().await.unwrap();
        slates_rt::futures::sleep(plan.at_ns).await.unwrap();
        let mut acceptor = Acceptor::new(holder, authority());
        for _ in 0..plan.serves {
          // Ignore the result: a holder that serves before the commit resolves acknowledges cleanly; a
          // holder whose owner has already given up serves into a closed connection, which simply fails.
          let _ = serve_record(&mut endpoint, &mut acceptor).await;
        }
        // The last acknowledgement is in flight when `serve_record` returns; settle it, bounded by the
        // keepalive — an owner that already gave up never acknowledges it.
        let _ = settle_within(&mut endpoint, KEEPALIVE_NS).await;
        // A non-serving holder has now stayed alive for its keepalive; it exits, closing its socket.
      })
      .unwrap();
  }

  // The owner task: dial each holder, establish, then commit under `budget`, timing the dispatch.
  let candidates: Vec<HostId> = std::iter::once(OWNER)
    .chain((0..plans.len()).map(holder_id))
    .collect();
  sim
    .spawn_on(id, async move {
      let dial = owner_port_tx
        .into_iter()
        .zip(holder_port_rx)
        .zip(holder_certs);
      let mut remotes = Vec::new();
      for (index, ((owner_port_tx, holder_port_rx), holder_cert)) in dial.enumerate() {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = owner_port_tx.send(socket.local_addr().unwrap().port());
        let holder_port = recv_port(holder_port_rx).await;
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, holder_port);
        let mut endpoint =
          Endpoint::client(socket, peer, &owner_identity, &holder_cert, NAME, shape()).unwrap();
        endpoint.establish().await.unwrap();
        remotes.push((holder_id(index), endpoint));
      }

      let mut owner_acceptor = Acceptor::new(OWNER, authority());
      let started = slates_rt::futures::now_ns();
      let committed = commit_record(
        OWNER,
        &mut owner_acceptor,
        &Placement::of(&candidates),
        &record(0, b"head@v1"),
        QUORUM,
        remotes,
        budget,
      )
      .await;
      let elapsed_ns = slates_rt::futures::now_ns().saturating_sub(started);
      let placement = committed.outcome.map_err(|e: ClusterError| e.to_string());
      let reusable: Vec<HostId> = committed.reusable.iter().map(|(host, _)| *host).collect();
      let mut sessions = committed.reusable;
      // Give the stragglers their bounded time to finish, then recover their sessions.
      let mut stragglers = committed.stragglers;
      slates_rt::futures::sleep(RECOVERY_WAIT_NS).await.unwrap();
      let (late, _) = stragglers.recover();
      let recovered: Vec<HostId> = late.iter().map(|(host, _)| *host).collect();
      sessions.extend(late);
      let recommitted = if let Some(quorum) = recommit {
        commit_record(
          OWNER,
          &mut owner_acceptor,
          &Placement::of(&candidates),
          &record(1, b"head@v2"),
          quorum,
          sessions,
          budget,
        )
        .await
        .outcome
        .ok()
      } else {
        None
      };
      let _ = result_tx.send(Outcome {
        placement,
        elapsed_ns,
        reusable,
        recovered,
        recommitted,
      });
    })
    .unwrap();

  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// AC (§4.8 "late work"): a commit whose quorum is still filling as it passes its base deadline is
/// granted a bounded extension and commits, rather than being reported uncertain. The second holder
/// acknowledges only past the base deadline, so the commit could not have placed under a hard deadline —
/// that it both places **and** ran longer than the base deadline is the observable proof the extension
/// fired (non-vacuous: a silently-dead extension path would leave this uncertain at the base deadline).
#[test]
fn a_progressing_commit_is_extended_past_its_deadline_and_places() {
  let outcome = run_commit(
    CommitBudget::with_extension(
      BASE_DEADLINE_NS,
      POLL_NS,
      LOOKAHEAD_NUM,
      LOOKAHEAD_DEN,
      EXTENSION_NS,
      MAX_EXTENSIONS,
      STALL_WINDOW_NS,
    ),
    &[
      HolderPlan {
        serves: 1,
        at_ns: FIRST_ACK_NS,
      },
      HolderPlan {
        serves: 1,
        at_ns: SECOND_ACK_NS,
      },
    ],
    None,
  );
  let placement = outcome
    .placement
    .expect("a progressing commit places under an extending budget");
  assert!(
    placement.placed(QUORUM),
    "the owner and both holders form the quorum: {placement:?}"
  );
  assert!(
    outcome.elapsed_ns > BASE_DEADLINE_NS,
    "the commit ran past its base deadline ({BASE_DEADLINE_NS} ns), so the extension is what let the \
     late acknowledgement land — elapsed {} ns",
    outcome.elapsed_ns
  );
}

/// AC (§4.8 "late work"): a commit that stalls — no holder ever acknowledges — is **not** extended
/// indefinitely; once its acknowledged set has not advanced for a stall window it is left to time out
/// and reported uncertain (partial acceptance may have occurred; it is never claimed placed). This is
/// the bound that keeps the extension from masking a genuinely stuck commit, and it proves the extension
/// is not an unconditional "wait forever": the dispatch gives up well within its full extension budget.
#[test]
fn a_stalled_commit_expires_and_is_reported_uncertain() {
  let outcome = run_commit(
    CommitBudget::with_extension(
      BASE_DEADLINE_NS,
      POLL_NS,
      LOOKAHEAD_NUM,
      LOOKAHEAD_DEN,
      EXTENSION_NS,
      MAX_EXTENSIONS,
      STALL_WINDOW_NS,
    ),
    &[
      HolderPlan {
        serves: 0,
        at_ns: KEEPALIVE_NS,
      },
      HolderPlan {
        serves: 0,
        at_ns: KEEPALIVE_NS,
      },
    ],
    None,
  );
  match outcome.placement {
    Err(e) => assert!(
      e.contains("uncertain") || e.contains("Uncertain"),
      "a stalled commit is reported uncertain, not {e:?}"
    ),
    Ok(placement) => panic!("a stalled commit must not place: {placement:?}"),
  }
  // It gave up on the stall, not by exhausting the whole extension budget: the deadline could have grown
  // to BASE + MAX_EXTENSIONS × EXTENSION, and a stall-driven expiry stops well short of that.
  let full_budget_ns = BASE_DEADLINE_NS + u64::from(MAX_EXTENSIONS) * EXTENSION_NS;
  assert!(
    outcome.elapsed_ns >= BASE_DEADLINE_NS && outcome.elapsed_ns < full_budget_ns,
    "expired on the stall within the extension budget (base {BASE_DEADLINE_NS} ns ≤ elapsed {} ns < \
     full {full_budget_ns} ns)",
    outcome.elapsed_ns
  );
}

/// AC (§4.8 "records are sent to all candidates; committed at `f + 1`", keep-session): a holder still in
/// flight when the commit returns at an early quorum — a **straggler** — is not cancelled and does not lose
/// its session: the owner recovers it later from the dispatch's `Stragglers` and reuses it. Three holders at
/// `f = 2`: the owner and the two prompt holders form the quorum at [`FIRST_ACK_NS`], so the commit returns
/// with those two sessions reusable and the third holder's still out; the straggler acknowledges at
/// [`STRAGGLER_ACK_NS`] and its session is recovered. The recovered session is then proven **live by use**,
/// deterministically: the two prompt holders served once and are gone, so a second record committed at
/// `f = 1` over all three sessions can reach its second acknowledgement **only** through the recovered one
/// — it placing with the straggler among its acknowledgers is possible on no other path. Before this, the
/// dispatch cancelled the straggler and dropped its endpoint — a session the per-peer-socket mesh cannot
/// re-establish — so every early quorum at `f > 1` cost a live holder its session for good.
#[test]
fn an_early_quorums_straggler_hands_its_session_back_and_it_is_reused() {
  let outcome = run_commit(
    CommitBudget::with_extension(
      BASE_DEADLINE_NS,
      POLL_NS,
      LOOKAHEAD_NUM,
      LOOKAHEAD_DEN,
      EXTENSION_NS,
      MAX_EXTENSIONS,
      STALL_WINDOW_NS,
    ),
    &[
      HolderPlan {
        serves: 1,
        at_ns: FIRST_ACK_NS,
      },
      HolderPlan {
        serves: 1,
        at_ns: FIRST_ACK_NS,
      },
      HolderPlan {
        serves: 2,
        at_ns: STRAGGLER_ACK_NS,
      },
    ],
    Some(Quorum { f: 1 }),
  );
  let placement = outcome
    .placement
    .expect("the owner and the two prompt holders place the first commit");
  assert!(
    placement.placed(QUORUM),
    "placed at the early quorum: {placement:?}"
  );
  let straggler = holder_id(2);
  assert!(
    !placement.acked.contains(&straggler),
    "the straggler had not acknowledged when the commit returned: {placement:?}"
  );
  let mut reusable = outcome.reusable;
  reusable.sort();
  assert_eq!(
    reusable,
    vec![holder_id(0), holder_id(1)],
    "the two prompt holders' sessions came back at the return"
  );
  assert_eq!(
    outcome.recovered,
    vec![straggler],
    "the straggler's session was recovered from the stragglers rather than dropped"
  );
  let recommitted = outcome
    .recommitted
    .expect("the second commit places through the recovered session");
  assert!(
    recommitted.placed(Quorum { f: 1 }) && recommitted.acked.contains(&straggler),
    "the recovered session is live: the straggler's acknowledgement over it is the only second one the \
     commit could get — {recommitted:?}"
  );
}

/// Format: the stream a consensus round's requests ride in [`run_broadcast`] (any stream a holder serves).
const ROUND_STREAM: u64 = 7;
/// Shape: when the one answering voter of a round replies — in the last quarter of [`BASE_DEADLINE_NS`], past
/// the lookahead (three quarters of it) and before the deadline: the round trip of the one live voter a
/// lost leader leaves a candidate, on KIND's asymmetric profile an 80 ms reply against a 100 ms base
/// (`docs/bugs/2026-09-29-a-round-with-no-reply-yet-gave-up-at-its-lookahead.md`).
const LATE_FIRST_REPLY_NS: u64 = 13_000_000;

/// Runs one consensus round — [`broadcast`] from an owner to one holder per plan, under `budget` — over the
/// sim fabric. A holder with `serves` answers that many requests after its planned delay; one with none
/// stays alive to its keepalive, silent. Returns the holders whose replies the round collected and how
/// long it ran.
fn run_broadcast(budget: CommitBudget, plans: &[HolderPlan]) -> (Vec<HostId>, u64) {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let owner_identity = self_signed(NAME);
  let owner_cert = owner_identity.certificate();
  let mut dial = Vec::new();
  for (index, plan) in plans.iter().copied().enumerate() {
    let holder_identity = self_signed(NAME);
    let holder_cert = holder_identity.certificate();
    let (owner_port_tx, owner_port_rx) = channel::<u16>();
    let (holder_port_tx, holder_port_rx) = channel::<u16>();
    let owner_cert = owner_cert.clone();
    sim
      .spawn_on(id, async move {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = holder_port_tx.send(socket.local_addr().unwrap().port());
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(owner_port_rx).await);
        let mut endpoint = Endpoint::server(
          socket,
          peer,
          &holder_identity,
          std::slice::from_ref(&owner_cert),
          shape(),
        )
        .unwrap();
        endpoint.establish().await.unwrap();
        slates_rt::futures::sleep(plan.at_ns).await.unwrap();
        for _ in 0..plan.serves {
          let _ = endpoint.serve_once(|_, _| b"granted".to_vec()).await;
        }
        let _ = settle_within(&mut endpoint, KEEPALIVE_NS).await;
      })
      .unwrap();
    dial.push((holder_id(index), holder_cert, owner_port_tx, holder_port_rx));
  }
  let (result_tx, result_rx) = channel::<(Vec<HostId>, u64)>();
  sim
    .spawn_on(id, async move {
      let mut requests = Vec::new();
      for (holder, holder_cert, owner_port_tx, holder_port_rx) in dial {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = owner_port_tx.send(socket.local_addr().unwrap().port());
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(holder_port_rx).await);
        let mut endpoint =
          Endpoint::client(socket, peer, &owner_identity, &holder_cert, NAME, shape()).unwrap();
        endpoint.establish().await.unwrap();
        requests.push((holder, b"pre-vote".to_vec(), endpoint));
      }
      let started = slates_rt::futures::now_ns();
      let (replied, _stragglers) =
        broadcast(requests, ROUND_STREAM, Priority::Control, budget).await;
      let elapsed = slates_rt::futures::now_ns().saturating_sub(started);
      let replied: Vec<HostId> = replied
        .iter()
        .filter(|(_, reply, _)| !reply.bytes.is_empty())
        .map(|(host, _, _)| *host)
        .collect();
      let _ = result_tx.send((replied, elapsed));
    })
    .unwrap();
  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// The extension budget the round tests run under: the constants above, as [`a_stalled_commit_expires_and_is_reported_uncertain`] uses them.
fn round_budget() -> CommitBudget {
  CommitBudget::with_extension(
    BASE_DEADLINE_NS,
    POLL_NS,
    LOOKAHEAD_NUM,
    LOOKAHEAD_DEN,
    EXTENSION_NS,
    MAX_EXTENSIONS,
    STALL_WINDOW_NS,
  )
}

/// §4.8 "late work" (`docs/bugs/2026-09-29-a-round-with-no-reply-yet-gave-up-at-its-lookahead.md`): a round
/// that has gathered no reply by its lookahead is not judged stalled there — it is given its whole base
/// deadline for a first reply. One voter is silent, as a lost leader is, and the other answers in the last
/// quarter of the base deadline; the round collects that answer. On KIND a candidate whose only live voter
/// answered there failed every pre-election, and the outranked voter, whose base was longer, won instead.
#[test]
fn a_round_is_given_its_whole_base_deadline_for_a_first_reply() {
  let (replied, elapsed_ns) = run_broadcast(
    round_budget(),
    &[
      HolderPlan {
        serves: 0,
        at_ns: KEEPALIVE_NS,
      },
      HolderPlan {
        serves: 1,
        at_ns: LATE_FIRST_REPLY_NS,
      },
    ],
  );
  assert_eq!(
    replied,
    vec![holder_id(1)],
    "the late first reply was collected (the round ran {elapsed_ns} ns)"
  );
}

/// §4.8 "late work": a round that gathers nothing at all is not extended — it ends at its base deadline, so
/// a dead voter never holds a round past it (`docs/bugs/2026-09-12-broadcast-waits-out-dead-voter.md`).
#[test]
fn a_round_with_no_reply_at_all_ends_at_its_base_deadline() {
  let (replied, elapsed_ns) = run_broadcast(
    round_budget(),
    &[HolderPlan {
      serves: 0,
      at_ns: KEEPALIVE_NS,
    }],
  );
  assert!(replied.is_empty(), "nothing replied: {replied:?}");
  assert!(
    (BASE_DEADLINE_NS..BASE_DEADLINE_NS + EXTENSION_NS).contains(&elapsed_ns),
    "ended at its base deadline, unextended: {elapsed_ns} ns against {BASE_DEADLINE_NS} ns"
  );
}
