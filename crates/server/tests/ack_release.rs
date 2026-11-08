//! An acknowledgement answered once the client's own shard has released its records (A-216, §4.9 "Exactly-once"):
//! a release lagging on another shard is never met by a request as if it were new, and a release refused there is
//! counted and taken over by the next watermark. A raw client over the real rendezvous and rings, choosing its own
//! request ids, against an in-process two-shard daemon whose other shard is held busy.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::panic_in_result_fn
)]
#![cfg(unix)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_ipc::protocol::{NamePolicy, Refusal, ReplyBody, RequestBody, SizeClass, pack, unpack};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

/// Shape: the reply deadline (nanoseconds): five seconds, far past any hold here.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon to come up or a full ring to drain.
const CREDIT_WAIT: Duration = Duration::from_secs(5);
/// Shape: how long the other shard is held: one liveness budget. A reply that waited for it took at least that; one
/// that did not comes in microseconds.
const HOLD: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Shape: how often a hold that lasts until the client's answer looks whether it may end: a hundredth of the hold the
/// other tests use, so it ends promptly once the answer is in.
const HOLD_POLL: Duration = Duration::from_millis(10);
/// Shape: the names tried while looking for one the other shard owns; with two shards a run that finds none in this
/// many is a fault of the hash, not luck.
const NAME_TRIES: usize = 64;

/// A raw client: its ring end and its own request sequence, so a test can retry any id it chooses.
struct Client {
  end: ClientEnd,
  client: u32,
  sequence: u32,
}

impl Client {
  fn connect(instance: &str) -> Client {
    let started = Instant::now();
    loop {
      match connect(instance) {
        Ok(connected) => {
          let client = connected.region.client_id();
          return Client {
            end: ClientEnd::connected(connected),
            client,
            sequence: 0,
          };
        }
        Err(IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
          std::thread::yield_now();
        }
        Err(e) => panic!("{e}"),
      }
    }
  }

  /// The id the next fresh request takes.
  fn next(&mut self) -> RequestId {
    self.sequence += 1;
    RequestId {
      client: self.client,
      sequence: self.sequence,
    }
  }

  /// Sends `body` as the next request and waits for the reply.
  fn call(&mut self, body: &RequestBody) -> (RequestId, ReplyBody) {
    let id = self.next();
    (id, self.call_as(id, body))
  }

  /// Sends `body` under `id` (a retry reuses an id) and waits for the reply.
  fn call_as(&mut self, id: RequestId, body: &RequestBody) -> ReplyBody {
    let index = self.end.next_request_index();
    let slot = pack(
      self.end.region_mut(),
      slates_ipc::protocol::Direction::Request,
      index,
      id.word(),
      body,
    )
    .unwrap();
    let started = Instant::now();
    loop {
      match self.end.send(&slot) {
        Ok(()) => break,
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::thread::yield_now(),
        Err(e) => panic!("{e}"),
      }
    }
    let reply = self
      .end
      .wait(Some(DEADLINE_NS))
      .unwrap_or_else(|e| panic!("{e} waiting for {id:?} {body:?}"));
    assert_eq!(reply.request, id.word(), "the reply answers the request");
    unpack(self.end.region(), reply.kind, &reply.payload).unwrap()
  }
}

