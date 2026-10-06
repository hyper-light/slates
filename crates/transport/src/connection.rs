//! The sans-io session-plane connection driver (§4.10a §8): the state machine that composes the
//! stream layer ([`crate::stream`]), the reliability core ([`crate::conn`]) and the flow-control
//! credit law ([`crate::flow`]) into *reliable, ordered, exactly-once* delivery of **many multiplexed
//! streams** over a lossy, reordering datagram path. It owns no socket and no clock: [`poll_transmit`]
//! yields the next packet's frames and its packet number, [`handle_incoming`] takes a received
//! packet's number and frames, and the caller (the [`crate::endpoint::Endpoint`]) does the I/O —
//! protecting, sending, receiving and unprotecting. Keeping the protocol logic sans-io is what lets
//! the oracle below drive loss and reorder *directly and deterministically*, with no OS network and no
//! injected-fault plumbing (the pattern quinn-proto and rustls follow, and the shape `session.rs`,
//! `stream.rs`, `conn.rs` and `flow.rs` are already written in).
//!
//! [`poll_transmit`]: Connection::poll_transmit
//! [`handle_incoming`]: Connection::handle_incoming
//!
//! Multiplexing: several streams share one connection (one handshake, one packet-number space, one
//! reliability and acknowledgement machine). Each carries an independent ordered byte sequence keyed by
//! stream id, with its own flow-control window; a send picks one fresh frame from the send streams in
//! round-robin so no stream starves another, and a received frame is demultiplexed to its stream's
//! reassembler. Packet numbers and loss recovery stay connection-wide — a lost packet's one frame is
//! retransmitted whatever stream it belonged to. What each carries (the reliable record classes of
//! §4.10a) is the caller's; here they are just byte streams.
//!
//! Time (the constrained-link design, research note §5.3; 2026-09-27): every entry point takes the caller's
//! clock. The connection owns the RTT estimator (RFC 9002 §5), declares loss by both of RFC 9002 §6.1's
//! thresholds (three packets, or 9/8 of the RTT), arms the probe timeout with its exponential backoff
//! (§6.2), detects persistent congestion (§7.6), feeds each acknowledgement's RTT sample and acknowledged
//! bytes to the congestion controller (`crate::congestion`, Copa) that sets the window, the
//! pacing rate and the send quantum, and paces ack-eliciting packets (`crate::pacer`). [`next_timeout`]
//! is the earliest of the loss timer, the probe timer and the pacing release; [`on_timeout`] acts on it.
//! Flow control is the ratified dual-level credit law (`crate::flow`), its window auto-tuned toward the
//! path's BDP up to the session's receive ceiling.
//!
//! [`next_timeout`]: Connection::next_timeout
//! [`on_timeout`]: Connection::on_timeout

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::congestion::{AckEvent, Controller, LossEvent};
use crate::conn::{
  AckGenerator, Lost, REORDER_THRESHOLD, SentPacket, SentTracker, ack_runs, tracked_bytes,
};
use crate::flow::FlowController;
use crate::pacer::Pacer;
use crate::pmtud::{BASE_PLPMTU, PathMtu, PathMtuStats};
use crate::reorder::Reordering;
use crate::rtt::RttEstimator;
use crate::session::{ACK_FRAME_BASE_BYTES, ACK_RANGE_BYTES, Frame, STREAM_FRAME_HEADER_BYTES};
use crate::stream::{StreamAssembler, StreamSender};
use crate::streams::{Arrival, ReplyAdmission, StreamSpace, priority};

/// Format: RFC 9002 §6.2.1 — the probe timeout doubles after each consecutive expiry; the most doublings
/// applied, enough that a one-millisecond timeout climbs past any initial PTO (`1 ms × 2^12 ≈ 4 s`), and
/// a shift no larger than this cannot overflow.
const PTO_BACKOFF_SHIFT_CAP: u32 = 12;

/// The initial receive-window credit, in bytes, for a frame cap of `max_frame_len`.
/// Derived: `(REORDER_THRESHOLD + 1) × max_frame_len` — the least in-flight data that keeps
/// reorder-based loss detection working. A loss is declared when `REORDER_THRESHOLD` later packets are
/// acknowledged past a gap (RFC 9002 §6.1.1); a window that admits one lost packet plus that many past
/// it lets the gap form. Anchored to `conn::REORDER_THRESHOLD` and the frame size. The window then
/// auto-tunes toward the path's BDP (`crate::flow`), up to the session's receive ceiling.
pub fn initial_receive_window(max_frame_len: usize) -> u64 {
  (REORDER_THRESHOLD + 1).saturating_mul(stream_bytes_per_packet(max_frame_len))
}

/// The connection credit a sender of `class` leaves unspent for the classes above it, for a frame cap of
/// `max_frame_len` (§4.10a; the constrained-link design §5.3: a control exchange never waits behind a bulk
/// one — in the packet schedule *or* in the connection's flow-control credit).
/// Derived: one packet's stream bytes per more urgent class — so the first packet of an exchange of any
/// class above finds credit waiting, however much the classes below have queued, and needs no credit update
/// from the peer (a round trip). The receiver advertises the largest of these, `class_credit_reserve(Bulk)`,
/// on top of its stream window ([`FlowController`]), so a lone bulk stream still has its whole stream
/// window: the reserve is headroom, never a cut. Both ends derive it from the same frame cap (R8). Until
/// 2026-09-30 one bulk stream could spend the last byte of connection credit, and a control ping on the
/// same session waited up to 68 ms of a 40 ms path for the peer's `MaxData`
/// (`docs/bugs/2026-09-30-bulk-spent-the-connection-credit-a-control-exchange-needed.md`).
pub fn class_credit_reserve(class: Priority, max_frame_len: usize) -> u64 {
  class
    .classes_above()
    .saturating_mul(stream_bytes_per_packet(max_frame_len))
}

/// The most stream data one packet of `packet_budget` encoded frame bytes carries: the budget less one
/// `Stream` frame's header.
pub fn stream_bytes_per_packet(packet_budget: usize) -> u64 {
  packet_budget
    .saturating_sub(STREAM_FRAME_HEADER_BYTES)
    .max(1) as u64
}

/// The smallest packet budget a connection works with: room for an acknowledgement with no extra ranges
/// plus the two credit frames that ride it (`MaxData`, `MaxStreams`), and for a `Stream` frame carrying at
/// least one byte. Derived: `max(ACK base + 2 × credit frame, Stream header + 1)` from the frame formats
/// (`crate::session`).
pub const MIN_PACKET_BUDGET: usize = {
  let ack = ACK_FRAME_BASE_BYTES + 2 * CREDIT_FRAME_BYTES;
  let stream = STREAM_FRAME_HEADER_BYTES + 1;
  if ack > stream { ack } else { stream }
};
/// The additional ACK ranges one acknowledgement carries at a packet budget of `packet_budget` bytes: the
/// budget less the ACK's base and the two credit frames riding with it (the connection credit and the
/// stream credit), over the bytes each range costs.
fn ack_ranges_for(packet_budget: u64) -> usize {
  usize::try_from(packet_budget)
    .unwrap_or(usize::MAX)
    .saturating_sub(ACK_FRAME_BASE_BYTES + 2 * CREDIT_FRAME_BYTES)
    / ACK_RANGE_BYTES
}

/// Format: a `MaxData` or `MaxStreams` frame's bytes (kind, one `u64`).
const CREDIT_FRAME_BYTES: usize = 1 + 8;
/// Format: a `MaxStreamData` frame's bytes (kind, stream id, max).
const STREAM_CREDIT_FRAME_BYTES: usize = 1 + 8 + 8;

pub use crate::streams::{Priority, Role, StreamCensus, StreamRefusal};

/// How many of the most recent probe copies stay tracked while a peer is silent
/// ([`Connection::queue_probe_copy`]).
/// Derived: two — persistent congestion (RFC 9002 §7.6.2) is judged from the span between the earliest and
/// the latest **lost** packets. When the path returns, the newest probe is the one acknowledged and the
/// one before it is the latest lost, so keeping those two keeps the span's endpoints — the originals (the
/// earliest) and that previous probe (the latest lost) — exactly as the RFC computes them; any older copy
/// only sits between them. Measured: keeping one collapsed the span to the originals and missed a 450 ms
/// blackout (`persistent_congestion_collapses_the_window`, 2026-09-28).
const PROBE_COPIES_KEPT: usize = 2;

/// One value per [`Priority`] class, reached by class without indexing (nothing here can go out of
/// bounds).
#[derive(Clone, Debug, Default)]
struct PerClass<T> {
  control: T,
  metadata: T,
  bulk: T,
}

impl<T> PerClass<T> {
  fn get(&self, class: Priority) -> &T {
    match class {
      Priority::Control => &self.control,
      Priority::Metadata => &self.metadata,
      Priority::Bulk => &self.bulk,
    }
  }

  fn get_mut(&mut self, class: Priority) -> &mut T {
    match class {
      Priority::Control => &mut self.control,
      Priority::Metadata => &mut self.metadata,
      Priority::Bulk => &mut self.bulk,
    }
  }
}

/// Everything a connection holds per stream, counted — the leak witness the tests assert returns to zero
/// once a session quiesces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectionCensus {
  /// The stream-id space's open and awaited streams.
  pub streams: StreamCensus,
  /// Send halves still sending or awaiting acknowledgement.
  pub send_streams: usize,
  /// Receive halves still reassembling.
  pub recv_streams: usize,
  /// Send streams with unacknowledged frames tracked.
  pub unacked: usize,
  /// Frames queued for retransmission.
  pub retransmit: usize,
  /// Stream control frames awaiting their first transmission.
  pub control: usize,
  /// Ack-eliciting packets in flight.
  pub in_flight: usize,
  /// Receive streams whose last advertised credit is remembered.
  pub credit_tracked: usize,
  /// The path-MTU probe in flight (`crate::pmtud`): at most one by construction — the search never sends a
  /// second while one is out — and connection-level, not exchange state, so a quiescent session may hold it
  /// (it resolves with the next traffic's acknowledgements). Not counted in `in_flight`.
  pub path_probe: usize,
  /// Send streams whose last blocked report is remembered.
  pub blocked_tracked: usize,
}

/// Why fresh sending stopped, counted over the connection's life — the evidence for what limits a sender
/// (its congestion window, the pacer, the peer's credit, or simply no data), read by the bake-offs and the
/// tests that ask why a flow is slow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendStops {
  /// The packet budget was filled (the normal, unconstrained stop).
  pub budget: u64,
  /// The congestion window was full.
  pub window: u64,
  /// The pacer held the next packet.
  pub pacer: u64,
  /// The connection credit every class with data may spend was spent (a class leaves the classes above it
  /// their reserve, [`class_credit_reserve`]).
  pub credit: u64,
  /// No stream had data it could send (none at all, or none within its stream credit).
  pub empty: u64,
  /// Streams the fresh-frame scheduler examined (the work witness: it must not grow with the streams that
  /// have nothing to send).
  pub examined: u64,
}

/// What a connection is built with: the packet budget it frames at (the datagram payload one packet
/// carries), the initial receive window both ends derive, and the ceiling that window may auto-tune to (the
/// session's receive memory budget).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionShape {
  /// The most stream bytes one packet carries.
  pub max_datagram: u64,
  /// The initial receive window, bytes.
  pub initial_window: u64,
  /// The receive window's ceiling, bytes.
  pub receive_ceiling: u64,
}

impl ConnectionShape {
  /// The shape for frames of `max_frame_len` bytes: the derived initial window and a ceiling of
  /// `receive_ceiling` bytes.
  pub fn for_frame_cap(max_frame_len: usize, receive_ceiling: u64) -> ConnectionShape {
    ConnectionShape {
      max_datagram: max_frame_len as u64,
      initial_window: initial_receive_window(max_frame_len),
      receive_ceiling,
    }
  }
}

/// One end of a reliable, flow-controlled, congestion-controlled, paced, multi-stream connection. The send
/// side is a set of streams (each a source bounded by the credit the peer advertises for it) served in
/// strict priority by class and round-robin within a class, plus the connection-wide in-flight tracker and retransmission queue; the receive side is
/// a set of ordered reassemblers keyed by stream id, the acknowledgement generator, and the flow-control
/// accounting that advertises each stream's credit a window ahead of what has been read from it.
pub struct Connection {
  /// The send streams, keyed by id (each framed only within the credit the peer has advertised for it).
  send_streams: BTreeMap<u64, StreamSender>,
  /// By class, the send streams that can frame data now (within the peer's stream credit, with bytes or
  /// a final frame inside their own credit), as (install order, id): the order the connection took them on,
  /// replies and its own exchanges alike. A stream joins
  /// when it becomes sendable (opened, granted credit, admitted by the peer's stream credit) and leaves when
  /// framing empties it or it is forgotten, so a fresh frame costs O(log n) whatever the streams with
  /// nothing to send; the scan it replaced stepped over every one of them for each frame.
  ready: PerClass<std::collections::BTreeSet<(u64, u64)>>,
  /// By class, the stream served last: the next is the first ready one after it in install order,
  /// wrapping. A finished stream resets it, so service restarts at the oldest, as the scan did; a pure
  /// rotation that never restarted was measured and lost (control p99 ×1.37 at 100 Mbit/s, 20 ms, 1 %
  /// loss: completion time favours first-come service over processor sharing among similar sizes). The
  /// scan's cursor was an index that wrapped when it served the last stream, so streams installed after
  /// that waited a whole cycle; serving after the last served instead measured control p99 ×0.989,
  /// metadata ×0.915 and bulk ×1.024 over the 20-seed class grid (docs/wip/BENCHMARKS.md, 2026-10-06).
  served: PerClass<Option<(u64, u64)>>,
  /// The install order the next send stream takes.
  next_installed: u64,
  /// The service rule computed the slow way, in tests: send stream ids by class in install order and the
  /// one served last (none after a completion); the frames whose choice differed from the ready sets'.
  #[cfg(test)]
  scan: PerClass<(Vec<u64>, Option<u64>)>,
  #[cfg(test)]
  scan_differed: u64,
  /// Stream control frames (`ResetStream`, `StopSending`, `MaxStreams`) awaiting their first
  /// transmission; each leaves in the next packet and is retransmitted on loss like stream data.
  control: VecDeque<Frame>,
  /// Each send stream's frames not yet acknowledged, by offset: a frame's acknowledgement (through any copy
  /// of it — a retransmission or a probe) removes it, a lost copy of an acknowledged frame is not resent,
  /// and a drained stream with none left is complete and released.
  unacked: BTreeMap<u64, std::collections::BTreeSet<u64>>,
  /// The stream-id space: which streams are open or closed, and the stream credit each end extends
  /// (`crate::streams`).
  streams: StreamSpace,
  /// Connection-wide in-flight packet tracking and loss detection (packet numbers are per-connection).
  sent: SentTracker,
  /// Frames freed by loss detection or a probe, awaiting retransmission.
  retransmit: VecDeque<Frame>,
  /// The receive reassemblers, keyed by stream id (created when a stream's first frame arrives).
  recv_streams: BTreeMap<u64, StreamAssembler>,
  /// Receive streams a stream frame reached since the reader last took them ([`Connection::take_readable`]): the
  /// endpoint reads only these, where it once read every open stream at every drain — a fetch of 1,024 chunks keeps
  /// about that many reply streams open, so each packet cost a pass over all of them (2026-10-05, `fetch_bench` at
  /// 64 MiB: 68% of the samples in `Endpoint::drain`).
  readable: BTreeSet<u64>,
  /// Receive streams whose credit may have moved since it was last advertised: added when a stream opens and when a
  /// read advances it, every open stream when the window grows; consulted instead of every open stream on every
  /// packet (the same 64 MiB fetch then spent most of its time comparing each stream's credit in `poll_transmit`).
  credit_dirty: BTreeSet<u64>,
  /// Which packet numbers have arrived, and the acknowledgement to send back.
  acks: AckGenerator,
  /// The receive-side flow-control accounting (per stream): advertises credit a window ahead of reads.
  flow: FlowController,
  /// The initial receive window (what each new stream's sender starts with, R8).
  initial_window: u64,
  /// Set when an ack-eliciting packet has arrived and its acknowledgement has not yet been sent.
  ack_owed: bool,
  /// How many frames this end has retransmitted (the non-vacuity counter for the loss-recovery path).
  retransmitted: u64,
  /// The congestion controller.
  controller: Controller,
  /// The bytes in flight (sent, not yet acknowledged, declared lost, or forgotten).
  in_flight: u64,
  /// The RTT estimator (RFC 9002 §5).
  rtt: RttEstimator,
  /// When the first RTT sample was taken (persistent congestion needs losses after it, §7.6.2).
  first_rtt_sample_at: Option<u64>,
  /// The pacer (`crate::pacer`).
  pacer: Pacer,
  /// When the pacer next releases a packet, while ack-eliciting data waits on it.
  pacing_release: Option<u64>,
  /// The earliest time an unacknowledged packet crosses the time threshold (RFC 9002 §6.1.2).
  loss_time: Option<u64>,
  /// Consecutive probe timeouts without an acknowledgement (RFC 9002 §6.2.1's `pto_count`).
  pto_count: u32,
  /// Ack-eliciting packets the probe timeout allows past the window and the pacer (RFC 9002 §6.2.4).
  probes_owed: u32,
  /// The packet number at which the sender last found its window full with data waiting — the sender was
  /// window-limited for any packet acknowledged below it (`C.is_cwnd_limited`, draft-ietf-ccwg-bbr §2.2).
  window_full_at: Option<u64>,
  /// Send side: total fresh stream bytes sent across all streams (never counting a retransmission),
  /// the connection-wide counterpart to each stream's send offset. Held below `peer_max_data`.
  connection_sent: u64,
  /// Send side: the connection-wide flow-control ceiling the peer has advertised (`MaxData`).
  peer_max_data: u64,
  /// The frame cap the class credit reserves are derived from ([`class_credit_reserve`]) — the shape's, fixed
  /// for the session, so both ends derive the same reserve whatever the path MTU search does later.
  reserve_frame_cap: usize,
  /// ACK-of-ACK bookkeeping (RFC 9000 §13.2.4): for each ack-eliciting packet this end sent that also
  /// carried an acknowledgement, the largest packet number that acknowledgement covered.
  sent_acks: BTreeMap<u64, u64>,
  /// Received packets discarded as duplicates of a packet number already processed (RFC 9000 §12.3).
  duplicates: u64,
  /// Stream frames the peer sent in breach of flow control or a stream's final size (RFC 9000 §4.1,
  /// §4.5), dropped and counted.
  violations: u64,
  /// Streams this end has reset whose `ResetStream` the peer has not yet acknowledged — owed to the peer
  /// as much as unacknowledged stream data is (the peer is waiting on the stream until it learns).
  resets_owed: std::collections::BTreeSet<u64>,
  /// The probe packets that carried the most recent copies of the oldest packet, oldest first — at most
  /// [`PROBE_COPIES_KEPT`] ([`Connection::queue_probe_copy`]) — and whether a copy is queued for the next
  /// probe.
  probe_copies: VecDeque<u64>,
  probe_copy_pending: bool,
  /// Why fresh sending stopped, counted ([`Connection::send_stops`]).
  stops: SendStops,
  /// The stream credit last advertised for each receive stream: a stream whose credit moved since is
  /// advertised first. Held only for open receive streams (removed as each closes).
  credit_sent: BTreeMap<u64, u64>,
  /// The receive stream the rotating credit refresh reached last: every stream's credit is re-advertised
  /// in turn as room allows, so a credit carried by a lost acknowledgement is always sent again.
  credit_cursor: u64,
  /// The connection credit limit a `DataBlocked` was last sent for, and each send stream's limit a
  /// `StreamDataBlocked` was last sent for (held only for open send streams) — each is sent once per limit.
  blocked_sent: Option<u64>,
  stream_blocked_sent: BTreeMap<u64, u64>,
  /// The peer's stream credit a `StreamsBlocked` was last sent for.
  streams_blocked_sent: Option<u64>,
  /// Path MTU discovery (`crate::pmtud`), once the peer's largest datagram is known
  /// ([`Connection::enable_path_mtu`]); until then the connection frames at its shape's budget.
  path_mtu: Option<PathMtu>,
  /// The adaptive reordering tolerance (`crate::reorder`): the loss thresholds widened by spurious losses.
  reordering: Reordering,
}

