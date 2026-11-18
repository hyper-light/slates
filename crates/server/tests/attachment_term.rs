//! A CLI attachment's term (§4.7, A-230, A-384): it outlives the client that made it, is renewed by every verb that names
//! it and by its holder's `Renew`, ends at its term's lapse with the write lease it held, and is refused
//! `AttachmentExpired` when named after, never `NotFound`. Its term is its holder's: at most the node's failover delay,
//! within it the tail of its renewal gaps and a liveness window; a restart gives every attachment a fresh one. Each test
//! holds the failover delay short by injection (`Daemon::inject_failover`), since nothing sets it.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_client::{AttachRequest, ClientError, Intent, Refusal};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};

/// Shape: the failover delay each test holds its daemon at, so a CLI attachment's term before its holder's pace is
/// measured: past the reaper's silence and sweep (two liveness budgets), so a term's survival of its client's end is
/// seen, and short enough to wait out.
const TERM: Duration = Duration::from_secs(3);
/// Shape: the pause after which a renewal is due (more than half the term gone) and the term is not yet over.
const PAST_HALF: Duration = Duration::from_millis(2_000);
/// Shape: how long the lapse may take to be seen: the term, and one sweep of the reaper's cadence (a liveness budget).
const LAPSE_WAIT: Duration = Duration::from_secs(5);

/// A-230. Do: on a daemon whose failover delay is held at `TERM`, attach a volume for write in the CLI's form and end that
/// client;
/// from a second client, write a file under the attachment past half its term, then again past the first term's end
/// (the first write renewed it); then wait out a whole term with no use and name the attachment once more, by a write
/// and by a detach; then attach for write again. Expect: both writes succeed; once the term lapsed both uses are
/// refused `AttachmentExpired` naming the attachment, never `NotFound`; and the new write attachment is granted, the
/// lapsed one having let its write lease go.
#[test]
fn a_cli_attachment_lives_its_term_renewed_by_use_and_is_refused_as_expired_after() {
  let profile = common::machine_profile();
  let (daemon, instance) = term_daemon(&profile, "term", TERM);
  let attachment;
  let volume;
  {
    let mut maker = connect(&instance);
    volume = maker.create(&scratch("termed")).unwrap();
    attachment = maker
      .attach_with(volume, None, Intent::Write, AttachRequest::Termed)
      .unwrap()
      .attachment;
  }
  let mut user = connect(&instance);
  #[allow(clippy::disallowed_methods)] // a test pausing past half the term it checks
  std::thread::sleep(PAST_HALF);
  user
    .fs_write((volume, attachment), "first.txt", b"one", 0o644)
    .expect("a write within the term, after its client ended");
  #[allow(clippy::disallowed_methods)] // past the first term's end, within the renewed one
  std::thread::sleep(PAST_HALF);
  user
    .fs_write((volume, attachment), "second.txt", b"two", 0o644)
    .expect("a write past the first term, which the first write renewed");
  #[allow(clippy::disallowed_methods)] // a whole term and a sweep with no use
  std::thread::sleep(LAPSE_WAIT);
  let expired = match user.fs_write((volume, attachment), "late.txt", b"late", 0o644) {
    Err(ClientError::Refused(refusal)) => refusal,
    other => panic!("the attachment never lapsed: {other:?}"),
  };
  assert_eq!(expired, Refusal::AttachmentExpired { attachment });
  assert!(matches!(
    user.detach(attachment),
    Err(ClientError::Refused(Refusal::AttachmentExpired { attachment: named })) if named == attachment
  ));
  user
    .attach_with(volume, None, Intent::Write, AttachRequest::Root)
    .expect("the lapsed attachment let its write lease go");
  drop(user);
  daemon.stop();
}

/// Shape: how long the test waits for a shard to have taken a just-connected client.
const TAKEN_WAIT: Duration = Duration::from_secs(20);

/// The account the daemon authenticated `client_id`'s channel as, asked of each shard until one has taken the client:
/// the uid an enrolled consumer of that account is enrolled under (as `consumers.rs` reads it).
fn channel_account(daemon: &Daemon, client_id: u32) -> u32 {
  let started = std::time::Instant::now();
  loop {
    let found = daemon.shards().iter().copied().find_map(|shard| {
      daemon
        .observation(
          Some(shard),
          slates_server::daemon::OBSERVE_BUDGET_NS,
          move |state: &mut slates_server::state::ShardState| {
            state
              .clients
              .iter()
              .find(|(_, slot)| slot.client_id == client_id)
              .map(|(_, slot)| slot.principal.clone())
          },
        )
        .unwrap()
        .wait()
        .unwrap()
    });
    if let Some(principal) = found {
      let slates_db::catalog::Principal::Uid { uid, .. } = principal else {
        panic!("a channel authenticated as something other than a host account: {principal:?}");
      };
      return uid;
    }
    assert!(
      started.elapsed() < TAKEN_WAIT,
      "a shard took client {client_id}"
    );
    std::thread::yield_now();
  }
}

