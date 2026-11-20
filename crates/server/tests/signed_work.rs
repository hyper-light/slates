//! Signed work as a client meets it (A-321; §4.16, §4.15, §4.12 "The consumer contract"; Ada, 2026-11-18: "we can (and
//! should) support signed work"): a merge or a landing may carry an SSH signature over its statement; the daemon judges
//! it under the trusted signers only the launcher sets — a merge's under its work's and its green's, a landing's under
//! its volume's — records the verdict as the act's provenance (a merge's with the version, a landing's on its audit
//! records) and answers it; a volume may require signed merges. A signature never permits anything: the write modes
//! decide (A-285). Driven through real daemons and the wire, a landing into a real directory on the host's disk (on
//! every platform since the Windows writer, A-185); the signing keys are software Ed25519 keys signing as
//! `ssh-keygen -Y sign -n slates-work` signs (`common::signer`).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

mod common;

use slates_anchor::AnchorSegment;
use slates_client::{
  Client, ClientError, PolicySubject, Refusal, SignatureFault, Signer, SignerKey, Submitted,
  Verification, VolumeId, WriteAct, WriteMode, WritePolicy,
};
use slates_ipc::HeldRendezvous;
use slates_server::landing::{merge_approval_proof, policy_proof, signers_proof};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

use common::client::{channel_account, connect};
use common::signer::TestSigner;
// What only the landing tests name: a landing writes a real target on the host's disk.
use common::client::scratch;
use common::landing::GRANT_TERM_NS;
use common::target::{TargetDir, target_dir};
use slates_client::{Filter, GrantScope, Intent, Landing, SignedLanding};
use slates_ipc::protocol::AuditKind;
use slates_server::landing::{enroll_proof, grant_proof};

/// Shape: one shard, so a green and its works share it; `a_signed_merge_off_its_greens_shard_is_judged_there` runs
/// several.
const TEST_SHARDS: u16 = 1;
/// Shape: enough shards that some work lands on another shard than its green (A-247's work route).
const SPREAD_SHARDS: u16 = 4;
/// Format: the launcher secret the restart tests' starter hands its anchor.
const LAUNCHER: [u8; 32] = [9; 32];
/// Format: the permission bits of the file a landing test writes.
const MODE: u32 = 0o644;

/// A daemon of `shards` shards over a fresh segment, its local consensus group made.
fn daemon(tag: &str, shards: u16) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-signed-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(shards));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("signed-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The daemon's counter `name`, summed over its shards.
fn counter(daemon: &Daemon, name: &str) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(name)
    .copied()
    .unwrap_or(0)
}

/// The signer `key` names on the wire: an Ed25519 key and its fingerprint.
fn signer_of(key: &TestSigner) -> Verification {
  Verification::Verified {
    signer: Signer {
      key: SignerKey::Ed25519,
      fingerprint: key.fingerprint(),
    },
  }
}

/// Sets `volume`'s trusted signers to `keys`, as the launcher does under `secret`, from the version it reads.
fn trust(client: &mut Client, secret: &[u8; 32], volume: VolumeId, keys: &[&TestSigner]) {
  let keys: Vec<Vec<u8>> = keys.iter().map(|key| key.public()).collect();
  let (_, version) = client.signers(volume).unwrap();
  client
    .set_signers(
      volume,
      (&keys, version),
      signers_proof(secret, volume, &keys, version),
    )
    .unwrap();
}

/// Sets `volume`'s write policy, as the launcher does under `secret`, from the version it reads.
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

/// The policy that merges as `merge` says, lands as asked, and requires signed merges or not.
fn merging(merge: WriteMode, require_signed_merges: bool) -> WritePolicy {
  WritePolicy {
    merge,
    land: WriteMode::Ask,
    require_signed_merges,
  }
}

/// A work over `green` named `name`, with `bytes` written into `f`.
fn work_with(client: &mut Client, green: VolumeId, name: &str, bytes: &[u8]) -> VolumeId {
  let (work, _) = client.create_work(green, name).unwrap();
  client.edit(work, name, 0, 0, bytes).unwrap();
  work
}

/// `work`'s merge statement now, signed by `key`.
fn signed(client: &mut Client, work: VolumeId, key: &TestSigner) -> Vec<u8> {
  let statement = client.merge_statement(work, &[]).unwrap();
  key.sign(&statement.statement)
}