/// What one packet has used so far: its encoded frame bytes (against the datagram budget) and its stream
/// data bytes (against the congestion window).
#[derive(Clone, Copy, Debug, Default)]
struct Packing {
  used: u64,
  data: u64,
}

/// Why fresh data stopped being framed in one `poll_transmit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FreshStop {
  /// The packet budget was filled.
  Budget,
  /// The congestion window was full.
  Window,
  /// The pacer holds the next packet until the given time.
  Pacer(u64),
  /// The connection credit every class with data may spend is spent.
  Credit,
  /// No stream has data within its credit.
  Empty,
}

impl Connection {
  /// A fresh connection of `shape`, playing `role`.
  pub fn new(shape: ConnectionShape, role: Role) -> Connection {
    let reserve_frame_cap = usize::try_from(shape.max_datagram).unwrap_or(usize::MAX);
    Connection {
      send_streams: BTreeMap::new(),
      ready: PerClass::default(),
      served: PerClass::default(),
      next_installed: 0,
      #[cfg(test)]
      scan: PerClass::default(),
      #[cfg(test)]
      scan_differed: 0,
      control: VecDeque::new(),
      unacked: BTreeMap::new(),
      // Derived: one stream per packet's worth of stream data within the session's receive budget — the
      // receive ceiling over the stream bytes a packet carries — so the peer's open streams can never hold
      // more than the budget in reassembly state; both ends derive the same limit from the same shape (R8).
      streams: StreamSpace::new(
        role,
        shape.receive_ceiling
          / stream_bytes_per_packet(usize::try_from(shape.max_datagram).unwrap_or(usize::MAX)),
      ),
      sent: SentTracker::new(),
      retransmit: VecDeque::new(),
      recv_streams: BTreeMap::new(),
      readable: BTreeSet::new(),
      credit_dirty: BTreeSet::new(),
      // The receive side holds as many ranges as one acknowledgement can report (its first range plus
      // the additional ones that fit the packet budget): beyond them a range could never be acknowledged
      // anyway, and the generator stays bounded however many packets arrive.
      acks: AckGenerator::new(ack_ranges_for(shape.max_datagram).saturating_add(1)),
      flow: FlowController::new(
        shape.initial_window,
        shape.receive_ceiling,
        class_credit_reserve(Priority::Bulk, reserve_frame_cap),
      ),
      initial_window: shape.initial_window,
      ack_owed: false,
      retransmitted: 0,
      controller: Controller::new(shape.max_datagram),
      in_flight: 0,
      rtt: RttEstimator::new(),
      first_rtt_sample_at: None,
      pacer: Pacer::new(),
      pacing_release: None,
      loss_time: None,
      pto_count: 0,
      probes_owed: 0,
      window_full_at: None,
      connection_sent: 0,
      // The peer's initial connection credit is what it advertises before any read — the initial window
      // and the class reserve on top of it; both ends derive the same one (R8).
      peer_max_data: shape
        .initial_window
        .saturating_add(class_credit_reserve(Priority::Bulk, reserve_frame_cap)),
      reserve_frame_cap,
      sent_acks: BTreeMap::new(),
      duplicates: 0,
      violations: 0,
      resets_owed: std::collections::BTreeSet::new(),
      probe_copies: VecDeque::new(),
      probe_copy_pending: false,
      stops: SendStops::default(),
      credit_sent: BTreeMap::new(),
      credit_cursor: 0,
      blocked_sent: None,
      stream_blocked_sent: BTreeMap::new(),
      streams_blocked_sent: None,
      path_mtu: None,
      reordering: Reordering::default(),
    }
  }

  /// Starts path MTU discovery (`crate::pmtud`) toward a peer that reads datagrams of up to `peer_max`
  /// bytes (its transport parameters, `crate::params`). Idempotent: a search already running is kept.
  pub fn enable_path_mtu(&mut self, peer_max: usize) {
    if self.path_mtu.is_none() {
      self.path_mtu = Some(PathMtu::new(peer_max));
    }
  }

  /// The frame bytes a packet carries now: the confirmed path MTU's budget once discovery runs, never less
  /// than `base` (the shape's budget, which fits the floor every path carries).
  pub fn packet_budget(&self, base: usize) -> usize {
    self.path_mtu.as_ref().map_or(base, |pmtu| {
      crate::endpoint::packet_budget_for(pmtu.current()).max(base)
    })
  }

  /// The confirmed path MTU, once discovery runs.
  pub fn path_mtu(&self) -> Option<usize> {
    self.path_mtu.as_ref().map(PathMtu::current)
  }

  /// What path MTU discovery has counted, once it runs.
  pub fn path_mtu_stats(&self) -> Option<PathMtuStats> {
    self.path_mtu.as_ref().map(PathMtu::stats)
  }

  /// The path-MTU probe due at `now`, as `(packet_number, datagram_bytes)`: a packet carrying one `Ping`,
  /// which the caller pads to exactly that size. Tracked in flight (so its acknowledgement or loss is seen)
  /// with no stream bytes, so it takes no room in the congestion window; its loss is kept out of the
  /// controller (RFC 9000 §14.4). `None` when no probe is due.
  pub fn poll_probe(&mut self, now: u64) -> Option<(u64, usize)> {
    // A spent packet-number space sends nothing, before any state moves (the session ends: AUD-29-27).
    if self.sent.exhausted() {
      return None;
    }
    let size = self.path_mtu.as_mut()?.next_probe(now)?;
    let pn = self.sent.next_pn()?;
    self.sent.on_sent(
      pn,
      vec![Frame::Ping],
      now,
      u64::try_from(size).unwrap_or(u64::MAX),
    );
    if let Some(pmtu) = self.path_mtu.as_mut() {
      pmtu.on_probe_sent(pn, size);
    }
    Some((pn, size))
  }

  /// The local stack refused the probe `pn`'s datagram as too large at `now`: it was never sent, and its
  /// size bounds the search.
  pub fn on_probe_refused(&mut self, pn: u64, now: u64) {
    self.sent.forget_unsent(pn);
    if let Some(pmtu) = self.path_mtu.as_mut() {
      pmtu.on_probe_refused(pn, now);
    }
  }

  /// The most of this end's streams that may wait past the peer's credit (`StreamSpace::limit`).
  pub fn stream_limit(&self) -> u64 {
    self.streams.limit()
  }

  /// Notes whether exchanges wait unbound for the stream credit (`StreamSpace::set_wanting`).
  pub fn set_streams_wanted(&mut self, wanted: bool) {
    self.streams.set_wanting(wanted);
  }

  /// Whether an exchange of class `priority` may open its stream now (`StreamSpace::admits`).
  pub fn admits(&self, priority: Priority) -> bool {
    self.streams.admits(priority)
  }

  /// Opens this end's next stream, of request `kind` in class `priority`, carrying the whole of `data`
  /// (finished), and returns its id; the peer's reply arrives on the same id. The stream waits unsent
  /// while the peer's stream credit does not cover it, and a limit's worth waiting is a typed refusal
  /// (`crate::streams`). The send side starts with the initial window of credit (both ends derive the
  /// same window, R8); more is granted only as the peer's `MaxStreamData` for it arrives.
  pub fn open_exchange(
    &mut self,
    kind: u64,
    priority: Priority,
    data: &[u8],
  ) -> Result<u64, StreamRefusal> {
    let stream_id = self.streams.open_local(kind, priority)?;
    self.install_sender(stream_id, data);
    Ok(stream_id)
  }

  /// Replies on the peer's stream `stream_id` with the whole of `data`, in the class the stream carries.
  /// A stream the peer has already stopped or reset takes the reply as moot (dropped, `Ok`); a stream the
  /// peer never opened, or a second reply, is a typed refusal.
  pub fn reply(&mut self, stream_id: u64, data: &[u8]) -> Result<(), StreamRefusal> {
    match self.streams.begin_reply(stream_id)? {
      ReplyAdmission::Send => {
        self.install_sender(stream_id, data);
        Ok(())
      }
      ReplyAdmission::Moot => Ok(()),
    }
  }

  fn install_sender(&mut self, stream_id: u64, data: &[u8]) {
    let mut sender = StreamSender::new();
    sender.write(data);
    sender.grant_credit(self.initial_window);
    sender.finish();
    sender.set_installed(self.next_installed);
    self.next_installed = self.next_installed.saturating_add(1);
    #[cfg(test)]
    if !self.send_streams.contains_key(&stream_id) {
      self.scan.get_mut(priority(stream_id)).0.push(stream_id);
    }
    if self.send_streams.insert(stream_id, sender).is_none() {
      self.requeue(stream_id);
    }
  }

  /// Puts `stream_id` in its class's ready set if it can frame data now, or takes it out if it cannot.
  fn requeue(&mut self, stream_id: u64) {
    let Some(sender) = self.send_streams.get(&stream_id) else {
      return;
    };
    let entry = (sender.installed(), stream_id);
    let sendable = self.streams.sendable(stream_id) && sender.has_sendable();
    let ready = self.ready.get_mut(priority(stream_id));
    if sendable {
      ready.insert(entry);
    } else {
      ready.remove(&entry);
    }
  }

  /// Abandons this end's sending half of `stream_id` (RFC 9000 §3.1, §19.4): nothing more of it is sent
  /// or retransmitted, its bytes leave flight, and a `ResetStream` tells the peer to discard what it holds.
  /// A peer's stream this end has not replied on yet is reset at size zero, so the peer stops waiting for
  /// a reply that will not come. Nothing happens for a half already done.
  pub fn reset_stream(&mut self, stream_id: u64) {
    if !self.streams.send_open(stream_id) {
      return;
    }
    let final_size = self
      .send_streams
      .get(&stream_id)
      .map_or(0, StreamSender::send_offset);
    self.forget_send_stream(stream_id);
    self.resets_owed.insert(stream_id);
    self.control.push_back(Frame::ResetStream {
      stream_id,
      final_size,
    });
    self.streams.close_send(stream_id);
  }

  /// Abandons this end's receiving half of `stream_id` (RFC 9000 §3.5, §19.5): what arrived is discarded,
  /// its credit returned, and a `StopSending` asks the peer to stop sending — an abandoned exchange's reply
  /// stops costing the path. Nothing happens for a half already done.
  pub fn stop_sending(&mut self, stream_id: u64) {
    if !self.streams.receiving(stream_id) {
      return;
    }
    self.discard_recv(stream_id, None);
    self.control.push_back(Frame::StopSending { stream_id });
  }

  /// Seeds the RTT estimator with a round trip measured outside the connection (the handshake's first
  /// flight, RFC 9002 §5.1), at `now`.
  pub fn seed_rtt(&mut self, sample: u64, now: u64) {
    self.rtt.on_sample(sample, 0);
    self.first_rtt_sample_at.get_or_insert(now);
  }

  /// The next packet to send at `now`, as `(packet_number, frames)`, or `None` when there is nothing to
  /// send right now. Fills the packet up to `packet_budget` stream bytes with several ack-eliciting frames
  /// (RFC 9000 §12.2): queued retransmissions first, then fresh frames round-robin, while the congestion
  /// window, the pacer and the connection credit allow — a probe the timeout owed may pass the window and
  /// the pacer (RFC 9002 §6.2.4); then, if an acknowledgement is owed, the acknowledgement followed by the
  /// current receive credit. An acknowledgement-only packet is neither paced nor window-limited.
  pub fn poll_transmit(&mut self, now: u64, packet_budget: usize) -> Option<(u64, Vec<Frame>)> {
    // A spent packet-number space sends nothing, before any frame is taken from its queue (AUD-29-27).
    if self.sent.exhausted() {
      return None;
    }
    let budget = packet_budget as u64;
    // With nothing recoverable in flight no probe timer runs, so a blocked sender reports itself (a lone
    // path-MTU probe carries nothing and does not count: it once silenced this report and deadlocked a
    // credit-blocked session, `docs/bugs/2026-09-28-a-lone-path-probe-silenced-the-blocked-report.md`).
    if self.recoverable_in_flight() == 0 {
      self.note_blocked();
    }
    let probe = self.probes_owed > 0;
    let mut packing = Packing::default();
    // The acknowledgement leads (sized to fit, with the connection credit), then stream control frames,
    // then retransmissions and fresh data, then as much stream credit as the room left holds — every frame
    // counted by its encoded length, so the packet never exceeds its budget (the datagram's floor).
    let mut frames = Vec::new();
    let ack_largest = self.push_acknowledgement(&mut frames, &mut packing, budget);
    let carries_ack = !frames.is_empty();
    let mut reliable: Vec<Frame> = Vec::new();
    self.take_control(&mut reliable, &mut packing, budget);
    let stop = match self.pacing_hold(now, budget, probe) {
      Some(release) => FreshStop::Pacer(release),
      None => self
        .take_retransmissions(budget, probe, &mut reliable, &mut packing)
        .unwrap_or_else(|| self.take_fresh(budget, probe, &mut reliable, &mut packing)),
    };
    self.note_stop(stop);
    frames.extend(reliable.iter().cloned());
    if carries_ack || self.credit_moved() {
      self.push_stream_credit(&mut frames, &mut packing, budget);
    }
    if frames.is_empty() {
      return None;
    }
    let pn = self.sent.next_pn()?;
    if !reliable.is_empty() {
      // The datagram's size on the wire: the frames plus the packet's fixed overhead (an upper bound — the
      // packet number may encode shorter than its longest form).
      let size = packing
        .used
        .saturating_add(crate::endpoint::PACKET_OVERHEAD_BYTES as u64);
      self.record_sent(now, pn, reliable, probe, ack_largest, size);
    }
    Some((pn, frames))
  }

  /// With nothing in flight no probe timer runs, so a sender blocked by credit would wait forever if the
  /// acknowledgement that carried fresh credit was lost: it tells the peer instead (RFC 9000 §19.12-13),
  /// with a reliable, ack-eliciting `DataBlocked` / `StreamDataBlocked`, whose acknowledgement carries the
  /// credit. Each is sent once per limit; a raised limit that blocks again is reported again.
  fn note_blocked(&mut self) {
    if self.connection_credit_blocked() && self.blocked_sent != Some(self.peer_max_data) {
      self.blocked_sent = Some(self.peer_max_data);
      self.control.push_back(Frame::DataBlocked {
        limit: self.peer_max_data,
      });
    }
    if let Some(limit) = self.streams.waiting_limit()
      && self.streams_blocked_sent != Some(limit)
    {
      self.streams_blocked_sent = Some(limit);
      self.control.push_back(Frame::StreamsBlocked { limit });
    }
    let newly_blocked: Vec<(u64, u64)> = self
      .send_streams
      .iter()
      .filter(|&(&stream_id, _)| self.streams.sendable(stream_id))
      .filter_map(|(&stream_id, sender)| sender.blocked_at().map(|limit| (stream_id, limit)))
      .filter(|(stream_id, limit)| self.stream_blocked_sent.get(stream_id) != Some(limit))
      .collect();
    for (stream_id, limit) in newly_blocked {
      self.stream_blocked_sent.insert(stream_id, limit);
      self
        .control
        .push_back(Frame::StreamDataBlocked { stream_id, limit });
    }
  }

