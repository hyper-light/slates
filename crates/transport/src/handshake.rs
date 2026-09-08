//! The session-plane TLS 1.3 handshake over `rustls::quic` (§4.10a §3, §8, slice 4e). slates's
//! owned QUIC dialect keeps TLS 1.3 for the handshake and record protection (D-15, not hecate's
//! Noise): `rustls::quic` runs the RFC 9001 handshake in CRYPTO frames and hands back the packet-
//! protection keys per encryption level. A node authenticates with its **enrolled identity** — here
//! a self-signed certificate the peer pins (the test stands in for enrollment distributing it);
//! there is no CA PKI. A term/epoch advance drops the session (fencing, D-16) — owed with membership.
//!
//! This module holds the **one `Arc` in slates**: `rustls::quic::{Client,Server}Connection::new`
//! take `Arc<ClientConfig>`/`Arc<ServerConfig>` by signature (D-8 exception 2 — a foreign API that
//! takes `Arc`). The two owners are this connection value and rustls's internal handshake state; no
//! slates type is shared by `Arc`.

// D-8 exception 2: the `Arc` here is only rustls's config, required by its constructor signature.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::quic::{ClientConnection, ServerConnection, Version};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// A refusal building or driving the handshake.
#[derive(Debug)]
pub enum HandshakeError {
  /// rustls reported a TLS error.
  Tls(rustls::Error),
  /// A certificate or key could not be built.
  Setup(String),
}

impl std::fmt::Display for HandshakeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      HandshakeError::Tls(e) => write!(f, "tls: {e}"),
      HandshakeError::Setup(s) => write!(f, "handshake setup: {s}"),
    }
  }
}

impl std::error::Error for HandshakeError {}

impl From<rustls::Error> for HandshakeError {
  fn from(e: rustls::Error) -> Self {
    HandshakeError::Tls(e)
  }
}

/// A node's TLS identity: its certificate chain (a single self-signed cert standing in for the
/// enrolled identity) and its private key.
pub struct Identity {
  cert: CertificateDer<'static>,
  key: PrivateKeyDer<'static>,
}

impl Identity {
  /// A node's identity from its enrolled certificate and private key, DER-encoded (enrollment, §4.13,
  /// distributes these — owed; this is the seam it fills). The self-signed test identities are minted
  /// with `rcgen` in the tests.
  pub fn from_der(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Identity {
    Identity { cert, key }
  }

  /// The peer-pinnable certificate (the identity to trust, as enrollment would distribute it).
  pub fn certificate(&self) -> CertificateDer<'static> {
    self.cert.clone()
  }
}

/// The `ring` crypto provider (the one that builds without cmake here).
// structural: allow — D-8 exception 2: rustls's provider/config types cross its API as `Arc`.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
  // structural: allow — D-8 exception 2: rustls's `builder_with_provider` takes `Arc` by signature.
  Arc::new(rustls::crypto::ring::default_provider())
}

/// A server config presenting `identity`, TLS 1.3 only, no client auth (peer auth is the pinned
/// server identity; mutual enrolled-identity auth is owed).
pub fn server_config(identity: &Identity) -> Result<ServerConfig, HandshakeError> {
  ServerConfig::builder_with_provider(provider())
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())
    .map_err(HandshakeError::from)
}

/// A client config that trusts exactly `pinned` (the peer's enrolled certificate) — TLS 1.3 only.
pub fn client_config(pinned: CertificateDer<'static>) -> Result<ClientConfig, HandshakeError> {
  let mut roots = RootCertStore::empty();
  roots
    .add(pinned)
    .map_err(|e| HandshakeError::Setup(e.to_string()))?;
  ClientConfig::builder_with_provider(provider())
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_no_client_auth()
    .pipe(Ok)
}

/// A tiny helper so the config builders read as a pipeline.
trait Pipe: Sized {
  fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
    f(self)
  }
}
impl<T> Pipe for T {}

/// slates's QUIC transport parameters (opaque to rustls; the codec's own params are owed). A fixed
/// non-empty value so both ends present some.
const TRANSPORT_PARAMS: &[u8] = b"slates-quic-v1";

/// Builds the client and server QUIC connections for `name`, the client pinning the server's cert.
pub fn connect(
  server: &Identity,
  name: &str,
) -> Result<(ClientConnection, ServerConnection), HandshakeError> {
  connect_with(server, &server.certificate(), name)
}

/// Builds the connections with the client pinning `client_pin` (which may differ from the server's
/// real certificate — the wrong-pin test uses the difference). For separate endpoints each side
/// builds only its own connection ([`client_connection`]/[`server_connection`]); this pairs them for
/// the in-process handshake test.
pub fn connect_with(
  server: &Identity,
  client_pin: &CertificateDer<'static>,
  name: &str,
) -> Result<(ClientConnection, ServerConnection), HandshakeError> {
  Ok((
    client_connection(client_pin, name)?,
    server_connection(server)?,
  ))
}

/// The client half of the handshake: pins `pinned` (the peer's enrolled certificate) and targets the
/// peer as `name`. Needs no private key — only the certificate it trusts.
pub fn client_connection(
  pinned: &CertificateDer<'static>,
  name: &str,
) -> Result<ClientConnection, HandshakeError> {
  let cfg = client_config(pinned.clone())?;
  let server_name =
    ServerName::try_from(name.to_owned()).map_err(|e| HandshakeError::Setup(e.to_string()))?;
  // structural: allow — D-8 exception 2: rustls's `ClientConnection::new` takes `Arc` by signature.
  let cfg = Arc::new(cfg);
  ClientConnection::new(cfg, Version::V1, server_name, TRANSPORT_PARAMS.to_vec())
    .map_err(HandshakeError::from)
}

