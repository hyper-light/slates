//! The SWIM wire (§4.8, §4.10a) — the on-the-wire encoding of the failure detector's messages so a
//! [`Detector`](crate::detector::Detector)'s [`Ping`](crate::detector::Ping),
//! [`Ack`](crate::detector::Ack) and [`PingReq`](crate::detector::PingReq) can ride the fleet
//! transport, each piggybacking a bounded batch of gossiped membership updates (SWIM's infection-style
//! dissemination shares the probe traffic). The live driver that sends these over authenticated
//! sessions and feeds replies back into the detector is composed on top; this module is the pure codec.
//!
//! An acknowledgement also carries the sender's Vivaldi network coordinate ([`crate::coordinates`]), so
//! a prober learns the coordinate of every peer it probes and can predict the round-trip time to it —
//! the live half of coordinate-aware indirect probing.
//!
//! Every decode is a parser of external bytes, so it checks the length against the message's shape
//! before allocating and rejects a truncated header, an unknown tag, an unknown liveness byte, a gossip
//! count that does not match the bytes that arrived, or a coordinate declaring more dimensions than the
//! decoder accepts — a hostile datagram is a typed [`SwimWireError`], never a panic or an
//! over-allocation. The encoding is little-endian throughout (floats as their bit pattern) so two hosts
//! encode a message identically.

use std::mem::size_of;

use slates_db::register::HostId;
use slates_rt::error::RtError;
use slates_rt::futures::{now_ns, sleep};
use slates_transport::endpoint::{Endpoint, EndpointError};

use crate::CommitBudget;
use crate::coordinates::NetworkCoordinate;
use crate::detector::Detector;
use crate::membership::{Liveness, MemberState};

/// A SWIM message on the wire: a probe, its acknowledgement, or an indirect-probe request, each naming
/// the sender and carrying a piggybacked batch of membership updates to gossip. (`Eq` is not derived
/// because an acknowledgement carries the sender's Vivaldi coordinate, which holds floating point.)
#[derive(Clone, Debug, PartialEq)]
pub enum SwimMessage {
  /// A direct probe from `from`.
  Ping {
    /// The probing node.
    from: HostId,
    /// A per-probe token the acknowledgement must echo — the probe sequence number (memberlist's `SeqNo`;
    /// SWIM §4). It correlates the acknowledgement to *this* ping: without it a stale acknowledgement (one
    /// the peer sent to an earlier ping, before it died, that a real datagram socket buffered and the
    /// reliable transport redelivered on the reused probe stream) would pass for a fresh reply and keep a
    /// dead peer looking alive, so the survivor never retired it (`docs/bugs/2026-09-10-swim-stale-ack.md`).
    nonce: u64,
    /// The prober's random per-start nonce (§4.8, AUD-07), from which its member id derives
    /// together with the authenticated certificate anchor. A different nonce identifies a
    /// different member; its numeric value cannot establish an ordering between starts.
    boot_nonce: u64,
    /// The membership updates piggybacked on this probe.
    gossip: Vec<(HostId, MemberState)>,
  },
  /// An acknowledgement from `from` (the reply to a [`SwimMessage::Ping`] or an indirect probe), carrying
  /// the sender's network coordinate so the prober learns it (and can predict the RTT to it).
  Ack {
    /// The acknowledging node.
    from: HostId,
    /// Echoes the probing [`Ping`]'s `nonce`, so the prober counts this acknowledgement only for the probe
    /// it is answering — never for an earlier one whose reply was redelivered.
    nonce: u64,
    /// The acknowledging node's daemon boot_nonce — the same announcement a ping makes, so the prober
    /// validates the id its peer answers under exactly as the peer validates the prober's.
    boot_nonce: u64,
    /// The membership updates piggybacked on this acknowledgement.
    gossip: Vec<(HostId, MemberState)>,
    /// The acknowledging node's Vivaldi coordinate.
    coordinate: NetworkCoordinate,
  },
  /// A request from `from` to probe `target` on its behalf (the indirect probe, SWIM §4.1: the `k`
  /// ping-requests a prober sends when its direct ping goes unanswered, so a lost packet is retried through
  /// relays before the target is suspected).
  PingReq {
    /// The requesting node.
    from: HostId,
    /// The node to probe indirectly.
    target: HostId,
    /// The requester's probe nonce for the direct ping that went unanswered: the relay echoes it in its
    /// [`SwimMessage::IndirectAck`], so the requester credits the relayed answer to *that* probe (and
    /// rejects one echoing a probe older than its suspicion window), exactly as a direct acknowledgement
    /// is correlated by nonce.
    nonce: u64,
    /// The membership updates piggybacked on this request.
    gossip: Vec<(HostId, MemberState)>,
  },
  /// A relay's answer to a [`SwimMessage::PingReq`]: `from` (the relay) reached `target` on the
  /// requester's behalf — its own probe of the target was acknowledged — and reports that back, echoing the
  /// requester's probe `nonce`. Carried on the relay's own probe session to the requester (the relay's
  /// serve side cannot answer the request inline: the relay's probe of the target is driven by the task
  /// that owns that target's session), so it announces the relay's `boot_nonce` like any message the relay
  /// originates, and the requester validates the relay's identity before crediting it.
  IndirectAck {
    /// The relay that reached the target.
    from: HostId,
    /// The member the relay reached.
    target: HostId,
    /// The requester's probe nonce the relay was asked about.
    nonce: u64,
    /// The relay's daemon boot_nonce — its own identity announcement.
    boot_nonce: u64,
    /// The membership updates piggybacked on this answer.
    gossip: Vec<(HostId, MemberState)>,
  },
}

