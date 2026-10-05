//! SecP384r1MLKEM1024 (draft-ietf-tls-ecdhe-mlkem; condition 8; hyper-raft `docs/seal.md` §10): the hybrid key exchange
//! slates' own nodes prefer between them — ML-KEM-1024, CNSA 2.0's key establishment and NIST category 5, with ECDH over
//! P-384, so the session key stays safe while either problem stays hard (SP 800-56C Rev. 2 §2, a concatenated shared
//! secret). rustls 0.23 ships X25519MLKEM768 and SecP256r1MLKEM768 but not this group; it is built here from rustls'
//! own public P-384 and ML-KEM-1024 groups, the same way rustls builds its hybrids, with the draft's layout for the
//! P-curve hybrids: the classical part first in both shares and in the secret (client share = P-384 point ‖ ML-KEM-1024
//! encapsulation key; server share = P-384 point ‖ ML-KEM-1024 ciphertext; secret = ECDH secret ‖ ML-KEM secret), at
//! codepoint 0x11ED. Interoperability with an independent implementation (OpenSSL 3.5) is shown by
//! `examples/tls_interop.rs`, in both directions.
//!
//! Only the fleet planes offer it (both ends are slates); the RPC-with-TLS export keeps the standard groups, which a
//! kernel's TLS handshake daemon offers.

use rustls::crypto::{ActiveKeyExchange, CompletedKeyExchange, SharedSecret, SupportedKxGroup};
use rustls::{Error, NamedGroup, PeerMisbehaved, ProtocolVersion};

/// Format: the group's codepoint (draft-ietf-tls-ecdhe-mlkem, IANA TLS Supported Groups: SecP384r1MLKEM1024).
pub const SECP384R1_MLKEM1024_CODEPOINT: u16 = 0x11ED;
/// Format: an uncompressed P-384 point (SEC 1 §2.3.3): the classical share, both directions.
const P384_SHARE: usize = 97;
/// Format: an ML-KEM-1024 encapsulation key (FIPS 203): the post-quantum part of the client's share.
const MLKEM1024_ENCAPSULATION_KEY: usize = 1568;
/// Format: an ML-KEM-1024 ciphertext (FIPS 203): the post-quantum part of the server's share.
const MLKEM1024_CIPHERTEXT: usize = 1568;

/// The SecP384r1MLKEM1024 group (the module doc).
pub static SECP384R1_MLKEM1024: &dyn SupportedKxGroup = &Hybrid;

/// The hybrid: P-384 then ML-KEM-1024.
#[derive(Debug)]
struct Hybrid;

fn invalid_share() -> Error {
  Error::PeerMisbehaved(PeerMisbehaved::InvalidKeyShare)
}

/// `share` as (classical, post-quantum), refusing any length but the layout's.
fn split(share: &[u8], post_quantum: usize) -> Result<(&[u8], &[u8]), Error> {
  if share.len() != P384_SHARE.saturating_add(post_quantum) {
    return Err(invalid_share());
  }
  share.split_at_checked(P384_SHARE).ok_or_else(invalid_share)
}

impl SupportedKxGroup for Hybrid {
  fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
    let classical = rustls::crypto::aws_lc_rs::kx_group::SECP384R1.start()?;
    let post_quantum = rustls::crypto::aws_lc_rs::kx_group::MLKEM1024.start()?;
    let share = [classical.pub_key(), post_quantum.pub_key()].concat();
    Ok(Box::new(ActiveHybrid {
      classical,
      post_quantum,
      share,
    }))
  }

  fn start_and_complete(&self, client_share: &[u8]) -> Result<CompletedKeyExchange, Error> {
    let (classical_share, post_quantum_share) = split(client_share, MLKEM1024_ENCAPSULATION_KEY)?;
    let classical =
      rustls::crypto::aws_lc_rs::kx_group::SECP384R1.start_and_complete(classical_share)?;
    let post_quantum =
      rustls::crypto::aws_lc_rs::kx_group::MLKEM1024.start_and_complete(post_quantum_share)?;
    Ok(CompletedKeyExchange {
      group: self.name(),
      pub_key: [
        classical.pub_key.as_slice(),
        post_quantum.pub_key.as_slice(),
      ]
      .concat(),
      secret: SharedSecret::from(
        [
          classical.secret.secret_bytes(),
          post_quantum.secret.secret_bytes(),
        ]
        .concat(),
      ),
    })
  }

  fn name(&self) -> NamedGroup {
    NamedGroup::from(SECP384R1_MLKEM1024_CODEPOINT)
  }

  fn fips(&self) -> bool {
    // SP 800-56C Rev. 2: the element first in the concatenated secret controls approval — here the P-384 ECDH, an
    // approved scheme (as rustls reasons for its own P-curve hybrid).
    rustls::crypto::aws_lc_rs::kx_group::SECP384R1.fips()
  }

  fn usable_for_version(&self, version: ProtocolVersion) -> bool {
    version == ProtocolVersion::TLSv1_3
  }
}

/// A client's started exchange: both halves and their concatenated share.
struct ActiveHybrid {
  classical: Box<dyn ActiveKeyExchange>,
  post_quantum: Box<dyn ActiveKeyExchange>,
  share: Vec<u8>,
}

impl ActiveKeyExchange for ActiveHybrid {
  fn complete(self: Box<Self>, peer_share: &[u8]) -> Result<SharedSecret, Error> {
    let (classical_share, post_quantum_share) = split(peer_share, MLKEM1024_CIPHERTEXT)?;
    let classical = self.classical.complete(classical_share)?;
    let post_quantum = self.post_quantum.complete(post_quantum_share)?;
    Ok(SharedSecret::from(
      [classical.secret_bytes(), post_quantum.secret_bytes()].concat(),
    ))
  }

  fn pub_key(&self) -> &[u8] {
    &self.share
  }

  fn group(&self) -> NamedGroup {
    NamedGroup::from(SECP384R1_MLKEM1024_CODEPOINT)
  }
}
