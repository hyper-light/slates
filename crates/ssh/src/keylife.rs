//! How much and how long one key serves (A-594; RFC 4344 §3; RFC 4253 §9; `draft-ietf-sshm-chacha20-poly1305-04` §8).
//! No bound here is a tuning literal: each follows from the cipher by the formula its analysis gives, or is learned.
//!
//! **By use, derived from the cipher.**
//! - Packets: "SSH implementations SHOULD rekey at least once every 2**32 outgoing packets" [B: RFC 4344 §3.1], which is
//!   also the sequence number's whole space (RFC 4253 §6.4) and so, under ChaCha20-Poly1305, whose nonce is the sequence
//!   number, the most packets a key can seal without reusing a nonce (the chacha draft §8: "must never reuse a
//!   {key, nonce}").
//! - Blocks: for a block cipher of `L` bits, "SHOULD NOT encrypt more than 2**(L/4) blocks before rekeying again"
//!   [B: RFC 4344 §3.2]; AES's `L` is 128, so 2^32 blocks of 16 bytes. ChaCha20 is a stream cipher, with no birthday
//!   bound of that kind; its own limit, 2^70 bytes under one nonce (the chacha draft §8), cannot be reached by one packet.
//! - When to rekey: before the limit, at its half, the point RFC 4344 §3.1 itself prefers for packets ("rekey after
//!   receiving more than 2**31 packets"), applied the same way to blocks and to both directions, as OpenSSH applies its
//!   own (`MAX_PACKETS (1U<<31)` in `packet.c` at `fa4b2a82`) \[C\]. The other half covers what is in flight while the new
//!   exchange runs; a key that reaches its whole limit anyway ends the connection.
//! - An operator may cap a key's use lower ([`UsageCap`], as the session plane's `cap_key_usage` does), never higher.
//!
//! **By data and time, the RFC's.** RFC 4253 §9: "It is RECOMMENDED that the keys be changed after each gigabyte of
//! transmitted data or after each hour of connection time, whichever comes sooner", and a re-exchange "should not be
//! performed too often". So a key is due after a gigabyte of packets in either direction ([`REKEY_BYTES`]) and after an
//! hour ([`RECOMMENDED_KEY_TIME_NS`]), a protocol rule, not a tunable. A-594 had learned the time from the lifetimes of
//! this edge's earlier connections; one short session (31 ms, measured) then put every long transfer on a rekey every few
//! seconds, which a stall at a rekey made visible (A-745, found by use), so nothing an earlier session measured moves it.
//! The transport never rekeys by time sooner than its last exchange took, so a re-exchange can never fill the connection.

use crate::packet::{AES_BLOCK_LEN, Cipher, Usage};

/// Format: nanoseconds in a second.
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// RFC 4253 §9's key time, an hour ("after each hour of connection time").
pub const RECOMMENDED_KEY_TIME_NS: u64 = NANOS_PER_SECOND.saturating_mul(SECONDS_PER_HOUR);

/// RFC 4253 §9's data a key serves in each direction, a gigabyte ("after each gigabyte of transmitted data"), taken as
/// 2^30 bytes.
pub const REKEY_BYTES: u64 = 1 << GIGABYTE_BITS;

/// Format: the bits of a gigabyte's byte count, 2^30.
const GIGABYTE_BITS: u32 = 30;

/// Format: seconds in an hour.
const SECONDS_PER_HOUR: u64 = 3_600;

/// Format: bits in a byte.
const BITS_PER_BYTE: usize = 8;

/// The bits of a sequence number (RFC 4253 §6.4, `uint32 sequence_number`).
const SEQUENCE_BITS: u32 = u32::BITS;

/// The most a key may process before it must be replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyLimits {
  /// Packets.
  pub packets: u64,
  /// Block-cipher blocks.
  pub blocks: u64,
  /// Bytes after which a re-exchange is due (RFC 4253 §9's gigabyte): a rekey point, never a limit that ends the key.
  pub bytes: u64,
}

/// An operator's cap on a key's use, below the derived limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageCap {
  /// The most packets, when lower than the cipher's.
  pub packets: Option<u64>,
  /// The most blocks, when lower than the cipher's.
  pub blocks: Option<u64>,
}

/// RFC 4344 §3.2's bound for a block cipher of `block_bits` bits: 2^(L/4) blocks.
pub fn block_cipher_bound(block_bits: u32) -> u64 {
  // Derived: RFC 4344 §3.2's exponent is a quarter of the block's bits, L/4.
  let exponent = block_bits.checked_div(4).unwrap_or(0);
  1u64.checked_shl(exponent).unwrap_or(u64::MAX)
}

impl KeyLimits {
  /// The limits `cipher`'s analysis gives.
  pub fn for_cipher(cipher: Cipher) -> Self {
    let packets = 1u64.checked_shl(SEQUENCE_BITS).unwrap_or(u64::MAX);
    let blocks = match cipher {
      Cipher::Aes256Gcm => block_cipher_bound(
        u32::try_from(AES_BLOCK_LEN.saturating_mul(BITS_PER_BYTE)).unwrap_or(u32::MAX),
      ),
      Cipher::ChaCha20Poly1305 => u64::MAX,
    };
    Self {
      packets,
      blocks,
      bytes: REKEY_BYTES,
    }
  }

  /// These limits under `cap`, which can only lower them.
  pub fn capped(self, cap: UsageCap) -> Self {
    Self {
      packets: cap
        .packets
        .map_or(self.packets, |packets| packets.min(self.packets)),
      blocks: cap
        .blocks
        .map_or(self.blocks, |blocks| blocks.min(self.blocks)),
      bytes: self.bytes,
    }
  }

  /// Whether `usage` passes the limits: the key may not process it.
  pub fn exceeded_by(self, usage: Usage) -> bool {
    usage.packets > self.packets || usage.blocks > self.blocks
  }

  /// Whether `usage` has reached the rekey point: half of each limit, or the RFC's gigabyte of data.
  pub fn due(self, usage: Usage) -> bool {
    usage.packets >= self.packets.div_ceil(2)
      || usage.blocks >= self.blocks.div_ceil(2)
      || usage.bytes >= self.bytes
  }
}