/// A refusal to decode a SWIM message from received bytes (the closed hostile-input taxonomy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwimWireError {
  /// The bytes are shorter than the message's fixed header requires.
  Truncated,
  /// The leading tag byte is not one of the known message kinds.
  UnknownTag {
    /// The tag byte that arrived.
    tag: u8,
  },
  /// A gossip entry's liveness byte is not `Alive`, `Suspect` or `Dead`.
  UnknownLiveness {
    /// The liveness byte that arrived.
    liveness: u8,
  },
  /// The declared gossip-entry count does not match the number of bytes that followed — a truncated or
  /// over-long batch (the check that bounds allocation to what actually arrived).
  GossipLengthMismatch,
  /// An acknowledgement's network coordinate was malformed: too many dimensions (a hostile
  /// over-allocation), or a byte length that does not match the declared dimensions.
  MalformedCoordinate,
}

/// Format: the message tag occupies one leading byte; these are its values.
const TAG_PING: u8 = 1;
const TAG_ACK: u8 = 2;
const TAG_PING_REQ: u8 = 3;
const TAG_INDIRECT_ACK: u8 = 4;

/// Format: a liveness is one byte in a gossip entry; these are its values (the detector's three states).
const LIVENESS_ALIVE: u8 = 0;
const LIVENESS_SUSPECT: u8 = 1;
const LIVENESS_DEAD: u8 = 2;

/// Format: one gossip entry is a host id (u64), a liveness byte, and an incarnation (u64), little-endian.
const GOSSIP_ENTRY_BYTES: usize = size_of::<u64>() + size_of::<u8>() + size_of::<u64>();
/// Format: the piggybacked batch is prefixed by its entry count as a u32.
const GOSSIP_COUNT_BYTES: usize = size_of::<u32>();

impl SwimMessage {
  /// The sender named in the message (`from`).
  pub fn from(&self) -> HostId {
    match self {
      SwimMessage::Ping { from, .. }
      | SwimMessage::Ack { from, .. }
      | SwimMessage::PingReq { from, .. }
      | SwimMessage::IndirectAck { from, .. } => *from,
    }
  }

  /// The piggybacked gossip batch.
  pub fn gossip(&self) -> &[(HostId, MemberState)] {
    match self {
      SwimMessage::Ping { gossip, .. }
      | SwimMessage::Ack { gossip, .. }
      | SwimMessage::PingReq { gossip, .. }
      | SwimMessage::IndirectAck { gossip, .. } => gossip,
    }
  }

  /// The sender's network coordinate, when the message is an acknowledgement (which carries it).
  pub fn coordinate(&self) -> Option<&NetworkCoordinate> {
    match self {
      SwimMessage::Ack { coordinate, .. } => Some(coordinate),
      SwimMessage::Ping { .. } | SwimMessage::PingReq { .. } | SwimMessage::IndirectAck { .. } => {
        None
      }
    }
  }

  /// The probe token: a [`Ping`](SwimMessage::Ping)'s nonce, the value an [`Ack`](SwimMessage::Ack)
  /// echoes back, the requester's probe nonce a [`PingReq`](SwimMessage::PingReq) asks about, or the one
  /// an [`IndirectAck`](SwimMessage::IndirectAck) echoes. The prober compares its ping's nonce to the
  /// acknowledgement's — direct or relayed — to reject a stale reply.
  pub fn nonce(&self) -> Option<u64> {
    match self {
      SwimMessage::Ping { nonce, .. }
      | SwimMessage::Ack { nonce, .. }
      | SwimMessage::PingReq { nonce, .. }
      | SwimMessage::IndirectAck { nonce, .. } => Some(*nonce),
    }
  }

  /// The sender's announced daemon boot_nonce: a [`Ping`](SwimMessage::Ping)'s, an
  /// [`Ack`](SwimMessage::Ack)'s or an [`IndirectAck`](SwimMessage::IndirectAck)'s (each is the sender's
  /// own announcement); `None` for a ping-request, which asks about a third party and announces nothing
  /// about its sender's identity (the requester is identified by the authenticated session it arrives on).
  pub fn boot_nonce(&self) -> Option<u64> {
    match self {
      SwimMessage::Ping { boot_nonce, .. }
      | SwimMessage::Ack { boot_nonce, .. }
      | SwimMessage::IndirectAck { boot_nonce, .. } => Some(*boot_nonce),
      SwimMessage::PingReq { .. } => None,
    }
  }

  /// The third member an indirect exchange is about: a [`PingReq`](SwimMessage::PingReq)'s target, or the
  /// member an [`IndirectAck`](SwimMessage::IndirectAck) reports reached; `None` for a direct probe or its
  /// acknowledgement.
  pub fn target(&self) -> Option<HostId> {
    match self {
      SwimMessage::PingReq { target, .. } | SwimMessage::IndirectAck { target, .. } => {
        Some(*target)
      }
      SwimMessage::Ping { .. } | SwimMessage::Ack { .. } => None,
    }
  }