  /// Moves queued stream control frames into the packet while they fit.
  fn take_control(&mut self, reliable: &mut Vec<Frame>, packing: &mut Packing, budget: u64) {
    while let Some(front) = self.control.front() {
      let bytes = front.encoded_len() as u64;
      if packing.used.saturating_add(bytes) > budget {
        return;
      }
      if let Some(frame) = self.control.pop_front() {
        packing.used = packing.used.saturating_add(bytes);
        reliable.push(frame);
      }
    }
  }

  /// When ack-eliciting data waits and no probe is owed: the pacer's release time if it holds the next
  /// packet (sized at the budget it may fill), else `None`.
  fn pacing_hold(&mut self, now: u64, budget: u64, probe: bool) -> Option<u64> {
    if probe || (self.retransmit.is_empty() && !self.has_fresh_data()) {
      return None;
    }
    let rate = self.controller.pacing_rate(&self.rtt).max(1);
    let quantum = self.controller.send_quantum(&self.rtt);
    let release = self.pacer.release_time(now, budget, rate, quantum);
    (release > now).then_some(release)
  }

  /// Whether `bytes` more fit the congestion window over what is in flight and already `packed` (a lone
  /// packet always fits when nothing is in flight, RFC 9002 §7.5, so the window never deadlocks).
  fn window_room(&self, packed: u64, bytes: u64) -> bool {
    let in_flight = self.in_flight.saturating_add(packed);
    in_flight == 0 || in_flight.saturating_add(bytes) <= self.controller.window()
  }

  /// Moves queued retransmissions into the packet while the budget and window allow (a probe passes the
  /// window). Returns why it stopped if that ends the packet, `None` to go on to fresh data.
  fn take_retransmissions(
    &mut self,
    budget: u64,
    probe: bool,
    reliable: &mut Vec<Frame>,
    packing: &mut Packing,
  ) -> Option<FreshStop> {
    loop {
      let front = self.retransmit.front()?;
      let mut bytes = front.encoded_len() as u64;
      if bytes > budget {
        // A frame no packet of this budget can hold — framed for a larger path MTU before a black hole
        // fell the session back (`crate::pmtud`) — is split at a stream offset so the head fills this
        // packet and the tail waits first in the queue; without the split it would stop every packet here.
        self.split_front_retransmission(budget.saturating_sub(packing.used));
        bytes = self
          .retransmit
          .front()
          .map_or(0, |frame| frame.encoded_len() as u64);
      }
      let front = self.retransmit.front()?;
      let data = tracked_bytes(front);
      if packing.used.saturating_add(bytes) > budget {
        return Some(FreshStop::Budget);
      }
      if !probe && !self.window_room(packing.data, data) {
        return Some(FreshStop::Window);
      }
      let frame = self.retransmit.pop_front()?;
      packing.used = packing.used.saturating_add(bytes);
      packing.data = packing.data.saturating_add(data);
      reliable.push(frame);
    }
  }

  /// Splits the queued retransmission at the front, a `Stream` frame, so its head fits `room` encoded bytes:
  /// the head (the same offset, never the `fin`) stays first, the tail (the rest, with the `fin`) second.
  /// The tail's offset is recorded as owed at once, so the stream is not taken for delivered when the head
  /// is acknowledged before the tail is sent. Nothing changes for any other frame or a room too small to
  /// carry a byte.
  fn split_front_retransmission(&mut self, room: u64) {
    let Some(head_len) = usize::try_from(room)
      .ok()
      .and_then(|room| room.checked_sub(STREAM_FRAME_HEADER_BYTES))
      .filter(|&len| len > 0)
    else {
      return;
    };
    let Some(Frame::Stream {
      stream_id,
      offset,
      fin,
      data,
    }) = self.retransmit.front()
    else {
      return;
    };
    if data.len() <= head_len {
      return;
    }
    let (stream_id, offset, fin) = (*stream_id, *offset, *fin);
    let (Some(head_bytes), Some(tail_bytes)) = (data.get(..head_len), data.get(head_len..)) else {
      return;
    };
    let tail_offset = offset.saturating_add(u64::try_from(head_len).unwrap_or(u64::MAX));
    let head = Frame::Stream {
      stream_id,
      offset,
      fin: false,
      data: head_bytes.to_vec(),
    };
    let tail = Frame::Stream {
      stream_id,
      offset: tail_offset,
      fin,
      data: tail_bytes.to_vec(),
    };
    self.retransmit.pop_front();
    self.retransmit.push_front(tail);
    self.retransmit.push_front(head);
    if let Some(offsets) = self.unacked.get_mut(&stream_id) {
      offsets.insert(tail_offset);
    }
  }

  /// Frames fresh stream data into the packet while the budget, the window (a probe passes it) and the
  /// peer's connection credit allow; returns why it stopped.
  fn take_fresh(
    &mut self,
    budget: u64,
    probe: bool,
    reliable: &mut Vec<Frame>,
    packing: &mut Packing,
  ) -> FreshStop {
    loop {
      let room = budget.saturating_sub(packing.used);
      let Some(data_room) = room
        .checked_sub(STREAM_FRAME_HEADER_BYTES as u64)
        .filter(|&d| d > 0)
      else {
        return FreshStop::Budget;
      };
      // The most any class may spend: the most urgent class leaves no reserve.
      let connection_credit = self.class_credit(Priority::Control);
      if connection_credit == 0 {
        return FreshStop::Credit;
      }
      let cap = data_room.min(connection_credit);
      if !probe && !self.window_room(packing.data, cap) {
        return FreshStop::Window;
      }
      let frame = match self.next_fresh_frame(data_room) {
        Ok(frame) => frame,
        Err(stop) => return stop,
      };
      let data = tracked_bytes(&frame);
      self.connection_sent = self.connection_sent.saturating_add(data);
      packing.used = packing.used.saturating_add(frame.encoded_len() as u64);
      packing.data = packing.data.saturating_add(data);
      reliable.push(frame);
    }
  }

  /// Records why sending stopped: a full window with data waiting marks the sender window-limited (so the
  /// controller may grow the window, RFC 9002 §7.8); the pacer's hold is remembered as the next release.
  fn note_stop(&mut self, stop: FreshStop) {
    let counter = match stop {
      FreshStop::Budget => &mut self.stops.budget,
      FreshStop::Window => &mut self.stops.window,
      FreshStop::Pacer(_) => &mut self.stops.pacer,
      FreshStop::Credit => &mut self.stops.credit,
      FreshStop::Empty => &mut self.stops.empty,
    };
    *counter = counter.saturating_add(1);
    match stop {
      FreshStop::Window if self.has_fresh_data() || !self.retransmit.is_empty() => {
        self.window_full_at = Some(self.sent.peek_next_pn());
      }
      _ => {}
    }
    self.pacing_release = match stop {
      FreshStop::Pacer(release) => Some(release),
      _ => None,
    };
  }

  /// Tracks ack-eliciting packet `pn` carrying `reliable`, sent at `now`: the controller's and sampler's
  /// send hooks, the pacer's tokens (a probe spends none), the bytes in flight, and ACK-of-ACK.
  fn record_sent(
    &mut self,
    now: u64,
    pn: u64,
    reliable: Vec<Frame>,
    probe: bool,
    ack_largest: Option<u64>,
    size: u64,
  ) {
    if let Some(largest) = ack_largest {
      self.sent_acks.insert(pn, largest);
    }
    let bytes: u64 = reliable.iter().map(tracked_bytes).sum();
    for frame in &reliable {
      if let Frame::Stream {
        stream_id, offset, ..
      } = frame
        && self.send_streams.contains_key(stream_id)
      {
        self.unacked.entry(*stream_id).or_default().insert(*offset);
      }
    }
    if probe {
      if self.probe_copy_pending {
        self.probe_copies.push_back(pn);
        self.probe_copy_pending = false;
      }
      self.probes_owed = self.probes_owed.saturating_sub(1);
    } else {
      let rate = self.controller.pacing_rate(&self.rtt).max(1);
      let quantum = self.controller.send_quantum(&self.rtt);
      self.pacer.on_sent(now, bytes, rate, quantum);
    }
    self.in_flight = self.in_flight.saturating_add(bytes);
    self.sent.on_sent(pn, reliable, now, size);
  }

  /// Whether any send stream has data it could frame within its credit.
  fn has_fresh_data(&self) -> bool {
    self
      .send_streams
      .iter()
      .any(|(&stream_id, sender)| self.streams.sendable(stream_id) && sender.has_sendable())
  }

  /// If an acknowledgement is owed, appends it — followed by the connection-wide credit and each
  /// received stream's credit, piggybacked so a lost credit frame re-advertises with the next
  /// acknowledgement — to `frames`, clears the owed flag, and returns the largest packet number the
  /// acknowledgement covered (for ACK-of-ACK bookkeeping). `None` when none is owed or generated.
  fn push_acknowledgement(
    &mut self,
    frames: &mut Vec<Frame>,
    packing: &mut Packing,
    budget: u64,
  ) -> Option<u64> {
    if !self.ack_owed {
      return None;
    }
    // As many additional ranges as fit beside the acknowledgement's base and the connection credit;
    // beyond them older received runs are retransmitted and deduped rather than acknowledged.
    let ack = self.acks.ack_frame(ack_ranges_for(budget))?;
    let largest = if let Frame::Ack { largest, .. } = ack {
      Some(largest)
    } else {
      None
    };
    // The connection credit and the stream credit ride every acknowledgement, never an ack-eliciting packet
    // of their own: a lost one is superseded by the next, and a blocked sender's report forces one.
    let credit = self.flow.connection_credit_frame();
    let streams = Frame::MaxStreams {
      max: self.streams.credit(),
    };
    packing.used = packing
      .used
      .saturating_add((ack.encoded_len() + credit.encoded_len() + streams.encoded_len()) as u64);
    frames.push(ack);
    frames.push(credit);
    frames.push(streams);
    self.ack_owed = false;
    largest
  }

  /// Whether any receive stream's credit moved since it was last advertised — such credit is sent even in
  /// a packet of its own (a credit-only packet, like an acknowledgement, is not ack-eliciting), so a small
  /// packet budget whose acknowledgement leaves no room beside it never strands a credit-blocked peer.
  fn credit_moved(&self) -> bool {
    self
      .credit_dirty
      .iter()
      .any(|&stream_id| self.credit_sent.get(&stream_id) != Some(&self.flow.stream_max(stream_id)))
  }

  /// Adds as much stream credit as the packet's room holds: first every receive stream whose credit moved
  /// since it was last advertised, then the rest in rotation from where the last refresh stopped — so the
  /// frames per packet stay bounded by the room, never by the number of open streams, and a credit carried
  /// by a lost acknowledgement is re-advertised within a rotation.
  fn push_stream_credit(&mut self, frames: &mut Vec<Frame>, packing: &mut Packing, budget: u64) {
    let room = usize::try_from(budget.saturating_sub(packing.used)).unwrap_or(0);
    let mut slots = room / STREAM_CREDIT_FRAME_BYTES;
    if slots == 0 {
      return;
    }
    let mut chosen: Vec<u64> = Vec::new();
    let mut current: Vec<u64> = Vec::new();
    for &stream_id in &self.credit_dirty {
      if slots == 0 {
        break;
      }
      if self.credit_sent.get(&stream_id) != Some(&self.flow.stream_max(stream_id)) {
        chosen.push(stream_id);
        slots -= 1;
      } else {
        current.push(stream_id);
      }
    }
    // Advertised now, or found already advertised: no longer owed.
    for stream_id in chosen.iter().chain(&current) {
      self.credit_dirty.remove(stream_id);
    }
    let rotation: Vec<u64> = self
      .recv_streams
      .range(self.credit_cursor.saturating_add(1)..)
      .chain(self.recv_streams.range(..=self.credit_cursor))
      .map(|(&stream_id, _)| stream_id)
      .filter(|stream_id| !chosen.contains(stream_id))
      .take(slots)
      .collect();
    if let Some(&last) = rotation.last() {
      self.credit_cursor = last;
    }
    for stream_id in chosen.into_iter().chain(rotation) {
      let frame = self.flow.stream_credit_frame(stream_id);
      self
        .credit_sent
        .insert(stream_id, self.flow.stream_max(stream_id));
      packing.used = packing.used.saturating_add(frame.encoded_len() as u64);
      frames.push(frame);
    }
  }

  /// The next fresh stream frame to send, or `None` when no send stream has data within its credit right
  /// now: the most urgent class with data first, round-robin within a class. Strict priority was chosen by
  /// the scheduler bake-off (2026-09-28, `docs/wip/BENCHMARKS.md`): the control class's p99 within 1.15×
  /// of the best scheduler in every scenario, against round-robin's 4.8×, and a weighted (deficit
  /// round-robin) scheduler that starved the control and metadata classes outright. The losers were
  /// deleted.
  fn next_fresh_frame(&mut self, data_room: u64) -> Result<Frame, FreshStop> {
    let mut held_by_credit = false;
    for class in Priority::ALL {
      let cap = data_room.min(self.class_credit(class));
      if cap == 0 {
        held_by_credit = held_by_credit || self.class_has_sendable(class);
        continue;
      }
      if let Some(frame) = self.round_robin_class(class, usize::try_from(cap).unwrap_or(usize::MAX))
      {
        return Ok(frame);
      }
    }
    Err(if held_by_credit {
      FreshStop::Credit
    } else {
      FreshStop::Empty
    })
  }

  /// The connection credit a stream of `class` may spend now: the peer's ceiling less what was sent, less
  /// the reserve `class` leaves for the classes above it ([`class_credit_reserve`]).
  fn class_credit(&self, class: Priority) -> u64 {
    self
      .peer_max_data
      .saturating_sub(self.connection_sent)
      .saturating_sub(class_credit_reserve(class, self.reserve_frame_cap))
  }

  /// Whether a stream of `class` has bytes it could frame now, within its stream credit.
  fn class_has_sendable(&self, class: Priority) -> bool {
    !self.ready.get(class).is_empty()
  }

  /// This end's streams whose sequences the peer's raised stream credit newly admits, `[from, to)`, may
  /// now frame: they join their rings. The sequence is an id's high bits, so they are one range of ids.
  fn admit(&mut self, (from, to): (u64, u64)) {
    if from >= to {
      return;
    }
    let role = self.role();
    let low = crate::streams::compose(0, Priority::Control, Role::Client, from);
    let high = crate::streams::compose(0, Priority::Control, Role::Client, to);
    let admitted: Vec<u64> = self
      .send_streams
      .range(low..high)
      .map(|(id, _)| *id)
      .filter(|id| crate::streams::initiator(*id) == role)
      .collect();
    for stream_id in admitted {
      self.requeue(stream_id);
    }
  }

  /// Whether some class has data still to frame and no connection credit it may spend — the sender is
  /// blocked at the peer's `MaxData` (RFC 9000 §4.1, §19.12) as far as that class can go.
  fn connection_credit_blocked(&self) -> bool {
    Priority::ALL.iter().any(|&class| {
      // A rare question (asked only with nothing recoverable in flight): a walk of the send streams.
      self.class_credit(class) == 0
        && self.send_streams.iter().any(|(stream_id, sender)| {
          priority(*stream_id) == class && self.streams.sendable(*stream_id) && !sender.is_drained()
        })
    })
  }

  /// The next frame of `stream_id`, if the peer's stream credit covers it and it has data within its
  /// flow-control credit.
  fn frame_of(&mut self, stream_id: u64, max_frame_len: usize) -> Option<Frame> {
    if !self.streams.sendable(stream_id) {
      return None;
    }
    self
      .send_streams
      .get_mut(&stream_id)
      .and_then(|sender| sender.next_frame(stream_id, max_frame_len))
  }

  /// Round-robin within one class: the first ready stream after the one served last, wrapping, frames;
  /// one that cannot frame within `max_frame_len` is passed over. Each ready stream is tried at most once.
  fn round_robin_class(&mut self, class: Priority, max_frame_len: usize) -> Option<Frame> {
    let mut after = *self.served.get(class);
    for _ in 0..self.ready.get(class).len() {
      let ready = self.ready.get(class);
      let next = after
        .and_then(|last| {
          ready
            .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
            .next()
        })
        .or_else(|| ready.first())
        .copied()?;
      self.stops.examined = self.stops.examined.saturating_add(1);
      // The scan's choice is the first stream it would try, so only a call's first candidate is compared:
      // one that cannot frame in the room left is passed over by both.
      #[cfg(test)]
      if after == *self.served.get(class) {
        let (order, last) = self.scan.get(class);
        let ready = self.ready.get(class);
        let start = last
          .and_then(|last| order.iter().position(|id| *id == last))
          .map_or(0, |at| at + 1);
        let count = order.len().max(1);
        let expected = (0..order.len())
          .filter_map(|step| order.get((start + step) % count))
          .find(|id| ready.iter().any(|(_, ready_id)| ready_id == *id));
        if expected != Some(&next.1) {
          self.scan_differed += 1;
        }
      }
      let frame = self.frame_of(next.1, max_frame_len);
      self.requeue(next.1);
      if frame.is_some() {
        *self.served.get_mut(class) = Some(next);
        #[cfg(test)]
        {
          self.scan.get_mut(class).1 = Some(next.1);
        }
        return frame;
      }
      after = Some(next);
    }
    None
  }

