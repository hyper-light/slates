//! Version-control credentials (§4.13 "Principals", §4.17.2 "Access"; A-625 part 3): the human surface's verbs that
//! enroll and revoke them, the partition each lives on, and the lookups the HTTP and SSH edges and `svn://`'s CRAM-MD5
//! authenticate through.
//!
//! **One record for both edges.** A credential is an SSH user key, held by its blob and named by the blob's SHA-256
//! (OpenSSH's fingerprint, [`ssh_key_id`]), or a token, held as its SHA-256 ([`token_id`]) and its CRAM-MD5 keyed
//! contexts ([`cram_md5_contexts`], A-692). The issuer mints a token from the system's generator
//! ([`TOKEN_RANDOM_BYTES`]), shows it to the human once ([`token_text`]) and sends the daemon only its hash and its
//! contexts, so a token never crosses a channel and the daemon never holds one. Each record names the
//! §4.13 principal it proves and when it stops proving it (`slates_db::catalog::CredentialRecord`), and lives on the
//! partition its id routes to ([`owner_of`]), so the HTTP edge's authenticator and the SSH edge's key holders find a
//! presented credential with one lookup there ([`find`]), with no global index (D-14).
//!
//! **`svn://`'s CRAM-MD5** (A-692; §4.17.4 item 10, RFC 2195). A token is also a Subversion client's password over
//! `svn://`, whose CRAM-MD5 sends no password but a keyed digest of the server's challenge, so the server must hold
//! something that makes that digest. It holds HMAC-MD5's keyed contexts, MD5's state after the token's two pad blocks
//! (RFC 2104 §4; RFC 2195 §4: storing them avoids "explicit cleartext storage of the shared secret"), which the issuer
//! derives as it mints the token and the record keeps beside its hash: they answer a challenge as the token does, and
//! give no token back, so no HTTP request can be made from them. The client names its token by the token's id, as the
//! CLI prints it, since CRAM-MD5 finds a secret by its user's name ([`cram_md5_key`]).
//!
//! **Only a human enrolls** (§4.13: enrollments and grants are the human surface's, never MCP's or an SDK's).
//! `EnrollCredential` and `RevokeCredential` carry the launcher's proof, a keyed hash under the issuer secret of exactly
//! what is enrolled or revoked ([`enroll_proof`], [`revoke_proof`], checked by `crate::landing::proven_by_launcher`),
//! and a consumer's channel is refused `Forbidden` before anything else, whatever proof it carries (A-285). MCP and the
//! SDKs hold no issuer secret, so no agent enrolls or revokes a credential, its own or anyone's.
//!
//! **Keys the edge takes.** An SSH key is enrolled only when the SSH edge's parser takes it (`slates_ssh::userauth`:
//! `ssh-ed25519`, and RSA from 2048 to 8192 bits), so a human learns at enrollment, not at a refused login, that a key
//! cannot serve. The record keeps the key type the blob names, so a later algorithm joins with no change of record.
//!
//! **What an edge decides.** A credential proves its principal while it is held and live on the wall clock
//! (`CredentialRecord::live_at`): expiry is decided at authentication, never in the database's `apply`, where the clock
//! would replay differently. A revocation removes the record, so the next lookup finds nothing. A credential that proves
//! an enrolled consumer proves it only while the consumer is enrolled and not revoked (§4.13: "every later effect
//! refuses"), which both edges ask of the consumer's own partition ([`consumer_revoked`]).

use slates_anchor::layout::ISSUER_SECRET_BYTES;
use slates_db::Op;
use slates_db::catalog::{CredentialKind, CredentialRecord, Principal};
use slates_ipc::protocol::{CredentialMaterial, Refusal, ReplyBody};
use slates_ssh::userauth::{KeyRefusal, SignatureAlgorithm, UserKey};
use slates_svn::handshake::{CramContexts, CramKey};
use slates_vfs::clock::Clock;

use crate::error::refusal_of_db;
use crate::state::ShardState;
use crate::verbs::{forbidden, refused, shard_of_partition, to_db_principal};

/// Format: the domain tags that keep the issuer secret's uses apart: an enrollment's proof never verifies as a
/// revocation's, a consumer's enrollment's or a grant's, whatever follows the tag; neither tag is a prefix of another.
const ENROLL_DOMAIN: &[u8] = b"slates.credential-enroll";
/// Format: see `ENROLL_DOMAIN`.
const REVOKE_DOMAIN: &[u8] = b"slates.credential-revoke";

