//! The binary packet protocol (A-594; RFC 4253 §6; RFC 5647 §7; `draft-ietf-sshm-chacha20-poly1305-04` §6, §7).
//!
//! A packet is `uint32 packet_length, byte padding_length, payload, random padding`, then the tag. Before the first
//! `SSH_MSG_NEWKEYS` nothing is encrypted and there is no tag; after it, each direction runs one of two AEAD ciphers:
//! - `chacha20-poly1305@openssh.com`: the length is encrypted under the key's second half, the rest under its first
//!   half, and Poly1305 tags both; the nonce is the packet's sequence number (the draft §6). The length is decrypted alone
//!   first, as the draft's §7 says ("When 4 bytes of ciphertext length have been received, they may be decrypted"); it
//!   is checked before the body is read, and the tag before the body is decrypted ("the MAC MUST be checked before
//!   decryption").
//! - `aes256-gcm@openssh.com`: the length is the additional authenticated data, the rest is encrypted (RFC 5647 §7.3),
//!   and the nonce is a 4-byte fixed field and an 8-byte invocation counter taken from the derived IV and counted up one
//!   per packet (§7.1).
//!
//! **What is checked, before what.** From its first four bytes ([`Opener::frame`]): a length too short to hold the
//! padding byte, four bytes of padding and a payload byte; a length not aligned to the cipher's block, which RFC 4253 §6
//! says "MUST be enforced, even when using stream ciphers"; and a packet past the caller's cap, refused before a byte of
//! its body is read. Then, whole ([`Opener::open`]): the tag, and the padding, "at least four bytes" and leaving a payload.
//! The alignment is the rule OpenSSH applies (`packet.c` at `fa4b2a82`) \[C\]: in the clear, the whole packet is a multiple
//! of 8; under an AEAD, the part after the length is a multiple of the cipher's block, 16 for AES and 8 for ChaCha20,
//! whose length field is not part of what is padded.
//!
//! **What is sent.** The least padding that aligns the packet and is at least four bytes, drawn from the caller's
//! [`Entropy`], and no packet larger than RFC 4253 §6.1's 35000 bytes, the size every implementation "MUST be able to
//! process".
//!
//! **Usage.** Each direction counts the packets and the block-cipher blocks its key has processed, which [`crate::keylife`]
//! bounds. Sequence numbers count every packet and wrap at 2^32 (RFC 4253 §6.4); the transport resets them where strict key
//! exchange says.

use core::ops::Range;

use aws_lc_rs::aead::chacha20_poly1305_openssh::{
  KEY_LEN as CHACHA_KEY_LEN, OpeningKey as ChaChaOpening, SealingKey as ChaChaSealing,
  TAG_LEN as CHACHA_TAG_LEN,
};
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};

use crate::entropy::Entropy;
use crate::to_u64;

/// Format: the largest packet every implementation must accept, in bytes on the wire, tag included: RFC 4253 §6.1, "All
/// implementations MUST be able to process packets with ... a total packet size of 35000 bytes or less". The server sends
/// nothing larger, and accepts no cap below it.
pub const MAX_PACKET_EVERY_PEER_ACCEPTS: usize = 35_000;

/// Format: the bytes of the length field.
pub const LENGTH_LEN: usize = 4;

/// Format: the fewest padding bytes a packet carries: RFC 4253 §6, "There MUST be at least four bytes of padding".
pub const MIN_PADDING: usize = 4;

/// Format: the alignment in the clear: RFC 4253 §6, "a multiple of the cipher block size or 8, whichever is larger".
pub const CLEAR_ALIGNMENT: usize = 8;

/// The tag of both ciphers: GCM's full 16 bytes (RFC 5647 §6.3, "MUST use the full 16-octet Authentication Tag") and
/// Poly1305's (the chacha draft §4).
pub const TAG_LEN: usize = CHACHA_TAG_LEN;

/// Format: AES's block, 128 bits (FIPS 197), to which AES-GCM's plaintext is padded (RFC 5647 §7.1).
pub const AES_BLOCK_LEN: usize = 16;

