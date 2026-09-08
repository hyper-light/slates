//! The session-plane connection endpoint (§4.10a §8) — the I/O edge that carries a *live* session over
//! the runtime's UDP socket: it drives the `rustls::quic` handshake to the 1-RTT keys, then pumps the
//! sans-io [`Connection`] driver — protecting each packet it wants to send and feeding it each packet
//! that arrives — so a stream is delivered **reliably** over the wire. This is the session-plane
//! analogue of the control plane's `plane.rs`.
//!
//! What it does: the handshake over UDP; full 1-RTT packet protection to RFC 9001 shape (an RFC 9000
//! short header (§17.3) carrying a truncated packet number from `crate::packet_number`, payload AEAD
//! with that header as associated data, and **header protection** (§5.4) masking the first byte and the
//! packet-number field); and reliable single-stream delivery driven by the [`Connection`] —
//! acknowledgements flow, and a transfer is complete only when every packet is acknowledged. Over the
//! lossless simulation fabric no retransmission is needed; the loss-recovery and probe paths are proven
//! by the `connection` oracle, and driving the probe from a real timeout is owed with the runtime timer.
//! Remaining connection work: flow-credit enforcement, congestion control, connection IDs, and
//! multiplexing many streams. The `Arc` here is rustls's config (D-8 exception 2), in `crate::handshake`.

use rustix::net::SocketAddrV4;
use rustls::quic::{ClientConnection, KeyChange, Keys, ServerConnection};
use slates_rt::udp::UdpSocket;

use crate::connection::{Connection, initial_receive_window};
use crate::handshake::{HandshakeError, Identity, client_connection, server_connection};
use crate::packet_number::{MAX_PACKET_NUMBER_BYTES, decode_packet_number, encode_packet_number};
use crate::session::{Frame, decode_frames, encode_frames};

/// Format: RFC 9000 §17.3 — bit 6 of a short-header first byte, always 1 ("fixed bit"); a packet with
/// it clear is not a valid short header.
const FIXED_BIT: u8 = 0x40;
/// Format: RFC 9000 §17.3, §17.3.1 — bits 3-4 of a short-header first byte are reserved and MUST be 0
/// once header protection is removed; a non-zero value there is a protocol error.
const SHORT_HEADER_RESERVED_MASK: u8 = 0x18;
/// Format: RFC 9000 §17.3 — bits 0-1 of a short-header first byte carry the packet-number length minus
/// one, so a stored 0..=3 means a 1..=4 byte field.
const PACKET_NUMBER_LENGTH_MASK: u8 = 0x03;
/// Shape: this slice binds one UDP socket to one peer, so a packet needs no connection ID to tell
/// connections apart; the destination connection ID is therefore zero-length. A non-zero ID (for
/// connection migration, or several connections on one socket) is owed.
const CONNECTION_ID_BYTES: usize = 0;
/// The offset of the packet-number field: past the single first byte and the connection ID.
/// Format: RFC 9000 §17.3 short-header layout.
const PACKET_NUMBER_OFFSET: usize = 1 + CONNECTION_ID_BYTES;
/// The offset at which header protection samples the ciphertext.
/// Format: RFC 9001 §5.4.2 — the sample begins four bytes into the packet-number field (as if the
/// number were the maximum four bytes), so both ends sample the same bytes whatever the field's real
/// length. Derived from the packet-number offset and the maximum field width.
const HEADER_PROTECTION_SAMPLE_OFFSET: usize =
  PACKET_NUMBER_OFFSET + MAX_PACKET_NUMBER_BYTES as usize;

/// The handshake turn ceiling: the most drain-send-receive turns `establish` takes before it refuses
/// a stuck handshake with `NotReady`, so the loop is bounded (banned item 8 — no unbounded loop). It
/// caps only the failure path (a peer that never completes), so its exact value is not performance-
/// tuned — only comfortably larger than the four turns a healthy handshake needs.
/// Shape: TLS 1.3 over QUIC is a three-flight handshake (ClientHello; ServerHello..Finished; client
/// Finished) — at most one turn per flight plus a terminal drain, four turns; this ceiling sits well
/// above that to absorb a retransmitted, split, or coalesced flight.
const HANDSHAKE_TURN_CEILING: usize = 16;

