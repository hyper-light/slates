//! The control-plane AEAD seal (§4.10a §7; design: `docs/wip/fleet-transport.md`, a draft awaiting
//! ratification). A control datagram's sealed region — the envelope header and body
//! ([`ControlDatagram::write_sealed_region`]) — is encrypted and authenticated with AES-256-GCM, the
//! cipher slates already commits to on the wire (D-15's TLS 1.3), and the cleartext routing prologue
//! (version, sender, key epoch) is bound in as additional authenticated data (AAD) so it cannot be
//! forged or re-pointed without breaking the tag [A: NIST SP 800-38D; A: McGrew & Viega, GCM, 2004].
//!
//! **Nonces are a counter, never random.** The 96-bit nonce is `counter ‖ channel`: a per-[`Sealer`]
//! monotonic 64-bit counter and a 32-bit channel that separates directions/streams under one key. A
//! counter is used once and only once — the sealer refuses to advance past `u64::MAX` (its nonce space
//! exhausted, [`SealError::CounterExhausted`]), and an [`Opener`] refuses a counter it has already
//! accepted ([`SealError::Replay`]). This is the deterministic nonce construction GCM's one hard rule
//! demands (SP 800-38D §8.2.1) held by construction, and it is *why* the seal needs no RNG: the
//! discipline is deterministic and testable, and the wire is byte-identical across builds (a golden
//! vector gates it, `a_sealed_datagram_matches_its_golden_vector`).
//!
//! **The key is injected**, not derived here: [`Sealer::from_key`]/[`Opener::from_key`] take the bytes
//! ([`KEY_BYTES`], AES-256's key size read from the cipher) that the key *schedule* — an HKDF
//! derivation over an enrolled host secret, **owed** with enrollment — will produce. This crate owns
//! the seal, not identity, exactly as the codec owns the wire shape and not the socket.
//!
//! **Reorder note.** An `Opener` accepts strictly increasing counters (drop-older), which suits the
//! control plane where a superseded datagram is anti-information; a sliding replay window for a
//! reorder-tolerant stream is owed with the session plane. High-water advances only *after* the tag
//! verifies, so a forged high counter with a bad tag cannot lock out honest datagrams.

use aes_gcm::aead::generic_array::typenum::Unsigned;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, KeySizeUser, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};

use crate::{
  ControlDatagram, FrameError, PROLOGUE_BYTES, PROTOCOL_VERSION, Reader, write_prologue,
};

/// The seal key size in bytes, read from the cipher: AES-256 takes a 256-bit (32-byte) key. Derived
/// from `Aes256Gcm`'s own `KeySize`, so it is the cipher's fact, not a literal — the key schedule
/// produces exactly this many bytes.
pub const KEY_BYTES: usize = <Aes256Gcm as KeySizeUser>::KeySize::USIZE;

/// The additional-authenticated-data length: the routing prologue without the framing `sealed_len`
/// (version, sender, key epoch). These identity-bearing fields are authenticated by the seal; the
/// `sealed_len` is pure framing the decoder bounds-checks and the tag over the ciphertext covers.
const AAD_BYTES: usize = PROLOGUE_BYTES - size_of::<u32>();

/// The AES-GCM nonce as the cipher sizes it (96 bits): `counter ‖ channel`.
type SealNonce = Nonce<<Aes256Gcm as AeadCore>::NonceSize>;

/// A refusal on the sealed path (§4.10a §7): the closed set of ways a seal or an open can fail. Every
/// one is a corrupt, forged, replayed or exhausted datagram — never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealError {
  /// The sealer's 64-bit nonce counter is exhausted; the key must be rotated (never reached in
  /// practice — 2^64 datagrams — but refused rather than wrapped, which would reuse a nonce).
  CounterExhausted,
  /// The opener already accepted this counter (a replay, or an out-of-order redelivery on a
  /// drop-older channel).
  Replay,
  /// The AEAD tag did not verify: a tampered datagram, the wrong key, or a forged prologue (the
  /// routing fields are bound as AAD).
  BadSeal,
  /// The datagram ended before the prologue, the wire counter, or the ciphertext could be read.
  Truncated,
  /// The version byte was not one this build decodes.
  BadVersion,
  /// Bytes remained after the datagram.
  TrailingBytes,
  /// The decrypted sealed region did not parse as an envelope and body.
  Envelope(FrameError),
}

