//! `curve25519-sha256`, the exchange hash and the keys it gives (A-594; RFC 8731 §3; RFC 5656 §4; RFC 4253 §7.2, §8).
//!
//! **The exchange.** The client sends `SSH_MSG_KEX_ECDH_INIT` with its ephemeral X25519 public key `Q_C`; the server makes
//! its own ephemeral key, computes the shared secret `X = X25519(server private, Q_C)` and answers
//! `SSH_MSG_KEX_ECDH_REPLY` with its host key `K_S`, its public key `Q_S` and its signature of the exchange hash. Both
//! keys are 32 bytes, and "Clients and servers MUST also abort if the length of the received public keys are not the
//! expected lengths"; the server checks "whether the computed Diffie-Hellman shared secret is the all-zero value and
//! abort[s] if so" [B: RFC 8731 §3], by OR-ing its bytes as RFC 7748 §6.1 suggests, whatever AWS-LC already refused.
//!
//! **The shared secret's form.** "The 32 or 56 bytes of X are converted into K by interpreting the octets as an unsigned
//! fixed-length integer encoded in network byte order", then written as an `mpint` [B: RFC 8731 §3.1]. `K` enters the
//! hash and the key derivation in that form, its `uint32` length included, as NIST CAVS's SSH vectors give it.
//!
//! **The hash.** `H = SHA-256(string V_C, string V_S, string I_C, string I_S, string K_S, string Q_C, string Q_S,
//! mpint K)` [B: RFC 5656 §4]: the two identification lines without CR LF, the two KEXINIT payloads whole, the host key
//! blob and the two public keys. The first exchange's `H` is the session id, kept for the connection's life.
//!
//! **The keys.** `HASH(K || H || X || session_id)` for each letter `X`, extended by `HASH(K || H || key so far)` until
//! long enough [B: RFC 4253 §7.2]: IVs "A" (client to server) and "B", keys "C" and "D". The integrity keys "E" and "F"
//! are not derived: both ciphers are AEADs and "the integrity key is not used" [B: RFC 5647 §5.1]. [`derive`] takes its
//! hash as a parameter so NIST's SHA-1 vectors exercise the extension (a 192-bit key from a 160-bit hash) that
//! ChaCha20-Poly1305's 64-byte key needs from SHA-256.
//!
//! **Secrets.** The shared secret and every derived key live in a [`Secret`], overwritten with zeros when dropped. The
//! write goes through `core::hint::black_box` so it is not removed as a dead store; that is best effort, as the standard
//! library promises nothing stronger and the workspace takes no dependency for it. AWS-LC holds the cipher keys after.

use core::fmt;

use aws_lc_rs::agreement::{self, PrivateKey, UnparsedPublicKey, X25519};
use aws_lc_rs::digest::{self, SHA256};

use crate::packet::Cipher;
use crate::to_u64;
use crate::wire::{Reader, UINT32_LEN, WireRefusal, put_byte, put_mpint_unsigned, put_string};

/// `SSH_MSG_KEX_ECDH_INIT` (RFC 5656 §7.1).
pub const KEX_ECDH_INIT: u8 = 30;

/// `SSH_MSG_KEX_ECDH_REPLY` (RFC 5656 §7.1).
pub const KEX_ECDH_REPLY: u8 = 31;

/// The bytes of an X25519 key, public or private (RFC 7748 §5; RFC 8731 §3: "strings of 32 bytes for Curve25519").
pub const X25519_LEN: usize = 32;

/// The bytes of a SHA-256 hash (FIPS 180-4): the exchange hash and the session id.
pub const HASH_LEN: usize = 32;

/// Why the key exchange was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KexRefusal {
  /// The client's public key is not 32 bytes.
  PublicKeyLength {
    /// Its length.
    length: u64,
  },
  /// The shared secret was all zeros, or AWS-LC refused the client's key.
  SharedSecretRejected,
  /// AWS-LC refused the server's ephemeral key.
  PrivateKeyRejected,
  /// A value would not encode.
  Encoding(WireRefusal),
}

