//! Hardware-accelerated SHA-1 with Marc Stevens collision detection (§4.17.2, §4.17.3; A-600, A-588, A-893).
//!
//! Both git's `sha1dc_git.c` (`git_SHA1DCInit`: detection on, safe hash off) and Mercurial 7.2's
//! `thirdparty/sha1dc/cext.c` (`SHA1DCSetSafeHash(&ctx, 0)`) hash objects with SHA-1 while detecting known
//! collision attacks (SHAttered and every other Marc Stevens disturbance vector [A: Stevens, "Counter-Cryptanalysis",
//! CRYPTO 2013; B: `cr-marcstevens/sha1collisiondetection`]), refusing any input whose blocks trigger a matching
//! recompression while naming the plain SHA-1 digest.
//!
//! **Design and evidence.** Profiling initial pushes of `git.git` (424,984 objects) and `hg-src` (57,036 changesets)
//! showed RustCrypto's scalar `sha1-checked` taking ~73–74% of all daemon CPU because it runs scalar Rust SHA-1
//! compression on every 64-byte block so intermediate states at rounds 58 and 65 are always available. Across the 32
//! Stevens disturbance vectors, however, the unavoidable bit conditions (`ubc_check`) pass on only ~2.5–3.5% of random
//! blocks (~97% return `ubc_mask == 0`).
//!
//! This module splits every 64-byte block along that boundary with **zero `unsafe`** in `slates`:
//! 1. [`ubc`] expands only `W[0..65]` (the highest word `ubc_check` reads is `W[64]`) four blocks at a time in
//!    4-lane `[u32; 4]` arrays so LLVM auto-vectorizes the message schedule and unconditional bit-condition filters
//!    into 128-bit SIMD.
//! 2. Contiguous zero-mask runs (~97% of blocks, averaging ~33 blocks / ~2 KiB per run so data stays L1-hot) feed
//!    directly into `sha1::block_api::compress` (RustCrypto `sha1 0.11.0`), which dispatches at run time to ARMv8
//!    `vsha1c/p/m/su0/su1`, x86 SHA-NI, or scalar fallback.
//! 3. Only the ~2.5–3.5% of single blocks with `ubc_mask != 0` run [`recompress::compress_and_check`] starting from
//!    the exact incoming chaining state `ihv`, capturing rounds 58 and 65 and running Stevens's backward/forward
//!    recompression for each set bit in `ubc_mask`.
//!
//! **What was rejected.** Whole-object fallback (running hardware SHA-1 first and falling back to scalar `sha1-checked`
//! whenever any block in the object had `ubc_mask != 0`) was ruled out by probability before coding: at ~3% per block,
//! an 80–150 KiB Mercurial manifest (1,300–2,400 blocks) triggers at least one block with probability > 99.999%,
//! giving zero speedup on large objects. Per-block recompression accelerates objects of any size equally.
//!
//! **Oracle.** Held bit-for-bit against `sha1-checked` across golden vectors, SHAttered-1/2, boundary lengths,
//! arbitrary streaming chunk splits, all 32 disturbance vectors, and proptest histories, with non-vacuity counters
//! proving both the hardware fast path and the recompression slow path execute.

mod recompress;
mod ubc;

/// Format: SHA-1 block length in bytes (FIPS 180-4 §5.1.1).
const BLOCK_LEN: usize = 64;

/// Format: SHA-1 digest length in bytes (FIPS 180-4 §1).
pub const DIGEST_LEN: usize = 20;

/// Format: bytes reserved at the end of the final SHA-1 padding block for the 64-bit big-endian bit length
/// (FIPS 180-4 §5.1.1).
const LENGTH_TRAILER_START: usize = 56;

/// Format: leading byte of SHA-1 message padding (`0b1000_0000`, FIPS 180-4 §5.1.1).
const PAD_LEAD_BYTE: u8 = 0x80;

/// Format: bits per byte, converting absorbed byte count to SHA-1's 64-bit message bit length.
const BITS_PER_BYTE: u64 = 8;

/// Shape: eight-block SIMD batch (`2 × u32x4` = 256-bit pair of 128-bit NEON/SSE registers, hiding load/rotate
/// latency during schedule expansion and UBC filtering).
const OCTET_BLOCKS: usize = 8;

/// Format: bytes per 32-bit SHA-1 schedule word (FIPS 180-4 §5.2.1).
const WORD_BYTES: usize = 4;

/// Format: 32-bit words in SHA-1's chaining state `H0..H4` (FIPS 180-4 §5.3.1).
const STATE_WORDS: usize = 5;

/// SHA-1's 160-bit chaining state.
type State = [u32; STATE_WORDS];

/// Format: SHA-1 initial chaining state `H0..H4` (FIPS 180-4 §5.3.1).
const INITIAL_STATE: State = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];

