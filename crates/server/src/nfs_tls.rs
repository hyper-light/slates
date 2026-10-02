//! The network export's RPC-with-TLS session (RFC 9289; §4.6 "Kubernetes publication without privilege",
//! AUD-29-75): the network export's listener and, under it, rustls's server connection driven sans-io
//! beneath the RPC record framing. A connection must open with an `AUTH_TLS` probe (§4.1), which is
//! answered `STARTTLS`; anything else is refused and the connection closed, because this listener serves
//! only RPC-with-TLS — over cleartext a mount capability would be a bearer secret on the network. Once
//! the mutual handshake completes, each call is served exactly as the loopback listener serves it
//! ([`crate::nfs::reply_to_message`]), authorized by its mount capability and nothing else. Every rule is
//! tested by driving a real rustls client against a real socket.
//!
//! The rules (RFC 9289 §5, §5.2.1), each enforced, never left to configuration:
//!
//! - **TLS 1.3 only** ("MUST NOT negotiate TLS versions prior to 1.3").
//! - **Mutual authentication**: the client must present a certificate whose path verifies to one of the
//!   configured authorities (the fleet's CA, §4.13); a client without one fails the handshake.
//! - **ALPN `sunrpc`**: the server answers with only that identifier, and a client that offered none is
//!   refused after the handshake, so a session that is not RPC-with-TLS never carries a call.
//! - **No resumption tickets**: a session is one connection's.
//!
//! No `Arc` lives here (R2, D-8): the connection itself (TLS 1.3, the client verifier over the operator's
//! authorities, ALPN `sunrpc`, no tickets) is built per connection by the transport crate
//! (`slates_transport::handshake::rpc_tls_connection`), which holds rustls's one signature-mandated `Arc`
//! with a single owner and keeps the node's key.

use std::io::{Read, Write};

use rustls::ServerConnection;
use rustls::pki_types::CertificateDer;
use slates_bridge_nfs::rpc::{RecordReader, write_record};
use slates_bridge_nfs::rpc_tls::{
  ALPN_SUNRPC, Probe, bad_credential_reply, carries_auth_tls, classify, starttls_reply,
  too_weak_reply,
};
use slates_rt::shard::Kept;
use slates_rt::tcp::{TcpListener, TcpStream};
use slates_rt::{futures, registry};
use slates_transport::handshake::{Identity, rpc_tls_connection};

/// The status refusal counts the network export keeps (every closed connection is counted by its reason,
/// never silent; banned item 9). Format: refusal names in the daemon's status report.
const SPAWN_REFUSED: &str = "nfs.tls.serve_spawn";
/// A connection that opened with a cleartext call: answered `AUTH_TOOWEAK` and closed.
const CLEARTEXT_REFUSED: &str = "nfs.tls.cleartext";
/// A connection whose first call misused `AUTH_TLS`: answered `AUTH_BADCRED` and closed.
const BAD_PROBE: &str = "nfs.tls.bad_probe";
/// Bytes after the probe before its reply was read (RFC 9289 §5.1.1: "MUST discard ... SHOULD drop").
const SPURIOUS: &str = "nfs.tls.spurious";
/// A connection the node could not build a session for (no operator authority, or the identity gone).
const NO_SESSION: &str = "nfs.tls.no_session";
/// A session the peer broke: a failed handshake, an unverified client, no `sunrpc`, an overrun.
const SESSION_REFUSED: &str = "nfs.tls.session";
/// A call carrying `AUTH_TLS` inside an established session: answered `AUTH_BADCRED`.
const AUTH_TLS_INSIDE: &str = "nfs.tls.auth_tls_inside";

/// Format: the largest plaintext one TLS 1.3 record carries (RFC 8446 §5.1: "MUST NOT exceed 2^14 bytes").
const TLS_RECORD_PLAINTEXT: usize = 1 << 14;
/// Format: the bytes of an RPC record marker (RFC 5531 §11: a four-byte fragment header).
const RECORD_MARKER_BYTES: usize = 4;
/// Derived: one RPC message at the codec's cap with its marker, plus one TLS record that can complete
/// before the reader drains — the most plaintext a reader that takes each whole message can be left
/// holding. A peer that sends more before the reader takes it is refused, so the buffer is bounded
/// whatever the peer sends (banned item 8). Anchors: `slates_bridge_nfs::rpc::MAX_MESSAGE`, RFC 8446 §5.1.
const MAX_PENDING_PLAINTEXT: usize =
  slates_bridge_nfs::rpc::MAX_MESSAGE + RECORD_MARKER_BYTES + TLS_RECORD_PLAINTEXT;

/// Why a session refused, or failed to build.
#[derive(Debug)]
pub enum TlsRefusal {
  /// rustls's own buffers refused a read or write (an I/O error inside the session, never the socket's).
  Io(String),
  /// The peer broke TLS: a failed handshake, a bad record, an unverified certificate.
  Handshake(rustls::Error),
  /// The handshake completed without the `sunrpc` protocol (RFC 9289 §5: the client "MUST include" it).
  NotSunrpc,
  /// The peer sent more plaintext than one message before it was read.
  PlaintextOverrun,
}

