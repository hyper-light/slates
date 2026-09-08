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
use std::sync::mpsc::{TryRecvError, channel};

use slates_db::register::HostId;
use slates_rt::error::RtError;
use slates_rt::futures::{cancel, now_ns, sleep, spawn_child};
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
    /// The membership updates piggybacked on this probe.
    gossip: Vec<(HostId, MemberState)>,
  },
  /// An acknowledgement from `from` (the reply to a [`SwimMessage::Ping`] or an indirect probe), carrying
  /// the sender's network coordinate so the prober learns it (and can predict the RTT to it).
  Ack {
    /// The acknowledging node.
    from: HostId,
    /// The membership updates piggybacked on this acknowledgement.
    gossip: Vec<(HostId, MemberState)>,
    /// The acknowledging node's Vivaldi coordinate.
    coordinate: NetworkCoordinate,
  },
  /// A request from `from` to probe `target` on its behalf (the indirect probe).
  PingReq {
    /// The requesting node.
    from: HostId,
    /// The node to probe indirectly.
    target: HostId,
    /// The membership updates piggybacked on this request.
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
      | SwimMessage::PingReq { from, .. } => *from,
    }
  }

  /// The piggybacked gossip batch.
  pub fn gossip(&self) -> &[(HostId, MemberState)] {
    match self {
      SwimMessage::Ping { gossip, .. }
      | SwimMessage::Ack { gossip, .. }
      | SwimMessage::PingReq { gossip, .. } => gossip,
    }
  }

  /// The sender's network coordinate, when the message is an acknowledgement (which carries it).
  pub fn coordinate(&self) -> Option<&NetworkCoordinate> {
    match self {
      SwimMessage::Ack { coordinate, .. } => Some(coordinate),
      SwimMessage::Ping { .. } | SwimMessage::PingReq { .. } => None,
    }
  }

  /// The canonical little-endian bytes: the tag, the sender, the target (for a ping-request only), then
  /// the gossip batch (its u32 count and each entry). Two hosts encode a message identically.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      SwimMessage::Ping { from, gossip } => {
        out.push(TAG_PING);
        out.extend_from_slice(&from.0.to_le_bytes());
        encode_gossip(&mut out, gossip);
      }
      SwimMessage::Ack {
        from,
        gossip,
        coordinate,
      } => {
        out.push(TAG_ACK);
        out.extend_from_slice(&from.0.to_le_bytes());
        encode_gossip(&mut out, gossip);
        encode_coordinate(&mut out, coordinate);
      }
      SwimMessage::PingReq {
        from,
        target,
        gossip,
      } => {
        out.push(TAG_PING_REQ);
        out.extend_from_slice(&from.0.to_le_bytes());
        out.extend_from_slice(&target.0.to_le_bytes());
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
        let (gossip, leftover) = decode_gossip(rest)?;
        if !leftover.is_empty() {
          return Err(SwimWireError::GossipLengthMismatch);
        }
        Ok(SwimMessage::Ping { from, gossip })
      }
      TAG_ACK => {
        let (from, rest) = take_host(rest)?;
        let (gossip, leftover) = decode_gossip(rest)?;
        let coordinate = decode_coordinate(leftover)?;
        Ok(SwimMessage::Ack {
          from,
          gossip,
          coordinate,
        })
      }
      TAG_PING_REQ => {
        let (from, rest) = take_host(rest)?;
        let (target, rest) = take_host(rest)?;
        let (gossip, leftover) = decode_gossip(rest)?;
        if !leftover.is_empty() {
          return Err(SwimWireError::GossipLengthMismatch);
        }
        Ok(SwimMessage::PingReq {
          from,
          target,
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
  if bytes.len() < size_of::<u64>() {
    return Err(SwimWireError::Truncated);
  }
  let (head, rest) = bytes.split_at(size_of::<u64>());
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Ok((HostId(u64::from_le_bytes(word)), rest))
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
    /// The membership updates the acknowledgement carried.
    gossip: Vec<(HostId, MemberState)>,
    /// The measured round-trip time of this probe, in nanoseconds (the shard clock).
    rtt_ns: u64,
    /// The target's network coordinate, carried on the acknowledgement, for the caller to learn
    /// (`detector.learn_coordinate`) so it can predict the RTT to the target thereafter.
    coordinate: NetworkCoordinate,
  },
  /// The deadline elapsed with no acknowledgement — a probe failure (the target may be down, or a packet
  /// lost). The caller does not acknowledge; the detector's next tick suspects, and the indirect probe or
  /// a later period clears or confirms it.
  TimedOut,
}

/// What the probe's request task or the deadline task reports back.
enum ProbeReply {
  /// The target replied (bytes, empty if the request failed) and its endpoint, handed back for reuse.
  Replied(Vec<u8>, Box<Endpoint>),
  /// The deadline elapsed first.
  Deadline,
}

/// Sends one SWIM `probe` over `endpoint` and awaits the acknowledgement within `budget.deadline_ns`,
/// racing the request against a deadline task so a dead target cannot hang the prober (§4.8; the same
/// bounded-wait discipline as the commit dispatch — a probe never blocks a protocol period forever). On
/// an acknowledgement the endpoint is returned for reuse, so its packet-number space stays continuous
/// across probe periods (RFC 9000 §12.3), and the target's piggybacked gossip is delivered; on the
/// deadline the request task is cancelled and its endpoint dropped (a crashed peer's connection is
/// worthless), so a caller that retries reconnects. The budget is the caller's to derive (owed — a
/// measured RTT budget).
pub async fn probe_once(
  endpoint: Endpoint,
  probe: &SwimMessage,
  budget: CommitBudget,
) -> Result<(Option<Endpoint>, ProbeOutcome), RtError> {
  let bytes = probe.encode();
  let started_ns = now_ns();
  let (tx, rx) = channel::<ProbeReply>();

  let request_tx = tx.clone();
  let request = spawn_child(async move {
    let mut endpoint = endpoint;
    let reply = endpoint
      .request(PROBE_STREAM, &bytes)
      .await
      .unwrap_or_default();
    let _ = request_tx.send(ProbeReply::Replied(reply, Box::new(endpoint)));
  })?;

  let deadline = budget.deadline_ns;
  let deadline_task = spawn_child(async move {
    sleep(deadline).await;
    let _ = tx.send(ProbeReply::Deadline);
  })?;

  loop {
    match rx.try_recv() {
      Ok(ProbeReply::Replied(reply, endpoint)) => {
        let _ = cancel(deadline_task);
        // A missing or non-ack reply is not an acknowledgement — treat it as a probe failure.
        let outcome = match SwimMessage::decode(&reply) {
          Ok(SwimMessage::Ack {
            gossip, coordinate, ..
          }) => ProbeOutcome::Acked {
            gossip,
            rtt_ns: now_ns().saturating_sub(started_ns),
            coordinate,
          },
          _ => ProbeOutcome::TimedOut,
        };
        return Ok((Some(*endpoint), outcome));
      }
      Ok(ProbeReply::Deadline) => {
        let _ = cancel(request);
        return Ok((None, ProbeOutcome::TimedOut));
      }
      Err(TryRecvError::Empty) => sleep(budget.poll_interval_ns).await,
      Err(TryRecvError::Disconnected) => return Ok((None, ProbeOutcome::TimedOut)),
    }
  }
}

/// Serves one SWIM probe on a node (§4.8): receives a peer's message over `endpoint`, folds its
/// piggybacked gossip into `detector`, and replies with an acknowledgement carrying up to `gossip_fanout`
/// of this node's own gossip — so a probe both proves this node alive and spreads the view. A malformed
/// message is answered with no reply (the prober counts nothing). The caller loops this to keep serving.
pub async fn serve_probe(
  endpoint: &mut Endpoint,
  detector: &mut Detector,
  local: HostId,
  gossip_fanout: usize,
) -> Result<(), EndpointError> {
  endpoint
    .serve_once(|request| match SwimMessage::decode(&request) {
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
        gossip: sample_gossip(),
      },
      SwimMessage::Ack {
        from: B,
        gossip: Vec::new(),
        coordinate: sample_coordinate(),
      },
      SwimMessage::PingReq {
        from: A,
        target: B,
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

  /// The encoding is fixed and little-endian — a golden vector pins it so a drift is caught (a Ping from
  /// host 2 carrying one gossip entry: host 3, Suspect, incarnation 1).
  #[test]
  fn ping_has_a_golden_encoding() {
    let message = SwimMessage::Ping {
      from: HostId(2),
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
    // Tag + full from, but the gossip count word is missing.
    assert_eq!(
      SwimMessage::decode(&[TAG_PING, 0, 0, 0, 0, 0, 0, 0, 0]),
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
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // count = huge
    assert_eq!(
      SwimMessage::decode(&bytes),
      Err(SwimWireError::GossipLengthMismatch),
      "a count without the bytes to back it is refused"
    );
    // Claim one entry but supply only part of it.
    let mut short = vec![TAG_ACK];
    short.extend_from_slice(&7u64.to_le_bytes());
    short.extend_from_slice(&1u32.to_le_bytes());
    short.extend_from_slice(&[9, 9, 9]); // a partial entry
    assert_eq!(
      SwimMessage::decode(&short),
      Err(SwimWireError::GossipLengthMismatch)
    );
  }
}