/// A refusal on the endpoint.
#[derive(Debug)]
pub enum EndpointError {
  /// The handshake failed to build or complete.
  Handshake(HandshakeError),
  /// The TLS/crypto layer reported an error (handshake read or packet protection).
  Tls(rustls::Error),
  /// The UDP socket refused.
  Io(slates_rt::error::RtError),
  /// A packet arrived before the handshake produced keys, or was too short to sample for header
  /// protection.
  NotReady,
  /// A received short header was malformed once unprotected (fixed bit clear, or a reserved bit set).
  Header,
  /// The frames inside a packet did not decode.
  Frames(crate::session::SessionError),
}

impl From<rustls::Error> for EndpointError {
  fn from(e: rustls::Error) -> Self {
    EndpointError::Tls(e)
  }
}
impl From<slates_rt::error::RtError> for EndpointError {
  fn from(e: slates_rt::error::RtError) -> Self {
    EndpointError::Io(e)
  }
}

/// The two QUIC connection kinds behind one interface, so the endpoint drives either.
enum Quic {
  Client(ClientConnection),
  Server(ServerConnection),
}

impl Quic {
  fn write_hs(&mut self, out: &mut Vec<u8>) -> Option<KeyChange> {
    match self {
      Quic::Client(c) => c.write_hs(out),
      Quic::Server(s) => s.write_hs(out),
    }
  }
  fn read_hs(&mut self, data: &[u8]) -> Result<(), rustls::Error> {
    match self {
      Quic::Client(c) => c.read_hs(data),
      Quic::Server(s) => s.read_hs(data),
    }
  }
  fn is_handshaking(&self) -> bool {
    match self {
      Quic::Client(c) => c.is_handshaking(),
      Quic::Server(s) => s.is_handshaking(),
    }
  }
}

/// One end of a session: the UDP socket, the peer, the QUIC handshake state, the 1-RTT keys once
/// established, and — the crucial part for correctness — **one long-lived [`Connection`]** carrying
/// every exchange, so the packet-number space is continuous and a number is never reused under the
/// 1-RTT keys (RFC 9000 §12.3, RFC 9001 §5.3). `rx_largest` is the persistent packet-number decode
/// cursor; `frame_cap` sizes each frame and the receive window. Completed streams are forgotten after
/// each exchange so a long-lived connection does not accumulate them without bound.
pub struct Endpoint {
  socket: UdpSocket,
  peer: SocketAddrV4,
  quic: Quic,
  keys: Option<Keys>,
  conn: Connection,
  rx_largest: u64,
  frame_cap: usize,
}

impl Endpoint {
  /// The client end: presents its own `identity` (mutual authentication — the peer authenticates this
  /// caller), pins the server's `pinned` certificate, talks to `peer` as `name`, framing at `frame_cap`.
  pub fn client(
    socket: UdpSocket,
    peer: SocketAddrV4,
    identity: &Identity,
    pinned: &rustls::pki_types::CertificateDer<'static>,
    name: &str,
    frame_cap: usize,
  ) -> Result<Endpoint, EndpointError> {
    let client = client_connection(identity, pinned, name).map_err(EndpointError::Handshake)?;
    Ok(Endpoint {
      socket,
      peer,
      quic: Quic::Client(client),
      keys: None,
      conn: Connection::new(initial_receive_window(frame_cap)),
      rx_largest: 0,
      frame_cap,
    })
  }