/// A-321: do trust one key on a green; merge a work signed by it, one signed by a key nobody trusts, one signed over a
/// statement its work has since moved past, and one unsigned; read each version's merge record back, and version 0's.
/// Expect all four merged — a signature permits nothing and refuses nothing here, the green requiring none — answered
/// and recorded: verified naming the trusted key, invalid `UnregisteredKey`, invalid `BadSignature`, and unsigned, each
/// record read back from the green's accepted record though checkpoints folded the versions' chain entries (each work
/// moved past its version; `a_merges_verdict_survives_a_daemon_restart` reads them back from the checkpoint); version 0
/// the origin, unsigned.
#[test]
fn a_signed_merge_is_judged_recorded_and_read_back() {
  let (daemon, instance) = daemon("judged", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let green = client.create_green("judged-green", false).unwrap();
  let trusted = TestSigner::fresh();
  let stranger = TestSigner::fresh();
  trust(&mut client, &secret, green, &[&trusted]);
  expect_a_trusted_merge_verified(&mut client, green, &trusted);
  let second = work_with(&mut client, green, "second", b"signed by a stranger");
  let by_stranger = signed(&mut client, second, &stranger);
  let unregistered = Verification::Invalid {
    reason: SignatureFault::UnregisteredKey,
  };
  assert_eq!(
    client
      .submit_with_signature(second, &[], Some(&by_stranger))
      .unwrap(),
    (Submitted::Accepted(2), unregistered)
  );
  expect_a_stale_and_an_unsigned_merge_recorded(&mut client, green, &trusted);
  assert_eq!(
    client.merge_record(green, 2).unwrap().verification,
    unregistered
  );
  assert_eq!(
    client.merge_record(green, 0).unwrap().verification,
    Verification::Unsigned,
    "the origin, which no merge made"
  );
  assert!(
    matches!(
      client.merge_record(green, 5),
      Err(ClientError::Refused(Refusal::NotFound))
    ),
    "a version not yet committed has no record"
  );
  expect_the_verdicts_were_the_verifiers(&daemon);
  drop(client);
  daemon.stop();
}

/// The trusted merge of `a_signed_merge_is_judged_recorded_and_read_back`. Do: write a work, read its merge statement,
/// sign it with the trusted key and submit it. Expect the statement to read as a person reads it, and the merge
/// accepted at version 1 and verified naming the trusted key, as its record reads back.
fn expect_a_trusted_merge_verified(client: &mut Client, green: VolumeId, trusted: &TestSigner) {
  let first = work_with(client, green, "first", b"signed by the trusted key");
  let statement = client.merge_statement(first, &[]).unwrap();
  let text = String::from_utf8(statement.statement.clone()).unwrap();
  assert!(
    text.starts_with("slates merge v1\nwork "),
    "a person reads what they sign: {text}"
  );
  let signature = trusted.sign(&statement.statement);
  assert_eq!(
    client
      .submit_with_signature(first, &[], Some(&signature))
      .unwrap(),
    (Submitted::Accepted(1), signer_of(trusted))
  );
  assert_eq!(
    client.merge_record(green, 1).unwrap().verification,
    signer_of(trusted)
  );
}

/// Non-vacuity for `a_signed_merge_is_judged_recorded_and_read_back`: checkpoints folded the chain entries that carried
/// the verdicts, and every verdict was the verifier's (one verified, one unregistered key, one bad signature).
fn expect_the_verdicts_were_the_verifiers(daemon: &Daemon) {
  assert!(counter(daemon, "merge.checkpoint.taken") > 0);
  assert_eq!(
    [
      "signed_work.verified",
      "signed_work.invalid.unregistered_key",
      "signed_work.invalid.bad_signature",
    ]
    .map(|name| counter(daemon, name)),
    [1, 1, 1]
  );
}

/// The stale and unsigned merges of `a_signed_merge_is_judged_recorded_and_read_back`. Do: sign a work's statement with
/// the trusted key, edit the work again, and submit it with that signature; then submit another work unsigned. Expect
/// the first merged at 3, its signature `BadSignature` (it named an increment the work moved past), and the second at 4,
/// unsigned, as each record reads back.
fn expect_a_stale_and_an_unsigned_merge_recorded(
  client: &mut Client,
  green: VolumeId,
  trusted: &TestSigner,
) {
  let third = work_with(client, green, "third", b"signed, then changed");
  let stale = signed(client, third, trusted);
  client
    .edit(third, "third", 0, 0, b"after the signature ")
    .unwrap();
  let bad = Verification::Invalid {
    reason: SignatureFault::BadSignature,
  };
  assert_eq!(
    client
      .submit_with_signature(third, &[], Some(&stale))
      .unwrap(),
    (Submitted::Accepted(3), bad)
  );
  assert_eq!(client.merge_record(green, 3).unwrap().verification, bad);
  let fourth = work_with(client, green, "fourth", b"unsigned");
  assert_eq!(
    client.submit_with_signature(fourth, &[], None).unwrap(),
    (Submitted::Accepted(4), Verification::Unsigned)
  );
  assert_eq!(
    client.merge_record(green, 4).unwrap().verification,
    Verification::Unsigned
  );
}

/// A-321: do require signed merges on a bypass green trusting one key; submit a work unsigned, signed by a stranger,
/// signed under git's namespace, and signed by the trusted key; then require them on a work alone, over a green that
/// requires none, and submit it unsigned. Expect the first three refused `SignatureRequired` naming the green and how
/// each signature was judged (unsigned, `UnregisteredKey`, `Namespace`), nothing merged, the statement still given (a
/// held submit is never refused by the requirement); the trusted one merged at 1, verified; and the work's own
/// requirement refusing its unsigned merge, naming the work.
#[test]
fn a_volume_requiring_signed_merges_refuses_an_unsigned_or_invalid_one() {
  let (daemon, instance) = daemon("required", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let green = client.create_green("required-green", false).unwrap();
  let trusted = TestSigner::fresh();
  let stranger = TestSigner::fresh();
  trust(&mut client, &secret, green, &[&trusted]);
  set(
    &mut client,
    &secret,
    green,
    merging(WriteMode::Bypass, true),
  );
  let work = work_with(&mut client, green, "work", b"needs a signature");
  let statement = client.merge_statement(work, &[]).unwrap().statement;
  let attempts = [
    (None, Verification::Unsigned),
    (
      Some(stranger.sign(&statement)),
      Verification::Invalid {
        reason: SignatureFault::UnregisteredKey,
      },
    ),
    (
      Some(trusted.sign_under("git", &statement)),
      Verification::Invalid {
        reason: SignatureFault::Namespace,
      },
    ),
  ];
  for (signature, verification) in attempts {
    let refused = client.submit_with_signature(work, &[], signature.as_deref());
    assert!(
      matches!(
        &refused,
        Err(ClientError::Refused(Refusal::SignatureRequired {
          by: PolicySubject::Volume { volume },
          verification: judged,
        })) if *volume == green && *judged == verification
      ),
      "{refused:?}"
    );
    assert_eq!(client.versions(green).unwrap(), 0, "nothing merged");
  }
  assert_eq!(
    client
      .submit_with_signature(work, &[], Some(&trusted.sign(&statement)))
      .unwrap(),
    (Submitted::Accepted(1), signer_of(&trusted))
  );
  let lax = client.create_green("lax-green", false).unwrap();
  let strict = work_with(&mut client, lax, "strict", b"its own rule");
  set(
    &mut client,
    &secret,
    strict,
    merging(WriteMode::Bypass, true),
  );
  let refused = client.submit(strict);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::SignatureRequired {
        by: PolicySubject::Volume { volume },
        verification: Verification::Unsigned,
      })) if volume == strict
    ),
    "{refused:?}"
  );
  drop(client);
  daemon.stop();
}