  /// The canonical little-endian bytes: the tag, the sender, the probe nonce and the sender's boot_nonce
  /// (a ping or an acknowledgement) or the target (a ping-request), then the gossip batch (its u32 count
  /// and each entry). Two hosts encode a message identically.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      SwimMessage::Ping {
        from,
        nonce,
        boot_nonce,
        gossip,
      } => {
        out.push(TAG_PING);
        out.extend_from_slice(&from.0.to_le_bytes());
        out.extend_from_slice(&nonce.to_le_bytes());
        out.extend_from_slice(&boot_nonce.to_le_bytes());
        encode_gossip(&mut out, gossip);
      }
      SwimMessage::Ack {
        from,
        nonce,
        boot_nonce,
        gossip,
        coordinate,
      } => {
        out.push(TAG_ACK);
        out.extend_from_slice(&from.0.to_le_bytes());
        out.extend_from_slice(&nonce.to_le_bytes());
        out.extend_from_slice(&boot_nonce.to_le_bytes());
        encode_gossip(&mut out, gossip);
        encode_coordinate(&mut out, coordinate);
      }
      SwimMessage::PingReq {
        from,
        target,
        nonce,
        gossip,
      } => {
        out.push(TAG_PING_REQ);
        out.extend_from_slice(&from.0.to_le_bytes());
        out.extend_from_slice(&target.0.to_le_bytes());
        out.extend_from_slice(&nonce.to_le_bytes());
        encode_gossip(&mut out, gossip);
      }
      SwimMessage::IndirectAck {
        from,
        target,
        nonce,
        boot_nonce,
        gossip,
      } => {
        out.push(TAG_INDIRECT_ACK);
        out.extend_from_slice(&from.0.to_le_bytes());
        out.extend_from_slice(&target.0.to_le_bytes());
        out.extend_from_slice(&nonce.to_le_bytes());
        out.extend_from_slice(&boot_nonce.to_le_bytes());
        encode_gossip(&mut out, gossip);
      }
    }
    out
  }

  /// Decodes a message from received bytes, or a typed refusal for a hostile or truncated datagram.
  pub fn decode(bytes: &[u8]) -> Result<SwimMessage, SwimWireError> {
    let (&tag, rest) = bytes.split_first().ok_or(SwimWireError::Truncated)?;
    match tag {
      TAG_PING => {
        let (from, rest) = take_host(rest)?;
        let (nonce, rest) = take_word(rest)?;
        let (boot_nonce, rest) = take_word(rest)?;
        let (gossip, leftover) = decode_gossip(rest)?;
        if !leftover.is_empty() {
          return Err(SwimWireError::GossipLengthMismatch);
        }
        Ok(SwimMessage::Ping {
          from,
          nonce,
          boot_nonce,
          gossip,
        })
      }
      TAG_ACK => {
        let (from, rest) = take_host(rest)?;
        let (nonce, rest) = take_word(rest)?;
        let (boot_nonce, rest) = take_word(rest)?;
        let (gossip, leftover) = decode_gossip(rest)?;
        let coordinate = decode_coordinate(leftover)?;
        Ok(SwimMessage::Ack {
          from,
          nonce,
          boot_nonce,
          gossip,
          coordinate,
        })
      }
      TAG_PING_REQ => {
        let (from, rest) = take_host(rest)?;
        let (target, rest) = take_host(rest)?;
        let (nonce, rest) = take_word(rest)?;
        let (gossip, leftover) = decode_gossip(rest)?;
        if !leftover.is_empty() {
          return Err(SwimWireError::GossipLengthMismatch);
        }
        Ok(SwimMessage::PingReq {
          from,
          target,
          nonce,
          gossip,
        })
      }
      TAG_INDIRECT_ACK => {
        let (from, rest) = take_host(rest)?;
        let (target, rest) = take_host(rest)?;
        let (nonce, rest) = take_word(rest)?;
        let (boot_nonce, rest) = take_word(rest)?;
        let (gossip, leftover) = decode_gossip(rest)?;
        if !leftover.is_empty() {
          return Err(SwimWireError::GossipLengthMismatch);
        }
        Ok(SwimMessage::IndirectAck {
          from,
          target,
          nonce,
          boot_nonce,
          gossip,
        })
      }
      other => Err(SwimWireError::UnknownTag { tag: other }),
    }
  }
}

/// Appends a gossip batch: its u32 entry count, then each entry (subject, liveness byte, incarnation).
fn encode_gossip(out: &mut Vec<u8>, gossip: &[(HostId, MemberState)]) {
  let count = u32::try_from(gossip.len()).unwrap_or(u32::MAX);
  out.extend_from_slice(&count.to_le_bytes());
  for (subject, state) in gossip
    .iter()
    .take(usize::try_from(count).unwrap_or(usize::MAX))
  {
    out.extend_from_slice(&subject.0.to_le_bytes());
    out.push(liveness_byte(state.liveness));
    out.extend_from_slice(&state.incarnation.to_le_bytes());
  }
}

/// Reads the u64 host id at the front of `bytes`, returning it and the remainder, or `Truncated`.
fn take_host(bytes: &[u8]) -> Result<(HostId, &[u8]), SwimWireError> {
  let (word, rest) = take_word(bytes)?;
  Ok((HostId(word), rest))
}

/// Reads a little-endian u64 at the front of `bytes` (the probe nonce, and the raw word `take_host`
/// wraps), returning it and the remainder, or `Truncated` if fewer than eight bytes remain.
fn take_word(bytes: &[u8]) -> Result<(u64, &[u8]), SwimWireError> {
  if bytes.len() < size_of::<u64>() {
    return Err(SwimWireError::Truncated);
  }
  let (head, rest) = bytes.split_at(size_of::<u64>());
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Ok((u64::from_le_bytes(word), rest))
}

/// A decoded gossip batch and the bytes that follow it in the message (the coordinate, for an
/// acknowledgement; empty otherwise).
type GossipAndRest<'a> = (Vec<(HostId, MemberState)>, &'a [u8]);

