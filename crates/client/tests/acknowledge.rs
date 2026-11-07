//! A synchronous call is never held behind the client's periodic acknowledgement (§4.9's completion records, A-216,
//! R9): the acknowledgement releases the client's records on every shard, and its reply waited for all of them, so the
//! call that sent it first paid every shard's turn, a parked shard's wake included. Since A-216 the reply is sent once
//! the client's own shard has released, and the other shards' releases run as tracked tasks. Driven through the Rust
//! client against an in-process two-shard daemon whose other shard is held busy.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn
)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_anchor::AnchorSegment;
use slates_client::{Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass, VolumeId};
use slates_ipc::HeldRendezvous;
use slates_ipc::protocol::{ReplyBody, RequestBody};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the test's machine profile (milliseconds), as `client.rs` measures it.
const PROBE_MS: u64 = 5;
/// Shape: how long the other shard is held: one liveness budget, so a call that waits for it waits out a second, and a
/// call that does not returns in microseconds.
const HOLD: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Raised by the held shard when its hold begins, so the test's calls start inside it.
static HELD: AtomicBool = AtomicBool::new(false);

/// Shape: the volumes made while looking for one the client's own shard owns; with two shards, a run that finds none
/// in this many is a fault of the placement, not luck.
const PLACEMENT_TRIES: usize = 64;

fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  let measured = PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures");
  static RESERVED: OnceLock<bool> = OnceLock::new();
  let _ = RESERVED.get_or_init(|| {
    let config = DaemonConfig::derive(&measured, "ack-reserve", Some(2));
    slates_server::seal_keys::reserve(config.seal_key_slots)
  });
  measured
}

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn two_shard_daemon(tag: &str) -> (Daemon, Client) {
  let profile = profile();
  let instance = format!("ack-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-ack-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let started = Instant::now();
  let client = loop {
    match Client::connect(&instance, deadlines()) {
      Ok(client) => break client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < HOLD.saturating_mul(10) =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  };
  (daemon, client)
}

/// The index of the daemon's shard that serves `client_id`.
fn serving_shard(daemon: &Daemon, client_id: u32) -> usize {
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

/// A volume the client's own shard owns, so a status of it is served there alone.
fn local_volume(daemon: &Daemon, client: &mut Client, mine: usize) -> VolumeId {
  for attempt in 0..PLACEMENT_TRIES {
    let id = client
      .create(&CreateSpec {
        name: format!("local-{attempt}"),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    if usize::from(slates_server::verbs::owner_of(id)) == mine {
      return id;
    }
  }
  panic!(
    "no volume of {PLACEMENT_TRIES} landed on shard {mine} of {}",
    daemon.shards().len()
  );
}

/// A-216, §4.9, R9. Do: on a two-shard daemon, hold the shard that does not serve the client for a second, and
/// meanwhile make status calls on a volume the client's own shard owns until one of them sends the client's periodic
/// acknowledgement. Expect: that call returns in under half the hold, as every other does; the acknowledgement goes out
/// while the other shard is still held; and the client never has a second reply in flight (none dropped unawaited,
/// nothing outstanding after a call). Before A-216 the call that carried the acknowledgement waited for the held
/// shard's part of it and failed `Stalled` after the reply deadline (1 s).
#[test]
fn a_call_is_never_held_behind_the_periodic_acknowledgement() {
  let (daemon, mut client) = two_shard_daemon("held");
  let mine = serving_shard(&daemon, client.client_id());
  let id = local_volume(&daemon, &mut client, mine);
  let other = daemon
    .shards()
    .iter()
    .enumerate()
    .find(|(index, _)| *index != mine)
    .map(|(_, shard)| *shard)
    .unwrap();
  let hold = daemon
    .observation(Some(other), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      HELD.store(true, Ordering::Release);
      // The held shard's thread does nothing else for the hold: the scatter's part on it waits.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(HOLD);
    })
    .unwrap();
  let holder = std::thread::spawn(move || hold.wait());
  let waiting = Instant::now();
  while !HELD.load(Ordering::Acquire) {
    assert!(
      waiting.elapsed() < HOLD.saturating_mul(10),
      "the hold began"
    );
    std::thread::yield_now();
  }
  let held_at = Instant::now();
  let before = client.acknowledged();
  let mut acknowledging_call = None;
  while held_at.elapsed() < HOLD / 2 {
    let started = Instant::now();
    client.status(id).unwrap();
    let took = started.elapsed();
    assert!(
      client.outstanding().is_empty() && client.unawaited_dropped() == 0,
      "one reply in flight at a time: outstanding {:?}, dropped {}",
      client.outstanding(),
      client.unawaited_dropped()
    );
    if client.acknowledged() != before {
      acknowledging_call = Some(took);
      break;
    }
  }
  let sent_while_held = held_at.elapsed() < HOLD;
  holder.join().unwrap().unwrap();
  let took = acknowledging_call.expect("the client acknowledged within half the hold");
  eprintln!(
    "the call that sent the acknowledgement took {took:?}, against a {HOLD:?} hold of the other shard"
  );
  assert!(
    sent_while_held,
    "the acknowledgement went out while the other shard was held"
  );
  assert!(
    took < HOLD / 2,
    "the call that sent the acknowledgement took {took:?}, against a {HOLD:?} hold of the other shard"
  );
  daemon.stop();
}

/// Raised by the held shard of [`an_acknowledgement_never_covers_a_request_still_awaited`] when its hold begins.
static HELD_AWAITED: AtomicBool = AtomicBool::new(false);
/// Shape: the command ring's slots in [`an_acknowledgement_never_covers_a_request_still_awaited`]: the smallest
/// supported ring, so the periodic acknowledgement is due after two replies (`slots / 2`) and three requests fit in
/// flight at once.
const SMALL_RING_SLOTS: u32 = 4;

/// A create of `name` (1 MiB scratch).
fn create_body(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// A name whose create the shard at `partition` of two owns, so its completion is recorded there.
fn name_owned_by(partition: usize, tag: &str) -> String {
  (0..PLACEMENT_TRIES)
    .map(|n| format!("{tag}-{n}"))
    .find(|name| usize::from(slates_server::verbs::owner_of_name(name, 2)) == partition)
    .expect("a name the partition owns")
}

/// Waits for `word`'s reply, taking it (a refusal is the client's typed error).
fn take_reply(client: &mut Client, word: u64) -> Result<ReplyBody, ClientError> {
  let started = Instant::now();
  loop {
    match client.poll_reply_word(word) {
      Ok(Some(reply)) => return Ok(reply),
      Ok(None) => {}
      Err(error) => return Err(error),
    }
    assert!(
      started.elapsed() < HOLD.saturating_mul(10),
      "a reply to {word:#x}"
    );
    std::thread::yield_now();
  }
}

/// A daemon over the played anchor's segment and rendezvous, so a second one recovers the first one's records.
fn daemon_over(
  profile: &MachineProfile,
  config: &DaemonConfig,
  (segment, rendezvous): (&AnchorSegment, &HeldRendezvous),
) -> Daemon {
  let (handoff, len) = segment.handoff().unwrap();
  let content = segment.content_handoff().unwrap();
  let daemon = Daemon::start(
    profile,
    config.clone(),
    SegmentSource::Handoff {
      handoff,
      len,
      content,
      rendezvous: rendezvous.adoption(),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  daemon
}

/// §4.9 "Exactly-once", AC-2.3. Do: on a two-shard daemon under a played anchor, with a ring small enough that the
/// periodic acknowledgement is due after two replies, hold the shard that does not serve the client; begin a create
/// that shard owns (its reply waits for the hold), then two creates the client's own shard owns, take their two
/// replies and let the client send the acknowledgement it finds due; end the hold without taking the first create's
/// reply; restart the daemon over the same anchor, reconnect under the client's id and resend the first create under
/// its own id. Expect: the resend is answered with the create's own reply, the volume another client lists under its
/// name, never `DuplicateRequest`. A client with several requests in flight (any async caller, the multiplexing load
/// harness) acknowledges only below the lowest one it still awaits: an acknowledgement covering a request whose reply
/// has not come releases the completion record its retry needs, so the restart turns an applied create into a refusal
/// (and on the fleet, a deferred submit's acceptance into `DuplicateRequest`).
#[test]
fn an_acknowledgement_never_covers_a_request_still_awaited() {
  let profile = profile();
  let instance = format!("ack-awaited-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(2));
  config.region.slots = SMALL_RING_SLOTS;
  let segment = AnchorSegment::create(
    &format!("slates-seg-ack-awaited-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-ack-awaited-{}", std::process::id()),
    config.content_bytes(),
  )
  .unwrap();
  let rendezvous = HeldRendezvous::hold(&instance).unwrap();
  let first = daemon_over(&profile, &config, (&segment, &rendezvous));
  let started = Instant::now();
  let mut client = connect_within(&instance, started);
  let (awaited, applied) = acknowledge_past_a_held_create(&first, &mut client, started);
  first.stop();
  let second = daemon_over(&profile, &config, (&segment, &rendezvous));
  let reply = resend_after_restart(&mut client, awaited, started);
  eprintln!(
    "acknowledged up to {}, the awaited create's resend answered {reply:?}",
    client.acknowledged()
  );
  assert_eq!(
    reply,
    Ok(ReplyBody::Created { id: applied }),
    "the resend of a request still awaited meets its own completion record"
  );
  drop(client);
  second.stop();
}

/// A client of `instance`, retrying while the daemon comes up.
fn connect_within(instance: &str, started: Instant) -> Client {
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < HOLD.saturating_mul(10) =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// Holds the shard that does not serve `client`, begins a create it owns, takes two own-shard creates' replies and
/// sends the acknowledgement the client finds due; once the hold ends and another client sees the held create applied,
/// returns the held create's word (its reply never taken) and the volume it made.
fn acknowledge_past_a_held_create(
  daemon: &Daemon,
  client: &mut Client,
  started: Instant,
) -> (u64, VolumeId) {
  let mine = serving_shard(daemon, client.client_id());
  let other_index = 1 - mine;
  let hold = daemon
    .observation(
      daemon.shards().get(other_index).copied(),
      OBSERVE_BUDGET_NS,
      move |_: &mut ShardState| {
        HELD_AWAITED.store(true, Ordering::Release);
        // The held shard's thread does nothing else for the hold: the create it owns waits.
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(HOLD);
      },
    )
    .unwrap();
  let holder = std::thread::spawn(move || hold.wait());
  while !HELD_AWAITED.load(Ordering::Acquire) {
    assert!(
      started.elapsed() < HOLD.saturating_mul(10),
      "the hold began"
    );
    std::thread::yield_now();
  }
  let awaited_name = name_owned_by(other_index, "awaited");
  let awaited = client.begin(&create_body(&awaited_name)).unwrap().word();
  for tag in ["mine-a", "mine-b"] {
    let word = client
      .begin(&create_body(&name_owned_by(mine, tag)))
      .unwrap()
      .word();
    assert!(
      matches!(take_reply(client, word), Ok(ReplyBody::Created { .. })),
      "the own shard's create is answered inside the hold"
    );
    client.begin_ack_if_due().unwrap();
  }
  holder.join().unwrap().unwrap();
  (
    awaited,
    listed_volume(&daemon_instance(daemon), &awaited_name, started),
  )
}

/// The instance a daemon serves.
fn daemon_instance(daemon: &Daemon) -> String {
  daemon.instance().to_owned()
}

/// The id of the volume named `name`, once another client of `instance` lists it; then one more listing, so every
/// shard has served what was sent before it (a scatter reaches each), the acknowledgement's release too.
fn listed_volume(instance: &str, name: &str, started: Instant) -> VolumeId {
  let mut observer = Client::connect(instance, deadlines()).unwrap();
  let id = loop {
    if let Some(volume) = observer
      .list()
      .unwrap()
      .into_iter()
      .find(|volume| volume.name == name)
    {
      break volume.id;
    }
    assert!(
      started.elapsed() < HOLD.saturating_mul(10),
      "the held create is applied"
    );
  };
  observer.list().unwrap();
  id
}

/// Reconnects `client` under its id to the restarted daemon and resends `awaited` under its own id: its reply.
fn resend_after_restart(
  client: &mut Client,
  awaited: u64,
  started: Instant,
) -> Result<ReplyBody, ClientError> {
  while !client
    .try_reconnect()
    .unwrap_or_else(|e| panic!("reconnect: {e}"))
  {
    assert!(
      started.elapsed() < HOLD.saturating_mul(20),
      "the client reconnects"
    );
    std::thread::yield_now();
  }
  while !client
    .resend_awaited(awaited)
    .unwrap_or_else(|e| panic!("resend: {e}"))
  {
    assert!(
      started.elapsed() < HOLD.saturating_mul(20),
      "the resend goes"
    );
    std::thread::yield_now();
  }
  take_reply(client, awaited)
}