/// A-321 with A-285's ask: do require signed merges on an asking green trusting one key; submit a work signed by it,
/// then approve the increment without the signature, then with it. Expect the submit held for approval with its
/// verdict (verified) and nothing merged; the unsigned approval refused `SignatureRequired` when it would merge; the
/// signed one merged at 1 and recorded verified. The signature decided nothing the mode decides: the merge waited for its
/// approval all the same.
#[test]
fn an_asked_signed_merge_carries_its_verdict_to_its_approval() {
  let (daemon, instance) = daemon("asked", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let green = client.create_green("asked-green", false).unwrap();
  let trusted = TestSigner::fresh();
  trust(&mut client, &secret, green, &[&trusted]);
  set(&mut client, &secret, green, merging(WriteMode::Ask, true));
  let work = work_with(&mut client, green, "asked", b"held, then approved");
  let signature = signed(&mut client, work, &trusted);
  let (held, verification) = client
    .submit_with_signature(work, &[], Some(&signature))
    .unwrap();
  let Submitted::ApprovalRequired { identity, .. } = held else {
    panic!("an asked merge is held for approval: {held:?}");
  };
  assert_eq!(verification, signer_of(&trusted));
  assert_eq!(client.versions(green).unwrap(), 0, "nothing merged yet");
  let proof = merge_approval_proof(&secret, work, &identity);
  let unsigned = client.submit_approved_signed(work, &[], (identity, proof), None);
  assert!(
    matches!(
      unsigned,
      Err(ClientError::Refused(Refusal::SignatureRequired {
        verification: Verification::Unsigned,
        ..
      }))
    ),
    "{unsigned:?}"
  );
  assert_eq!(
    client
      .submit_approved_signed(work, &[], (identity, proof), Some(&signature))
      .unwrap(),
    (Submitted::Accepted(1), signer_of(&trusted))
  );
  assert_eq!(
    client.merge_record(green, 1).unwrap().verification,
    signer_of(&trusted)
  );
  drop(client);
  daemon.stop();
}

/// A-321: the launcher sets a volume's trusted signers, once per version. Do: set them with a forged proof, from a
/// version the policy moved past, and naming a key slates does not verify; then set two keys, one named twice; and set a
/// consumer's policy that requires signed merges. Expect `GrantIssuerUnverified`, `PolicyChanged` and `BadRequest`,
/// nothing set; the two keys read back once each in order, the policy version moved; and the consumer's requirement
/// refused `BadRequest`, since it is a volume's rule.
#[test]
fn the_launcher_sets_a_volumes_trusted_signers_once_each() {
  let (daemon, instance) = daemon("signers", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut launcher = connect(&instance);
  let green = launcher.create_green("signers-green", false).unwrap();
  let (one, two) = (TestSigner::fresh(), TestSigner::fresh());
  let keys = vec![one.public()];
  let not_a_key = vec![b"not a key".to_vec()];
  let refusals = [
    launcher.set_signers(green, (&keys, 0), signers_proof(&[0; 32], green, &keys, 0)),
    launcher.set_signers(green, (&keys, 7), signers_proof(&secret, green, &keys, 7)),
    launcher.set_signers(
      green,
      (&not_a_key, 0),
      signers_proof(&secret, green, &not_a_key, 0),
    ),
  ];
  assert!(
    matches!(
      refusals,
      [
        Err(ClientError::Refused(Refusal::GrantIssuerUnverified)),
        Err(ClientError::Refused(Refusal::PolicyChanged { current: 0 })),
        Err(ClientError::Refused(Refusal::BadRequest { .. })),
      ]
    ),
    "{refusals:?}"
  );
  assert_eq!(launcher.signers(green).unwrap(), (Vec::new(), 0));
  let named = vec![one.public(), two.public(), one.public()];
  launcher
    .set_signers(green, (&named, 0), signers_proof(&secret, green, &named, 0))
    .unwrap();
  assert_eq!(
    launcher.signers(green).unwrap(),
    (vec![one.public(), two.public()], 1)
  );
  // Any consumer id names a consumer's policy; the requirement is refused before the id is looked up.
  let subject = PolicySubject::Consumer { consumer: 1 };
  let requiring = merging(WriteMode::Bypass, true);
  let refused = launcher.set_policy(
    subject,
    (requiring, 0),
    policy_proof(&secret, &subject, &requiring, 0),
  );
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::BadRequest { .. }))
    ),
    "{refused:?}"
  );
  drop(launcher);
  daemon.stop();
}