impl From<rustls::Error> for TlsRefusal {
  fn from(error: rustls::Error) -> TlsRefusal {
    TlsRefusal::Handshake(error)
  }
}

/// One connection's TLS session, after its `AUTH_TLS` probe was answered `STARTTLS`.
pub struct TlsSession {
  connection: ServerConnection,
  /// Whether the completed handshake's protocol was checked (once, when the handshake ends).
  protocol_checked: bool,
}

impl TlsSession {
  /// A session over `connection`, waiting for the client's `ClientHello`.
  pub fn new(connection: ServerConnection) -> TlsSession {
    TlsSession {
      connection,
      protocol_checked: false,
    }
  }

  /// Whether the handshake is still running (no call may be read until it ends).
  pub fn is_handshaking(&self) -> bool {
    self.connection.is_handshaking()
  }

  /// The client's certificate chain, once the handshake verified it: the RPC client's identity
  /// (RFC 9289 §5.2.1: "the tuple (serial number of the presented certificate; Issuer) uniquely
  /// identifies the RPC client").
  pub fn client_certificates(&self) -> Option<&[CertificateDer<'static>]> {
    self.connection.peer_certificates()
  }

  /// Takes `ciphertext` from the socket and appends whatever plaintext it completes to `plaintext`. A
  /// refusal means the connection must be closed after [`TlsSession::transmit`] sends any alert.
  pub fn receive(&mut self, ciphertext: &[u8], plaintext: &mut Vec<u8>) -> Result<(), TlsRefusal> {
    let mut rest = ciphertext;
    while !rest.is_empty() {
      // `read_tls` takes as much as its buffer holds; the loop hands it the rest after processing.
      let taken = self
        .connection
        .read_tls(&mut rest)
        .map_err(|e| TlsRefusal::Io(format!("reading TLS records: {e}")))?;
      self.connection.process_new_packets()?;
      self.check_protocol()?;
      self.drain_plaintext(plaintext)?;
      if taken == 0 {
        break;
      }
    }
    Ok(())
  }

  /// Encrypts one framed reply for the socket (sent by the next [`TlsSession::transmit`]).
  pub fn send(&mut self, framed_reply: &[u8]) -> Result<(), TlsRefusal> {
    self
      .connection
      .writer()
      .write_all(framed_reply)
      .map_err(|e| TlsRefusal::Io(format!("writing a reply into the session: {e}")))
  }

  /// Appends to `out` every TLS byte the session has to send: handshake flights, alerts, sealed replies.
  pub fn transmit(&mut self, out: &mut Vec<u8>) -> Result<(), TlsRefusal> {
    while self.connection.wants_write() {
      self
        .connection
        .write_tls(out)
        .map_err(|e| TlsRefusal::Io(format!("sealing TLS records: {e}")))?;
    }
    Ok(())
  }

  /// Refuses a completed handshake that did not agree on `sunrpc` (checked once, as it completes).
  fn check_protocol(&mut self) -> Result<(), TlsRefusal> {
    if self.protocol_checked || self.connection.is_handshaking() {
      return Ok(());
    }
    self.protocol_checked = true;
    if self.connection.alpn_protocol() == Some(ALPN_SUNRPC) {
      Ok(())
    } else {
      Err(TlsRefusal::NotSunrpc)
    }
  }

