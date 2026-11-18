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
use slates_server::landing::{enroll_proof, grant_proof, merge_approval_proof, policy_proof};
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
    require_signed_merges: false,
  }
}

/// Sets `volume`'s policy as the launcher does, under the daemon's issuer secret, from the version it reads.
fn set(client: &mut Client, secret: &[u8; 32], volume: VolumeId, policy: WritePolicy) {
  let subject = PolicySubject::Volume { volume };
  let (_, version) = client.policy(subject).unwrap();
  client
    .set_policy(
      subject,
      (policy, version),
      policy_proof(secret, &subject, &policy, version),
    )
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
    (Some(landing(WriteMode::Bypass)), 1)
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
    (None, 0)
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
  let raised = agent.set_policy(
    subject,
    (bypass, 0),
    policy_proof(&secret, &subject, &bypass, 0),
  );
  assert!(
    matches!(raised, Err(ClientError::Refused(Refusal::Forbidden { .. }))),
    "a consumer-bound channel sets no policy, even with a genuine proof: {raised:?}"
  );
  let forged = launcher.set_policy(
    subject,
    (bypass, 0),
    policy_proof(&[0; 32], &subject, &bypass, 0),
  );
  assert!(
    matches!(
      forged,
      Err(ClientError::Refused(Refusal::GrantIssuerUnverified))
    ),
    "a forged proof: {forged:?}"
  );
  assert_eq!(launcher.policy(subject).unwrap(), (None, 0));
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

/// A work over a fresh green with one file written: (green, work).
fn work_with_an_edit(client: &mut Client, name: &str) -> (VolumeId, VolumeId) {
  let green = client
    .create_green(&format!("{name}-green"), false)
    .unwrap();
  let (work, _) = client.create_work(green, name).unwrap();
  client.edit(work, "f", 0, 0, b"merged work").unwrap();
  (green, work)
}

/// The policy that merges as `merge` says and lands as asked.
fn merging(merge: WriteMode) -> WritePolicy {
  WritePolicy {
    merge,
    land: WriteMode::Ask,
    require_signed_merges: false,
  }
}

/// A-285 merges: do submit a work with no policy set, then a work whose policy asks, approving first another
/// increment and then the one asked, and a work whose policy forbids. Expect the first accepted at once (as every merge
/// was before policies); the asked one answered with its identity and one path, nothing merged until the approval of
/// exactly that increment (another is refused `GrantMismatch`), then accepted; the forbidden one refused
/// `PolicyForbids` naming the act and the volume.
#[test]
fn a_merge_follows_its_volumes_write_policy() {
  let (daemon, instance) = daemon("merge");
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let (_, unset) = work_with_an_edit(&mut client, "unset");
  assert!(matches!(
    client.submit(unset),
    Ok(slates_client::Submitted::Accepted(_))
  ));
  let (green, asked) = work_with_an_edit(&mut client, "asked");
  set(&mut client, &secret, asked, merging(WriteMode::Ask));
  let Ok(slates_client::Submitted::ApprovalRequired { identity, paths }) = client.submit(asked)
  else {
    panic!("an asked merge answers an approval request");
  };
  assert_eq!(paths, 1);
  assert_eq!(client.versions(green).unwrap(), 0, "nothing merged yet");
  let mut other = identity;
  other[0] ^= 1;
  let proof = merge_approval_proof(&secret, asked, &other);
  let refused = client.submit_approved(asked, &[], other, proof);
  assert!(
    matches!(refused, Err(ClientError::Refused(Refusal::GrantMismatch))),
    "an approval of another increment: {refused:?}"
  );
  let proof = merge_approval_proof(&secret, asked, &identity);
  assert!(matches!(
    client.submit_approved(asked, &[], identity, proof),
    Ok(slates_client::Submitted::Accepted(1))
  ));
  expect_a_forbidden_merge_refused(&mut client, &secret);
  drop(client);
  daemon.stop();
}

/// The forbidden merge of `a_merge_follows_its_volumes_write_policy`. Do: submit a work whose policy, set with
/// `secret`'s authority, forbids merges. Expect it refused `PolicyForbids`, naming the merge and the work volume.
fn expect_a_forbidden_merge_refused(client: &mut Client, secret: &[u8; 32]) {
  let (_, forbidden) = work_with_an_edit(client, "forbidden");
  set(client, secret, forbidden, merging(WriteMode::Forbid));
  let refused = client.submit(forbidden);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::PolicyForbids {
        act: WriteAct::Merge,
        by: PolicySubject::Volume { volume },
      })) if volume == forbidden
    ),
    "{refused:?}"
  );
}