/// A-321, R10: an agent never sets its own volume's trusted signers, as it never sets its policy. Do: from a channel
/// bound to an enrolled consumer, set a green's signers with a genuine proof (the launcher's secret leaked to the
/// agent). Expect `Forbidden`, and nothing set. The consumer is enrolled under the host account the daemon authenticated
/// the agent's channel as (`common::client::channel_account`).
#[test]
fn an_agent_cannot_set_trusted_signers() {
  let (daemon, instance) = daemon("agent-signers", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut launcher = connect(&instance);
  let green = launcher.create_green("agent-green", false).unwrap();
  let mut agent = connect(&instance);
  let account = channel_account(&daemon, agent.client_id());
  let (consumer, capability) = launcher
    .enroll(
      account,
      slates_server::landing::enroll_proof(&secret, account),
    )
    .unwrap();
  agent.attest(consumer, &capability).unwrap();
  let keys = vec![TestSigner::fresh().public()];
  let refused = agent.set_signers(green, (&keys, 0), signers_proof(&secret, green, &keys, 0));
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "{refused:?}"
  );
  assert_eq!(launcher.signers(green).unwrap(), (Vec::new(), 0));
  drop(agent);
  drop(launcher);
  daemon.stop();
}

/// A-321 under A-247: a work whose name hashes to another shard than its green's is judged by the green's owner, which
/// holds no record of it, so the work's trusted signers and its requirement travel with its submit. Do: over several
/// shards, trust a key on works only (none on the green), require signed merges on them, and merge each work signed by
/// that key, then one more unsigned. Expect every signed merge verified naming the key, whichever shard its work is on,
/// and the unsigned one refused `SignatureRequired` naming its work.
#[test]
fn a_signed_merge_off_its_greens_shard_is_judged_there() {
  let (daemon, instance) = daemon("spread", SPREAD_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let green = client.create_green("spread-green", false).unwrap();
  let key = TestSigner::fresh();
  for (index, name) in ["w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7"]
    .iter()
    .enumerate()
  {
    let work = work_with(&mut client, green, name, name.as_bytes());
    trust(&mut client, &secret, work, &[&key]);
    set(&mut client, &secret, work, merging(WriteMode::Bypass, true));
    let signature = signed(&mut client, work, &key);
    let version = u64::try_from(index).unwrap() + 1;
    assert_eq!(
      client
        .submit_with_signature(work, &[], Some(&signature))
        .unwrap(),
      (Submitted::Accepted(version), signer_of(&key)),
      "work {name}"
    );
  }
  let last = work_with(&mut client, green, "unsigned", b"no signature");
  set(&mut client, &secret, last, merging(WriteMode::Bypass, true));
  let refused = client.submit(last);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::SignatureRequired {
        by: PolicySubject::Volume { volume },
        ..
      })) if volume == last
    ),
    "{refused:?}"
  );
  drop(client);
  daemon.stop();
}

/// A-321, provenance is durable on both of its records. Do: with a lagging work holding the green's floor at 0, merge a
/// signed work and an unsigned one, and restart the daemon over the same segment; read both versions' merge records;
/// destroy the lagging work, so a checkpoint folds both versions' chain entries, restart again and read them again.
/// Expect, after each restart, the verdicts as they were judged — the signed version verified naming its key, the other
/// unsigned — never judged again: after the first from the chain entries replayed (no checkpoint was taken), after the
/// second from the checkpoint that folded them; and the signer set kept at its version.
#[test]
fn a_merges_verdict_survives_a_daemon_restart() {
  let (config, segment, rendezvous) = anchored("restart");
  let instance = config.instance.clone();
  let anchor = (&segment, &rendezvous);
  let first = start_over(&config, anchor);
  let mut client = connect(&instance);
  let green = client.create_green("kept-green", false).unwrap();
  let key = TestSigner::fresh();
  trust(&mut client, &LAUNCHER, green, &[&key]);
  let (laggard, _) = client.create_work(green, "laggard").unwrap();
  let signed_work = work_with(&mut client, green, "signed", b"signed before the restart");
  let signature = signed(&mut client, signed_work, &key);
  assert_eq!(
    client
      .submit_with_signature(signed_work, &[], Some(&signature))
      .unwrap(),
    (Submitted::Accepted(1), signer_of(&key))
  );
  let unsigned = work_with(
    &mut client,
    green,
    "unsigned",
    b"unsigned before the restart",
  );
  assert_eq!(client.submit(unsigned).unwrap(), Submitted::Accepted(2));
  assert_eq!(
    counter(&first, "merge.checkpoint.taken"),
    0,
    "the lagging work held every version in the chain"
  );
  drop(client);
  first.stop();
  let second = start_over(&config, anchor);
  let mut client = connect(&instance);
  expect_the_verdicts_kept(&mut client, green, &key, "from the replayed chain entries");
  client.destroy(laggard).unwrap();
  assert!(
    counter(&second, "merge.checkpoint.taken") > 0,
    "the floor rose past both versions, and a checkpoint folded them"
  );
  drop(client);
  second.stop();
  let third = start_over(&config, anchor);
  let mut client = connect(&instance);
  expect_the_verdicts_kept(&mut client, green, &key, "from the checkpoint");
  drop(client);
  third.stop();
}