fn daemon(name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("ackrel-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("ackrel-{name}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn create(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// The partition that serves `client_id`, asked of each shard.
fn serving(daemon: &Daemon, client_id: u32) -> usize {
  daemon
    .shards()
    .iter()
    .position(|shard| {
      daemon
        .observation(
          Some(*shard),
          OBSERVE_BUDGET_NS,
          move |state: &mut ShardState| {
            state
              .clients
              .iter()
              .any(|(_, slot)| slot.client_id == client_id)
          },
        )
        .unwrap()
        .wait()
        .unwrap()
    })
    .expect("a shard serves the client")
}

/// A name the shard at `partition` owns, so a create of it records its completion there.
fn name_on(partition: usize, tag: &str) -> String {
  (0..NAME_TRIES)
    .map(|n| format!("{tag}-{n}"))
    .find(|name| usize::from(slates_server::verbs::owner_of_name(name, 2)) == partition)
    .expect("a name the partition owns")
}

/// Holds the shard at `partition` for `hold`: its thread runs nothing else meanwhile. Returns once the hold has
/// begun, with the thread that waits for it to end.
fn hold(
  daemon: &Daemon,
  partition: usize,
  hold: Duration,
  held: &'static AtomicBool,
) -> std::thread::JoinHandle<()> {
  held.store(false, Ordering::Release);
  let shard = daemon.shards()[partition];
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      held.store(true, Ordering::Release);
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(hold);
    })
    .unwrap();
  let waiter = std::thread::spawn(move || observation.wait().unwrap());
  let started = Instant::now();
  while !held.load(Ordering::Acquire) {
    assert!(started.elapsed() < CREDIT_WAIT, "the hold began");
    std::thread::yield_now();
  }
  waiter
}

/// Holds the shard at `partition` until the test lowers `holding` (or the client's reply deadline passes): its thread
/// runs nothing else meanwhile. Returns once the hold has begun, with the thread that waits for it to end. A scatter's
/// part must be refused while its shard is held, and the part's wait for room ends by the scatter's own shard's clock,
/// which on a busy machine runs late against any fixed hold: the scheduled CI run 42206043007 on ubuntu x86_64 tried
/// the part once more after a one-second hold had ended, the drained channel took it, and the listing was answered in
/// full. So the hold ends only once the client has its answer.
///
/// The hold's flags are its own: the scatter tests run in parallel, and with one flag shared among them the first test
/// to be answered ended the others' holds (ten of ten rounds failed on CI's loaded Linux runners, diag `ack-1`). Returns
/// the flag that ends the hold when lowered.
fn hold_until_answered(
  daemon: &Daemon,
  partition: usize,
) -> (std::thread::JoinHandle<()>, &'static AtomicBool) {
  let held = &*Box::leak(Box::new(AtomicBool::new(false)));
  let holding = &*Box::leak(Box::new(AtomicBool::new(true)));
  let shard = daemon.shards()[partition];
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      held.store(true, Ordering::Release);
      let started = Instant::now();
      while holding.load(Ordering::Acquire) && started.elapsed() < Duration::from_nanos(DEADLINE_NS)
      {
        let (_hold, rx) = std::sync::mpsc::channel::<()>();
        let _ = rx.recv_timeout(HOLD_POLL);
      }
    })
    .unwrap();
  let waiter = std::thread::spawn(move || observation.wait().unwrap());
  let started = Instant::now();
  while !held.load(Ordering::Acquire) {
    assert!(started.elapsed() < CREDIT_WAIT, "the hold began");
    std::thread::yield_now();
  }
  (waiter, holding)
}

/// A-216, §4.9. Do: on a two-shard daemon, create a volume the other shard owns (its completion is recorded there),
/// then hold that shard for a second and, inside the hold, acknowledge up to the create and retry the create's id; after
/// the hold, create another volume there and retry the first create's id once more. Expect: the acknowledgement is
/// answered in under half the hold, although the held shard has not released; the retry inside the hold is answered
/// `DuplicateRequest` at once, by the client's own shard, never run again and never answered from the lagging record;
/// the new create makes a new volume; and the last retry is `DuplicateRequest` too. A lagging release concerns only
/// sequences at or below the watermark, and every request passes the client's own shard's window first.
#[test]
fn a_lagging_release_is_never_met_by_a_request_as_new() {
  check_lagging(&run_lagging());
}

/// What [`a_lagging_release_is_never_met_by_a_request_as_new`] saw.
struct Lagging {
  volume: slates_ipc::protocol::VolumeId,
  acknowledged: ReplyBody,
  acknowledged_in: Duration,
  retried: ReplyBody,
  retried_in: Duration,
  new: ReplyBody,
  again: ReplyBody,
}

