//! Tests of the version-control credentials (A-625 part 3): the ids against OpenSSH's and FIPS 180-2's own vectors, the
//! public key line as `ssh-keygen` writes it, the human surface's verbs through the daemon's dispatch on a real shard,
//! and the lookup the edges ask from another shard.

use slates_db::HostId;
use slates_db::catalog::{CredentialKind, CredentialRecord, Principal};
use slates_ipc::protocol::{CredentialMaterial, Refusal, ReplyBody, RequestBody};

use super::*;

/// The key slates-ssh's userauth tests read: an RSA key `ssh-keygen` wrote (OpenSSH 10.2p1), its `.pub` line.
const RSA_LINE: &str = include_str!("../../../ssh/fixtures/userauth/user_rsa.pub");
/// Format: `ssh-keygen -l -E sha256` on that key (slates-ssh's `the_fingerprint_is_openssh_s`): the SHA-256 of its blob
/// in base64 without its padding.
const RSA_FINGERPRINT: &str = "Wymbjagh5qjwNh2R9f44I8A1HSB7TrTf6LdcnH26LQ8";
/// Format: FIPS 180-2 appendix B.1: the SHA-256 of "abc".
const ABC_SHA256: [u8; 32] = [
  0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23,
  0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
];
/// Shape: an instant on the wall clock, in nanoseconds since the Unix epoch (2026-11-19).
const EXPIRES_NS: u64 = 1_795_000_000_000_000_000;
/// Shape: how long the test thread waits for a lookup asked on a shard: the daemon's liveness budget, the most a
/// lookup's call waits.
const ANSWER_WAIT_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
/// Shape: the tokens the two-shard lookup enrolls, enough that each partition holds some.
const TOKENS: u8 = 8;

/// The blob of the key `ssh-keygen` wrote.
fn rsa_blob() -> Vec<u8> {
  key_blob_of_line(RSA_LINE).expect("ssh-keygen's line reads")
}

/// The channel of the human at the terminal: an account of the daemon's host.
fn human() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: 501,
  }
}

/// An agent's channel: a consumer enrolled under that account.
fn agent() -> Principal {
  Principal::Consumer {
    host: HostId(0),
    account: 501,
    consumer: 9,
  }
}

/// The principal the tests' credentials prove, as the wire names it.
fn proven_on_wire() -> slates_ipc::protocol::Principal {
  slates_ipc::protocol::Principal::Uid { host: 7, uid: 502 }
}

/// That principal, as the catalog names it.
fn proven() -> Principal {
  Principal::Uid {
    host: HostId(7),
    uid: 502,
  }
}

/// An enrollment of `material` until `expires_ns`, proven under this daemon's issuer secret.
fn enrollment(
  state: &ShardState,
  material: CredentialMaterial,
  expires_ns: Option<u64>,
) -> RequestBody {
  let principal = proven_on_wire();
  let proof = enroll_proof(&state.issuer_secret, &principal, &material, expires_ns);
  RequestBody::EnrollCredential {
    principal,
    material,
    expires_ns,
    proof,
  }
}

/// A revocation of `id`, proven under this daemon's issuer secret.
fn revocation(state: &ShardState, id: [u8; 32]) -> RequestBody {
  RequestBody::RevokeCredential {
    id,
    proof: revoke_proof(&state.issuer_secret, &id),
  }
}

/// `body` from `channel`, through the daemon's dispatch.
fn dispatched(state: &mut ShardState, channel: &Principal, body: RequestBody) -> ReplyBody {
  crate::verbs::dispatch(state, 0, channel, body)
}

/// The credential this shard's partition holds under `id`.
fn held(state: &ShardState, id: &[u8; 32]) -> Option<CredentialRecord> {
  state.db.partition().credentials().get(id).cloned()
}

/// What `kind` counted on this shard.
fn counted(state: &ShardState, kind: &str) -> u64 {
  state.refusals.get(kind).copied().unwrap_or(0)
}

/// A refusal's reply.
fn refusal(reply: ReplyBody) -> Option<Refusal> {
  match reply {
    ReplyBody::Refused { refusal } => Some(refusal),
    _ => None,
  }
}

