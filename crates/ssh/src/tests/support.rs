//! What the tests share (A-594): hex, and entropy that is deterministic, scripted or failing, so a whole exchange runs
//! from known bytes and a golden vector can use the document's own key or padding.

use std::collections::VecDeque;

use crate::entropy::{Entropy, EntropyFailed};

/// The bytes `text` spells in hex, whitespace ignored.
pub(crate) fn hex(text: &str) -> Vec<u8> {
  let digits: Vec<u8> = text
    .bytes()
    .filter(|byte| !byte.is_ascii_whitespace())
    .collect();
  assert!(digits.len() % 2 == 0, "odd number of hex digits");
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
    let value = ALPHABET
      .iter()
      .position(|letter| *letter == character)
      .expect("a base64 character") as u32;
    word = (word << 6) | value;
    bits += 6;
    if bits >= 8 {
      bits -= 8;
      bytes.push((word >> bits) as u8);
      word &= (1 << bits) - 1;
    }
  }
  bytes
}
