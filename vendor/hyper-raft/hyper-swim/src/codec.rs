//! The SWIM wire (§4.8, §4.10a) — the on-the-wire encoding of the failure detector's messages so a
//! [`Detector`](crate::detector::Detector)'s [`Ping`](crate::detector::Ping),
//! [`Ack`](crate::detector::Ack) and [`PingReq`](crate::detector::PingReq) can ride the fleet
//! transport, each piggybacking a bounded batch of gossiped membership updates (SWIM's infection-style
//! dissemination shares the probe traffic), and the chunks of a member's view its anti-entropy
//! exchanges ([`SwimMessage::Sync`]). The live driver that sends these over authenticated
//! sessions and feeds replies back into the detector is composed on top; this module is the pure codec.
//!
//! An acknowledgement also carries the sender's Vivaldi network coordinate ([`crate::coordinates`]), so
//! a prober learns the coordinate of every peer it probes and can predict the round-trip time to it —
//! the live half of coordinate-aware indirect probing.
//!
//! A message borrows what it carries: a sender's gossip entries and coordinate when it is encoded,
//! the received bytes when it is decoded. [`SwimMessage::encode_into`] writes into the caller's
//! buffer and [`SwimMessage::decode`] reads in place, so a protocol period allocates nothing once the
//! buffers have grown (`docs/benchmarks.md`, "hyper-swim").
//!
//! Every decode is a parser of external bytes, so it checks the whole message's shape before it
//! hands anything out and rejects a truncated header, an unknown tag, an unknown liveness byte, a gossip
//! count that does not match the bytes that arrived, or a coordinate of other dimensions than the
//! engine's — a hostile datagram is a typed [`SwimWireError`], never a panic or an over-allocation. The encoding is little-endian throughout (floats as their bit pattern) so two hosts
//! encode a message identically.

use std::mem::size_of;

use crate::HostId;
use crate::coordinates::{DIMENSIONS, NetworkCoordinate};
use crate::membership::{Liveness, MemberState};

/// A SWIM message on the wire: a probe, its acknowledgement, or an indirect-probe request, each naming
/// the sender and carrying a piggybacked batch of membership updates to gossip. (`Eq` is not derived
/// because an acknowledgement carries the sender's Vivaldi coordinate, which holds floating point.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SwimMessage<'a> {
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
        /// The newest regional configuration version the prober has installed or seen (§4.8 "Leases and
        /// reads"; AUD-08): a holder learns from it that a retired owner has seen its retirement, which opens
        /// the promotion of that owner's objects at once rather than after the membership horizon.
        configuration_version: u64,
        /// The membership updates piggybacked on this probe.
        gossip: GossipBatch<'a>,
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
        /// The version of the regional configuration the acknowledging node's council holds (§4.8 "Leases and
        /// reads"; AUD-08) — the configuration `standing` was read from, so the prober can tell a holder that has
        /// not admitted it yet (an older configuration) from one that has retired it (a newer one).
        configuration_version: u64,
        /// The acknowledging node's view of the prober's authority standing: the version its configuration
        /// fixed the prober's settled neighbourhood at, `None` when the prober is no member of it. The prober
        /// credits this answer toward its owner lease only when its own standing recognizes it — "a majority
        /// observation must belong to the relevant authority generation" (`slates_db::register::Standing`) — so
        /// another host's configuration change leaves the lease as it was.
        standing: Option<u64>,
        /// The membership updates piggybacked on this acknowledgement.
        gossip: GossipBatch<'a>,
        /// The acknowledging node's Vivaldi coordinate.
        coordinate: Coordinate<'a>,
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
        gossip: GossipBatch<'a>,
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
        gossip: GossipBatch<'a>,
    },
    /// Anti-entropy (Demers et al. 1987, §1.3 and §1.5): the digest of `from`'s view, and a chunk
    /// of the view itself, members it holds, alive, suspected or dead within their records'
    /// windows, each with its state, as gossip entries are. An exchange opens with the digest alone
    /// and a pull; a partner whose digest differs answers with its whole view, chunk by chunk, its
    /// first chunk asking a pull, and the opener answers that with its own
    /// ([`Detector::sync_into`](crate::detector::Detector::sync_into)).
    Sync {
        /// The member whose view this is.
        from: HostId,
        /// The sender's daemon boot_nonce, its identity announcement, as every message it originates
        /// makes.
        boot_nonce: u64,
        /// The digest of the sender's whole view when it sent this
        /// ([`Membership::digest`](crate::membership::Membership::digest)).
        digest: u64,
        /// Whether the sender asks the receiver's view in return.
        pull: bool,
        /// The members of the sender's view this chunk carries, in id order; none in an opening.
        gossip: GossipBatch<'a>,
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
    /// An acknowledgement's network coordinate was malformed: other dimensions than the engine's
    /// ([`DIMENSIONS`]), or a byte length that does not match them.
    MalformedCoordinate,
    /// An acknowledgement's standing presence byte is neither absent nor present.
    UnknownPresence {
        /// The presence byte that arrived.
        presence: u8,
    },
    /// A view chunk's pull byte is neither asked nor not.
    UnknownPull {
        /// The pull byte that arrived.
        pull: u8,
    },
}