/// A-230 and §4.13: a CLI attachment's term is renewed only by its own principal's use. Do: attach a volume for write
/// in the CLI's form as the owner; enroll a consumer of the same host account and bind a second channel to it (another
/// principal); past half the term, have the consumer name the attachment by a write, a detach, a bind and an advance;
/// then, past the first term's end and within the term a renewal would have given, name it as the owner by a write.
/// Expect: each of the consumer's uses refused `Forbidden`, and the owner's write refused `AttachmentExpired`: another
/// principal's use renewed nothing. Before, every verb that names an attachment renewed it ahead of its principal
/// check (`verbs::use_attachment`), so any channel that knew the id kept another account's attachment alive.
#[test]
fn a_cli_attachment_is_renewed_by_its_own_principals_use_alone() {
  let profile = common::machine_profile();
  let (daemon, instance) = term_daemon(&profile, "term-foreign", TERM);
  let mut owner = connect(&instance);
  let volume = owner.create(&scratch("termed-foreign")).unwrap();
  let attachment = owner
    .attach_with(volume, None, Intent::Write, AttachRequest::Termed)
    .unwrap()
    .attachment;
  let mut stranger = connect(&instance);
  let account = channel_account(&daemon, stranger.client_id());
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::enroll_proof(&secret, account);
  let (consumer, capability) = owner.enroll(account, proof).unwrap();
  stranger.attest(consumer, &capability).unwrap();
  #[allow(clippy::disallowed_methods)] // a test pausing past half the term it checks
  std::thread::sleep(PAST_HALF);
  let foreign = [
    stranger
      .fs_write((volume, attachment), "foreign.txt", b"x", 0o644)
      .err(),
    stranger.detach(attachment).err(),
    stranger.bind_mount(attachment, "/nonexistent-mount").err(),
    stranger.advance(attachment, None).err(),
  ];
  #[allow(clippy::disallowed_methods)]
  // past the first term's end, within the term a renewal would have given
  std::thread::sleep(PAST_HALF);
  let owners_use = owner.fs_write((volume, attachment), "late.txt", b"late", 0o644);
  drop((owner, stranger));
  daemon.stop();
  for (verb, refused) in ["fs_write", "detach", "bind_mount", "advance"]
    .iter()
    .zip(&foreign)
  {
    assert!(
      matches!(
        refused,
        Some(ClientError::Refused(Refusal::Forbidden { .. }))
      ),
      "another principal's {verb} of the attachment is refused Forbidden: {refused:?}"
    );
  }
  assert!(
    matches!(
      owners_use,
      Err(ClientError::Refused(Refusal::AttachmentExpired { attachment: named })) if named == attachment
    ),
    "the attachment lapsed at its first term: another principal's uses renewed nothing: {owners_use:?}"
  );
}