  /// The server end presenting `identity` and requiring a client certificate found among
  /// `allowed_clients` (mutual authentication — it authenticates its caller), talking to `peer`,
  /// framing at `frame_cap`.
  pub fn server(
    socket: UdpSocket,
    peer: SocketAddrV4,
    identity: &Identity,
    allowed_clients: &[rustls::pki_types::CertificateDer<'static>],
    frame_cap: usize,
  ) -> Result<Endpoint, EndpointError> {
    let server = server_connection(identity, allowed_clients).map_err(EndpointError::Handshake)?;
    Ok(Endpoint {
      socket,
      peer,
      quic: Quic::Server(server),
      keys: None,
      conn: Connection::new(initial_receive_window(frame_cap)),
      rx_largest: 0,
      frame_cap,
    })
  }

  /// The next packet number the connection will assign — its packet-number cursor, monotonic across
  /// every exchange this endpoint carries (a test asserts it never regresses: no reuse under the keys).
  pub fn tx_packet_number(&self) -> u64 {
    self.conn.tx_packet_number()
  }

  /// Drives the handshake to the 1-RTT keys, shuttling CRYPTO bytes over UDP. Each turn *drains* all
  /// pending handshake bytes — `rustls::quic::write_hs` writes only up to the next encryption-level
  /// boundary and must be called repeatedly to flush a whole flight, so a single call would send (say)
  /// only `ServerHello` and then park owing `EncryptedExtensions..Finished`, deadlocking against a peer
  /// that waits for exactly that flight. Having drained and sent, it checks whether the handshake is
  /// done (before blocking on a receive, so a finished peer never hangs waiting for a packet that will
  /// not come), and otherwise receives the peer's next flight.
  pub async fn establish(&mut self) -> Result<(), EndpointError> {
    let mut buf = [0u8; 2048];
    for _ in 0..HANDSHAKE_TURN_CEILING {
      let out = self.drain_handshake();
      if !out.is_empty() {
        self.socket.send_to(&out, self.peer)?;
      }
      if !self.quic.is_handshaking() && self.keys.is_some() {
        return Ok(());
      }
      let (n, _from) = self.socket.recv_from(&mut buf).await?;
      self.quic.read_hs(&buf[..n])?;
    }
    Err(EndpointError::NotReady)
  }

  /// Drains every handshake byte the connection currently has to send, across encryption-level
  /// boundaries, capturing the 1-RTT keys when they arrive. Stops when a `write_hs` call neither
  /// changes keys nor writes more bytes (nothing left at any level).
  fn drain_handshake(&mut self) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
      let before = out.len();
      let change = self.quic.write_hs(&mut out);
      let no_change = change.is_none();
      if let Some(KeyChange::OneRtt { keys, .. }) = change {
        self.keys = Some(keys);
      }
      if no_change && out.len() == before {
        return out;
      }
    }
  }

  /// Flushes every packet the connection currently wants to send: each `poll_transmit` gives a packet
  /// number (from the connection's continuous packet-number space) and its frames, which are protected
  /// under the local 1-RTT keys (the number sized against what the peer has acknowledged) and sent.
  fn flush(&mut self) -> Result<(), EndpointError> {
    let keys = self.keys.as_ref().ok_or(EndpointError::NotReady)?;
    while let Some((pn, frames)) = self.conn.poll_transmit(self.frame_cap) {
      let datagram = protect_packet(keys, pn, self.conn.tx_largest_acked(), &frames)?;
      self.socket.send_to(&datagram, self.peer)?;
    }
    Ok(())
  }

  /// Receives one protected packet, reconstructs its number against the persistent decode cursor
  /// (advancing it), and feeds it to the connection.
  async fn receive_into(&mut self) -> Result<(), EndpointError> {
    let mut buf = [0u8; 2048];
    let (n, _from) = self.socket.recv_from(&mut buf).await?;
    let keys = self.keys.as_ref().ok_or(EndpointError::NotReady)?;
    let (pn, frames) = unprotect_packet(keys, self.rx_largest, &buf[..n])?;
    self.rx_largest = self.rx_largest.max(pn);
    self.conn.handle_incoming(pn, &frames);
    Ok(())
  }

  /// Sends `data` as stream `stream_id` **reliably**: drives a [`Connection`] that frames the data,
  /// sends it, and — receiving the peer's acknowledgements — considers the transfer done only when
  /// every packet has been acknowledged. Over the lossless simulation fabric no retransmission is
  /// needed; the loss-recovery path (and its probe for a lost tail, which over a real network needs a
  /// timeout the runtime's timer will drive) is exercised by the `connection` oracle.
  pub async fn send_stream(&mut self, stream_id: u64, data: &[u8]) -> Result<(), EndpointError> {
    self.conn.open(stream_id, data);
    loop {
      self.flush()?;
      if self.conn.send_complete() {
        self.conn.forget_stream(stream_id);
        return Ok(());
      }
      self.receive_into().await?;
    }
  }

  /// Receives stream `stream_id` **reliably**, acknowledging what arrives and returning its bytes once
  /// the stream's `fin` completes. Drives a [`Connection`]: each received packet is acknowledged (the
  /// acknowledgement flushed before the next receive) so the sender learns of delivery, and the final
  /// acknowledgement is flushed after completion so the sender can finish.
  pub async fn recv_stream(&mut self, stream_id: u64) -> Result<Vec<u8>, EndpointError> {
    let mut received = Vec::new();
    loop {
      self.receive_into().await?;
      self.flush()?;
      received.extend_from_slice(&self.conn.read_stream(stream_id));
      if self.conn.recv_stream_complete(stream_id) {
        self.conn.forget_stream(stream_id);
        return Ok(received);
      }
    }
  }

  /// The client side of a **request/reply** exchange over the session (§4.8 "lookups route by id to
  /// the current owner"): sends `request` reliably as stream `stream_id`, then receives the peer's
  /// reply on the same stream id (the reply travels the other direction), returning its bytes. One
  /// [`Connection`] carries both — the request as this end's send stream, the reply as its receive
  /// stream — so a lost frame either way is recovered. The reply's completion is the exchange's
  /// completion; the final acknowledgement of the reply rides the flush before this returns, so the
  /// peer's [`serve_once`] finishes too. (Records ride the session plane per §4.10a §8; this is the
  /// RPC seam register/placement (slice 5) will use — a to-ratify integration shape.)
  ///
  /// [`serve_once`]: Endpoint::serve_once
  pub async fn request(
    &mut self,
    stream_id: u64,
    request: &[u8],
  ) -> Result<Vec<u8>, EndpointError> {
    self.conn.open(stream_id, request);
    let mut reply = Vec::new();
    loop {
      self.flush()?;
      reply.extend_from_slice(&self.conn.read_stream(stream_id));
      if self.conn.recv_stream_complete(stream_id) {
        self.conn.forget_stream(stream_id);
        return Ok(reply);
      }
      self.receive_into().await?;
    }
  }

  /// The server side of one request/reply exchange: receives a request stream, passes its bytes to
  /// `handler`, and sends the reply back on the same stream id, returning once the reply is
  /// acknowledged. Drives one [`Connection`]: phase one receives and acknowledges the request until its
  /// `fin`; phase two frames the reply and completes when the peer has acknowledged all of it.
  pub async fn serve_once<H>(&mut self, handler: H) -> Result<(), EndpointError>
  where
    H: FnOnce(Vec<u8>) -> Vec<u8>,
  {
    // Phase one: receive the request in full, acknowledging and *draining* as it arrives — draining is
    // what slides the flow-control window forward, so a request larger than one window keeps flowing
    // (without it the credit never grows past the initial window and the sender stalls). The request
    // rides one stream, so its id is the one that arrives.
    let mut request = Vec::new();
    let request_id = loop {
      self.receive_into().await?;
      self.flush()?;
      let ids = self.conn.recv_stream_ids();
      for &id in &ids {
        request.extend_from_slice(&self.conn.read_stream(id));
      }
      if let Some(id) = ids
        .into_iter()
        .find(|&id| self.conn.recv_stream_complete(id))
      {
        break id;
      }
    };

    // Phase two: send the reply on the same stream id until the peer has acknowledged it whole.
    let reply = handler(request);
    self.conn.open(request_id, &reply);
    loop {
      self.flush()?;
      if self.conn.send_complete() {
        self.conn.forget_stream(request_id);
        return Ok(());
      }
      self.receive_into().await?;
    }
  }
}

