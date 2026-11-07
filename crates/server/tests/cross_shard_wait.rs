//! A cross-shard call waits in the target shard's own time (§4.3, §4.14, the R3 debt of `LIVENESS_BUDGET_NS`, class A):
//! a shard kept busy, but making progress, answers late, and the call must still hear it; a shard that makes no
//! progress at all for a window is wedged, and the call gives up then. This is the rule observations use (A-65) and the
//! liveness watchdog uses (A-228). Each test drives one call family through the product's client against an in-process
//! two-shard daemon, with the shard the family calls kept in one poll that is starved (on the CPU a fifth of the time)
//! or wedged (off it).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::panic_in_result_fn
)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines};
use slates_ipc::protocol::{Refusal, ReplyBody, RequestBody};
use slates_rt::shard::ShardId;
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the liveness budget, each call's budget.
const WINDOW: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Shape: how long a starved poll lasts: a window and a half, so an answer behind it comes past a wall window.
const STARVED_FOR: Duration = Duration::from_millis(1_500);
/// Shape: how long a wedged poll lasts: three windows, so a call that gives up after one is told from one that waited
/// it out by two windows, far more than a busy machine delays a reply.
const WEDGED_FOR: Duration = Duration::from_secs(3);
/// Shape: the starved poll's time on the CPU and off it in each turn: a fifth of the time on it, as a thread kept off
/// its core by spinners runs, so its CPU in a window stays far under the call's budget while it keeps making progress.
const BURST: Duration = Duration::from_millis(10);
const OFF: Duration = Duration::from_millis(40);
/// Shape: the client's reply deadline: ten windows, far past any hold here, so what is measured is the daemon's own
/// wait on its shard and not the client's (the client's deadline is the liveness window, A-232's to derive).
const REPLY_WAIT_NS: u64 = 10 * LIVENESS_BUDGET_NS;
/// Shape: how long a client retries the rendezvous while the daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the clients connected while looking for one the shard that is not the control shard serves; with two shards a
/// run that finds none in this many is a fault of the placement, not luck.
const CLIENT_TRIES: usize = 16;

fn daemon(name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("xwait-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-xwait-{name}-{}", std::process::id()),
    },
  )
  .unwrap();
  (daemon, instance)
}