/// Key material, overwritten with zeros when dropped.
pub struct Secret(Vec<u8>);

impl Secret {
  /// The material.
  pub fn bytes(&self) -> &[u8] {
    &self.0
  }
}

impl Drop for Secret {
  fn drop(&mut self) {
    self.0.fill(0);
    core::hint::black_box(&self.0);
  }
}

impl fmt::Debug for Secret {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "Secret({} bytes)", self.0.len())
  }
}

/// The server's half of a `curve25519-sha256` exchange.
#[derive(Debug)]
pub struct ServerHalf {
  /// The server's ephemeral public key, `Q_S`.
  pub public: [u8; X25519_LEN],
  /// The shared secret `K` as an `mpint`, its length included.
  pub shared_secret: Secret,
}

/// Reads `SSH_MSG_KEX_ECDH_INIT`: the client's public key, `Q_C`.
pub fn read_ecdh_init(payload: &[u8]) -> Result<&[u8], WireRefusal> {
  let mut reader = Reader::new(payload);
  reader.byte()?;
  let public = reader.string()?;
  reader.finish()?;
  Ok(public)
}

/// The server's half under its ephemeral private key `private`, against the client's public key `client_public`.
pub fn curve25519_server(
  private: &[u8; X25519_LEN],
  client_public: &[u8],
) -> Result<ServerHalf, KexRefusal> {
  if client_public.len() != X25519_LEN {
    return Err(KexRefusal::PublicKeyLength {
      length: to_u64(client_public.len()),
    });
  }
  let key =
    PrivateKey::from_private_key(&X25519, private).map_err(|_| KexRefusal::PrivateKeyRejected)?;
  let public = key
    .compute_public_key()
    .map_err(|_| KexRefusal::PrivateKeyRejected)?;
  let public: [u8; X25519_LEN] = public
    .as_ref()
    .try_into()
    .map_err(|_| KexRefusal::PrivateKeyRejected)?;
  let peer = UnparsedPublicKey::new(&X25519, client_public);
  let shared_secret = agreement::agree(
    &key,
    peer,
    KexRefusal::SharedSecretRejected,
    shared_secret_mpint,
  )?;
  Ok(ServerHalf {
    public,
    shared_secret,
  })
}

/// `X` as RFC 8731 §3.1's `mpint` K, refusing the all-zero value.
fn shared_secret_mpint(secret: &[u8]) -> Result<Secret, KexRefusal> {
  if secret.iter().fold(0u8, |any, byte| any | byte) == 0 {
    return Err(KexRefusal::SharedSecretRejected);
  }
  let mut encoded = Vec::with_capacity(secret.len().saturating_add(UINT32_LEN).saturating_add(1));
  put_mpint_unsigned(&mut encoded, secret).map_err(KexRefusal::Encoding)?;
  Ok(Secret(encoded))
}

/// What the exchange hash covers (RFC 5656 §4).
#[derive(Clone, Copy, Debug)]
pub struct ExchangeInputs<'a> {
  /// `V_C`: the client's identification line, CR LF excluded.
  pub client_identification: &'a [u8],
  /// `V_S`: the server's identification line, CR LF excluded.
  pub server_identification: &'a [u8],
  /// `I_C`: the client's KEXINIT payload.
  pub client_kexinit: &'a [u8],
  /// `I_S`: the server's KEXINIT payload.
  pub server_kexinit: &'a [u8],
  /// `K_S`: the server's host key blob.
  pub host_key: &'a [u8],
  /// `Q_C`: the client's ephemeral public key.
  pub client_public: &'a [u8],
  /// `Q_S`: the server's ephemeral public key.
  pub server_public: &'a [u8],
  /// `K`: the shared secret as an `mpint`, its length included.
  pub shared_secret: &'a [u8],
}

