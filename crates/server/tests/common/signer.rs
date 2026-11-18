//! A signer for signed work's tests (A-321): an Ed25519 key made fresh from AWS-LC's secure random, and SSH signatures
//! over a statement built as `ssh-keygen -Y sign -n slates-work` builds them (OpenSSH `PROTOCOL.sshsig`), so a test
//! signs what a person's key signs without the tool. The verifier's own golden vectors are real `ssh-keygen` signatures
//! (`crates/server/src/signed_work.rs`); this signer only stands in for a key a test cannot type.

use aws_lc_rs::digest;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};

/// Format: the namespace signed work is signed under (`slates_server::signed_work::NAMESPACE`).
pub(crate) const NAMESPACE: &str = slates_server::signed_work::NAMESPACE;

/// A software Ed25519 key, as `ssh-keygen -t ed25519` would make one.
pub(crate) struct TestSigner {
  pair: Ed25519KeyPair,
}

/// Appends an SSH `string`: a big-endian `uint32` length, then the bytes.
fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
  out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
  out.extend_from_slice(bytes);
}

impl TestSigner {
  /// A fresh key.
  pub(crate) fn fresh() -> TestSigner {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    TestSigner {
      pair: Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap(),
    }
  }

  /// The public key in SSH wire form (what `SetSigners` takes).
  pub(crate) fn public(&self) -> Vec<u8> {
    let mut blob = Vec::new();
    put_string(&mut blob, b"ssh-ed25519");
    put_string(&mut blob, self.pair.public_key().as_ref());
    blob
  }

  /// The key's fingerprint: SHA-256 of its wire form, as `ssh-keygen -l -E sha256` computes it.
  pub(crate) fn fingerprint(&self) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest::digest(&digest::SHA256, &self.public()).as_ref());
    out
  }

  /// An SSH signature blob over `statement` under the signed-work namespace.
  pub(crate) fn sign(&self, statement: &[u8]) -> Vec<u8> {
    self.sign_under(NAMESPACE, statement)
  }

  /// An SSH signature blob over `statement` under `namespace`, hashed with SHA-512 as `ssh-keygen` hashes it.
  pub(crate) fn sign_under(&self, namespace: &str, statement: &[u8]) -> Vec<u8> {
    let mut signed = b"SSHSIG".to_vec();
    put_string(&mut signed, namespace.as_bytes());
    put_string(&mut signed, b"");
    put_string(&mut signed, b"sha512");
    put_string(
      &mut signed,
      digest::digest(&digest::SHA512, statement).as_ref(),
    );
    let mut inner = Vec::new();
    put_string(&mut inner, b"ssh-ed25519");
    put_string(&mut inner, self.pair.sign(&signed).as_ref());
    let mut blob = b"SSHSIG".to_vec();
    blob.extend_from_slice(&1u32.to_be_bytes());
    put_string(&mut blob, &self.public());
    put_string(&mut blob, namespace.as_bytes());
    put_string(&mut blob, b"");
    put_string(&mut blob, b"sha512");
    put_string(&mut blob, &inner);
    blob
  }
}
