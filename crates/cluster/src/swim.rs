//! The SWIM wire (§4.8, §4.10a) — the on-the-wire encoding of the failure detector's messages so a
//! [`Detector`](crate::detector::Detector)'s [`Ping`](crate::detector::Ping),
//! [`Ack`](crate::detector::Ack) and [`PingReq`](crate::detector::PingReq) can ride the fleet
//! transport, each piggybacking a bounded batch of gossiped membership updates (SWIM's infection-style
//! dissemination shares the probe traffic). The live driver that sends these over authenticated
//! sessions and feeds replies back into the detector is composed on top; this module is the pure codec.
//!
//! Every decode is a parser of external bytes, so it checks the length against the message's shape
//! before allocating and rejects a truncated header, an unknown tag, an unknown liveness byte, or a
//! gossip count that does not match the bytes that arrived — a hostile datagram is a typed
//! [`SwimWireError`], never a panic or an over-allocation. The encoding is little-endian throughout so
//! two hosts encode a message identically (the determinism the golden vectors pin).

use std::mem::size_of;

use slates_db::register::HostId;

use crate::membership::{Liveness, MemberState};

/// A SWIM message on the wire: a probe, its acknowledgement, or an indirect-probe request, each naming
/// the sender and carrying a piggybacked batch of membership updates to gossip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwimMessage {
  /// A direct probe from `from`.
  Ping {
    /// The probing node.
    from: HostId,
    /// The membership updates piggybacked on this probe.
    gossip: Vec<(HostId, MemberState)>,
  },
  /// An acknowledgement from `from` (the reply to a [`SwimMessage::Ping`] or an indirect probe).
  Ack {
    /// The acknowledging node.
    from: HostId,
    /// The membership updates piggybacked on this acknowledgement.
    gossip: Vec<(HostId, MemberState)>,
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
      SwimMessage::Ack { from, gossip } => {
        out.push(TAG_ACK);
        out.extend_from_slice(&from.0.to_le_bytes());
        encode_gossip(&mut out, gossip);
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
        let gossip = decode_gossip(rest)?;
        Ok(SwimMessage::Ping { from, gossip })
      }
      TAG_ACK => {
        let (from, rest) = take_host(rest)?;
        let gossip = decode_gossip(rest)?;
        Ok(SwimMessage::Ack { from, gossip })
      }
      TAG_PING_REQ => {
        let (from, rest) = take_host(rest)?;
        let (target, rest) = take_host(rest)?;
        let gossip = decode_gossip(rest)?;
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

/// Decodes a gossip batch from the remaining bytes: the u32 count, then exactly `count` entries. The
/// count is checked against the bytes that actually arrived before anything is allocated, so a datagram
/// claiming a huge count without the bytes to back it is refused rather than over-allocating.
fn decode_gossip(bytes: &[u8]) -> Result<Vec<(HostId, MemberState)>, SwimWireError> {
  if bytes.len() < GOSSIP_COUNT_BYTES {
    return Err(SwimWireError::Truncated);
  }
  let (count_bytes, mut rest) = bytes.split_at(GOSSIP_COUNT_BYTES);
  let mut count_word = [0u8; GOSSIP_COUNT_BYTES];
  count_word.copy_from_slice(count_bytes);
  let count = usize::try_from(u32::from_le_bytes(count_word)).unwrap_or(usize::MAX);

  // The remaining bytes must be exactly the entries the count declares — no more, no less.
  let wanted = count
    .checked_mul(GOSSIP_ENTRY_BYTES)
    .ok_or(SwimWireError::GossipLengthMismatch)?;
  if rest.len() != wanted {
    return Err(SwimWireError::GossipLengthMismatch);
  }

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
  Ok(gossip)
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
