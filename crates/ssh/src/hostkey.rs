//! The server's host key, `ssh-ed25519` (A-594; RFC 8709; §4.17.2 "host and user keys `ssh-ed25519`").
//!
//! The key signs each exchange hash (RFC 4253 §8, RFC 5656 §4), so a client that pinned it knows it reached this server.
//! Its public form is the blob a client stores, `string "ssh-ed25519", string key` (RFC 8709 §4), and a signature is
//! `string "ssh-ed25519", string signature` (§6), the 64-byte signature of RFC 8032 §5.1.6, which is deterministic: the
//! same key over the same hash signs the same bytes, so a transcript with known keys is reproducible. Where the key is
//! kept, and its rotation, are the server's (the next pieces); here it is built from its 32-byte seed (RFC 8032 §5.1.5)
//! or a PKCS#8 document and never leaves AWS-LC.

use core::fmt;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};

use crate::wire::{WireRefusal, put_string};

/// The algorithm name of an Ed25519 host key and of its signatures (RFC 8709 §4, §6).
pub const ED25519: &str = "ssh-ed25519";

/// Format: the bytes of an Ed25519 seed, the private key RFC 8032 §5.1.5 names.
pub const ED25519_SEED_LEN: usize = 32;

/// A host key AWS-LC refused to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostKeyRejected;

/// A server's host key.
pub struct HostKey {
  /// The key pair, held by AWS-LC.
  pair: Ed25519KeyPair,
  /// The public key blob, `K_S` in the exchange hash.
  blob: Vec<u8>,
}

impl fmt::Debug for HostKey {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("HostKey")
      .field("algorithm", &ED25519)
      .field("blob", &self.blob)
      .finish()
  }
}

impl HostKey {
  /// The Ed25519 host key whose private key is `seed`.
  pub fn ed25519_from_seed(seed: &[u8; ED25519_SEED_LEN]) -> Result<Self, HostKeyRejected> {
    let pair = Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| HostKeyRejected)?;
    Self::from_pair(pair)
  }

  /// The Ed25519 host key in a PKCS#8 document (v1 or v2, as AWS-LC reads them).
  pub fn ed25519_from_pkcs8(document: &[u8]) -> Result<Self, HostKeyRejected> {
    let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(document).map_err(|_| HostKeyRejected)?;
    Self::from_pair(pair)
  }

  /// Wraps `pair` with its public blob.
  fn from_pair(pair: Ed25519KeyPair) -> Result<Self, HostKeyRejected> {
    let blob = algorithm_and(pair.public_key().as_ref()).map_err(|_| HostKeyRejected)?;
    Ok(Self { pair, blob })
  }

  /// The key's algorithm name, as the server's KEXINIT lists it.
  pub fn algorithm(&self) -> &'static str {
    ED25519
  }

  /// The public key blob a client pins: `string "ssh-ed25519", string key` (RFC 8709 §4).
  pub fn public_blob(&self) -> &[u8] {
    &self.blob
  }

  /// The signature blob over `message`: `string "ssh-ed25519", string signature` (RFC 8709 §6).
  pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, WireRefusal> {
    let signature = self.pair.sign(message);
    algorithm_and(signature.as_ref())
  }
}

/// `string "ssh-ed25519", string value`, the shape of both the key blob and the signature blob.
fn algorithm_and(value: &[u8]) -> Result<Vec<u8>, WireRefusal> {
  let mut blob = Vec::new();
  put_string(&mut blob, ED25519.as_bytes())?;
  put_string(&mut blob, value)?;
  Ok(blob)
}