/// The padding block of `chacha20-poly1305@openssh.com`: as a stream cipher it has none of its own, so RFC 4253 §6's
/// floor of 8 applies, as OpenSSH's cipher table gives it (`cipher.c`) \[C\].
pub const CHACHA_ALIGNMENT: usize = CLEAR_ALIGNMENT;

/// Format: the bytes of AES-256's key (RFC 5647 §6.2, `K_LEN` 32).
pub const GCM_KEY_LEN: usize = 32;

/// The bytes of AES-GCM's IV (RFC 5647 §6.2, `N_MIN` = `N_MAX` = 12).
pub const GCM_IV_LEN: usize = NONCE_LEN;

/// Format: the bytes of the IV's fixed field; the rest is the invocation counter (RFC 5647 §7.1).
const GCM_FIXED_LEN: usize = 4;

/// The most padding a packet can carry: its `padding_length` is one byte.
const MAX_PADDING: usize = u8::MAX as usize;

/// An AEAD cipher a direction may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cipher {
  /// `chacha20-poly1305@openssh.com`.
  ChaCha20Poly1305,
  /// `aes256-gcm@openssh.com`.
  Aes256Gcm,
}

impl Cipher {
  /// The ciphers the server offers, in its order of preference.
  pub const OFFERED: [Cipher; 2] = [Cipher::ChaCha20Poly1305, Cipher::Aes256Gcm];

  /// The name a KEXINIT lists.
  pub fn name(self) -> &'static str {
    match self {
      Cipher::ChaCha20Poly1305 => "chacha20-poly1305@openssh.com",
      Cipher::Aes256Gcm => "aes256-gcm@openssh.com",
    }
  }

  /// The cipher a KEXINIT name stands for.
  pub fn from_name(name: &[u8]) -> Option<Self> {
    Self::OFFERED
      .into_iter()
      .find(|cipher| cipher.name().as_bytes() == name)
  }

  /// The key material a direction derives for it (RFC 4253 §7.2's "C" and "D").
  pub fn key_len(self) -> usize {
    match self {
      Cipher::ChaCha20Poly1305 => CHACHA_KEY_LEN,
      Cipher::Aes256Gcm => GCM_KEY_LEN,
    }
  }

  /// The IV a direction derives for it (RFC 4253 §7.2's "A" and "B"); ChaCha20 takes its nonce from the sequence number.
  pub fn iv_len(self) -> usize {
    match self {
      Cipher::ChaCha20Poly1305 => 0,
      Cipher::Aes256Gcm => GCM_IV_LEN,
    }
  }

  /// The block the part after the length is padded to.
  fn alignment(self) -> usize {
    match self {
      Cipher::ChaCha20Poly1305 => CHACHA_ALIGNMENT,
      Cipher::Aes256Gcm => AES_BLOCK_LEN,
    }
  }
}

/// What a key has processed since it was taken into use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
  /// Packets sealed or opened.
  pub packets: u64,
  /// Block-cipher blocks encrypted or decrypted: AES-GCM's padded plaintext over 16, and its tag's block. ChaCha20 is a
  /// stream cipher and counts none.
  pub blocks: u64,
}

impl Usage {
  /// This usage and one packet more of `blocks` blocks.
  pub fn after(self, blocks: u64) -> Usage {
    Usage {
      packets: self.packets.saturating_add(1),
      blocks: self.blocks.saturating_add(blocks),
    }
  }
}

/// Key material of the wrong length, or a key AWS-LC refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMaterialRefusal {
  /// The key is not the cipher's length.
  KeyLength,
  /// The IV is not the cipher's length.
  IvLength,
  /// AWS-LC refused the key.
  Rejected,
}

/// Why a packet was not sealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealRefusal {
  /// A packet carrying the payload would pass [`MAX_PACKET_EVERY_PEER_ACCEPTS`].
  TooLarge {
    /// The payload's bytes.
    payload: u64,
    /// The largest packet sent, on the wire.
    ceiling: u64,
  },
  /// The padding's random bytes could not be drawn.
  Entropy,
  /// AWS-LC refused to seal.
  Cipher,
}