/// Format: the message tag occupies one leading byte; a ping's.
const TAG_PING: u8 = 1;
/// Format: an acknowledgement's tag.
const TAG_ACK: u8 = 2;
/// Format: an indirect-probe request's tag.
const TAG_PING_REQ: u8 = 3;
/// Format: an indirect acknowledgement's tag.
const TAG_INDIRECT_ACK: u8 = 4;
/// Format: a view chunk's tag.
const TAG_SYNC: u8 = 5;

/// Format: a view chunk's pull is one byte; a chunk that asks no view in return.
const PULL_NONE: u8 = 0;
/// Format: the pull byte of a chunk that asks the receiver's view in return.
const PULL_ASKED: u8 = 1;

/// Format: an acknowledgement's standing is one presence byte, then the eight-byte version when present.
const STANDING_ABSENT: u8 = 0;
/// Format: the presence byte of a standing that follows.
const STANDING_PRESENT: u8 = 1;

/// Format: a liveness is one byte in a gossip entry; alive's.
const LIVENESS_ALIVE: u8 = 0;
/// Format: suspect's liveness byte.
const LIVENESS_SUSPECT: u8 = 1;
/// Format: dead's liveness byte.
const LIVENESS_DEAD: u8 = 2;

/// Format: one gossip entry is a host id (u64), a liveness byte, and an incarnation (u64), little-endian.
const GOSSIP_ENTRY_BYTES: usize = size_of::<u64>() + size_of::<u8>() + size_of::<u64>();
/// Format: the piggybacked batch is prefixed by its entry count as a u32.
const GOSSIP_COUNT_BYTES: usize = size_of::<u32>();

/// The gossip entries a message can carry in `room` bytes when its encoding with an empty batch is
/// `bare` bytes long: what is left, in whole entries.
pub fn gossip_capacity(room: usize, bare: usize) -> usize {
    room.saturating_sub(bare)
        .checked_div(GOSSIP_ENTRY_BYTES)
        .unwrap_or(0)
}

/// A piggybacked gossip batch: the entries a sender holds, or a received batch read in place.
#[derive(Clone, Copy, Debug)]
pub enum GossipBatch<'a> {
    /// The entries a sender piggybacks.
    Entries(&'a [(HostId, MemberState)]),
    /// A received batch, read in place.
    Wire(WireGossip<'a>),
}

impl<'a> GossipBatch<'a> {
    /// The number of entries.
    pub fn len(&self) -> usize {
        match self {
            GossipBatch::Entries(entries) => entries.len(),
            GossipBatch::Wire(wire) => wire.0.len(),
        }
    }

    /// Whether the batch carries nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entries, in the order they were sent.
    pub fn iter(&self) -> GossipEntries<'a> {
        match *self {
            GossipBatch::Entries(entries) => GossipEntries::Entries(entries.iter()),
            GossipBatch::Wire(wire) => GossipEntries::Wire(wire.0.iter()),
        }
    }
}

impl PartialEq for GossipBatch<'_> {
    /// Two batches are equal when they carry the same entries, held or received.
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl<'a> IntoIterator for GossipBatch<'a> {
    type Item = (HostId, MemberState);
    type IntoIter = GossipEntries<'a>;

    fn into_iter(self) -> GossipEntries<'a> {
        self.iter()
    }
}