fn run_lagging() -> Lagging {
  static HELD: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("lag");
  let mut client = Client::connect(&instance);
  let other = 1 - serving(&daemon, client.client);
  let first = name_on(other, "lag-first");
  let (created_id, created) = client.call(&create(&first));
  let ReplyBody::Created { id: volume } = created else {
    panic!("create: {created:?}");
  };
  assert_eq!(
    usize::from(slates_server::verbs::owner_of(volume)),
    other,
    "the volume is the other shard's"
  );
  let holding = hold(&daemon, other, HOLD, &HELD);
  let held_at = Instant::now();
  let (_, acknowledged) = client.call(&RequestBody::Acknowledge {
    up_to: created_id.sequence,
    owed: Vec::new(),
  });
  let acknowledged_in = held_at.elapsed();
  let retried = client.call_as(created_id, &create(&first));
  let retried_in = held_at.elapsed();
  holding.join().unwrap();
  let (_, new) = client.call(&create(&name_on(other, "lag-second")));
  let again = client.call_as(created_id, &create(&first));
  daemon.stop();
  Lagging {
    volume,
    acknowledged,
    acknowledged_in,
    retried,
    retried_in,
    new,
    again,
  }
}

fn is_duplicate(reply: &ReplyBody) -> bool {
  matches!(
    reply,
    ReplyBody::Refused {
      refusal: Refusal::DuplicateRequest
    }
  )
}

fn check_lagging(run: &Lagging) {
  assert!(
    matches!(run.acknowledged, ReplyBody::Acknowledged),
    "acknowledged: {:?}",
    run.acknowledged
  );
  assert!(
    run.acknowledged_in < HOLD / 2,
    "the acknowledgement was answered in {:?}, against a {HOLD:?} hold of the other shard",
    run.acknowledged_in
  );
  assert!(
    is_duplicate(&run.retried),
    "a retry at the watermark is refused by the client's own shard: {:?}",
    run.retried
  );
  assert!(
    run.retried_in < HOLD / 2,
    "the retry was answered in {:?}, inside the hold",
    run.retried_in
  );
  assert!(
    matches!(run.new, ReplyBody::Created { id } if id != run.volume),
    "a new request is new: {:?}",
    run.new
  );
  assert!(
    is_duplicate(&run.again),
    "after the release too: {:?}",
    run.again
  );
}

/// A-216, §4.9's bound. Do: on a two-shard daemon, with a volume the other shard owns, hold that shard and fill its
/// control channel, then acknowledge, so its release cannot even be sent; end the hold; then ask the volume's status
/// again and again without acknowledging, until refused; then acknowledge and ask once more. Expect: the acknowledgement
/// is answered `Acknowledged` (the client's own shard released) and the refused release is counted
/// `ack.release_refused`; the statuses are refused `AcknowledgementOwed` once the other shard's retained records reach
/// the bound, never stalled or run as duplicates; and the next acknowledgement's watermark releases them there, so a
/// new status is served again.
#[test]
fn a_refused_release_is_counted_and_the_next_watermark_takes_it_over() {
  check_refused(&run_refused());
}

/// What [`a_refused_release_is_counted_and_the_next_watermark_takes_it_over`] saw.
struct RefusedRelease {
  flooded: usize,
  acknowledged: ReplyBody,
  counted: (u64, u64),
  served: u64,
  owed: Option<Refusal>,
  released: ReplyBody,
  fresh: ReplyBody,
}

fn release_refusals(daemon: &Daemon) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get("ack.release_refused")
    .copied()
    .unwrap_or(0)
}

/// Asks `volume`'s status up to `most` times, never acknowledging: how many were served, and the refusal that ended
/// them.
fn statuses_until_owed(
  client: &mut Client,
  volume: slates_ipc::protocol::VolumeId,
  most: u64,
) -> (u64, Option<Refusal>) {
  let mut served = 0u64;
  for _ in 0..most {
    match client.call(&RequestBody::Status { volume }).1 {
      ReplyBody::Status { .. } => served += 1,
      ReplyBody::Refused {
        refusal: found @ Refusal::AcknowledgementOwed { .. },
      } => return (served, Some(found)),
      other => panic!("status: {other:?}"),
    }
  }
  (served, None)
}