/// Format: the prefix of a token the issuer mints, so a secret scanner can name one.
pub const TOKEN_PREFIX: &str = "slates_";
/// Format: a minted token's random bytes: 256 bits, the strength of the SHA-256 that names it.
pub const TOKEN_RANDOM_BYTES: usize = 32;
/// Format: the hex digits one byte is written in.
const HEX_DIGITS_PER_BYTE: usize = 2;
/// Format: the bits one hex digit holds.
const HEX_DIGIT_BITS: u32 = 4;
/// Format: the value of the hex letter `a`.
const HEX_LETTER_BASE: u8 = 10;
/// Derived: a minted token's text's bytes: the prefix, then two hex digits a random byte.
const TOKEN_TEXT_BYTES: usize = TOKEN_PREFIX.len() + HEX_DIGITS_PER_BYTE * TOKEN_RANDOM_BYTES;

/// Counter: an enrollment or a revocation of a credential sent on a consumer's channel, refused `Forbidden` (A-285).
const BY_CONSUMER: &str = "credential_by_consumer";
/// Counter: an enrollment or a revocation whose proof of issuer authority did not verify.
const ISSUER_UNVERIFIED: &str = "grant_issuer_unverified";

/// The SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> [u8; 32] {
  let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
  <[u8; 32]>::try_from(hash.as_ref()).unwrap_or_default()
}

/// An SSH key's id: the SHA-256 of its blob, OpenSSH's fingerprint (`ssh-keygen -l -E sha256`), so the id a human lists
/// is the one `ssh-keygen` prints.
pub fn ssh_key_id(blob: &[u8]) -> [u8; 32] {
  sha256(blob)
}

/// An SSH key's fingerprint as `ssh-keygen -l -E sha256` prints it (`SHA256:` and the blob's SHA-256 in base64 without
/// padding): how the CLI names a key's id to a human.
pub fn key_fingerprint(blob: &[u8]) -> String {
  slates_ssh::userauth::fingerprint(blob)
}

/// An HTTP token's id: the SHA-256 of the token's bytes as a client presents them.
pub fn token_id(token: &[u8]) -> [u8; 32] {
  sha256(token)
}

/// A token's CRAM-MD5 keyed contexts (the module doc; A-692): HMAC-MD5's MD5 states after its pad blocks, in Dovecot's
/// layout, what the issuer sends beside the token's id and the record keeps.
pub fn cram_md5_contexts(token: &[u8]) -> [u8; CramContexts::BYTES] {
  CramContexts::of_secret(token).to_bytes()
}

/// The id of the credential `material` gives.
pub fn id_of(material: &CredentialMaterial) -> [u8; 32] {
  match material {
    CredentialMaterial::SshKey { blob } => ssh_key_id(blob),
    CredentialMaterial::HttpToken { id, .. } => *id,
  }
}

/// The partition a credential lives on: its id's first eight bytes over the daemon's `partitions`. An id is a SHA-256,
/// so credentials spread evenly; the same on every start, so a recovered partition holds what routes to it.
pub fn owner_of(id: &[u8; 32], partitions: usize) -> u16 {
  let word = u64::from_le_bytes(
    id.first_chunk::<{ size_of::<u64>() }>()
      .copied()
      .unwrap_or_default(),
  );
  let count = u64::try_from(partitions.max(1)).unwrap_or(u64::MAX);
  u16::try_from(word.checked_rem(count).unwrap_or(0)).unwrap_or(u16::MAX)
}

/// The proof of issuer authority an `EnrollCredential` request carries: the keyed hash, under the issuer secret, of the
/// enrollment's domain tag and the wire encodings of the principal, the credential and its expiry, so a proof enrolls
/// exactly that credential for exactly that principal until exactly then.
pub fn enroll_proof(
  secret: &[u8; ISSUER_SECRET_BYTES],
  principal: &slates_ipc::protocol::Principal,
  material: &CredentialMaterial,
  expires_ns: Option<u64>,
) -> [u8; 32] {
  let mut bytes = Vec::new();
  slates_wire::Wire::encode(principal, &mut bytes);
  slates_wire::Wire::encode(material, &mut bytes);
  slates_wire::Wire::encode(&expires_ns, &mut bytes);
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(ENROLL_DOMAIN);
  hasher.update(&bytes);
  *hasher.finalize().as_bytes()
}