/// The entries of a [`GossipBatch`].
#[derive(Clone, Debug)]
pub enum GossipEntries<'a> {
    /// Held entries.
    Entries(std::slice::Iter<'a, (HostId, MemberState)>),
    /// Received entries.
    Wire(std::slice::Iter<'a, [u8; GOSSIP_ENTRY_BYTES]>),
}

impl Iterator for GossipEntries<'_> {
    type Item = (HostId, MemberState);

    fn next(&mut self) -> Option<(HostId, MemberState)> {
        match self {
            GossipEntries::Entries(entries) => entries.next().copied(),
            // The decoder checked every entry, so a received one always reads.
            GossipEntries::Wire(chunks) => chunks.next().and_then(|entry| read_entry(entry).ok()),
        }
    }
}

/// A received gossip batch: whole entries whose liveness bytes [`SwimMessage::decode`] has checked,
/// which only it makes.
#[derive(Clone, Copy, Debug)]
pub struct WireGossip<'a>(&'a [[u8; GOSSIP_ENTRY_BYTES]]);

/// A received coordinate whose length [`SwimMessage::decode`] has checked against its dimensions,
/// which only it makes.
#[derive(Clone, Copy, Debug)]
pub struct WireCoordinate<'a>(&'a [u8]);

/// An acknowledgement's coordinate: the sender's own, or a received one read in place.
#[derive(Clone, Copy, Debug)]
pub enum Coordinate<'a> {
    /// The coordinate a sender holds.
    Held(&'a NetworkCoordinate),
    /// A received coordinate, read in place.
    Wire(WireCoordinate<'a>),
}

impl Coordinate<'_> {
    /// The coordinate as a value of its own. A received one's bits are as they arrived: whether
    /// it can be used is [`NetworkCoordinate::is_usable`]'s to say.
    pub fn to_coordinate(&self) -> NetworkCoordinate {
        match *self {
            Coordinate::Held(held) => *held,
            Coordinate::Wire(WireCoordinate(bytes)) => {
                // The decoder checked the count and the length, so every scalar reads.
                let mut rest = bytes.get(GOSSIP_COUNT_BYTES..).unwrap_or(&[]);
                let mut next = || {
                    let (value, tail) = take_f64(rest).unwrap_or((0.0, &[]));
                    rest = tail;
                    value
                };
                let mut position = [0.0; DIMENSIONS];
                for component in &mut position {
                    *component = next();
                }
                let height = next();
                let error = next();
                NetworkCoordinate {
                    position,
                    height,
                    error,
                }
            }
        }
    }
}

impl PartialEq for Coordinate<'_> {
    /// Two coordinates are equal when their values are, held or received.
    fn eq(&self, other: &Self) -> bool {
        self.to_coordinate() == other.to_coordinate()
    }
}

impl<'a> SwimMessage<'a> {
    /// The sender named in the message (`from`).
    pub fn from(&self) -> HostId {
        match self {
            SwimMessage::Ping { from, .. }
            | SwimMessage::Ack { from, .. }
            | SwimMessage::PingReq { from, .. }
            | SwimMessage::IndirectAck { from, .. }
            | SwimMessage::Sync { from, .. } => *from,
        }
    }