fn run_refused() -> RefusedRelease {
  static HELD: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("refused");
  let mut client = Client::connect(&instance);
  let other = 1 - serving(&daemon, client.client);
  let (_, created) = client.call(&create(&name_on(other, "refused")));
  let ReplyBody::Created { id: volume } = created else {
    panic!("create: {created:?}");
  };
  let slots = u64::try_from(client.end.region().cmd().slots()).unwrap();
  let before = release_refusals(&daemon);
  let holding = hold(&daemon, other, HOLD, &HELD);
  let flooded = daemon.flood_control_channel_of(other).unwrap();
  let (_, acknowledged) = client.call(&RequestBody::Acknowledge {
    up_to: client.sequence,
    owed: Vec::new(),
  });
  holding.join().unwrap();
  let counted = (before, release_refusals(&daemon));
  let (served, owed) = statuses_until_owed(&mut client, volume, slots * 6);
  let answered = client.sequence - 1;
  let (_, released) = client.call(&RequestBody::Acknowledge {
    up_to: answered,
    owed: Vec::new(),
  });
  let (_, fresh) = client.call(&RequestBody::Status { volume });
  daemon.stop();
  RefusedRelease {
    flooded,
    acknowledged,
    counted,
    served,
    owed,
    released,
    fresh,
  }
}

fn check_refused(run: &RefusedRelease) {
  check_counted(run);
  check_recovered(run);
}

/// The refused release: answered by the client's own shard, and counted.
fn check_counted(run: &RefusedRelease) {
  assert!(
    run.flooded > 0,
    "the other shard's control channel was filled"
  );
  assert!(
    matches!(run.acknowledged, ReplyBody::Acknowledged),
    "the client's own shard released: {:?}",
    run.acknowledged
  );
  assert!(
    run.counted.1 > run.counted.0,
    "the refused release is counted: {} before, {} after",
    run.counted.0,
    run.counted.1
  );
}

/// The bound met, typed, and the next watermark taking the refused release over.
fn check_recovered(run: &RefusedRelease) {
  assert!(
    matches!(run.owed, Some(Refusal::AcknowledgementOwed { .. })),
    "the other shard refused at its bound after {} statuses: {:?}",
    run.served,
    run.owed
  );
  assert!(
    matches!(run.released, ReplyBody::Acknowledged),
    "the next watermark: {:?}",
    run.released
  );
  assert!(
    matches!(run.fresh, ReplyBody::Status { .. }),
    "served again once the next watermark released: {:?}",
    run.fresh
  );
}

/// What a scatter answered with its other shard held and that shard's control channel full, and how long it took.
fn scattered_with_the_other_shard_full(
  name: &str,
  body: &RequestBody,
) -> (usize, ReplyBody, Duration) {
  let (daemon, instance) = daemon(name);
  let mut client = Client::connect(&instance);
  let other = 1 - serving(&daemon, client.client);
  let (holding, hold_on) = hold_until_answered(&daemon, other);
  let flooded = daemon.flood_control_channel_of(other).unwrap();
  let started = Instant::now();
  let (_, reply) = client.call(body);
  let took = started.elapsed();
  hold_on.store(false, Ordering::Release);
  holding.join().unwrap();
  let shard = usize::from(daemon.shards()[other].0);
  daemon.stop();
  assert!(flooded > 0, "the other shard's control channel was filled");
  (shard, reply, took)
}

/// The answer a scatter must give when a part cannot be sent to `shard`: the typed refusal. The other shard is held until
/// the answer is in ([`hold_until_answered`]), so a part that waited however long on a busy shard still met a full
/// channel; before, a fixed hold could end first, and the drained channel took the part. The
/// part waits for room for half the liveness budget first, so the answer comes after that, and within the client's reply
/// deadline. The answer's delivery is not held to the hold: on a busy shard it takes longer, and only the decision's
/// moment is the design's (2026-11-06).
fn check_typed_refusal(verb: &str, (other, reply, took): (usize, ReplyBody, Duration)) {
  assert!(
    matches!(reply, ReplyBody::Refused { refusal: Refusal::Overloaded { shard } } if usize::from(shard) == other),
    "{verb} with the other shard's control channel full: {reply:?}"
  );
  assert!(
    took >= HOLD / 2 && took < Duration::from_nanos(DEADLINE_NS),
    "{verb} was answered in {took:?}: after its part waited half the liveness budget for room, within the client's reply \
     deadline"
  );
}