/// An input in which SHA-1 collision detection found a known collision attack, carrying the input's plain SHA-1 digest
/// (as git's `sha1dc_git.c` and Mercurial's `thirdparty/sha1dc/cext.c` compute with `safe_hash = false`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollisionAttack {
  /// The plain SHA-1 digest of the colliding input.
  pub sha1: [u8; DIGEST_LEN],
}

/// Streaming SHA-1 hasher with Marc Stevens collision detection (`detect_collision = true`, `safe_hash = false`).
///
/// Occupies 96 bytes on the stack (no heap allocation, no internal 80-word buffers retained across calls).
#[derive(Clone)]
pub struct Hasher {
  state: [u32; 5],
  total_bytes: u64,
  tail: [u8; BLOCK_LEN],
  tail_len: u8,
  found_collision: bool,
  #[cfg(test)]
  fast_blocks: u64,
  #[cfg(test)]
  recompressed_blocks: u64,
}

impl Default for Hasher {
  fn default() -> Self {
    Self::new()
  }
}

impl Hasher {
  /// A fresh collision-detecting SHA-1 hasher with safe-hash mitigation disabled.
  #[inline]
  pub const fn new() -> Self {
    Self {
      state: INITIAL_STATE,
      total_bytes: 0,
      tail: [0; BLOCK_LEN],
      tail_len: 0,
      found_collision: false,
      #[cfg(test)]
      fast_blocks: 0,
      #[cfg(test)]
      recompressed_blocks: 0,
    }
  }

  /// Absorbs `bytes` into the running hash and collision detector.
  #[inline]
  pub fn update(&mut self, mut bytes: &[u8]) {
    let added = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    self.total_bytes = self.total_bytes.wrapping_add(added);

    let used = usize::from(self.tail_len);
    if used > 0 {
      let need = BLOCK_LEN.saturating_sub(used);
      if bytes.len() < need {
        if let Some(dst) = self.tail.get_mut(used..used.saturating_add(bytes.len())) {
          dst.copy_from_slice(bytes);
        }
        self.tail_len = u8::try_from(used.saturating_add(bytes.len())).unwrap_or(0);
        return;
      }
      if let (Some(dst), Some(head)) = (self.tail.get_mut(used..BLOCK_LEN), bytes.get(..need)) {
        dst.copy_from_slice(head);
      }
      let full = self.tail;
      self.tail_len = 0;
      self.process_one(&full);
      bytes = bytes.get(need..).unwrap_or_default();
    }

    let (blocks, remainder) = bytes.as_chunks::<BLOCK_LEN>();
    if !blocks.is_empty() {
      self.process_blocks(blocks);
    }
    if !remainder.is_empty() {
      if let Some(dst) = self.tail.get_mut(..remainder.len()) {
        dst.copy_from_slice(remainder);
      }
      self.tail_len = u8::try_from(remainder.len()).unwrap_or(0);
    }
  }

  /// Finalizes the hash, returning the 20-byte digest or [`CollisionAttack`] if any block triggered a collision.
  #[inline]
  pub fn finish(mut self) -> Result<[u8; DIGEST_LEN], CollisionAttack> {
    let bit_len = self.total_bytes.wrapping_mul(BITS_PER_BYTE);
    let used = usize::from(self.tail_len);
    let mut pad_block = [0u8; BLOCK_LEN];
    if let (Some(dst), Some(src)) = (pad_block.get_mut(..used), self.tail.get(..used)) {
      dst.copy_from_slice(src);
    }
    if let Some(slot) = pad_block.get_mut(used) {
      *slot = PAD_LEAD_BYTE;
    }
    if used >= LENGTH_TRAILER_START {
      self.process_one(&pad_block);
      pad_block = [0u8; BLOCK_LEN];
    }
    if let Some(trailer) = pad_block.get_mut(LENGTH_TRAILER_START..BLOCK_LEN) {
      trailer.copy_from_slice(&bit_len.to_be_bytes());
    }
    self.process_one(&pad_block);

    let sha1 = encode_state(&self.state);
    if self.found_collision {
      Err(CollisionAttack { sha1 })
    } else {
      Ok(sha1)
    }
  }

