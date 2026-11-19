//! TLS for an S3 base's requests (A-471; §4.15 "An S3 base"): rustls's client over aws-lc-rs (the workspace's one
//! cryptographic library, as the transport's handshake), driven sans-io beneath the HTTP framing, as the network export
//! drives its server side (`slates_server::nfs_tls`): the socket's bytes go in, plaintext comes out, and what the
//! session must send is taken out to be written.
//!
//! The server's certificate is verified against the authorities the endpoint names (an S3-compatible store's own CA, or
//! a test's). The platform's trust roots, which an endpoint on the public internet needs, are part 3's to choose (a
//! source of roots is a dependency decision: compiled-in roots or the operating system's store), so an endpoint names
//! its authorities until then. TLS 1.2 and 1.3, rustls' safe defaults; S3 serves both.
//!
//! rustls' configuration crosses its API as `Arc` (D-8 exception 2): one per endpoint, held by its client, its one
//! owner; nothing else here shares anything.
#![allow(clippy::disallowed_types)]

use std::io::{Read, Write};

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};

/// Format: the largest plaintext one TLS record carries (RFC 8446 §5.1, RFC 5246 §6.2.1: 2^14 bytes).
pub const TLS_RECORD_PLAINTEXT: usize = 1 << 14;

/// A client configuration as rustls takes it.
// structural: allow — D-8 exception 2: rustls takes its configuration as `Arc`; one owner, the endpoint's client.
pub type Config = std::sync::Arc<ClientConfig>;

/// Why a session refused, or could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsError {
  /// The endpoint's authorities or host name could not make a configuration.
  Setup(String),
  /// The peer broke TLS: a failed handshake, a bad record, an unverified certificate.
  Session(String),
  /// More plaintext arrived than the reader may hold.
  Overrun,
}

/// A client configuration verifying servers against `authorities` (DER certificates), over aws-lc-rs.
pub fn client_config(authorities: &[CertificateDer<'static>]) -> Result<Config, TlsError> {
  let mut roots = RootCertStore::empty();
  for authority in authorities {
    roots
      .add(authority.clone())
      .map_err(|error| TlsError::Setup(error.to_string()))?;
  }
  // structural: allow — D-8 exception 2: `builder_with_provider` takes the provider as `Arc` by signature.
  let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
  let config = ClientConfig::builder_with_provider(provider)
    .with_safe_default_protocol_versions()
    .map_err(|error| TlsError::Setup(error.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
  // structural: allow — D-8 exception 2: `ClientConnection::new` takes the configuration as `Arc` by signature.
  Ok(std::sync::Arc::new(config))
}

/// One connection's TLS session, client side.
pub struct TlsClient {
  connection: ClientConnection,
}

impl TlsClient {
  /// A session to `host` (its certificate must name it) under `config`, its `ClientHello` ready to send.
  pub fn new(config: Config, host: &str) -> Result<TlsClient, TlsError> {
    let name = ServerName::try_from(host.to_owned())
      .map_err(|error| TlsError::Setup(format!("{host}: {error}")))?;
    let connection =
      ClientConnection::new(config, name).map_err(|error| TlsError::Setup(error.to_string()))?;
    Ok(TlsClient { connection })
  }

  /// Whether the handshake is still running.
  pub fn is_handshaking(&self) -> bool {
    self.connection.is_handshaking()
  }

  /// Takes `ciphertext` from the socket and appends the plaintext it completes to `plaintext`, refusing plaintext past
  /// `cap` bytes held.
  pub fn receive(
    &mut self,
    ciphertext: &[u8],
    plaintext: &mut Vec<u8>,
    cap: usize,
  ) -> Result<(), TlsError> {
    let mut rest = ciphertext;
    while !rest.is_empty() {
      let taken = self
        .connection
        .read_tls(&mut rest)
        .map_err(|error| TlsError::Session(format!("reading TLS records: {error}")))?;
      self
        .connection
        .process_new_packets()
        .map_err(|error| TlsError::Session(error.to_string()))?;
      self.drain(plaintext, cap)?;
      if taken == 0 {
        break;
      }
    }
    Ok(())
  }

  /// Moves the session's decrypted bytes to `plaintext`, refusing more than `cap` held.
  fn drain(&mut self, plaintext: &mut Vec<u8>, cap: usize) -> Result<(), TlsError> {
    let mut record = [0u8; TLS_RECORD_PLAINTEXT];
    loop {
      match self.connection.reader().read(&mut record) {
        Ok(0) => return Ok(()),
        Ok(count) => {
          if plaintext.len().saturating_add(count) > cap {
            return Err(TlsError::Overrun);
          }
          plaintext.extend_from_slice(record.get(..count).unwrap_or_default());
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(error) => return Err(TlsError::Session(format!("reading the session: {error}"))),
      }
    }
  }

  /// Seals `plaintext` for the socket, appending every TLS byte the session has to send (handshake flights, alerts, the
  /// sealed bytes) to `out`.
  pub fn send(&mut self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), TlsError> {
    let mut rest = plaintext;
    while !rest.is_empty() {
      let taken = self
        .connection
        .writer()
        .write(rest)
        .map_err(|error| TlsError::Session(format!("writing into the session: {error}")))?;
      rest = rest.get(taken..).unwrap_or_default();
      let before = out.len();
      self.transmit(out)?;
      if taken == 0 && out.len() == before {
        return Err(TlsError::Session(
          "the session took none of the request and sealed nothing to make room".to_owned(),
        ));
      }
    }
    self.transmit(out)
  }

  /// Appends to `out` every TLS byte the session has to send.
  pub fn transmit(&mut self, out: &mut Vec<u8>) -> Result<(), TlsError> {
    while self.connection.wants_write() {
      self
        .connection
        .write_tls(out)
        .map_err(|error| TlsError::Session(format!("sealing TLS records: {error}")))?;
    }
    Ok(())
  }
}
