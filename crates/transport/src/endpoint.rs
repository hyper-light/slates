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
//! lossless simulation fabric no retransmission is needed; over a real datagram socket a lost packet or a
//! lost acknowledgement is recovered by the tail-loss probe — every reliable exchange waits for the next
//! packet only up to the estimated PTO ([`Endpoint::receive_or_probe`], RFC 9002 §6.2.1) and, on a
//! timeout, retransmits the oldest in-flight packet ([`Connection::probe`]) — the loss-recovery and probe
//! paths themselves are proven by the `connection` oracle. Flow-control (per-stream and connection-wide
//! credit), congestion control, and multi-stream multiplexing are enforced by the [`Connection`] this
//! drives; RTT is estimated here (this end holds the clock — see [`Endpoint::smoothed_rtt`]). Every
//! 1-RTT packet carries the session's **connection id** — eight bytes both ends derive from the TLS
//! exporter once the handshake completes ([`Endpoint::connection_id`]) — so a socket shared by several
//! peers routes each packet to its session (`crate::demux`); an endpoint reaches the wire through its
//! own socket or such a shared one (`Link`). Remaining connection work: an MTU budget (several frames
//! per packet). The `Arc` here is rustls's config (D-8 exception 2), in `crate::handshake`.

use std::collections::BTreeMap;
use std::future::Future;
use std::task::Poll;
use std::time::Instant;

use rustix::net::SocketAddrV4;
use rustls::pki_types::CertificateDer;
use rustls::quic::{KeyChange, Keys};
use slates_rt::udp::UdpSocket;

use crate::connection::{Connection, initial_receive_window};
use crate::demux::{Demux, DemuxId, Slot, with_demux};
use crate::handshake::{HandshakeError, Identity, client_connection, server_connection};
use crate::packet_number::{MAX_PACKET_NUMBER_BYTES, decode_packet_number, encode_packet_number};
use crate::rtt::{GRANULARITY_NS, RttEstimator};
use crate::session::{Frame, decode_frames, encode_frames};

/// Format: RFC 9000 §14.1 — the smallest datagram every QUIC path must carry (1200 bytes); the fleet's
/// frame cap and a receive queue's sizing derive from it.
pub const MIN_DATAGRAM_BYTES: usize = 1200;
/// Format: RFC 9000 §17.3 — bit 6 of a short-header first byte, always 1 ("fixed bit"); a packet with
/// it clear is not a valid short header.
const FIXED_BIT: u8 = 0x40;
/// Format: RFC 9000 §17.3, §17.3.1 — bits 3-4 of a short-header first byte are reserved and MUST be 0
/// once header protection is removed; a non-zero value there is a protocol error.
const SHORT_HEADER_RESERVED_MASK: u8 = 0x18;
/// Format: RFC 9000 §17.3 — bits 0-1 of a short-header first byte carry the packet-number length minus
/// one, so a stored 0..=3 means a 1..=4 byte field.
const PACKET_NUMBER_LENGTH_MASK: u8 = 0x03;
/// Format: RFC 9000 §17.2/§17.3 — the destination connection id every short header carries, eight
/// bytes (within the standard's 0..=20). Both ends derive it from the TLS exporter once the handshake
/// completes ([`Endpoint::connection_id`]), so it is unique per session and never negotiated on the
/// wire; a socket shared by several peers routes each 1-RTT packet by it (`crate::demux`).
pub const CONNECTION_ID_BYTES: usize = 8;
/// A session's connection id (see [`CONNECTION_ID_BYTES`]).
pub type ConnectionId = [u8; CONNECTION_ID_BYTES];
/// Format: the TLS exporter label the connection id is derived under (RFC 8446 §7.5, RFC 5705): a
/// label private to this dialect, so no other exporter use can collide with it.
const CONNECTION_ID_LABEL: &[u8] = b"slates connection id v1";
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

/// The most times `establish` retransmits a handshake flight, while waiting for the peer's next flight,
/// before it abandons the connection with `NotReady` — the bound on the handshake's wait (banned item 8: no
/// unbounded wait). Each retransmit waits one probe timeout (RFC 9002 §6.2.2, two thirds of a second before
/// the handshake yields an RTT sample), so this covers a peer that boots many seconds after this node first
/// dials it — a fleet forms as its nodes come up one after another — while still terminating on a peer that
/// never answers.
/// Shape: it caps only the failure-and-formation path (a healthy handshake completes in its first few turns
/// and never retransmits), so the exact value is not performance-tuned — only large enough that the wait,
/// `MAX_HANDSHAKE_RETRANSMITS` × the initial probe timeout ≈ twenty seconds, spans a plausible boot skew.
const MAX_HANDSHAKE_RETRANSMITS: u32 = 32;

/// How many consecutive probe-timeout silences end the server's handshake-confirmation wait
/// ([`Endpoint::confirm_handshake`]). A client that has not yet heard the server's confirmation
/// retransmits its final flight every probe timeout; so once the server — which already holds that
/// flight and knows the exchange is mutually complete — has seen the peer fall silent this many
/// timeouts running, the client has received a confirmation and stopped, and the server may leave. A
/// client still in need keeps the server here by retransmitting (each received flight resets the count),
/// and the server resends the confirmation on each of these silences, so a confirmation lost inside the
/// window is still recovered; the whole wait is additionally capped by `MAX_HANDSHAKE_RETRANSMITS`
/// received datagrams (banned item 8: no unbounded wait).
/// Shape: a small tail-loss tolerance — on any path a client answers a live confirmation within one
/// round trip, so a handful of silent timeouts is conclusive; the exact value is not performance-tuned.
const HANDSHAKE_CONFIRM_SILENCE: u32 = 3;

/// Format: the most doublings the probe-timeout backoff applies — enough that a timeout of the timer
/// granularity (RFC 9002 §6.1.2, one millisecond) climbs past any initial PTO (`1 ms × 2^12 ≈ 4 s`), the
/// ceiling the backed-off timeout is held under anyway; a shift no larger than this cannot overflow.
const PTO_BACKOFF_SHIFT_CAP: u32 = 12;

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
  /// The demultiplexer closed this session: its peer established a new one (a re-dial after a loss),
  /// or the peer was retired. The reader ends its loop; nothing more arrives here.
  Closed,
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

