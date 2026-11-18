//! A volume's write policy as a client meets it (R10, D-22, A-285): landing follows the mode the user gave the agent.
//! Bypass lands at once; ask answers an approval request and lands once an approval binds its manifest; forbid
//! refuses; only the launcher's issuer sets a policy, and an agent's consumer-bound channel never can.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]
// A real landing target on the host's disk: macOS and Linux.
#![cfg(unix)]

use slates_client::{
  Client, ClientError, Filter, Landing, PolicySubject, Refusal, VolumeId, WriteAct, WriteMode,
  WritePolicy,
};
use slates_ipc::protocol::{GrantScope, Intent};
use slates_server::landing::{enroll_proof, grant_proof, policy_proof};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::landing::GRANT_TERM_NS;
use common::target::{TargetDir, target_dir};

/// Shape: one shard, so the volume, its presentations and its grants meet on one shard.
const TEST_SHARDS: u16 = 1;

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-policy-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("policy-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// A volume holding `f` = `bytes`.
fn volume_with(client: &mut Client, name: &str, bytes: &[u8]) -> VolumeId {
  let volume = client.create(&scratch(name)).unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "f", bytes, 0o644)
    .unwrap();
  volume
}

/// The policy that lands as `land` says and merges as asked.
fn landing(land: WriteMode) -> WritePolicy {
  WritePolicy {
    merge: WriteMode::Ask,
    land,
  }
}

/// Sets `volume`'s policy as the launcher does, under the daemon's issuer secret.
fn set(client: &mut Client, secret: &[u8; 32], volume: VolumeId, policy: WritePolicy) {
  let subject = PolicySubject::Volume { volume };
  client
    .set_policy(subject, policy, policy_proof(secret, &subject, &policy))
    .unwrap();
}

/// What the target holds at `f`, if anything.
fn on_disk(target: &TargetDir) -> Option<Vec<u8>> {
  #[allow(clippy::disallowed_methods)]
  std::fs::read(format!("{}/f", target.path)).ok()
}

/// A-285 bypass: do set a volume's landing mode to bypass and land it, with no grant and no approval. Expect the
/// landing written in that one call, `f` on disk, and the policy read back as set.
#[test]
fn a_bypass_volume_lands_at_once_with_no_approval() {
  let (daemon, instance) = daemon("bypass");
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = volume_with(&mut client, "bypassed", b"agent's work");
  set(&mut client, &secret, volume, landing(WriteMode::Bypass));
  assert_eq!(
    client.policy(PolicySubject::Volume { volume }).unwrap(),
    Some(landing(WriteMode::Bypass))
  );
  let landed = client.land(volume, None, &target.path, Filter::default(), None);
  assert!(
    matches!(landed, Ok(Landing::Landed(_))),
    "a bypass landing is written at once: {landed:?}"
  );
  assert_eq!(on_disk(&target).as_deref(), Some(&b"agent's work"[..]));
  drop(client);
  daemon.stop();
}

/// A-285 ask: do land a volume with no policy set (ask), then explicitly ask; approve a manifest other than the one
/// presented, then the presented one, and land under the grant. Expect the approval request with nothing written, the
/// other manifest refused `GrantMismatch`, and the presented one landed.
#[test]
fn an_ask_volume_lands_only_after_an_approval_of_its_manifest() {
  let (daemon, instance) = daemon("ask");
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = volume_with(&mut client, "asked", b"needs approval");
  assert_eq!(
    client.policy(PolicySubject::Volume { volume }).unwrap(),
    None
  );
  set(&mut client, &secret, volume, landing(WriteMode::Ask));
  let Ok(Landing::GrantRequired {
    landing: presented,
    manifest,
    ..
  }) = client.land(volume, None, &target.path, Filter::default(), None)
  else {
    panic!("an ask landing answers an approval request");
  };
  assert_eq!(
    on_disk(&target),
    None,
    "nothing is written before the approval"
  );
  let mut other = manifest;
  other[0] ^= 1;
  let proof = grant_proof(&secret, presented, &other, GrantScope::Once, GRANT_TERM_NS);
  let refused = client.grant(presented, other, GrantScope::Once, GRANT_TERM_NS, proof);
  assert!(
    matches!(refused, Err(ClientError::Refused(Refusal::GrantMismatch))),
    "an approval of another manifest: {refused:?}"
  );
  let proof = grant_proof(
    &secret,
    presented,
    &manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let grant = client
    .grant(presented, manifest, GrantScope::Once, GRANT_TERM_NS, proof)
    .unwrap();
  let landed = client.land(volume, None, &target.path, Filter::default(), Some(grant));
  assert!(matches!(landed, Ok(Landing::Landed(_))), "{landed:?}");
  assert_eq!(on_disk(&target).as_deref(), Some(&b"needs approval"[..]));
  drop(client);
  daemon.stop();
}

/// A-285 forbid: do set a volume's landing mode to forbid and land it. Expect `PolicyForbids`, naming the act and the
/// volume, and nothing written.
#[test]
fn a_forbid_volume_refuses_its_landing() {
  let (daemon, instance) = daemon("forbid");
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = volume_with(&mut client, "forbidden", b"never written");
  set(&mut client, &secret, volume, landing(WriteMode::Forbid));
  let refused = client.land(volume, None, &target.path, Filter::default(), None);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::PolicyForbids {
        act: WriteAct::Land,
        by: PolicySubject::Volume { volume: by },
      })) if by == volume
    ),
    "{refused:?}"
  );
  assert_eq!(on_disk(&target), None);
  drop(client);
  daemon.stop();
}

/// A-285: an agent can never raise its own policy. Do: from a channel bound to an enrolled consumer, set the volume's
/// policy to bypass with a genuine proof; from an unbound channel, set it with a forged proof. Expect `Forbidden` and
/// `GrantIssuerUnverified`, the policy still unset, and a landing still answered with an approval request.
#[test]
fn an_agent_cannot_raise_its_own_policy() {
  let (daemon, instance) = daemon("raise");
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut launcher = connect(&instance);
  let volume = volume_with(&mut launcher, "guarded", b"agent's work");
  let account = rustix::process::getuid().as_raw();
  let (consumer, capability) = launcher
    .enroll(account, enroll_proof(&secret, account))
    .unwrap();
  let mut agent = connect(&instance);
  agent.attest(consumer, &capability).unwrap();
  let subject = PolicySubject::Volume { volume };
  let bypass = landing(WriteMode::Bypass);
  let raised = agent.set_policy(subject, bypass, policy_proof(&secret, &subject, &bypass));
  assert!(
    matches!(raised, Err(ClientError::Refused(Refusal::Forbidden { .. }))),
    "a consumer-bound channel sets no policy, even with a genuine proof: {raised:?}"
  );
  let forged = launcher.set_policy(subject, bypass, policy_proof(&[0; 32], &subject, &bypass));
  assert!(
    matches!(
      forged,
      Err(ClientError::Refused(Refusal::GrantIssuerUnverified))
    ),
    "a forged proof: {forged:?}"
  );
  assert_eq!(launcher.policy(subject).unwrap(), None);
  let presented = launcher.land(volume, None, &target.path, Filter::default(), None);
  assert!(
    matches!(presented, Ok(Landing::GrantRequired { .. })),
    "{presented:?}"
  );
  assert_eq!(on_disk(&target), None);
  drop(agent);
  drop(launcher);
  daemon.stop();
}
