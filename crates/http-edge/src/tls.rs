//! HTTPS for the edge (§4.17.2 "Transports" item 1: "TLS is rustls over aws-lc-rs"): rustls's server session driven
//! sans-io beneath the HTTP framing, as the network export drives its own (`slates_server::nfs_tls`) and an S3 base its
//! client (`slates_base::s3::tls`): the socket's bytes go in, plaintext comes out, and what the session must send is
//! taken out to be written.
//!
//! **The connection** ([`ServingIdentity::connection`]): the operator's certificate chain and key, the leaf first
//! (RFC 8446 §4.4.2); TLS 1.3 and 1.2, rustls's safe defaults, which every client git's HTTP stack links speaks; ALPN
//! `http/1.1` alone (RFC 7301), so a client that offers ALPN without it is refused in the handshake
//! (`no_application_protocol`) and one that offers none is served; no client certificate, since the request's
//! credentials prove the client (§4.17.2 "Access"); no resumption, so a session is one connection's and no key outlives
//! it in a ticket or a cache. The provider is rustls's aws-lc-rs one with its standard groups, X25519MLKEM768 first:
//! the edge's peers are git clients, not slates nodes, so the fleet's post-quantum-only groups would refuse most of them.
//!
//! **The session** ([`HttpsSession`]): ciphertext in, plaintext out; plaintext in, sealed records out. rustls holds at
//! most 64 KiB of sealed records unsent and takes no more plaintext past that, so a reply is handed over in the pieces it
//! takes, the records each fills moved out between them: the lesson the network export learned (its `send`, G-mount step
//! 5).
//!
//! rustls's configuration crosses its API as `Arc` (D-8 exception 2): one per connection, made here, held by that
//! connection alone; the key is cloned into it and nothing is shared between connections.
#![allow(clippy::disallowed_types)]

use std::io::{Read as _, Write as _};