/// The reads of `a_merges_verdict_survives_a_daemon_restart` after a restart: version 1 verified naming `key`, version
/// 2 unsigned, and the green's signer set at version 1, each read `from` where the test says.
fn expect_the_verdicts_kept(client: &mut Client, green: VolumeId, key: &TestSigner, from: &str) {
  assert_eq!(
    client.merge_record(green, 1).unwrap().verification,
    signer_of(key),
    "version 1, {from}"
  );
  assert_eq!(
    client.merge_record(green, 2).unwrap().verification,
    Verification::Unsigned,
    "version 2, {from}"
  );
  assert_eq!(
    client.signers(green).unwrap(),
    (vec![key.public()], 1),
    "the signer set at its version"
  );
}

/// What a restart test holds as its anchor, named for `tag`: the daemon's configuration, a segment with content and the
/// launcher secret [`LAUNCHER`] published, and the rendezvous each daemon in turn adopts.
fn anchored(tag: &str) -> (DaemonConfig, AnchorSegment, HeldRendezvous) {
  let name = format!("signed-{tag}");
  let profile = common::machine_profile();
  let config = DaemonConfig::derive(
    &profile,
    &format!("{name}-{}", std::process::id()),
    Some(TEST_SHARDS),
  );
  let mut segment = AnchorSegment::create(
    &common::fixture_object("slates-seg", &name),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &common::fixture_object("slates-con", &name),
    config.content_bytes(),
  )
  .unwrap();
  segment.publish_launcher_secret(&LAUNCHER).unwrap();
  let rendezvous = HeldRendezvous::hold(&config.instance).unwrap();
  (config, segment, rendezvous)
}

/// A daemon over the segment and rendezvous the test holds as its anchor, its local consensus group made if it has
/// none, as `launcher_secret.rs` starts one.
fn start_over(
  config: &DaemonConfig,
  (segment, rendezvous): (&AnchorSegment, &HeldRendezvous),
) -> Daemon {
  let (handoff, len) = segment.handoff().unwrap();
  let content = segment.content_handoff().unwrap();
  let daemon = Daemon::start(
    &common::machine_profile(),
    config.clone(),
    SegmentSource::Handoff {
      handoff,
      len,
      content,
      rendezvous: rendezvous.adoption(),
    },
  )
  .unwrap();
  let _ = daemon.bootstrap(true);
  daemon
}

/// A-321: a held merge never merges, so its forbidding mode still refuses it. Do: forbid merges on a green and ask for a
/// work's statement. Expect `PolicyForbids`, naming the merge and the green: a statement is given only for a merge the
/// mode lets through or asks about.
#[test]
fn a_forbidden_merge_has_no_statement() {
  let (daemon, instance) = daemon("forbidden", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let green = client.create_green("forbidden-green", false).unwrap();
  set(
    &mut client,
    &secret,
    green,
    merging(WriteMode::Forbid, false),
  );
  let work = work_with(&mut client, green, "forbidden", b"never merged");
  let refused = client.merge_statement(work, &[]);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::PolicyForbids {
        act: WriteAct::Merge,
        by: PolicySubject::Volume { volume },
      })) if volume == green
    ),
    "{refused:?}"
  );
  drop(client);
  daemon.stop();
}

/// A volume holding `f` = `bytes`: what a landing test lands.
fn volume_with(client: &mut Client, name: &str, bytes: &[u8]) -> VolumeId {
  let volume = client.create(&scratch(name)).unwrap();
  write_f(client, volume, bytes);
  volume
}

/// Writes `f` = `bytes` into `volume`, through an attachment of its own.
fn write_f(client: &mut Client, volume: VolumeId, bytes: &[u8]) {
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "f", bytes, MODE)
    .unwrap();
}

/// The policy that lands as `land` says and merges as asked.
fn landing(land: WriteMode) -> WritePolicy {
  WritePolicy {
    merge: WriteMode::Ask,
    land,
    require_signed_merges: false,
  }
}

/// What `target` holds at `f`, if anything.
fn on_disk(target: &TargetDir) -> Option<Vec<u8>> {
  #[allow(clippy::disallowed_methods)] // reading the landed target back, read-only
  std::fs::read(format!("{}/f", target.path)).ok()
}

/// `volume`'s landing into `target` presented and never landed, whatever its mode, carrying `signature`.
fn presented(
  client: &mut Client,
  volume: VolumeId,
  target: &TargetDir,
  signature: Option<&[u8]>,
) -> SignedLanding {
  client
    .present_with_signature((volume, None), &target.path, Filter::default(), signature)
    .unwrap()
}

/// `volume`'s landing into `target` under its mode, with `grant` if one was issued, carrying `signature`.
fn land_signed(
  client: &mut Client,
  (volume, target): (VolumeId, &TargetDir),
  grant: Option<u64>,
  signature: Option<&[u8]>,
) -> Result<SignedLanding, ClientError> {
  client.land_with_signature(
    (volume, None),
    &target.path,
    Filter::default(),
    grant,
    signature,
  )
}