/// Why a packet was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketRefusal {
  /// The length cannot hold the padding byte, four bytes of padding and one payload byte.
  LengthTooShort {
    /// The packet length read.
    length: u32,
  },
  /// The length is not aligned to the cipher's block.
  LengthUnaligned {
    /// The packet length read.
    length: u32,
    /// The block it must be a multiple of.
    alignment: u32,
  },
  /// The packet passes the cap.
  LengthPastCap {
    /// Its size on the wire, tag included.
    size: u64,
    /// The cap.
    cap: u64,
  },
  /// The bytes given are not the whole packet its length names.
  Incomplete {
    /// The bytes given.
    have: u64,
    /// The bytes the packet takes.
    need: u64,
  },
  /// The tag did not authenticate the packet.
  TagMismatch,
  /// Fewer than four bytes of padding.
  PaddingTooShort {
    /// The padding length read.
    padding: u8,
  },
  /// The padding leaves no payload.
  PaddingPastPacket {
    /// The padding length read.
    padding: u8,
    /// The packet length read.
    length: u32,
  },
}

/// AES-256-GCM with SSH's nonce: a fixed field and an invocation counter (RFC 5647 §7.1).
struct GcmKey {
  /// The key.
  key: LessSafeKey,
  /// The IV's first four bytes, fixed for the key's life.
  fixed: [u8; GCM_FIXED_LEN],
  /// The IV's last eight bytes, counted up one per packet.
  invocation: u64,
}

impl GcmKey {
  /// The key `key` with the derived IV `iv`.
  fn new(key: &[u8], iv: &[u8]) -> Result<Self, KeyMaterialRefusal> {
    if key.len() != GCM_KEY_LEN {
      return Err(KeyMaterialRefusal::KeyLength);
    }
    let iv: [u8; GCM_IV_LEN] = iv.try_into().map_err(|_| KeyMaterialRefusal::IvLength)?;
    let (fixed, invocation) = iv.split_at(GCM_FIXED_LEN);
    let fixed: [u8; GCM_FIXED_LEN] = fixed.try_into().map_err(|_| KeyMaterialRefusal::IvLength)?;
    // Format: the invocation counter's eight bytes, the IV's rest after its fixed field (RFC 5647 §7.1).
    let invocation: [u8; 8] = invocation
      .try_into()
      .map_err(|_| KeyMaterialRefusal::IvLength)?;
    let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| KeyMaterialRefusal::Rejected)?;
    Ok(Self {
      key: LessSafeKey::new(unbound),
      fixed,
      invocation: u64::from_be_bytes(invocation),
    })
  }

  /// This packet's nonce, and the counter moved on for the next. The counter wraps, as RFC 5647 §7.1's 64-bit integer
  /// does; a key never processes 2^64 packets (its limit is 2^32, [`crate::keylife`]), so no nonce repeats.
  fn next_nonce(&mut self) -> Nonce {
    let mut bytes = [0u8; GCM_IV_LEN];
    let (fixed, invocation) = bytes.split_at_mut(GCM_FIXED_LEN);
    fixed.copy_from_slice(&self.fixed);
    invocation.copy_from_slice(&self.invocation.to_be_bytes());
    self.invocation = self.invocation.wrapping_add(1);
    Nonce::assume_unique_for_key(bytes)
  }
}

/// How one direction seals.
enum SealKey {
  /// Before the first `SSH_MSG_NEWKEYS`.
  Clear,
  /// `chacha20-poly1305@openssh.com`.
  ChaCha(ChaChaSealing),
  /// `aes256-gcm@openssh.com`.
  Gcm(GcmKey),
}

/// How one direction opens.
enum OpenKey {
  /// Before the first `SSH_MSG_NEWKEYS`.
  Clear,
  /// `chacha20-poly1305@openssh.com`.
  ChaCha(ChaChaOpening),
  /// `aes256-gcm@openssh.com`.
  Gcm(GcmKey),
}