use rustls::ServerConnection;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Format: the protocol ALPN agrees on (RFC 7301 §3; the IANA registry's `http/1.1`).
pub const ALPN_HTTP_1_1: &[u8] = b"http/1.1";

/// Why TLS refused, or could not be set up.
#[derive(Clone, Debug, PartialEq)]
pub enum TlsRefused {
  /// The identity cannot make a server configuration: no certificate, or a key rustls cannot use.
  Setup(rustls::Error),
  /// The peer broke TLS: a failed handshake, a bad record, a protocol ALPN cannot agree on.
  Session(rustls::Error),
  /// rustls's own buffers refused a read or a write (an error inside the session, never the socket's).
  Io(String),
}

/// The certificate chain and key the edge presents: the operator's, issued for the names clients reach it by.
pub struct ServingIdentity {
  /// The leaf first, then its intermediates.
  chain: Vec<CertificateDer<'static>>,
  /// The leaf's private key.
  key: PrivateKeyDer<'static>,
}

impl ServingIdentity {
  /// An identity presenting `chain` with `key`, refused when they cannot make a server configuration.
  pub fn new(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
  ) -> Result<ServingIdentity, TlsRefused> {
    let identity = ServingIdentity { chain, key };
    // Refused now rather than at the first client: a chain and key rustls cannot use make no configuration.
    identity.config()?;
    Ok(identity)
  }

  /// A server connection for one client, waiting for its `ClientHello`.
  pub fn connection(&self) -> Result<ServerConnection, TlsRefused> {
    let config = self.config()?;
    // structural: allow — D-8 exception 2: `ServerConnection::new` takes `Arc` by signature; one owner, this connection.
    ServerConnection::new(std::sync::Arc::new(config)).map_err(TlsRefused::Setup)
  }

  /// One connection's configuration: TLS 1.3 and 1.2, ALPN `http/1.1`, no client certificate, no resumption.
  fn config(&self) -> Result<rustls::ServerConfig, TlsRefused> {
    // structural: allow — D-8 exception 2: `builder_with_provider` takes the provider as `Arc` by signature.
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
      .with_safe_default_protocol_versions()
      .map_err(TlsRefused::Setup)?
      .with_no_client_auth()
      .with_single_cert(self.chain.clone(), self.key.clone_key())
      .map_err(TlsRefused::Setup)?;
    config.alpn_protocols = vec![ALPN_HTTP_1_1.to_vec()];
    config.send_tls13_tickets = 0;
    // structural: allow — D-8 exception 2: rustls keeps its session store as `Arc` by type; none is kept here.
    config.session_storage = std::sync::Arc::new(rustls::server::NoServerSessionStorage {});
    Ok(config)
  }
}

/// Format: the most plaintext one TLS record carries (RFC 8446 §5.1, RFC 5246 §6.2.1: 2^14 bytes), the session's read
/// buffer.
const TLS_RECORD_PLAINTEXT: usize = 1 << 14;

/// One connection's TLS session.
pub struct HttpsSession {
  /// rustls's connection.
  connection: ServerConnection,
  /// Records sealed and not yet taken by [`HttpsSession::transmit`].
  sealed: Vec<u8>,
  /// Whether the peer's `close_notify` came.
  peer_closed: bool,
}

impl HttpsSession {
  /// A session over `connection`.
  pub fn new(connection: ServerConnection) -> HttpsSession {
    HttpsSession {
      connection,
      sealed: Vec::new(),
      peer_closed: false,
    }
  }

  /// Whether the handshake is still running: no request may be read until it ends.
  pub fn is_handshaking(&self) -> bool {
    self.connection.is_handshaking()
  }

  /// Whether the peer sent `close_notify`: its side of the connection has ended cleanly.
  pub fn closed_by_peer(&self) -> bool {
    self.peer_closed
  }

  /// Takes `ciphertext` from the socket and appends the plaintext it completes to `plaintext`. A refusal means the
  /// connection ends after [`HttpsSession::transmit`] sends the alert rustls queued.
  pub fn receive(&mut self, ciphertext: &[u8], plaintext: &mut Vec<u8>) -> Result<(), TlsRefused> {
    let mut rest = ciphertext;
    while !rest.is_empty() {
      // `read_tls` takes what its buffer holds; the loop hands it the rest once that is processed and read.
      let taken = self
        .connection
        .read_tls(&mut rest)
        .map_err(|error| TlsRefused::Io(error.to_string()))?;
      self
        .connection
        .process_new_packets()
        .map_err(TlsRefused::Session)?;
      self.read_plaintext(plaintext)?;
      if taken == 0 {
        break;
      }
    }
    Ok(())
  }

  /// Moves the session's decrypted bytes to `plaintext`, noting the peer's `close_notify`.
  fn read_plaintext(&mut self, plaintext: &mut Vec<u8>) -> Result<(), TlsRefused> {
    let mut chunk = [0u8; TLS_RECORD_PLAINTEXT];
    loop {
      match self.connection.reader().read(&mut chunk) {
        Ok(0) => {
          self.peer_closed = true;
          return Ok(());
        }
        Ok(count) => plaintext.extend_from_slice(chunk.get(..count).unwrap_or_default()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(error) => return Err(TlsRefused::Io(error.to_string())),
      }
    }
  }

  /// Seals `plaintext` for the socket (sent by the next [`HttpsSession::transmit`]), handed over in the pieces rustls
  /// takes, the records each fills moved out between them.
  pub fn send(&mut self, plaintext: &[u8]) -> Result<(), TlsRefused> {
    let mut rest = plaintext;
    while !rest.is_empty() {
      let taken = self
        .connection
        .writer()
        .write(rest)
        .map_err(|error| TlsRefused::Io(error.to_string()))?;
      rest = rest.get(taken..).unwrap_or_default();
      let sealed_before = self.sealed.len();
      self.seal()?;
      if taken == 0 && self.sealed.len() == sealed_before {
        return Err(TlsRefused::Io(
          "the session took none of the plaintext and sealed nothing to make room".to_owned(),
        ));
      }
    }
    Ok(())
  }

  /// Seals `close_notify` (RFC 8446 §6.1): the server's side ends cleanly.
  pub fn close(&mut self) -> Result<(), TlsRefused> {
    self.connection.send_close_notify();
    self.seal()
  }

  /// Appends to `out` every byte the session has to send: handshake flights, alerts and sealed records.
  pub fn transmit(&mut self, out: &mut Vec<u8>) -> Result<(), TlsRefused> {
    out.append(&mut self.sealed);
    while self.connection.wants_write() {
      self
        .connection
        .write_tls(out)
        .map_err(|error| TlsRefused::Io(error.to_string()))?;
    }
    Ok(())
  }

  /// Moves the records rustls holds into [`HttpsSession::sealed`], which frees its buffer for more plaintext.
  fn seal(&mut self) -> Result<(), TlsRefused> {
    while self.connection.wants_write() {
      self
        .connection
        .write_tls(&mut self.sealed)
        .map_err(|error| TlsRefused::Io(error.to_string()))?;
    }
    Ok(())
  }
}

#[cfg(test)]
#[path = "tests/tls.rs"]
pub(crate) mod tests;
