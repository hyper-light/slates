//! The launcher's authority across a daemon restart (R10, A-285): a launcher-class proof verifies under the launcher
//! secret its starter handed the anchor, as under the issuer secret the daemon mints at each start, so a launcher that
//! outlives a daemon (kiln, under its own restarts too) keeps its authority. Driven through real daemons under an anchor
//! the test plays, as `seal.rs` plays one.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::unwrap_in_result
)]

mod common;

use slates_anchor::AnchorSegment;
use slates_client::{ClientError, PolicySubject, Refusal, WriteMode, WritePolicy};
use slates_ipc::HeldRendezvous;
use slates_server::landing::policy_proof;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

use common::client::{connect, scratch};

/// Shape: one shard: the volume and its policy live on it.
const SHARDS: u16 = 1;
/// Format: the launcher secret the test's starter hands the anchor.
const LAUNCHER: [u8; 32] = [7; 32];

/// The anchor the test plays: its segment, holding the launcher secret, and its rendezvous.
struct PlayedAnchor {
  segment: AnchorSegment,
  rendezvous: HeldRendezvous,
}

/// A daemon over `anchor`'s handoff, its local consensus group made if it has none.
fn start_over(config: &DaemonConfig, anchor: &PlayedAnchor) -> Daemon {
  let (handoff, len) = anchor.segment.handoff().unwrap();
  let content = anchor.segment.content_handoff().unwrap();
  let daemon = Daemon::start(
    &common::machine_profile(),
    config.clone(),
    SegmentSource::Handoff {
      handoff,
      len,
      content,
      rendezvous: anchor.rendezvous.adoption(),
    },
  )
  .unwrap();
  // The first start makes the group; a restart recovers it, and its bootstrap is refused as already made.
  let _ = daemon.bootstrap(true);
  daemon
}

/// The policy that lands as `land` says and merges at once.
fn landing(land: WriteMode) -> WritePolicy {
  WritePolicy {
    merge: WriteMode::Bypass,
    land,
    require_signed_merges: false,
  }
}

/// A-285: do hand the anchor a launcher secret, set a volume's policy under it, restart the daemon (a new issuer secret),
/// then set the policy under the first daemon's issuer secret and under the launcher secret. Expect the first setting
/// and the last accepted, the stale issuer's proof refused `GrantIssuerUnverified`, and the policy the last one set.
#[test]
fn a_launcher_proof_verifies_across_a_daemon_restart() {
  let instance = format!("launcher-{}", std::process::id());
  let profile = common::machine_profile();
  let config = DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  let mut segment = AnchorSegment::create(
    &common::fixture_object("slates-seg", "launcher"),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &common::fixture_object("slates-con", "launcher"),
    config.content_bytes(),
  )
  .unwrap();
  segment.publish_launcher_secret(&LAUNCHER).unwrap();
  let anchor = PlayedAnchor {
    segment,
    rendezvous: HeldRendezvous::hold(&config.instance).unwrap(),
  };
  let first = start_over(&config, &anchor);
  let first_issuer = first.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let volume = client.create(&scratch("launched")).unwrap();
  let subject = PolicySubject::Volume { volume };
  let ask = landing(WriteMode::Ask);
  client
    .set_policy(
      subject,
      (ask, 0),
      policy_proof(&LAUNCHER, &subject, &ask, 0),
    )
    .unwrap();
  drop(client);
  first.stop();
  let second = start_over(&config, &anchor);
  let mut client = connect(&instance);
  let bypass = landing(WriteMode::Bypass);
  let stale = client.set_policy(
    subject,
    (bypass, 1),
    policy_proof(&first_issuer, &subject, &bypass, 1),
  );
  assert!(
    matches!(
      stale,
      Err(ClientError::Refused(Refusal::GrantIssuerUnverified))
    ),
    "the first daemon's issuer secret proves nothing to the second: {stale:?}"
  );
  client
    .set_policy(
      subject,
      (bypass, 1),
      policy_proof(&LAUNCHER, &subject, &bypass, 1),
    )
    .unwrap();
  assert_eq!(client.policy(subject).unwrap(), (Some(bypass), 2));
  drop(client);
  second.stop();
}
