//! MD5 (RFC 1321) with its chaining state in the open, for CRAM-MD5's keyed contexts alone (A-692; RFC 2104 §4, RFC
//! 2195 §4). A server that keeps HMAC-MD5's contexts in place of a CRAM-MD5 secret finishes each digest from a state it
//! kept: MD5's state after the key's inner pad block, and after its outer pad block. RustCrypto's `md-5` 0.10, which
//! A-599 took for HMAC-MD5, keeps its state private (`Md5Core`'s fields and its `compress` module are not public), and
//! the vendored aws-lc-rs digests whole messages alone, so the compression and the padding are written here from RFC
//! 1321 §3. The handshake's tests hold them to RFC 1321's test suite, RFC 2202's HMAC-MD5 cases, contexts derived by a
//! separate implementation from the RFC, and both libraries' MD5 over generated messages (`md-5` and aws-lc-rs, the
//! oracles).
//!
//! No I/O, no allocation, no panic: every index is an iterator's or a checked one, and the arithmetic is MD5's own,
//! modulo 2^32 for the words and modulo 2^64 for the message's length in bits (RFC 1321 §3.2).

use super::DIGEST_BYTES;

/// An MD5 chaining state: the words A, B, C and D (RFC 1321 §3.3).
pub(super) type State = [u32; 4];

/// Format: MD5's block, the bytes one compression takes (RFC 1321 §3.4: sixteen 32-bit words).
pub(super) const BLOCK_BYTES: usize = 64;

/// Format: the words of a block (RFC 1321 §3.4: X\[0\] to X\[15\]).
const BLOCK_WORDS: usize = 16;

/// Format: the bytes of a word, read and written low-order byte first (RFC 1321 §2).
const WORD_BYTES: usize = 4;

/// Format: the chaining words' initial values (RFC 1321 §3.3: A = 01 23 45 67, B = 89 ab cd ef, C = fe dc ba 98,
/// D = 76 54 32 10, each listed low-order byte first).
pub(super) const INITIAL: State = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// Format: the sine table T\[1\] to T\[64\], sixteen to a round (RFC 1321 §3.4: T\[i\] is the integer part of
/// 4294967296 times abs(sin(i)), i in radians), as the RFC lists it.
const SINES: [[u32; BLOCK_WORDS]; 4] = [
  [
    // Format: T[1] to T[4].
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    // Format: T[5] to T[8].
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    // Format: T[9] to T[12].
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    // Format: T[13] to T[16].
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
  ],
  [
    // Format: T[17] to T[20].
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    // Format: T[21] to T[24].
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    // Format: T[25] to T[28].
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    // Format: T[29] to T[32].
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
  ],
  [
    // Format: T[33] to T[36].
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    // Format: T[37] to T[40].
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    // Format: T[41] to T[44].
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    // Format: T[45] to T[48].
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
  ],
  [
    // Format: T[49] to T[52].
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    // Format: T[53] to T[56].
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    // Format: T[57] to T[60].
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    // Format: T[61] to T[64].
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
  ],
];

/// Format: each round's four left rotations, which its steps take in turn (RFC 1321 §3.4: S11 to S14, S21 to S24, S31
/// to S34, S41 to S44).
const ROTATIONS: [[u32; 4]; 4] = [
  [7, 12, 17, 22],
  [5, 9, 14, 20],
  // Format: S31 to S34, then S41 to S44.
  [4, 11, 16, 23],
  [6, 10, 15, 21],
];

/// Format: each round's first message word and the stride between the words its steps read, modulo the block's
/// sixteen (RFC 1321 §3.4: round 1 reads X\[i\], round 2 X\[1 + 5i\], round 3 X\[5 + 3i\] and round 4 X\[7i\]).
const WORD_ORDER: [(usize, usize); 4] = [(0, 1), (1, 5), (5, 3), (0, 7)];

/// Format: the padding's first byte, a single 1 bit (RFC 1321 §3.1).
const PAD_START: u8 = 0x80;

/// Format: the bytes the message's length takes at the end of the padding (RFC 1321 §3.2: 64 bits, low-order byte
/// first).
const LENGTH_BYTES: usize = 8;

/// Format: the bits a byte holds (RFC 1321 §3.2 counts the message's length in bits).
const BITS_PER_BYTE: u64 = 8;