    /// The piggybacked gossip batch.
    pub fn gossip(&self) -> GossipBatch<'a> {
        match *self {
            SwimMessage::Ping { gossip, .. }
            | SwimMessage::Ack { gossip, .. }
            | SwimMessage::PingReq { gossip, .. }
            | SwimMessage::IndirectAck { gossip, .. }
            | SwimMessage::Sync { gossip, .. } => gossip,
        }
    }

    /// The sender's network coordinate, when the message is an acknowledgement (which carries it).
    pub fn coordinate(&self) -> Option<Coordinate<'a>> {
        match *self {
            SwimMessage::Ack { coordinate, .. } => Some(coordinate),
            SwimMessage::Ping { .. }
            | SwimMessage::PingReq { .. }
            | SwimMessage::IndirectAck { .. }
            | SwimMessage::Sync { .. } => None,
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
            SwimMessage::Sync { .. } => None,
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
            | SwimMessage::IndirectAck { boot_nonce, .. }
            | SwimMessage::Sync { boot_nonce, .. } => Some(*boot_nonce),
            SwimMessage::PingReq { .. } => None,
        }
    }

    /// The newest regional configuration version the sender announced: a [`Ping`](SwimMessage::Ping)'s or
    /// an [`Ack`](SwimMessage::Ack)'s (the owner-lease evidence, §4.8 "Leases and reads"); `None` for the
    /// indirect messages, which announce none.
    pub fn configuration_version(&self) -> Option<u64> {
        match self {
            SwimMessage::Ping {
                configuration_version,
                ..
            }
            | SwimMessage::Ack {
                configuration_version,
                ..
            } => Some(*configuration_version),
            SwimMessage::PingReq { .. }
            | SwimMessage::IndirectAck { .. }
            | SwimMessage::Sync { .. } => None,
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
            SwimMessage::Ping { .. } | SwimMessage::Ack { .. } | SwimMessage::Sync { .. } => None,
        }
    }

    /// Writes the canonical little-endian bytes into `out`, replacing what it held: the tag, the
    /// sender, the probe nonce and the sender's boot_nonce (a ping or an acknowledgement) or the target
    /// (a ping-request), then the gossip batch (its u32 count and each entry). Two hosts encode a
    /// message identically. `out` keeps its capacity, so a reused buffer stops allocating once it has
    /// grown to the largest message.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.clear();
        match self {
            SwimMessage::Ping {
                from,
                nonce,
                boot_nonce,
                configuration_version,
                gossip,
            } => {
                out.push(TAG_PING);
                out.extend_from_slice(&from.0.to_le_bytes());
                out.extend_from_slice(&nonce.to_le_bytes());
                out.extend_from_slice(&boot_nonce.to_le_bytes());
                out.extend_from_slice(&configuration_version.to_le_bytes());
                encode_gossip(out, *gossip);
            }
            SwimMessage::Ack {
                from,
                nonce,
                boot_nonce,
                configuration_version,
                standing,
                gossip,
                coordinate,
            } => {
                out.push(TAG_ACK);
                out.extend_from_slice(&from.0.to_le_bytes());
                out.extend_from_slice(&nonce.to_le_bytes());
                out.extend_from_slice(&boot_nonce.to_le_bytes());
                out.extend_from_slice(&configuration_version.to_le_bytes());
                match standing {
                    Some(generation) => {
                        out.push(STANDING_PRESENT);
                        out.extend_from_slice(&generation.to_le_bytes());
                    }
                    None => out.push(STANDING_ABSENT),
                }
                encode_gossip(out, *gossip);
                encode_coordinate(out, *coordinate);
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
                encode_gossip(out, *gossip);
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
                encode_gossip(out, *gossip);
            }
            SwimMessage::Sync {
                from,
                boot_nonce,
                digest,
                pull,
                gossip,
            } => {
                out.push(TAG_SYNC);
                out.extend_from_slice(&from.0.to_le_bytes());
                out.extend_from_slice(&boot_nonce.to_le_bytes());
                out.extend_from_slice(&digest.to_le_bytes());
                out.push(if *pull { PULL_ASKED } else { PULL_NONE });
                encode_gossip(out, *gossip);
            }
        }
    }

    /// Decodes a message from received bytes in place, or a typed refusal for a hostile or truncated
    /// datagram. The whole message is checked before it is returned.
    pub fn decode(bytes: &'a [u8]) -> Result<SwimMessage<'a>, SwimWireError> {
        let (&tag, rest) = bytes.split_first().ok_or(SwimWireError::Truncated)?;
        match tag {
            TAG_PING => {
                let (from, rest) = take_host(rest)?;
                let (nonce, rest) = take_word(rest)?;
                let (boot_nonce, rest) = take_word(rest)?;
                let (configuration_version, rest) = take_word(rest)?;
                let (gossip, leftover) = decode_gossip(rest)?;
                if !leftover.is_empty() {
                    return Err(SwimWireError::GossipLengthMismatch);
                }
                Ok(SwimMessage::Ping {
                    from,
                    nonce,
                    boot_nonce,
                    configuration_version,
                    gossip,
                })
            }
            TAG_ACK => {
                let (from, rest) = take_host(rest)?;
                let (nonce, rest) = take_word(rest)?;
                let (boot_nonce, rest) = take_word(rest)?;
                let (configuration_version, rest) = take_word(rest)?;
                let (standing, rest) = take_standing(rest)?;
                let (gossip, leftover) = decode_gossip(rest)?;
                let coordinate = decode_coordinate(leftover)?;
                Ok(SwimMessage::Ack {
                    from,
                    nonce,
                    boot_nonce,
                    configuration_version,
                    standing,
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
            TAG_SYNC => {
                let (from, rest) = take_host(rest)?;
                let (boot_nonce, rest) = take_word(rest)?;
                let (digest, rest) = take_word(rest)?;
                let (&pull, rest) = rest.split_first().ok_or(SwimWireError::Truncated)?;
                let pull = match pull {
                    PULL_NONE => false,
                    PULL_ASKED => true,
                    pull => return Err(SwimWireError::UnknownPull { pull }),
                };
                let (gossip, leftover) = decode_gossip(rest)?;
                if !leftover.is_empty() {
                    return Err(SwimWireError::GossipLengthMismatch);
                }
                Ok(SwimMessage::Sync {
                    from,
                    boot_nonce,
                    digest,
                    pull,
                    gossip,
                })
            }
            other => Err(SwimWireError::UnknownTag { tag: other }),
        }
    }
}

/// Appends a gossip batch: its u32 entry count, then each entry (subject, liveness byte, incarnation).
fn encode_gossip(out: &mut Vec<u8>, gossip: GossipBatch<'_>) {
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

/// Reads an acknowledgement's standing at the front of `bytes`: its presence byte, then the version when
/// present; `Truncated` if the bytes end first, `UnknownPresence` for any other presence byte.
fn take_standing(bytes: &[u8]) -> Result<(Option<u64>, &[u8]), SwimWireError> {
    let (&presence, rest) = bytes.split_first().ok_or(SwimWireError::Truncated)?;
    match presence {
        STANDING_ABSENT => Ok((None, rest)),
        STANDING_PRESENT => {
            let (generation, rest) = take_word(rest)?;
            Ok((Some(generation), rest))
        }
        presence => Err(SwimWireError::UnknownPresence { presence }),
    }
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
type GossipAndRest<'a> = (GossipBatch<'a>, &'a [u8]);

/// Decodes a gossip batch from the front of `bytes`: the u32 count, then exactly `count` entries,
/// returning the batch **and the bytes that follow it** (empty for a ping/ping-request, the coordinate
/// for an acknowledgement). The count is checked against the bytes that actually arrived, and every
/// entry's liveness byte is checked, before the batch is handed out.
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
    let (entries, leftover) = after_count.split_at(wanted);
    let (entries, _) = entries.as_chunks::<GOSSIP_ENTRY_BYTES>();
    for entry in entries {
        read_entry(entry)?;
    }
    Ok((GossipBatch::Wire(WireGossip(entries)), leftover))
}

/// Reads one gossip entry: the subject, its liveness byte and incarnation.
fn read_entry(entry: &[u8]) -> Result<(HostId, MemberState), SwimWireError> {
    let (subject, rest) = take_host(entry)?;
    let (&liveness, rest) = rest.split_first().ok_or(SwimWireError::Truncated)?;
    let (incarnation, _) = take_word(rest)?;
    Ok((
        subject,
        MemberState {
            liveness: liveness_from_byte(liveness)?,
            incarnation,
        },
    ))
}

/// Format: one coordinate component (and the height and error) is a little-endian `f64` stored as its
/// bit pattern.
const F64_BYTES: usize = size_of::<u64>();
/// Format: the coordinate is a `u32` dimension count, that many `f64` components of the point, then
/// the height and error `f64`s.
const COORDINATE_SCALARS: usize = 2;

/// Appends a network coordinate: the u32 dimension count, each component of the point, then the
/// height and error — every scalar a little-endian `f64` bit pattern. A received coordinate is its
/// bytes.
fn encode_coordinate(out: &mut Vec<u8>, coordinate: Coordinate<'_>) {
    let coordinate = match coordinate {
        Coordinate::Held(held) => held,
        Coordinate::Wire(WireCoordinate(bytes)) => {
            out.extend_from_slice(bytes);
            return;
        }
    };
    let dims = u32::try_from(DIMENSIONS).unwrap_or(u32::MAX);
    out.extend_from_slice(&dims.to_le_bytes());
    for component in coordinate.position {
        out.extend_from_slice(&component.to_bits().to_le_bytes());
    }
    out.extend_from_slice(&coordinate.height.to_bits().to_le_bytes());
    out.extend_from_slice(&coordinate.error.to_bits().to_le_bytes());
}

/// Decodes a network coordinate from `bytes`, which must be exactly the coordinate: a dimension count
/// equal to the engine's ([`DIMENSIONS`]), whose space the coordinate has to be in to mean anything,
/// and that many components and the two scalars.
fn decode_coordinate(bytes: &[u8]) -> Result<Coordinate<'_>, SwimWireError> {
    if bytes.len() < GOSSIP_COUNT_BYTES {
        return Err(SwimWireError::Truncated);
    }
    let (dims_bytes, rest) = bytes.split_at(GOSSIP_COUNT_BYTES);
    let mut dims_word = [0u8; GOSSIP_COUNT_BYTES];
    dims_word.copy_from_slice(dims_bytes);
    if usize::try_from(u32::from_le_bytes(dims_word)).ok() != Some(DIMENSIONS) {
        return Err(SwimWireError::MalformedCoordinate);
    }
    let wanted = DIMENSIONS
        .checked_add(COORDINATE_SCALARS)
        .and_then(|scalars| scalars.checked_mul(F64_BYTES))
        .ok_or(SwimWireError::MalformedCoordinate)?;
    if rest.len() != wanted {
        return Err(SwimWireError::MalformedCoordinate);
    }
    Ok(Coordinate::Wire(WireCoordinate(bytes)))
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

#[cfg(test)]
mod tests {
    use super::*;

    const A: HostId = HostId(2);
    const B: HostId = HostId(3);

    /// Encodes into a buffer of its own.
    trait Encoded {
        fn encoded(&self) -> Vec<u8>;
    }

    impl Encoded for SwimMessage<'_> {
        fn encoded(&self) -> Vec<u8> {
            let mut out = Vec::new();
            self.encode_into(&mut out);
            out
        }
    }

    const SAMPLE_GOSSIP: [(HostId, MemberState); 2] = [
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
    ];

    fn sample_coordinate() -> NetworkCoordinate {
        NetworkCoordinate {
            position: [1.5, -2.0],
            height: 3.25,
            error: 0.75,
        }
    }

    /// An acknowledgement's coordinate round-trips, and a coordinate of other dimensions than the
    /// engine's is refused.
    #[test]
    fn a_coordinate_round_trips_and_a_huge_one_is_refused() {
        let coordinate = sample_coordinate();
        let ack = SwimMessage::Ack {
            from: A,
            nonce: 42,
            boot_nonce: 1,
            configuration_version: 6,
            standing: Some(4),
            gossip: GossipBatch::Entries(&SAMPLE_GOSSIP),
            coordinate: Coordinate::Held(&coordinate),
        };
        assert_eq!(
            SwimMessage::decode(&ack.encoded()),
            Ok(ack),
            "the coordinate round-trips"
        );

        // An Ack whose coordinate claims a vast dimension count with no bytes to back it is refused.
        let mut hostile = vec![TAG_ACK];
        hostile.extend_from_slice(&7u64.to_le_bytes()); // from
        hostile.extend_from_slice(&0u64.to_le_bytes()); // nonce
        hostile.extend_from_slice(&0u64.to_le_bytes()); // boot_nonce
        hostile.extend_from_slice(&0u64.to_le_bytes()); // configuration_version
        hostile.push(STANDING_ABSENT); // no standing
        hostile.extend_from_slice(&0u32.to_le_bytes()); // empty gossip
        let bare = hostile.clone();
        hostile.extend_from_slice(&u32::MAX.to_le_bytes()); // coordinate dims = huge
        assert_eq!(
            SwimMessage::decode(&hostile),
            Err(SwimWireError::MalformedCoordinate),
            "an over-large coordinate is refused"
        );
        // One more dimension than the engine's, with the bytes to back it, is refused all the same:
        // a point of another space means nothing in this one.
        let mut wider = bare;
        let dims = u32::try_from(DIMENSIONS + 1).unwrap();
        wider.extend_from_slice(&dims.to_le_bytes());
        for _ in 0..DIMENSIONS + 1 + COORDINATE_SCALARS {
            wider.extend_from_slice(&1.0f64.to_bits().to_le_bytes());
        }
        assert_eq!(
            SwimMessage::decode(&wider),
            Err(SwimWireError::MalformedCoordinate)
        );
    }

    /// Hostile input on the acknowledgement's standing: a presence byte that is neither absent nor present is
    /// refused as such, and a standing whose version is cut short, or whose presence byte is the last byte, is
    /// refused as truncated — before any gossip is read.
    #[test]
    fn a_hostile_standing_is_refused() {
        let coordinate = sample_coordinate();
        let ack = SwimMessage::Ack {
            from: A,
            nonce: 42,
            boot_nonce: 1,
            configuration_version: 6,
            standing: Some(0x0102_0304_0506_0708),
            gossip: GossipBatch::Entries(&[]),
            coordinate: Coordinate::Held(&coordinate),
        }
        .encoded();
        // tag + from + nonce + boot_nonce + configuration_version: the presence byte's offset.
        let presence = 1 + 4 * size_of::<u64>();
        let mut unknown = ack.clone();
        if let Some(byte) = unknown.get_mut(presence) {
            *byte = 2;
        }
        assert_eq!(
            SwimMessage::decode(&unknown),
            Err(SwimWireError::UnknownPresence { presence: 2 })
        );
        for cut in [presence, presence + 1, presence + size_of::<u64>()] {
            assert_eq!(
                SwimMessage::decode(ack.get(..cut).unwrap_or_default()),
                Err(SwimWireError::Truncated),
                "cut at {cut}"
            );
        }
    }

    /// Every message kind round-trips through encode/decode unchanged, gossip included.
    #[test]
    fn every_message_round_trips() {
        let coordinate = sample_coordinate();
        let messages = [
            SwimMessage::Ping {
                from: A,
                nonce: 1,
                boot_nonce: 3,
                configuration_version: 4,
                gossip: GossipBatch::Entries(&SAMPLE_GOSSIP),
            },
            SwimMessage::Ack {
                from: B,
                nonce: u64::MAX,
                boot_nonce: u64::MAX,
                configuration_version: u64::MAX,
                standing: None,
                gossip: GossipBatch::Entries(&[]),
                coordinate: Coordinate::Held(&coordinate),
            },
            SwimMessage::Ack {
                from: A,
                nonce: 5,
                boot_nonce: 6,
                configuration_version: 7,
                standing: Some(u64::MAX),
                gossip: GossipBatch::Entries(&SAMPLE_GOSSIP),
                coordinate: Coordinate::Held(&coordinate),
            },
            SwimMessage::PingReq {
                from: A,
                target: B,
                nonce: 9,
                gossip: GossipBatch::Entries(&SAMPLE_GOSSIP),
            },
            SwimMessage::IndirectAck {
                from: B,
                target: A,
                nonce: 9,
                boot_nonce: 11,
                gossip: GossipBatch::Entries(&SAMPLE_GOSSIP),
            },
        ];
        for message in messages {
            let bytes = message.encoded();
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
            gossip: GossipBatch::Entries(&[]),
        };
        let mut expected = vec![TAG_INDIRECT_ACK];
        expected.extend_from_slice(&4u64.to_le_bytes()); // from = 4
        expected.extend_from_slice(&3u64.to_le_bytes()); // target = 3
        expected.extend_from_slice(&5u64.to_le_bytes()); // nonce = 5
        expected.extend_from_slice(&7u64.to_le_bytes()); // boot_nonce = 7
        expected.extend_from_slice(&0u32.to_le_bytes()); // gossip count = 0
        assert_eq!(message.encoded(), expected, "the byte layout is fixed");
        assert_eq!(
            SwimMessage::decode(&expected),
            Ok(message),
            "and decodes back"
        );
    }

    /// A view chunk's encoding is fixed and little-endian, a golden vector pins it (host 2 at daemon
    /// boot_nonce 7, view digest 11, asking a pull, carrying host 3 suspected at incarnation 1); a
    /// pull byte that is neither is refused, and a chunk cut inside its header is truncated.
    #[test]
    fn sync_has_a_golden_encoding() {
        let entries = [(
            HostId(3),
            MemberState {
                liveness: Liveness::Suspect,
                incarnation: 1,
            },
        )];
        let message = SwimMessage::Sync {
            from: HostId(2),
            boot_nonce: 7,
            digest: 11,
            pull: true,
            gossip: GossipBatch::Entries(&entries),
        };
        let mut expected = vec![TAG_SYNC];
        expected.extend_from_slice(&2u64.to_le_bytes()); // from = 2
        expected.extend_from_slice(&7u64.to_le_bytes()); // boot_nonce = 7
        expected.extend_from_slice(&11u64.to_le_bytes()); // digest = 11
        expected.push(PULL_ASKED); // pull
        expected.extend_from_slice(&1u32.to_le_bytes()); // gossip count = 1
        expected.extend_from_slice(&3u64.to_le_bytes()); // host 3
        expected.push(LIVENESS_SUSPECT);
        expected.extend_from_slice(&1u64.to_le_bytes()); // incarnation 1
        assert_eq!(message.encoded(), expected, "the byte layout is fixed");
        assert_eq!(
            SwimMessage::decode(&expected),
            Ok(message),
            "and decodes back"
        );
        let pull = 1 + 3 * size_of::<u64>();
        let mut bad = expected.clone();
        bad[pull] = 2;
        assert_eq!(
            SwimMessage::decode(&bad),
            Err(SwimWireError::UnknownPull { pull: 2 })
        );
        for cut in 1..=pull {
            assert_eq!(
                SwimMessage::decode(&expected[..cut]),
                Err(SwimWireError::Truncated)
            );
        }
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
            gossip: GossipBatch::Entries(&[]),
        }
        .encoded();
        let request = SwimMessage::PingReq {
            from: HostId(4),
            target: HostId(3),
            nonce: 5,
            gossip: GossipBatch::Entries(&[]),
        }
        .encoded();
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
    /// host 2 at daemon boot_nonce 7 announcing configuration version 9, carrying one gossip entry: host 3,
    /// Suspect, incarnation 1).
    #[test]
    fn ping_has_a_golden_encoding() {
        let message = SwimMessage::Ping {
            from: HostId(2),
            nonce: 5,
            boot_nonce: 7,
            configuration_version: 9,
            gossip: GossipBatch::Entries(&[(
                HostId(3),
                MemberState {
                    liveness: Liveness::Suspect,
                    incarnation: 1,
                },
            )]),
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
            9,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // configuration_version = 9
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
        assert_eq!(message.encoded(), expected, "the byte layout is fixed");
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
        // Tag + from + nonce + boot_nonce, but the configuration version word is missing.
        let mut boot_nonce_ok = nonce_ok.clone();
        boot_nonce_ok.extend_from_slice(&1u64.to_le_bytes()); // boot_nonce
        assert_eq!(
            SwimMessage::decode(&boot_nonce_ok),
            Err(SwimWireError::Truncated),
            "configuration version cut"
        );
        // Tag + from + nonce + boot_nonce + configuration version, but the gossip count word is missing.
        let mut version_ok = boot_nonce_ok.clone();
        version_ok.extend_from_slice(&3u64.to_le_bytes()); // configuration_version
        assert_eq!(
            SwimMessage::decode(&version_ok),
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
            0, 0, 0, 0, 0, 0, 0, 0, // configuration_version
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
        bytes.extend_from_slice(&0u64.to_le_bytes()); // configuration_version
        bytes.push(STANDING_ABSENT); // no standing
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
        short.extend_from_slice(&0u64.to_le_bytes()); // configuration_version
        short.push(STANDING_ABSENT); // no standing
        short.extend_from_slice(&1u32.to_le_bytes());
        short.extend_from_slice(&[9, 9, 9]); // a partial entry
        assert_eq!(
            SwimMessage::decode(&short),
            Err(SwimWireError::GossipLengthMismatch)
        );
    }
}