impl std::fmt::Display for SealError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      SealError::CounterExhausted => f.write_str("the sealer's nonce counter is exhausted"),
      SealError::Replay => f.write_str("the opener already accepted this counter"),
      SealError::BadSeal => f.write_str("the AEAD tag did not verify"),
      SealError::Truncated => f.write_str("the sealed datagram ended early"),
      SealError::BadVersion => f.write_str("the sealed datagram version is unknown"),
      SealError::TrailingBytes => f.write_str("the sealed datagram has trailing bytes"),
      SealError::Envelope(e) => write!(f, "the decrypted region is malformed: {e}"),
    }
  }
}

impl std::error::Error for SealError {}

/// The additive AEAD data for a datagram: the routing prologue minus its framing length, built the
/// same way on both ends so seal and open agree byte for byte. A tampered sender or key epoch changes
/// these bytes and the tag fails.
fn routing_aad(sender: u64, key_epoch: u32) -> [u8; AAD_BYTES] {
  let mut aad = [0u8; AAD_BYTES];
  aad[0] = PROTOCOL_VERSION;
  aad[size_of::<u8>()..size_of::<u8>() + size_of::<u64>()].copy_from_slice(&sender.to_le_bytes());
  aad[size_of::<u8>() + size_of::<u64>()..].copy_from_slice(&key_epoch.to_le_bytes());
  aad
}

/// Builds the 96-bit nonce for `counter` on `channel`: `counter` little-endian in the low 8 bytes,
/// `channel` little-endian in the high 4. A zeroed array of the cipher's nonce size, then filled, so
/// no width is written as a literal.
fn nonce_for(counter: u64, channel: u32) -> SealNonce {
  let mut nonce = SealNonce::default();
  nonce[..size_of::<u64>()].copy_from_slice(&counter.to_le_bytes());
  nonce[size_of::<u64>()..].copy_from_slice(&channel.to_le_bytes());
  nonce
}

/// The sending half of a keyed control channel: it seals plaintext under a counter nonce and advances
/// the counter once per datagram. One `Sealer` per (key, channel, direction).
pub struct Sealer {
  cipher: Aes256Gcm,
  channel: u32,
  counter: u64,
}

impl std::fmt::Debug for Sealer {
  /// Redacts the key: a sealer prints its channel and counter, never key material.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Sealer")
      .field("channel", &self.channel)
      .field("counter", &self.counter)
      .finish_non_exhaustive()
  }
}

impl Sealer {
  /// A sealer over `key` (exactly [`KEY_BYTES`] bytes) on `channel`, starting at counter zero.
  /// Infallible: the array length is the cipher's key size by construction, so the conversion cannot
  /// fail — no panic path.
  pub fn from_key(key: &[u8; KEY_BYTES], channel: u32) -> Sealer {
    let key: Key<Aes256Gcm> = (*key).into();
    Sealer {
      cipher: Aes256Gcm::new(&key),
      channel,
      counter: 0,
    }
  }

  /// Seals `plaintext` with `aad` bound in, returning the counter used (which travels on the wire so
  /// the opener can rebuild the nonce) and the ciphertext-with-tag. Advances the counter only on
  /// success; refuses when the nonce space is exhausted rather than wrapping (which would reuse a
  /// nonce — catastrophic for GCM).
  fn seal(&mut self, aad: &[u8], plaintext: &[u8]) -> Result<(u64, Vec<u8>), SealError> {
    let counter = self.counter;
    let advanced = counter.checked_add(1).ok_or(SealError::CounterExhausted)?;
    let nonce = nonce_for(counter, self.channel);
    let ciphertext = self
      .cipher
      .encrypt(
        &nonce,
        Payload {
          msg: plaintext,
          aad,
        },
      )
      .map_err(|_| SealError::BadSeal)?;
    self.counter = advanced;
    Ok((counter, ciphertext))
  }
}