/// One compression of `block` into `state` (RFC 1321 §3.4): four rounds of sixteen steps over the block's words, then
/// each working word added to the state's.
pub(super) fn compress(state: &mut State, block: &[u8; BLOCK_BYTES]) {
  let mut words = [0u32; BLOCK_WORDS];
  for (word, bytes) in words.iter_mut().zip(block.as_chunks::<WORD_BYTES>().0) {
    *word = u32::from_le_bytes(*bytes);
  }
  let mut working = *state;
  let rounds = SINES.iter().zip(ROTATIONS).zip(WORD_ORDER);
  for (round, ((sines, rotations), order)) in rounds.enumerate() {
    for (step, (sine, rotation)) in sines.iter().zip(rotations.iter().cycle()).enumerate() {
      let word = words.get(word_index(order, step)).copied().unwrap_or(0);
      working = stepped(round, working, word.wrapping_add(*sine), *rotation);
    }
  }
  for (held, added) in state.iter_mut().zip(working) {
    *held = held.wrapping_add(added);
  }
}

/// The message word step `step` of a round reads, from the round's first word and stride ([`WORD_ORDER`]).
fn word_index((first, stride): (usize, usize), step: usize) -> usize {
  first.wrapping_add(stride.wrapping_mul(step)) % BLOCK_WORDS
}

/// One step of round `round` over the working words `[a, b, c, d]` (RFC 1321 §3.4): `a` becomes `b` plus
/// `(a + F(b, c, d) + X[k] + T[i]) <<< s`, `added` being `X[k] + T[i]` and `rotation` being `s`, and the words turn so
/// the next step's `a` is this one's `d`.
fn stepped(round: usize, [a, b, c, d]: State, added: u32, rotation: u32) -> State {
  let mixed = match round {
    0 => (b & c) | (!b & d),
    1 => (b & d) | (c & !d),
    2 => b ^ c ^ d,
    _ => c ^ (b | !d),
  };
  let turned = a
    .wrapping_add(mixed)
    .wrapping_add(added)
    .rotate_left(rotation);
  [d, b.wrapping_add(turned), b, c]
}

/// The digest of `message` finished from `state`, which has absorbed `absorbed` bytes in whole blocks (RFC 1321 §3.1
/// to §3.5): the message's whole blocks compressed, then its tail with the padding's one bit, zeros and the length of
/// everything absorbed in bits, in one block or two; the digest is the state's words, each low-order byte first.
pub(super) fn finish(mut state: State, absorbed: u64, message: &[u8]) -> [u8; DIGEST_BYTES] {
  let (blocks, tail) = message.as_chunks::<BLOCK_BYTES>();
  for block in blocks {
    compress(&mut state, block);
  }
  let length = u64::try_from(message.len()).unwrap_or(u64::MAX);
  let bits = absorbed.wrapping_add(length).wrapping_mul(BITS_PER_BYTE);
  let mut last = [0u8; 2 * BLOCK_BYTES];
  let padded = tail.iter().chain(std::iter::once(&PAD_START));
  for (slot, byte) in last.iter_mut().zip(padded) {
    *slot = *byte;
  }
  let used = if tail.len() < BLOCK_BYTES - LENGTH_BYTES {
    BLOCK_BYTES
  } else {
    2 * BLOCK_BYTES
  };
  if let Some(field) = last.get_mut(used.saturating_sub(LENGTH_BYTES)..used) {
    for (slot, byte) in field.iter_mut().zip(bits.to_le_bytes()) {
      *slot = byte;
    }
  }
  let ending = last.get(..used).unwrap_or_default();
  for block in ending.as_chunks::<BLOCK_BYTES>().0 {
    compress(&mut state, block);
  }
  bytes_of(state)
}

/// The MD5 digest of `message` (RFC 1321): [`finish`] from the initial state.
pub(super) fn digest(message: &[u8]) -> [u8; DIGEST_BYTES] {
  finish(INITIAL, 0, message)
}

/// A state's words as bytes, each low-order byte first (RFC 1321 §3.5): a digest, or a kept context's half.
pub(super) fn bytes_of(state: State) -> [u8; DIGEST_BYTES] {
  let mut bytes = [0u8; DIGEST_BYTES];
  for (chunk, word) in bytes.as_chunks_mut::<WORD_BYTES>().0.iter_mut().zip(state) {
    *chunk = word.to_le_bytes();
  }
  bytes
}

/// The state whose words `bytes` holds, each low-order byte first: [`bytes_of`]'s inverse.
pub(super) fn state_of(bytes: &[u8; DIGEST_BYTES]) -> State {
  let mut state = [0u32; 4];
  for (word, chunk) in state.iter_mut().zip(bytes.as_chunks::<WORD_BYTES>().0) {
    *word = u32::from_le_bytes(*chunk);
  }
  state
}