/// Protects `frames` into a packet to RFC 9001 shape under `keys` (pure — no socket, no state). Builds
/// a short header (fixed bit set, the packet-number length in its low bits) carrying `pn` truncated to
/// the fewest bytes `largest_acked` allows, pads the frame bytes up to the length header protection
/// needs to sample, AEAD-seals them under the local 1-RTT packet key with that header as associated
/// data, then masks the first byte and the packet-number field with the local header-protection key.
fn protect_packet(
  keys: &Keys,
  pn: u64,
  largest_acked: Option<u64>,
  frames: &[Frame],
) -> Result<Vec<u8>, EndpointError> {
  // The short header: fixed bit set, spin/reserved/key-phase zero, low bits = packet-number length.
  let encoded = encode_packet_number(pn, largest_acked);
  let pn_len = encoded.len();
  let first_byte = FIXED_BIT | u8::try_from(pn_len - 1).unwrap_or(0);
  let mut packet = Vec::with_capacity(PACKET_NUMBER_OFFSET + pn_len);
  packet.push(first_byte);
  packet.extend_from_slice(encoded.as_slice());
  let header_len = packet.len();

  // The frame bytes, padded (RFC 9000 §19.1 PADDING = zero bytes) so the packet is long enough that
  // header protection can sample the ciphertext even without counting the tag.
  let sample_len = keys.local.header.sample_len();
  let mut payload = encode_frames(frames);
  let min_payload = (HEADER_PROTECTION_SAMPLE_OFFSET + sample_len).saturating_sub(header_len);
  if payload.len() < min_payload {
    payload.resize(min_payload, 0);
  }

  // AEAD-seal the payload with the plaintext header as associated data, then assemble the packet.
  let tag = keys
    .local
    .packet
    .encrypt_in_place(pn, &packet, &mut payload)?;
  packet.extend_from_slice(&payload);
  packet.extend_from_slice(tag.as_ref());

  // Apply header protection: sample the ciphertext, mask the first byte and packet-number field.
  let (head, tail) = packet.split_at_mut(HEADER_PROTECTION_SAMPLE_OFFSET);
  let sample = &tail[..sample_len];
  let (first, number) = head.split_at_mut(PACKET_NUMBER_OFFSET);
  keys
    .local
    .header
    .encrypt_in_place(sample, &mut first[0], number)?;
  Ok(packet)
}

