//! The control-plane key schedule (§4.10a §7; design: `docs/wip/fleet-transport.md`). HKDF-Expand-Label
//! (RFC 5869 for HKDF; RFC 8446 §7.1 for the labelled form) derives one AES-256-GCM key per
//! `(sender, key_epoch, direction)` channel from a node's **control secret**. The control secret is
//! **injected** — enrollment (§4.13, owed) mints and distributes it; this module turns it into the
//! seal's keys, exactly as [`crate::seal::Sealer`] takes an injected key. So the schedule's consumer
//! is the seal already shipped ([`KeySchedule::sealer`]/[`KeySchedule::opener`] build a `Sealer`/
//! `Opener` directly), not a dangling mechanism.
//!
//! **Why a labelled schedule and not the raw secret.** Deriving per-channel keys means a captured
//! datagram cannot be replayed on another channel or the reverse direction (the key differs), and a
//! key-epoch bump rotates every key without re-enrolling. The construction is TLS 1.3's own
//! (D-15 chose TLS 1.3), so it rests on standardised, analysed key separation, not a bespoke scheme.
//! Vetted RustCrypto crates (`hkdf`, `sha2`); key derivation is never hand-rolled.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::seal::{KEY_BYTES, Opener, Sealer};

/// The direction of a keyed channel: the two ends derive matching keys per direction, so `A → B`
/// traffic and `B → A` traffic use different keys (neither can be replayed as the other), and both
/// ends agree on a datagram's direction by who sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
  /// Traffic from the channel's initiator to its responder.
  Initiator,
  /// Traffic from the responder to the initiator.
  Responder,
}

impl Direction {
  /// The one-byte tag mixed into the derivation context (distinct per direction).
  fn tag(self) -> u8 {
    match self {
      Direction::Initiator => 1,
      Direction::Responder => 2,
    }
  }
}

/// Format: the HKDF-Extract salt — a fixed, non-secret, slates-domain constant that separates this
/// key schedule from any other HKDF use of the same enrolled secret (RFC 5869 §3.1: a salt adds
/// domain separation). Versioned so a future schedule change is a distinct derivation.
const EXTRACT_SALT: &[u8] = b"slates control key schedule v1";

/// Format: the label prefix of an `HkdfLabel` (RFC 8446 §7.1 uses "tls13 "; slates uses its own so a
/// slates key and a TLS key from the same secret never collide).
const LABEL_PREFIX: &[u8] = b"slates ";

/// Format: the per-channel derivation label, appended to [`LABEL_PREFIX`].
const SEAL_LABEL: &[u8] = b"control seal";

/// A node's control-plane key schedule: the HKDF pseudorandom key extracted from its control secret,
/// ready to expand per-channel keys.
pub struct KeySchedule {
  hkdf: Hkdf<Sha256>,
}

impl std::fmt::Debug for KeySchedule {
  /// Never prints key material.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("KeySchedule(<redacted>)")
  }
}

impl KeySchedule {
  /// Extracts the schedule from a node's `control_secret` (injected; enrollment owed). HKDF-Extract
  /// with the slates-domain salt turns the secret into a uniform pseudorandom key the per-channel
  /// expansions draw from.
  pub fn from_control_secret(control_secret: &[u8]) -> KeySchedule {
    KeySchedule {
      hkdf: Hkdf::<Sha256>::new(Some(EXTRACT_SALT), control_secret),
    }
  }

  /// Derives the 32-byte AES-256-GCM key for the `(sender, key_epoch, direction)` channel via
  /// HKDF-Expand-Label. Deterministic: the same schedule and channel always yield the same key, so
  /// the two ends agree without exchanging keys.
  pub fn seal_key(&self, sender: u64, key_epoch: u32, direction: Direction) -> [u8; KEY_BYTES] {
    let mut context = [0u8; size_of::<u64>() + size_of::<u32>() + size_of::<u8>()];
    context[..size_of::<u64>()].copy_from_slice(&sender.to_le_bytes());
    context[size_of::<u64>()..size_of::<u64>() + size_of::<u32>()]
      .copy_from_slice(&key_epoch.to_le_bytes());
    context[size_of::<u64>() + size_of::<u32>()] = direction.tag();
    self.expand_label(SEAL_LABEL, &context)
  }