/// The proof of issuer authority a `RevokeCredential` request carries: the keyed hash, under the issuer secret, of the
/// revocation's domain tag and the credential's id.
pub fn revoke_proof(secret: &[u8; ISSUER_SECRET_BYTES], id: &[u8; 32]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(REVOKE_DOMAIN);
  hasher.update(id);
  *hasher.finalize().as_bytes()
}

/// A minted token's text: [`TOKEN_PREFIX`] and its random bytes in lowercase hex, every character one Basic's password
/// and a bearer token both carry (RFC 7617 §2, RFC 6750 §2.1).
pub fn token_text(random: &[u8; TOKEN_RANDOM_BYTES]) -> String {
  let mut text = String::with_capacity(TOKEN_TEXT_BYTES);
  text.push_str(TOKEN_PREFIX);
  for byte in random {
    text.push_str(&format!("{byte:02x}"));
  }
  text
}

/// Why a public key line was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyLineRefusal {
  /// The line is not `TYPE BASE64 [COMMENT]`.
  Fields,
  /// The key's field is not base64.
  Base64,
  /// The blob does not name the type the line does, or names none.
  Type,
}

impl std::fmt::Display for KeyLineRefusal {
  /// The refusal as the CLI says it.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(match self {
      KeyLineRefusal::Fields => {
        "a public key line is TYPE BASE64 [COMMENT], as ssh-keygen writes it"
      }
      KeyLineRefusal::Base64 => "the key's field is not base64",
      KeyLineRefusal::Type => "the key's blob does not name the type its line names",
    })
  }
}

/// The blob of an OpenSSH public key line, as `ssh-keygen` writes a `.pub` file (`TYPE BASE64 [COMMENT]`, sshd(8)
/// "AUTHORIZED_KEYS FILE FORMAT" without options): the key's field decoded, its first string the line's type.
pub fn key_blob_of_line(line: &str) -> Result<Vec<u8>, KeyLineRefusal> {
  let mut fields = line.split_whitespace();
  let (Some(kind), Some(encoded)) = (fields.next(), fields.next()) else {
    return Err(KeyLineRefusal::Fields);
  };
  let blob = crate::signed_work::base64_decode(encoded).map_err(|_| KeyLineRefusal::Base64)?;
  let mut reader = slates_ssh::wire::Reader::new(&blob);
  match reader.string() {
    Ok(named) if named == kind.as_bytes() => Ok(blob),
    _ => Err(KeyLineRefusal::Type),
  }
}

/// The key type of the key in `blob`, as the SSH edge takes keys (the module doc): the type of the first accepted
/// algorithm that parses it; refused with the parser's reason when none does.
fn accepted_key(blob: &[u8]) -> Result<&'static str, String> {
  let mut refusal = None;
  for algorithm in SignatureAlgorithm::ACCEPTED {
    match UserKey::parse(algorithm, blob) {
      Ok(_) => return Ok(algorithm.key_type()),
      Err(KeyRefusal::Type) => {}
      Err(other) => refusal = Some(other),
    }
  }
  let mut types: Vec<&str> = SignatureAlgorithm::ACCEPTED
    .iter()
    .map(|algorithm| algorithm.key_type())
    .collect();
  types.dedup();
  Err(match refusal {
    Some(refusal) => format!("the SSH key does not read as its type's key: {refusal:?}"),
    None => format!("the SSH edge takes {} keys alone", types.join(" and ")),
  })
}

/// What a credential is held as, from what the request gives: a key as the type its blob names, after the SSH edge's
/// parser took it; a token as its hash.
fn kind_of(material: CredentialMaterial) -> Result<CredentialKind, String> {
  match material {
    CredentialMaterial::SshKey { blob } => Ok(CredentialKind::SshKey {
      algorithm: accepted_key(&blob)?.to_owned(),
      blob,
    }),
    CredentialMaterial::HttpToken {
      cram_md5_contexts, ..
    } => Ok(CredentialKind::HttpToken { cram_md5_contexts }),
  }
}

/// What an `EnrollCredential` request asks.
pub(crate) struct Enrollment {
  /// The principal the credential proves.
  pub(crate) principal: slates_ipc::protocol::Principal,
  /// The credential.
  pub(crate) material: CredentialMaterial,
  /// When it stops proving the principal; none for never.
  pub(crate) expires_ns: Option<u64>,
  /// The proof of issuer authority.
  pub(crate) proof: [u8; 32],
}