/// The server half of the handshake: presents `identity`'s certificate and private key.
pub fn server_connection(identity: &Identity) -> Result<ServerConnection, HandshakeError> {
  let cfg = server_config(identity)?;
  // structural: allow — D-8 exception 2: rustls's `ServerConnection::new` takes `Arc` by signature.
  ServerConnection::new(Arc::new(cfg), Version::V1, TRANSPORT_PARAMS.to_vec())
    .map_err(HandshakeError::from)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A fresh self-signed identity for `name`, minted with `ring` via `rcgen` (no CA, no cmake) —
  /// the test's stand-in for an enrolled credential.
  fn self_signed(name: &str) -> Identity {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
      .unwrap()
      .self_signed(&key)
      .unwrap();
    Identity::from_der(
      cert.der().clone(),
      PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
  }

  /// Drives the handshake to completion by shuttling CRYPTO bytes both ways until neither side is
  /// still handshaking, or gives up after a bounded number of rounds.
  fn drive(
    client: &mut ClientConnection,
    server: &mut ServerConnection,
  ) -> Result<(), rustls::Error> {
    for _ in 0..16 {
      if !client.is_handshaking() && !server.is_handshaking() {
        return Ok(());
      }
      let mut to_server = Vec::new();
      client.write_hs(&mut to_server);
      if !to_server.is_empty() {
        server.read_hs(&to_server)?;
      }
      let mut to_client = Vec::new();
      server.write_hs(&mut to_client);
      if !to_client.is_empty() {
        client.read_hs(&to_client)?;
      }
    }
    Ok(())
  }

  /// The riskiest, most-blind piece of the `Connection`, probed in isolation: the handshake's 1-RTT
  /// packet keys actually protect and unprotect a payload. Drives the handshake capturing each side's
  /// `OneRtt` keys, then encrypts a payload with the client's local packet key and decrypts it with
  /// the server's remote packet key — the record protection the `Connection` will wrap frames in.
  #[test]
  fn the_handshake_yields_working_packet_keys() {
    use rustls::quic::KeyChange;

    let identity = self_signed("slates-node");
    let (mut client, mut server) = connect(&identity, "slates-node").unwrap();
    let mut client_keys = None;
    let mut server_keys = None;
    for _ in 0..16 {
      if !client.is_handshaking() && !server.is_handshaking() {
        break;
      }
      let mut to_server = Vec::new();
      if let Some(KeyChange::OneRtt { keys, .. }) = client.write_hs(&mut to_server) {
        client_keys = Some(keys);
      }
      if !to_server.is_empty() {
        server.read_hs(&to_server).unwrap();
      }
      let mut to_client = Vec::new();
      if let Some(KeyChange::OneRtt { keys, .. }) = server.write_hs(&mut to_client) {
        server_keys = Some(keys);
      }
      if !to_client.is_empty() {
        client.read_hs(&to_client).unwrap();
      }
    }

    let client_keys = client_keys.expect("client derived 1-RTT keys");
    let server_keys = server_keys.expect("server derived 1-RTT keys");

    // Protect a payload with the client's local key; unprotect with the server's remote key.
    let plaintext = b"session frames go here";
    let header = [0x40u8, 0, 0, 0]; // a placeholder short-header (the AAD); real header owed.
    let packet_number = 0u64;
    let mut buf = plaintext.to_vec();
    let tag = client_keys
      .local
      .packet
      .encrypt_in_place(packet_number, &header, &mut buf)
      .unwrap();
    buf.extend_from_slice(tag.as_ref());
    let opened = server_keys
      .remote
      .packet
      .decrypt_in_place(packet_number, &header, &mut buf)
      .unwrap();
    assert_eq!(
      opened, plaintext,
      "the handshake keys protect and unprotect a packet"
    );
  }

  /// A client and server complete a TLS 1.3 handshake over `rustls::quic`, the client pinning the
  /// server's enrolled (self-signed) identity — the session plane's authenticated handshake.
  #[test]
  fn a_pinned_tls13_handshake_completes() {
    let identity = self_signed("slates-node");
    let (mut client, mut server) = connect(&identity, "slates-node").unwrap();
    drive(&mut client, &mut server).unwrap();
    assert!(!client.is_handshaking(), "the client handshake completed");
    assert!(!server.is_handshaking(), "the server handshake completed");
    // The negotiated protocol is TLS 1.3.
    assert_eq!(
      client.protocol_version(),
      Some(rustls::ProtocolVersion::TLSv1_3)
    );
  }

  /// A client that pins the WRONG certificate rejects the server — the handshake is authenticated,
  /// not permissive.
  #[test]
  fn a_wrong_pin_is_rejected() {
    let server_identity = self_signed("slates-node");
    let other = self_signed("slates-node");
    // The client pins `other`'s cert but talks to the real server.
    let (mut client, mut server) =
      connect_with(&server_identity, &other.certificate(), "slates-node").unwrap();
    let outcome = drive(&mut client, &mut server);
    // Either the drive errors, or the client never completes — never a silent accept of a wrong pin.
    assert!(
      outcome.is_err() || client.is_handshaking(),
      "a wrong-pinned certificate must not authenticate"
    );
  }
}