/// The exchange hash `H` (RFC 5656 §4).
pub fn exchange_hash(inputs: &ExchangeInputs<'_>) -> Result<[u8; HASH_LEN], KexRefusal> {
  let mut context = digest::Context::new(&SHA256);
  for field in [
    inputs.client_identification,
    inputs.server_identification,
    inputs.client_kexinit,
    inputs.server_kexinit,
    inputs.host_key,
    inputs.client_public,
    inputs.server_public,
  ] {
    let length = u32::try_from(field.len()).map_err(|_| {
      KexRefusal::Encoding(WireRefusal::TooLong {
        length: field.len(),
      })
    })?;
    context.update(&length.to_be_bytes());
    context.update(field);
  }
  context.update(inputs.shared_secret);
  context
    .finish()
    .as_ref()
    .try_into()
    .map_err(|_| KexRefusal::Encoding(WireRefusal::TooLong { length: HASH_LEN }))
}

/// RFC 4253 §7.2's derivation of `length` bytes for `letter`, under `algorithm`.
pub fn derive(
  algorithm: &'static digest::Algorithm,
  shared_secret: &[u8],
  exchange_hash: &[u8],
  letter: u8,
  session_id: &[u8],
  length: usize,
) -> Secret {
  let mut first = digest::Context::new(algorithm);
  first.update(shared_secret);
  first.update(exchange_hash);
  first.update(&[letter]);
  first.update(session_id);
  let mut key = Secret(first.finish().as_ref().to_vec());
  while key.0.len() < length {
    let mut next = digest::Context::new(algorithm);
    next.update(shared_secret);
    next.update(exchange_hash);
    next.update(&key.0);
    key.0.extend_from_slice(next.finish().as_ref());
  }
  key.0.truncate(length);
  key
}

/// The IV letter of what the client sends (RFC 4253 §7.2).
pub const IV_CLIENT_TO_SERVER: u8 = b'A';
/// The IV letter of what the server sends.
pub const IV_SERVER_TO_CLIENT: u8 = b'B';
/// The key letter of what the client sends.
pub const KEY_CLIENT_TO_SERVER: u8 = b'C';
/// The key letter of what the server sends.
pub const KEY_SERVER_TO_CLIENT: u8 = b'D';

/// One direction's new cipher and its keys.
#[derive(Debug)]
pub struct DirectionKeys {
  /// The cipher.
  pub cipher: Cipher,
  /// Its key.
  pub key: Secret,
  /// Its IV (empty for ChaCha20-Poly1305).
  pub iv: Secret,
}

/// Both directions' keys after an exchange.
#[derive(Debug)]
pub struct NewKeys {
  /// What the client sends.
  pub client_to_server: DirectionKeys,
  /// What the server sends.
  pub server_to_client: DirectionKeys,
}

/// The keys an exchange gives each direction's cipher (RFC 4253 §7.2, under SHA-256 for `curve25519-sha256`).
pub fn new_keys(
  shared_secret: &[u8],
  exchange_hash: &[u8],
  session_id: &[u8],
  client_to_server: Cipher,
  server_to_client: Cipher,
) -> NewKeys {
  let direction = |cipher: Cipher, iv_letter: u8, key_letter: u8| DirectionKeys {
    cipher,
    key: derive(
      &SHA256,
      shared_secret,
      exchange_hash,
      key_letter,
      session_id,
      cipher.key_len(),
    ),
    iv: derive(
      &SHA256,
      shared_secret,
      exchange_hash,
      iv_letter,
      session_id,
      cipher.iv_len(),
    ),
  };
  NewKeys {
    client_to_server: direction(client_to_server, IV_CLIENT_TO_SERVER, KEY_CLIENT_TO_SERVER),
    server_to_client: direction(server_to_client, IV_SERVER_TO_CLIENT, KEY_SERVER_TO_CLIENT),
  }
}

/// `SSH_MSG_KEX_ECDH_REPLY`: the host key blob, the server's public key and the signature blob (RFC 5656 §4).
pub fn ecdh_reply(
  host_key: &[u8],
  server_public: &[u8],
  signature: &[u8],
) -> Result<Vec<u8>, WireRefusal> {
  let mut payload = Vec::new();
  put_byte(&mut payload, KEX_ECDH_REPLY);
  put_string(&mut payload, host_key)?;
  put_string(&mut payload, server_public)?;
  put_string(&mut payload, signature)?;
  Ok(payload)
}
