//! The session-plane TLS 1.3 handshake over `rustls::quic` (§4.10a §3, §8, slice 4e). slates's
//! owned QUIC dialect keeps TLS 1.3 for the handshake and record protection (D-15, not hecate's
//! Noise): `rustls::quic` runs the RFC 9001 handshake in CRYPTO frames and hands back the packet-
//! protection keys per encryption level. Authentication is **mutual**: each node presents its
//! enrolled certificate. Configured peers pin the exact leaf; optional operator CA roots admit
//! enrollment candidates. Subsequent dials still verify the exact advertised leaf, so sharing
//! an issuer never permits impersonating another peer (§4.13).
//!
//! This module holds rustls's `Arc`s: `rustls::quic::{Client,Server}Connection::new` take
//! `Arc<ClientConfig>`/`Arc<ServerConfig>` by signature (D-8 exception 2 — a foreign API that takes
//! `Arc`), as do the verifier builders. The two owners are this connection value and rustls's internal
//! handshake state; no slates type is shared by `Arc`. The NFS edge's RPC-with-TLS connection is built
//! here too ([`rpc_tls_connection`]), so no other crate names `Arc` and a node's key stays here.

// D-8 exception 2: the `Arc` here is only rustls's config, required by its constructor signature.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::quic::{ClientConnection, ServerConnection, Version};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

use crate::params::TransportParameters;

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
  authorities: Vec<CertificateDer<'static>>,
}

impl Identity {
  /// A node's identity from its enrolled certificate and private key, DER-encoded (enrollment, §4.13,
  /// distributes these — owed; this is the seam it fills). The self-signed test identities are minted
  /// with `rcgen` in the tests.
  pub fn from_der(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Identity {
    Identity {
      cert,
      key,
      authorities: Vec::new(),
    }
  }

  /// Trust anchors the operator supplied for issued peer certificates. The endpoint still pins
  /// the exact presented leaf, so another certificate under this issuer cannot impersonate a peer.
  pub fn with_authorities(mut self, authorities: Vec<CertificateDer<'static>>) -> Self {
    self.authorities = authorities;
    self
  }

  /// The peer-pinnable certificate (the identity to trust, as enrollment would distribute it).
  pub fn certificate(&self) -> CertificateDer<'static> {
    self.cert.clone()
  }
}

/// The aws-lc-rs crypto provider (AWS-LC, the workspace's one cryptographic library).
// structural: allow — D-8 exception 2: rustls's provider/config types cross its API as `Arc`.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
  // structural: allow — D-8 exception 2: rustls's `builder_with_provider` takes `Arc` by signature.
  Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Fills `out` with bytes from the crypto provider's secure random — the one source of randomness the
/// tree links (the TLS handshake's own; no second RNG crate). A secret minted here is what the daemon
/// publishes as its grant-issuer authority (§4.13) and what enrollment derives a consumer's capability
/// from; both are refused rather than minted if the provider cannot fill the buffer.
pub fn secure_random(out: &mut [u8]) -> Result<(), HandshakeError> {
  rustls::crypto::aws_lc_rs::default_provider()
    .secure_random
    .fill(out)
    .map_err(|_| HandshakeError::Setup("the crypto provider's secure random refused".to_owned()))
}

