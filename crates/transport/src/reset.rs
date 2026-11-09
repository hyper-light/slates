//! Stateless reset (RFC 9000 §10.3; A-261): an endpoint that has lost a connection's state, a daemon restarted under the
//! same identity, answers a packet for that connection with a datagram ending in a token only it could have issued for
//! the connection's id, and the peer ends the connection at once instead of waiting on a session that is gone.
//!
//! **The key.** A node's reset key is derived with HKDF-SHA256 from its identity's private key, which only the node holds
//! (never from the certificate, which every peer sees: a key derivable from public material would let anyone forge a
//! reset). A daemon restarted under the same identity derives the same key, so it can recompute the token of a session it
//! no longer remembers (RFC 9000 §10.3.2: "an endpoint that loses state can use the same method to generate a valid
//! stateless reset token"). A node that comes back under a new identity derives another key and cannot reset the old
//! node's sessions, which is right: it is another node.
//!
//! **The token** is HMAC-SHA256 of the session's connection id under that key, truncated to 16 bytes (§10.3.2's
//! construction). A session's id is unique (both ends derive it from the TLS exporter) and never changes, so each session
//! has one token per end; the two ends' keys differ, so their tokens do too (§10.3.2: a token is never used for two ids,
//! nor an id and key for another connection). A token is issued only inside the encrypted session, so only the peer
//! learns it: a reset is not otherwise protected (§10.3.2), and a token seen is a session anyone can end.
//!
//! **The datagram** (§10.3) is shaped as a short-header packet, unpredictable but for its last 16 bytes, the token. It is
//! always shorter than the packet that drew it (§10.3.3, so two endpoints cannot loop resets), never shorter than
//! [`MIN_RESET_BYTES`], and no longer than [`RESET_SEND_BYTES`].

use rustls::crypto::tls13::{Hkdf, OkmBlock};

use crate::endpoint::ConnectionId;

/// Format: a stateless reset token's length (RFC 9000 §10.3: the last 16 bytes of the datagram).
pub const RESET_TOKEN_BYTES: usize = 16;

/// A stateless reset token.
pub type ResetToken = [u8; RESET_TOKEN_BYTES];

/// Format: the shortest stateless reset (RFC 9000 §10.3: 5 bytes that look like a short header's start and are
/// unpredictable in at least 38 bits, then the token). A datagram that would have to be shorter to stay below its trigger
/// draws no reset.
pub const MIN_RESET_BYTES: usize = 21;

/// Derived: the length of a reset this end sends when its trigger allows: 43 bytes, at least the 41 below which an
/// observer might tell a reset from a short packet (RFC 9000 §10.3.3), and as short as that allows, so a reset costs the
/// path as little as possible. One byte shorter than any trigger of 43 bytes or fewer (§10.3: "one byte shorter").
pub const RESET_SEND_BYTES: usize = 43;

/// Format: the HKDF salt that keeps this key apart from every other key derived from the same identity.
const RESET_KEY_SALT: &[u8] = b"slates/stateless-reset/v1";

/// Format: the HKDF info for the reset key.
const RESET_KEY_INFO: &[u8] = b"stateless reset key";

/// Format: a short header's fixed bit, set in a reset's first byte (RFC 9000 §17.3.1), with the long-header bit clear.
const SHORT_FIXED_BIT: u8 = 0x40;

/// Format: the bits of a reset's first byte that are left unpredictable (all but the two header-form bits).
const FIRST_BYTE_RANDOM_BITS: u8 = 0x3f;

/// A node's stateless reset key, derived from its identity's private key ([`ResetKey::derive`]).
#[derive(Clone)]
pub struct ResetKey {
  hkdf: &'static dyn Hkdf,
  key: OkmBlock,
}

impl ResetKey {
  /// The reset key of the node whose private key's secret bytes are `secret`: HKDF-SHA256 over them, salted and labelled
  /// for this use alone. `None` when the provider offers no SHA-256 suite.
  pub fn derive(secret: &[u8]) -> Option<ResetKey> {
    let suite = rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256.tls13()?;
    let hkdf = suite.hkdf_provider;
    let expander = hkdf.extract_from_secret(Some(RESET_KEY_SALT), secret);
    let key = expander.expand_block(&[RESET_KEY_INFO]);
    Some(ResetKey { hkdf, key })
  }

  /// The token this node issues for the session with connection id `id`.
  pub fn token(&self, id: &ConnectionId) -> ResetToken {
    let tag = self.hkdf.hmac_sign(&self.key, id);
    let mut token = [0u8; RESET_TOKEN_BYTES];
    for (out, byte) in token.iter_mut().zip(tag.as_ref()) {
      *out = *byte;
    }
    token
  }
}

impl std::fmt::Debug for ResetKey {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("ResetKey(..)")
  }
}

/// How long a reset drawn by a datagram of `trigger_bytes` is: shorter than it, at most [`RESET_SEND_BYTES`], and `None`
/// when it would have to be shorter than [`MIN_RESET_BYTES`] (RFC 9000 §10.3, §10.3.3).
pub fn reset_length(trigger_bytes: usize) -> Option<usize> {
  let length = trigger_bytes.checked_sub(1)?.min(RESET_SEND_BYTES);
  (length >= MIN_RESET_BYTES).then_some(length)
}