/// A-625 part 3 golden: do take the ids of the key `ssh-keygen` wrote and of the token "abc". Expect the key's id to be
/// the fingerprint OpenSSH printed for it and the token's the SHA-256 FIPS 180-2 gives for "abc", whichever way the
/// request names them.
#[test]
fn a_credentials_id_is_the_sha256_openssh_and_fips_180_name() {
  let blob = rsa_blob();
  let printed = crate::signed_work::base64_decode(&format!("{RSA_FINGERPRINT}="))
    .expect("the fingerprint is base64");
  assert_eq!(ssh_key_id(&blob).as_slice(), printed.as_slice());
  assert_eq!(token_id(b"abc"), ABC_SHA256);
  assert_eq!(
    id_of(&CredentialMaterial::SshKey { blob: blob.clone() }),
    ssh_key_id(&blob)
  );
  assert_eq!(
    id_of(&CredentialMaterial::HttpToken { id: ABC_SHA256 }),
    ABC_SHA256
  );
}

/// A-625 part 3. Do: read the key line `ssh-keygen` wrote, the same with no comment, its key under another type, its
/// key field cut by a character, and a line of one field. Expect: the blob, its first string `ssh-rsa`, for the first
/// two; then `Type`, `Base64` and `Fields`.
#[test]
fn a_public_key_line_is_read_as_ssh_keygen_writes_it() {
  let blob = rsa_blob();
  assert_eq!(blob.get(4..11), Some(b"ssh-rsa".as_slice()));
  let fields: Vec<&str> = RSA_LINE.split_whitespace().collect();
  let encoded = fields.get(1).copied().expect("a key field");
  assert_eq!(
    key_blob_of_line(&format!("ssh-rsa {encoded}")),
    Ok(blob.clone())
  );
  assert_eq!(
    key_blob_of_line(&format!("ssh-ed25519 {encoded}")),
    Err(KeyLineRefusal::Type)
  );
  let cut = encoded.get(..encoded.len() - 1).expect("a shorter field");
  assert_eq!(
    key_blob_of_line(&format!("ssh-rsa {cut}")),
    Err(KeyLineRefusal::Base64)
  );
  assert_eq!(key_blob_of_line("ssh-rsa"), Err(KeyLineRefusal::Fields));
}

/// A-625 part 3 (§4.13: a human enrolls, an agent never). Do, through the daemon's dispatch on a real shard: enroll the
/// key `ssh-keygen` wrote with the launcher's proof; again; then from an agent's channel with a proof that verifies,
/// and from the human's with a proof made for another expiry; revoke it from the agent's channel, then the human's,
/// then again. Expect: `CredentialEnrolled` naming the key's fingerprint, its record proving the principal asked for as
/// an `ssh-rsa` key; `AlreadyExists`; `Forbidden` and `GrantIssuerUnverified`, each counted, with the record as it was;
/// `Forbidden` again; `CredentialRevoked` with the record gone; then `NotFound`.
#[test]
fn a_human_enrolls_and_revokes_a_key_and_an_agent_cannot() {
  crate::daemon::audit_on_shard(|state| {
    let blob = rsa_blob();
    let id = ssh_key_id(&blob);
    let key = CredentialMaterial::SshKey { blob: blob.clone() };
    let asked = enrollment(state, key.clone(), None);
    assert_eq!(
      dispatched(state, &human(), asked.clone()),
      ReplyBody::CredentialEnrolled { id }
    );
    let record = CredentialRecord {
      id,
      principal: proven(),
      kind: CredentialKind::SshKey {
        algorithm: "ssh-rsa".to_owned(),
        blob,
      },
      expires_ns: None,
    };
    assert_eq!(held(state, &id), Some(record.clone()));
    assert!(matches!(
      refusal(dispatched(state, &human(), asked.clone())),
      Some(Refusal::AlreadyExists { .. })
    ));
    refused_to_others(state, key, asked);
    assert_eq!(held(state, &id), Some(record));
    assert_eq!(
      refusal(dispatched(state, &agent(), revocation(state, id))),
      Some(Refusal::Forbidden {
        verb: "revoke_credential".to_owned()
      })
    );
    assert_eq!(
      dispatched(state, &human(), revocation(state, id)),
      ReplyBody::CredentialRevoked
    );
    assert_eq!(held(state, &id), None);
    assert_eq!(
      refusal(dispatched(state, &human(), revocation(state, id))),
      Some(Refusal::NotFound)
    );
  });
}

/// The two refusals of an enrollment of `key` an agent's channel and a proof for another expiry meet, each counted.
fn refused_to_others(state: &mut ShardState, key: CredentialMaterial, asked: RequestBody) {
  assert_eq!(
    refusal(dispatched(state, &agent(), asked)),
    Some(Refusal::Forbidden {
      verb: "enroll_credential".to_owned()
    })
  );
  assert_eq!(counted(state, BY_CONSUMER), 1);
  let RequestBody::EnrollCredential { proof, .. } = enrollment(state, key.clone(), None) else {
    panic!("an enrollment is an EnrollCredential request");
  };
  let retimed = RequestBody::EnrollCredential {
    principal: proven_on_wire(),
    material: key,
    expires_ns: Some(EXPIRES_NS),
    proof,
  };
  let unverified = counted(state, ISSUER_UNVERIFIED);
  assert_eq!(
    refusal(dispatched(state, &human(), retimed)),
    Some(Refusal::GrantIssuerUnverified)
  );
  assert_eq!(counted(state, ISSUER_UNVERIFIED), unverified + 1);
}