/// Decodes a gossip batch from the front of `bytes`: the u32 count, then exactly `count` entries,
/// returning the batch **and the bytes that follow it** (empty for a ping/ping-request, the coordinate
/// for an acknowledgement). The count is checked against the bytes that actually arrived before anything
/// is allocated, so a datagram claiming a huge count without the bytes to back it is refused rather than
/// over-allocating.
fn decode_gossip(bytes: &[u8]) -> Result<GossipAndRest<'_>, SwimWireError> {
  if bytes.len() < GOSSIP_COUNT_BYTES {
    return Err(SwimWireError::Truncated);
  }
  let (count_bytes, after_count) = bytes.split_at(GOSSIP_COUNT_BYTES);
  let mut count_word = [0u8; GOSSIP_COUNT_BYTES];
  count_word.copy_from_slice(count_bytes);
  let count = usize::try_from(u32::from_le_bytes(count_word)).unwrap_or(usize::MAX);

  // At least the entries the count declares must have arrived (anything after is the next field).
  let wanted = count
    .checked_mul(GOSSIP_ENTRY_BYTES)
    .ok_or(SwimWireError::GossipLengthMismatch)?;
  if after_count.len() < wanted {
    return Err(SwimWireError::GossipLengthMismatch);
  }
  let (mut rest, leftover) = after_count.split_at(wanted);

  let mut gossip = Vec::with_capacity(count);
  for _ in 0..count {
    let (entry, tail) = rest.split_at(GOSSIP_ENTRY_BYTES);
    let (subject, entry_rest) = take_host(entry)?;
    let (&liveness_byte, incarnation_bytes) =
      entry_rest.split_first().ok_or(SwimWireError::Truncated)?;
    let liveness = liveness_from_byte(liveness_byte)?;
    let mut incarnation_word = [0u8; size_of::<u64>()];
    incarnation_word.copy_from_slice(incarnation_bytes);
    gossip.push((
      subject,
      MemberState {
        liveness,
        incarnation: u64::from_le_bytes(incarnation_word),
      },
    ));
    rest = tail;
  }
  Ok((gossip, leftover))
}

/// Format: one coordinate component (and the height, adjustment and error) is a little-endian `f64`
/// stored as its bit pattern.
const F64_BYTES: usize = size_of::<u64>();
/// Format: the coordinate is a `u32` dimension count, that many `f64` vector components, then the
/// height, adjustment and error `f64`s.
const COORDINATE_SCALARS: usize = 3;
/// Shape: the largest coordinate dimension a decoder will accept before allocating — a hostile datagram
/// cannot force an unbounded vector. Far above any sensible Vivaldi dimension (the engine uses eight).
const MAX_COORDINATE_DIMS: usize = 64;

/// Appends a network coordinate: the u32 dimension count, each vector component, then height, adjustment
/// and error — every scalar a little-endian `f64` bit pattern.
fn encode_coordinate(out: &mut Vec<u8>, coordinate: &NetworkCoordinate) {
  let dims = u32::try_from(coordinate.vec.len()).unwrap_or(u32::MAX);
  out.extend_from_slice(&dims.to_le_bytes());
  for component in coordinate
    .vec
    .iter()
    .take(usize::try_from(dims).unwrap_or(usize::MAX))
  {
    out.extend_from_slice(&component.to_bits().to_le_bytes());
  }
  out.extend_from_slice(&coordinate.height.to_bits().to_le_bytes());
  out.extend_from_slice(&coordinate.adjustment.to_bits().to_le_bytes());
  out.extend_from_slice(&coordinate.error.to_bits().to_le_bytes());
}

/// Decodes a network coordinate from `bytes`, which must be exactly the coordinate — the dimension count
/// is bounded before allocating, and the byte length must match the declared dimensions plus the three
/// scalars.
fn decode_coordinate(bytes: &[u8]) -> Result<NetworkCoordinate, SwimWireError> {
  if bytes.len() < GOSSIP_COUNT_BYTES {
    return Err(SwimWireError::Truncated);
  }
  let (dims_bytes, rest) = bytes.split_at(GOSSIP_COUNT_BYTES);
  let mut dims_word = [0u8; GOSSIP_COUNT_BYTES];
  dims_word.copy_from_slice(dims_bytes);
  let dims = usize::try_from(u32::from_le_bytes(dims_word)).unwrap_or(usize::MAX);
  if dims > MAX_COORDINATE_DIMS {
    return Err(SwimWireError::MalformedCoordinate);
  }
  let wanted = dims
    .checked_add(COORDINATE_SCALARS)
    .and_then(|scalars| scalars.checked_mul(F64_BYTES))
    .ok_or(SwimWireError::MalformedCoordinate)?;
  if rest.len() != wanted {
    return Err(SwimWireError::MalformedCoordinate);
  }
  let mut cursor = rest;
  let mut vec = Vec::with_capacity(dims);
  for _ in 0..dims {
    let (component, tail) = take_f64(cursor).ok_or(SwimWireError::MalformedCoordinate)?;
    vec.push(component);
    cursor = tail;
  }
  let (height, cursor) = take_f64(cursor).ok_or(SwimWireError::MalformedCoordinate)?;
  let (adjustment, cursor) = take_f64(cursor).ok_or(SwimWireError::MalformedCoordinate)?;
  let (error, _) = take_f64(cursor).ok_or(SwimWireError::MalformedCoordinate)?;
  Ok(NetworkCoordinate {
    vec,
    height,
    adjustment,
    error,
  })
}

/// Reads a little-endian `f64` (its bit pattern) from the front of `bytes`, returning it and the
/// remainder, or `None` if fewer than eight bytes remain.
fn take_f64(bytes: &[u8]) -> Option<(f64, &[u8])> {
  if bytes.len() < F64_BYTES {
    return None;
  }
  let (head, rest) = bytes.split_at(F64_BYTES);
  let mut word = [0u8; F64_BYTES];
  word.copy_from_slice(head);
  Some((f64::from_bits(u64::from_le_bytes(word)), rest))
}

/// The wire byte for a liveness.
fn liveness_byte(liveness: Liveness) -> u8 {
  match liveness {
    Liveness::Alive => LIVENESS_ALIVE,
    Liveness::Suspect => LIVENESS_SUSPECT,
    Liveness::Dead => LIVENESS_DEAD,
  }
}

/// The liveness for a wire byte, or `UnknownLiveness` for a foreign value.
fn liveness_from_byte(byte: u8) -> Result<Liveness, SwimWireError> {
  match byte {
    LIVENESS_ALIVE => Ok(Liveness::Alive),
    LIVENESS_SUSPECT => Ok(Liveness::Suspect),
    LIVENESS_DEAD => Ok(Liveness::Dead),
    other => Err(SwimWireError::UnknownLiveness { liveness: other }),
  }
}