/// The shard that serves `client_id`, asked of each shard.
fn serving(daemon: &Daemon, client_id: u32) -> ShardId {
  daemon
    .shards()
    .iter()
    .copied()
    .find(|shard| {
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

/// A client of `instance` whose reply deadline is [`REPLY_WAIT_NS`], retrying the rendezvous while the daemon starts.
fn connect(instance: &str) -> Client {
  let deadlines = Deadlines {
    reply_ns: REPLY_WAIT_NS,
    ..common::client::deadlines()
  };
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// A client served on `shard`, with the others connected on the way (kept, so the placement does not hand their seats
/// back meanwhile).
fn client_on(daemon: &Daemon, instance: &str, shard: ShardId) -> (Client, Vec<Client>) {
  let mut others = Vec::new();
  for _ in 0..CLIENT_TRIES {
    let client = connect(instance);
    if serving(daemon, client.client_id()) == shard {
      return (client, others);
    }
    others.push(client);
  }
  panic!("no client of {CLIENT_TRIES} was served on {shard:?}");
}

/// Runs on the CPU for `span`, without yielding.
fn spin_for(span: Duration) {
  let started = Instant::now();
  while started.elapsed() < span {
    std::hint::spin_loop();
  }
}

/// How the called shard's poll spends its time.
#[derive(Clone, Copy)]
enum Held {
  /// On the CPU a fifth of the time: alive, and given little CPU.
  Starved,
  /// Off the CPU throughout: a host call that does not return.
  Wedged,
}

/// Puts one poll on `shard` (`STARVED_FOR` or `WEDGED_FOR` long) and returns once it has begun, with the thread that waits for it to end.
fn hold(
  daemon: &Daemon,
  shard: ShardId,
  held: Held,
  begun: &'static AtomicBool,
) -> std::thread::JoinHandle<()> {
  begun.store(false, Ordering::Release);
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      begun.store(true, Ordering::Release);
      let span = match held {
        Held::Starved => STARVED_FOR,
        Held::Wedged => WEDGED_FOR,
      };
      let started = Instant::now();
      while started.elapsed() < span {
        if matches!(held, Held::Starved) {
          spin_for(BURST);
        }
        let (_hold, rx) = std::sync::mpsc::channel::<()>();
        let _ = rx.recv_timeout(match held {
          Held::Starved => OFF,
          Held::Wedged => span.saturating_sub(started.elapsed()),
        });
      }
    })
    .unwrap();
  let waiter = std::thread::spawn(move || {
    let _ = observation.wait();
  });
  let began = Instant::now();
  while !begun.load(Ordering::Acquire) {
    assert!(began.elapsed() < WINDOW * 20, "the held poll began");
    std::thread::yield_now();
  }
  waiter
}

/// Bootstraps the daemon as `member` from `client` and returns the reply and how long it took. The member is asked of
/// the daemon before the hold, since the question is answered on the control shard.
fn bootstrap(member: u64, client: &mut Client) -> (Result<ReplyBody, ClientError>, Duration) {
  let started = Instant::now();
  let reply = client.call(&RequestBody::Bootstrap { root: true, member });
  (reply, started.elapsed())
}

/// R3 class A, the consensus verbs (`verbs.rs` `consensus_on_control`), A-65, §4.3, §4.8. Do: with a client served on
/// the shard that is not the control shard, keep the control shard in one starved poll for a window and a half, and
/// bootstrap the daemon from that client meanwhile. Expect: the bootstrap is acknowledged, after more than a wall
/// window. On main the call to the control shard gave up at the wall window with its answer still behind the starved
/// poll, and the client was refused `Overloaded` while the bootstrap went on to commit: a live shard's answer lost.
#[test]
fn a_bootstrap_whose_control_shard_is_starved_is_acknowledged_past_a_wall_window() {
  static BEGUN: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("boot-starved");
  let (control, other) = (daemon.shards()[0], daemon.shards()[1]);
  let (mut client, _others) = client_on(&daemon, &instance, other);
  let member = daemon.member_identity().unwrap().0;
  let holding = hold(&daemon, control, Held::Starved, &BEGUN);
  let (reply, took) = bootstrap(member, &mut client);
  holding.join().unwrap();
  println!("a bootstrap with its control shard starved: {reply:?} after {took:?}");
  assert!(
    took > WINDOW,
    "the answer came past a wall window ({took:?}), so the test is not vacuous"
  );
  assert!(
    matches!(reply, Ok(ReplyBody::Acknowledged)),
    "a starved control shard's bootstrap was lost after {took:?}: {reply:?}"
  );
  drop(client);
  daemon.stop();
}

/// R3 class A, the consensus verbs, A-65, §4.3. Do: as the starved case, with the control shard's poll off the CPU
/// throughout (wedged). Expect: the client is refused `Overloaded` naming the control shard, after a window and before
/// the wedge ends: a shard that makes no progress for a window is not waited for.
#[test]
fn a_bootstrap_whose_control_shard_is_wedged_is_refused_after_a_window() {
  static BEGUN: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("boot-wedged");
  let (control, other) = (daemon.shards()[0], daemon.shards()[1]);
  let (mut client, _others) = client_on(&daemon, &instance, other);
  let member = daemon.member_identity().unwrap().0;
  let holding = hold(&daemon, control, Held::Wedged, &BEGUN);
  let (reply, took) = bootstrap(member, &mut client);
  holding.join().unwrap();
  println!("a bootstrap with its control shard wedged: {reply:?} after {took:?}");
  assert!(
    matches!(reply, Err(ClientError::Refused(Refusal::Overloaded { shard })) if shard == control.0),
    "a wedged control shard refused the bootstrap Overloaded, got {reply:?}"
  );
  assert!(
    took >= WINDOW && took < WEDGED_FOR,
    "the wedged shard was given up after a window and before the wedge ended, not after {took:?}"
  );
  drop(client);
  daemon.stop();
}

/// Shape: the account the revoke tests enroll their consumer under: any one, since enrollment and revocation are
/// decided by the issuer's proof, not by the caller's own account.
const ACCOUNT: u32 = 1;

/// Bootstraps the daemon (a fresh one records nothing before, AC-8.1) and enrolls a consumer from `client` under the
/// daemon's issuer secret: its id, its capability and the secret. Done before any hold.
fn enrolled(daemon: &Daemon, client: &mut Client, account: u32) -> (u64, [u8; 32], [u8; 32]) {
  let member = daemon.member_identity().unwrap().0;
  let (booted, _) = bootstrap(member, client);
  assert!(
    matches!(booted, Ok(ReplyBody::Acknowledged)),
    "the daemon was bootstrapped: {booted:?}"
  );
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::enroll_proof(&secret, account);
  let enrolled = client.call(&RequestBody::Enroll { account, proof });
  let Ok(ReplyBody::Enrolled {
    consumer,
    secret: capability,
  }) = enrolled
  else {
    panic!("the consumer was enrolled: {enrolled:?}");
  };
  (consumer, capability, secret)
}

/// The shard that holds `consumer`'s record.
#[cfg(unix)]
fn owner_of(daemon: &Daemon, consumer: u64) -> ShardId {
  daemon.shards()[usize::from(slates_server::verbs::owner_of_consumer(consumer))]
}

/// The other shard of the two.
fn other_than(daemon: &Daemon, shard: ShardId) -> ShardId {
  *daemon
    .shards()
    .iter()
    .find(|other| **other != shard)
    .unwrap()
}

/// Revokes `consumer` from `client` with the issuer's proof: the reply and how long it took.
fn revoke(
  client: &mut Client,
  secret: &[u8; 32],
  consumer: u64,
) -> (Result<ReplyBody, ClientError>, Duration) {
  let proof = slates_server::landing::revoke_proof(secret, consumer);
  let started = Instant::now();
  let reply = client.call(&RequestBody::Revoke { consumer, proof });
  (reply, started.elapsed())
}

/// R3 class A, the consumer verbs (`verbs.rs` `revoke_everywhere`), A-65, §4.3, §4.13. Do: enroll a consumer, then,
/// from a client served on one shard, revoke it while the other shard runs one starved poll for a window and a half.
/// The revocation calls the consumer's own shard to record it and then every shard to mark it, so one of its calls
/// waits on the starved shard whichever holds the record. Expect: the consumer is revoked, after more than a wall
/// window. On main that call gave up at the wall second and the human was refused `NotFound` while the record stood.
#[test]
fn a_revoke_with_a_starved_shard_is_revoked_past_a_wall_window() {
  static BEGUN: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("revoke-starved");
  let mut client = connect(&instance);
  let (consumer, _, secret) = enrolled(&daemon, &mut client, ACCOUNT);
  let starved = other_than(&daemon, serving(&daemon, client.client_id()));
  let holding = hold(&daemon, starved, Held::Starved, &BEGUN);
  let (reply, took) = revoke(&mut client, &secret, consumer);
  holding.join().unwrap();
  println!("a revoke with a starved shard: {reply:?} after {took:?}");
  assert!(
    took > WINDOW,
    "the answer came past a wall window ({took:?}), so the test is not vacuous"
  );
  assert!(
    matches!(reply, Ok(ReplyBody::Revoked)),
    "a starved shard's revocation was lost after {took:?}: {reply:?}"
  );
  drop(client);
  daemon.stop();
}

/// R3 class A, the consumer verbs, A-65, §4.3. Do: as the starved revoke, with the other shard's poll off the CPU
/// throughout (wedged). Expect: refused `NotFound` after a window and before the wedge ends.
#[test]
fn a_revoke_with_a_wedged_shard_is_refused_after_a_window() {
  static BEGUN: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("revoke-wedged");
  let mut client = connect(&instance);
  let (consumer, _, secret) = enrolled(&daemon, &mut client, ACCOUNT);
  let wedged = other_than(&daemon, serving(&daemon, client.client_id()));
  let holding = hold(&daemon, wedged, Held::Wedged, &BEGUN);
  let (reply, took) = revoke(&mut client, &secret, consumer);
  holding.join().unwrap();
  println!("a revoke with a wedged shard: {reply:?} after {took:?}");
  assert!(
    matches!(reply, Err(ClientError::Refused(Refusal::NotFound))),
    "a wedged shard refused the revocation NotFound, got {reply:?}"
  );
  assert!(
    took >= WINDOW && took < WEDGED_FOR,
    "the wedged shard was given up after a window and before the wedge ended, not after {took:?}"
  );
  drop(client);
  daemon.stop();
}

/// R3 class A, the consumer verbs (`verbs.rs` attest), A-65, §4.3, §4.13. Unix: an attestation binds a channel whose
/// principal is the consumer's account's uid (`verify_attestation`). Do: enroll a consumer under this process's uid, then, from a client
/// served on the shard that does not hold its record, attest as it while the record's shard runs one starved poll for
/// a window and a half. Expect: the channel is attested, after more than a wall window. On main the call for the
/// consumer's record gave up at the wall second and the workload was refused `NotFound`, as if never enrolled.
#[cfg(unix)]
#[test]
fn an_attest_whose_record_shard_is_starved_is_attested_past_a_wall_window() {
  static BEGUN: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon("attest-starved");
  let mut owner = connect(&instance);
  let uid = rustix::process::getuid().as_raw();
  let (consumer, capability, _) = enrolled(&daemon, &mut owner, uid);
  let record = owner_of(&daemon, consumer);
  let (mut workload, _others) = client_on(&daemon, &instance, other_than(&daemon, record));
  let proof = slates_server::landing::attest_proof(&capability, workload.client_id());
  let holding = hold(&daemon, record, Held::Starved, &BEGUN);
  let started = Instant::now();
  let reply = workload.call(&RequestBody::Attest { consumer, proof });
  let took = started.elapsed();
  holding.join().unwrap();
  println!("an attest with its record's shard starved: {reply:?} after {took:?}");
  assert!(
    took > WINDOW,
    "the answer came past a wall window ({took:?}), so the test is not vacuous"
  );
  assert!(
    matches!(reply, Ok(ReplyBody::Attested)),
    "a starved shard's attestation was lost after {took:?}: {reply:?}"
  );
  drop(workload);
  drop(owner);
  daemon.stop();
}