/// A-285: an agent never approves its own merge. Do: from a channel bound to an enrolled consumer, approve an asked
/// merge with a genuine proof. Expect `Forbidden`, and nothing merged.
#[test]
fn an_agent_cannot_approve_its_own_merge() {
  let (daemon, instance) = daemon("approve");
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut launcher = connect(&instance);
  let (green, work) = work_with_an_edit(&mut launcher, "held");
  set(&mut launcher, &secret, work, merging(WriteMode::Ask));
  let Ok(slates_client::Submitted::ApprovalRequired { identity, .. }) = launcher.submit(work)
  else {
    panic!("an asked merge answers an approval request");
  };
  let account = rustix::process::getuid().as_raw();
  let (consumer, capability) = launcher
    .enroll(account, enroll_proof(&secret, account))
    .unwrap();
  let mut agent = connect(&instance);
  agent.attest(consumer, &capability).unwrap();
  let approved = agent.submit_approved(
    work,
    &[],
    identity,
    merge_approval_proof(&secret, work, &identity),
  );
  assert!(
    matches!(
      approved,
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "{approved:?}"
  );
  assert_eq!(launcher.versions(green).unwrap(), 0);
  drop(agent);
  drop(launcher);
  daemon.stop();
}

/// A-285: an agent never approves its own landing, as it never approves its own merge. Do: from a channel bound to an
/// enrolled consumer, land the agent's own volume, whose policy asks, then grant that presented landing with a genuine
/// proof (the launcher's secret leaked to the agent). Expect the approval request, the grant refused `Forbidden`, and
/// nothing written: the landing's approval is the launcher's or the harness's, never the agent's own channel's.
#[test]
fn an_agent_cannot_approve_its_own_landing() {
  let (daemon, instance) = daemon("approve-landing");
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut launcher = connect(&instance);
  let account = rustix::process::getuid().as_raw();
  let (consumer, capability) = launcher
    .enroll(account, enroll_proof(&secret, account))
    .unwrap();
  let mut agent = connect(&instance);
  agent.attest(consumer, &capability).unwrap();
  let volume = volume_with(&mut agent, "self-approved", b"agent's work");
  set(&mut launcher, &secret, volume, landing(WriteMode::Ask));
  let Ok(Landing::GrantRequired {
    landing: presented,
    manifest,
    ..
  }) = agent.land(volume, None, &target.path, Filter::default(), None)
  else {
    panic!("an ask landing answers an approval request");
  };
  let proof = grant_proof(
    &secret,
    presented,
    &manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let granted = agent.grant(presented, manifest, GrantScope::Once, GRANT_TERM_NS, proof);
  assert!(
    matches!(
      granted,
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "a consumer-bound channel approves no landing, even with a genuine proof: {granted:?}"
  );
  assert_eq!(on_disk(&target), None);
  drop(agent);
  drop(launcher);
  daemon.stop();
}

/// A-285: a present-only landing never writes, whatever the policy. Do: present a bypass volume's landing and a
/// forbidden volume's. Expect each answered with its plan (an approval request), nothing on disk, and the bypass
/// volume's ordinary `land` afterwards written at once.
#[test]
fn a_presentation_writes_nothing_whatever_the_policy() {
  let (daemon, instance) = daemon("present");
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut client = connect(&instance);
  let bypassed = volume_with(&mut client, "presented-bypass", b"reviewed first");
  set(&mut client, &secret, bypassed, landing(WriteMode::Bypass));
  let forbidden = volume_with(&mut client, "presented-forbid", b"never written");
  set(&mut client, &secret, forbidden, landing(WriteMode::Forbid));
  for volume in [bypassed, forbidden] {
    let presented = client.present(volume, None, &target.path, Filter::default());
    assert!(
      matches!(presented, Ok(Landing::GrantRequired { .. })),
      "{presented:?}"
    );
    assert_eq!(on_disk(&target), None, "a presentation writes nothing");
  }
  let landed = client.land(bypassed, None, &target.path, Filter::default(), None);
  assert!(matches!(landed, Ok(Landing::Landed(_))), "{landed:?}");
  assert_eq!(on_disk(&target).as_deref(), Some(&b"reviewed first"[..]));
  drop(client);
  daemon.stop();
}

/// A-285: a launcher's proof sets a policy once. Do: set a volume's landing mode to bypass from version 0, then lower it
/// to forbid from version 1, then replay the first setting (the captured bypass proof for version 0). Expect the replay
/// refused `PolicyChanged { current: 2 }` and the policy still forbid: a captured proof cannot raise a lowered policy.
#[test]
fn a_replayed_policy_proof_cannot_raise_a_lowered_policy() {
  let (daemon, instance) = daemon("replay");
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let volume = volume_with(&mut client, "lowered", b"agent's work");
  let subject = PolicySubject::Volume { volume };
  let bypass = landing(WriteMode::Bypass);
  let captured = policy_proof(&secret, &subject, &bypass, 0);
  client.set_policy(subject, (bypass, 0), captured).unwrap();
  set(&mut client, &secret, volume, landing(WriteMode::Forbid));
  let replayed = client.set_policy(subject, (bypass, 0), captured);
  assert!(
    matches!(
      replayed,
      Err(ClientError::Refused(Refusal::PolicyChanged { current: 2 }))
    ),
    "{replayed:?}"
  );
  assert_eq!(
    client.policy(subject).unwrap(),
    (Some(landing(WriteMode::Forbid)), 2)
  );
  drop(client);
  daemon.stop();
}

/// A-285, a session's own policy: the stricter of the consumer's and the volume's governs, and a policy set after the
/// agent bound reaches its channel at once. Do: bind an agent's channel to an enrolled consumer, which makes its own
/// volume; then, as the launcher, set that volume's landing mode to bypass and the consumer's landing mode to ask and
/// its merge mode to forbid;
/// the agent lands the volume and submits a work; then the launcher lowers the consumer's landing mode to forbid and the
/// agent lands again. Expect the consumer's policy read back at version 1; the first landing answered with an approval
/// request (ask over the volume's bypass) and nothing written; the submit refused `PolicyForbids` by the consumer; the
/// second landing refused `PolicyForbids` by the consumer.
#[test]
fn a_consumers_own_policy_holds_its_agent_to_the_stricter_mode() {
  let (daemon, instance) = daemon("consumer");
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut launcher = connect(&instance);
  let account = rustix::process::getuid().as_raw();
  let (consumer, capability) = launcher
    .enroll(account, enroll_proof(&secret, account))
    .unwrap();
  let mut agent = connect(&instance);
  agent.attest(consumer, &capability).unwrap();
  // The agent's own volume and work, as an agent works; the launcher sets the policies.
  let volume = volume_with(&mut agent, "session-held", b"agent's work");
  set(&mut launcher, &secret, volume, landing(WriteMode::Bypass));
  let subject = PolicySubject::Consumer { consumer };
  let asked = WritePolicy {
    merge: WriteMode::Forbid,
    land: WriteMode::Ask,
    require_signed_merges: false,
  };
  launcher
    .set_policy(
      subject,
      (asked, 0),
      policy_proof(&secret, &subject, &asked, 0),
    )
    .unwrap();
  assert_eq!(launcher.policy(subject).unwrap(), (Some(asked), 1));
  let presented = agent.land(volume, None, &target.path, Filter::default(), None);
  assert!(
    matches!(presented, Ok(Landing::GrantRequired { .. })),
    "the consumer's ask holds a bypass volume's landing: {presented:?}"
  );
  assert_eq!(on_disk(&target), None);
  expect_the_consumers_forbid_refuses_its_merge(&mut agent, consumer);
  let forbidden = WritePolicy {
    merge: WriteMode::Forbid,
    land: WriteMode::Forbid,
    require_signed_merges: false,
  };
  launcher
    .set_policy(
      subject,
      (forbidden, 1),
      policy_proof(&secret, &subject, &forbidden, 1),
    )
    .unwrap();
  let refused = agent.land(volume, None, &target.path, Filter::default(), None);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::PolicyForbids {
        act: WriteAct::Land,
        by: PolicySubject::Consumer { .. },
      }))
    ),
    "a lowered policy reaches the bound channel at once: {refused:?}"
  );
  assert_eq!(on_disk(&target), None);
  drop(agent);
  drop(launcher);
  daemon.stop();
}

/// The merge of `a_consumers_own_policy_holds_its_agent_to_the_stricter_mode`. Do: as `agent`, bound to `consumer`
/// whose own policy forbids merges, make a green and a work, edit the work and submit it. Expect the submit refused
/// `PolicyForbids`, naming the merge and the consumer.
fn expect_the_consumers_forbid_refuses_its_merge(agent: &mut Client, consumer: u64) {
  let green = agent.create_green("session-green", false).unwrap();
  let (work, _) = agent.create_work(green, "session-work").unwrap();
  agent.edit(work, "f", 0, 0, b"merged work").unwrap();
  let merged = agent.submit(work);
  assert!(
    matches!(
      merged,
      Err(ClientError::Refused(Refusal::PolicyForbids {
        act: WriteAct::Merge,
        by: PolicySubject::Consumer { consumer: by },
      })) if by == consumer
    ),
    "{merged:?}"
  );
}