/// The QUIC connection behind the endpoint — rustls's own client/server enum, so the handshake, the
/// exporter (the connection id) and the peer's certificate come from one type whichever side this is.
type Quic = rustls::quic::Connection;

/// What the endpoint asks of the connection beyond rustls's own surface.
trait QuicExt {
  /// Whether this is the TLS **client** — the side that, in TLS 1.3, finishes the handshake the instant
  /// it *sends* its Certificate/Finished flight, so it (not the server) bears the tail-loss risk that
  /// [`Endpoint::confirm_handshake`] closes.
  fn is_client(&self) -> bool;
  /// The connection id both ends derive from the TLS exporter (RFC 8446 §7.5) — refused by rustls
  /// before the handshake completes.
  fn export_connection_id(&self) -> Result<ConnectionId, rustls::Error>;
  /// The peer's end-entity certificate, once the handshake authenticated it.
  fn peer_certificate(&self) -> Option<CertificateDer<'static>>;
}

impl QuicExt for Quic {
  fn is_client(&self) -> bool {
    matches!(self, Quic::Client(_))
  }
  fn export_connection_id(&self) -> Result<ConnectionId, rustls::Error> {
    self.export_keying_material([0u8; CONNECTION_ID_BYTES], CONNECTION_ID_LABEL, None)
  }
  fn peer_certificate(&self) -> Option<CertificateDer<'static>> {
    self
      .peer_certificates()
      .and_then(|chain| chain.first())
      .cloned()
  }
}

/// How an endpoint reaches the wire: its own socket (one socket, one peer — a dialing client, or a
/// server told its peer), or a socket shared with other sessions through a demultiplexer, which routes
/// each received datagram to this session's inbox by connection id (`crate::demux`). Sends go straight
/// to the socket either way.
enum Link {
  /// This endpoint's own socket.
  Own(UdpSocket),
  /// A shared socket: sends through the demultiplexer's socket, receives from this session's inbox. The
  /// demultiplexer is named by id (looked up on this shard), so the endpoint stays `Send`.
  Shared {
    /// The demultiplexer (process-lifetime, one per socket) on this shard.
    demux: DemuxId,
    /// This session's slot in it.
    slot: Slot,
  },
}

/// One end of a session: the UDP socket, the peer, the QUIC handshake state, the 1-RTT keys once
/// established, and — the crucial part for correctness — **one long-lived [`Connection`]** carrying
/// every exchange, so the packet-number space is continuous and a number is never reused under the
/// 1-RTT keys (RFC 9000 §12.3, RFC 9001 §5.3). `rx_largest` is the persistent packet-number decode
/// cursor; `frame_cap` sizes each frame and the receive window. Completed streams are forgotten after
/// each exchange so a long-lived connection does not accumulate them without bound.
pub struct Endpoint {
  link: Link,
  peer: SocketAddrV4,
  quic: Quic,
  keys: Option<Keys>,
  /// The connection id, once derived from the completed handshake ([`Endpoint::connection_id`]).
  cid: Option<ConnectionId>,
  conn: Connection,
  rx_largest: u64,
  frame_cap: usize,
  /// The RTT estimator (RFC 9002 §5.3), fed from this end's clock: when an acknowledgement newly frees
  /// a packet, the round trip is `now` minus that packet's send time. Drives the probe timeout.
  rtt: RttEstimator,
  /// The send time of each ack-eliciting packet still awaiting acknowledgement, keyed by packet number,
  /// for the RTT sample. Pruned as packets are acknowledged, so it stays within the in-flight window.
  send_times: BTreeMap<u64, Instant>,
  /// A client's final handshake flight, kept after establishment: a raw handshake datagram arriving on
  /// an established session is a server still asking for it (its confirmation raced this end's exit
  /// from the handshake), and is answered by resending it. Empty on a server.
  final_flight: Vec<u8>,
  /// Datagrams an established session discarded rather than folded — a raw handshake retransmit, a
  /// packet naming another session, one that did not open under the keys (RFC 9000 §12.2: an
  /// undecryptable packet is discarded, never fatal). A counter, so a test can assert the discard path
  /// ran.
  discarded: u64,
  /// Consecutive probe timeouts without an acknowledgement in between (RFC 9002 §6.2.1's PTO count):
  /// each one doubles the next probe timeout, so a peer that has gone silent is retransmitted to at a
  /// falling rate rather than every estimated round trip — on a loopback path a few hundred
  /// microseconds, which would send thousands of retransmits a second into a dead port and starve the
  /// shard's live sessions. Reset to zero by the next acknowledgement.
  pto_count: u32,
}