/// Unprotects a received packet to RFC 9001 shape under `keys`, returning the reconstructed packet
/// number and the frames (pure — no socket, no state). Removes header protection (sampling the still-
/// encrypted ciphertext to unmask the first byte and packet-number field), validates the short header,
/// reconstructs the full number against `rx_largest`, then AEAD-opens the payload under the remote
/// 1-RTT key with the unmasked header as associated data.
fn unprotect_packet(
  keys: &Keys,
  rx_largest: u64,
  datagram: &[u8],
) -> Result<(u64, Vec<Frame>), EndpointError> {
  let sample_len = keys.remote.header.sample_len();
  if datagram.len() < HEADER_PROTECTION_SAMPLE_OFFSET + sample_len {
    return Err(EndpointError::NotReady);
  }
  let mut packet = datagram.to_vec();

  // Remove header protection: the packet-number field is unknown length, so hand the masker the full
  // maximum-width span; it unmasks the first byte, reads the length, and unmasks exactly that many.
  let (head, tail) = packet.split_at_mut(HEADER_PROTECTION_SAMPLE_OFFSET);
  let sample = &tail[..sample_len];
  let (first, number) = head.split_at_mut(PACKET_NUMBER_OFFSET);
  keys
    .remote
    .header
    .decrypt_in_place(sample, &mut first[0], number)?;

  // Validate the now-plaintext short header.
  let first_byte = packet[0];
  if first_byte & FIXED_BIT == 0 || first_byte & SHORT_HEADER_RESERVED_MASK != 0 {
    return Err(EndpointError::Header);
  }
  let pn_len = usize::from((first_byte & PACKET_NUMBER_LENGTH_MASK) + 1);
  let header_len = PACKET_NUMBER_OFFSET + pn_len;
  let pn = decode_packet_number(rx_largest, &packet[PACKET_NUMBER_OFFSET..header_len]);

  // AEAD-open the payload with the unmasked header as associated data.
  let aad = packet[..header_len].to_vec();
  let mut buf = packet[header_len..].to_vec();
  let plaintext = keys.remote.packet.decrypt_in_place(pn, &aad, &mut buf)?;
  let frames = decode_frames(plaintext).map_err(EndpointError::Frames)?;
  Ok((pn, frames))
}