/// Format: a SWIM probe rides one stream per peer connection; the target's `serve_once` accepts whichever
/// stream arrives, so the exact id is a fixed label, not a tunable.
const PROBE_STREAM: u64 = 1;

/// The outcome of one live probe over the transport.
pub enum ProbeOutcome {
  /// The target acknowledged within the deadline. The piggybacked gossip is returned for the caller to
  /// fold into the view (`detector.on_ack` and `detector.apply_gossip`), and the measured round-trip time
  /// (`rtt_ns`) for it to feed the Vivaldi coordinate (`detector.observe_rtt`) so per-peer RTT prediction
  /// learns from real samples.
  Acked {
    /// The member id the acknowledging node **announced** as its own (`Ack.from`). On a mutually-TLS
    /// authenticated session this rides back from the node the caller probed; the caller compares it to the
    /// id it probed and, if they differ, the node has restarted under a new ephemeral member id (a different
    /// boot nonce) — the caller stops crediting the old id (it ages out) and the node's new id is learned
    /// from its own probes (§4.8 "Recovery"; task #22 learn-on-contact).
    from: HostId,
    /// The daemon boot_nonce the acknowledging node announced with `from` — what the caller validates that
    /// id against (`from` must be `member_id(anchor, boot_nonce)` for the certificate the session
    /// authenticated), with no numeric age ordering between boot nonces.
    boot_nonce: u64,
    /// The membership updates the acknowledgement carried.
    gossip: Vec<(HostId, MemberState)>,
    /// The measured round-trip time of this probe, in nanoseconds (the shard clock).
    rtt_ns: u64,
    /// The target's network coordinate, carried on the acknowledgement, for the caller to learn
    /// (`detector.learn_coordinate`) so it can predict the RTT to the target thereafter.
    coordinate: NetworkCoordinate,
  },
  /// The session cannot carry another exchange: the transport reported a terminal fault — the
  /// demultiplexer closed the session (`EndpointError::Closed`: the peer established a new one, or this
  /// end retired it) or the socket refused (`EndpointError::Io`). The endpoint is **released** (the
  /// caller's session is `None` after this) so the next period dials afresh — re-resolving the peer's
  /// address — instead of re-probing a dead session every period as a miss until the suspicion window
  /// retires a peer that may be live. Counted as a miss for the detector all the same: no acknowledgement
  /// came. (Ada's rejoin design, item 2, 2026-09-14.)
  Broken,
  /// The deadline elapsed with no acknowledgement — a probe failure (the target may be down, or a packet
  /// lost). The caller does not acknowledge; the detector's next tick suspects, and the indirect probe or
  /// a later period clears or confirms it.
  TimedOut,
}

/// Sends one SWIM `probe` over `endpoint` and awaits an acknowledgement that **echoes the probe's nonce**
/// within `budget.deadline_ns`, driving the request/reply inline and racing it against a deadline so a dead
/// target cannot hang the prober (§4.8; the same bounded-wait discipline as the commit dispatch — a probe
/// never blocks a protocol period forever). The **endpoint is returned for reuse whatever the outcome** —
/// acknowledged *or* timed out — so its packet-number space stays continuous across probe periods
/// (RFC 9000 §12.3) and, crucially, a single missed probe does not drop the session. The budget is the
/// caller's to derive from the peer's measured round trip (`slates_server::fleet::ProbeTiming`).
///
/// **Correctness (the nonce).** The reply counts only if it echoes this probe's nonce, so a *stale*
/// acknowledgement — one the peer sent to an earlier probe, that a real datagram socket buffered and the
/// reliable transport redelivered — does not satisfy the probe; a dead peer times out rather than looking
/// alive forever (`docs/bugs/2026-09-10-swim-stale-ack.md`). Every probe rides its own **fresh** stream
/// (RFC 9000 §2.1, `Endpoint::request`), so the late reply to a probe abandoned at its deadline arrives on
/// that older stream and is discarded by the transport below the current exchange's floor — it is never
/// read as this probe's reply, and this probe is never deduplicated at the peer against the abandoned one
/// (`docs/bugs/2026-09-13-reused-stream-id-collides-behind-an-unacked-reply.md`). A reply that *does*
/// arrive on this probe's stream with another nonce is therefore the peer answering the wrong probe — a
/// failure, as it was before.
///
/// **Robustness (keeping the session).** A timeout is a *transient miss*, not a verdict: a lost packet or a
/// moment's scheduling jitter produces one. Dropping the session on one
/// miss (a fresh handshake to replace it, and a verdict taken from one packet) would
/// retire a peer on any transient glitch — which is what made both survivors retire a *live* peer during
/// formation. So the session is kept and the caller re-probes it: the ping carries this node's suspicion,
/// the still-live peer refutes it (SWIM's incarnation refutation, [`crate::detector::Detector`]), and the
/// acknowledgement's gossip clears the suspicion — while a genuinely dead peer, never refuting, still ages
/// to death across the suspicion window. One missed probe never retires anyone; sustained silence does.
pub async fn probe_once(
  mut endpoint: Endpoint,
  probe: &SwimMessage,
  budget: CommitBudget,
) -> Result<(Option<Endpoint>, ProbeOutcome), RtError> {
  let bytes = probe.encode();
  let expected = probe.nonce();
  let started_ns = now_ns();
  let received = race_reply(&mut endpoint, &bytes, budget).await;

  // A reply counts only if it decodes as an acknowledgement echoing this probe's nonce; a wrong-nonce
  // reply, a protocol fault on one packet, or the deadline (`None`) is a probe failure keeping the
  // endpoint; a terminal transport fault releases it (`Broken`).
  let outcome = match received {
    Some(Ok(reply)) => match SwimMessage::decode(&reply) {
      Ok(SwimMessage::Ack {
        from,
        nonce,
        boot_nonce,
        gossip,
        coordinate,
      }) if Some(nonce) == expected => ProbeOutcome::Acked {
        from,
        boot_nonce,
        gossip,
        rtt_ns: now_ns().saturating_sub(started_ns),
        coordinate,
      },
      _ => ProbeOutcome::TimedOut,
    },
    Some(Err(error)) => outcome_of_request_error(&error),
    None => ProbeOutcome::TimedOut,
  };
  if matches!(outcome, ProbeOutcome::Broken) {
    return Ok((None, outcome));
  }
  Ok((Some(endpoint), outcome))
}