/// The 64 bytes of a ChaCha20-Poly1305 key, or a refusal.
fn chacha_material(key: &[u8], iv: &[u8]) -> Result<[u8; CHACHA_KEY_LEN], KeyMaterialRefusal> {
  if !iv.is_empty() {
    return Err(KeyMaterialRefusal::IvLength);
  }
  key.try_into().map_err(|_| KeyMaterialRefusal::KeyLength)
}

/// The sending half of a direction: its key, its sequence number and its key's usage.
pub struct Sealer {
  /// The key, or none before the first exchange.
  key: SealKey,
  /// The sequence number of the next packet.
  sequence: u32,
  /// What the key has sealed.
  usage: Usage,
}

impl core::fmt::Debug for Sealer {
  fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    formatter
      .debug_struct("Sealer")
      .field("cipher", &self.cipher())
      .field("sequence", &self.sequence)
      .field("usage", &self.usage)
      .finish()
  }
}

impl Sealer {
  /// The clear sealer a connection starts with.
  pub fn clear() -> Self {
    Self {
      key: SealKey::Clear,
      sequence: 0,
      usage: Usage::default(),
    }
  }

  /// A sealer running `cipher` under `key` and `iv`, at sequence number 0.
  pub fn new(cipher: Cipher, key: &[u8], iv: &[u8]) -> Result<Self, KeyMaterialRefusal> {
    let key = match cipher {
      Cipher::ChaCha20Poly1305 => SealKey::ChaCha(ChaChaSealing::new(&chacha_material(key, iv)?)),
      Cipher::Aes256Gcm => SealKey::Gcm(GcmKey::new(key, iv)?),
    };
    Ok(Self {
      key,
      sequence: 0,
      usage: Usage::default(),
    })
  }

  /// The cipher, or none in the clear.
  pub fn cipher(&self) -> Option<Cipher> {
    match self.key {
      SealKey::Clear => None,
      SealKey::ChaCha(_) => Some(Cipher::ChaCha20Poly1305),
      SealKey::Gcm(_) => Some(Cipher::Aes256Gcm),
    }
  }

  /// The sequence number the next packet is sealed under.
  pub fn sequence(&self) -> u32 {
    self.sequence
  }

  /// Sets the next packet's sequence number: strict key exchange's reset, and a carried-on count across new keys.
  pub fn set_sequence(&mut self, sequence: u32) {
    self.sequence = sequence;
  }

  /// What the key has sealed.
  pub fn usage(&self) -> Usage {
    self.usage
  }

  /// The usage after one more packet carrying `payload_len` bytes, what [`Sealer::seal`] would count; none if no packet
  /// can carry it.
  pub fn usage_after(&self, payload_len: usize) -> Option<Usage> {
    layout(self.cipher(), payload_len).map(|shape| self.usage.after(shape.blocks))
  }

  /// Seals `payload` as the next packet and appends it to `out`.
  pub fn seal<E: Entropy + ?Sized>(
    &mut self,
    payload: &[u8],
    entropy: &mut E,
    out: &mut Vec<u8>,
  ) -> Result<(), SealRefusal> {
    let shape = layout(self.cipher(), payload.len()).ok_or(SealRefusal::TooLarge {
      payload: to_u64(payload.len()),
      ceiling: to_u64(MAX_PACKET_EVERY_PEER_ACCEPTS),
    })?;
    let start = out.len();
    write_plain(&shape, payload, entropy, out)?;
    if let Err(refusal) = self.encrypt(&shape, start, out) {
      out.truncate(start);
      return Err(refusal);
    }
    self.usage = self.usage.after(shape.blocks);
    self.sequence = self.sequence.wrapping_add(1);
    Ok(())
  }