  /// Processes a slice of complete 64-byte blocks: eight-lane NEON/SSE UBC filtering over contiguous zero-mask runs
  /// routed to hardware SHA-1 compression, with exact single-block recompression whenever a block's UBC mask is
  /// non-zero.
  fn process_blocks(&mut self, blocks: &[[u8; BLOCK_LEN]]) {
    let (octets, trailing) = blocks.as_chunks::<OCTET_BLOCKS>();
    let mut clean_start: usize = 0;
    let mut scratch = ubc::OctetScratch::new();

    for (octet_index, octet) in octets.iter().enumerate() {
      let masks = ubc::check_octet(octet, &mut scratch);
      if masks == [0; OCTET_BLOCKS] {
        continue;
      }
      let octet_block_start = octet_index.saturating_mul(OCTET_BLOCKS);
      self.flush_clean_run(blocks, clean_start, octet_block_start);
      self.process_octet_mixed(octet, masks);
      clean_start = octet_block_start.saturating_add(OCTET_BLOCKS);
    }

    let octets_end = octets.len().saturating_mul(OCTET_BLOCKS);
    self.flush_clean_run(blocks, clean_start, octets_end);

    let mut tail_clean_start: usize = 0;
    for (index, block) in trailing.iter().enumerate() {
      let mask = ubc::check_one(block);
      if mask == 0 {
        continue;
      }
      self.flush_clean_run(trailing, tail_clean_start, index);
      self.process_triggered(block, mask);
      tail_clean_start = index.saturating_add(1);
    }
    self.flush_clean_run(trailing, tail_clean_start, trailing.len());
  }

  /// Processes a single octet where at least one lane produced a non-zero UBC mask, still batching adjacent clean
  /// lanes inside the octet into hardware SHA-1.
  fn process_octet_mixed(
    &mut self,
    octet: &[[u8; BLOCK_LEN]; OCTET_BLOCKS],
    masks: [u32; OCTET_BLOCKS],
  ) {
    let mut clean_start: usize = 0;
    for (index, (block, mask)) in octet.iter().zip(masks).enumerate() {
      if mask == 0 {
        continue;
      }
      self.flush_clean_run(octet, clean_start, index);
      self.process_triggered(block, mask);
      clean_start = index.saturating_add(1);
    }
    self.flush_clean_run(octet, clean_start, octet.len());
  }

  /// Processes one standalone 64-byte block (the boundary stitch block or a padding block).
  #[inline]
  fn process_one(&mut self, block: &[u8; BLOCK_LEN]) {
    let mask = ubc::check_one(block);
    if mask == 0 {
      self.compress_fast(core::slice::from_ref(block));
    } else {
      self.process_triggered(block, mask);
    }
  }

  /// Feeds a contiguous run of zero-UBC-mask blocks `blocks[start..end]` to hardware SHA-1 compression.
  #[inline]
  fn flush_clean_run(&mut self, blocks: &[[u8; BLOCK_LEN]], start: usize, end: usize) {
    if start >= end {
      return;
    }
    if let Some(run) = blocks.get(start..end) {
      self.compress_fast(run);
    }
  }

  /// Runs hardware-accelerated SHA-1 compression over blocks known to have `ubc_mask == 0`.
  #[inline]
  fn compress_fast(&mut self, blocks: &[[u8; BLOCK_LEN]]) {
    sha1::block_api::compress(&mut self.state, blocks);
    #[cfg(test)]
    {
      self.fast_blocks = self
        .fast_blocks
        .saturating_add(u64::try_from(blocks.len()).unwrap_or(u64::MAX));
    }
  }

  /// Runs single-block SHA-1 compression with intermediate states and Stevens recompression for `mask != 0`.
  #[cold]
  #[inline(never)]
  fn process_triggered(&mut self, block: &[u8; BLOCK_LEN], mask: u32) {
    if recompress::compress_and_check(&mut self.state, block, mask) {
      self.found_collision = true;
    }
    #[cfg(test)]
    {
      self.recompressed_blocks = self.recompressed_blocks.saturating_add(1);
    }
  }

  /// Non-vacuity test counter: blocks compressed through hardware SHA-1 (`ubc_mask == 0`).
  #[cfg(test)]
  pub(crate) fn fast_blocks(&self) -> u64 {
    self.fast_blocks
  }

  /// Non-vacuity test counter: blocks checked through Stevens recompression (`ubc_mask != 0`).
  #[cfg(test)]
  pub(crate) fn recompressed_blocks(&self) -> u64 {
    self.recompressed_blocks
  }
}

/// One-shot collision-detecting SHA-1 of `bytes`.
#[inline]
pub fn hash(bytes: &[u8]) -> Result<[u8; DIGEST_LEN], CollisionAttack> {
  let mut hasher = Hasher::new();
  hasher.update(bytes);
  hasher.finish()
}

/// Encodes the five big-endian SHA-1 state words into a 20-byte digest.
#[inline]
fn encode_state(state: &State) -> [u8; DIGEST_LEN] {
  let [h0, h1, h2, h3, h4] = *state;
  let pieces = [
    h0.to_be_bytes(),
    h1.to_be_bytes(),
    h2.to_be_bytes(),
    h3.to_be_bytes(),
    h4.to_be_bytes(),
  ];
  let mut out = [0u8; DIGEST_LEN];
  for (dst, src) in out.as_chunks_mut::<WORD_BYTES>().0.iter_mut().zip(pieces) {
    *dst = src;
  }
  out
}

#[cfg(test)]
mod tests;