/// A one-shard daemon of its own segment, bootstrapped, whose failover delay is held at `ceiling` (A-384: injected, since
/// nothing sets the estimate), and its instance.
fn term_daemon(
  profile: &slates_machine::MachineProfile,
  tag: &str,
  ceiling: Duration,
) -> (Daemon, String) {
  let instance = format!("srv-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(profile, &instance, Some(1));
  let daemon = Daemon::start(
    profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", tag),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  daemon
    .inject_failover(Some(u64::try_from(ceiling.as_nanos()).unwrap()))
    .expect("the failover delay is held");
  (daemon, instance)
}

/// Shape: the failover delay the held-attachment test holds its daemon at: past twice the term a holder renewing at the
/// named cadence settles at (about four thirds of a liveness window), so the holder's own pace shows below it, with room
/// for a wait of two settled terms.
const HELD_CEILING: Duration = Duration::from_secs(4);

/// A-384. Do: on a daemon whose failover delay is held at [`HELD_CEILING`], attach a volume for write in the CLI's form and
/// renew it with `Renew` alone, at the cadence each reply names, for twice the ceiling; then write under it;
/// then stop renewing and, two of the last named terms later (less than the ceiling), write again. Expect every renewal
/// answered, the terms settling below the ceiling (the holder's own pace: its gaps' tail and a liveness window), the
/// first write accepted (nothing but the renewals kept the attachment through two ceilings), and the late write refused
/// `AttachmentExpired`: the attachment lapsed at its holder's pace, before the ceiling would have ended it.
#[test]
fn a_held_attachment_lives_while_its_holder_renews_and_lapses_at_its_own_pace() {
  let profile = common::machine_profile();
  let (daemon, instance) = term_daemon(&profile, "term-held", HELD_CEILING);
  let mut holder = connect(&instance);
  let volume = holder.create(&scratch("held")).unwrap();
  let attachment = holder
    .attach_with(volume, None, Intent::Write, AttachRequest::Termed)
    .unwrap()
    .attachment;
  let began = Instant::now();
  let mut terms = Vec::new();
  let mut refused = None;
  while began.elapsed() < HELD_CEILING.saturating_mul(2) {
    match holder.renew(attachment) {
      Ok((term_ns, renew_every_ns)) => {
        terms.push(term_ns);
        #[allow(clippy::disallowed_methods)] // the holder's pace: the cadence the reply named
        std::thread::sleep(Duration::from_nanos(renew_every_ns));
      }
      Err(error) => {
        refused = Some(error);
        break;
      }
    }
  }
  let written = holder.fs_write((volume, attachment), "held.txt", b"held", 0o644);
  let last_term = terms.last().copied().unwrap_or(u64::MAX);
  #[allow(clippy::disallowed_methods)] // two settled terms with no use
  std::thread::sleep(Duration::from_nanos(last_term.saturating_mul(2)));
  let late = holder.fs_write((volume, attachment), "late.txt", b"late", 0o644);
  drop(holder);
  daemon.stop();
  eprintln!("the terms the renewals named: {terms:?}");
  assert!(refused.is_none(), "every renewal was answered: {refused:?}");
  assert!(
    written.is_ok(),
    "the renewals alone kept the attachment: {written:?}"
  );
  let ceiling_ns = u64::try_from(HELD_CEILING.as_nanos()).unwrap();
  assert!(
    last_term.saturating_mul(2) < ceiling_ns,
    "the term settled at the holder's pace, two of them below the ceiling: {terms:?}"
  );
  assert!(
    matches!(
      late,
      Err(ClientError::Refused(Refusal::AttachmentExpired { attachment: named })) if named == attachment
    ),
    "the attachment lapsed at its holder's pace, before the ceiling: {late:?}"
  );
}

/// A-384, Lifeguard's rule (Dadgar et al. 2018): a daemon never counts its own downtime against a holder. Do: with the
/// test as the anchor, attach a volume for write in the CLI's form on a daemon whose failover delay is held at `TERM`;
/// stop it; wait out two terms; start a second daemon over the same segment and renew the attachment at once, then write
/// under it. Expect the renewal answered and the write accepted: the restart gave the attachment a fresh term. Before,
/// the term that lapsed while no daemon served ended it at the second daemon's first use, refused `AttachmentExpired`.
#[test]
fn a_restart_gives_a_cli_attachment_a_fresh_term() {
  use common::anchor::{held_anchor, source_of};
  let profile = common::machine_profile();
  let instance = format!("srv-term-restart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let anchor = held_anchor("term-restart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&anchor)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  first
    .inject_failover(Some(u64::try_from(TERM.as_nanos()).unwrap()))
    .expect("the failover delay is held");
  let (volume, attachment) = {
    let mut maker = connect(&instance);
    let volume = maker.create(&scratch("termed-restart")).unwrap();
    let attachment = maker
      .attach_with(volume, None, Intent::Write, AttachRequest::Termed)
      .unwrap()
      .attachment;
    (volume, attachment)
  };
  first.stop();
  #[allow(clippy::disallowed_methods)] // no daemon serves for two terms
  std::thread::sleep(TERM.saturating_mul(2));
  let second = Daemon::start(&profile, config, source_of(&anchor)).unwrap();
  let mut user = connect(&instance);
  let renewed = user.renew(attachment);
  let written = user.fs_write((volume, attachment), "after.txt", b"after", 0o644);
  drop(user);
  second.stop();
  assert!(
    renewed.is_ok(),
    "the restart gave the attachment a fresh term: {renewed:?}"
  );
  assert!(
    written.is_ok(),
    "a write under it after the restart: {written:?}"
  );
}