/// One request on the probe stream raced against the caller's deadline: `Some(result)` when the exchange
/// completed (a reply, or a transport error), `None` when the deadline won. Driven inline; on the deadline
/// the request future is dropped (releasing the borrow of `endpoint`) and the exchange is **abandoned** on
/// the still-owned endpoint, so nothing of it rides the next exchange's flush and its late reply, if any,
/// is discarded below the next exchange's floor — the reliable exchange is not self-bounded, so this
/// deadline is the caller-owned bound it relies on. A delivered reply is preferred over the deadline when
/// both are ready, so an exchange that just made it is not traded for a timeout. Shared by the direct probe
/// ([`probe_once`]) and the indirect-probe traffic ([`deliver_once`]).
async fn race_reply(
  endpoint: &mut Endpoint,
  bytes: &[u8],
  budget: CommitBudget,
) -> Option<Result<Vec<u8>, EndpointError>> {
  let received = {
    let mut request = std::pin::pin!(endpoint.request(PROBE_STREAM, bytes));
    let mut deadline = std::pin::pin!(sleep(budget.deadline_ns));
    std::future::poll_fn(|cx| {
      if let std::task::Poll::Ready(result) = std::future::Future::poll(request.as_mut(), cx) {
        return std::task::Poll::Ready(Some(result));
      }
      if std::future::Future::poll(deadline.as_mut(), cx).is_ready() {
        return std::task::Poll::Ready(None);
      }
      std::task::Poll::Pending
    })
    .await
  };
  if received.is_none() {
    endpoint.abandon_exchange();
  }
  received
}

/// How one indirect-probe message fared over a probe session ([`deliver_once`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
  /// The peer received it and answered the exchange (with anything: the message needs no reply body).
  Delivered,
  /// The exchange did not complete within the budget, or one bad packet ended it; the session is kept.
  Undelivered,
  /// The session cannot carry another exchange; released.
  Broken,
}

/// Sends one indirect-probe message — a [`SwimMessage::PingReq`] to a relay, or a
/// [`SwimMessage::IndirectAck`] back to the requester — over an existing probe `endpoint`, **bounded** by the
/// caller's `budget` exactly as a direct probe is ([`probe_once`]; the same race, the same abandonment on
/// the deadline), returning the endpoint for reuse whatever the outcome short of a terminal fault. The
/// message needs no reply body (the peer's serve side records it and answers the exchange with whatever it
/// answers — an acknowledgement or nothing), so any completed exchange is [`Delivery::Delivered`]; the
/// peer's *answer* to what was asked travels back on its own probe session, not on this exchange.
pub async fn deliver_once(
  mut endpoint: Endpoint,
  message: &SwimMessage,
  budget: CommitBudget,
) -> (Option<Endpoint>, Delivery) {
  let bytes = message.encode();
  match race_reply(&mut endpoint, &bytes, budget).await {
    Some(Ok(_)) => (Some(endpoint), Delivery::Delivered),
    Some(Err(error)) => match outcome_of_request_error(&error) {
      ProbeOutcome::Broken => (None, Delivery::Broken),
      _ => (Some(endpoint), Delivery::Undelivered),
    },
    None => (Some(endpoint), Delivery::Undelivered),
  }
}

/// How a request error on the probe session is judged: a fault that ends the session for good —
/// closed by the demultiplexer, or a socket refusal — is [`ProbeOutcome::Broken`]; every other error
/// (a packet that did not unprotect or decode, a flight the handshake layer refused) is one bad packet
/// on a session that may still carry the next probe, a miss. Pure, so it is tested by itself.
pub fn outcome_of_request_error(error: &EndpointError) -> ProbeOutcome {
  match error {
    EndpointError::Closed | EndpointError::Io(_) | EndpointError::Admission(_) => {
      ProbeOutcome::Broken
    }
    EndpointError::Handshake(_)
    | EndpointError::Tls(_)
    | EndpointError::NotReady
    | EndpointError::Header
    | EndpointError::Frames(_)
    | EndpointError::FlightTooLarge { .. } => ProbeOutcome::TimedOut,
  }
}

/// Serves one SWIM probe on a node (§4.8): receives a peer's message over `endpoint`, folds its
/// piggybacked gossip into `detector`, and replies with an acknowledgement carrying up to `gossip_fanout`
/// of this node's own gossip — so a probe both proves this node alive and spreads the view — and this
/// node's daemon `local_boot_nonce`, the announcement the prober validates `local` against (task #22). A
/// malformed message is answered with no reply (the prober counts nothing). The caller loops this to keep
/// serving.
pub async fn serve_probe(
  endpoint: &mut Endpoint,
  detector: &mut Detector,
  local: HostId,
  local_boot_nonce: u64,
  gossip_fanout: usize,
) -> Result<(), EndpointError> {
  endpoint
    .serve_once(|_, request| match SwimMessage::decode(&request) {
      Ok(message) => {
        // Fold in the sender's gossip, counting the sender as a confirmer of any suspicion it carries;
        // learn its coordinate too, if it carried one.
        detector.apply_gossip_from(message.from(), message.gossip());
        if let Some(coordinate) = message.coordinate() {
          detector.learn_coordinate(message.from(), coordinate.clone());
        }
        let gossip = detector.gossip(gossip_fanout);
        SwimMessage::Ack {
          from: local,
          // Echo the probe's nonce so the prober can tell this acknowledgement answers its current ping; a
          // message that carried none (not a ping) echoes zero, which a real probe's non-zero nonce rejects.
          nonce: message.nonce().unwrap_or(0),
          boot_nonce: local_boot_nonce,
          gossip,
          coordinate: detector.coordinate(),
        }
        .encode()
      }
      Err(_) => Vec::new(),
    })
    .await
}

