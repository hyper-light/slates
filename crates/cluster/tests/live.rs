//! Live shipping over the fleet transport (A-200, §4.10; `slates_cluster::live`), on the simulated UDP fabric with
//! real endpoints (TLS 1.3, packet protection): an owner and two holders at `f = 1`. Test by use (R5).

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::CommitBudget;
use slates_cluster::live::{
  LIVE_ENTRY_STREAM, LIVE_STATE_STREAM, fold_late, serve_entry, serve_install, ship,
};
use slates_db::live::{Next, Replica, Shipper};
use slates_db::register::{HostEpoch, HostId, ObjectId, Quorum};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::{Endpoint, MAX_PACKET_PAYLOAD};
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const OWNER: HostId = HostId(1);
const HOLDERS: [HostId; 2] = [HostId(2), HostId(3)];
const OBJECT: ObjectId = ObjectId([5; 16]);
/// Shape: a holder's decode cap: far above the test's payloads.
const CAP: usize = 1 << 16;
/// Shape: a dispatch's deadline: ten seconds of virtual time, far past any exchange over the fabric's instant paths
/// (each write here takes no virtual time), so the deadline is never what this test measures.
const DEADLINE_NS: u64 = 10_000_000_000;
/// Shape: the collector's poll, the daemon's 10 ms.
const POLL_NS: u64 = 10_000_000;
/// Shape: a serve loop's park between checks of its stop channel.
const SERVE_SLICE_NS: u64 = 1_000_000;
/// Shape: how often a serving holder looks whether its scenario has let it go: a hundred serve slices, so a serve is cut
/// rarely enough not to starve its session (cut every slice, it did).
const SERVE_POLL_NS: u64 = 100 * SERVE_SLICE_NS;
/// Shape: the writes shipped before the late holder joins: more than the retained tail ([`shipper`]'s two), so it
/// is installed.
const WRITES: u64 = 6;

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 256,
    timers_per_shard: 256,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
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

fn shape() -> slates_transport::connection::ConnectionShape {
  slates_transport::connection::ConnectionShape::for_frame_cap(MAX_PACKET_PAYLOAD, 4 << 20)
}

/// Format: the request kind the owner ends a holder's serving with (a test's own, beside the live kinds).
const STOP_KIND: u64 = 99;

/// A holder: once `start` says so (at once unless silent), serves entries and installs into its replica of [`OBJECT`]
/// until the owner's stop request, then reports its final head and state.
async fn holder(
  mut endpoint: Endpoint,
  start: Receiver<()>,
  host: HostId,
  done: Sender<(HostId, Option<slates_db::live::Head>, Vec<u8>)>,
) {
  endpoint.establish().await.unwrap();
  loop {
    match start.try_recv() {
      Ok(()) => break,
      // Told nothing and let go: a holder its owner can no longer reach ends without serving.
      Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
      Err(std::sync::mpsc::TryRecvError::Empty) => {
        slates_rt::futures::sleep(SERVE_SLICE_NS).await.unwrap();
      }
    }
  }
  let mut replica = Replica::new(OWNER, OBJECT);
  let mut state: Vec<u8> = Vec::new();
  let mut stopping = false;
  while !stopping {
    // Let go once the scenario has ended without stopping it (its session dropped, so no stop can come): a regression
    // that leaves a holder serving then fails its test instead of hanging it.
    if matches!(
      start.try_recv(),
      Err(std::sync::mpsc::TryRecvError::Disconnected)
    ) {
      return;
    }
    let served = slates_rt::futures::within(
      SERVE_POLL_NS,
      endpoint.serve_once(|kind, request| {
        if kind == LIVE_ENTRY_STREAM {
          serve_entry(&mut replica, &request, CAP, |entry| {
            state.extend_from_slice(&entry.payload);
            true
          })
        } else if kind == LIVE_STATE_STREAM {
          serve_install(&mut replica, &request, CAP, |install| {
            state.clone_from(&install.state);
            true
          })
        } else {
          stopping = kind == STOP_KIND;
          Vec::new()
        }
      }),
    )
    .await
    .unwrap();
    if let Some(served) = served {
      served.unwrap();
    }
  }
  endpoint.settle().await.unwrap();
  let _ = done.send((host, replica.head(), state));
}