/// A-625 part 3. Do, through the daemon's dispatch: enroll a token by its hash with an expiry; enroll a blob whose type
/// no SSH edge algorithm takes, and an Ed25519 blob of 31 key bytes. Expect: the token's record, its kind `HttpToken`
/// and its expiry, and neither the token nor anything else of it held; each key refused `BadRequest`, nothing recorded.
#[test]
fn a_token_is_held_as_its_hash_and_a_key_the_edge_cannot_take_is_refused() {
  crate::daemon::audit_on_shard(|state| {
    let id = token_id(b"slates_0123");
    let token = CredentialMaterial::HttpToken { id };
    let asked = enrollment(state, token, Some(EXPIRES_NS));
    assert_eq!(
      dispatched(state, &human(), asked),
      ReplyBody::CredentialEnrolled { id }
    );
    assert_eq!(
      held(state, &id),
      Some(CredentialRecord {
        id,
        principal: proven(),
        kind: CredentialKind::HttpToken,
        expires_ns: Some(EXPIRES_NS),
      })
    );
    for blob in [ssh_blob(b"ssh-dss", 32), ssh_blob(b"ssh-ed25519", 31)] {
      let id = ssh_key_id(&blob);
      let asked = enrollment(state, CredentialMaterial::SshKey { blob }, None);
      assert!(matches!(
        refusal(dispatched(state, &human(), asked)),
        Some(Refusal::BadRequest { .. })
      ));
      assert_eq!(held(state, &id), None);
    }
  });
}

/// A key blob of type `kind` with one key string of `len` bytes (RFC 4253 §6.6's strings).
fn ssh_blob(kind: &[u8], len: usize) -> Vec<u8> {
  let key = vec![7u8; len];
  let mut blob = Vec::new();
  for field in [kind, key.as_slice()] {
    blob.extend_from_slice(&u32::try_from(field.len()).unwrap_or(0).to_be_bytes());
    blob.extend_from_slice(field);
  }
  blob
}

/// A-625 part 3 (D-14: ids route to owners). Do: on a two-shard daemon, enroll tokens each on the shard its id routes
/// to, then from the first shard look each up, and an id never enrolled. Expect: every token found with its principal
/// and expiry, those of both partitions (so a lookup crossed shards), and none for the stranger.
#[test]
fn a_credential_is_found_from_any_shard_on_the_partition_its_id_routes_to() {
  let daemon = crate::daemon::audit_daemon_of(2);
  let ids: Vec<[u8; 32]> = (0..TOKENS).map(|serial| token_id(&[serial])).collect();
  for id in &ids {
    let partition = usize::from(owner_of(id, 2));
    let id = *id;
    let reply = crate::daemon::observe_at(&daemon, partition, move |state| {
      let asked = enrollment(
        state,
        CredentialMaterial::HttpToken { id },
        Some(EXPIRES_NS),
      );
      dispatched(state, &human(), asked)
    });
    assert_eq!(reply, ReplyBody::CredentialEnrolled { id });
  }
  let partitions: std::collections::BTreeSet<u16> = ids.iter().map(|id| owner_of(id, 2)).collect();
  assert_eq!(partitions.len(), 2, "the tokens reach both partitions");
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  let asked = ids.clone();
  crate::daemon::observe_first(&daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let mut found = Vec::new();
      for id in asked {
        found.push(find(id).await);
      }
      let stranger = find(token_id(b"never enrolled")).await;
      let _ = sent.send((found, stranger));
    })
    .expect("the lookup's task spawns");
    slates_rt::futures::detach(task).expect("the lookup's task detaches");
  });
  let answer = answered.recv_timeout(std::time::Duration::from_nanos(ANSWER_WAIT_NS));
  daemon.stop();
  let (found, stranger) = answer.expect("the lookups answered");
  for (id, found) in ids.iter().zip(found) {
    let record = found
      .expect("the partition answered")
      .expect("the token is held");
    assert_eq!(
      (record.id, record.principal, record.expires_ns),
      (*id, proven(), Some(EXPIRES_NS))
    );
  }
  assert_eq!(stranger, Ok(None));
}