  /// Encrypts the plain packet at `out[start..]` in place and appends its tag.
  fn encrypt(&mut self, shape: &Shape, start: usize, out: &mut Vec<u8>) -> Result<(), SealRefusal> {
    match &mut self.key {
      SealKey::Clear => Ok(()),
      SealKey::ChaCha(key) => {
        let mut tag = [0u8; TAG_LEN];
        let region = out.get_mut(start..).ok_or(SealRefusal::Cipher)?;
        key.seal_in_place(self.sequence, region, &mut tag);
        out.extend_from_slice(&tag);
        Ok(())
      }
      SealKey::Gcm(key) => {
        let length = shape.packet_length.to_be_bytes();
        let body_start = start.checked_add(LENGTH_LEN).ok_or(SealRefusal::Cipher)?;
        let body = out.get_mut(body_start..).ok_or(SealRefusal::Cipher)?;
        let nonce = key.next_nonce();
        let tag = key
          .key
          .seal_in_place_separate_tag(nonce, Aad::from(length), body)
          .map_err(|_| SealRefusal::Cipher)?;
        out.extend_from_slice(tag.as_ref());
        Ok(())
      }
    }
  }
}

/// A packet's shape on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
  /// `packet_length`: the padding byte, the payload and the padding.
  packet_length: u32,
  /// The padding bytes.
  padding: u8,
  /// The block-cipher blocks it costs its key.
  blocks: u64,
}

/// The shape of a packet carrying `payload_len` bytes under `cipher` (none in the clear), or none if it would pass
/// [`MAX_PACKET_EVERY_PEER_ACCEPTS`].
fn layout(cipher: Option<Cipher>, payload_len: usize) -> Option<Shape> {
  let alignment = cipher.map_or(CLEAR_ALIGNMENT, Cipher::alignment);
  let length_padded = if cipher.is_some() { 0 } else { LENGTH_LEN };
  let aligned = payload_len.checked_add(1)?.checked_add(length_padded)?;
  let short = alignment.checked_sub(aligned.checked_rem(alignment)?)?;
  let padding = if short < MIN_PADDING {
    short.checked_add(alignment)?
  } else {
    short
  };
  let packet_length = payload_len.checked_add(1)?.checked_add(padding)?;
  let tag = if cipher.is_some() { TAG_LEN } else { 0 };
  let size = packet_length.checked_add(LENGTH_LEN)?.checked_add(tag)?;
  if size > MAX_PACKET_EVERY_PEER_ACCEPTS {
    return None;
  }
  let blocks = match cipher {
    Some(Cipher::Aes256Gcm) => to_u64(packet_length.checked_div(AES_BLOCK_LEN)?.checked_add(1)?),
    _ => 0,
  };
  Some(Shape {
    packet_length: u32::try_from(packet_length).ok()?,
    padding: u8::try_from(padding).ok()?,
    blocks,
  })
}

/// Appends the plain packet: length, padding length, payload, random padding.
fn write_plain<E: Entropy + ?Sized>(
  shape: &Shape,
  payload: &[u8],
  entropy: &mut E,
  out: &mut Vec<u8>,
) -> Result<(), SealRefusal> {
  let mut padding = [0u8; MAX_PADDING];
  let drawn = padding
    .get_mut(..usize::from(shape.padding))
    .ok_or(SealRefusal::Cipher)?;
  entropy.fill(drawn).map_err(|_| SealRefusal::Entropy)?;
  out.extend_from_slice(&shape.packet_length.to_be_bytes());
  out.push(shape.padding);
  out.extend_from_slice(payload);
  out.extend_from_slice(drawn);
  Ok(())
}

/// A packet opened: the sequence number it was opened under, and where its payload lies in the packet's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
  /// The packet's sequence number, which an `SSH_MSG_UNIMPLEMENTED` names (RFC 4253 §11.4).
  pub sequence: u32,
  /// The payload's range in the bytes given to [`Opener::open`].
  pub payload: Range<usize>,
}

/// The receiving half of a direction: its key, its sequence number and its key's usage.
pub struct Opener {
  /// The key, or none before the first exchange.
  key: OpenKey,
  /// The sequence number of the next packet.
  sequence: u32,
  /// What the key has opened.
  usage: Usage,
}