impl Drop for Endpoint {
  fn drop(&mut self) {
    // A session on a shared socket gives its slot back, so the demultiplexer forgets its routes and can
    // reuse the slot under a new generation.
    if let Link::Shared { demux, slot } = &self.link {
      let _ = with_demux(*demux, |d| d.release(*slot));
    }
  }
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
      link: Link::Own(socket),
      peer,
      quic: Quic::Client(client),
      keys: None,
      cid: None,
      conn: Connection::new(initial_receive_window(frame_cap)),
      rx_largest: 0,
      frame_cap,
      rtt: RttEstimator::new(),
      send_times: BTreeMap::new(),
      final_flight: Vec::new(),
      discarded: 0,
      pto_count: 0,
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
      link: Link::Own(socket),
      peer,
      quic: Quic::Server(server),
      keys: None,
      cid: None,
      conn: Connection::new(initial_receive_window(frame_cap)),
      rx_largest: 0,
      frame_cap,
      rtt: RttEstimator::new(),
      send_times: BTreeMap::new(),
      final_flight: Vec::new(),
      discarded: 0,
      pto_count: 0,
    })
  }

  /// A server end a demultiplexer opened for a dialer it heard from `peer` on its shared socket
  /// (`crate::demux`): it presents the demultiplexer's identity and requires the dialer's certificate
  /// among its allowed clients (mutual authentication — the same trust `server` enforces), and reads
  /// its datagrams from its inbox at `slot`. The consumer drives [`establish`](Endpoint::establish) as
  /// for any endpoint; once the handshake completes the session's connection id is bound in the
  /// demultiplexer so the peer's 1-RTT packets route here.
  pub(crate) fn accepted(
    demux: DemuxId,
    slot: Slot,
    peer: SocketAddrV4,
    on: &Demux,
  ) -> Result<Endpoint, EndpointError> {
    let server = on.server_connection().map_err(EndpointError::Handshake)?;
    let frame_cap = on.frame_cap();
    Ok(Endpoint {
      link: Link::Shared { demux, slot },
      peer,
      quic: Quic::Server(server),
      keys: None,
      cid: None,
      conn: Connection::new(initial_receive_window(frame_cap)),
      rx_largest: 0,
      frame_cap,
      rtt: RttEstimator::new(),
      send_times: BTreeMap::new(),
      final_flight: Vec::new(),
      discarded: 0,
      pto_count: 0,
    })
  }

  /// Datagrams this established session discarded rather than folded (see the field): a raw handshake
  /// retransmit answered and dropped, a packet for another session, one that did not open.
  pub fn discarded(&self) -> u64 {
    self.discarded
  }

  /// The peer's end-entity certificate, once the handshake authenticated it — how a server that
  /// accepted a session on a shared socket tells which peer dialed it.
  pub fn peer_certificate(&self) -> Option<CertificateDer<'static>> {
    self.quic.peer_certificate()
  }

  /// This session's connection id — the eight bytes both ends derive from the TLS exporter once the
  /// handshake completes (never negotiated on the wire), carried in every 1-RTT short header. Refused
  /// `NotReady` before the handshake completes. The first derivation on a shared link binds the id (and
  /// the peer's certificate) in the demultiplexer, so the peer's packets route to this session — and
  /// replaces any session the same peer established before.
  pub fn connection_id(&mut self) -> Result<ConnectionId, EndpointError> {
    if let Some(cid) = self.cid {
      return Ok(cid);
    }
    if self.quic.is_handshaking() {
      return Err(EndpointError::NotReady);
    }
    let cid = self.quic.export_connection_id()?;
    self.cid = Some(cid);
    if let Link::Shared { demux, slot } = &self.link {
      let peer = self.quic.peer_certificate().map(|c| c.as_ref().to_vec());
      with_demux(*demux, |d| d.bind(*slot, cid, peer)).ok_or(EndpointError::Closed)?;
    }
    Ok(cid)
  }

  /// Sends one datagram to the peer over this endpoint's link.
  fn send(&self, datagram: &[u8]) -> Result<(), EndpointError> {
    match &self.link {
      Link::Own(socket) => socket
        .send_to(datagram, self.peer)
        .map(|_| ())
        .map_err(EndpointError::Io),
      Link::Shared { demux, .. } => {
        with_demux(*demux, |d| d.send_to(datagram, self.peer)).ok_or(EndpointError::Closed)?
      }
    }
  }

  /// Receives one datagram over the link into `buf` — its length and source — or `Ok(None)` if
  /// `timeout_ns` elapses first. The receive is raced against a timer; a delivered datagram is preferred
  /// when both are ready, so a live exchange never trades a received packet for a spurious
  /// retransmission. On a shared link the datagram comes from this session's inbox, which the
  /// demultiplexer fills; `Closed` once the demultiplexer closed the session.
  async fn recv_within(
    &self,
    buf: &mut [u8],
    timeout_ns: u64,
  ) -> Result<Option<(usize, SocketAddrV4)>, EndpointError> {
    let outcome = match &self.link {
      Link::Own(socket) => within(socket.recv_from(buf), timeout_ns)
        .await
        .map(|received| received.map_err(EndpointError::Io)),
      Link::Shared { demux, slot } => {
        within(
          std::future::poll_fn(|cx| {
            with_demux(*demux, |d| d.poll_recv(*slot, buf, cx))
              .unwrap_or(Poll::Ready(Err(EndpointError::Closed)))
          }),
          timeout_ns,
        )
        .await
      }
    };
    outcome.transpose()
  }

  /// The smoothed round-trip time this end has estimated (nanoseconds), zero before any acknowledgement
  /// yields a sample (RFC 9002 §5.3). A live exchange feeds it through [`Endpoint::ingest`].
  pub fn smoothed_rtt(&self) -> u64 {
    self.rtt.smoothed_rtt()
  }

  /// The probe timeout this end would arm to recover a tail loss (RFC 9002 §6.2.1), from the estimated
  /// RTT; before any sample, twice the initial RTT. (Driving a timed receive from it is owed.)
  pub fn pto(&self) -> u64 {
    self.rtt.pto(0)
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
    let mut sent_at: Option<Instant> = None;
    let mut last_flight: Vec<u8> = Vec::new();
    // The peer's flight this end last fed to `read_hs`. Retransmits that raced this end's reply arrive as
    // an exact re-send of a flight already consumed; `read_hs` treats its input as an ordered byte stream
    // and would fault on the repeat (a fresh `ClientHello` where it expects the client's `Finished`), so a
    // datagram identical to this is skipped rather than fed. The aggressive early backoff makes such a
    // raced duplicate common — the peer answers before this end's next retransmit would have fired — so
    // this is what keeps the fast retransmit from corrupting an otherwise-healthy handshake.
    let mut last_consumed: Vec<u8> = Vec::new();
    for _ in 0..HANDSHAKE_TURN_CEILING {
      let out = self.drain_handshake();
      if !out.is_empty() {
        self.send(&out)?;
        sent_at = Some(Instant::now());
        last_flight = out;
      }
      if !self.quic.is_handshaking() && self.keys.is_some() {
        // The TLS bytes are all exchanged, but TLS 1.3 leaves the two ends *asymmetrically* finished —
        // the client the moment it sends its final flight, the server only when it receives it — so a
        // dropped final flight would strand the server. Confirm delivery before returning (RFC 9001
        // §4.1.2, RFC 9000 §19.20). A client keeps its final flight past establishment, to answer a
        // server whose confirmation raced this exit and that is still asking for the flight.
        if self.quic.is_client() {
          self.final_flight = last_flight.clone();
        }
        return self.confirm_handshake(&last_flight).await;
      }
      // Receive the peer's next flight, retransmitting our last flight each probe timeout so a dropped
      // handshake packet — the common case being a peer not yet listening when we first sent — is recovered
      // on the *same* connection (RFC 9002 §6.2: no handshake acknowledgement, arm the PTO and retransmit).
      // This is what lets a client keep one dial socket across a peer's boot rather than re-dialing from a
      // fresh port: a fresh-port re-dial would race the server's `accept`, which pins the first source it
      // hears. The accept side, still to learn its peer, has no flight yet and no peer to send to, so it
      // simply waits. Bounded by a retransmit ceiling (banned item 8: no unbounded wait).
      // The retransmit interval backs off exponentially from the timer granularity toward the probe
      // timeout, rather than waiting a full probe timeout each time. The dominant handshake loss is a peer
      // not yet listening when this node first dials it (a fleet forms as its nodes boot one after
      // another); a flat two-thirds-of-a-second wait would then reach a peer that binds its socket a
      // moment later only on the *next* such tick — up to that long after it is reachable — which loses the
      // race to form every session of an N-node mesh before an early death. Starting at the granularity
      // and doubling reaches a slow-to-listen peer within milliseconds while the ceiling keeps a peer that
      // never answers from being retried faster than the estimated round trip; the retransmit count still
      // bounds the whole wait (banned item 8: no unbounded wait).
      let n = {
        let mut attempts = 0u32;
        let mut backoff = GRANULARITY_NS;
        loop {
          let period = backoff.min(self.handshake_probe_ceiling());
          match self.recv_within(&mut buf, period).await? {
            Some((rn, _from)) => {
              // Skip an exact re-send of the flight already consumed (a peer retransmit that raced this
              // end's reply): feeding it to `read_hs` would fault the handshake stream. It still counts
              // against the bound, so a peer flooding duplicates cannot loop this forever (banned item 8).
              if !last_consumed.is_empty() && buf[..rn] == last_consumed[..] {
                attempts += 1;
                if attempts > MAX_HANDSHAKE_RETRANSMITS {
                  return Err(EndpointError::NotReady);
                }
                continue;
              }
              break rn;
            }
            None => {
              attempts += 1;
              if attempts > MAX_HANDSHAKE_RETRANSMITS {
                return Err(EndpointError::NotReady);
              }
              backoff = backoff.saturating_mul(2);
              // A server the demultiplexer opened has no flight until it has read the client's first, so it
              // simply waits; a dialer resends its last flight.
              if !last_flight.is_empty() {
                self.send(&last_flight)?;
                sent_at = Some(Instant::now());
              }
            }
          }
        }
      };
      // The handshake's own round trip seeds the RTT estimator (RFC 9002 §5.1: the handshake gives the
      // first sample): the time from this end's last flight to the peer's reply. Without it the probe
      // timeout the reliable exchanges arm against would stay at the conservative initial RTT (two thirds
      // of a second) until the first post-handshake acknowledgement — far too coarse for a loopback or
      // LAN commit to recover a dropped packet within its budget. A coalesced or split flight (or one that
      // followed a retransmit) makes this an approximation, which is all a seed needs to be.
      if let Some(flight) = sent_at.take() {
        let sample = u64::try_from(Instant::now().saturating_duration_since(flight).as_nanos())
          .unwrap_or(u64::MAX);
        self.rtt.on_sample(sample, 0);
      }
      // Remember this flight so a later exact re-send of it (a peer retransmit) is recognized and skipped
      // above rather than fed to `read_hs` a second time.
      last_consumed.clear();
      last_consumed.extend_from_slice(&buf[..n]);
      self.quic.read_hs(&buf[..n])?;
    }
    Err(EndpointError::NotReady)
  }

  /// Confirms the handshake's final flight was delivered before `establish` returns, closing the
  /// TLS-1.3 tail-loss hole (RFC 9001 §4.1.2, RFC 9000 §19.20): TLS 1.3 finishes the **client** the
  /// instant it *sends* its Certificate/Finished flight but the **server** only when it *receives* that
  /// flight, so a dropped final flight would leave the client believing it is done while the server
  /// waits forever (retransmitting its own flight into a client that has stopped listening — the exact
  /// stall an N-node fleet's mesh hit). The two sides play complementary roles, and the residual
  /// two-army uncertainty is resolved the way QUIC resolves it — the server, which upon finishing
  /// already holds the client's flight, announces completion, and the client waits to hear it.
  async fn confirm_handshake(&mut self, last_flight: &[u8]) -> Result<(), EndpointError> {
    if self.quic.is_client() {
      self.confirm_as_client(last_flight).await
    } else {
      self.confirm_as_server().await
    }
  }

  /// The client's half of handshake confirmation: it cannot know its final flight (`last_flight`)
  /// arrived, so it waits for **any decryptable 1-RTT packet** from the server — proof the server
  /// reached its own 1-RTT keys, which in TLS 1.3 it can only do by receiving this flight — and
  /// retransmits the flight on each probe timeout (and at once on a raw handshake datagram, which means
  /// the server is still waiting) until then. Bounded by the handshake retransmit ceiling (banned
  /// item 8).
  async fn confirm_as_client(&mut self, last_flight: &[u8]) -> Result<(), EndpointError> {
    let mut buf = [0u8; 2048];
    let mut retransmits = 0u32;
    let mut backoff = GRANULARITY_NS;
    loop {
      let period = backoff.min(self.handshake_probe_ceiling());
      match self.recv_within(&mut buf, period).await? {
        Some((n, _)) => {
          // A packet that unprotects under the 1-RTT keys is the server's confirmation: it has its keys,
          // so it received our final flight, and the handshake is complete both ways.
          if self.ingest(&buf[..n]).is_ok() {
            return Ok(());
          }
          // Otherwise a raw handshake retransmit (the server has not seen our final flight yet): resend
          // it at once. A received datagram is progress — the peer is alive and still asking — so it does
          // not count toward the give-up budget, which counts only silent timeouts.
          self.send(last_flight)?;
        }
        None => {
          retransmits += 1;
          if retransmits > MAX_HANDSHAKE_RETRANSMITS {
            return Err(EndpointError::NotReady);
          }
          backoff = backoff.saturating_mul(2);
          self.send(last_flight)?;
        }
      }
    }
  }

  /// The server's half of handshake confirmation: having received the client's final flight, it already
  /// knows the exchange is mutually complete, so it announces that with a 1-RTT confirmation the client
  /// can decrypt and resends it whenever it still sees the client's raw flight retransmits (the client
  /// has not heard the confirmation yet). It leaves the moment the client sends **1-RTT traffic of its
  /// own** — a probe or a record commit, which the client only sends once it has the confirmation and has
  /// left its own handshake — or, as a fallback for a client that establishes but then sends nothing,
  /// once the client has fallen silent for [`HANDSHAKE_CONFIRM_SILENCE`] backoff intervals grown to the
  /// ceiling (roughly a second of quiet — far longer than the client's own matched backoff would leave a
  /// still-needed flight unretransmitted). A datagram that unprotects as 1-RTT is dropped, not ingested:
  /// the client's exchange retransmits it to the serve loop this returns into, so no half-consumed
  /// request is left buffered where that loop would deadlock. Bounded overall by
  /// `MAX_HANDSHAKE_RETRANSMITS` silent timeouts (banned item 8).
  async fn confirm_as_server(&mut self) -> Result<(), EndpointError> {
    let mut buf = [0u8; 2048];
    self.send_confirm()?;
    let mut silent = 0u32;
    let mut backoff = GRANULARITY_NS;
    loop {
      let period = backoff.min(self.handshake_probe_ceiling());
      match self.recv_within(&mut buf, period).await? {
        Some((n, _)) => {
          // A datagram that unprotects under the 1-RTT keys is the client's own application traffic — it
          // has our confirmation and moved on, so the handshake is done. Drop this datagram (do not
          // ingest it): the client's exchange retransmits it to the serve loop this returns into.
          let cid = self.connection_id()?;
          let confirmed = self
            .keys
            .as_ref()
            .is_some_and(|keys| unprotect_packet(keys, &cid, self.rx_largest, &buf[..n]).is_ok());
          if confirmed {
            return Ok(());
          }
          // A raw handshake retransmit: the client has not heard our confirmation. Resend it, and reset
          // the silence and its backoff — the client is still here and asking.
          silent = 0;
          backoff = GRANULARITY_NS;
          self.send_confirm()?;
        }
        None => {
          silent += 1;
          if silent > MAX_HANDSHAKE_RETRANSMITS {
            return Ok(());
          }
          // Fall out once the quiet has spanned enough intervals *and* those intervals have grown to the
          // ceiling — so the fallback exit only fires after a genuinely long silence, never mid-formation
          // while the client is still retransmitting on its own (matched) backoff.
          if silent > HANDSHAKE_CONFIRM_SILENCE && backoff >= self.handshake_probe_ceiling() {
            return Ok(());
          }
          backoff = backoff.saturating_mul(2);
          self.send_confirm()?;
        }
      }
    }
  }

  /// Sends one 1-RTT handshake-confirmation packet to the peer (see [`Connection::emit_confirm`]): a
  /// fresh packet number and a re-advertised flow-control credit, protected under the local 1-RTT keys.
  fn send_confirm(&mut self) -> Result<(), EndpointError> {
    let cid = self.connection_id()?;
    let (pn, frames) = self.conn.emit_confirm();
    let largest_acked = self.conn.tx_largest_acked();
    let keys = self.keys.as_ref().ok_or(EndpointError::NotReady)?;
    let datagram = protect_packet(keys, &cid, pn, largest_acked, &frames)?;
    self.send(&datagram)?;
    Ok(())
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
    let cid = self.connection_id()?;
    let keys = self.keys.as_ref().ok_or(EndpointError::NotReady)?;
    while let Some((pn, frames)) = self.conn.poll_transmit(self.frame_cap) {
      let datagram = protect_packet(keys, &cid, pn, self.conn.tx_largest_acked(), &frames)?;
      self.send(&datagram)?;
      // Record the send time for the RTT sample; pruned when the packet is acknowledged. A pure
      // acknowledgement packet's entry is never sampled and is swept when a later packet is acknowledged.
      self.send_times.insert(pn, Instant::now());
    }
    Ok(())
  }

  /// Receives one packet, but waits at most `timeout_ns` for it, and folds it into the connection:
  /// `Ok(false)` if the timer wins (no packet arrived in time). This is the loss-recovery clock the
  /// reliable exchanges ([`request`], [`serve_once`], [`send_stream`], [`recv_stream`]) drive — over the
  /// lossless simulation fabric a packet always arrives, but over a real datagram socket a lost packet
  /// (or a lost acknowledgement) would otherwise stall the exchange forever, since a dropped tail leaves
  /// no later acknowledgement to expose the gap. On a timeout the caller probes ([`Connection::probe`])
  /// to retransmit the oldest in-flight packet and flushes it. The timer also re-drives the receive
  /// itself: a fresh receive on the next call reads any datagram already delivered, so the exchange makes
  /// progress even if a single readiness wake was missed.
  ///
  /// [`request`]: Endpoint::request
  /// [`serve_once`]: Endpoint::serve_once
  /// [`send_stream`]: Endpoint::send_stream
  /// [`recv_stream`]: Endpoint::recv_stream
  async fn receive_and_ingest(&mut self, timeout_ns: u64) -> Result<bool, EndpointError> {
    let mut buf = [0u8; 2048];
    match self.recv_within(&mut buf, timeout_ns).await? {
      Some((n, _from)) => {
        self.fold_or_discard(&buf[..n])?;
        Ok(true)
      }
      None => Ok(false),
    }
  }

  /// Folds a received datagram into the connection, or discards it (counted) when it is not this
  /// session's 1-RTT traffic: a raw handshake datagram is the peer still finishing the handshake this end
  /// has left — a stale retransmit that raced it — and is answered (a server resends its confirmation, a
  /// client its final flight) so the peer completes too; a packet naming another session, or one that
  /// does not open under the keys, is dropped as RFC 9000 §12.2 has it, never fatal to the session. Only
  /// a decoded packet whose frames are malformed (the peer broke the protocol), a closed session, or a
  /// socket refusal end the exchange.
  fn fold_or_discard(&mut self, datagram: &[u8]) -> Result<(), EndpointError> {
    if !is_short_header(datagram) {
      self.discarded = self.discarded.saturating_add(1);
      if self.quic.is_client() {
        if !self.final_flight.is_empty() {
          let flight = self.final_flight.clone();
          self.send(&flight)?;
        }
      } else {
        self.send_confirm()?;
      }
      return Ok(());
    }
    match self.ingest(datagram) {
      Ok(()) => Ok(()),
      Err(EndpointError::Header | EndpointError::Tls(_) | EndpointError::NotReady) => {
        self.discarded = self.discarded.saturating_add(1);
        Ok(())
      }
      Err(other) => Err(other),
    }
  }

  /// Folds one received datagram into the connection: reconstructs its packet number against the persistent
  /// decode cursor (advancing it), feeds its frames to the connection, and folds an RTT sample in when the
  /// acknowledgement newly frees a packet (RFC 9002 §5.1: the round trip is now minus that packet's send
  /// time).
  fn ingest(&mut self, datagram: &[u8]) -> Result<(), EndpointError> {
    let now = Instant::now();
    let cid = self.connection_id()?;
    let keys = self.keys.as_ref().ok_or(EndpointError::NotReady)?;
    let (pn, frames) = unprotect_packet(keys, &cid, self.rx_largest, datagram)?;
    self.rx_largest = self.rx_largest.max(pn);
    if let Some(largest) = self.conn.handle_incoming(pn, &frames) {
      // An acknowledgement ends a run of probe timeouts: the next timeout starts from the estimate again.
      self.pto_count = 0;
      if let Some(sent_at) = self.send_times.get(&largest) {
        let sample =
          u64::try_from(now.saturating_duration_since(*sent_at).as_nanos()).unwrap_or(u64::MAX);
        // This dialect does not carry the peer's reported ack delay yet, so it is zero.
        self.rtt.on_sample(sample, 0);
      }
      // Prune the send times the acknowledgement covered, bounding the map to the in-flight window.
      self.send_times.retain(|&sent_pn, _| sent_pn > largest);
    }
    Ok(())
  }

  /// The probe timeout to arm a stalled reliable exchange against (RFC 9002 §6.2.1), from this end's RTT
  /// estimator; before any sample it is twice the initial RTT, so a lost first packet is still recovered.
  /// This dialect carries no peer ack-delay, so the max-ack-delay term is zero.
  fn probe_timeout(&self) -> u64 {
    self.rtt.pto(0)
  }

  /// The ceiling on the exponential-backoff retransmit interval *during the handshake* — the conservative
  /// initial PTO, not the estimated one. The first flight's round trip seeds a smoothed RTT that, on a
  /// loopback or same-host peer, is a few microseconds, dropping [`probe_timeout`](Endpoint::probe_timeout)
  /// to about the timer granularity; capping the handshake's retry there would exhaust its retransmit
  /// budget in tens of milliseconds and abandon a peer whose shard is momentarily busy establishing the
  /// rest of a mesh. Establishing a connection stays patient against the initial PTO instead (RFC 9002
  /// §6.2.2), while the post-handshake reliable exchanges still arm against the true estimate.
  fn handshake_probe_ceiling(&self) -> u64 {
    self.rtt.initial_pto()
  }

  /// Waits for the next packet within the probe timeout; on a timeout, drives tail-loss recovery by probing
  /// the oldest in-flight packet ([`Connection::probe`]) so the caller's next flush retransmits it. The
  /// reliable exchanges call this in place of a bare receive, so a lost packet or a lost acknowledgement —
  /// which a real datagram socket can drop and which no later acknowledgement would expose — cannot stall
  /// them. A probe with nothing in flight is a no-op, so a timeout while merely waiting on the peer is free.
  /// This wait is not self-bounded: a reliable exchange retransmits until it completes or its **caller**
  /// stops it (the fleet probe races it against a deadline and cancels it — [`crate::endpoint`] callers own
  /// the bound), which is what a peer that dies mid-exchange relies on to not strand the loop.
  async fn receive_or_probe(&mut self) -> Result<(), EndpointError> {
    // The probe timer is armed only while ack-eliciting packets are in flight (RFC 9002 §6.2.1): an idle
    // session — a server between requests, a client between exchanges — has nothing to retransmit, so it
    // waits at the conservative initial PTO instead of the estimated one, which on a loopback path is
    // a few hundred microseconds and would wake every idle session thousands of times a second, starving
    // the live exchanges on a busy shard. The long idle re-drive stays as the safety net against a
    // missed readiness wake.
    // With packets in flight the timeout backs off exponentially over consecutive expirations (RFC 9002
    // §6.2.1: "the PTO period MUST be set to twice its current value" after each one), climbing from the
    // estimate up to the conservative initial PTO the estimator started from — so a peer that has gone
    // silent is retransmitted to a few times per second, not a few thousand.
    let timeout = if self.conn.in_flight_count() == 0 {
      self.rtt.initial_pto()
    } else {
      let doublings = 1u64 << self.pto_count.min(PTO_BACKOFF_SHIFT_CAP);
      self
        .probe_timeout()
        .saturating_mul(doublings)
        .min(self.rtt.initial_pto())
    };
    if !self.receive_and_ingest(timeout).await? {
      self.conn.probe();
      self.pto_count = self.pto_count.saturating_add(1);
    }
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
      self.receive_or_probe().await?;
    }
  }

  /// Receives stream `stream_id` **reliably**, acknowledging what arrives and returning its bytes once
  /// the stream's `fin` completes. Drives a [`Connection`]: each received packet is acknowledged (the
  /// acknowledgement flushed before the next receive) so the sender learns of delivery, and the final
  /// acknowledgement is flushed after completion so the sender can finish.
  pub async fn recv_stream(&mut self, stream_id: u64) -> Result<Vec<u8>, EndpointError> {
    let mut received = Vec::new();
    loop {
      self.receive_or_probe().await?;
      // Drain *before* flushing: reading slides the flow-control window forward, and the flush that
      // follows advertises credit reflecting what was just read. Flushing first would advertise a
      // round-stale window and stall a transfer larger than one window at the window boundary.
      received.extend_from_slice(&self.conn.read_stream(stream_id));
      self.flush()?;
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
    // Start from a clean stream: a caller that abandoned an earlier exchange on this id at its deadline
    // (the fleet probe does) may have left that exchange's reply — arrived late, complete, unread — in the
    // receive side, and this exchange would otherwise read *that* reply as its own and stay one reply
    // behind on every exchange after it (a peer retired while answering every probe on time).
    self.conn.forget_stream(stream_id);
    self.conn.open(stream_id, request);
    let mut reply = Vec::new();
    loop {
      // Drain the reply, then flush: the flush sends the request (opened above, still credited) and the
      // acknowledgement whose piggybacked credit reflects the reply bytes just read — so a reply larger
      // than one window keeps flowing. Flushing before the drain would advertise a round-stale window.
      reply.extend_from_slice(&self.conn.read_stream(stream_id));
      self.flush()?;
      if self.conn.recv_stream_complete(stream_id) {
        self.conn.forget_stream(stream_id);
        return Ok(reply);
      }
      self.receive_or_probe().await?;
    }
  }

  /// Forgets `stream_id` on this session: drops any in-flight frames of an exchange abandoned mid-flight
  /// and its receive state. A caller that races [`request`](Endpoint::request) against its own deadline
  /// and keeps the session for reuse (the fleet's `request_within`) calls this when the deadline wins, so
  /// the abandoned exchange's stream does not ride the next flush on the reused session — its stale bytes
  /// would otherwise reach the peer alongside the next request and be folded into an unrelated exchange
  /// (`docs/bugs/2026-09-10-abandoned-request-retransmit-lockstep.md`: the same discipline `request`
  /// applies to its *own* stream, now available to the deadline-racing caller for the stream it abandons).
  pub fn forget_stream(&mut self, stream_id: u64) {
    self.conn.forget_stream(stream_id);
  }

  /// The server side of one request/reply exchange: receives a request stream, passes its stream id
  /// and bytes to `handler`, and sends the reply back on the same stream id, returning once the reply
  /// is acknowledged. The stream id is the request's **kind** on a session that carries several RPCs
  /// (a record commit, a promotion, a content put each ride their own id), so a server dispatches on it
  /// rather than guessing a message's kind from its bytes. Drives one [`Connection`]: phase one receives
  /// and acknowledges the request until its `fin`; phase two frames the reply and completes when the
  /// peer has acknowledged all of it.
  pub async fn serve_once<H>(&mut self, handler: H) -> Result<(), EndpointError>
  where
    H: FnOnce(u64, Vec<u8>) -> Vec<u8>,
  {
    // Phase one: receive one request in full, acknowledging and *draining* as it arrives — draining is
    // what slides the flow-control window forward, so a request larger than one window keeps flowing
    // (without it the credit never grows past the initial window and the sender stalls). Each request
    // **kind** rides its own stream id, and the bytes of each are kept apart in `pending` keyed by id: a
    // peer that reused this session after abandoning an earlier exchange at its deadline (the fleet's
    // `request_within` races `request` against a deadline and cancels it) may still have that exchange's
    // stream half-open, and folding its stray bytes into an unrelated request would hand the handler a
    // corrupt message. The first stream to reach its `fin` is this request; it is served with *its own*
    // bytes, and any other stream's partial bytes stay buffered for the exchange they belong to.
    let mut pending: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let (request_id, request) = loop {
      self.receive_or_probe().await?;
      let ids = self.conn.recv_stream_ids();
      for &id in &ids {
        let chunk = self.conn.read_stream(id);
        if !chunk.is_empty() {
          pending.entry(id).or_default().extend_from_slice(&chunk);
        }
      }
      // Flush *after* draining, so the acknowledgement advertises credit that reflects what was just
      // read — draining slides the flow-control window, and advertising before it would lag a round and
      // stall a request larger than one window at the window boundary (also carries the final ACK).
      self.flush()?;
      if let Some(id) = ids
        .into_iter()
        .find(|&id| self.conn.recv_stream_complete(id))
      {
        break (id, pending.remove(&id).unwrap_or_default());
      }
    };

    // Phase two: send the reply on the same stream id until the peer has acknowledged it whole. This is an
    // active exchange (a reply is in flight awaiting acknowledgement), so it carries the stall bound — a
    // peer that stops acknowledging is abandoned rather than retransmitted into forever.
    let reply = handler(request_id, request);
    self.conn.open(request_id, &reply);
    loop {
      self.flush()?;
      if self.conn.send_complete() {
        self.conn.forget_stream(request_id);
        return Ok(());
      }
      self.receive_or_probe().await?;
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
  cid: &ConnectionId,
  pn: u64,
  largest_acked: Option<u64>,
  frames: &[Frame],
) -> Result<Vec<u8>, EndpointError> {
  // The short header: fixed bit set, spin/reserved/key-phase zero, low bits = packet-number length; then
  // the connection id, then the packet number.
  let encoded = encode_packet_number(pn, largest_acked);
  let pn_len = encoded.len();
  let first_byte = FIXED_BIT | u8::try_from(pn_len - 1).unwrap_or(0);
  let mut packet = Vec::with_capacity(PACKET_NUMBER_OFFSET + pn_len);
  packet.push(first_byte);
  packet.extend_from_slice(cid);
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
  cid: &ConnectionId,
  rx_largest: u64,
  datagram: &[u8],
) -> Result<(u64, Vec<Frame>), EndpointError> {
  let sample_len = keys.remote.header.sample_len();
  if datagram.len() < HEADER_PROTECTION_SAMPLE_OFFSET + sample_len {
    return Err(EndpointError::NotReady);
  }
  // The connection id is not header-protected (RFC 9001 §5.4.1 masks only the first byte and the
  // packet number): a packet naming another session is refused before any crypto.
  if connection_id_of(datagram).as_ref() != Some(cid) {
    return Err(EndpointError::Header);
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

/// Whether a datagram is a 1-RTT short-header packet rather than a raw handshake flight: the fixed bit
/// (RFC 9000 §17.3) is set on every short header and is not masked by header protection (RFC 9001
/// §5.4.1 masks only the five low bits), while a TLS handshake message's first byte is its type, all of
/// which are small (RFC 8446 §4: `client_hello` 1 … `finished` 20) — the bit is never set there.
pub(crate) fn is_short_header(datagram: &[u8]) -> bool {
  datagram.first().is_some_and(|first| first & FIXED_BIT != 0)
}

/// The connection id a short-header packet carries (the bytes after the first), if it is long enough.
pub(crate) fn connection_id_of(datagram: &[u8]) -> Option<ConnectionId> {
  datagram
    .get(1..1 + CONNECTION_ID_BYTES)
    .and_then(|bytes| bytes.try_into().ok())
}

/// Races a receive against a timer of `timeout_ns`: its output if it lands first, `None` on the timer.
/// A delivered datagram is preferred when both are ready.
async fn within<F: Future>(receive: F, timeout_ns: u64) -> Option<F::Output> {
  let mut receive = std::pin::pin!(receive);
  let mut timer = std::pin::pin!(slates_rt::futures::sleep(timeout_ns));
  std::future::poll_fn(|cx| {
    if let Poll::Ready(out) = receive.as_mut().poll(cx) {
      return Poll::Ready(Some(out));
    }
    if timer.as_mut().poll(cx).is_ready() {
      return Poll::Ready(None);
    }
    Poll::Pending
  })
  .await
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

  /// The connection id the packet tests use — any eight bytes; the exporter derivation is tested on its
  /// own below.
  const CID: ConnectionId = [7, 6, 5, 4, 3, 2, 1, 0];

  /// The plaintext short header a given packet number would have with no header protection, for the
  /// non-vacuity comparison below.
  fn plaintext_header(pn: u64) -> Vec<u8> {
    let encoded = encode_packet_number(pn, None);
    let mut header = vec![FIXED_BIT | u8::try_from(encoded.len() - 1).unwrap()];
    header.extend_from_slice(&CID);
    header.extend_from_slice(encoded.as_slice());
    header
  }

  /// AC (§4.10a §8 "connection IDs"): both ends of a completed handshake derive the **same** connection
  /// id from the TLS exporter, two handshakes derive **different** ones (the id is a function of the
  /// session's secrets, so it is unique per session), and before completion the derivation is refused.
  /// Drives an in-process handshake between two QUIC connections to completion (no socket).
  fn drive_handshake(client: &mut Quic, server: &mut Quic) {
    for _ in 0..HANDSHAKE_TURN_CEILING {
      if !client.is_handshaking() && !server.is_handshaking() {
        break;
      }
      let mut to_server = Vec::new();
      client.write_hs(&mut to_server);
      if !to_server.is_empty() {
        server.read_hs(&to_server).unwrap();
      }
      let mut to_client = Vec::new();
      server.write_hs(&mut to_client);
      if !to_client.is_empty() {
        client.read_hs(&to_client).unwrap();
      }
    }
  }

  #[test]
  fn both_ends_derive_one_connection_id_per_session() {
    let identity = self_signed("slates-node");
    let (client, server) = connect(&identity, &identity, "slates-node").unwrap();
    let (mut client, mut server) = (Quic::Client(client), Quic::Server(server));
    assert!(
      client.export_connection_id().is_err(),
      "no id before the handshake completes"
    );
    drive_handshake(&mut client, &mut server);
    let at_client = client.export_connection_id().unwrap();
    let at_server = server.export_connection_id().unwrap();
    assert_eq!(at_client, at_server, "one id, derived at both ends");
    let (other_client, other_server) = connect(&identity, &identity, "slates-node").unwrap();
    let (mut other_client, mut other_server) =
      (Quic::Client(other_client), Quic::Server(other_server));
    drive_handshake(&mut other_client, &mut other_server);
    assert_ne!(
      other_client.export_connection_id().unwrap(),
      at_client,
      "another session, another id"
    );
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
      let wire = protect_packet(&client_keys, &CID, pn, None, &frames).unwrap();
      let plain = plaintext_header(pn);
      if wire[..plain.len()] != plain[..] {
        any_masked = true;
      }
      assert_eq!(
        connection_id_of(&wire),
        Some(CID),
        "the id rides in the clear, so a demultiplexer can route by it"
      );
      let (got_pn, got_frames) = unprotect_packet(&server_keys, &CID, rx_largest, &wire).unwrap();
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
    let wire = protect_packet(&client_keys, &CID, 3, None, &frames).unwrap();

    // Flip the last byte (inside the AEAD tag): opening must fail.
    let mut tampered = wire.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(
      unprotect_packet(&server_keys, &CID, 0, &tampered).is_err(),
      "a tampered packet must not open"
    );

    // A packet shorter than the header-protection sample is refused as not-ready, not a panic.
    assert!(matches!(
      unprotect_packet(&server_keys, &CID, 0, &wire[..4]),
      Err(EndpointError::NotReady)
    ));

    // A packet naming another session's id is refused at the header, before any crypto.
    let other: ConnectionId = [9; CONNECTION_ID_BYTES];
    assert!(matches!(
      unprotect_packet(&server_keys, &other, 0, &wire),
      Err(EndpointError::Header)
    ));
  }
}