/// A stateless reset ending in `token`, drawn by a datagram of `trigger_bytes`: shaped as a short-header packet, its
/// other bytes from `random` (the provider's secure random). `None` when the trigger is too short to answer, or the
/// random source refused.
pub fn reset_datagram(
  token: &ResetToken,
  trigger_bytes: usize,
  random: impl FnOnce(&mut [u8]) -> bool,
) -> Option<Vec<u8>> {
  let length = reset_length(trigger_bytes)?;
  let mut datagram = vec![0u8; length];
  let unpredictable = length.checked_sub(RESET_TOKEN_BYTES)?;
  if !random(datagram.get_mut(..unpredictable)?) {
    return None;
  }
  let first = datagram.first_mut()?;
  *first = SHORT_FIXED_BIT | (*first & FIRST_BYTE_RANDOM_BITS);
  datagram.get_mut(unpredictable..)?.copy_from_slice(token);
  Some(datagram)
}

/// Whether `datagram` is a stateless reset bearing `token`: long enough, and its last [`RESET_TOKEN_BYTES`] equal to the
/// token, compared in time that does not depend on where they differ.
pub fn is_reset(datagram: &[u8], token: &ResetToken) -> bool {
  if datagram.len() < MIN_RESET_BYTES {
    return false;
  }
  let Some(tail) = datagram.get(datagram.len().saturating_sub(RESET_TOKEN_BYTES)..) else {
    return false;
  };
  tail
    .iter()
    .zip(token)
    .fold(0u8, |differ, (a, b)| differ | (a ^ b))
    == 0
}

#[cfg(test)]
mod tests {
  use super::*;

  fn key(secret: &[u8]) -> ResetKey {
    ResetKey::derive(secret).unwrap()
  }

  fn fill(seed: u8) -> impl FnOnce(&mut [u8]) -> bool {
    move |out: &mut [u8]| {
      for (index, byte) in out.iter_mut().enumerate() {
        *byte = seed.wrapping_add(u8::try_from(index % 251).unwrap());
      }
      true
    }
  }

  /// RFC 9000 §10.3.2, the restart this exists for: the same secret makes the same token for an id, so a daemon that
  /// lost the session recomputes it; another secret, or another id, makes another. Do: derive twice from one secret and
  /// once from another, and take tokens for two ids. Expect: equal only for the same secret and id.
  #[test]
  fn a_restarted_node_makes_the_same_token_and_another_node_cannot() {
    let id_a: ConnectionId = [1, 2, 3, 4, 5, 6, 7, 8];
    let id_b: ConnectionId = [8, 7, 6, 5, 4, 3, 2, 1];
    let before = key(b"the node's private key").token(&id_a);
    let after_restart = key(b"the node's private key").token(&id_a);
    assert_eq!(before, after_restart);
    assert_ne!(before, key(b"another node's private key").token(&id_a));
    assert_ne!(before, key(b"the node's private key").token(&id_b));
  }

  /// RFC 9000 §10.3 and §10.3.3: a reset is shorter than its trigger, at least 21 bytes, at most 43, shaped as a short
  /// header, and ends in the token. Do: build resets for triggers from 0 to 1,500 bytes. Expect: none below 22 bytes of
  /// trigger; otherwise the lengths and shape above.
  #[test]
  fn a_reset_is_shorter_than_its_trigger_and_ends_in_the_token() {
    let token = key(b"secret").token(&[9; 8]);
    for trigger in 0..1_500usize {
      match reset_datagram(&token, trigger, fill(7)) {
        None => assert!(
          trigger <= MIN_RESET_BYTES,
          "a {trigger}-byte trigger can be answered"
        ),
        Some(reset) => {
          assert!(
            reset.len() < trigger,
            "shorter than its {trigger}-byte trigger"
          );
          assert!((MIN_RESET_BYTES..=RESET_SEND_BYTES).contains(&reset.len()));
          assert!(
            crate::endpoint::is_short_header(&reset),
            "shaped as a short header"
          );
          assert_eq!(reset.first().unwrap() & 0x80, 0, "not a long header");
          assert!(is_reset(&reset, &token));
        }
      }
    }
  }

  /// Hostile input: only a datagram ending in the token is a reset. Do: test a reset with each byte of its token flipped,
  /// truncated at every length, under another token, and datagrams too short to carry a token. Expect: none is a reset.
  #[test]
  fn nothing_but_the_token_makes_a_reset() {
    let token = key(b"secret").token(&[9; 8]);
    let reset = reset_datagram(&token, 100, fill(3)).unwrap();
    for index in reset.len() - RESET_TOKEN_BYTES..reset.len() {
      let mut flipped = reset.clone();
      if let Some(byte) = flipped.get_mut(index) {
        *byte ^= 0x01;
      }
      assert!(!is_reset(&flipped, &token), "byte {index} flipped");
    }
    for cut in 0..reset.len() {
      assert!(
        !is_reset(reset.get(..cut).unwrap(), &token),
        "truncated at {cut}"
      );
    }
    assert!(!is_reset(&reset, &key(b"other").token(&[9; 8])));
    assert!(!is_reset(&token, &token), "the token alone is too short");
  }

  /// A refused random source sends nothing rather than a predictable reset.
  #[test]
  fn a_refused_random_source_sends_no_reset() {
    let token = key(b"secret").token(&[9; 8]);
    assert_eq!(reset_datagram(&token, 100, |_| false), None);
  }
}