/// The owner's side of one scenario, given its warm sessions.
type Scenario = Box<
  dyn FnOnce(
      Vec<(HostId, Endpoint)>,
      BTreeMap<HostId, Sender<()>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    + Send,
>;

/// Runs `scenario` with holder 2 serving and holder 3 `silent` until told; returns each holder's final head and state.
fn run(
  silent: bool,
  scenario: Scenario,
) -> BTreeMap<HostId, (Option<slates_db::live::Head>, Vec<u8>)> {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  // Every socket is bound on the shard, so it is a port on the simulated fabric: one bound on this thread is an OS
  // socket, which the simulation driver cannot watch (refused since 2026-11-08; before, its sessions were woken only
  // by their timers).
  let (sockets_tx, sockets_rx) = channel();
  sim
    .spawn_on(shard, async move {
      let bind = || UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = sockets_tx.send(HOLDERS.map(|_| (bind(), bind())));
    })
    .unwrap();
  sim.run_until_idle();
  let sockets = sockets_rx.try_recv().unwrap();
  let owner_identity = identity();
  let owner_certificate = owner_identity.certificate();
  let mut dials = Vec::new();
  let mut commands = BTreeMap::new();
  let (done_tx, done_rx) = channel();
  for ((index, host), (holder_socket, owner_socket)) in HOLDERS.into_iter().enumerate().zip(sockets)
  {
    let holder_identity = identity();
    let holder_certificate = holder_identity.certificate();
    let holder_address = holder_socket.local_addr().unwrap();
    let owner_address = owner_socket.local_addr().unwrap();
    let owner_certificate = owner_certificate.clone();
    let (start_tx, start_rx) = channel();
    if !(silent && index == 1) {
      start_tx.send(()).unwrap();
    }
    commands.insert(host, start_tx);
    let done = done_tx.clone();
    sim
      .spawn_on(shard, async move {
        let endpoint = Endpoint::server(
          holder_socket,
          owner_address,
          &holder_identity,
          std::slice::from_ref(&owner_certificate),
          shape(),
        )
        .unwrap();
        holder(endpoint, start_rx, host, done).await;
      })
      .unwrap();
    dials.push((host, owner_socket, holder_address, holder_certificate));
  }
  sim
    .spawn_on(shard, async move {
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
      scenario(sessions, commands).await;
    })
    .unwrap();
  sim.run_until_idle();
  // Non-vacuity of every scenario here: its sessions' datagrams crossed the simulated fabric (its paths, links and
  // clock), not an OS socket beside it.
  let fabric = slates_rt::sim::sim_udp_stats();
  assert!(
    fabric.delivered > 0,
    "the sessions ran over the simulated fabric: {fabric:?}"
  );
  drop(done_tx);
  done_rx
    .try_iter()
    .map(|(host, head, state)| (host, (head, state)))
    .collect()
}

/// Ships one write and returns the sessions, folding what stragglers have returned.
async fn ship_one(
  shipper: &mut Shipper,
  payload: Vec<u8>,
  sessions: Vec<(HostId, Endpoint)>,
  late: &mut Vec<slates_cluster::Stragglers>,
) -> (bool, Vec<(HostId, Endpoint)>) {
  let entry = shipper.append(payload).unwrap();
  let shipped = ship(
    shipper,
    (LIVE_ENTRY_STREAM, &entry.encode()),
    Some(entry.position),
    sessions,
    CommitBudget::hard(DEADLINE_NS, POLL_NS),
  )
  .await;
  let durable = shipper.durable() == Some(entry.position);
  let mut sessions = shipped.reusable;
  late.push(shipped.stragglers);
  let mut kept = Vec::new();
  for mut stragglers in late.drain(..) {
    let (recovered, done) = fold_late(shipper, &mut stragglers);
    sessions.extend(recovered);
    if !done {
      kept.push(stragglers);
    }
  }
  *late = kept;
  (durable, sessions)
}

fn shipper(credit: u64, retain: usize) -> Shipper {
  Shipper::new(
    (OWNER, OBJECT, HostEpoch(1)),
    Quorum { f: 1 },
    &[OWNER, HOLDERS[0], HOLDERS[1]],
    None,
    (credit, retain),
  )
}

/// Shape: how long a lost holder's session must have acknowledged nothing before its request is ended, the daemon's
/// liveness budget.
const SILENCE_NS: u64 = 1_000_000_000;

/// A-200: an exchange to a holder its caller has given up, whose session acknowledges nothing (a stopped process), ends
/// within a silence of the loss, not at its deadline, and gives its session up at once. Do send an entry to both holders,
/// holder 3 silent, with a watch that names holder 3 lost from the start and a deadline ten times the silence. Expect
/// every request of the exchange to have reported within the silence and a tenth more, holder 2's reply among them, and
/// holder 3 named cut and unanswered with its session not among those to lend again and no request still in flight when
/// the exchange returns: the caller drops its link on the spot (the KIND stall was a lend left with nothing behind it). Before, nothing but the reply or the
/// deadline ended it: an install to a stopped holder held its session for the whole of a 30 s budget in 12 of 12 runs
/// (diag-live-durability-58), and a holder restarted meanwhile waited it out to catch up (15 of 23 runs over 1 s against 1
/// of 24 with it ended, diag-live-durability-64).
#[test]
fn an_exchange_to_a_lost_silent_holder_ends_within_a_silence_not_at_its_deadline() {
  type Lend = (Vec<HostId>, Vec<HostId>, Vec<HostId>, bool);
  let (report_tx, report_rx) = channel::<(u64, Vec<HostId>, Lend)>();
  let _ = run(
    true,
    Box::new(move |sessions, commands| {
      Box::pin(async move {
        let mut shipper = shipper(2, 2);
        let entry = shipper.append(b"one".to_vec()).unwrap();
        let bytes = entry.encode();
        let began = slates_rt::futures::now_ns();
        let exchanged = slates_cluster::live::exchange_watched(
          (LIVE_ENTRY_STREAM, &bytes),
          sessions,
          CommitBudget::hard(10 * SILENCE_NS, POLL_NS),
          Some(slates_cluster::live::Watch {
            lost: |host| host == HOLDERS[1],
            silence_ns: SILENCE_NS,
          }),
          |_, _| false,
        )
        .await;
        let answered: Vec<HostId> = exchanged.replies.iter().map(|(host, _)| *host).collect();
        let reusable: Vec<HostId> = exchanged.reusable.iter().map(|(host, _)| *host).collect();
        let mut stragglers = exchanged.stragglers;
        let (_, all_back) = stragglers.recover_replies();
        let took = slates_rt::futures::now_ns().saturating_sub(began);
        let _ = report_tx.send((
          took,
          answered,
          (
            exchanged.cut.clone(),
            exchanged.unanswered.clone(),
            reusable,
            all_back,
          ),
        ));
        for (_, mut endpoint) in exchanged.reusable {
          let _ = endpoint
            .request(
              STOP_KIND,
              slates_transport::connection::Priority::Control,
              &[],
            )
            .await;
        }
        drop(commands);
      })
    }),
  );
  let (took, answered, (cut, unanswered, reusable, all_back)) = report_rx.try_recv().unwrap();
  assert!(
    answered.contains(&HOLDERS[0]),
    "holder 2 answered: {answered:?}"
  );
  assert_eq!(cut, vec![HOLDERS[1]], "holder 3's request was cut");
  assert!(
    unanswered.contains(&HOLDERS[1]) && !reusable.contains(&HOLDERS[1]),
    "its session is given up: {reusable:?}"
  );
  assert!(
    all_back,
    "no request is still in flight when the exchange returns"
  );
  assert!(
    took <= SILENCE_NS + SILENCE_NS / 10,
    "the exchange ended {took} ns after it began, past a silence ({SILENCE_NS} ns) of its holder's loss"
  );
}

/// Shape: a link slow enough that an install of [`INSTALL_BYTES`] takes about two silences to cross it.
const SLOW_LINK_BITS_PER_SECOND: u64 = 256_000;
/// Shape: an install under the holders' bound ([`CAP`]) that takes several silences to cross [`SLOW_LINK_BITS_PER_SECOND`].
const INSTALL_BYTES: usize = 60_000;

/// A-200: a holder its caller has given up but whose session is still taking the request's bytes is never cut off, so a
/// false suspicion of a live holder costs nothing. Do send holder 2 an install of 60 KB over a 256 kbit/s link, about two
/// silences of transfer, with a watch that names it lost the whole time. Expect it answered, and no request cut.
/// Non-vacuous: the exchange took longer than a silence, so a watch that read only the loss would have cut it.
#[test]
fn a_lost_holder_still_taking_the_request_is_never_cut_off() {
  let (report_tx, report_rx) = channel::<(u64, Vec<HostId>, Vec<HostId>)>();
  let _ = run(
    false,
    Box::new(move |sessions, commands| {
      Box::pin(async move {
        let mut shipper = shipper(2, 2);
        let _ = shipper.append(b"one".to_vec()).unwrap();
        let install = shipper.install(vec![7; INSTALL_BYTES]).unwrap();
        let bytes = install.encode();
        let link = slates_rt::sim::sim_udp_add_link(slates_rt::sim::SimLink {
          rate_bits_per_second: SLOW_LINK_BITS_PER_SECOND,
          queue_bytes: 1 << 20,
        });
        slates_rt::sim::sim_udp_set_path(slates_rt::sim::SimPath::in_order(0, 0).through(link));
        let (to_two, rest): (Vec<_>, Vec<_>) = sessions
          .into_iter()
          .partition(|(host, _)| *host == HOLDERS[0]);
        let began = slates_rt::futures::now_ns();
        let exchanged = slates_cluster::live::exchange_watched(
          (LIVE_STATE_STREAM, &bytes),
          to_two,
          CommitBudget::hard(10 * SILENCE_NS, POLL_NS),
          Some(slates_cluster::live::Watch {
            lost: |_| true,
            silence_ns: SILENCE_NS,
          }),
          |_, _| true,
        )
        .await;
        let took = slates_rt::futures::now_ns().saturating_sub(began);
        let answered: Vec<HostId> = exchanged.replies.iter().map(|(host, _)| *host).collect();
        let _ = report_tx.send((took, answered, exchanged.cut.clone()));
        slates_rt::sim::sim_udp_set_path(slates_rt::sim::SimPath::NONE);
        for (_, mut endpoint) in exchanged.reusable.into_iter().chain(rest) {
          let _ = endpoint
            .request(
              STOP_KIND,
              slates_transport::connection::Priority::Control,
              &[],
            )
            .await;
        }
        drop(commands);
      })
    }),
  );
  let (took, answered, cut) = report_rx.try_recv().unwrap();
  assert!(
    took > SILENCE_NS,
    "the install took longer than a silence: {took} ns"
  );
  assert_eq!(
    answered,
    vec![HOLDERS[0]],
    "holder 2 answered (cut: {cut:?}, took {took} ns)"
  );
  assert!(cut.is_empty(), "no request was cut: {cut:?}");
}

/// A-200: a holder that joins late (it held nothing while six writes became durable through the other holder alone) is
/// told behind, is installed at the owner's head by the owner's period (`Shipper::pending`), and from then on applies
/// entries; both holders end holding exactly the owner's state. Over real endpoints on the simulated fabric.
#[test]
fn a_holder_that_joins_late_is_installed_and_then_follows() {
  let (report_tx, report_rx) = channel();
  let states = run(
    false,
    Box::new(move |sessions, commands| {
      Box::pin(async move {
        let mut shipper = shipper(4, 2);
        let mut late = Vec::new();
        let mut written = Vec::new();
        let (mut sessions, mut aside): (Vec<_>, Vec<_>) = sessions
          .into_iter()
          .partition(|(host, _)| *host == HOLDERS[0]);
        for value in 0..WRITES {
          let payload = value.to_le_bytes().to_vec();
          written.extend_from_slice(&payload);
          let (durable, back) = ship_one(&mut shipper, payload, sessions, &mut late).await;
          assert!(durable, "write {value} durable through one holder");
          sessions = gather(&mut shipper, back, &mut late, 1).await;
        }
        // The late holder joins: the next write reaches it, it holds nothing, and it says so.
        sessions.append(&mut aside);
        let payload = WRITES.to_le_bytes().to_vec();
        written.extend_from_slice(&payload);
        let (durable, back) = ship_one(&mut shipper, payload, sessions, &mut late).await;
        assert!(durable, "the joining write is durable");
        sessions = gather(&mut shipper, back, &mut late, 2).await;
        // The owner's period: send each holder what it lacks until neither lacks anything.
        let mut installs = 0;
        for _ in 0..8 {
          let mut quiet = true;
          for (host, endpoint) in std::mem::take(&mut sessions) {
            let (stream, message) = match shipper.pending(host) {
              Next::Nothing => {
                sessions.push((host, endpoint));
                continue;
              }
              Next::Entries(entries) => (LIVE_ENTRY_STREAM, entries[0].encode()),
              Next::Install => {
                installs += 1;
                (
                  LIVE_STATE_STREAM,
                  shipper.install(written.clone()).unwrap().encode(),
                )
              }
              Next::Fenced(epoch) => panic!("fenced at {epoch:?}"),
            };
            quiet = false;
            let shipped = ship(
              &mut shipper,
              (stream, &message),
              None,
              vec![(host, endpoint)],
              CommitBudget::hard(DEADLINE_NS, POLL_NS),
            )
            .await;
            late.push(shipped.stragglers);
            sessions.extend(shipped.reusable);
          }
          sessions = gather(&mut shipper, sessions, &mut late, 2).await;
          if quiet {
            break;
          }
        }
        // And it follows: the next write is applied by both holders as an entry.
        let payload = (WRITES + 1).to_le_bytes().to_vec();
        written.extend_from_slice(&payload);
        let (durable, back) = ship_one(&mut shipper, payload, sessions, &mut late).await;
        assert!(durable, "the write after the install is durable");
        let sessions = gather(&mut shipper, back, &mut late, 2).await;
        let _ = report_tx.send((installs, shipper.head(), written));
        for (_, mut endpoint) in sessions {
          let _ = endpoint
            .request(
              STOP_KIND,
              slates_transport::connection::Priority::Control,
              &[],
            )
            .await;
        }
        drop(commands);
      })
    }),
  );
  let (installs, head, written) = report_rx.try_recv().unwrap();
  assert_eq!(installs, 1, "the late holder was installed once");
  assert_eq!(states.len(), 2, "both holders reported");
  for (host, (held, state)) in states {
    assert_eq!(held, head, "holder {host:?} ends at the owner's head");
    assert_eq!(
      state, written,
      "holder {host:?} ends holding the owner's state"
    );
  }
}

/// Waits (parked) until `want` sessions are back, folding every straggler's reply on the way.
async fn gather(
  shipper: &mut Shipper,
  mut sessions: Vec<(HostId, Endpoint)>,
  late: &mut Vec<slates_cluster::Stragglers>,
  want: usize,
) -> Vec<(HostId, Endpoint)> {
  loop {
    let mut pending = Vec::new();
    for mut stragglers in late.drain(..) {
      let (recovered, done) = fold_late(shipper, &mut stragglers);
      sessions.extend(recovered);
      if !done {
        pending.push(stragglers);
      }
    }
    *late = pending;
    if sessions.len() >= want || late.is_empty() {
      sessions.sort_by_key(|(host, _)| host.0);
      return sessions;
    }
    slates_rt::futures::sleep(SERVE_SLICE_NS).await.unwrap();
  }
}

/// A-200, the live lane's straggler wait (`slates_server::live_ship::finish_stragglers`, `Stragglers::settle`): do, in a
/// task of its own as a live lane is, ship an entry to both holders with the second silent, take the first answer, settle
/// the exchange's stragglers and end the task; then wait out the dispatch's deadline and recover anything still owed.
/// Expect both holders' sessions back. An exchange's requests are children of the task that made it, and a task's end
/// cancels its children: a request still in flight when the task ended was dropped with its holder's session in it,
/// never reported, and the session's slot stayed lent for good, so the owner never shipped that holder again (KIND,
/// both of an owner's sessions `lent=true`, raft-chaos run 42318867890). Red-checked by mutation: without the settle the
/// silent holder's session never comes back.
#[test]
fn a_lane_that_ends_with_a_holder_unanswered_gets_every_session_back() {
  let (report_tx, report_rx) = channel::<Vec<HostId>>();
  let _ = run(
    true,
    Box::new(move |sessions, commands| {
      Box::pin(async move {
        let mut shipper = shipper(2, 2);
        let entry = shipper.append(b"one".to_vec()).unwrap();
        let bytes = entry.encode();
        let (back_tx, back_rx) = channel();
        let lane = slates_rt::futures::spawn(async move {
          let exchanged = slates_cluster::live::exchange(
            (LIVE_ENTRY_STREAM, &bytes),
            sessions,
            CommitBudget::hard(DEADLINE_NS, POLL_NS),
            |_, _| true,
          )
          .await;
          let mut back = exchanged.reusable;
          let mut stragglers = exchanged.stragglers;
          // The lane's wait: every request reports before its task ends.
          let (late, _) = stragglers.settle().await;
          back.extend(late.into_iter().map(|(host, _, endpoint)| (host, endpoint)));
          let _ = back_tx.send((back, stragglers));
        })
        .unwrap();
        let _ = slates_rt::futures::join(lane).await;
        let (mut back, mut stragglers) = back_rx.try_recv().unwrap();
        // Anything a straggler still owed comes back by the dispatch's deadline; a request cancelled with its task never
        // does.
        slates_rt::futures::sleep(DEADLINE_NS).await.unwrap();
        back.extend(stragglers.recover().0);
        let mut hosts: Vec<HostId> = back.iter().map(|(host, _)| *host).collect();
        hosts.sort_by_key(|host| host.0);
        let _ = report_tx.send(hosts);
        // The silent holder serves from here if its session came back, and every holder whose session did is told to
        // stop; one whose session did not is let go unstarted.
        if back.iter().any(|(host, _)| *host == HOLDERS[1]) {
          let _ = commands[&HOLDERS[1]].send(());
        }
        for (_, mut endpoint) in back {
          let _ = endpoint
            .request(
              STOP_KIND,
              slates_transport::connection::Priority::Control,
              &[],
            )
            .await;
        }
        drop(commands);
      })
    }),
  );
  let hosts = report_rx.try_recv().unwrap();
  assert_eq!(
    hosts,
    HOLDERS.to_vec(),
    "every session the lane took came back"
  );
}