#[cfg(test)]
mod tests {
  use super::*;

  const A: HostId = HostId(2);
  const B: HostId = HostId(3);

  fn sample_gossip() -> Vec<(HostId, MemberState)> {
    vec![
      (
        A,
        MemberState {
          liveness: Liveness::Suspect,
          incarnation: 7,
        },
      ),
      (
        B,
        MemberState {
          liveness: Liveness::Dead,
          incarnation: 4,
        },
      ),
    ]
  }

  fn sample_coordinate() -> NetworkCoordinate {
    NetworkCoordinate {
      vec: vec![1.5, -2.0, 0.0],
      height: 3.25,
      adjustment: -0.5,
      error: 0.75,
    }
  }

  /// An acknowledgement's coordinate round-trips, and a coordinate declaring more dimensions than the
  /// decoder accepts is refused before allocating.
  #[test]
  fn a_coordinate_round_trips_and_a_huge_one_is_refused() {
    let ack = SwimMessage::Ack {
      from: A,
      nonce: 42,
      boot_nonce: 1,
      gossip: sample_gossip(),
      coordinate: sample_coordinate(),
    };
    assert_eq!(
      SwimMessage::decode(&ack.encode()),
      Ok(ack),
      "the coordinate round-trips"
    );

    // An Ack whose coordinate claims a vast dimension count with no bytes to back it is refused.
    let mut hostile = vec![TAG_ACK];
    hostile.extend_from_slice(&7u64.to_le_bytes()); // from
    hostile.extend_from_slice(&0u64.to_le_bytes()); // nonce
    hostile.extend_from_slice(&0u64.to_le_bytes()); // boot_nonce
    hostile.extend_from_slice(&0u32.to_le_bytes()); // empty gossip
    hostile.extend_from_slice(&u32::MAX.to_le_bytes()); // coordinate dims = huge
    assert_eq!(
      SwimMessage::decode(&hostile),
      Err(SwimWireError::MalformedCoordinate),
      "an over-large coordinate is refused before allocating"
    );
  }

  /// Every message kind round-trips through encode/decode unchanged, gossip included.
  #[test]
  fn every_message_round_trips() {
    let messages = [
      SwimMessage::Ping {
        from: A,
        nonce: 1,
        boot_nonce: 3,
        gossip: sample_gossip(),
      },
      SwimMessage::Ack {
        from: B,
        nonce: u64::MAX,
        boot_nonce: u64::MAX,
        gossip: Vec::new(),
        coordinate: sample_coordinate(),
      },
      SwimMessage::PingReq {
        from: A,
        target: B,
        nonce: 9,
        gossip: sample_gossip(),
      },
      SwimMessage::IndirectAck {
        from: B,
        target: A,
        nonce: 9,
        boot_nonce: 11,
        gossip: sample_gossip(),
      },
    ];
    for message in messages {
      let bytes = message.encode();
      assert_eq!(
        SwimMessage::decode(&bytes),
        Ok(message),
        "round-trip is identity"
      );
    }
  }

  /// The indirect-acknowledgement encoding is fixed and little-endian — a golden vector pins it (a relay,
  /// host 4 at daemon boot_nonce 7, reporting it reached host 3 for the requester's probe nonce 5, with no
  /// gossip), so a drift in the tag or field order is caught.
  #[test]
  fn indirect_ack_has_a_golden_encoding() {
    let message = SwimMessage::IndirectAck {
      from: HostId(4),
      target: HostId(3),
      nonce: 5,
      boot_nonce: 7,
      gossip: Vec::new(),
    };
    let mut expected = vec![TAG_INDIRECT_ACK];
    expected.extend_from_slice(&4u64.to_le_bytes()); // from = 4
    expected.extend_from_slice(&3u64.to_le_bytes()); // target = 3
    expected.extend_from_slice(&5u64.to_le_bytes()); // nonce = 5
    expected.extend_from_slice(&7u64.to_le_bytes()); // boot_nonce = 7
    expected.extend_from_slice(&0u32.to_le_bytes()); // gossip count = 0
    assert_eq!(message.encode(), expected, "the byte layout is fixed");
    assert_eq!(
      SwimMessage::decode(&expected),
      Ok(message),
      "and decodes back"
    );
  }

  /// A ping-request and an indirect acknowledgement cut short anywhere inside their fixed header — after
  /// the sender, after the target, inside the nonce — are each refused `Truncated`, never read past the
  /// bytes that arrived.
  #[test]
  fn a_truncated_indirect_message_is_refused() {
    let full = SwimMessage::IndirectAck {
      from: HostId(4),
      target: HostId(3),
      nonce: 5,
      boot_nonce: 7,
      gossip: Vec::new(),
    }
    .encode();
    let request = SwimMessage::PingReq {
      from: HostId(4),
      target: HostId(3),
      nonce: 5,
      gossip: Vec::new(),
    }
    .encode();
    for bytes in [&full, &request] {
      // Every prefix short of the gossip count is a truncated header; the count itself is checked by the
      // gossip decoder.
      for cut in 1..(bytes.len() - GOSSIP_COUNT_BYTES) {
        assert_eq!(
          SwimMessage::decode(&bytes[..cut]),
          Err(SwimWireError::Truncated),
          "a message cut at byte {cut} of {} is refused, not read past its end",
          bytes.len()
        );
      }
    }
  }