#[cfg(test)]
mod tests {
  // Test harness: an unwrap here is a failed test.
  #![allow(clippy::unwrap_used)]

  use rustls::pki_types::PrivateKeyDer;

  use super::*;
  use crate::handshake::{Identity, connect};

  /// A fresh self-signed identity, minted with `ring` via `rcgen`.
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

  /// Drives an in-process handshake to the 1-RTT keys, returning `(client_keys, server_keys)` so the
  /// packet-protection functions can be exercised without a socket. (The direction is: the client's
  /// local packet key equals the server's remote packet key, so a client-protected packet opens with
  /// the server's keys.)
  fn handshake_keys() -> (Keys, Keys) {
    let identity = self_signed("slates-node");
    let (mut client, mut server) = connect(&identity, &identity, "slates-node").unwrap();
    let mut client_keys = None;
    let mut server_keys = None;
    for _ in 0..HANDSHAKE_TURN_CEILING {
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
    (client_keys.unwrap(), server_keys.unwrap())
  }

  /// The plaintext short header a given packet number would have with no header protection, for the
  /// non-vacuity comparison below.
  fn plaintext_header(pn: u64) -> Vec<u8> {
    let encoded = encode_packet_number(pn, None);
    let mut header = vec![FIXED_BIT | u8::try_from(encoded.len() - 1).unwrap()];
    header.extend_from_slice(encoded.as_slice());
    header
  }

  /// AC (§4.10a §8): a packet protected under the local keys opens under the peer's remote keys,
  /// recovering the number and frames — across several packet numbers, so the truncated-number path is
  /// exercised — and header protection genuinely masks the header. The masking check is the
  /// non-vacuity counter (CLAUDE.md §4): a silently dead header-protection path would leave every wire
  /// header equal to its plaintext, which this asserts never happens across the whole run.
  #[test]
  fn header_protection_masks_and_the_packet_round_trips() {
    let (client_keys, server_keys) = handshake_keys();
    // A deliberately tiny frame so the packet must be padded up to the sampleable length.
    let frames = vec![Frame::MaxData { max: 0x0102_0304 }];

    let mut any_masked = false;
    let mut rx_largest = 0u64;
    for pn in 0..8u64 {
      let wire = protect_packet(&client_keys, pn, None, &frames).unwrap();
      let plain = plaintext_header(pn);
      if wire[..plain.len()] != plain[..] {
        any_masked = true;
      }
      let (got_pn, got_frames) = unprotect_packet(&server_keys, rx_largest, &wire).unwrap();
      assert_eq!(got_pn, pn, "reconstructed packet number");
      assert_eq!(got_frames, frames, "recovered frames");
      rx_largest = rx_largest.max(got_pn);
    }
    assert!(
      any_masked,
      "header protection never changed any header — a dead masking path"
    );
  }

  /// AC (§4.10a §8, hostile): a packet with a flipped payload byte fails to open (the AEAD tag catches
  /// it), and a packet too short to sample is refused — never a panic, never a silent accept.
  #[test]
  fn a_tampered_or_short_packet_is_refused() {
    let (client_keys, server_keys) = handshake_keys();
    let frames = vec![Frame::Stream {
      stream_id: 1,
      offset: 0,
      fin: true,
      data: b"payload".to_vec(),
    }];
    let wire = protect_packet(&client_keys, 3, None, &frames).unwrap();

    // Flip the last byte (inside the AEAD tag): opening must fail.
    let mut tampered = wire.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(
      unprotect_packet(&server_keys, 0, &tampered).is_err(),
      "a tampered packet must not open"
    );

    // A packet shorter than the header-protection sample is refused as not-ready, not a panic.
    assert!(matches!(
      unprotect_packet(&server_keys, 0, &wire[..4]),
      Err(EndpointError::NotReady)
    ));
  }
}