/// A presentation's id, manifest and statement, from its answer.
fn shown(answer: &SignedLanding) -> (u64, [u8; 32], Vec<u8>) {
  let Landing::GrantRequired {
    landing, manifest, ..
  } = &answer.landing
  else {
    panic!("a presentation: {answer:?}");
  };
  let Some(statement) = answer.statement.clone() else {
    panic!("a presentation states what a signer signs: {answer:?}");
  };
  (*landing, *manifest, statement)
}

/// A landing's verdict, from an answer that says it landed.
fn landed_verdict(answer: &SignedLanding) -> Verification {
  let Landing::Landed(outcome) = &answer.landing else {
    panic!("a landing written: {answer:?}");
  };
  assert_eq!(
    outcome.verification, answer.verification,
    "the outcome carries the verdict the answer reports"
  );
  outcome.verification
}

/// Lowercase hex, as the statement names a volume and a manifest.
fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A-321 (§4.15, §4.12): do trust one key on a volume whose landings ask; present its landing unsigned; sign the
/// statement that presentation states with the trusted key and land with the signature and no grant; land again under
/// the grant the launcher issues for the manifest shown; read the audit. Expect the unsigned presentation answered
/// unsigned with the statement a person reads — the landing's first line, the volume, the target with its length, the
/// manifest shown; the signed landing presented again over the same manifest (the mode asks: a signature permits
/// nothing), verified naming the key, nothing written; the granted landing written and verified; and in the audit the
/// unsigned presentation's one record unsigned, every record of the signed landing as a whole (its presentation, the
/// granted run's plan, its finish) verified, and no other record judging anything. Non-vacuity: the verifier counted
/// the verified judgments, one at the presentation and one at the finish.
#[test]
fn a_signed_landing_is_judged_recorded_and_read_back() {
  let (daemon, instance) = daemon("landed", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = volume_with(&mut client, "landed", b"signed landing");
  let key = TestSigner::fresh();
  trust(&mut client, &secret, volume, &[&key]);
  set(&mut client, &secret, volume, landing(WriteMode::Ask));
  let unsigned = presented(&mut client, volume, &target, None);
  let (first, manifest, statement) =
    expect_the_statement_a_person_reads(&unsigned, volume, &target);
  let signature = key.sign(&statement);
  let asked = land_signed(&mut client, (volume, &target), None, Some(&signature)).unwrap();
  let (second, again, _) = shown(&asked);
  assert_eq!(
    (again, asked.verification),
    (manifest, signer_of(&key)),
    "judged over the manifest shown, and still asked"
  );
  assert_eq!(
    on_disk(&target),
    None,
    "nothing written before the approval"
  );
  let proof = grant_proof(&secret, second, &manifest, GrantScope::Once, GRANT_TERM_NS);
  let grant = client
    .grant(second, manifest, GrantScope::Once, GRANT_TERM_NS, proof)
    .unwrap();
  let landed = land_signed(
    &mut client,
    (volume, &target),
    Some(grant),
    Some(&signature),
  )
  .unwrap();
  assert_eq!(landed_verdict(&landed), signer_of(&key));
  assert_eq!(on_disk(&target).as_deref(), Some(&b"signed landing"[..]));
  expect_the_landings_audited(&mut client, (first, second), &key);
  assert_eq!(counter(&daemon, "signed_work.verified"), 2);
  drop(client);
  daemon.stop();
}

/// The unsigned presentation of `a_signed_landing_is_judged_recorded_and_read_back`. Expect it unsigned, and its
/// statement exactly the landing's — its first line, the volume, the target as the request named it with its length,
/// the manifest shown, one field a line. Returns its id, manifest and statement.
fn expect_the_statement_a_person_reads(
  unsigned: &SignedLanding,
  volume: VolumeId,
  target: &TargetDir,
) -> (u64, [u8; 32], Vec<u8>) {
  assert_eq!(unsigned.verification, Verification::Unsigned);
  let (landing, manifest, statement) = shown(unsigned);
  assert_eq!(
    String::from_utf8(statement.clone()).unwrap(),
    format!(
      "slates landing v1\nvolume {}\ntarget {}:{}\nmanifest {}\n",
      hex(&volume.bytes),
      target.path.len(),
      target.path,
      hex(&manifest)
    )
  );
  (landing, manifest, statement)
}

/// The audit of `a_signed_landing_is_judged_recorded_and_read_back`. Expect the unsigned presentation `first` judged on
/// its one record, unsigned; every record of the signed landing `second` that judges verified naming `key`, a planned
/// and a finished one among them; and no record judging anything but a landing's planned and finished ones.
fn expect_the_landings_audited(client: &mut Client, (first, second): (u64, u64), key: &TestSigner) {
  let records = client.audit(0).unwrap();
  let judged = |landing: u64| {
    records
      .iter()
      .filter(|record| record.landing == Some(landing))
      .filter_map(|record| Some((record.audit_kind, record.verification?)))
      .collect::<Vec<_>>()
  };
  assert_eq!(
    judged(first),
    vec![(AuditKind::LandingPlanned, Verification::Unsigned)],
    "{records:?}"
  );
  let signed = judged(second);
  assert!(
    signed.iter().all(|(_, verdict)| *verdict == signer_of(key)),
    "{records:?}"
  );
  for kind in [AuditKind::LandingPlanned, AuditKind::LandingFinished] {
    assert!(
      signed.iter().any(|(judged, _)| *judged == kind),
      "{kind:?}: {records:?}"
    );
  }
  assert!(
    records
      .iter()
      .filter(|record| record.verification.is_some())
      .all(|record| matches!(
        record.audit_kind,
        AuditKind::LandingPlanned | AuditKind::LandingFinished
      )),
    "{records:?}"
  );
}

/// A bypass volume `name` holding `f` = `bytes` and trusting `trusted`, a fresh target, and the statement a presentation
/// of the landing states: one landing of `a_landings_signature_decides_nothing_its_mode_decides`.
fn bypassed(
  client: &mut Client,
  secret: &[u8; 32],
  (name, bytes): (&str, &[u8]),
  trusted: &TestSigner,
) -> (VolumeId, TargetDir, Vec<u8>) {
  let volume = volume_with(client, name, bytes);
  trust(client, secret, volume, &[trusted]);
  set(client, secret, volume, landing(WriteMode::Bypass));
  let target = target_dir();
  let (_, _, statement) = shown(&presented(client, volume, &target, None));
  (volume, target, statement)
}

/// A-321 under Ada's rule of 2026-11-18: a landing's signature is provenance, never a permission, and its verdict is the
/// verifier's whatever it is. Do: on volumes that land at once (bypass) trusting one key, land one signed by a key
/// nobody trusts, one signed by the trusted key under git's namespace, and one signed by the trusted key over a
/// statement its volume has since moved past (`f` changed after the presentation that stated it), each into its own
/// target; then forbid a fourth's landings and land it with a signature that verifies. Expect the first three written
/// at once and answered invalid — `UnregisteredKey`, `Namespace`, `BadSignature` — each counted once at the bypass
/// presentation and once at the finish; and the forbidden one refused `PolicyForbids` though its signature verifies,
/// never judged, nothing written.
#[test]
fn a_landings_signature_decides_nothing_its_mode_decides() {
  let (daemon, instance) = daemon("undecided", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let (trusted, stranger) = (TestSigner::fresh(), TestSigner::fresh());
  expect_landed_invalid(
    &mut client,
    (&secret, &trusted),
    ("stranger", &|statement| stranger.sign(statement)),
    SignatureFault::UnregisteredKey,
  );
  expect_landed_invalid(
    &mut client,
    (&secret, &trusted),
    ("namespace", &|statement| {
      trusted.sign_under("git", statement)
    }),
    SignatureFault::Namespace,
  );
  expect_a_stale_signature_landed_bad(&mut client, &secret, &trusted);
  expect_a_verified_signature_forbidden(&mut client, &secret, &trusted);
  assert_eq!(
    [
      "signed_work.invalid.unregistered_key",
      "signed_work.invalid.namespace",
      "signed_work.invalid.bad_signature",
      "signed_work.verified",
    ]
    .map(|name| counter(&daemon, name)),
    [2, 2, 2, 0]
  );
  drop(client);
  daemon.stop();
}

/// A signer by name: its name and the function that signs bytes with it.
type NamedSigner<'a> = (&'a str, &'a dyn Fn(&[u8]) -> Vec<u8>);

/// One invalid landing of `a_landings_signature_decides_nothing_its_mode_decides`. Do: land a bypass volume `name`
/// trusting `trusted`, signed by `sign` over the statement its presentation stated. Expect it written at once and
/// answered invalid for `fault`: the verdict is provenance and refuses nothing.
fn expect_landed_invalid(
  client: &mut Client,
  (secret, trusted): (&[u8; 32], &TestSigner),
  (name, sign): NamedSigner<'_>,
  fault: SignatureFault,
) {
  let (volume, target, statement) = bypassed(client, secret, (name, name.as_bytes()), trusted);
  let landed = land_signed(client, (volume, &target), None, Some(&sign(&statement))).unwrap();
  assert_eq!(
    landed_verdict(&landed),
    Verification::Invalid { reason: fault },
    "{name}"
  );
  assert_eq!(on_disk(&target).as_deref(), Some(name.as_bytes()), "{name}");
}

/// The stale landing of `a_landings_signature_decides_nothing_its_mode_decides`. Do: sign the statement of a bypass
/// volume's landing with the trusted key, change `f`, and land with that signature. Expect it written with `f` as it
/// is now, and answered `BadSignature`: the statement it signed named a manifest the volume moved past.
fn expect_a_stale_signature_landed_bad(
  client: &mut Client,
  secret: &[u8; 32],
  trusted: &TestSigner,
) {
  let (volume, target, statement) =
    bypassed(client, secret, ("stale", b"before the signature"), trusted);
  let signature = trusted.sign(&statement);
  write_f(client, volume, b"after the signature");
  let landed = land_signed(client, (volume, &target), None, Some(&signature)).unwrap();
  assert_eq!(
    landed_verdict(&landed),
    Verification::Invalid {
      reason: SignatureFault::BadSignature
    }
  );
  assert_eq!(
    on_disk(&target).as_deref(),
    Some(&b"after the signature"[..])
  );
}

/// The forbidden landing of `a_landings_signature_decides_nothing_its_mode_decides`. Do: sign a bypass volume's landing
/// statement with the trusted key, forbid the volume's landings, and land with the signature. Expect `PolicyForbids`
/// naming the landing and the volume, and nothing written: a forbidden landing is refused signed or not.
fn expect_a_verified_signature_forbidden(
  client: &mut Client,
  secret: &[u8; 32],
  trusted: &TestSigner,
) {
  let (volume, target, statement) =
    bypassed(client, secret, ("forbidden", b"never landed"), trusted);
  set(client, secret, volume, landing(WriteMode::Forbid));
  let refused = land_signed(
    client,
    (volume, &target),
    None,
    Some(&trusted.sign(&statement)),
  );
  assert!(
    matches!(
      &refused,
      Err(ClientError::Refused(Refusal::PolicyForbids {
        act: WriteAct::Land,
        by: PolicySubject::Volume { volume: by },
      })) if *by == volume
    ),
    "{refused:?}"
  );
  assert_eq!(on_disk(&target), None);
}

/// An agent's channel bound to a consumer whose own policy asks for its landings (and forbids its merges), enrolled and
/// set by `launcher` under `secret`, as `write_policy.rs` binds one. The consumer is enrolled under the host account
/// `daemon` authenticated the agent's channel as.
fn asking_agent(
  daemon: &Daemon,
  launcher: &mut Client,
  secret: &[u8; 32],
  instance: &str,
) -> Client {
  let mut agent = connect(instance);
  let account = channel_account(daemon, agent.client_id());
  let (consumer, capability) = launcher
    .enroll(account, enroll_proof(secret, account))
    .unwrap();
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
      policy_proof(secret, &subject, &asked, 0),
    )
    .unwrap();
  agent.attest(consumer, &capability).unwrap();
  agent
}

/// A-321 under A-285: an agent's channel whose consumer policy asks turns its landing into a presentation, and a signed
/// landing keeps its signature through that turn. Do: as an agent whose consumer asks, make a volume the launcher sets
/// to bypass and trusting one key; present its landing for the statement and land it signed by that key. Expect the
/// landing presented, nothing written (the consumer's ask governs the volume's bypass), and verified naming the key.
#[test]
fn an_agents_asked_landing_keeps_its_signature() {
  let (daemon, instance) = daemon("agent-landing", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut launcher = connect(&instance);
  let mut agent = asking_agent(&daemon, &mut launcher, &secret, &instance);
  let volume = volume_with(&mut agent, "agent-landing", b"the agent's signed work");
  let key = TestSigner::fresh();
  trust(&mut launcher, &secret, volume, &[&key]);
  set(&mut launcher, &secret, volume, landing(WriteMode::Bypass));
  let (_, manifest, statement) = shown(&presented(&mut agent, volume, &target, None));
  let asked = land_signed(
    &mut agent,
    (volume, &target),
    None,
    Some(&key.sign(&statement)),
  )
  .unwrap();
  let (_, shown_again, _) = shown(&asked);
  assert_eq!(
    (shown_again, asked.verification),
    (manifest, signer_of(&key)),
    "presented with its verdict, as an approver sees it"
  );
  assert_eq!(on_disk(&target), None, "the consumer's ask held it");
  drop(agent);
  drop(launcher);
  daemon.stop();
}

/// A-321, a landing's provenance is durable. Do: over a segment the test holds as its anchor, land two bypass volumes at
/// once, one signed by its trusted key and one unsigned; restart the daemon over the same segment and read the audit.
/// Expect each finished landing's records as they were judged — verified naming the key, and unsigned — read back from
/// the recovered log, and the restarted daemon's verifier never asked (its counters at zero): provenance is how the
/// landing was judged when it landed, never judged again.
#[test]
fn a_landings_verdict_survives_a_daemon_restart() {
  let (config, segment, rendezvous) = anchored("landing-restart");
  let anchor = (&segment, &rendezvous);
  let first = start_over(&config, anchor);
  let mut client = connect(&config.instance);
  let key = TestSigner::fresh();
  let signed = land_at_once(&mut client, ("kept-signed", Some(&key)), &key);
  let unsigned = land_at_once(&mut client, ("kept-unsigned", None), &key);
  assert_eq!(
    (signed.1, unsigned.1),
    (signer_of(&key), Verification::Unsigned),
    "as judged when they landed"
  );
  drop(client);
  first.stop();
  let second = start_over(&config, anchor);
  let mut client = connect(&config.instance);
  let records = client.audit(0).unwrap();
  for (landing, verdict, _) in [&signed, &unsigned] {
    let finished = records.iter().find(|record| {
      record.landing == Some(*landing) && record.audit_kind == AuditKind::LandingFinished
    });
    assert_eq!(
      finished.and_then(|record| record.verification),
      Some(*verdict),
      "landing {landing}, read back after the restart: {records:?}"
    );
  }
  assert_eq!(
    counter(&second, "signed_work.verified"),
    0,
    "nothing judged again"
  );
  drop(client);
  second.stop();
}

/// One landing of `a_landings_verdict_survives_a_daemon_restart`: a bypass volume `name` trusting `trusted`, landed at
/// once, signed by `signer` over its statement or unsigned. Its landing id, its verdict, and its target, kept for the
/// restart that rebuilds the volume over it.
fn land_at_once(
  client: &mut Client,
  (name, signer): (&str, Option<&TestSigner>),
  trusted: &TestSigner,
) -> (u64, Verification, TargetDir) {
  let (volume, target, statement) = bypassed(client, &LAUNCHER, (name, name.as_bytes()), trusted);
  let signature = signer.map(|key| key.sign(&statement));
  let answer = land_signed(client, (volume, &target), None, signature.as_deref()).unwrap();
  let Landing::Landed(outcome) = &answer.landing else {
    panic!("{name} lands at once: {answer:?}");
  };
  (outcome.landing, answer.verification, target)
}