/// A server config presenting `identity`, TLS 1.3 only, that **requires and pins the client's**
/// enrolled certificate (mutual authentication): the client must present a certificate the server
/// finds among `allowed_clients`, so the holder authenticates its caller's identity — server-cert
/// pinning alone does not (§4.13). Session-resumption tickets are disabled: slates's owned dialect has
/// no use for TLS-level resumption, and a post-handshake `NewSessionTicket` would arrive as CRYPTO
/// bytes the 1-RTT packet reader is not meant to parse.
pub fn server_config(
  identity: &Identity,
  allowed_clients: &[CertificateDer<'static>],
) -> Result<ServerConfig, HandshakeError> {
  let verifier = client_verifier(allowed_clients)?;
  // structural: allow — D-8 exception 2: `with_client_cert_verifier` takes `Arc` by signature.
  let verifier = Arc::new(RosterVerifier { inner: verifier });
  let mut config = ServerConfig::builder_with_provider(provider())
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_client_cert_verifier(verifier)
    .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
  config.send_tls13_tickets = 0;
  Ok(config)
}

/// One RPC-with-TLS server connection (RFC 9289; §4.6 "Kubernetes publication without privilege"),
/// presenting this node's `identity` and requiring a client certificate that verifies to the identity's
/// operator authorities ([`Identity::with_authorities`]): TLS 1.3 only (§5, "MUST NOT negotiate TLS versions
/// prior to 1.3"), the server's ALPN list exactly `alpn` (§5: the server "MUST include only" `sunrpc`), no
/// resumption tickets. An identity with no authorities has nothing to verify a client against and is refused
/// (`Setup`), so the network export never opens unauthenticated.
///
/// Built per connection, as [`server_connection`] is: rustls's constructor takes `Arc<ServerConfig>` by
/// signature (D-8 exception 2), so the `Arc` is made here and its one owner is the returned connection —
/// nothing is shared between connections, and the node's private key never leaves this module.
pub fn rpc_tls_connection(
  identity: &Identity,
  alpn: &[u8],
) -> Result<rustls::ServerConnection, HandshakeError> {
  if identity.authorities.is_empty() {
    return Err(HandshakeError::Setup(
      "no operator authority to verify an RPC-with-TLS client against".to_owned(),
    ));
  }
  let verifier = client_verifier(&identity.authorities)?;
  // structural: allow — D-8 exception 2: `with_client_cert_verifier` takes `Arc` by signature.
  let verifier = Arc::new(RosterVerifier { inner: verifier });
  let mut config = ServerConfig::builder_with_provider(provider())
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_client_cert_verifier(verifier)
    .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
  config.alpn_protocols = vec![alpn.to_vec()];
  config.send_tls13_tickets = 0;
  // structural: allow — D-8 exception 2: `rustls::ServerConnection::new` takes `Arc` by signature; one owner.
  rustls::ServerConnection::new(Arc::new(config)).map_err(HandshakeError::from)
}

/// Verifies a relayed enrollment certificate with the same trust and expiry rules as TLS.
/// Calling this does not prove possession; the later pinned TLS session proves that separately.
pub fn verify_enrolled_certificate(
  certificate: &CertificateDer<'static>,
  authorities: &[CertificateDer<'static>],
) -> Result<(), HandshakeError> {
  client_verifier(authorities)?.verify_client_cert(
    certificate,
    &[],
    rustls::pki_types::UnixTime::now(),
  )?;
  Ok(())
}

fn client_verifier(
  allowed_clients: &[CertificateDer<'static>],
  // structural: allow — D-8 exception 2: rustls returns this verifier as Arc; RosterVerifier and rustls own it.
) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>, HandshakeError> {
  let mut roots = RootCertStore::empty();
  for cert in allowed_clients {
    roots
      .add(cert.clone())
      .map_err(|e| HandshakeError::Setup(e.to_string()))?;
  }
  // structural: allow — D-8 exception 2: rustls's verifier builder takes `Arc` by signature.
  let roots = Arc::new(roots);
  let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots, provider())
    .build()
    .map_err(|e| HandshakeError::Setup(e.to_string()))?;
  Ok(verifier)
}

/// The roster verifier: rustls's web-PKI client verifier over the admitted certificates, with **no
/// certificate-authority hints**. The default verifier lists every admitted certificate's subject in
/// the CertificateRequest (`root_hint_subjects`, RFC 8446 §4.2.4), so a server's handshake flight grew
/// with its roster — a 64-peer roster made a 3,024-byte flight against the receiver's 2,048-byte
/// datagram buffer (694 bytes with one peer), and the KIND lane's 38-peer node could never be dialed
/// (`Tls(InvalidMessage(HandshakePayloadTooLarge))` at every dialer, forever; 2026-09-14). A fleet peer
/// always presents its one enrolled certificate whatever the server hints, so the hints carry nothing
/// (RFC 8446 §4.2.4 makes them optional). Every other decision — which certificates are admitted, the
/// signature checks, the schemes — is the inner verifier's, unchanged.
#[derive(Debug)]
struct RosterVerifier {
  // structural: allow — D-8 exception 2: rustls hands its verifier out as `Arc<dyn ..>` by signature.
  inner: Arc<dyn rustls::server::danger::ClientCertVerifier>,
}

impl rustls::server::danger::ClientCertVerifier for RosterVerifier {
  fn offer_client_auth(&self) -> bool {
    self.inner.offer_client_auth()
  }

  fn client_auth_mandatory(&self) -> bool {
    self.inner.client_auth_mandatory()
  }

  fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
    &[]
  }

  fn verify_client_cert(
    &self,
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    now: rustls::pki_types::UnixTime,
  ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
    self
      .inner
      .verify_client_cert(end_entity, intermediates, now)
  }

  fn verify_tls12_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
  ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    self.inner.verify_tls12_signature(message, cert, dss)
  }

  fn verify_tls13_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
  ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    self.inner.verify_tls13_signature(message, cert, dss)
  }

  fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
    self.inner.supported_verify_schemes()
  }

  fn requires_raw_public_keys(&self) -> bool {
    self.inner.requires_raw_public_keys()
  }
}