  /// The encoding is fixed and little-endian — a golden vector pins it so a drift is caught (a Ping from
  /// host 2 at daemon boot_nonce 7, carrying one gossip entry: host 3, Suspect, incarnation 1).
  #[test]
  fn ping_has_a_golden_encoding() {
    let message = SwimMessage::Ping {
      from: HostId(2),
      nonce: 5,
      boot_nonce: 7,
      gossip: vec![(
        HostId(3),
        MemberState {
          liveness: Liveness::Suspect,
          incarnation: 1,
        },
      )],
    };
    let expected = [
      TAG_PING, // tag
      2,
      0,
      0,
      0,
      0,
      0,
      0,
      0, // from = 2
      5,
      0,
      0,
      0,
      0,
      0,
      0,
      0, // nonce = 5
      7,
      0,
      0,
      0,
      0,
      0,
      0,
      0, // boot_nonce = 7
      1,
      0,
      0,
      0, // gossip count = 1
      3,
      0,
      0,
      0,
      0,
      0,
      0,
      0,                // subject = 3
      LIVENESS_SUSPECT, // liveness
      1,
      0,
      0,
      0,
      0,
      0,
      0,
      0, // incarnation = 1
    ];
    assert_eq!(message.encode(), expected, "the byte layout is fixed");
  }

  /// An empty input, a truncated header and a truncated gossip count are each refused, not panicked.
  #[test]
  fn truncated_input_is_refused() {
    assert_eq!(SwimMessage::decode(&[]), Err(SwimWireError::Truncated));
    assert_eq!(
      SwimMessage::decode(&[TAG_PING, 1, 2, 3]),
      Err(SwimWireError::Truncated),
      "header cut"
    );
    // Tag + full from, but the nonce word is missing.
    assert_eq!(
      SwimMessage::decode(&[TAG_PING, 0, 0, 0, 0, 0, 0, 0, 0]),
      Err(SwimWireError::Truncated),
      "nonce cut"
    );
    // Tag + full from + full nonce, but the boot_nonce word is missing.
    let mut nonce_ok = vec![TAG_PING];
    nonce_ok.extend_from_slice(&2u64.to_le_bytes()); // from
    nonce_ok.extend_from_slice(&9u64.to_le_bytes()); // nonce
    assert_eq!(
      SwimMessage::decode(&nonce_ok),
      Err(SwimWireError::Truncated),
      "boot_nonce cut"
    );
    // Tag + from + nonce + boot_nonce, but the gossip count word is missing.
    let mut boot_nonce_ok = nonce_ok.clone();
    boot_nonce_ok.extend_from_slice(&1u64.to_le_bytes()); // boot_nonce
    assert_eq!(
      SwimMessage::decode(&boot_nonce_ok),
      Err(SwimWireError::Truncated),
      "gossip count cut"
    );
  }

  /// An unknown tag and an unknown liveness byte are refused with their offending value.
  #[test]
  fn foreign_bytes_are_refused() {
    assert_eq!(
      SwimMessage::decode(&[0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
      Err(SwimWireError::UnknownTag { tag: 0xFF })
    );
    // A Ping with one gossip entry whose liveness byte is foreign (5).
    let bytes = [
      TAG_PING, 2, 0, 0, 0, 0, 0, 0, 0, // from
      0, 0, 0, 0, 0, 0, 0, 0, // nonce
      0, 0, 0, 0, 0, 0, 0, 0, // boot_nonce
      1, 0, 0, 0, // count = 1
      3, 0, 0, 0, 0, 0, 0, 0, // subject
      5, // foreign liveness
      1, 0, 0, 0, 0, 0, 0, 0, // incarnation
    ];
    assert_eq!(
      SwimMessage::decode(&bytes),
      Err(SwimWireError::UnknownLiveness { liveness: 5 })
    );
  }

  /// A gossip count that does not match the bytes that arrived is refused before allocating — a datagram
  /// claiming a huge batch it did not carry cannot force an over-allocation.
  #[test]
  fn a_lying_gossip_count_is_refused() {
    // Claim u32::MAX entries with no entry bytes at all.
    let mut bytes = vec![TAG_ACK];
    bytes.extend_from_slice(&7u64.to_le_bytes()); // from
    bytes.extend_from_slice(&0u64.to_le_bytes()); // nonce
    bytes.extend_from_slice(&0u64.to_le_bytes()); // boot_nonce
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // count = huge
    assert_eq!(
      SwimMessage::decode(&bytes),
      Err(SwimWireError::GossipLengthMismatch),
      "a count without the bytes to back it is refused"
    );
    // Claim one entry but supply only part of it.
    let mut short = vec![TAG_ACK];
    short.extend_from_slice(&7u64.to_le_bytes());
    short.extend_from_slice(&0u64.to_le_bytes()); // nonce
    short.extend_from_slice(&0u64.to_le_bytes()); // boot_nonce
    short.extend_from_slice(&1u32.to_le_bytes());
    short.extend_from_slice(&[9, 9, 9]); // a partial entry
    assert_eq!(
      SwimMessage::decode(&short),
      Err(SwimWireError::GossipLengthMismatch)
    );
  }

  /// A terminal transport fault — the demultiplexer closed the session, or the socket refused — is
  /// `Broken` (the session is released); a fault on one packet is a miss on a session kept for the next
  /// probe, as the deadline is.
  #[test]
  fn a_closed_session_or_a_socket_refusal_is_broken_and_one_bad_packet_is_a_miss() {
    assert!(matches!(
      outcome_of_request_error(&EndpointError::Closed),
      ProbeOutcome::Broken
    ));
    assert!(matches!(
      outcome_of_request_error(&EndpointError::Io(slates_rt::error::RtError::DriverLost)),
      ProbeOutcome::Broken
    ));
    for one_packet in [
      EndpointError::NotReady,
      EndpointError::Header,
      EndpointError::FlightTooLarge { bytes: 1, cap: 0 },
    ] {
      assert!(
        matches!(
          outcome_of_request_error(&one_packet),
          ProbeOutcome::TimedOut
        ),
        "{one_packet:?} keeps the session"
      );
    }
  }
}