/// The receiving half of a keyed control channel: it verifies and decrypts a sealed datagram and
/// refuses a counter it has already accepted. One `Opener` per (key, channel, direction).
pub struct Opener {
  cipher: Aes256Gcm,
  channel: u32,
  high_water: Option<u64>,
}

impl std::fmt::Debug for Opener {
  /// Redacts the key: an opener prints its channel and replay high-water, never key material.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Opener")
      .field("channel", &self.channel)
      .field("high_water", &self.high_water)
      .finish_non_exhaustive()
  }
}

impl Opener {
  /// An opener over `key` (exactly [`KEY_BYTES`] bytes) on `channel`, having accepted nothing yet.
  /// Infallible for the same reason as [`Sealer::from_key`].
  pub fn from_key(key: &[u8; KEY_BYTES], channel: u32) -> Opener {
    let key: Key<Aes256Gcm> = (*key).into();
    Opener {
      cipher: Aes256Gcm::new(&key),
      channel,
      high_water: None,
    }
  }

  /// Verifies and decrypts `ciphertext` sealed at `counter` with `aad`. Refuses a non-increasing
  /// counter (replay/drop-older) *before* the crypto, and advances the high-water only *after* the tag
  /// verifies, so a forged high counter with a bad tag cannot lock out honest datagrams.
  fn open(&mut self, counter: u64, aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, SealError> {
    if let Some(high_water) = self.high_water
      && counter <= high_water
    {
      return Err(SealError::Replay);
    }
    let nonce = nonce_for(counter, self.channel);
    let plaintext = self
      .cipher
      .decrypt(
        &nonce,
        Payload {
          msg: ciphertext,
          aad,
        },
      )
      .map_err(|_| SealError::BadSeal)?;
    self.high_water = Some(counter);
    Ok(plaintext)
  }
}

impl ControlDatagram {
  /// Seals this datagram: the cleartext prologue (version, sender, key epoch, sealed length) then the
  /// sealed region (the wire counter, then AES-256-GCM over the envelope-and-body plaintext with the
  /// routing prologue bound as AAD). The sender must present the `Sealer` whose key matches this
  /// datagram's `sender`/`key_epoch` (the keyring that enforces that is owed with enrollment).
  pub fn encode_sealed(&self, sealer: &mut Sealer) -> Result<Vec<u8>, SealError> {
    let mut plaintext = Vec::with_capacity(crate::ENVELOPE_BYTES + self.body.len());
    self.write_sealed_region(&mut plaintext);
    let aad = routing_aad(self.sender, self.key_epoch);
    let (counter, ciphertext) = sealer.seal(&aad, &plaintext)?;
    let sealed_len = size_of::<u64>() + ciphertext.len();
    let mut out = Vec::with_capacity(PROLOGUE_BYTES + sealed_len);
    write_prologue(&mut out, self.sender, self.key_epoch, sealed_len);
    out.extend_from_slice(&counter.to_le_bytes());
    out.extend_from_slice(&ciphertext);
    Ok(out)
  }