/// §4.14, A-216's sibling sweep. Do: on a two-shard daemon, hold the other shard and fill its control channel, then
/// list the volumes. Expect: `Overloaded { shard }` naming the other shard, once its part has waited half the liveness
/// budget for room and inside the hold. Before, the
/// listing's part for that shard was settled through a nested state borrow that never ran, and the listing was never
/// answered.
#[test]
fn a_listing_whose_part_cannot_be_sent_is_refused_overloaded() {
  check_typed_refusal(
    "list",
    scattered_with_the_other_shard_full("list-full", &RequestBody::List { after: None }),
  );
}

/// §4.8 grants, A-216's sibling sweep. Do: as the listing test, but ask for the grants. Expect: `Overloaded { shard }`,
/// after the wait for room and inside the hold; before, never answered.
#[test]
fn a_grants_listing_whose_part_cannot_be_sent_is_refused_overloaded() {
  check_typed_refusal(
    "grants",
    scattered_with_the_other_shard_full("grants-full", &RequestBody::Grants),
  );
}

/// §4.14 `slates.status`, A-216's sibling sweep. Do: as the listing test, but ask for the daemon's status. Expect:
/// `Overloaded { shard }`, after the wait for room and inside the hold; before, never answered.
#[test]
fn a_daemon_status_whose_part_cannot_be_sent_is_refused_overloaded() {
  check_typed_refusal(
    "status",
    scattered_with_the_other_shard_full("status-full", &RequestBody::DaemonStatus),
  );
}

/// Shape: how long the starver keeps the client's own shard busy at a time: twice the room wait's pause (10 ms), so each
/// of the part's pauses runs late by more than it lasts.
const STARVED_FOR: Duration = Duration::from_millis(20);

/// §4.9, §4.14, the busy machine (Ada's rule: the normal case). Do: as `a_daemon_status_whose_part_cannot_be_sent_…`,
/// with the client's own shard, where the status's part waits for room, kept busy 20 ms at a time back to back
/// meanwhile. Expect: the typed refusal still comes after half the liveness budget and inside the hold. The part's wait
/// for room is a span of time, so a shard whose every pause runs late still waits its budget and not much more. Before,
/// it was 51 tries of a 10 ms pause each, and on a busy shard each try lasted a pause plus a busy poll: the wait ran past
/// the hold, as it did once at load 62–68 on macOS (2026-11-06).
#[test]
fn a_status_on_a_starved_shard_is_refused_overloaded_on_time() {
  static STARVING: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("status-starved");
  let mut client = Client::connect(&instance);
  let mine = serving(&daemon, client.client);
  let other = 1 - mine;
  let (holding, hold_on) = hold_until_answered(&daemon, other);
  let flooded = daemon.flood_control_channel_of(other).unwrap();
  STARVING.store(true, Ordering::Release);
  let caller = std::thread::spawn(move || {
    let started = Instant::now();
    let (_, reply) = client.call(&RequestBody::DaemonStatus);
    let took = started.elapsed();
    STARVING.store(false, Ordering::Release);
    (reply, took)
  });
  let shard_of_client = daemon.shards()[mine];
  // Until the call ends, however it ends: a call that panics never lowers the flag.
  while STARVING.load(Ordering::Acquire) && !caller.is_finished() {
    let starved = daemon
      .observation(
        Some(shard_of_client),
        OBSERVE_BUDGET_NS,
        |_: &mut ShardState| {
          // A busy poll of a fixed length: a spin, since a short sleep on macOS under a launchd job's timer policy
          // comes back several times late (CLAUDE.md, the loop-responsiveness gotcha).
          let began = Instant::now();
          while began.elapsed() < STARVED_FOR {
            std::hint::spin_loop();
          }
        },
      )
      .unwrap();
    let _ = starved.wait();
  }
  let (reply, took) = caller.join().unwrap();
  hold_on.store(false, Ordering::Release);
  holding.join().unwrap();
  let shard = usize::from(daemon.shards()[other].0);
  daemon.stop();
  assert!(flooded > 0, "the other shard's control channel was filled");
  println!("status on a starved shard answered in {took:?}");
  check_typed_refusal("status on a starved shard", (shard, reply, took));
}
