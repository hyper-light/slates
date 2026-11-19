//! What the tests share (A-594): hex, and entropy that is deterministic, scripted or failing, so a whole exchange runs
//! from known bytes and a golden vector can use the document's own key or padding.

use std::collections::VecDeque;

use aws_lc_rs::kem::{AlgorithmId, DecapsulationKey, ML_KEM_768};

use crate::entropy::{Entropy, EntropyFailed};

/// The bytes `text` spells in hex, whitespace ignored.
pub(crate) fn hex(text: &str) -> Vec<u8> {
  let digits: Vec<u8> = text
    .bytes()
    .filter(|byte| !byte.is_ascii_whitespace())
    .collect();
  assert!(digits.len().is_multiple_of(2), "odd number of hex digits");
  digits
    .chunks(2)
    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
    .collect()
}

/// `N` bytes from hex.
pub(crate) fn hex_array<const N: usize>(text: &str) -> [u8; N] {
  hex(text).try_into().unwrap()
}

/// A deterministic stream: xorshift64* (Vigna, "An experimental exploration of Marsaglia's xorshift generators,
/// scrambled", 2016), seeded.
#[derive(Clone, Debug)]
pub(crate) struct Seeded {
  state: u64,
}

impl Seeded {
  pub(crate) fn new(seed: u64) -> Self {
    Self { state: seed.max(1) }
  }

  fn next_word(&mut self) -> u64 {
    self.state ^= self.state >> 12;
    self.state ^= self.state << 25;
    self.state ^= self.state >> 27;
    self.state.wrapping_mul(0x2545_f491_4f6c_dd1d)
  }
}

impl Entropy for Seeded {
  fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyFailed> {
    for byte in out {
      *byte = (self.next_word() >> 56) as u8;
    }
    Ok(())
  }
}

/// `script`'s bytes first, then a seeded stream.
#[derive(Clone, Debug)]
pub(crate) struct Scripted {
  script: VecDeque<u8>,
  then: Seeded,
}

impl Scripted {
  pub(crate) fn new(script: &[u8]) -> Self {
    Self {
      script: script.iter().copied().collect(),
      then: Seeded::new(7),
    }
  }
}

impl Entropy for Scripted {
  fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyFailed> {
    for byte in out.iter_mut() {
      match self.script.pop_front() {
        Some(scripted) => *byte = scripted,
        None => self.then.fill(std::slice::from_mut(byte))?,
      }
    }
    Ok(())
  }
}

/// Entropy that fails every draw after `allowed` successful ones.
#[derive(Clone, Debug)]
pub(crate) struct FailingAfter {
  allowed: usize,
  inner: Seeded,
}

impl FailingAfter {
  pub(crate) fn new(allowed: usize) -> Self {
    Self {
      allowed,
      inner: Seeded::new(11),
    }
  }
}

impl Entropy for FailingAfter {
  fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyFailed> {
    if self.allowed == 0 {
      return Err(EntropyFailed);
    }
    self.allowed -= 1;
    self.inner.fill(out)
  }
}

/// The bytes `text` spells in RFC 4648 §4's base64, whitespace and padding ignored.
pub(crate) fn base64(text: &str) -> Vec<u8> {
  const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut bytes = Vec::new();
  let mut word = 0u32;
  let mut bits = 0u32;
  for character in text
    .bytes()
    .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
  {
    let position = ALPHABET
      .iter()
      .position(|letter| *letter == character)
      .expect("a base64 character");
    let value = u32::try_from(position).unwrap();
    word = (word << 6) | value;
    bits += 6;
    if bits >= 8 {
      bits -= 8;
      bytes.push(u8::try_from((word >> bits) & 0xff).unwrap());
      word &= (1 << bits) - 1;
    }
  }
  bytes
}

/// A fresh ML-KEM-768 key pair from AWS-LC: the key that decapsulates, and the encapsulation key's bytes a client sends
/// as `C_PK2` (FIPS 203 §7.1).
pub(crate) fn mlkem_key_pair() -> (DecapsulationKey<AlgorithmId>, Vec<u8>) {
  let key = DecapsulationKey::generate(&ML_KEM_768).unwrap();
  let encapsulation = key
    .encapsulation_key()
    .unwrap()
    .key_bytes()
    .unwrap()
    .as_ref()
    .to_vec();
  (key, encapsulation)
}

/// `key` with twelve-bit coefficient `index` of its vector `t̂` set to `value`, laid out as FIPS 203 §4.2.1's
/// `ByteEncode12` lays out each pair: the first coefficient in byte 3j and the low half of byte 3j + 1, the second in the
/// high half of byte 3j + 1 and in byte 3j + 2.
pub(crate) fn with_coefficient(key: &[u8], index: usize, value: u16) -> Vec<u8> {
  let mut out = key.to_vec();
  let at = index / 2 * 3;
  let [low, high] = value.to_le_bytes();
  if index.is_multiple_of(2) {
    out[at] = low;
    out[at + 1] = (out[at + 1] & 0xf0) | (high & 0x0f);
  } else {
    out[at + 1] = (out[at + 1] & 0x0f) | (low << 4);
    out[at + 2] = u8::try_from(value >> 4).unwrap();
  }
  out
}
