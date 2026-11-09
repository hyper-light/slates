//! A CLI attachment's term (§4.7, A-230): it outlives the client that made it, is renewed by every verb that names it,
//! ends at its term's lapse with the write lease it held, and is refused `AttachmentExpired` when named after, never
//! `NotFound`. A short failover SLO makes the term a few seconds.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use slates_client::{AttachRequest, ClientError, Intent, Refusal};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};

/// Shape: the failover SLO, so the CLI attachment's term: past the reaper's silence and sweep (two liveness budgets),
/// so a term's survival of its client's end is seen, and short enough to wait out.
const TERM: Duration = Duration::from_secs(3);
/// Shape: the pause after which a renewal is due (more than half the term gone) and the term is not yet over.
const PAST_HALF: Duration = Duration::from_millis(2_000);
/// Shape: how long the lapse may take to be seen: the term, and one sweep of the reaper's cadence (a liveness budget).
const LAPSE_WAIT: Duration = Duration::from_secs(5);

/// A-230. Do: on a daemon whose failover SLO is `TERM`, attach a volume for write in the CLI's form and end that client;
/// from a second client, write a file under the attachment past half its term, then again past the first term's end
/// (the first write renewed it); then wait out a whole term with no use and name the attachment once more, by a write
/// and by a detach; then attach for write again. Expect: both writes succeed; once the term lapsed both uses are
/// refused `AttachmentExpired` naming the attachment, never `NotFound`; and the new write attachment is granted, the
/// lapsed one having let its write lease go.
#[test]
fn a_cli_attachment_lives_its_term_renewed_by_use_and_is_refused_as_expired_after() {
  let profile = common::machine_profile();
  let instance = format!("srv-term-{}", std::process::id());
  let term_ns = u64::try_from(TERM.as_nanos()).unwrap();
  let config = DaemonConfig::derive(&profile, &instance, Some(1)).with_failover_slo(term_ns);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "term"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
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
  let instance = format!("srv-term-foreign-{}", std::process::id());
  let term_ns = u64::try_from(TERM.as_nanos()).unwrap();
  let config = DaemonConfig::derive(&profile, &instance, Some(1)).with_failover_slo(term_ns);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "term-foreign"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
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