  /// Reads only the cleartext routing prologue — the sending node and its key epoch — without
  /// touching the sealed region or spending any crypto. The acceptance path uses this to find the
  /// key *before* it verifies, so an unknown sender is dropped for free (§4.10a §7 enforcement order).
  pub fn peek_routing(bytes: &[u8]) -> Result<(u64, u32), SealError> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|_| SealError::Truncated)? != PROTOCOL_VERSION {
      return Err(SealError::BadVersion);
    }
    let sender = reader.u64().map_err(|_| SealError::Truncated)?;
    let key_epoch = reader.u32().map_err(|_| SealError::Truncated)?;
    Ok((sender, key_epoch))
  }

  /// Decodes and opens a datagram from [`ControlDatagram::encode_sealed`]'s bytes. Every length is
  /// bounds-checked before it is read (a wild `sealed_len` never allocates), the tag is verified with
  /// the routing prologue as AAD, and any malformation, forgery or replay is a typed [`SealError`].
  /// The caller presents the `Opener` for the datagram's `sender`/`key_epoch`.
  pub fn decode_sealed(bytes: &[u8], opener: &mut Opener) -> Result<ControlDatagram, SealError> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|_| SealError::Truncated)? != PROTOCOL_VERSION {
      return Err(SealError::BadVersion);
    }
    let sender = reader.u64().map_err(|_| SealError::Truncated)?;
    let key_epoch = reader.u32().map_err(|_| SealError::Truncated)?;
    let sealed_len = reader.u32().map_err(|_| SealError::Truncated)? as usize;
    // The sealed region must at least hold the wire counter, and must fit the bytes that remain.
    if sealed_len < size_of::<u64>() || sealed_len > reader.remaining() {
      return Err(SealError::Truncated);
    }
    let counter = reader.u64().map_err(|_| SealError::Truncated)?;
    let ciphertext = reader
      .bytes(sealed_len - size_of::<u64>())
      .map_err(|_| SealError::Truncated)?;
    if !reader.is_empty() {
      return Err(SealError::TrailingBytes);
    }
    let aad = routing_aad(sender, key_epoch);
    let plaintext = opener.open(counter, &aad, ciphertext)?;
    let (envelope, body) =
      ControlDatagram::parse_sealed_region(&plaintext).map_err(SealError::Envelope)?;
    Ok(ControlDatagram {
      sender,
      key_epoch,
      envelope,
      body,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Envelope;

  /// A distinct, non-trivial key so a wrong-key open is a real test (not all-zeros).
  fn key(seed: u8) -> [u8; KEY_BYTES] {
    let mut k = [0u8; KEY_BYTES];
    let mut index: u8 = 0;
    for b in k.iter_mut() {
      *b = seed ^ index.wrapping_mul(31);
      index = index.wrapping_add(1);
    }
    k
  }

  fn sample() -> ControlDatagram {
    ControlDatagram {
      sender: 0x0102_0304_0506_0708,
      key_epoch: 7,
      envelope: Envelope {
        kind: 3,
        class: 1,
        flags: 0,
        epoch: 42,
        hlc: 0xDEAD_BEEF,
        request_id: 99,
      },
      body: b"a canonical body".to_vec(),
    }
  }

  const CHANNEL: u32 = 0x0000_0001;

  /// The nonce and key sizes this module lays out by hand match the cipher's own sizes, so the
  /// hand-built nonce (`counter ‖ channel`) exactly fills the AEAD nonce and the key array is the key.
  #[test]
  fn the_layout_matches_the_cipher() {
    assert_eq!(
      <Aes256Gcm as AeadCore>::NonceSize::USIZE,
      size_of::<u64>() + size_of::<u32>(),
      "the 96-bit nonce is counter(8) ‖ channel(4)"
    );
    assert_eq!(KEY_BYTES, <Aes256Gcm as KeySizeUser>::KeySize::USIZE);
  }

  /// A sealed datagram round-trips through encode_sealed/decode_sealed exactly.
  #[test]
  fn a_sealed_datagram_round_trips() {
    let datagram = sample();
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let mut opener = Opener::from_key(&key(1), CHANNEL);
    let wire = datagram.encode_sealed(&mut sealer).unwrap();
    assert_eq!(
      ControlDatagram::decode_sealed(&wire, &mut opener),
      Ok(datagram)
    );
  }

  /// The sealed region hides the plaintext: the envelope and body bytes do not appear on the wire.
  #[test]
  fn the_sealed_region_is_not_plaintext() {
    let datagram = sample();
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let wire = datagram.encode_sealed(&mut sealer).unwrap();
    // The body's cleartext must not be found in the sealed datagram.
    assert!(
      wire
        .windows(datagram.body.len())
        .all(|w| w != &datagram.body[..]),
      "the body bytes leaked into the sealed datagram"
    );
    // And the sealed form differs from the plaintext codec's bytes.
    assert_ne!(wire, datagram.encode());
  }

  /// Tampering with a ciphertext byte is caught by the tag (BadSeal), never decoded.
  #[test]
  fn a_tampered_ciphertext_is_refused() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let mut opener = Opener::from_key(&key(1), CHANNEL);
    let mut wire = sample().encode_sealed(&mut sealer).unwrap();
    let last = wire.len() - 1;
    wire[last] ^= 1;
    assert_eq!(
      ControlDatagram::decode_sealed(&wire, &mut opener),
      Err(SealError::BadSeal)
    );
  }

  /// Forging the cleartext sender (bound as AAD) breaks the tag, so a routing header cannot be swapped.
  #[test]
  fn a_forged_prologue_is_refused() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let mut opener = Opener::from_key(&key(1), CHANNEL);
    let mut wire = sample().encode_sealed(&mut sealer).unwrap();
    // Flip a byte of the sender field (offset: right after the version byte).
    wire[size_of::<u8>()] ^= 1;
    assert_eq!(
      ControlDatagram::decode_sealed(&wire, &mut opener),
      Err(SealError::BadSeal)
    );
  }

  /// A different key cannot open the datagram (BadSeal), never a wrong plaintext.
  #[test]
  fn a_wrong_key_is_refused() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let mut opener = Opener::from_key(&key(2), CHANNEL);
    let wire = sample().encode_sealed(&mut sealer).unwrap();
    assert_eq!(
      ControlDatagram::decode_sealed(&wire, &mut opener),
      Err(SealError::BadSeal)
    );
  }

  /// The counter increments per seal and travels on the wire, and a replayed datagram is refused.
  #[test]
  fn counters_increment_and_replays_are_refused() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let mut opener = Opener::from_key(&key(1), CHANNEL);
    let first = sample().encode_sealed(&mut sealer).unwrap();
    let second = sample().encode_sealed(&mut sealer).unwrap();
    // The wire counter sits right after the prologue; the two datagrams carry 0 then 1.
    assert_eq!(
      &first[PROLOGUE_BYTES..PROLOGUE_BYTES + size_of::<u64>()],
      &0u64.to_le_bytes()
    );
    assert_eq!(
      &second[PROLOGUE_BYTES..PROLOGUE_BYTES + size_of::<u64>()],
      &1u64.to_le_bytes()
    );
    // The opener accepts the first, then refuses the same bytes replayed.
    assert!(ControlDatagram::decode_sealed(&first, &mut opener).is_ok());
    assert_eq!(
      ControlDatagram::decode_sealed(&first, &mut opener),
      Err(SealError::Replay)
    );
  }

  /// The opener is drop-older: once it accepts counter 1, an earlier counter 0 is a replay.
  #[test]
  fn an_out_of_order_earlier_counter_is_dropped() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let mut opener = Opener::from_key(&key(1), CHANNEL);
    let first = sample().encode_sealed(&mut sealer).unwrap();
    let second = sample().encode_sealed(&mut sealer).unwrap();
    assert!(ControlDatagram::decode_sealed(&second, &mut opener).is_ok());
    assert_eq!(
      ControlDatagram::decode_sealed(&first, &mut opener),
      Err(SealError::Replay)
    );
  }

  /// A bad tag on a high counter does not advance the replay high-water (no lock-out DoS).
  #[test]
  fn a_forged_high_counter_does_not_lock_out_honest_datagrams() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let mut opener = Opener::from_key(&key(1), CHANNEL);
    let good = sample().encode_sealed(&mut sealer).unwrap();
    // A forged datagram claiming a very high counter but with a bad tag.
    let mut forged = good.clone();
    forged[PROLOGUE_BYTES..PROLOGUE_BYTES + size_of::<u64>()]
      .copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
      ControlDatagram::decode_sealed(&forged, &mut opener),
      Err(SealError::BadSeal)
    );
    // The honest datagram (counter 0) still opens: the forged high counter did not advance the window.
    assert!(ControlDatagram::decode_sealed(&good, &mut opener).is_ok());
  }

  /// An exhausted nonce counter is refused rather than wrapped (which would reuse a nonce).
  #[test]
  fn an_exhausted_counter_is_refused() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    sealer.counter = u64::MAX;
    assert_eq!(
      sample().encode_sealed(&mut sealer),
      Err(SealError::CounterExhausted)
    );
  }

  /// Hostile sealed inputs decode to a typed refusal, never a panic.
  #[test]
  fn hostile_sealed_datagrams_refuse_by_type() {
    let mut sealer = Sealer::from_key(&key(1), CHANNEL);
    let good = sample().encode_sealed(&mut sealer).unwrap();
    let opener = || Opener::from_key(&key(1), CHANNEL);

    assert_eq!(
      ControlDatagram::decode_sealed(&[], &mut opener()),
      Err(SealError::Truncated)
    );
    assert_eq!(
      ControlDatagram::decode_sealed(&good[..PROLOGUE_BYTES], &mut opener()),
      Err(SealError::Truncated)
    );
    // A wrong version.
    let mut bad_version = good.clone();
    bad_version[0] = PROTOCOL_VERSION.wrapping_add(1);
    assert_eq!(
      ControlDatagram::decode_sealed(&bad_version, &mut opener()),
      Err(SealError::BadVersion)
    );
    // Trailing bytes past the datagram.
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(
      ControlDatagram::decode_sealed(&trailing, &mut opener()),
      Err(SealError::TrailingBytes)
    );
    // A truncated tail loses the tag: truncated framing, or a failed tag — both typed, never a panic.
    let torn = &good[..good.len() - 1];
    assert!(matches!(
      ControlDatagram::decode_sealed(torn, &mut opener()),
      Err(SealError::Truncated) | Err(SealError::BadSeal)
    ));
  }

  /// The seal is deterministic: the same key, channel, counter, prologue and plaintext produce the
  /// same bytes on every build. A golden vector pins the wire so a cipher or layout change is caught.
  #[test]
  fn a_sealed_datagram_matches_its_golden_vector() {
    let datagram = sample();
    let mut a = Sealer::from_key(&key(1), CHANNEL);
    let mut b = Sealer::from_key(&key(1), CHANNEL);
    let one = datagram.encode_sealed(&mut a).unwrap();
    let two = datagram.encode_sealed(&mut b).unwrap();
    assert_eq!(one, two, "the seal is deterministic under counter nonces");
    // The golden hex (filled from the first run below); a change here means the wire changed.
    let golden = GOLDEN_SEALED;
    assert_eq!(
      hex(&one),
      golden,
      "sealed wire drifted from the golden vector"
    );
  }

  /// Lowercase hex of `bytes`, for the golden vector.
  fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * size_of::<u16>());
    for b in bytes {
      s.push_str(&format!("{b:02x}"));
    }
    s
  }

  /// The golden sealed datagram for `sample()` under `key(1)`, channel 1, counter 0. Prologue
  /// `01`(version) `0807060504030201`(sender) `07000000`(key epoch 7) `43000000`(sealed_len 67 =
  /// 8 counter + 27 envelope + 16 body + 16 tag), then `00…`(counter 0) and the 59-byte ciphertext.
  const GOLDEN_SEALED: &str = "010807060504030201070000004300000000000000000000006a90f14423badfe55e958e35cf2899e7381e7562f3873dfacd1e78c6414e761e358e92dc51100c579cb84cec8e93b11ce18e7b5ce1062c0b5b5e66";
}