  /// A stream frame from the peer: dropped for a closed stream, counted for one the peer may not open, else offered to
  /// the stream's assembler under its flow-control window, the stream then readable (and owed its first credit when
  /// new). A refusal means the peer broke flow control or sent two final sizes (RFC 9000 §4.1, §4.5): the data is
  /// dropped and the violation counted, never ignored.
  fn on_stream_frame(&mut self, stream_id: u64, offset: u64, data: &[u8], fin: bool) {
    match self.streams.arrive(stream_id) {
      // A late copy for a stream this end already closed: dropped, never reopening it.
      Arrival::Closed => return,
      // A stream the peer may not open (past the credit it was given, or an id it never had): dropped and counted. A
      // peer that keeps to its credit never meets this.
      Arrival::Violation => {
        self.violations = self.violations.saturating_add(1);
        return;
      }
      Arrival::Open => {}
    }
    let window = self.flow.stream_max(stream_id);
    let initial = self.initial_window;
    if !self.recv_streams.contains_key(&stream_id) {
      // A new stream's first credit is owed.
      self.credit_dirty.insert(stream_id);
    }
    let assembler = self
      .recv_streams
      .entry(stream_id)
      .or_insert_with(|| StreamAssembler::new(initial));
    // The window is the ceiling the sender could not exceed; a duplicate or reordered segment is deduped.
    assembler.grant_window(window);
    if assembler.offer(offset, data, fin).is_err() {
      self.violations = self.violations.saturating_add(1);
    } else {
      self.readable.insert(stream_id);
    }
  }

  /// The peer is blocked on `stream_id`'s credit (RFC 9000 §19.13): the credit is marked moved, so the acknowledgement
  /// this owes carries it first.
  fn on_stream_blocked(&mut self, stream_id: u64) {
    self.credit_sent.remove(&stream_id);
    if self.recv_streams.contains_key(&stream_id) {
      self.credit_dirty.insert(stream_id);
    }
  }

  /// Takes a packet received at `now`: records its number for acknowledgement, demultiplexes each stream
  /// frame to its reassembler, processes any acknowledgement (the RTT sample,
  /// loss detection and the congestion controller), applies each stream's advertised send credit, and
  /// queues for retransmission whatever is declared lost. An acknowledgement-only packet does not oblige
  /// an acknowledgement in return (RFC 9002 §2).
  pub fn handle_incoming(&mut self, now: u64, pn: u64, frames: &[Frame]) {
    // A packet whose number was already processed is a network duplicate (RFC 9000 §12.3): dropped whole.
    if self.acks.is_duplicate(pn) {
      self.duplicates = self.duplicates.saturating_add(1);
      return;
    }
    self.acks.record(pn);
    let mut ack_eliciting = false;
    for frame in frames {
      match frame {
        Frame::Stream {
          stream_id,
          offset,
          fin,
          data,
        } => {
          ack_eliciting = true;
          self.on_stream_frame(*stream_id, *offset, data, *fin);
        }
        Frame::Ack {
          largest,
          range,
          ranges,
        } => self.on_ack_frame(now, *largest, *range, ranges),
        Frame::MaxStreamData { stream_id, max } => {
          if let Some(sender) = self.send_streams.get_mut(stream_id) {
            sender.grant_credit(*max);
          }
          self.requeue(*stream_id);
        }
        Frame::MaxData { max } => {
          self.peer_max_data = self.peer_max_data.max(*max);
        }
        Frame::ResetStream {
          stream_id,
          final_size,
        } => {
          ack_eliciting = true;
          match self.streams.arrive(*stream_id) {
            Arrival::Open => self.discard_recv(*stream_id, Some(*final_size)),
            Arrival::Closed => {}
            Arrival::Violation => self.violations = self.violations.saturating_add(1),
          }
        }
        // A path-MTU probe asks only to be acknowledged (`crate::pmtud`).
        Frame::Ping => ack_eliciting = true,
        Frame::StopSending { stream_id } => {
          ack_eliciting = true;
          // RFC 9000 §3.5: a STOP_SENDING is answered by resetting the stream (a peer's stream not yet
          // replied on is reset at size zero, so its reply is never sent).
          self.reset_stream(*stream_id);
        }
        // Stream credit rides acknowledgements, as the connection credit does, so it elicits none — one
        // that did made every acknowledgement answer the last (an endless exchange of acknowledgements
        // found by the reuse oracle, 2026-09-28).
        Frame::MaxStreams { max } => {
          let admitted = self.streams.on_max_streams(*max);
          self.admit(admitted);
        }
        // A blocked peer (RFC 9000 §19.12-13): the acknowledgement this owes carries the connection
        // credit; a blocked stream's credit is marked moved, so that acknowledgement carries it first.
        Frame::DataBlocked { .. } | Frame::StreamsBlocked { .. } => ack_eliciting = true,
        Frame::StreamDataBlocked { stream_id, .. } => {
          ack_eliciting = true;
          self.on_stream_blocked(*stream_id);
        }
      }
    }
    if ack_eliciting {
      self.ack_owed = true;
    }
  }

  /// Processes one acknowledgement frame at `now` (RFC 9002 §B.4 `OnAckReceived`).
  fn on_ack_frame(
    &mut self,
    now: u64,
    largest: u64,
    range: u64,
    ranges: &[crate::session::AckRange],
  ) {
    let acked = self.sent.on_ack_frame(largest, range, ranges);
    // A declared loss this acknowledgement covers was spurious: the tolerance widens to cover it.
    if self.reordering.remembers_declared_losses() {
      let window_packets = self.window_packets();
      self.reordering.on_acknowledged(
        &ack_runs(largest, range, ranges),
        now,
        self.rtt.smoothed_rtt_or_initial(),
        window_packets,
      );
    }
    if acked.packets.is_empty() {
      return;
    }
    // ACK-of-ACK (RFC 9000 §13.2.4).
    for pn in &acked.pns {
      if let Some(covered) = self.sent_acks.remove(pn) {
        self.acks.confirm(covered);
      }
    }
    // The RTT sample (RFC 9002 §5.1): the largest acknowledged packet, if this acknowledgement newly
    // acknowledged it — unless it is a path-MTU probe. A probe pokes a peer that may be idle, which acks it
    // only at its next wake; the dialect carries no ack delay to subtract, so such a sample reads the
    // peer's idleness as path delay (measured: 1.1 ms against a 1 ms path, 2026-09-28) — the same class as
    // `docs/bugs/2026-09-28-idle-peer-acks-inflated-the-rtt.md`. RTT samples come only from packets the
    // peer is actively answering.
    let probe_pn = self
      .path_mtu
      .as_ref()
      .and_then(|pmtu| acked.packets.iter().find(|packet| pmtu.is_probe(packet.pn)))
      .map(|packet| packet.pn);
    if let Some(newest) = acked.packets.iter().max_by_key(|packet| packet.pn)
      && newest.pn == largest
      && probe_pn != Some(newest.pn)
    {
      self.rtt.on_sample(now.saturating_sub(newest.sent_at), 0);
      self.first_rtt_sample_at.get_or_insert(now);
    }
    self.pto_count = 0;
    self.in_flight = self.in_flight.saturating_sub(acked.bytes);
    self.note_mtu_acknowledged(&acked.packets, now);
    self.mark_acknowledged(&acked.frames);
    let lost = self.detect_losses(now);
    // The acknowledgement's RTT sample for the controller: from the send of the most recently sent packet
    // it newly acknowledged to now.
    let rtt_sample = acked
      .packets
      .iter()
      .filter(|packet| probe_pn != Some(packet.pn))
      .map(|packet| packet.sent_at)
      .max()
      .map(|sent_at| now.saturating_sub(sent_at));
    let newest_acked = acked
      .packets
      .iter()
      .map(|packet| packet.pn)
      .max()
      .unwrap_or(0);
    let cwnd_limited = self.window_full_at.is_some_and(|full| full > newest_acked);
    let ack_event = AckEvent {
      now,
      rtt_sample,
      newly_acked: acked.bytes,
      cwnd_limited,
      rtt: &self.rtt,
    };
    let loss_event = (!lost.packets.is_empty()).then(|| LossEvent {
      now,
      persistent: self.persistent_congestion(&lost.packets),
      srtt: self.rtt.smoothed_rtt_or_initial(),
    });
    self
      .controller
      .on_ack_and_loss(&ack_event, loss_event.as_ref());
    self.requeue_lost(lost);
  }

  /// Runs loss detection at `now` (RFC 9002 §6.1): removes the lost packets from flight, and re-arms the loss timer. The caller hands the packets to the controller and requeues
  /// their frames.
  fn detect_losses(&mut self, now: u64) -> Lost {
    let srtt = self.rtt.smoothed_rtt_or_initial();
    let loss_delay = self
      .rtt
      .loss_delay()
      .saturating_add(self.reordering.extra_delay(self.rtt.min_rtt(), srtt));
    let (mut lost, loss_time) =
      self
        .sent
        .take_lost(now, loss_delay, self.reordering.packet_threshold());
    self.loss_time = loss_time;
    let bytes: u64 = lost.packets.iter().map(|packet| packet.bytes).sum();
    self.in_flight = self.in_flight.saturating_sub(bytes);
    self.note_mtu_losses(&mut lost, now);
    // Remember the data packets declared lost, so a late acknowledgement shows them spurious and widens the
    // tolerance (`crate::reorder`); a path-MTU probe is lost for its size, not its order, and is not counted.
    let declared: Vec<u64> = lost.packets.iter().map(|packet| packet.pn).collect();
    let largest = self.sent.largest_acked().unwrap_or(0);
    let cap = usize::try_from(
      self
        .window_packets()
        .saturating_mul(crate::reorder::MEMORY_SRTTS),
    )
    .unwrap_or(usize::MAX);
    self
      .reordering
      .on_declared_lost(&declared, largest, now, srtt, cap);
    lost
  }

  /// The packets the congestion window holds at its current datagram size — what bounds the reordering
  /// tolerance (a threshold past the window could never be met).
  fn window_packets(&self) -> u64 {
    (self.controller.window() / self.controller_datagram().max(1)).max(REORDER_THRESHOLD)
  }

  /// The datagram size the controller counts in.
  fn controller_datagram(&self) -> u64 {
    self.controller.copa().max_datagram()
  }

  /// Spurious losses the reordering tolerance has detected (a packet declared lost, acknowledged after).
  pub fn spurious_losses(&self) -> u64 {
    self.reordering.spurious()
  }

  /// Folds acknowledged packets into path MTU discovery: a probe confirms its size (the controller follows a
  /// larger datagram), and any other packet above the floor shows the confirmed size still crosses.
  fn note_mtu_acknowledged(&mut self, packets: &[SentPacket], now: u64) {
    let Some(pmtu) = self.path_mtu.as_mut() else {
      return;
    };
    let mut grew = false;
    for packet in packets {
      if pmtu.is_probe(packet.pn) {
        grew |= pmtu.on_probe_acked(packet.pn, now);
      } else if packet.size > BASE_PLPMTU as u64 {
        pmtu.on_large_packet_acked();
      }
    }
    if grew {
      let size = pmtu.current();
      self
        .controller
        .set_max_datagram(crate::endpoint::packet_budget_for(size) as u64);
    }
  }

  /// A probe timeout fired with packets above the floor in flight and nothing acknowledged since (RFC 8899
  /// §4.3): after a path shrinks, every packet at the old size is dropped, so no acknowledgement comes to
  /// declare any of them lost — the timeout is the only evidence. It counts as one loss of an above-floor
  /// packet; at the black-hole threshold the session falls to the floor, and the probe's copy is split to fit
  /// it (`split_front_retransmission`). Without this a session whose path shrank retried full-size copies
  /// forever (`docs/bugs/2026-09-28-a-shrunken-path-deadlocked-before-its-black-hole-was-seen.md`).
  fn note_mtu_timeout(&mut self, now: u64) {
    if !self.sent.any_in_flight_larger_than(BASE_PLPMTU as u64) {
      return;
    }
    let Some(pmtu) = self.path_mtu.as_mut() else {
      return;
    };
    if pmtu.on_large_packet_lost(now) {
      self
        .controller
        .set_max_datagram(crate::endpoint::packet_budget_for(BASE_PLPMTU) as u64);
    }
  }

  /// Takes path-MTU probes out of a loss pass before the controller sees it (RFC 9000 §14.4: a lost probe is
  /// no congestion signal) and folds every loss into discovery: a lost probe counts against its size, and
  /// losses of other above-floor packets are black-hole evidence — on a black hole the session falls to the
  /// floor and the controller follows. A `Ping` is never retransmitted.
  fn note_mtu_losses(&mut self, lost: &mut Lost, now: u64) {
    let Some(pmtu) = self.path_mtu.as_mut() else {
      return;
    };
    let mut fell = false;
    lost.packets.retain(|packet| {
      if pmtu.is_probe(packet.pn) {
        pmtu.on_probe_lost(packet.pn, now);
        return false;
      }
      if packet.size > BASE_PLPMTU as u64 {
        fell |= pmtu.on_large_packet_lost(now);
      }
      true
    });
    lost.frames.retain(|frame| !matches!(frame, Frame::Ping));
    if fell {
      self
        .controller
        .set_max_datagram(crate::endpoint::packet_budget_for(BASE_PLPMTU) as u64);
    }
  }

  /// Whether `lost` establishes persistent congestion (RFC 9002 §7.6.2): an RTT sample existed before the
  /// earliest of them, and they span the persistent-congestion duration with nothing acknowledged between.
  fn persistent_congestion(&self, lost: &[SentPacket]) -> bool {
    let Some(first_sample) = self.first_rtt_sample_at else {
      return false;
    };
    let earliest = lost.iter().map(|packet| packet.sent_at).min().unwrap_or(0);
    first_sample < earliest
      && self
        .sent
        .persistent_congestion(lost, self.rtt.persistent_congestion_duration(0))
  }

  /// Queues the frames of lost packets for retransmission and drops their ACK-of-ACK entries; bounds the
  /// ACK-of-ACK map to the unacknowledged window.
  fn requeue_lost(&mut self, lost: Lost) {
    for pn in &lost.pns {
      self.sent_acks.remove(pn);
    }
    if let Some(largest_acked) = self.sent.largest_acked() {
      self.sent_acks = self.sent_acks.split_off(&largest_acked);
    }
    let frames = lost
      .frames
      .into_iter()
      .filter(|frame| self.still_owed(frame))
      .collect();
    self.queue_retransmit(frames);
  }

  /// Whether a lost frame still needs resending: a stream frame whose range another copy already had
  /// acknowledged, or whose stream was forgotten, does not; every other frame does.
  fn still_owed(&self, frame: &Frame) -> bool {
    match frame {
      Frame::Stream {
        stream_id, offset, ..
      } => self
        .unacked
        .get(stream_id)
        .is_some_and(|offsets| offsets.contains(offset)),
      // A reset another copy already had acknowledged is not resent.
      Frame::ResetStream { stream_id, .. } => self.resets_owed.contains(stream_id),
      // A blocked report is resent only while the sender is still blocked at the same limit.
      Frame::DataBlocked { limit } => {
        *limit == self.peer_max_data && self.connection_credit_blocked()
      }
      Frame::StreamsBlocked { limit } => self.streams.waiting_limit() == Some(*limit),
      Frame::StreamDataBlocked { stream_id, limit } => {
        self
          .send_streams
          .get(stream_id)
          .and_then(StreamSender::blocked_at)
          == Some(*limit)
      }
      _ => true,
    }
  }

  /// Marks the stream ranges `frames` carried acknowledged, and releases every send stream that is now
  /// drained with nothing unacknowledged: its exchange's data has been delivered.
  fn mark_acknowledged(&mut self, frames: &[Frame]) {
    let mut touched = Vec::new();
    for frame in frames {
      match frame {
        Frame::ResetStream { stream_id, .. } => {
          self.resets_owed.remove(stream_id);
        }
        // A blocked report the peer has acknowledged while this end is still blocked at the same limit is
        // re-armed: its answering credit may have been lost, so the next poll reports again (at most once
        // per round trip) until the credit rises.
        Frame::DataBlocked { limit } if self.blocked_sent == Some(*limit) => {
          self.blocked_sent = None
        }
        Frame::StreamsBlocked { limit } if self.streams_blocked_sent == Some(*limit) => {
          self.streams_blocked_sent = None
        }
        Frame::StreamDataBlocked { stream_id, limit }
          if self.stream_blocked_sent.get(stream_id) == Some(limit) =>
        {
          self.stream_blocked_sent.remove(stream_id);
        }
        _ => {}
      }
      if let Frame::Stream {
        stream_id, offset, ..
      } = frame
        && let Some(offsets) = self.unacked.get_mut(stream_id)
      {
        offsets.remove(offset);
        touched.push(*stream_id);
      }
    }
    for stream_id in touched {
      let complete = self
        .unacked
        .get(&stream_id)
        .is_none_or(std::collections::BTreeSet::is_empty)
        && self
          .send_streams
          .get(&stream_id)
          .is_some_and(StreamSender::is_drained);
      if complete {
        self.forget_send_stream(stream_id);
        self.streams.close_send(stream_id);
      }
    }
  }

  /// Whether the peer is still owed anything it needs: stream data not yet acknowledged, or a reset it has
  /// not acknowledged. Credit frames (`MaxStreams`, `MaxData`) and `StopSending` are not owed: they matter
  /// only to a peer that goes on using a live session, which keeps acknowledging them.
  pub fn owes_peer(&self) -> bool {
    !self.send_streams.is_empty() || !self.resets_owed.is_empty()
  }

  /// Whether this end is still sending stream `stream_id` (framing it, or awaiting acknowledgements).
  pub fn sending(&self, stream_id: u64) -> bool {
    self.send_streams.contains_key(&stream_id)
  }

  /// The earliest time this connection needs [`on_timeout`](Connection::on_timeout): the loss timer, the
  /// probe timeout while ack-eliciting packets are in flight (RFC 9002 §6.2.1, backed off by
  /// `2^pto_count`), and the pacer's release while data waits on it. `None` when nothing is pending.
  pub fn next_timeout(&self) -> Option<u64> {
    [self.loss_time.or(self.pto_deadline()), self.pacing_release]
      .into_iter()
      .flatten()
      .min()
  }

