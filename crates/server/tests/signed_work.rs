//! Signed merges as a client meets them (A-321; §4.16, §4.12 "The consumer contract"; Ada, 2026-11-18: "we can (and
//! should) support signed work"): a merge may carry an SSH signature over its statement; the daemon judges it under the
//! work's and the green's trusted signers, which only the launcher sets, records the verdict with the version as its
//! provenance and answers it; a volume may require signed merges. A signature never permits anything: the write modes
//! decide (A-285). Driven through real daemons and the wire; the signing keys are software Ed25519 keys signing as
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

use common::client::connect;
use common::signer::TestSigner;

/// Shape: one shard, so a green and its works share it; `a_signed_merge_off_its_greens_shard_is_judged_there` runs
/// several.
const TEST_SHARDS: u16 = 1;
/// Shape: enough shards that some work lands on another shard than its green (A-247's work route).
const SPREAD_SHARDS: u16 = 4;
/// Format: the launcher secret the restart test's starter hands its anchor.
const LAUNCHER: [u8; 32] = [9; 32];

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
/// agent). Expect `Forbidden`, and nothing set. Unix: the consumer is enrolled under the channel's uid (`consumers.rs`
/// covers enrollment on every platform).
#[cfg(unix)]
#[test]
fn an_agent_cannot_set_trusted_signers() {
  let (daemon, instance) = daemon("agent-signers", TEST_SHARDS);
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut launcher = connect(&instance);
  let green = launcher.create_green("agent-green", false).unwrap();
  let account = rustix::process::getuid().as_raw();
  let (consumer, capability) = launcher
    .enroll(
      account,
      slates_server::landing::enroll_proof(&secret, account),
    )
    .unwrap();
  let mut agent = connect(&instance);
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
  let instance = format!("signed-restart-{}", std::process::id());
  let profile = common::machine_profile();
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let mut segment = AnchorSegment::create(
    &common::fixture_object("slates-seg", "signed-restart"),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &common::fixture_object("slates-con", "signed-restart"),
    config.content_bytes(),
  )
  .unwrap();
  segment.publish_launcher_secret(&LAUNCHER).unwrap();
  let anchor = (&segment, &HeldRendezvous::hold(&config.instance).unwrap());
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