impl core::fmt::Debug for Opener {
  fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    formatter
      .debug_struct("Opener")
      .field("cipher", &self.cipher())
      .field("sequence", &self.sequence)
      .field("usage", &self.usage)
      .finish()
  }
}

impl Opener {
  /// The clear opener a connection starts with.
  pub fn clear() -> Self {
    Self {
      key: OpenKey::Clear,
      sequence: 0,
      usage: Usage::default(),
    }
  }

  /// An opener running `cipher` under `key` and `iv`, at sequence number 0.
  pub fn new(cipher: Cipher, key: &[u8], iv: &[u8]) -> Result<Self, KeyMaterialRefusal> {
    let key = match cipher {
      Cipher::ChaCha20Poly1305 => OpenKey::ChaCha(ChaChaOpening::new(&chacha_material(key, iv)?)),
      Cipher::Aes256Gcm => OpenKey::Gcm(GcmKey::new(key, iv)?),
    };
    Ok(Self {
      key,
      sequence: 0,
      usage: Usage::default(),
    })
  }

  /// The cipher, or none in the clear.
  pub fn cipher(&self) -> Option<Cipher> {
    match self.key {
      OpenKey::Clear => None,
      OpenKey::ChaCha(_) => Some(Cipher::ChaCha20Poly1305),
      OpenKey::Gcm(_) => Some(Cipher::Aes256Gcm),
    }
  }

  /// The sequence number the next packet is opened under.
  pub fn sequence(&self) -> u32 {
    self.sequence
  }

  /// Sets the next packet's sequence number.
  pub fn set_sequence(&mut self, sequence: u32) {
    self.sequence = sequence;
  }

  /// What the key has opened.
  pub fn usage(&self) -> Usage {
    self.usage
  }

  /// The length the packet starting `head` names, decrypted under ChaCha20's length key when that runs.
  fn packet_length(&self, head: [u8; LENGTH_LEN]) -> u32 {
    match &self.key {
      OpenKey::ChaCha(key) => u32::from_be_bytes(key.decrypt_packet_length(self.sequence, head)),
      OpenKey::Clear | OpenKey::Gcm(_) => u32::from_be_bytes(head),
    }
  }

  /// The size on the wire of the packet whose first four bytes are `head`, tag included, checked against the cipher's
  /// rules and against `cap` before any more of it is read.
  pub fn frame(&self, head: [u8; LENGTH_LEN], cap: usize) -> Result<usize, PacketRefusal> {
    let size = self.checked_size(head)?;
    if size > cap {
      return Err(PacketRefusal::LengthPastCap {
        size: to_u64(size),
        cap: to_u64(cap),
      });
    }
    Ok(size)
  }

  /// The size on the wire of the packet whose first four bytes are `head`, its length checked against the cipher's
  /// rules: long enough for the padding byte, four bytes of padding and a payload byte, and aligned.
  fn checked_size(&self, head: [u8; LENGTH_LEN]) -> Result<usize, PacketRefusal> {
    let length = self.packet_length(head);
    let cipher = self.cipher();
    let alignment = cipher.map_or(CLEAR_ALIGNMENT, Cipher::alignment);
    let fewest = to_u64(MIN_PADDING).saturating_add(2);
    if u64::from(length) < fewest {
      return Err(PacketRefusal::LengthTooShort { length });
    }
    let length_padded = if cipher.is_some() {
      0
    } else {
      to_u64(LENGTH_LEN)
    };
    let aligned = u64::from(length).saturating_add(length_padded);
    if aligned.checked_rem(to_u64(alignment)) != Some(0) {
      let alignment = u32::try_from(alignment).unwrap_or(u32::MAX);
      return Err(PacketRefusal::LengthUnaligned { length, alignment });
    }
    let tag = if cipher.is_some() { to_u64(TAG_LEN) } else { 0 };
    let size = u64::from(length)
      .saturating_add(to_u64(LENGTH_LEN))
      .saturating_add(tag);
    usize::try_from(size).map_err(|_| PacketRefusal::LengthPastCap {
      size,
      cap: to_u64(usize::MAX),
    })
  }