/// A client config that trusts exactly `pinned_server` (the peer's enrolled certificate) and
/// **presents `client_identity`** as its own certificate for mutual authentication — TLS 1.3 only.
pub fn client_config(
  pinned_server: CertificateDer<'static>,
  client_identity: &Identity,
) -> Result<ClientConfig, HandshakeError> {
  let mut roots = RootCertStore::empty();
  for authority in &client_identity.authorities {
    roots
      .add(authority.clone())
      .map_err(|error| HandshakeError::Setup(error.to_string()))?;
  }
  roots
    .add(pinned_server)
    .map_err(|e| HandshakeError::Setup(e.to_string()))?;
  ClientConfig::builder_with_provider(provider())
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_client_auth_cert(
      vec![client_identity.cert.clone()],
      client_identity.key.clone_key(),
    )
    .map_err(HandshakeError::from)
}

/// Builds the mutually-authenticated client and server QUIC connections for `name`: the `client`
/// presents its identity and pins the `server`'s cert, and the `server` pins the `client`'s cert.
pub fn connect(
  server: &Identity,
  client: &Identity,
  name: &str,
) -> Result<(ClientConnection, ServerConnection), HandshakeError> {
  connect_with(server, client, &client.certificate(), name)
}

/// Builds the connections with the server pinning `allowed_client` (which may differ from the client's
/// real certificate — the wrong-pin test uses the difference). For separate endpoints each side builds
/// only its own connection ([`client_connection`]/[`server_connection`]); this pairs them for the
/// in-process handshake test.
pub fn connect_with(
  server: &Identity,
  client: &Identity,
  allowed_client: &CertificateDer<'static>,
  name: &str,
) -> Result<(ClientConnection, ServerConnection), HandshakeError> {
  Ok((
    client_connection(
      client,
      &server.certificate(),
      name,
      &TransportParameters::local(),
    )?,
    server_connection(
      server,
      std::slice::from_ref(allowed_client),
      &TransportParameters::local(),
    )?,
  ))
}

/// The client half of the handshake: presents `client_identity`, pins `pinned_server` (the peer's
/// enrolled certificate), and targets the peer as `name`.
pub fn client_connection(
  client_identity: &Identity,
  pinned_server: &CertificateDer<'static>,
  name: &str,
  params: &TransportParameters,
) -> Result<ClientConnection, HandshakeError> {
  let cfg = client_config(pinned_server.clone(), client_identity)?;
  let server_name =
    ServerName::try_from(name.to_owned()).map_err(|e| HandshakeError::Setup(e.to_string()))?;
  // structural: allow — D-8 exception 2: rustls's `ClientConnection::new` takes `Arc` by signature.
  let cfg = Arc::new(cfg);
  ClientConnection::new(cfg, Version::V1, server_name, params.encode())
    .map_err(HandshakeError::from)
}

