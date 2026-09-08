//! The session-plane connection endpoint (§4.10a §8, slice 4f) — the culmination that wires the
//! layers into a *live* session over the runtime's UDP socket: it drives the `rustls::quic` handshake
//! to the 1-RTT keys, then protects each packet's frames with those keys (the AEAD proven in
//! `handshake.rs`) and carries a stream over the wire. This is the session-plane analogue of the
//! control plane's `plane.rs`.
//!
//! Scope of this slice (the smallest live session): the handshake over UDP, 1-RTT packet protection
//! (a plaintext packet-number header as AAD + payload AEAD — **header protection is owed**, an
//! obfuscation atop the AEAD), and one stream carried end to end. Reliability/ACK/loss (`conn.rs`)
//! and flow-control credit updates (`flow.rs`) are built and unit-composed; wiring their frames into
//! this loop, congestion, and multiplexing many streams are the remaining connection work. The
//! `Arc` here is rustls's config (D-8 exception 2), confined to `crate::handshake`.

use rustix::net::SocketAddrV4;
use rustls::quic::{ClientConnection, KeyChange, Keys, ServerConnection};
use slates_rt::udp::UdpSocket;

use crate::handshake::{HandshakeError, Identity, client_connection, server_connection};
use crate::session::{Frame, decode_frames, encode_frames};
use crate::stream::{StreamAssembler, StreamSender};

/// The packet-number header size (bytes): the plaintext short-header this slice uses as the AEAD's
/// additional data. Header protection (obfuscating it) is owed.
const HEADER_BYTES: usize = size_of::<u64>();

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
  /// A packet arrived before the handshake produced keys, or was too short to carry a header.
  NotReady,
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
/// established, and the outgoing packet-number counter.
pub struct Endpoint {
  socket: UdpSocket,
  peer: SocketAddrV4,
  quic: Quic,
  keys: Option<Keys>,
  tx_pn: u64,
}

impl Endpoint {
  /// The client end, pinning the server's `pinned` certificate (the enrolled identity it trusts) and
  /// talking to `peer` as `name`. Needs no private key.
  pub fn client(
    socket: UdpSocket,
    peer: SocketAddrV4,
    pinned: &rustls::pki_types::CertificateDer<'static>,
    name: &str,
  ) -> Result<Endpoint, EndpointError> {
    let client = client_connection(pinned, name).map_err(EndpointError::Handshake)?;
    Ok(Endpoint {
      socket,
      peer,
      quic: Quic::Client(client),
      keys: None,
      tx_pn: 0,
    })
  }

  /// The server end presenting `identity`, talking to `peer`.
  pub fn server(
    socket: UdpSocket,
    peer: SocketAddrV4,
    identity: &Identity,
  ) -> Result<Endpoint, EndpointError> {
    let server = server_connection(identity).map_err(EndpointError::Handshake)?;
    Ok(Endpoint {
      socket,
      peer,
      quic: Quic::Server(server),
      keys: None,
      tx_pn: 0,
    })
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

  /// Protects `frames` into a packet: a plaintext packet-number header (the AAD) then the AEAD of the
  /// frame bytes under the local 1-RTT key. Advances the packet number.
  fn protect(&mut self, frames: &[Frame]) -> Result<Vec<u8>, EndpointError> {
    let keys = self.keys.as_ref().ok_or(EndpointError::NotReady)?;
    let pn = self.tx_pn;
    self.tx_pn = self.tx_pn.saturating_add(1);
    let header = pn.to_le_bytes();
    let mut payload = encode_frames(frames);
    let tag = keys
      .local
      .packet
      .encrypt_in_place(pn, &header, &mut payload)?;
    let mut datagram = header.to_vec();
    datagram.extend_from_slice(&payload);
    datagram.extend_from_slice(tag.as_ref());
    Ok(datagram)
  }

  /// Unprotects a received packet into its frames: reads the packet-number header, then AEAD-opens
  /// the rest under the remote 1-RTT key.
  fn unprotect(&self, datagram: &[u8]) -> Result<Vec<Frame>, EndpointError> {
    let keys = self.keys.as_ref().ok_or(EndpointError::NotReady)?;
    if datagram.len() < HEADER_BYTES {
      return Err(EndpointError::NotReady);
    }
    let (header, rest) = datagram.split_at(HEADER_BYTES);
    let mut pn_bytes = [0u8; HEADER_BYTES];
    pn_bytes.copy_from_slice(header);
    let pn = u64::from_le_bytes(pn_bytes);
    let mut buf = rest.to_vec();
    let plaintext = keys.remote.packet.decrypt_in_place(pn, header, &mut buf)?;
    decode_frames(plaintext).map_err(EndpointError::Frames)
  }

  /// Sends `data` as one stream (id `stream_id`), framed within a generous credit at a frame cap and
  /// each packet protected, over the socket. (Reliability/flow wiring is owed; over the lossless sim
  /// this delivers directly.)
  pub async fn send_stream(
    &mut self,
    stream_id: u64,
    data: &[u8],
    frame_cap: usize,
  ) -> Result<(), EndpointError> {
    let mut sender = StreamSender::new();
    sender.write(data);
    sender.grant_credit(data.len() as u64);
    sender.finish();
    while let Some(frame) = sender.next_frame(stream_id, frame_cap) {
      let datagram = self.protect(&[frame])?;
      self.socket.send_to(&datagram, self.peer)?;
    }
    Ok(())
  }

  /// Receives protected packets and reassembles one stream into `assembler`, returning its bytes when
  /// the stream's `fin` completes.
  pub async fn recv_stream(
    &mut self,
    assembler: &mut StreamAssembler,
  ) -> Result<Vec<u8>, EndpointError> {
    let mut buf = [0u8; 2048];
    let mut received = Vec::new();
    while !assembler.is_complete() {
      let (n, _from) = self.socket.recv_from(&mut buf).await?;
      for frame in self.unprotect(&buf[..n])? {
        if let Frame::Stream {
          offset, fin, data, ..
        } = frame
        {
          // The receive window tracks the credit; here it is generous (flow wiring owed).
          assembler.grant_window(offset + data.len() as u64 + 1);
          let _ = assembler.offer(offset, &data, fin);
        }
      }
      received.extend_from_slice(&assembler.read());
    }
    Ok(received)
  }
}