/// `EnrollCredential` (the module doc), on the partition the credential's id routes to: refused `Forbidden` on a
/// consumer's channel and `GrantIssuerUnverified` for a proof that does not verify, each counted; `BadRequest` for a key
/// the SSH edge does not take; then recorded, or refused as the database refuses it (`AlreadyExists`,
/// `TableAtCapacity`).
pub(crate) fn enroll(state: &mut ShardState, channel: &Principal, asked: Enrollment) -> ReplyBody {
  if matches!(channel, Principal::Consumer { .. }) {
    state.count(BY_CONSUMER, 1);
    return forbidden("enroll_credential");
  }
  let Enrollment {
    principal,
    material,
    expires_ns,
    proof,
  } = asked;
  let proven = crate::landing::proven_by_launcher(
    state,
    |secret| enroll_proof(secret, &principal, &material, expires_ns),
    &proof,
  );
  if !proven {
    state.count(ISSUER_UNVERIFIED, 1);
    return refused(Refusal::GrantIssuerUnverified);
  }
  let id = id_of(&material);
  let kind = match kind_of(material) {
    Ok(kind) => kind,
    Err(reason) => return refused(Refusal::BadRequest { reason }),
  };
  let record = CredentialRecord {
    id,
    principal: to_db_principal(&principal),
    kind,
    expires_ns,
  };
  let now = state.clock.monotonic_ns();
  match state
    .db
    .mutate(&mut state.segment, &Op::CredentialEnrolled { record }, now)
  {
    Ok(_) => ReplyBody::CredentialEnrolled { id },
    Err(error) => refused(refusal_of_db(&error)),
  }
}

/// `RevokeCredential` (the module doc), on the partition the id routes to: refused `Forbidden` on a consumer's channel
/// and `GrantIssuerUnverified` for a proof that does not verify, each counted; then the record leaves, or `NotFound`.
pub(crate) fn revoke(
  state: &mut ShardState,
  channel: &Principal,
  id: [u8; 32],
  proof: [u8; 32],
) -> ReplyBody {
  if matches!(channel, Principal::Consumer { .. }) {
    state.count(BY_CONSUMER, 1);
    return forbidden("revoke_credential");
  }
  if !crate::landing::proven_by_launcher(state, |secret| revoke_proof(secret, &id), &proof) {
    state.count(ISSUER_UNVERIFIED, 1);
    return refused(Refusal::GrantIssuerUnverified);
  }
  let now = state.clock.monotonic_ns();
  match state
    .db
    .mutate(&mut state.segment, &Op::CredentialRevoked { id }, now)
  {
    Ok(_) => ReplyBody::CredentialRevoked,
    Err(error) => refused(refusal_of_db(&error)),
  }
}

/// Why a lookup gave no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unanswered {
  /// Asked off a daemon's shard, or the partition the id routes to is on none of its shards.
  NoShard,
  /// The partition's shard did not answer within the liveness budget of its own time.
  Silent,
}