  /// When the probe timeout fires while ack-eliciting packets are in flight: the most recent such packet's
  /// send time plus the PTO doubled per consecutive expiry (RFC 9002 §6.2.1), the backed-off value held
  /// under the larger of the PTO and the initial PTO — a peer that has gone silent is retried a few times a
  /// second, not once per minute after a dozen doublings (the fleet's takeover and re-dial rely on it; the
  /// endpoint held the same bound since 2026-09-13).
  fn pto_deadline(&self) -> Option<u64> {
    if self.probes_owed > 0 {
      // A probe is owed and not yet sent: the caller's next flush sends it; the timer re-arms from it.
      return None;
    }
    // A path-MTU probe alone in flight arms no probe timeout: it carries nothing to recover, and its fate is
    // read from the acknowledgements of the next traffic (RFC 8899 §5.1.1 lets a search wait on traffic).
    // Without this an idle session would send copies of a lost probe's `Ping` to learn a size nothing needs.
    if self.recoverable_in_flight() == 0 {
      return None;
    }
    let sent = self.sent.newest_sent_at()?;
    let pto = self.rtt.pto(0);
    let backoff = 1u64 << self.pto_count.min(PTO_BACKOFF_SHIFT_CAP);
    let timeout = pto
      .saturating_mul(backoff)
      .min(pto.max(self.rtt.initial_pto()));
    Some(sent.saturating_add(timeout))
  }

  /// Acts on the timers due at `now` (RFC 9002 §6.2 `OnLossDetectionTimeout`): a due loss timer declares
  /// the packets past the time threshold lost; else a due probe timeout owes a probe packet (new data if
  /// any, else the oldest in-flight packet's frames retransmitted), which may pass the window and the
  /// pacer, and doubles the next timeout. A due pacing release needs nothing: the next `poll_transmit`
  /// sends. Returns whether anything was queued to send.
  pub fn on_timeout(&mut self, now: u64) -> bool {
    if self.loss_time.is_some_and(|due| due <= now) {
      let lost = self.detect_losses(now);
      if lost.packets.is_empty() {
        return false;
      }
      let event = LossEvent {
        now,
        persistent: self.persistent_congestion(&lost.packets),
        srtt: self.rtt.smoothed_rtt_or_initial(),
      };
      self.controller.on_loss(&event);
      self.requeue_lost(lost);
      return true;
    }
    if self.pto_deadline().is_none_or(|due| due > now) {
      return false;
    }
    self.pto_count = self.pto_count.saturating_add(1);
    self.probes_owed = 1;
    self.note_mtu_timeout(now);
    // The probe carries new data only if new data can actually leave now — a stream with data inside its
    // own credit but no connection credit left cannot, and a probe that sends nothing leaves the timer
    // firing at the same instant forever (found by the loss oracle, 2026-09-27).
    let fresh_can_leave = Priority::ALL
      .iter()
      .any(|&class| self.class_credit(class) > 0 && self.class_has_sendable(class));
    if !fresh_can_leave && self.retransmit.is_empty() {
      // No new data: the probe carries a copy of the oldest in-flight packet's frames (RFC 9002 §6.2.4);
      // the original stays in flight until acknowledged or declared lost. A probe is not a congestion
      // signal; a loss it later reveals is.
      self.queue_probe_copy();
    }
    true
  }

  /// Sends a copy of the oldest in-flight packet as a probe now, whatever the timers say — the tail-loss
  /// probe a caller that owns its own deadline drives (RFC 9002 §6.2.4: the original stays in flight).
  /// Returns whether anything was queued. Prefer [`on_timeout`](Connection::on_timeout), which arms it
  /// from the RTT.
  pub fn probe(&mut self) -> bool {
    let probed = self.queue_probe_copy();
    if probed {
      self.probes_owed = 1;
    }
    probed
  }

  /// Queues a copy of the oldest in-flight packet's frames as a probe (RFC 9002 §6.2.4; the original stays
  /// tracked for loss accounting). Of the earlier probes' copies only the most recent
  /// [`PROBE_COPIES_KEPT`] stay tracked; the duplicate frames of any older one leave tracking (its other
  /// frames stay) — an acknowledgement of the original or of a newer copy settles them. A peer that stays
  /// silent therefore costs the originals plus that many copies, never one more tracked packet per probe
  /// timeout: without this a silent peer's session grew by 136,106 tracked packets over 90,640 virtual
  /// seconds (measured 2026-09-28). Returns whether anything was queued.
  fn queue_probe_copy(&mut self) -> bool {
    let frames = self.sent.copy_oldest();
    while self.probe_copies.len() >= PROBE_COPIES_KEPT {
      let Some(oldest) = self.probe_copies.pop_front() else {
        break;
      };
      let dropped = self.sent.drop_copied(oldest, &frames);
      self.in_flight = self.in_flight.saturating_sub(dropped);
    }
    let copied = !frames.is_empty();
    self.probe_copy_pending = copied;
    self.queue_retransmit(frames);
    copied
  }

  /// Allocates the next packet number and a bare, decryptable payload — a re-advertisement of the
  /// connection's current flow-control credit — for a **handshake-confirmation** packet
  /// ([`Endpoint::establish`](crate::Endpoint::establish)); see RFC 9000 §19.20, RFC 9001 §4.1.2. It is
  /// not ack-eliciting and not tracked; the endpoint resends a confirmation with a fresh number each
  /// probe timeout, so no number is ever reused under the packet keys (RFC 9001 §9.5).
  pub fn emit_confirm(&mut self) -> Option<(u64, Vec<Frame>)> {
    let pn = self.sent.next_pn()?;
    Some((pn, vec![self.flow.connection_credit_frame()]))
  }

  /// Whether the packet-number space is spent (`2^62`, RFC 9000 §12.3): the connection sends nothing more
  /// and its endpoint ends the session ([`EndpointError::PacketNumbersExhausted`](crate::EndpointError)).
  pub fn packet_numbers_exhausted(&self) -> bool {
    self.sent.exhausted()
  }

  /// Drains the bytes now contiguous on receive stream `stream_id` (in order, each once) at `now`, slides
  /// that stream's flow-control window forward by what was consumed, and auto-tunes the window, so the
  /// next acknowledgement advertises fresh credit a window ahead of the new read cursor. Empty if the
  /// stream is unknown.
  pub fn read_stream(&mut self, now: u64, stream_id: u64) -> Vec<u8> {
    let Some(assembler) = self.recv_streams.get_mut(&stream_id) else {
      return Vec::new();
    };
    let bytes = assembler.read();
    let read_offset = assembler.read_offset();
    self.flow.on_stream_consumed(stream_id, read_offset);
    if !bytes.is_empty() {
      self.credit_dirty.insert(stream_id);
      let window = self.flow.window();
      let rtt = self.rtt.has_sample().then(|| self.rtt.smoothed_rtt());
      self.flow.autotune(now, rtt);
      if self.flow.window() != window {
        // A grown window moves every open stream's ceiling.
        self.credit_dirty.extend(self.recv_streams.keys().copied());
      }
    }
    bytes
  }

  /// The receive streams a stream frame reached since the last call, still open: what a reader must read now. A
  /// stream closed since its frame arrived is left out (its half is gone; nothing of it is to be read).
  pub fn take_readable(&mut self) -> Vec<u64> {
    let readable = std::mem::take(&mut self.readable);
    readable
      .into_iter()
      .filter(|stream_id| self.recv_streams.contains_key(stream_id))
      .collect()
  }

  /// The stream ids seen on the receive side so far (a frame has arrived for each).
  pub fn recv_stream_ids(&self) -> Vec<u64> {
    self.recv_streams.keys().copied().collect()
  }

  /// The sender's current congestion window in bytes.
  pub fn congestion_window(&self) -> u64 {
    self.controller.window()
  }

  /// The congestion controller (for the bake-off's reports).
  pub fn controller(&self) -> &Controller {
    &self.controller
  }

  /// The bytes currently in flight.
  pub fn bytes_in_flight(&self) -> u64 {
    self.in_flight
  }

  /// The RTT estimator.
  pub fn rtt(&self) -> &RttEstimator {
    &self.rtt
  }

  /// The receive window currently advertised ahead of consumption, and how many times it has grown.
  pub fn receive_window(&self) -> (u64, u64) {
    (self.flow.window(), self.flow.growths())
  }

  /// How many packet numbers the receive side still tracks for acknowledgement.
  pub fn acks_tracked(&self) -> usize {
    self.acks.tracked()
  }

  /// Whether receive stream `stream_id` is complete (all bytes through its `fin`). False if unknown.
  pub fn recv_stream_complete(&self, stream_id: u64) -> bool {
    self
      .recv_streams
      .get(&stream_id)
      .is_some_and(StreamAssembler::is_complete)
  }

  /// Ack-eliciting packets sent and not yet acknowledged.
  pub fn in_flight_count(&self) -> usize {
    self.sent.in_flight_count()
  }

  /// Whether every send stream has been delivered — framed whole and every frame acknowledged, so each was
  /// released — and no stream control frame waits to leave.
  pub fn send_complete(&self) -> bool {
    self.send_streams.is_empty() && self.control.is_empty()
  }

  /// The ready sets hold exactly the send streams that can frame data now, each in its own class.
  #[cfg(test)]
  fn ready_is_exact(&self) -> bool {
    let mut held: Vec<u64> = Priority::ALL
      .iter()
      .flat_map(|&class| {
        self
          .ready
          .get(class)
          .iter()
          .map(|(_, id)| *id)
          .filter(move |id| priority(*id) == class)
      })
      .collect();
    let total = Priority::ALL
      .iter()
      .map(|&class| self.ready.get(class).len())
      .sum::<usize>();
    held.sort_unstable();
    let sendable: Vec<u64> = self
      .send_streams
      .iter()
      .filter(|(id, sender)| self.streams.sendable(**id) && sender.has_sendable())
      .map(|(id, _)| *id)
      .collect();
    total == held.len() && held == sendable
  }

  /// Forgets this end's sending half of `stream_id`: nothing of it is retransmitted afterwards, its
  /// in-flight bytes leave the window, and the connection credit they took is refunded.
  fn forget_send_stream(&mut self, stream_id: u64) {
    let class = priority(stream_id);
    if let Some(sender) = self.send_streams.get(&stream_id) {
      self
        .ready
        .get_mut(class)
        .remove(&(sender.installed(), stream_id));
    }
    // Service restarts at the oldest stream, as the scan this replaced did.
    *self.served.get_mut(class) = None;
    #[cfg(test)]
    {
      let (order, last) = self.scan.get_mut(class);
      order.retain(|id| *id != stream_id);
      *last = None;
    }
    self.send_streams.remove(&stream_id);
    self.unacked.remove(&stream_id);
    self.stream_blocked_sent.remove(&stream_id);
    self
      .retransmit
      .retain(|frame| !matches!(frame, Frame::Stream { stream_id: id, .. } if *id == stream_id));
    let dropped = self.sent.forget_stream(stream_id);
    self.in_flight = self.in_flight.saturating_sub(dropped);
    self.connection_sent = self.connection_sent.saturating_sub(dropped);
  }

  /// Discards this end's receiving half of `stream_id`: whatever arrived is dropped unread, the peer's
  /// connection credit is returned for it — through `final_size` when the peer reset the stream (RFC 9000
  /// §4.5: a reset stream's final size is consumed) — and the half is forgotten.
  fn discard_recv(&mut self, stream_id: u64, final_size: Option<u64>) {
    let received = self
      .recv_streams
      .get(&stream_id)
      .map_or(0, StreamAssembler::highest_offset);
    let consumed = final_size.unwrap_or(received).max(received);
    self.flow.on_stream_consumed(stream_id, consumed);
    self.close_recv(stream_id);
  }

  /// Closes this end's receiving half of `stream_id` — its bytes read through, discarded or reset. The
  /// stream-id space then knows it closed, so a late frame for it (a retransmission or probe copy still in
  /// flight, however late) is dropped rather than reopening it. A request and its reply share an id, so
  /// this never touches the sending half (`docs/bugs/2026-09-27-a-late-request-copy-forgot-the-reply.md`).
  pub fn close_recv(&mut self, stream_id: u64) {
    self.recv_streams.remove(&stream_id);
    self.credit_dirty.remove(&stream_id);
    self.readable.remove(&stream_id);
    self.credit_sent.remove(&stream_id);
    self.flow.forget_stream(stream_id);
    self.streams.close_receive(stream_id);
  }

  /// Whether `stream_id`'s receiving half is open (not read through, discarded, or reset by the peer).
  pub fn receiving(&self, stream_id: u64) -> bool {
    self.streams.receiving(stream_id)
  }

  /// Why fresh sending stopped, counted over the connection's life.
  pub fn send_stops(&self) -> SendStops {
    self.stops
  }

  /// The role this end plays (which stream ids it opens).
  pub fn role(&self) -> Role {
    self.streams.role()
  }

  /// Everything the connection holds per stream, counted (the leak witness).
  pub fn census(&self) -> ConnectionCensus {
    ConnectionCensus {
      streams: self.streams.census(),
      send_streams: self.send_streams.len(),
      recv_streams: self.recv_streams.len(),
      unacked: self.unacked.len(),
      retransmit: self.retransmit.len(),
      control: self.control.len(),
      in_flight: self.recoverable_in_flight(),
      path_probe: self.path_probes_in_flight(),
      credit_tracked: self.credit_sent.len(),
      blocked_tracked: self.stream_blocked_sent.len(),
    }
  }

  /// The path-MTU probes in flight: one while the search waits on its probe, else none.
  fn path_probes_in_flight(&self) -> usize {
    usize::from(self.path_mtu.as_ref().is_some_and(PathMtu::probe_in_flight))
  }

  /// The packets in flight that carry something to recover — every one but a lone path-MTU probe. What the
  /// connection's liveness rules count: the probe timeout, and the blocked reports sent when nothing is.
  fn recoverable_in_flight(&self) -> usize {
    self
      .sent
      .in_flight_count()
      .saturating_sub(self.path_probes_in_flight())
  }

  /// The next packet number this connection will assign — its packet-number cursor.
  pub fn tx_packet_number(&self) -> u64 {
    self.sent.peek_next_pn()
  }

  /// How many frames this end has retransmitted — the non-vacuity counter for the loss-recovery path.
  pub fn retransmitted(&self) -> u64 {
    self.retransmitted
  }

  /// How many stream frames the peer sent in breach of flow control or a final size (dropped).
  pub fn protocol_violations(&self) -> u64 {
    self.violations
  }

  /// How many received packets were discarded as duplicates (RFC 9000 §12.3).
  pub fn duplicates_discarded(&self) -> u64 {
    self.duplicates
  }

  /// The largest packet number the peer has acknowledged.
  pub fn tx_largest_acked(&self) -> Option<u64> {
    self.sent.largest_acked()
  }

  /// The absolute offset send stream `stream_id` has framed so far. Zero if the stream is unknown.
  pub fn send_offset(&self, stream_id: u64) -> u64 {
    self
      .send_streams
      .get(&stream_id)
      .map_or(0, StreamSender::send_offset)
  }

  /// The absolute offset receive stream `stream_id` has delivered so far. Zero if unknown.
  pub fn read_offset(&self, stream_id: u64) -> u64 {
    self
      .recv_streams
      .get(&stream_id)
      .map_or(0, StreamAssembler::read_offset)
  }

