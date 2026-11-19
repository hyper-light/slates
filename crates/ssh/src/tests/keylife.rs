//! How much and how long a key serves (A-594; RFC 4344 §3; RFC 4253 §9).

use crate::keylife::{
  KeyLimits, RECOMMENDED_KEY_TIME_NS, UsageCap, block_cipher_bound, key_time_limit_ns,
};
use crate::packet::{Cipher, Usage};

/// A-594 (RFC 4344 §3.2, "SHOULD NOT encrypt more than 2**(L/4) blocks"): do derive the bound for AES's 128-bit block
/// and for a 256-bit block. Expect 2^32 blocks for AES, and the larger one saturated rather than wrapped.
#[test]
fn the_block_bound_follows_the_block_length() {
  assert_eq!(block_cipher_bound(128), 1 << 32);
  assert_eq!(block_cipher_bound(256), u64::MAX);
}

/// A-594 (RFC 4344 §3.1, §3.2; the chacha draft §8): do derive each cipher's limits. Expect AES-GCM held to 2^32 packets
/// and 2^32 blocks; ChaCha20-Poly1305 to 2^32 packets, its nonce space, with no block bound.
#[test]
fn each_cipher_has_its_own_limits() {
  assert_eq!(
    KeyLimits::for_cipher(Cipher::Aes256Gcm),
    KeyLimits {
      packets: 1 << 32,
      blocks: 1 << 32
    }
  );
  assert_eq!(
    KeyLimits::for_cipher(Cipher::ChaCha20Poly1305),
    KeyLimits {
      packets: 1 << 32,
      blocks: u64::MAX
    }
  );
}

/// A-594: do cap AES-GCM's limits lower and higher. Expect the lower cap taken and the higher one ignored: a cap only
/// lowers.
#[test]
fn a_cap_only_lowers_the_limits() {
  let limits = KeyLimits::for_cipher(Cipher::Aes256Gcm);
  assert_eq!(
    limits.capped(UsageCap {
      packets: Some(8),
      blocks: None
    }),
    KeyLimits {
      packets: 8,
      blocks: 1 << 32
    }
  );
  assert_eq!(
    limits.capped(UsageCap {
      packets: Some(u64::MAX),
      blocks: Some(1 << 40)
    }),
    limits
  );
}

/// A-594 (RFC 4344 §3.1, "rekey after receiving more than 2**31 packets"): do test usages below, at and past half a
/// limit, and past the whole. Expect the rekey due from half on, and the key exceeded only past the whole limit.
#[test]
fn a_key_is_due_at_half_and_exceeded_past_its_limit() {
  let limits = KeyLimits {
    packets: 8,
    blocks: 100,
  };
  assert!(!limits.due(Usage {
    packets: 3,
    blocks: 49
  }));
  assert!(limits.due(Usage {
    packets: 4,
    blocks: 0
  }));
  assert!(limits.due(Usage {
    packets: 0,
    blocks: 50
  }));
  assert!(!limits.exceeded_by(Usage {
    packets: 8,
    blocks: 100
  }));
  assert!(limits.exceeded_by(Usage {
    packets: 9,
    blocks: 0
  }));
  assert!(limits.exceeded_by(Usage {
    packets: 0,
    blocks: 101
  }));
}

/// A-594 (RFC 4253 §9, "after each hour of connection time"): do take the key time with nothing measured, with a
/// measured tail of ten seconds, and with one of two hours. Expect the hour, ten seconds, and the hour again.
#[test]
fn the_key_time_is_the_learned_tail_within_the_hour() {
  assert_eq!(key_time_limit_ns(None), RECOMMENDED_KEY_TIME_NS);
  assert_eq!(key_time_limit_ns(Some(10_000_000_000)), 10_000_000_000);
  assert_eq!(
    key_time_limit_ns(Some(2 * RECOMMENDED_KEY_TIME_NS)),
    RECOMMENDED_KEY_TIME_NS
  );
}