  /// Opens the whole packet `packet` (as long as [`Opener::frame`] said): authenticates it, decrypts it in place and
  /// checks its padding. The payload's range is returned, and the sequence number it was opened under.
  pub fn open(&mut self, packet: &mut [u8]) -> Result<Opened, PacketRefusal> {
    let head: [u8; LENGTH_LEN] = packet
      .get(..LENGTH_LEN)
      .and_then(|bytes| bytes.try_into().ok())
      .ok_or(PacketRefusal::Incomplete {
        have: to_u64(packet.len()),
        need: to_u64(LENGTH_LEN),
      })?;
    let size = self.checked_size(head)?;
    if size != packet.len() {
      return Err(PacketRefusal::Incomplete {
        have: to_u64(packet.len()),
        need: to_u64(size),
      });
    }
    let length = self.packet_length(head);
    let plain_end = LENGTH_LEN.saturating_add(usize::try_from(length).unwrap_or(usize::MAX));
    self.decrypt(packet, head, plain_end)?;
    let payload = payload_range(packet, length)?;
    let blocks = match self.cipher() {
      Some(Cipher::Aes256Gcm) => to_u64(
        plain_end
          .saturating_sub(LENGTH_LEN)
          .checked_div(AES_BLOCK_LEN)
          .unwrap_or(0),
      )
      .saturating_add(1),
      _ => 0,
    };
    let sequence = self.sequence;
    self.usage = self.usage.after(blocks);
    self.sequence = self.sequence.wrapping_add(1);
    Ok(Opened { sequence, payload })
  }

  /// Authenticates and decrypts `packet` in place; `plain_end` is where its tag starts.
  fn decrypt(
    &mut self,
    packet: &mut [u8],
    head: [u8; LENGTH_LEN],
    plain_end: usize,
  ) -> Result<(), PacketRefusal> {
    let mismatch = PacketRefusal::TagMismatch;
    match &mut self.key {
      OpenKey::Clear => Ok(()),
      OpenKey::ChaCha(key) => {
        let (sealed, tag) = packet.split_at_mut_checked(plain_end).ok_or(mismatch)?;
        let tag: &[u8; TAG_LEN] = (&*tag).try_into().map_err(|_| mismatch)?;
        key
          .open_in_place(self.sequence, sealed, tag)
          .map(|_| ())
          .map_err(|_| mismatch)
      }
      OpenKey::Gcm(key) => {
        let (sealed, tag) = packet.split_at_mut_checked(plain_end).ok_or(mismatch)?;
        let body = sealed.get_mut(LENGTH_LEN..).ok_or(mismatch)?;
        let nonce = key.next_nonce();
        let opened = key
          .key
          .open_in_place_separate_tag(nonce, Aad::from(head), tag, body);
        opened.map(|_| ()).map_err(|_| mismatch)
      }
    }
  }
}

/// Where the payload lies in an opened packet of `length`, its padding checked (RFC 4253 §6).
fn payload_range(packet: &[u8], length: u32) -> Result<Range<usize>, PacketRefusal> {
  let padding = packet
    .get(LENGTH_LEN)
    .copied()
    .ok_or(PacketRefusal::LengthTooShort { length })?;
  if usize::from(padding) < MIN_PADDING {
    return Err(PacketRefusal::PaddingTooShort { padding });
  }
  let payload_len = usize::try_from(length)
    .ok()
    .and_then(|whole| whole.checked_sub(1))
    .and_then(|rest| rest.checked_sub(usize::from(padding)))
    .filter(|payload_len| *payload_len > 0)
    .ok_or(PacketRefusal::PaddingPastPacket { padding, length })?;
  let start = LENGTH_LEN.saturating_add(1);
  Ok(start..start.saturating_add(payload_len))
}