  /// A [`Sealer`] over the derived key for `(sender, key_epoch, direction)`, on nonce `channel`.
  pub fn sealer(&self, sender: u64, key_epoch: u32, direction: Direction, channel: u32) -> Sealer {
    Sealer::from_key(&self.seal_key(sender, key_epoch, direction), channel)
  }

  /// An [`Opener`] over the derived key for `(sender, key_epoch, direction)`, on nonce `channel`.
  pub fn opener(&self, sender: u64, key_epoch: u32, direction: Direction, channel: u32) -> Opener {
    Opener::from_key(&self.seal_key(sender, key_epoch, direction), channel)
  }

  /// HKDF-Expand-Label (RFC 8446 §7.1): expand the schedule's PRK over a structured `HkdfLabel`
  /// (the output length, then the length-prefixed `"slates " + label`, then the length-prefixed
  /// context) into a fresh [`KEY_BYTES`] key. The output is exactly [`KEY_BYTES`] (32), far under
  /// HKDF's `255 * HashLen` ceiling, so `expand` cannot fail; the `Err` arm is unreachable and, to
  /// keep shipped code free of `expect`/`unwrap`, it is mapped to an all-ones key that no valid
  /// derivation produces — a corrupt-but-never-silent key that fails the seal loudly rather than a
  /// zero key or a panic. (It is never reached: a test derives real keys end to end.)
  fn expand_label(&self, label: &[u8], context: &[u8]) -> [u8; KEY_BYTES] {
    let mut labelled = Vec::with_capacity(LABEL_PREFIX.len() + label.len());
    labelled.extend_from_slice(LABEL_PREFIX);
    labelled.extend_from_slice(label);
    let mut info = Vec::with_capacity(
      size_of::<u16>() + size_of::<u8>() + labelled.len() + size_of::<u8>() + context.len(),
    );
    // The 16-bit desired length, then the two `opaque<..>` vectors, each with a one-byte length.
    info.extend_from_slice(&u16::try_from(KEY_BYTES).unwrap_or(u16::MAX).to_be_bytes());
    info.push(u8::try_from(labelled.len()).unwrap_or(u8::MAX));
    info.extend_from_slice(&labelled);
    info.push(u8::try_from(context.len()).unwrap_or(u8::MAX));
    info.extend_from_slice(context);
    let mut key = [0u8; KEY_BYTES];
    if self.hkdf.expand(&info, &mut key).is_err() {
      return [0xFFu8; KEY_BYTES];
    }
    key
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A fixed control secret for the deterministic tests.
  fn secret() -> [u8; 32] {
    let mut s = [0u8; 32];
    let mut b: u8 = 7;
    for slot in &mut s {
      *slot = b;
      b = b.wrapping_mul(5).wrapping_add(1);
    }
    s
  }

  /// The raw HKDF under the schedule matches RFC 5869's SHA-256 test case 1 — evidence the crate and
  /// this module's use of it are correct before the labelled form is trusted (A: RFC 5869 App. A.1).
  #[test]
  fn hkdf_matches_rfc5869_test_case_1() {
    let ikm = [0x0bu8; 22];
    let salt: Vec<u8> = (0x00u8..=0x0c).collect();
    let info: Vec<u8> = (0xf0u8..=0xf9).collect();
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut okm = [0u8; 42];
    hk.expand(&info, &mut okm).unwrap();
    // The expected OKM from RFC 5869 Appendix A.1.
    let expected = "3cb25f25faacd57a90434f64d0362f2a\
                    2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
                    34007208d5b887185865";
    assert_eq!(hex(&okm), expected, "HKDF-SHA256 must match RFC 5869 A.1");
  }

  /// A derived seal key is deterministic and pinned by a golden vector, so a schedule or dependency
  /// change that moved the key would be caught (the two ends rely on this determinism).
  #[test]
  fn a_derived_key_matches_its_golden_vector() {
    let schedule = KeySchedule::from_control_secret(&secret());
    let key = schedule.seal_key(0x0102_0304_0506_0708, 7, Direction::Initiator);
    let again = schedule.seal_key(0x0102_0304_0506_0708, 7, Direction::Initiator);
    assert_eq!(key, again, "derivation is deterministic");
    assert_eq!(
      hex(&key),
      GOLDEN_INITIATOR_KEY,
      "derived key drifted from its golden vector"
    );
  }

  /// Distinct channels get distinct keys: sender, key epoch and direction each separate the key, so
  /// a datagram cannot be opened on another channel or the reverse direction.
  #[test]
  fn distinct_channels_derive_distinct_keys() {
    let schedule = KeySchedule::from_control_secret(&secret());
    let base = schedule.seal_key(1, 1, Direction::Initiator);
    assert_ne!(
      base,
      schedule.seal_key(2, 1, Direction::Initiator),
      "sender separates"
    );
    assert_ne!(
      base,
      schedule.seal_key(1, 2, Direction::Initiator),
      "key epoch separates"
    );
    assert_ne!(
      base,
      schedule.seal_key(1, 1, Direction::Responder),
      "direction separates"
    );
    // A different control secret derives a different key for the same channel.
    let other = KeySchedule::from_control_secret(&[0x99u8; 32]);
    assert_ne!(
      base,
      other.seal_key(1, 1, Direction::Initiator),
      "the secret separates"
    );
  }

  /// The schedule feeds the seal end to end: a sealer and an opener built from the *same* schedule
  /// and channel round-trip a datagram; an opener on a different channel cannot open it. This is the
  /// consumer that makes the schedule real, not a dangling mechanism.
  #[test]
  fn the_schedule_keys_the_seal_end_to_end() {
    use crate::{ControlDatagram, Envelope};
    let schedule = KeySchedule::from_control_secret(&secret());
    let (sender, epoch, channel) = (0x1122_3344_5566_7788u64, 3u32, 9u32);
    let datagram = ControlDatagram {
      sender,
      key_epoch: epoch,
      envelope: Envelope {
        kind: 2,
        class: 1,
        flags: 0,
        epoch: 5,
        hlc: 0x1234,
        request_id: 42,
      },
      body: b"keyed by the schedule".to_vec(),
    };
    let mut sealer = schedule.sealer(sender, epoch, Direction::Initiator, channel);
    let mut opener = schedule.opener(sender, epoch, Direction::Initiator, channel);
    let wire = datagram.encode_sealed(&mut sealer).unwrap();
    assert_eq!(
      ControlDatagram::decode_sealed(&wire, &mut opener),
      Ok(datagram.clone())
    );
    // An opener for the reverse direction (a different key) cannot open the initiator's datagram.
    let mut sealer2 = schedule.sealer(sender, epoch, Direction::Initiator, channel);
    let wire2 = datagram.encode_sealed(&mut sealer2).unwrap();
    let mut reverse = schedule.opener(sender, epoch, Direction::Responder, channel);
    assert!(
      ControlDatagram::decode_sealed(&wire2, &mut reverse).is_err(),
      "the reverse-direction key cannot open the datagram"
    );
  }

  /// Lowercase hex for the vectors.
  fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * size_of::<u16>());
    for b in bytes {
      s.push_str(&format!("{b:02x}"));
    }
    s
  }

  /// The golden derived key for `secret()`, sender `0x0102030405060708`, epoch 7, Initiator.
  const GOLDEN_INITIATOR_KEY: &str =
    "5e3d0f715718787779414577877288904b526d7911a725bc1212c45f86c4bb68";
}