  /// Moves the session's decrypted bytes to `plaintext`, refusing a peer that outruns the reader.
  fn drain_plaintext(&mut self, plaintext: &mut Vec<u8>) -> Result<(), TlsRefusal> {
    let mut chunk = [0u8; TLS_RECORD_PLAINTEXT];
    loop {
      match self.connection.reader().read(&mut chunk) {
        Ok(0) => return Ok(()),
        Ok(count) => {
          plaintext.extend_from_slice(chunk.get(..count).unwrap_or_default());
          if plaintext.len() > MAX_PENDING_PLAINTEXT {
            return Err(TlsRefusal::PlaintextOverrun);
          }
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(e) => return Err(TlsRefusal::Io(format!("reading the session: {e}"))),
      }
    }
  }
}

/// Serves the network export's listener on this shard until it fails: each accepted connection runs
/// [`serve_connection`] as its own detached task, its session built from `identity` (kept on this shard,
/// never copied: the key stays in the transport crate). Detached as the loopback listener's connections are;
/// the runtime's shutdown cancels them.
pub async fn serve_network(listener: TcpListener, port: u16, identity: Kept<Identity>) {
  while let Ok(stream) = listener.accept().await {
    let connect = move || {
      identity
        .with(|identity| rpc_tls_connection(identity, ALPN_SUNRPC).ok())
        .flatten()
    };
    match futures::spawn(serve_connection(stream, port, connect)) {
      Ok(task) => {
        let _ = futures::detach(task);
      }
      Err(_) => {
        crate::fleet::count_refusal(SPAWN_REFUSED);
      }
    }
    futures::yield_now().await;
  }
}

/// One network connection: the probe gate, then the TLS session and its calls. `connect` builds the session's
/// rustls connection once the probe is answered (`None`: refused, counted). Returns when the peer closes, a
/// rule refuses the connection, or the socket fails; every refusal is counted.
pub async fn serve_connection(
  stream: TcpStream,
  port: u16,
  connect: impl FnOnce() -> Option<ServerConnection>,
) {
  let mut chunk = vec![0u8; crate::nfs::RECORD_CHUNK];
  if !answer_probe(&stream, &mut chunk).await {
    return;
  }
  let Some(connection) = connect() else {
    crate::fleet::count_refusal(NO_SESSION);
    return;
  };
  let mut session = TlsSession::new(connection);
  if let Err(reason) = serve_session(&stream, port, &mut session, &mut chunk).await {
    crate::fleet::count_refusal(reason);
    // Send what the session has left (a TLS alert naming the failure), then close.
    let mut alert = Vec::new();
    if session.transmit(&mut alert).is_ok() && !alert.is_empty() {
      let _ = stream.write_all(&alert).await;
    }
  }
}

/// Reads the connection's first call in cleartext and answers it: `true` once a well-formed `AUTH_TLS` probe was
/// answered `STARTTLS` and nothing followed it unasked; otherwise the refusal is answered and counted, and the
/// connection is to be closed.
async fn answer_probe(stream: &TcpStream, chunk: &mut [u8]) -> bool {
  let mut cleartext = Vec::new();
  let mut records = RecordReader::default();
  let (message, consumed) = loop {
    match records.read(&cleartext) {
      Ok((Some(message), consumed)) => break (message, consumed),
      Ok((None, consumed)) => {
        cleartext.drain(..consumed);
      }
      Err(_) => return false,
    }
    match stream.read(chunk).await {
      Ok(0) | Err(_) => return false,
      Ok(count) => cleartext.extend_from_slice(chunk.get(..count).unwrap_or_default()),
    }
  };
  let (reply, accepted) = match classify(&message) {
    Ok(Probe::StartTls { xid }) => (starttls_reply(xid), true),
    Ok(Probe::BadCredential { xid }) => {
      crate::fleet::count_refusal(BAD_PROBE);
      (bad_credential_reply(xid), false)
    }
    Ok(Probe::Cleartext { xid }) => {
      crate::fleet::count_refusal(CLEARTEXT_REFUSED);
      (too_weak_reply(xid), false)
    }
    Err(_) => return false,
  };
  if stream.write_all(&write_record(&reply)).await.is_err() {
    return false;
  }
  // A client sends its `ClientHello` only after reading the `STARTTLS` reply, so bytes already behind the probe
  // were sent unasked: discarded, and the connection dropped (RFC 9289 §5.1.1).
  if accepted && consumed < cleartext.len() {
    crate::fleet::count_refusal(SPURIOUS);
    return false;
  }
  accepted
}

/// Runs the handshake and then serves each call inside the session until the peer closes. A refusal names the
/// counter it is recorded under.
async fn serve_session(
  stream: &TcpStream,
  port: u16,
  session: &mut TlsSession,
  chunk: &mut [u8],
) -> Result<(), &'static str> {
  let this = registry::current_shard().unwrap_or(0);
  let mut plaintext = Vec::new();
  let mut records = RecordReader::default();
  let mut outgoing = Vec::new();
  loop {
    let parsed = match records.read(&plaintext) {
      Ok(parsed) => parsed,
      Err(_) => return Err(SESSION_REFUSED),
    };
    match parsed {
      (Some(message), consumed) => {
        let reply = if carries_auth_tls(&message) == Some(true) {
          crate::fleet::count_refusal(AUTH_TLS_INSIDE);
          bad_credential_reply(xid_of(&message))
        } else {
          crate::nfs::reply_to_message(this, &message, port).await
        };
        session
          .send(&write_record(&reply))
          .map_err(|_| SESSION_REFUSED)?;
        plaintext.drain(..consumed);
        // One call per turn, as the loopback connection serves (§4.3, D-18).
        futures::yield_now().await;
      }
      (None, consumed) => {
        plaintext.drain(..consumed);
        let count = match stream.read(chunk).await {
          Ok(0) | Err(_) => return Ok(()),
          Ok(count) => count,
        };
        session
          .receive(chunk.get(..count).unwrap_or_default(), &mut plaintext)
          .map_err(|_| SESSION_REFUSED)?;
      }
    }
    outgoing.clear();
    session
      .transmit(&mut outgoing)
      .map_err(|_| SESSION_REFUSED)?;
    if !outgoing.is_empty() && stream.write_all(&outgoing).await.is_err() {
      return Ok(());
    }
  }
}

/// The transaction id of a call message (its first word), or 0 when the message is shorter than one word.
fn xid_of(message: &[u8]) -> u32 {
  message
    .get(..size_of::<u32>())
    .and_then(|word| <[u8; 4]>::try_from(word).ok())
    .map_or(0, u32::from_be_bytes)
}