/// The server half of the handshake: presents `identity` and requires a client certificate found among
/// `allowed_clients` (mutual authentication), so it knows which enrolled caller it is speaking to.
pub fn server_connection(
  identity: &Identity,
  allowed_clients: &[CertificateDer<'static>],
  params: &TransportParameters,
) -> Result<ServerConnection, HandshakeError> {
  let cfg = server_config(identity, allowed_clients)?;
  // structural: allow — D-8 exception 2: rustls's `ServerConnection::new` takes `Arc` by signature.
  ServerConnection::new(Arc::new(cfg), Version::V1, params.encode()).map_err(HandshakeError::from)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A fresh self-signed identity for `name`, minted with aws-lc-rs via `rcgen` (no CA) —
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

  /// Goal condition 8 (transfer protected with post-quantum key exchange, aws-lc-rs), 2026-10-04: do complete a
  /// mutual fleet handshake; expect both sides to have negotiated the hybrid X25519MLKEM768 group
  /// (draft-ietf-tls-ecdhe-mlkem, the group TLS deployments ship), never classical X25519 alone. A session
  /// recorded today stays confidential against a later quantum adversary only if its key exchange is hybrid.
  #[test]
  fn a_fleet_handshake_negotiates_the_hybrid_post_quantum_group() {
    let server = self_signed("server.slates");
    let client = self_signed("client.slates");
    let (mut client_side, mut server_side) = connect(&server, &client, "server.slates").unwrap();
    drive(&mut client_side, &mut server_side).unwrap();
    assert!(
      !client_side.is_handshaking() && !server_side.is_handshaking(),
      "the handshake completed"
    );
    for (side, group) in [
      ("client", client_side.negotiated_key_exchange_group()),
      ("server", server_side.negotiated_key_exchange_group()),
    ] {
      assert_eq!(
        group.map(|group| group.name()),
        Some(rustls::NamedGroup::X25519MLKEM768),
        "the {side} negotiated the hybrid post-quantum group"
      );
    }
  }

  /// Shape: the largest handshake flight a fleet server may send — the fragmenter's bound on a whole
  /// flight (`crate::flight::MAX_FLIGHT_BYTES`), past which the sender refuses it typed
  /// (`EndpointError::FlightTooLarge`); the assertion is against that bound, from the flight module, not
  /// a copy.
  const FLIGHT_BOUND: usize = crate::flight::MAX_FLIGHT_BYTES;

  /// A self-signed Ed25519 identity: its signatures are a fixed 64 bytes, so a flight it signs is the
  /// same size on every run — a measurement can compare two flights exactly (an ECDSA signature's DER
  /// form varies by a byte or two with the nonce, which made the comparison below flake).
  fn self_signed_ed25519(name: &str) -> Identity {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
      .unwrap()
      .self_signed(&key)
      .unwrap();
    Identity::from_der(
      cert.der().clone(),
      PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
  }

  /// The server's first flight (the bytes it writes after reading the ClientHello) for a server with
  /// `server_identity` that admits `client_identity` among a roster of `allowed` certificates: what a
  /// dialer must receive whole. The two identities are the caller's, so only the roster differs between
  /// two measurements.
  fn server_first_flight(
    server_identity: &Identity,
    client_identity: &Identity,
    allowed: usize,
  ) -> usize {
    let mut roster: Vec<CertificateDer<'static>> = (1..allowed)
      .map(|_| self_signed("slates-fleet").certificate())
      .collect();
    roster.push(client_identity.certificate());
    let mut client = client_connection(
      client_identity,
      &server_identity.certificate(),
      "slates-fleet",
      &TransportParameters::local(),
    )
    .unwrap();
    let mut server =
      server_connection(server_identity, &roster, &TransportParameters::local()).unwrap();
    let mut hello = Vec::new();
    client.write_hs(&mut hello);
    server.read_hs(&hello).unwrap();
    let mut flight = Vec::new();
    server.write_hs(&mut flight);
    // `write_hs` writes up to the next encryption-level boundary; drain the whole flight as the
    // endpoint does.
    loop {
      let before = flight.len();
      server.write_hs(&mut flight);
      if flight.len() == before {
        break;
      }
    }
    flight.len()
  }

  /// §4.8 ("certificates provisioned by the operator" — every node holds the whole roster) with §4.9
  /// (a fleet message rides one datagram): a server's handshake flight must not grow with the size of
  /// the roster it admits, or a large fleet's every handshake overflows the receiver's datagram buffer
  /// and faults. Do: measure the server's first flight admitting 1 and 64 clients. Expect: the two are
  /// the same size and inside the receiver's bound. Before 2026-09-14 rustls's default client verifier
  /// listed every admitted certificate's subject in the CertificateRequest (`root_hint_subjects`) —
  /// 64 rosters made a 2.8 KiB flight against a 2 KiB buffer, and the KIND lane's 38-peer node could
  /// never be dialed (`Tls(InvalidMessage(HandshakePayloadTooLarge))` at the dialer, forever).
  #[test]
  fn a_servers_handshake_flight_does_not_grow_with_the_roster_it_admits() {
    let server_identity = self_signed_ed25519("slates-fleet");
    let client_identity = self_signed_ed25519("slates-fleet");
    let one = server_first_flight(&server_identity, &client_identity, 1);
    let many = server_first_flight(&server_identity, &client_identity, 64);
    assert!(
      one <= FLIGHT_BOUND,
      "a one-peer roster's server flight fits the flight bound: {one} > {FLIGHT_BOUND}"
    );
    assert_eq!(
      many, one,
      "a 64-peer roster's server flight ({many} bytes) is the one-peer flight's size ({one} bytes): the \
       roster is not carried in the handshake"
    );
  }

  /// The riskiest, most-blind piece of the `Connection`, probed in isolation: the handshake's 1-RTT
  /// packet keys actually protect and unprotect a payload. Drives the handshake capturing each side's
  /// `OneRtt` keys, then encrypts a payload with the client's local packet key and decrypts it with
  /// the server's remote packet key — the record protection the `Connection` will wrap frames in.
  #[test]
  fn the_handshake_yields_working_packet_keys() {
    use rustls::quic::KeyChange;

    let identity = self_signed("slates-node");
    let (mut client, mut server) = connect(&identity, &identity, "slates-node").unwrap();
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
    let (mut client, mut server) = connect(&identity, &identity, "slates-node").unwrap();
    drive(&mut client, &mut server).unwrap();
    assert!(!client.is_handshaking(), "the client handshake completed");
    assert!(!server.is_handshaking(), "the server handshake completed");
    // The negotiated protocol is TLS 1.3.
    assert_eq!(
      client.protocol_version(),
      Some(rustls::ProtocolVersion::TLSv1_3)
    );
  }

  /// §4.10a (RFC 9000 §7.4): the parameters a peer presents cross the authenticated handshake unchanged,
  /// and a peer speaking another dialect is refused by the decode every endpoint runs when the handshake
  /// completes (`Endpoint::connection_id`) — not mis-read later. A real client presents raw parameters for
  /// dialect 9; the server receives exactly those bytes and refuses them typed, and its own parameters,
  /// read by the client, decode as this build's.
  #[test]
  fn a_peer_of_another_dialect_is_refused_at_the_handshake() {
    let identity = self_signed("slates-node");
    let foreign: Vec<u8> = vec![0x01, 0x00, 0x04, 0x00, 0x09, 0x00, 0x00, 0x00];
    let cfg = client_config(identity.certificate(), &identity).unwrap();
    let server_name = ServerName::try_from("slates-node".to_owned()).unwrap();
    // structural: allow — D-8 exception 2: rustls's `ClientConnection::new` takes `Arc` by signature.
    let mut client =
      ClientConnection::new(Arc::new(cfg), Version::V1, server_name, foreign.clone()).unwrap();
    let mut server = server_connection(
      &identity,
      std::slice::from_ref(&identity.certificate()),
      &TransportParameters::local(),
    )
    .unwrap();
    drive(&mut client, &mut server).unwrap();
    assert_eq!(
      server.quic_transport_parameters(),
      Some(foreign.as_slice()),
      "the client's parameters crossed the handshake unchanged"
    );
    assert_eq!(
      TransportParameters::decode(server.quic_transport_parameters()),
      Err(crate::params::ParamsError::Dialect { version: 9 }),
      "another dialect is refused typed"
    );
    assert_eq!(
      TransportParameters::decode(client.quic_transport_parameters()),
      Ok(TransportParameters::local()),
      "this build's parameters decode as declared"
    );
  }

  /// A client that pins the WRONG certificate rejects the server — the handshake is authenticated,
  /// not permissive.
  #[test]
  fn a_wrong_pin_is_rejected() {
    let server_identity = self_signed("slates-node");
    let other = self_signed("slates-node");
    // The client pins `other`'s cert but talks to the real server.
    let (mut client, mut server) = connect_with(
      &server_identity,
      &server_identity,
      &other.certificate(),
      "slates-node",
    )
    .unwrap();
    let outcome = drive(&mut client, &mut server);
    // Either the drive errors, or the client never completes — never a silent accept of a wrong pin.
    assert!(
      outcome.is_err() || client.is_handshaking(),
      "a wrong-pinned certificate must not authenticate"
    );
  }
}