/// The credential named `id`, from the partition it routes to, asked from a task on any shard of the daemon: what the
/// HTTP edge's authenticator and the SSH edge's key holders look a presented credential up by (the module doc). On the
/// asking shard the lookup runs inline; on another it is one bounded call there, waited for in that shard's own time
/// (`crate::xshard::call_in_shard_time_given_room`). None when no credential has that id.
pub async fn find(id: [u8; 32]) -> Result<Option<CredentialRecord>, Unanswered> {
  let (origin, owner) = crate::state::with_state(|state| {
    let owner = shard_of_partition(state, owner_of(&id, state.shards.len()));
    (state.shard, owner)
  })
  .ok_or(Unanswered::NoShard)?;
  let owner = owner.ok_or(Unanswered::NoShard)?;
  crate::xshard::call_in_shard_time_given_room(
    origin,
    owner,
    move |state| state.db.partition().credentials().get(&id).cloned(),
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await
  .ok_or(Unanswered::Silent)
}

/// Whether enrolled consumer `consumer` was revoked, or holds no enrollment (the module doc), asked from a task on any
/// shard of the daemon of the partition its id routes to, as [`find`] asks: what the HTTP edge's authenticator and the
/// SSH edge's lookup check of a credential that proves a consumer.
pub(crate) async fn consumer_revoked(consumer: u64) -> Result<bool, Unanswered> {
  let (origin, owner) = crate::state::with_state(|state| {
    let partition = crate::verbs::owner_of_consumer(consumer);
    (state.shard, shard_of_partition(state, partition))
  })
  .ok_or(Unanswered::NoShard)?;
  let owner = owner.ok_or(Unanswered::NoShard)?;
  crate::xshard::call_in_shard_time_given_room(
    origin,
    owner,
    move |state| {
      state
        .db
        .partition()
        .consumer(consumer)
        .is_none_or(|record| record.revoked)
    },
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await
  .ok_or(Unanswered::Silent)
}

/// Why a CRAM-MD5 user has no key ([`cram_md5_key`]). A caller answers each as `svnserve` answers a user its password
/// database lacks, "Username not found" (`Connection::key` with none), save [`Unkeyed::Unanswered`], with which no
/// attempt can be judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unkeyed {
  /// The user is no token's id as the CLI prints one: 64 lowercase hex digits.
  NotAnId,
  /// No credential has that id.
  Unknown,
  /// The credential is an SSH key, which CRAM-MD5 does not prove.
  NotAToken,
  /// The token's expiry has passed on the wall clock.
  Expired,
  /// The token proves an enrolled consumer that was revoked.
  Revoked,
  /// The record's partition did not answer ([`Unanswered`]), or the asking shard's wall clock could not be read.
  Unanswered,
}

/// A CRAM-MD5 user's key and the principal its token proves (A-692).
#[derive(Clone, Debug)]
pub struct CramUser {
  /// The key, made from the contexts the token's record keeps.
  pub key: CramKey,
  /// The principal the token proves: the session's, whatever name the client authenticated as.
  pub principal: Principal,
}

/// The CRAM-MD5 key of `user`, a token's id as the CLI prints it, and the principal the token proves, asked from a task
/// on any shard of the daemon (the module doc): what an ra_svn connection answers a client's CRAM-MD5 response with
/// (`slates_svn::serve::Connection::key`). The record is found on its partition ([`find`]) and gives a key only while
/// it is a token, live on the wall clock and, for a consumer's, while the consumer is enrolled, as the HTTP edge judges
/// a presented token (`vcs_http::daemon_edge`). The key is the record's contexts; the token is never held. The session's
/// principal is the record's: the user name names the token and no one.
pub async fn cram_md5_key(user: &[u8]) -> Result<CramUser, Unkeyed> {
  let id = id_of_user(user).ok_or(Unkeyed::NotAnId)?;
  let record = find(id)
    .await
    .map_err(|_| Unkeyed::Unanswered)?
    .ok_or(Unkeyed::Unknown)?;
  let CredentialKind::HttpToken { cram_md5_contexts } = &record.kind else {
    return Err(Unkeyed::NotAToken);
  };
  let key = CramKey::from_contexts(CramContexts::from_bytes(cram_md5_contexts));
  let now_ns = crate::state::with_state(|state| state.clock.wall_ns())
    .and_then(|ns| u64::try_from(ns).ok())
    .ok_or(Unkeyed::Unanswered)?;
  if !record.live_at(now_ns) {
    return Err(Unkeyed::Expired);
  }
  if let Principal::Consumer { consumer, .. } = &record.principal
    && consumer_revoked(*consumer)
      .await
      .map_err(|_| Unkeyed::Unanswered)?
  {
    return Err(Unkeyed::Revoked);
  }
  Ok(CramUser {
    key,
    principal: record.principal,
  })
}

/// The id CRAM-MD5 user `user` names: 64 lowercase hex digits, the CLI's form of a token's id, read as its 32 bytes;
/// none for any other user.
fn id_of_user(user: &[u8]) -> Option<[u8; 32]> {
  let (pairs, rest) = user.as_chunks::<HEX_DIGITS_PER_BYTE>();
  if !rest.is_empty() {
    return None;
  }
  let mut id = [0u8; 32];
  if pairs.len() != id.len() {
    return None;
  }
  for (byte, [high, low]) in id.iter_mut().zip(pairs) {
    *byte = (lower_hex_digit(*high)? << HEX_DIGIT_BITS) | lower_hex_digit(*low)?;
  }
  Some(id)
}

/// The value of lowercase hex digit `digit`; none for any other byte.
fn lower_hex_digit(digit: u8) -> Option<u8> {
  match digit {
    b'0'..=b'9' => digit.checked_sub(b'0'),
    b'a'..=b'f' => digit
      .checked_sub(b'a')
      .and_then(|value| value.checked_add(HEX_LETTER_BASE)),
    _ => None,
  }
}

#[cfg(test)]
mod tests;