  /// Queues frames for retransmission, counting them.
  fn queue_retransmit(&mut self, frames: Vec<Frame>) {
    self.retransmitted = self.retransmitted.saturating_add(frames.len() as u64);
    self.retransmit.extend(frames);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;

  /// Shape: the stream data one small test packet carries — sixteen bytes, so a short stream spans many
  /// packets and every loss, reorder and credit path runs, while an acknowledgement with its two credit
  /// frames still fits one packet (`MIN_PACKET_BUDGET`).
  const FRAME_DATA: usize = 16;
  /// Shape: the packet budget of the small test packets — one `Stream` frame of [`FRAME_DATA`] bytes.
  const FRAME_CAP: usize = STREAM_FRAME_HEADER_BYTES + FRAME_DATA;
  /// Shape: a packet budget of several frames for the multi-frame tests.
  const PACKET_BUDGET: usize = 4 * FRAME_CAP;
  /// Shape: a millisecond.
  const MS: u64 = 1_000_000;

  /// Shape: the request kind the tests' streams carry.
  const KIND: u64 = 1;

  /// A connection framing at `cap` whose receive window stays at the initial window (the ceiling equals
  /// it), playing `role`.
  fn fixed(cap: usize, role: Role) -> Connection {
    let window = initial_receive_window(cap);
    Connection::new(
      ConnectionShape {
        max_datagram: cap as u64,
        initial_window: window,
        receive_ceiling: window,
      },
      role,
    )
  }

  /// AUD-29-27: do: a connection whose packet-number space is spent, with data queued; poll a packet, a
  /// probe and a handshake confirmation; expect nothing sent, the queued data still queued (no frame taken
  /// from its queue), and the connection reporting its space spent — the endpoint then ends the session.
  #[test]
  fn a_spent_packet_number_space_sends_nothing_and_loses_nothing() {
    let cap = 1200;
    let mut sender = fixed(cap, Role::Client);
    let content = vec![7u8; 100];
    let _ = open(&mut sender, &content);
    sender.sent = crate::conn::SentTracker::starting_at(crate::packet_number::PACKET_NUMBER_SPACE);
    let budget = sender.packet_budget(cap);
    assert_eq!(sender.poll_transmit(1_000, budget), None);
    assert_eq!(sender.poll_probe(1_000), None);
    assert_eq!(sender.emit_confirm(), None);
    assert!(sender.packet_numbers_exhausted());
    // The queue was not touched: a fresh number space would still send the data.
    sender.sent = crate::conn::SentTracker::new();
    let sent = sender.poll_transmit(1_000, budget);
    assert!(sent.is_some_and(|(_, frames)| !frames.is_empty()));
  }

  /// §4.10a (RFC 9000 §19.13; `docs/bugs/2026-09-28-a-lone-path-probe-silenced-the-blocked-report.md`): a
  /// sender whose stream credit is spent, with all its data acknowledged, still reports itself blocked when
  /// the only packet it has in flight is a path-MTU probe — the probe carries nothing and must not stand in
  /// for traffic. Do X (spend the credit, deliver everything, send a probe that is lost, and let the peer's
  /// acknowledgement arrive without its credit), expect Y (the next packet carries a blocked report).
  #[test]
  fn a_blocked_sender_reports_even_with_a_path_probe_in_flight() {
    let cap = FRAME_CAP;
    let mut sender = fixed_window(cap);
    let mut receiver = fixed_receiver(cap);
    sender.enable_path_mtu(9_000);
    let content = vec![7u8; usize::try_from(initial_receive_window(cap)).unwrap() * 2];
    open(&mut sender, &content);
    // Send until the stream has nothing its credit lets it send, past each pacing release.
    let mut now = 1_000;
    while sender.class_has_sendable(Priority::Metadata) {
      match sender.poll_transmit(now, cap) {
        Some((pn, frames)) => receiver.handle_incoming(now, pn, &frames),
        None => now = sender.next_timeout().unwrap_or(now).max(now + 1),
      }
    }
    let (probe, _) = sender.poll_probe(now).expect("a probe is due");
    assert!(
      sender
        .path_mtu_stats()
        .is_some_and(|stats| stats.probes_sent == 1)
    );
    // The receiver's acknowledgement reaches the sender without its credit frames; the probe never arrived.
    let (pn, frames) = receiver
      .poll_transmit(now, cap)
      .expect("the receiver acknowledges");
    let ack_only: Vec<Frame> = frames
      .into_iter()
      .filter(|frame| matches!(frame, Frame::Ack { .. }))
      .collect();
    sender.handle_incoming(now, pn, &ack_only);
    assert_eq!(
      sender.census().in_flight,
      0,
      "every data packet is acknowledged"
    );
    assert_eq!(
      sender.census().path_probe,
      1,
      "the probe {probe} is the one packet out"
    );
    let (_, next) = sender
      .poll_transmit(now, cap)
      .expect("a blocked sender sends its report");
    assert!(
      next.iter().any(|frame| matches!(
        frame,
        Frame::DataBlocked { .. } | Frame::StreamDataBlocked { .. }
      )),
      "the report goes out: {next:?}"
    );
  }

  /// §4.10a (RFC 8899 §4.4): a probe the local stack refuses was never sent, so it leaves nothing in flight
  /// and gives its packet number back — the next packet takes it, and the peer sees no gap to report.
  #[test]
  fn a_refused_probe_gives_its_packet_number_back() {
    let cap = FRAME_CAP;
    let mut sender = fixed_window(cap);
    sender.enable_path_mtu(9_000);
    let (probe, _) = sender.poll_probe(1_000).expect("a probe is due");
    sender.on_probe_refused(probe, 1_000);
    assert_eq!(sender.census().path_probe, 0, "nothing is in flight");
    assert_eq!(
      sender.path_mtu_stats().map(|stats| stats.probes_refused),
      Some(1)
    );
    open(&mut sender, b"after");
    let (pn, _) = sender.poll_transmit(1_000, cap).expect("the data goes out");
    assert_eq!(pn, probe, "the refused probe's packet number is reused");
  }

  /// §4.10a (RFC 9002 §6.1.1; RFC 8985 §6.2; `crate::reorder`): a path that reorders but loses nothing
  /// teaches the loss thresholds. Each round the wire reverses every run of `DEPTH` packets in the sender's
  /// batch, so packets arrive up to `DEPTH - 1` places out of order — more than RFC 9002's three. Early rounds
  /// declare delivered packets lost (spurious); the tolerance widens to cover the distance, and the later half
  /// of the transfer retransmits a quarter or less of what the first half did — while every byte arrives
  /// exactly once, in order.
  #[test]
  fn a_reordering_path_teaches_the_loss_thresholds() {
    /// Shape: how many consecutive packets the wire reverses — past RFC 9002's threshold of three.
    const DEPTH: usize = 8;
    let cap = FRAME_CAP;
    let window = 64 * (cap as u64);
    let shape = ConnectionShape {
      max_datagram: cap as u64,
      initial_window: window,
      receive_ceiling: window,
    };
    let mut sender = Connection::new(shape, Role::Client);
    let mut receiver = Connection::new(shape, Role::Server);
    let content: Vec<u8> = (0..400_000u32).map(|at| (at % 251) as u8).collect();
    let id = open(&mut sender, &content);
    let mut now = 1_000_000u64;
    let mut received = Vec::new();
    let mut retransmitted_at_half = None;
    for _round in 0..20_000 {
      now = reordered_round(&mut sender, &mut receiver, id, now, DEPTH, &mut received);
      if sender.next_timeout().is_some_and(|due| due <= now) {
        sender.on_timeout(now);
      }
      if retransmitted_at_half.is_none() && received.len() >= content.len() / 2 {
        retransmitted_at_half = Some(sender.retransmitted());
      }
      if received.len() == content.len() && !sender.owes_peer() {
        break;
      }
    }
    assert_eq!(received, content, "every byte once, in order");
    let first_half = retransmitted_at_half.expect("the transfer passed its half");
    let second_half = sender.retransmitted() - first_half;
    assert!(
      sender.spurious_losses() > 0,
      "the reordering was seen as spurious loss"
    );
    assert!(
      second_half * 4 <= first_half.max(1),
      "the tolerance cut spurious retransmissions: {first_half} in the first half, {second_half} in the second"
    );
  }

  /// One round over a reordering wire: everything `sender` may send goes out, every run of `depth` packets is
  /// delivered reversed, and each packet is acknowledged as it arrives — so the sender sees a run's later
  /// packets acknowledged first. Returns the clock after the round.
  fn reordered_round(
    sender: &mut Connection,
    receiver: &mut Connection,
    id: u64,
    mut now: u64,
    depth: usize,
    received: &mut Vec<u8>,
  ) -> u64 {
    let cap = FRAME_CAP;
    let mut batch = Vec::new();
    while let Some(packet) = sender.poll_transmit(now, cap) {
      batch.push(packet);
    }
    for run in batch.chunks_mut(depth) {
      run.reverse();
    }
    now += 1_000_000;
    for (pn, frames) in batch {
      receiver.handle_incoming(now, pn, &frames);
      received.extend(receiver.read_stream(now, id));
      while let Some((ack_pn, ack_frames)) = receiver.poll_transmit(now, cap) {
        sender.handle_incoming(now, ack_pn, &ack_frames);
      }
      now += 10_000;
    }
    now + 1_000_000
  }

  /// The dialing end of a [`fixed`] pair — the one that opens the streams.
  fn fixed_window(cap: usize) -> Connection {
    fixed(cap, Role::Client)
  }

  /// The accepting end of a [`fixed`] pair.
  fn fixed_receiver(cap: usize) -> Connection {
    fixed(cap, Role::Server)
  }

  /// Opens a stream carrying `content` on `sender` and returns its id.
  fn open(sender: &mut Connection, content: &[u8]) -> u64 {
    sender
      .open_exchange(KIND, Priority::Metadata, content)
      .expect("the stream credit covers the test's streams")
  }

  /// Opens each labelled stream of `streams` on `sender`, returning `(label, id)` pairs: a test names its
  /// streams by label, the connection allocates their ids.
  fn open_labelled(sender: &mut Connection, streams: &[(u64, Vec<u8>)]) -> Vec<(u64, u64)> {
    streams
      .iter()
      .map(|(label, content)| (*label, open(sender, content)))
      .collect()
  }

  /// `received`, keyed by id, re-keyed by the labels `ids` maps them from.
  fn relabel(received: BTreeMap<u64, Vec<u8>>, ids: &[(u64, u64)]) -> BTreeMap<u64, Vec<u8>> {
    ids
      .iter()
      .filter_map(|(label, id)| received.get(id).map(|bytes| (*label, bytes.clone())))
      .collect()
  }

  /// Which transmissions the wire drops (counted across both directions, from zero) and whether it
  /// reverses each batch a sender emits (a deterministic reordering).
  struct Channel {
    drop_steps: Vec<u64>,
    step: u64,
    reorder: bool,
    /// Count and drop only sender-to-receiver packets (the data direction), so the dropped steps are
    /// data packets whatever the acknowledgements in between.
    forward_only: bool,
  }

  impl Channel {
    fn new(drop_steps: Vec<u64>) -> Channel {
      Channel {
        drop_steps,
        step: 0,
        reorder: false,
        forward_only: false,
      }
    }

    /// Drops the sender's `drop_steps`-th packets (counted in the data direction only).
    fn forward(drop_steps: Vec<u64>) -> Channel {
      Channel {
        forward_only: true,
        ..Channel::new(drop_steps)
      }
    }

    fn reordering(drop_steps: Vec<u64>) -> Channel {
      Channel {
        drop_steps,
        step: 0,
        reorder: true,
        forward_only: false,
      }
    }

    fn drops(&mut self, to_b: bool) -> bool {
      if self.forward_only && !to_b {
        return false;
      }
      let dropped = self.drop_steps.contains(&self.step);
      self.step = self.step.saturating_add(1);
      dropped
    }
  }

  /// A two-ended wire with a virtual clock: packets arrive `delay` after they are sent (in send order, or
  /// batch-reversed), and when nothing can move the clock jumps to the next arrival or the earlier of the
  /// two connections' timers — how the endpoint drives a connection over a real path.
  struct Wire {
    now: u64,
    delay: u64,
    in_flight: BTreeMap<(u64, u64), (bool, u64, Vec<Frame>)>,
    sequence: u64,
    channel: Channel,
    /// The most stream frames any packet sent on this wire carried (the packing path's witness).
    widest: usize,
  }

  impl Wire {
    fn new(delay: u64, channel: Channel) -> Wire {
      Wire {
        now: 0,
        delay,
        in_flight: BTreeMap::new(),
        sequence: 0,
        channel,
        widest: 0,
      }
    }

    /// Sends everything `from` wants to at the current time (`to_b`: the direction), returning how many
    /// packets left. `check` runs after each send, before the packet is handed to the wire.
    fn send(
      &mut self,
      from: &mut Connection,
      to_b: bool,
      budget: usize,
      mut check: impl FnMut(&Connection),
    ) -> usize {
      let mut batch = Vec::new();
      while let Some((pn, frames)) = from.poll_transmit(self.now, budget) {
        // Every packet fits its budget exactly as encoded — acknowledgement, credit and control frames
        // counted with the data (a packet past its budget crosses the datagram floor on a real path).
        let encoded: usize = frames.iter().map(Frame::encoded_len).sum();
        assert!(
          encoded <= budget,
          "a packet of {encoded} encoded bytes exceeded its {budget}-byte budget: {frames:?}"
        );
        check(from);
        let stream_frames = frames
          .iter()
          .filter(|f| matches!(f, Frame::Stream { .. }))
          .count();
        self.widest = self.widest.max(stream_frames);
        batch.push((pn, frames));
      }
      let count = batch.len();
      if self.channel.reorder {
        batch.reverse();
      }
      for (pn, frames) in batch {
        if !self.channel.drops(to_b) {
          let arrival = self.now.saturating_add(self.delay);
          self
            .in_flight
            .insert((arrival, self.sequence), (to_b, pn, frames));
          self.sequence += 1;
        }
      }
      count
    }

    /// Delivers every packet due by now; returns how many.
    fn deliver(&mut self, a: &mut Connection, b: &mut Connection) -> usize {
      let mut delivered = 0;
      while let Some(entry) = self.in_flight.first_entry() {
        if entry.key().0 > self.now {
          break;
        }
        let (to_b, pn, frames) = entry.remove();
        let target = if to_b { &mut *b } else { &mut *a };
        target.handle_incoming(self.now, pn, &frames);
        delivered += 1;
      }
      delivered
    }

    /// Advances the clock to the next event (an arrival, or either side's timer) and runs the timers due
    /// then. False when nothing is pending at all: the run has stalled or finished.
    fn advance(&mut self, a: &mut Connection, b: &mut Connection) -> bool {
      let next = [
        self.in_flight.keys().next().map(|(arrival, _)| *arrival),
        a.next_timeout(),
        b.next_timeout(),
      ]
      .into_iter()
      .flatten()
      .min();
      let Some(next) = next else {
        return false;
      };
      self.now = self.now.max(next);
      for side in [a, b] {
        if side.next_timeout().is_some_and(|due| due <= self.now) {
          side.on_timeout(self.now);
        }
      }
      true
    }
  }

  /// Runs `streams` from a sender to a receiver over `wire`, framing at `budget`, and returns
  /// what the receiver reassembled per stream and the sender (for its counters). Asserts the
  /// never-whole-object invariant on every send.
  fn transfer_on(
    streams: &[(u64, Vec<u8>)],
    budget: usize,
    mut wire: Wire,
  ) -> (BTreeMap<u64, Vec<u8>>, Connection) {
    let window = initial_receive_window(budget);
    let mut sender = fixed_window(budget);
    let ids = open_labelled(&mut sender, streams);
    let mut receiver = fixed_receiver(budget);
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut guard = 0u64;
    loop {
      guard += 1;
      assert!(guard < 1_000_000, "the connection must make progress");
      let reads: Vec<(u64, u64)> = ids
        .iter()
        .map(|(_, id)| (*id, receiver.read_offset(*id)))
        .collect();
      assert!(
        sender.ready_is_exact(),
        "the sender's ready sets hold exactly its sendable streams"
      );
      assert_eq!(
        sender.scan_differed, 0,
        "the sender served a stream the scan would not have"
      );
      assert_eq!(
        receiver.scan_differed, 0,
        "the receiver served a stream the scan would not have"
      );
      assert!(
        receiver.ready_is_exact(),
        "the receiver's ready sets hold exactly its sendable streams"
      );
      let mut moved = wire.send(&mut sender, true, budget, |from| {
        for (id, read) in &reads {
          assert!(
            from.send_offset(*id) <= read + window,
            "stream {id}: the sender raced more than a window ahead of the reader"
          );
        }
      });
      let now = wire.now;
      moved += wire.deliver(&mut sender, &mut receiver);
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(now, id));
      }
      moved += wire.send(&mut receiver, false, budget, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      let all_recv = ids.iter().all(|(_, id)| receiver.recv_stream_complete(*id));
      if sender.send_complete() && all_recv {
        break;
      }
      if moved == 0 && !wire.advance(&mut sender, &mut receiver) {
        break;
      }
    }
    (relabel(received, &ids), sender)
  }

  /// Like [`transfer_on`] with no path delay.
  fn transfer(streams: &[(u64, Vec<u8>)], channel: Channel) -> (BTreeMap<u64, Vec<u8>>, u64) {
    let (received, sender) = transfer_on(streams, FRAME_CAP, Wire::new(MS, channel));
    (received, sender.retransmitted())
  }

  /// A stream of `len` bytes with a per-stream fingerprint, so a demultiplexing mix-up would show.
  fn stream_content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
      .map(|i| u8::try_from((usize::from(seed).wrapping_add(i)) % 251).unwrap_or(0))
      .collect()
  }

  /// Sends one request and shows the probe path live: unacknowledged, a probe resends it (the retransmit
  /// counter moves). Returns the sender with the request still in flight, and the request's id.
  fn request_in_flight_and_probed() -> (Connection, u64) {
    let mut sender = fixed_window(FRAME_CAP);
    let id = open(&mut sender, &stream_content(9, 40));
    let (_pn, frames) = sender
      .poll_transmit(0, FRAME_CAP)
      .expect("the request goes out");
    assert!(
      frames
        .iter()
        .any(|f| matches!(f, Frame::Stream { stream_id, .. } if *stream_id == id))
    );
    assert_eq!(sender.in_flight_count(), 1);
    assert!(sender.probe(), "a probe finds the packet in flight");
    let (_pn, resent) = sender
      .poll_transmit(0, FRAME_CAP)
      .expect("the probe retransmits");
    assert!(
      resent
        .iter()
        .any(|f| matches!(f, Frame::Stream { stream_id, .. } if *stream_id == id))
    );
    assert!(sender.retransmitted() >= 1, "the retransmit path ran");
    (sender, id)
  }

  /// AC (§4.8 "Membership" — a probe abandoned at its deadline; RFC 9000 §3.1, §19.4): once a stream is
  /// reset, none of its data is ever retransmitted, its bytes leave the in-flight accounting, and the
  /// next packet carries the `ResetStream` — the only thing the stream still sends. Non-vacuous: before
  /// the reset, the same probe resends the packet.
  #[test]
  fn a_reset_streams_frames_are_never_retransmitted() {
    let (mut sender, id) = request_in_flight_and_probed();
    sender.reset_stream(id);
    assert_eq!(
      sender.in_flight_count(),
      0,
      "its packets carried nothing else"
    );
    assert_eq!(
      sender.bytes_in_flight(),
      0,
      "its bytes left the in-flight accounting"
    );
    assert!(!sender.probe(), "a probe finds nothing to resend");
    let retransmitted_before = sender.retransmitted();
    let (_pn, frames) = sender
      .poll_transmit(0, FRAME_CAP)
      .expect("the reset leaves");
    assert_eq!(
      frames,
      vec![Frame::ResetStream {
        stream_id: id,
        final_size: FRAME_DATA as u64
      }],
      "only the reset is sent, never the stream's data; its final size is the one frame framed (RFC 9000 §4.5)"
    );
    assert!(sender.poll_transmit(0, FRAME_CAP).is_none());
    assert_eq!(sender.retransmitted(), retransmitted_before);
  }

  /// AC (§4.10a §8, RFC 9000 §12.3): a packet whose number was already processed is discarded — its
  /// frames are not applied again and it owes no fresh acknowledgement.
  #[test]
  fn a_duplicate_packet_number_is_discarded_not_processed_again() {
    let mut sender = fixed_window(FRAME_CAP);
    let mut receiver = fixed_receiver(FRAME_CAP);
    let id = open(&mut sender, &stream_content(0xAB, 40));
    let (pn, frames) = sender
      .poll_transmit(0, FRAME_CAP)
      .expect("the request goes out");
    receiver.handle_incoming(0, pn, &frames);
    assert!(
      !receiver.read_stream(0, id).is_empty(),
      "the first receipt delivered bytes"
    );
    assert_eq!(receiver.duplicates_discarded(), 0);
    // The first receipt owes an acknowledgement (and, at this small budget, a separate packet with the
    // stream credit its read moved): drain everything it owes.
    assert!(
      drain(&mut receiver, 0) > 0,
      "the first receipt owes an acknowledgement"
    );
    receiver.handle_incoming(0, pn, &frames);
    assert_eq!(receiver.duplicates_discarded(), 1);
    assert!(
      receiver.read_stream(0, id).is_empty(),
      "the duplicate delivered no further bytes"
    );
    assert!(
      receiver.poll_transmit(0, FRAME_CAP).is_none(),
      "the duplicate owed no fresh acknowledgement"
    );
  }

  /// AC (§4.9; RFC 9000 §12.2): a packet carries several frames when the budget allows, and multiplexed
  /// streams still arrive exactly once, in order, under loss and reorder. Non-vacuous: a packet carried
  /// more than one stream frame, and the loss path ran.
  #[test]
  fn several_frames_per_packet_survive_loss_and_reorder() {
    let streams = vec![
      (1u64, stream_content(1, 500)),
      (3u64, stream_content(2, 20)),
      (7u64, stream_content(3, 300)),
    ];
    let (received, widest, retransmitted) = packed_transfer(&streams);
    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived exactly"
      );
    }
    assert!(
      widest > 1,
      "the widest packet carried {widest} stream frames"
    );
    assert!(retransmitted > 0, "the loss path ran");
  }

  /// Runs `streams` framed at [`PACKET_BUDGET`], dropping the third and eleventh packets and
  /// reversing each sender batch; returns what arrived, the most stream frames one packet carried, and the
  /// frames retransmitted.
  fn packed_transfer(streams: &[(u64, Vec<u8>)]) -> (BTreeMap<u64, Vec<u8>>, usize, u64) {
    let mut sender = fixed_window(PACKET_BUDGET);
    let ids = open_labelled(&mut sender, streams);
    let mut receiver = fixed_receiver(PACKET_BUDGET);
    let mut wire = Wire::new(MS, Channel::reordering(vec![3, 11]));
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for _ in 0..1_000_000 {
      let mut moved = wire.send(&mut sender, true, PACKET_BUDGET, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(wire.now, id));
      }
      moved += wire.send(&mut receiver, false, PACKET_BUDGET, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      if sender.send_complete() && ids.iter().all(|(_, id)| receiver.recv_stream_complete(*id)) {
        break;
      }
      if moved == 0 {
        assert!(wire.advance(&mut sender, &mut receiver), "stalled");
      }
    }
    (relabel(received, &ids), wire.widest, sender.retransmitted())
  }

  /// AC (§4.10a §8): three streams multiplexed over one connection each arrive exactly, in order, with no
  /// loss — and none is confused for another.
  #[test]
  fn three_streams_multiplex_losslessly() {
    let streams = vec![
      (1u64, stream_content(1, 500)),
      (3u64, stream_content(2, 20)),
      (7u64, stream_content(3, 300)),
    ];
    let (received, retransmits) = transfer(&streams, Channel::new(Vec::new()));
    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived exactly"
      );
    }
    assert_eq!(retransmits, 0, "nothing retransmitted with no loss");
  }

  /// AC (§4.10a §8, the connection tier of the dual-level credit law): the connection-wide flow-control
  /// window bounds the total unread bytes across all streams. Non-vacuous: the in-flight total reached
  /// within a frame of the window.
  #[test]
  fn the_connection_window_bounds_total_in_flight_across_streams() {
    let window = initial_receive_window(FRAME_CAP);
    // Three streams of two frames each: together past the four-frame window, each within it.
    let each = 2 * FRAME_DATA;
    let streams: Vec<(u64, Vec<u8>)> = vec![
      (1, stream_content(1, each)),
      (3, stream_content(2, each)),
      (7, stream_content(3, each)),
    ];
    assert!((streams.len() * each) as u64 > window && (each as u64) < window);
    let (received, peak) = window_bounded_transfer(&streams, window);
    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived exactly"
      );
    }
    assert!(
      peak >= window - FRAME_DATA as u64,
      "the connection window was never saturated ({peak})"
    );
  }

  /// §4.10a (the constrained-link design §5.3; `docs/bugs/2026-09-30-bulk-spent-the-connection-credit-a-control-exchange-needed.md`):
  /// connection credit is taken in priority order too. Do X (two bulk exchanges, each longer than the
  /// window, send until the bulk class stops on connection credit, with nothing read by the peer; then a
  /// control exchange begins), expect Y (the bulk class had the whole window — the reserve is on top of
  /// it — and the control exchange leaves in the next packets the pacer releases, with no credit update
  /// from the peer). Until 2026-09-30 the bulk class spent the last byte of connection credit and the
  /// control exchange waited a round trip for it.
  #[test]
  fn a_control_exchange_leaves_at_once_when_bulk_has_spent_its_credit() {
    let window = initial_receive_window(FRAME_CAP);
    let mut sender = fixed_window(FRAME_CAP);
    for seed in [4, 5] {
      let bulk = stream_content(seed, usize::try_from(2 * window).unwrap());
      sender.open_exchange(KIND, Priority::Bulk, &bulk).unwrap();
    }
    let now = send_until(&mut sender, 1_000, |sender, _| sender.stops.credit > 0);
    assert!(sender.stops.credit > 0, "the bulk class stopped on credit");
    assert_eq!(
      sender.connection_sent, window,
      "the bulk class had the whole window"
    );
    let ping = sender
      .open_exchange(KIND, Priority::Control, &stream_content(6, 8))
      .unwrap();
    let carries_ping = |frames: &[Frame]| {
      frames
        .iter()
        .any(|frame| matches!(frame, Frame::Stream { stream_id, .. } if *stream_id == ping))
    };
    let mut carried = false;
    send_until(&mut sender, now, |_, frames| {
      carried = carried || frames.is_some_and(carries_ping);
      carried
    });
    assert!(carried, "the control exchange left without a credit update");
  }

  /// The scheduler's work for one fresh frame does not grow with streams that have nothing to send. Do: a
  /// sender holding `idle` exchanges framed whole and not yet acknowledged (each stays a send stream until
  /// its frames are acknowledged), then one bulk exchange in the same class; send three of its frames (the window
  /// lets out about that much with nothing acknowledged).
  /// Expect: as many streams examined per frame beside 24 idle streams as beside none, and the frames sent
  /// (else the test proves nothing). The round-robin once stepped over every idle stream for each frame.
  #[test]
  fn a_fresh_frame_costs_the_same_beside_any_number_of_idle_streams() {
    let examined_per_frame = |idle: u8| {
      let window = initial_receive_window(FRAME_CAP);
      // Shape: a ceiling of many windows, so the stream limit admits the idle streams.
      let ceiling = window.saturating_mul(1_024);
      let mut sender = Connection::new(
        ConnectionShape {
          max_datagram: FRAME_CAP as u64,
          // Shape: credit for every idle exchange past the class reserves, so only the scheduler is measured.
          initial_window: window.saturating_mul(64),
          receive_ceiling: ceiling,
        },
        Role::Client,
      );
      for seed in 0..idle {
        sender
          .open_exchange(KIND, Priority::Bulk, &stream_content(seed, 8))
          .unwrap();
      }
      let now = send_until(&mut sender, 1_000, |sender, _| {
        sender.send_streams.values().all(StreamSender::is_drained)
      });
      assert!(
        sender.send_streams.values().all(StreamSender::is_drained),
        "the idle exchanges were framed whole: {} of {} drained, stops {:?}, in flight {}",
        sender
          .send_streams
          .values()
          .filter(|s| s.is_drained())
          .count(),
        sender.send_streams.len(),
        sender.stops,
        sender.in_flight
      );
      let bulk = sender
        .open_exchange(KIND, Priority::Bulk, &stream_content(99, 64 * FRAME_DATA))
        .unwrap();
      let before = sender.stops.examined;
      let mut frames = 0u64;
      send_until(&mut sender, now, |_, sent| {
        frames += sent.map_or(0, |sent| {
          sent
            .iter()
            .filter(|frame| matches!(frame, Frame::Stream { stream_id, .. } if *stream_id == bulk))
            .count() as u64
        });
        frames >= 3
      });
      // Shape: three frames, what the congestion window lets out with nothing acknowledged.
      assert!(frames >= 3, "the bulk exchange sent {frames} frames");
      (sender.stops.examined - before) / frames
    };
    // Shape: 24 idle streams, within what the first congestion window lets out with nothing acknowledged.
    assert_eq!(examined_per_frame(0), examined_per_frame(24));
  }

  /// Polls `sender` from `now`, moving time to each pacing release, until `done` (given the sender and the
  /// frames just sent, if any) holds or a bounded number of polls pass; returns the time reached.
  fn send_until(
    sender: &mut Connection,
    mut now: u64,
    mut done: impl FnMut(&Connection, Option<&[Frame]>) -> bool,
  ) -> u64 {
    for _ in 0..1_000 {
      if done(sender, None) {
        break;
      }
      assert!(
        sender.ready_is_exact(),
        "the ready sets hold exactly the sendable streams"
      );
      match sender.poll_transmit(now, FRAME_CAP) {
        Some((_, frames)) => {
          if done(sender, Some(&frames)) {
            break;
          }
        }
        None => now = sender.pacing_release.unwrap_or(now).max(now + 1),
      }
    }
    now
  }

  /// Runs `streams` over a lossless path, asserting on every send that the total sent across streams is
  /// within `window` of the total read; returns what arrived and the largest such lead seen.
  fn window_bounded_transfer(
    streams: &[(u64, Vec<u8>)],
    window: u64,
  ) -> (BTreeMap<u64, Vec<u8>>, u64) {
    let mut sender = fixed_window(FRAME_CAP);
    let ids = open_labelled(&mut sender, streams);
    let mut receiver = fixed_receiver(FRAME_CAP);
    let mut wire = Wire::new(MS, Channel::new(Vec::new()));
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut peak = 0u64;
    for _ in 0..1_000_000 {
      let reads: u64 = ids.iter().map(|(_, id)| receiver.read_offset(*id)).sum();
      let mut moved = wire.send(&mut sender, true, FRAME_CAP, |from| {
        let sent: u64 = ids.iter().map(|(_, id)| from.send_offset(*id)).sum();
        peak = peak.max(sent - reads);
        assert!(
          sent <= reads + window,
          "the connection raced {sent} > {reads} + {window}"
        );
      });
      moved += wire.deliver(&mut sender, &mut receiver);
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(wire.now, id));
      }
      moved += wire.send(&mut receiver, false, FRAME_CAP, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      if sender.send_complete() && ids.iter().all(|(_, id)| receiver.recv_stream_complete(*id)) {
        break;
      }
      if moved == 0 {
        assert!(wire.advance(&mut sender, &mut receiver), "stalled");
      }
    }
    (relabel(received, &ids), peak)
  }

  /// AC (§4.10a §8, RFC 9000 §13.2.4 ACK-of-ACK; RFC 9000 §4.6 stream credit): on a connection reused
  /// across many more request/reply exchanges than its stream limit, the receive-side acknowledgement set
  /// stays bounded, every exchange delivers exactly, the stream credit keeps flowing (the server's
  /// `MaxStreams` after each close), and nothing leaks: once the last exchange ends, both ends hold no
  /// stream state at all. Non-vacuous: the exchanges outnumber the limit many times over, so they complete
  /// only if the credit is raised.
  #[test]
  fn many_exchanges_reuse_a_connection_with_bounded_state_and_flowing_credit() {
    /// Shape: enough exchanges that an unpruned set would dwarf the bound below, and many times the
    /// stream limit.
    const EXCHANGES: u64 = 50;
    /// Shape: a few packets each way per exchange.
    const BODY: usize = 24;
    let mut a = fixed_window(FRAME_CAP);
    let mut b = fixed_receiver(FRAME_CAP);
    assert!(EXCHANGES > 4 * b.streams.limit(), "past the stream limit");
    let mut exchange = Lockstep {
      now: 0,
      peak_tracked: 0,
    };
    for i in 0..EXCHANGES {
      let request = stream_content(u8::try_from(i % 7).unwrap_or(0), BODY);
      let reply = stream_content(u8::try_from(i % 5).unwrap_or(0).wrapping_add(100), BODY);
      let (received_request, received_reply) = exchange.run(&mut a, &mut b, &request, &reply);
      assert_eq!(
        received_request, request,
        "exchange {i}: the request arrived exactly"
      );
      assert_eq!(
        received_reply, reply,
        "exchange {i}: the reply arrived exactly"
      );
    }
    let peak_tracked = exchange.peak_tracked;
    assert!(peak_tracked > 0, "exchanges actually ran and were tracked");
    assert!(
      peak_tracked < 40,
      "ACK-of-ACK kept the receive set bounded (peak {peak_tracked})"
    );
    for (end, census) in [("client", a.census()), ("server", b.census())] {
      assert_eq!(
        (
          census.streams,
          census.send_streams,
          census.recv_streams,
          census.unacked
        ),
        (StreamCensus::default(), 0, 0, 0),
        "the {end} holds no stream state once every exchange ended: {census:?}"
      );
    }
  }

  /// RFC 9002 §5 (RTT samples come from ack-eliciting packets): once an exchange is done, the server sends
  /// its idle peer nothing it must acknowledge — the stream credit its close raised rides acknowledgements.
  /// Do X (complete one exchange, then take everything the server still has to send), expect Y (only
  /// acknowledgement and credit frames, and nothing in flight). Regression: the raised credit went out in
  /// an ack-eliciting packet, which a peer idle between exchanges acknowledged only when it next woke — the
  /// server's RTT samples split between the path's 100 ms and 2.5-3.5 s, its probe timeout reached 6.6 s,
  /// and a lost reply cost 7 s (64 kbit/s, 100 ms, 1 % loss, 2026-09-28).
  #[test]
  fn a_finished_exchange_leaves_the_idle_peer_nothing_to_acknowledge() {
    let mut a = fixed_window(FRAME_CAP);
    let mut b = fixed_receiver(FRAME_CAP);
    let mut exchange = Lockstep {
      now: 0,
      peak_tracked: 0,
    };
    let _ = exchange.run(&mut a, &mut b, &stream_content(1, 8), &stream_content(2, 8));
    while let Some((_pn, frames)) = b.poll_transmit(exchange.now, FRAME_CAP) {
      for frame in &frames {
        assert!(
          matches!(
            frame,
            Frame::Ack { .. }
              | Frame::MaxData { .. }
              | Frame::MaxStreams { .. }
              | Frame::MaxStreamData { .. }
          ),
          "the server sent its idle peer {frame:?}"
        );
      }
    }
    assert_eq!(
      b.in_flight_count(),
      0,
      "nothing the idle peer must acknowledge"
    );
  }

  /// A lossless request/reply driver stepping two connections one packet each way per step, so each data
  /// packet also carries the pending acknowledgement; its clock jumps to the next timer when nothing moves.
  struct Lockstep {
    now: u64,
    /// The most packet numbers either end tracked for acknowledgement at once.
    peak_tracked: usize,
  }

  impl Lockstep {
    /// Runs one exchange: `a` sends `request`, `b` answers with `reply` once the request is whole, and the
    /// run ends when `a` has taken the reply and both ends' sends are acknowledged. Returns what each end
    /// received.
    fn run(
      &mut self,
      a: &mut Connection,
      b: &mut Connection,
      request: &[u8],
      reply: &[u8],
    ) -> (Vec<u8>, Vec<u8>) {
      let sid = open(a, request);
      let (mut received_request, mut received_reply) = (Vec::new(), Vec::new());
      for guard in 0..100_000 {
        assert!(
          guard < 99_999,
          "the exchange must make progress: client {:?} credit {} waiting {:?} in flight {} / server {:?} credit {} in flight {} now {}",
          a.census(),
          a.streams.credit(),
          a.streams.waiting_limit(),
          a.in_flight_count(),
          b.census(),
          b.streams.credit(),
          b.in_flight_count(),
          self.now
        );
        let a_sent = self.carry(a, b);
        received_request.extend(b.read_stream(self.now, sid));
        if b.recv_stream_complete(sid) {
          b.close_recv(sid);
          b.reply(sid, reply)
            .expect("the request's stream awaits its reply");
        }
        let b_sent = self.carry(b, a);
        received_reply.extend(a.read_stream(self.now, sid));
        if a.recv_stream_complete(sid) {
          a.close_recv(sid);
        }
        self.peak_tracked = self
          .peak_tracked
          .max(a.acks_tracked())
          .max(b.acks_tracked());
        let reply_taken = !a.receiving(sid) && !received_reply.is_empty();
        if reply_taken && a.send_complete() && b.send_complete() && a.in_flight_count() == 0 {
          break;
        }
        if !a_sent && !b_sent {
          self.now = [a.next_timeout(), b.next_timeout()]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.now + MS)
            .max(self.now);
          a.on_timeout(self.now);
          b.on_timeout(self.now);
        }
      }
      (received_request, received_reply)
    }

    /// Carries one packet from `from` to `to`, if `from` has one; whether it did.
    fn carry(&self, from: &mut Connection, to: &mut Connection) -> bool {
      match from.poll_transmit(self.now, FRAME_CAP) {
        Some((pn, frames)) => {
          to.handle_incoming(self.now, pn, &frames);
          true
        }
        None => false,
      }
    }
  }

  /// AC (§4.10a §8; RFC 9002 §6.2): a lone data packet that is dropped — a tail loss neither threshold
  /// can see, since nothing after it is acknowledged — is recovered by the probe timeout.
  #[test]
  fn a_single_packet_tail_loss_is_probed() {
    let streams = vec![(1u64, stream_content(4, 4))];
    let (received, sender) = transfer_on(&streams, FRAME_CAP, Wire::new(MS, Channel::new(vec![0])));
    assert_eq!(
      received.get(&1),
      Some(&stream_content(4, 4)),
      "the lone packet arrived"
    );
    assert!(sender.retransmitted() >= 1, "the probe recovered it");
  }

  /// RFC 9002 §5.1: the round trip is measured from the largest newly acknowledged packet's send to the
  /// acknowledgement — over a path of 20 ms each way, 40 ms.
  #[test]
  fn the_rtt_is_measured_from_send_to_acknowledgement() {
    let (_, sender) = transfer_on(
      &[(1u64, stream_content(1, 24))],
      FRAME_CAP,
      Wire::new(20 * MS, Channel::new(Vec::new())),
    );
    assert!(sender.rtt().has_sample());
    assert_eq!(
      sender.rtt().min_rtt(),
      40 * MS,
      "the minimum is the path's round trip"
    );
  }

  /// AC (§4.10a §8): multiplexed streams all arrive exactly despite dropped packets.
  #[test]
  fn multiplexed_streams_survive_loss() {
    let streams = vec![
      (1u64, stream_content(1, 400)),
      (2u64, stream_content(9, 400)),
    ];
    let (received, sender) = transfer_on(
      &streams,
      FRAME_CAP,
      Wire::new(MS, Channel::forward(vec![3, 4])),
    );
    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived despite loss"
      );
    }
    assert!(sender.retransmitted() >= 1, "the loss-recovery path ran");
  }

  /// RFC 9002 §6.1.2: a packet lost behind fewer than three later packets is still declared lost once it
  /// is older than 9/8 of the RTT — by the loss timer, not a probe timeout — and retransmitted.
  #[test]
  fn the_time_threshold_declares_a_loss_the_packet_threshold_cannot() {
    let mut sender = fixed_window(FRAME_CAP);
    let mut receiver = fixed_receiver(FRAME_CAP);
    sender.seed_rtt(10 * MS, 0);
    let _ = open(&mut sender, &stream_content(1, 3 * FRAME_CAP));
    let mut packets: Vec<(u64, Vec<Frame>)> = Vec::new();
    let mut sent_at = 0;
    while packets.len() < 3 {
      match sender.poll_transmit(sent_at, FRAME_CAP) {
        Some(packet) => packets.push(packet),
        None => sent_at = sender.next_timeout().expect("the pacer's release"),
      }
    }
    assert!(
      sent_at < 5 * MS,
      "the three left within the first few milliseconds"
    );
    // Packet 0 is lost; 1 and 2 arrive and are acknowledged at 10 ms.
    for (pn, frames) in &packets[1..] {
      receiver.handle_incoming(5 * MS, *pn, frames);
    }
    let (pn, ack) = receiver
      .poll_transmit(5 * MS, FRAME_CAP)
      .expect("an acknowledgement");
    sender.handle_incoming(10 * MS, pn, &ack);
    assert_eq!(
      sender.retransmitted(),
      0,
      "one packet behind two is not lost by count"
    );
    let due = sender.next_timeout().expect("the loss timer is armed");
    assert!(
      due <= 10 * MS + 10 * MS * 9 / 8,
      "armed at 9/8 of the RTT past the send, not a PTO"
    );
    assert!(sender.on_timeout(due));
    assert_eq!(
      sender.retransmitted(),
      1,
      "the timer declared packet 0 lost"
    );
  }

  /// RFC 9002 §7.7: once the RTT is known, ack-eliciting packets leave spaced at the pacing rate rather
  /// than all at once, after an initial burst of one send quantum.
  #[test]
  fn data_is_paced_after_the_first_quantum() {
    // An initial window covering the whole stream, so flow credit never stops the sender (the test has no
    // receiver to grant more) and only the pacer spaces the packets.
    let shape = ConnectionShape {
      max_datagram: 1000,
      initial_window: 20_000,
      receive_ceiling: 20_000,
    };
    let mut sender = Connection::new(shape, Role::Client);
    sender.seed_rtt(100 * MS, 0);
    let _ = open(&mut sender, &vec![7u8; 20_000]);
    let mut sends = Vec::new();
    let mut now = 0;
    for _ in 0..1000 {
      if sends.len() == 6 {
        break;
      }
      match sender.poll_transmit(now, 1000) {
        Some(_) => sends.push(now),
        None => {
          now = sender
            .pacing_release
            .expect("only the pacer holds the sender")
        }
      }
    }
    assert_eq!(sends.len(), 6, "all six packets left");
    // After the quantum (the 2-packet floor) a packet leaves every interval the controller's pacing rate
    // gives its stream data: a 1000-byte packet budget carries 978 bytes (one `Stream` frame header). With
    // Copa's 2 × cwnd / RTT — 10 kB over 100 ms — that is 978 / 200,000 s = 4.89 ms.
    let rate = sender.controller().pacing_rate(sender.rtt());
    let pace = crate::pacer::duration(stream_bytes_per_packet(1000), rate);
    assert_eq!(&sends[..2], &[0, 0], "the first quantum leaves at once");
    for pair in sends[2..].windows(2) {
      assert_eq!(
        pair[1] - pair[0],
        pace,
        "then one packet per data-sized interval"
      );
    }
  }

  /// RFC 9002 §7.6.2: losses spanning more than three probe timeouts with nothing acknowledged between —
  /// the path went dark — collapse the window to the minimum.
  #[test]
  fn persistent_congestion_collapses_the_window() {
    let mut sender = fixed_window(FRAME_CAP);
    let mut receiver = fixed_receiver(FRAME_CAP);
    sender.seed_rtt(10 * MS, 0);
    let _ = open(&mut sender, &stream_content(1, 20 * FRAME_CAP));
    // Everything sent in the first 500 ms is lost — the original flight and the probes the backed-off
    // timeout sends at about 31, 91, 211 and 451 ms — then the last probe gets through and is
    // acknowledged: the lost packets span 1–211 ms, past three PTOs (3 × 30 ms), with nothing acknowledged
    // between them.
    let mut now = 1;
    let mut last = None;
    while now < 500 * MS {
      if let Some(packet) = sender.poll_transmit(now, FRAME_CAP) {
        last = Some((now, packet));
      } else {
        let next = sender.next_timeout().unwrap_or(now + MS).max(now + 1);
        if next >= 500 * MS {
          break;
        }
        now = next;
        sender.on_timeout(now);
      }
    }
    // The path comes back: the last probe arrives a millisecond after it left, and its acknowledgement a
    // millisecond later — a prompt round trip, as a restored path gives.
    let (sent_at, (pn, frames)) = last.expect("something was sent");
    receiver.handle_incoming(sent_at + MS, pn, &frames);
    let (ack_pn, ack) = receiver
      .poll_transmit(sent_at + MS, FRAME_CAP)
      .expect("an acknowledgement");
    sender.handle_incoming(sent_at + 2 * MS, ack_pn, &ack);
    assert_eq!(
      sender.congestion_window(),
      2 * FRAME_CAP as u64,
      "the window collapsed to the minimum"
    );
  }

  /// §4.10a §8 "BDP-autotuned" (Chromium QUIC's rule): over a path whose BDP far exceeds the initial
  /// window, the receive window doubles whenever a window is read within two round trips, up to the
  /// ceiling and never past it — the connection's credit, the window and the class reserve on top of it
  /// ([`class_credit_reserve`]), is what the ceiling bounds — and the transfer completes.
  #[test]
  fn the_receive_window_autotunes_to_its_ceiling() {
    let cap = 1200usize;
    let initial = initial_receive_window(cap);
    let ceiling = 64 * initial;
    let shape = ConnectionShape {
      max_datagram: cap as u64,
      initial_window: initial,
      receive_ceiling: ceiling,
    };
    let mut sender = Connection::new(shape, Role::Client);
    let mut receiver = Connection::new(shape, Role::Server);
    // A receiver only acknowledges, which draws no RTT sample of its own; a session's handshake seeds both
    // ends' estimates (`Endpoint::establish`), which this does in its place.
    sender.seed_rtt(40 * MS, 0);
    receiver.seed_rtt(40 * MS, 0);
    let content = vec![5u8; 4 << 20];
    let stream = open(&mut sender, &content);
    let mut wire = Wire::new(20 * MS, Channel::new(Vec::new()));
    let mut received = Vec::new();
    for _ in 0..10_000_000 {
      let mut moved = wire.send(&mut sender, true, cap, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      received.extend(receiver.read_stream(wire.now, stream));
      moved += wire.send(&mut receiver, false, cap, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      if receiver.recv_stream_complete(stream) && sender.send_complete() {
        break;
      }
      if moved == 0 {
        assert!(wire.advance(&mut sender, &mut receiver), "stalled");
      }
    }
    assert_eq!(received.len(), content.len(), "the transfer completed");
    let (window, growths) = receiver.receive_window();
    assert!(growths > 0, "the window grew ({growths} times)");
    assert_eq!(
      window + class_credit_reserve(Priority::Bulk, cap),
      ceiling,
      "and stopped at the ceiling"
    );
  }

  proptest! {
    #![proptest_config(slates_test_seeds::seeded(proptest::test_runner::Config::default(), include_str!("../proptest-regressions/connection.txt")).unwrap())]
    /// The behavioural oracle (R5): for any streams and any loss pattern, the receiver reassembles each
    /// stream exactly — in order, each byte once, never confused.
    #[test]
    fn any_streams_any_loss_still_deliver(
      lens in prop::collection::vec(0usize..300, 1..4),
      drops in prop::collection::vec(0u64..150, 0..30),
    ) {
      let streams: Vec<(u64, Vec<u8>)> = lens
        .iter()
        .enumerate()
        .map(|(i, &len)| (u64::try_from(i * 2 + 1).unwrap_or(1), stream_content(u8::try_from(i).unwrap_or(0), len)))
        .collect();
      let (received, _) = transfer_on(&streams, FRAME_CAP, Wire::new(MS, Channel::new(drops)));
      for (id, content) in &streams {
        prop_assert_eq!(received.get(id), Some(content));
      }
    }

    /// The reordering oracle: any streams, any loss, out-of-order delivery — still exact.
    #[test]
    fn any_streams_survive_loss_and_reorder(
      lens in prop::collection::vec(0usize..200, 1..4),
      drops in prop::collection::vec(0u64..120, 0..20),
    ) {
      let streams: Vec<(u64, Vec<u8>)> = lens
        .iter()
        .enumerate()
        .map(|(i, &len)| (u64::try_from(i * 2 + 1).unwrap_or(1), stream_content(u8::try_from(i).unwrap_or(0), len)))
        .collect();
      let (received, _) = transfer_on(&streams, FRAME_CAP, Wire::new(MS, Channel::reordering(drops)));
      for (id, content) in &streams {
        prop_assert_eq!(received.get(id), Some(content));
      }
    }
  }

  /// A long transfer under steady random loss (5 % both ways, the bake-off's thin-link case) completes
  /// across many seeds — no deadlock between the sender's credit and the receiver's acknowledgements.
  #[test]
  fn a_long_transfer_under_random_loss_completes() {
    for seed in 1..=20u64 {
      let mut rng = slates_machine::stats::Xorshift::new(seed);
      let drops: Vec<u64> = (0..20_000u64).filter(|_| rng.below(20) == 0).collect();
      let streams = vec![(1u64, stream_content(7, 16 * 1024))];
      let (received, sender) = transfer_on(&streams, 64, Wire::new(10 * MS, Channel::new(drops)));
      assert_eq!(
        received.get(&1).map(Vec::len),
        Some(16 * 1024),
        "seed {seed}: stalled with {} in flight, window {}, rtx {}",
        sender.bytes_in_flight(),
        sender.congestion_window(),
        sender.retransmitted()
      );
    }
  }

  /// RFC 9002 §6.2.4 (probes are copies; the original stays tracked): a peer that never answers costs the
  /// originals plus [`PROBE_COPIES_KEPT`] tracked copies, however long it stays silent — a later probe's
  /// copy makes an older one redundant. Do X (send to a peer that never acknowledges and run every timer
  /// across many probe timeouts), expect Y (the probes keep going, and the tracked packets never exceed the
  /// originals plus the copies kept). Regression: each probe timeout added a tracked copy for as long as the session was held —
  /// 136,106 packets after 90,640 virtual seconds (2026-09-28). Non-vacuous: many probes were sent.
  #[test]
  fn a_silent_peer_costs_bounded_tracking_however_long_it_is_silent() {
    /// Shape: probe timeouts to run through — far past where the old growth was plain.
    const PROBE_TIMEOUTS: u64 = 1_000;
    let mut sender = fixed_window(FRAME_CAP);
    sender.seed_rtt(10 * MS, 0);
    // Two frames: the whole stream leaves in the first flight (the initial window holds more), so every
    // later packet is a probe copy and `originals` counts every original there will be.
    let _ = open(&mut sender, &stream_content(3, 2 * FRAME_DATA));
    let mut now = 0;
    while sender.poll_transmit(now, FRAME_CAP).is_some() {}
    let originals = sender.in_flight_count();
    assert_eq!(originals, 2, "the whole stream left in the first flight");
    assert!(originals > 0, "the stream is in flight");
    let mut peak = originals;
    let mut fired = 0u64;
    while fired < PROBE_TIMEOUTS {
      now = sender
        .next_timeout()
        .expect("data is owed, so a timer is armed");
      if sender.on_timeout(now) {
        fired += 1;
      }
      while sender.poll_transmit(now, FRAME_CAP).is_some() {}
      peak = peak.max(sender.in_flight_count());
    }
    assert!(
      sender.retransmitted() >= PROBE_TIMEOUTS,
      "the probes kept going ({} frames resent)",
      sender.retransmitted()
    );
    assert!(
      peak <= originals + PROBE_COPIES_KEPT,
      "tracking stayed bounded: peak {peak} packets for {originals} originals over {PROBE_TIMEOUTS} probe timeouts"
    );
  }

  /// §4.8 request/reply on one stream id (`Endpoint::serve_once`): the server has served a request and
  /// its reply is in flight on the same id; a late copy of the request then arrives (a probe the client
  /// sent before it heard anything). The copy meets a closed receiving half and is dropped — it neither
  /// reopens the stream nor touches the reply, which stays in flight and is still delivered. Regression:
  /// discarding the copy forgot the whole stream, so the reply left tracking unacknowledged, counted
  /// complete, and the client waited forever (a 64 kbit/s, 5 %-loss bake-off run deadlocked; 2026-09-27).
  #[test]
  fn a_late_request_copy_keeps_the_reply_in_flight() {
    let mut client = fixed_window(FRAME_CAP);
    let mut server = fixed_receiver(FRAME_CAP);
    let id = open(&mut client, &stream_content(1, 4));
    let (request_pn, request) = client.poll_transmit(0, FRAME_CAP).expect("the request");
    server.handle_incoming(0, request_pn, &request);
    assert_eq!(
      server.read_stream(0, id).len(),
      4,
      "the server read the request"
    );
    assert!(server.recv_stream_complete(id));
    server.close_recv(id);
    // The server replies on the same id; the reply packet is lost on the path.
    server
      .reply(id, &stream_content(2, 4))
      .expect("the request awaits its reply");
    // Everything the server sends now is lost: the acknowledgement of the request and the reply (at this
    // small budget they leave in separate packets).
    assert!(drain(&mut server, 0) > 0, "the reply left");
    // The client, having heard nothing, probes: a copy of its request in a new packet reaches the server.
    assert!(client.probe());
    let (probe_pn, probe) = client.poll_transmit(MS, FRAME_CAP).expect("the probe");
    server.handle_incoming(MS, probe_pn, &probe);
    assert!(
      !server.recv_stream_ids().contains(&id),
      "the late copy did not reopen the closed receiving half"
    );
    assert!(!server.send_complete(), "the reply is still owed");
    assert_eq!(
      server.in_flight_count(),
      1,
      "the reply packet is still in flight"
    );
    assert_eq!(
      recover_by_probe(&mut server, &mut client, id),
      stream_content(2, 4),
      "the reply reached the client"
    );
    assert!(client.recv_stream_complete(id));
  }

  /// Takes every packet `conn` has to send at `now` (dropping them) and returns how many there were.
  fn drain(conn: &mut Connection, now: u64) -> usize {
    let mut packets = 0;
    while conn.poll_transmit(now, FRAME_CAP).is_some() {
      packets += 1;
    }
    packets
  }

  /// Fires `sender`'s probe timeout and carries everything it then sends to `receiver`, returning what
  /// `receiver` reads on stream `id` — the lost packet's recovery.
  fn recover_by_probe(sender: &mut Connection, receiver: &mut Connection, id: u64) -> Vec<u8> {
    let now = sender.next_timeout().expect("the probe timer is armed");
    assert!(sender.on_timeout(now), "the probe timeout owes a probe");
    while let Some((pn, frames)) = sender.poll_transmit(now, FRAME_CAP) {
      receiver.handle_incoming(now, pn, &frames);
    }
    receiver.read_stream(now, id)
  }
}
