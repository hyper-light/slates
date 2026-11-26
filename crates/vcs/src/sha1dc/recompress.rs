//! Single-block SHA-1 compression with round-58/65 capture and Marc Stevens recompression (§4.17.2, §4.17.3; A-893).
//!
//! Invoked only on the 4.74% of uniform random 64-byte blocks where [`super::ubc`] returns a non-zero
//! disturbance-vector mask. Uses canonical FIPS 180-4 round state `[a, b, c, d, e]` for both forward compression and
//! Stevens's backward/forward recompression from `T58` or `T65`, and compresses the 32 Stevens disturbance-vector
//! `dm[80]` tables (10 KiB in `sha1-checked`) into four shifted base tables (1.4 KiB total, L1-resident) verified
//! bit-for-bit identical against the in-tree reference oracle.
//!
//! Translates the disturbance-vector tables and recompression algorithm from `cr-marcstevens/sha1collisiondetection`
//! (`lib/sha1.c`, `lib/ubc_check.c`):
//!
//! ```text
//! MIT License
//!
//! Copyright (c) 2017:
//!     Marc Stevens
//!     Cryptology Group
//!     Centrum Wiskunde & Informatica
//!     P.O. Box 94079, 1090 GB Amsterdam, Netherlands
//!     marc@marc-stevens.nl
//!
//!     Dan Shumow
//!     Microsoft Research
//!     danshu@microsoft.com
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.
//! ```

use super::{BLOCK_LEN, State, WORD_BYTES};

/// Format: SHA-1 round count and expanded message-schedule length (FIPS 180-4 §6.1.2).
const SCHEDULE_LEN: usize = 80;

/// Format: input 32-bit big-endian words in one 64-byte SHA-1 block (FIPS 180-4 §5.2.1).
const INPUT_WORDS: usize = 16;

/// Format: message-schedule expansion steps (`80 - 16 = 64`, FIPS 180-4 §6.1.2).
const EXPAND_STEPS: usize = SCHEDULE_LEN - INPUT_WORDS;

/// Format: SHA-1 round boundary between rounds 1 and 2 (FIPS 180-4 §4.1.1).
const ROUND_2: usize = 20;

/// Format: SHA-1 round boundary between rounds 2 and 3 (FIPS 180-4 §4.1.1).
const ROUND_3: usize = 40;

/// Format: SHA-1 round boundary between rounds 3 and 4 (FIPS 180-4 §4.1.1).
const ROUND_4: usize = 60;

/// Format: Stevens intermediate capture step 58 (`Testt::T58` in `sha1collisiondetection`).
const STEP_58: usize = 58;

/// Format: Stevens intermediate capture step 65 (`Testt::T65` in `sha1collisiondetection`).
const STEP_65: usize = 65;

/// Format: left rotation of `a` in each SHA-1 round step (FIPS 180-4 §6.1.2).
const ROT_A: u32 = 5;

/// Format: left rotation of `b` in each SHA-1 round step (FIPS 180-4 §6.1.2).
const ROT_B: u32 = 30;

/// Format: SHA-1 round 1 constant `K_0` (`0..20`, FIPS 180-4 §4.2.1).
const K0: u32 = 0x5a82_7999;

/// Format: SHA-1 round 2 constant `K_1` (`20..40`, FIPS 180-4 §4.2.1).
const K1: u32 = 0x6ed9_eba1;

/// Format: SHA-1 round 3 constant `K_2` (`40..60`, FIPS 180-4 §4.2.1).
const K2: u32 = 0x8f1b_bcdc;

/// Format: SHA-1 round 4 constant `K_3` (`60..80`, FIPS 180-4 §4.2.1).
const K3: u32 = 0xca62_c1d6;

/// Which intermediate step a disturbance vector recompresses from.
#[derive(Clone, Copy)]
enum TestStep {
  T58,
  T65,
}

/// Which of the four shifted base arrays holds a disturbance vector's 80-word XOR difference.
#[derive(Clone, Copy)]
enum Group {
  G1_0,
  G1_2,
  G2_0,
  G2_2,
}

/// Descriptor for one of Marc Stevens's 32 SHA-1 disturbance vectors (indexed by `ubc_mask` bit `0..32`).
#[derive(Clone, Copy)]
struct Dv {
  group: Group,
  offset: usize,
  test_step: TestStep,
}

impl Dv {
  /// Returns the exact 80-word XOR difference `dm` for this disturbance vector, verified in-bounds at compile time.
  #[inline]
  const fn dm_words(self) -> Option<&'static [u32; SCHEDULE_LEN]> {
    let base: &'static [u32] = match self.group {
      Group::G1_0 => &DM_1_0,
      Group::G1_2 => &DM_1_2,
      Group::G2_0 => &DM_2_0,
      Group::G2_2 => &DM_2_2,
    };
    let Some((_, tail)) = base.split_at_checked(self.offset) else {
      return None;
    };
    tail.first_chunk::<SCHEDULE_LEN>()
  }
}

/// Compresses one 64-byte block into `ihv` using canonical scalar FIPS 180-4 SHA-1 (used by the paired Miri path
/// where hardware crypto intrinsics are unsupported, and verified against hardware compression in tests).
#[cfg(any(miri, test))]
pub(super) fn compress_block(ihv: &mut State, block: &[u8; BLOCK_LEN]) {
  let w = expand_80(block);
  let _ = compress_with_states(ihv, &w);
}

/// Compresses `block` into `ihv` while capturing round-58 and round-65 states, then runs Marc Stevens's
/// recompression check for every disturbance-vector bit set in `ubc_mask`. Returns `true` if a collision attack
/// matches `ihv`.
pub(super) fn compress_and_check(ihv: &mut State, block: &[u8; BLOCK_LEN], ubc_mask: u32) -> bool {
  let w = expand_80(block);
  let (state_58, state_65) = compress_with_states(ihv, &w);
  let mut perturbed = [0u32; SCHEDULE_LEN];
  let mut remaining = ubc_mask;
  while remaining != 0 {
    let bit = remaining.trailing_zeros();
    remaining &= remaining.wrapping_sub(1);
    let Some(&dv) = usize::try_from(bit).ok().and_then(|index| DVS.get(index)) else {
      continue;
    };
    let Some(dm) = dv.dm_words() else {
      continue;
    };
    for ((dst, &orig), &diff) in perturbed.iter_mut().zip(&w).zip(dm) {
      *dst = orig ^ diff;
    }
    let candidate = match dv.test_step {
      TestStep::T58 => recompress_58(&state_58, &perturbed),
      TestStep::T65 => recompress_65(&state_65, &perturbed),
    };
    #[cfg(test)]
    record_dv_candidate(bit, candidate);
    if candidate == *ihv {
      return true;
    }
  }
  false
}

/// Test accessor exposing the exact 80-word XOR difference `dm` and test step (`58` or `65`) for `DVS[bit]`.
#[cfg(test)]
pub(super) fn dv_entry(bit: usize) -> Option<(&'static [u32; SCHEDULE_LEN], u32)> {
  let dv = *DVS.get(bit)?;
  let dm = dv.dm_words()?;
  // Format: Marc Stevens disturbance-vector test step numbers (58 and 65).
  let step = match dv.test_step {
    TestStep::T58 => 58,
    TestStep::T65 => 65,
  };
  Some((dm, step))
}

/// Test probe verifying that unperturbed `recompress_58` and `recompress_65` both rebuild the post-compression state.
#[cfg(test)]
pub(super) fn probe_unperturbed_recompress(
  mut ihv: State,
  block: &[u8; BLOCK_LEN],
) -> (State, State, State) {
  let w = expand_80(block);
  let (state_58, state_65) = compress_with_states(&mut ihv, &w);
  (
    ihv,
    recompress_58(&state_58, &w),
    recompress_65(&state_65, &w),
  )
}

#[cfg(test)]
std::thread_local! {
  /// Test-only observer collecting `(bit, candidate)` pairs evaluated inside [`compress_and_check`].
  static DV_OBSERVER: core::cell::RefCell<Option<Vec<(u32, State)>>> =
    const { core::cell::RefCell::new(None) };
}

/// Records one `(bit, candidate)` evaluation inside [`compress_and_check`] when a test observer is active.
#[cfg(test)]
fn record_dv_candidate(bit: u32, candidate: State) {
  DV_OBSERVER.with(|slot| {
    if let Some(log) = slot.borrow_mut().as_mut() {
      log.push((bit, candidate));
    }
  });
}

/// Runs [`compress_and_check`] while recording every `(bit, candidate)` pair it evaluates, clearing the thread-local
/// observer on exit or unwind.
#[cfg(test)]
pub(super) fn observe_compress_and_check(
  ihv: &mut State,
  block: &[u8; BLOCK_LEN],
  ubc_mask: u32,
) -> (bool, Vec<(u32, State)>) {
  struct ResetObserver;
  impl Drop for ResetObserver {
    fn drop(&mut self) {
      DV_OBSERVER.with(|slot| {
        *slot.borrow_mut() = None;
      });
    }
  }
  DV_OBSERVER.with(|slot| {
    *slot.borrow_mut() = Some(Vec::new());
  });
  let _guard = ResetObserver;
  let hit = compress_and_check(ihv, block, ubc_mask);
  let observed = DV_OBSERVER.with(|slot| slot.borrow_mut().take().unwrap_or_default());
  (hit, observed)
}

/// Expands one 64-byte big-endian block into the full 80-word SHA-1 message schedule (FIPS 180-4 §6.1.2).
fn expand_80(block: &[u8; BLOCK_LEN]) -> [u32; SCHEDULE_LEN] {
  let mut w = [0u32; SCHEDULE_LEN];
  if let Some(head) = w.get_mut(..INPUT_WORDS) {
    for (slot, chunk) in head.iter_mut().zip(block.as_chunks::<WORD_BYTES>().0) {
      *slot = u32::from_be_bytes(*chunk);
    }
  }
  for step in 0..EXPAND_STEPS {
    if let Some([w0, _, w2, _, _, _, _, _, w8, _, _, _, _, w13, _, _, dst, ..]) = w.get_mut(step..)
    {
      *dst = (*w13 ^ *w8 ^ *w2 ^ *w0).rotate_left(1);
    }
  }
  w
}

/// SHA-1 round 1 boolean function `Ch(b, c, d)` (FIPS 180-4 §4.1.1).
#[inline(always)]
fn f1(b: u32, c: u32, d: u32) -> u32 {
  d ^ (b & (c ^ d))
}

/// SHA-1 rounds 2 and 4 boolean function `Parity(b, c, d)` (FIPS 180-4 §4.1.1).
#[inline(always)]
fn f2(b: u32, c: u32, d: u32) -> u32 {
  b ^ c ^ d
}

/// SHA-1 round 3 boolean function `Maj(b, c, d)` (FIPS 180-4 §4.1.1).
#[inline(always)]
fn f3(b: u32, c: u32, d: u32) -> u32 {
  (b & c).wrapping_add(d & (b ^ c))
}

/// Advances canonical SHA-1 state `s` forward over `words` using boolean function `f` and constant `k`.
#[inline(always)]
fn forward<F: Fn(u32, u32, u32) -> u32>(mut s: State, words: &[u32], f: F, k: u32) -> State {
  for &word in words {
    let [a, b, c, d, e] = s;
    let next_a = a
      .rotate_left(ROT_A)
      .wrapping_add(f(b, c, d))
      .wrapping_add(e)
      .wrapping_add(k)
      .wrapping_add(word);
    s = [next_a, a, b.rotate_left(ROT_B), c, d];
  }
  s
}

/// Steps canonical SHA-1 state `s` backward over `words` (in reverse order) using `f` and `k`.
#[inline(always)]
fn backward<F: Fn(u32, u32, u32) -> u32>(mut s: State, words: &[u32], f: F, k: u32) -> State {
  for &word in words.iter().rev() {
    let [a_next, prev_a, b_rot, prev_c, prev_d] = s;
    let prev_b = b_rot.rotate_right(ROT_B);
    let prev_e = a_next.wrapping_sub(
      prev_a
        .rotate_left(ROT_A)
        .wrapping_add(f(prev_b, prev_c, prev_d))
        .wrapping_add(k)
        .wrapping_add(word),
    );
    s = [prev_a, prev_b, prev_c, prev_d, prev_e];
  }
  s
}

/// Adds two 5-word SHA-1 chaining states modulo `2^32`.
#[inline(always)]
fn add_state(left: State, right: State) -> State {
  let [a0, b0, c0, d0, e0] = left;
  let [a1, b1, c1, d1, e1] = right;
  [
    a0.wrapping_add(a1),
    b0.wrapping_add(b1),
    c0.wrapping_add(c1),
    d0.wrapping_add(d1),
    e0.wrapping_add(e1),
  ]
}

/// Runs forward SHA-1 compression on `w`, updating `ihv` and returning canonical states at steps 58 and 65.
fn compress_with_states(ihv: &mut State, w: &[u32; SCHEDULE_LEN]) -> (State, State) {
  let mut s = *ihv;
  s = forward(s, w.get(0..ROUND_2).unwrap_or_default(), f1, K0);
  s = forward(s, w.get(ROUND_2..ROUND_3).unwrap_or_default(), f2, K1);
  s = forward(s, w.get(ROUND_3..STEP_58).unwrap_or_default(), f3, K2);
  let state_58 = s;
  s = forward(s, w.get(STEP_58..ROUND_4).unwrap_or_default(), f3, K2);
  s = forward(s, w.get(ROUND_4..STEP_65).unwrap_or_default(), f4_alias, K3);
  let state_65 = s;
  s = forward(
    s,
    w.get(STEP_65..SCHEDULE_LEN).unwrap_or_default(),
    f4_alias,
    K3,
  );
  *ihv = add_state(*ihv, s);
  (state_58, state_65)
}

/// Recompresses backward from step 58 to 0 and forward from step 58 to 80 under perturbed schedule `m2`.
fn recompress_58(state_58: &State, m2: &[u32; SCHEDULE_LEN]) -> State {
  let mut back = *state_58;
  back = backward(back, m2.get(ROUND_3..STEP_58).unwrap_or_default(), f3, K2);
  back = backward(back, m2.get(ROUND_2..ROUND_3).unwrap_or_default(), f2, K1);
  back = backward(back, m2.get(0..ROUND_2).unwrap_or_default(), f1, K0);

  let mut fwd = *state_58;
  fwd = forward(fwd, m2.get(STEP_58..ROUND_4).unwrap_or_default(), f3, K2);
  fwd = forward(
    fwd,
    m2.get(ROUND_4..SCHEDULE_LEN).unwrap_or_default(),
    f4_alias,
    K3,
  );
  add_state(back, fwd)
}

/// Recompresses backward from step 65 to 0 and forward from step 65 to 80 under perturbed schedule `m2`.
fn recompress_65(state_65: &State, m2: &[u32; SCHEDULE_LEN]) -> State {
  let mut back = *state_65;
  back = backward(
    back,
    m2.get(ROUND_4..STEP_65).unwrap_or_default(),
    f4_alias,
    K3,
  );
  back = backward(back, m2.get(ROUND_3..ROUND_4).unwrap_or_default(), f3, K2);
  back = backward(back, m2.get(ROUND_2..ROUND_3).unwrap_or_default(), f2, K1);
  back = backward(back, m2.get(0..ROUND_2).unwrap_or_default(), f1, K0);

  let fwd = forward(
    *state_65,
    m2.get(STEP_65..SCHEDULE_LEN).unwrap_or_default(),
    f4_alias,
    K3,
  );
  add_state(back, fwd)
}

/// Alias of [`f2`] for round 4 (`Parity`, FIPS 180-4 §4.1.1).
#[inline(always)]
fn f4_alias(b: u32, c: u32, d: u32) -> u32 {
  f2(b, c, d)
}

/// Format: Stevens sha1collisiondetection disturbance-vector XOR words for type I, `b = 0` (`k = 43..=52`).
const DM_1_0: [u32; 89] = [
  // Format: Stevens DM_1_0[0..4].
  0x0400_0010,
  0xe800_0000,
  0x0800_000c,
  0x1800_0000,
  // Format: Stevens DM_1_0[4..8].
  0xb800_000a,
  0xc800_0010,
  0x2c00_0010,
  0xf400_0014,
  // Format: Stevens DM_1_0[8..12].
  0xb400_0008,
  0x0800_0000,
  0x9800_000c,
  0xd800_0010,
  // Format: Stevens DM_1_0[12..16].
  0x0800_0010,
  0xb800_0010,
  0x9800_0000,
  0x6000_0000,
  // Format: Stevens DM_1_0[16..20].
  0x0000_0008,
  0xc000_0000,
  0x9000_0014,
  0x1000_0010,
  // Format: Stevens DM_1_0[20..24].
  0xb800_0014,
  0x2800_0000,
  0x2000_0010,
  0x4800_0000,
  // Format: Stevens DM_1_0[24..28].
  0x0800_0018,
  0x6000_0000,
  0x9000_0010,
  0xf000_0010,
  // Format: Stevens DM_1_0[28..32].
  0x9000_0008,
  0xc000_0000,
  0x9000_0010,
  0xf000_0010,
  // Format: Stevens DM_1_0[32..36].
  0xb000_0008,
  0x4000_0000,
  0x9000_0000,
  0xf000_0010,
  // Format: Stevens DM_1_0[36..40].
  0x9000_0018,
  0x6000_0000,
  0x9000_0010,
  0x9000_0010,
  // Format: Stevens DM_1_0[40..44].
  0x9000_0000,
  0x8000_0000,
  0x0000_0010,
  0xa000_0000,
  // Format: Stevens DM_1_0[44..48].
  0x2000_0000,
  0xa000_0000,
  0x2000_0010,
  0x0000_0000,
  // Format: Stevens DM_1_0[48..52].
  0x2000_0010,
  0x2000_0000,
  0x0000_0010,
  0x2000_0000,
  // Format: Stevens DM_1_0[52..56].
  0x0000_0010,
  0xa000_0000,
  0x0000_0000,
  0x2000_0000,
  // Format: Stevens DM_1_0[56..60].
  0x2000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  // Format: Stevens DM_1_0[60..64].
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  // Format: Stevens DM_1_0[64..68].
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0001,
  // Format: Stevens DM_1_0[68..72].
  0x0000_0020,
  0x0000_0001,
  0x4000_0002,
  0x4000_0040,
  // Format: Stevens DM_1_0[72..76].
  0x4000_0002,
  0x8000_0004,
  0x8000_0080,
  0x8000_0006,
  // Format: Stevens DM_1_0[76..80].
  0x0000_0049,
  0x0000_0103,
  0x8000_0009,
  0x8000_0012,
  // Format: Stevens DM_1_0[80..84].
  0x8000_0202,
  0x0000_0018,
  0x0000_0164,
  0x0000_0408,
  // Format: Stevens DM_1_0[84..88].
  0x8000_00e6,
  0x8000_004c,
  0x0000_0803,
  0x8000_0161,
  // Format: Stevens DM_1_0[88..89].
  0x8000_0599,
];

/// Format: Stevens sha1collisiondetection disturbance-vector XOR words for type I, `b = 2` (`k = 46..=51`).
const DM_1_2: [u32; 85] = [
  // Format: Stevens DM_1_2[0..4].
  0xa000_0003,
  0x2000_0030,
  0x6000_0000,
  0xe000_002a,
  // Format: Stevens DM_1_2[4..8].
  0x2000_0043,
  0xb000_0040,
  0xd000_0053,
  0xd000_0022,
  // Format: Stevens DM_1_2[8..12].
  0x2000_0000,
  0x6000_0032,
  0x6000_0043,
  0x2000_0040,
  // Format: Stevens DM_1_2[12..16].
  0xe000_0042,
  0x6000_0002,
  0x8000_0001,
  0x0000_0020,
  // Format: Stevens DM_1_2[16..20].
  0x0000_0003,
  0x4000_0052,
  0x4000_0040,
  0xe000_0052,
  // Format: Stevens DM_1_2[20..24].
  0xa000_0000,
  0x8000_0040,
  0x2000_0001,
  0x2000_0060,
  // Format: Stevens DM_1_2[24..28].
  0x8000_0001,
  0x4000_0042,
  0xc000_0043,
  0x4000_0022,
  // Format: Stevens DM_1_2[28..32].
  0x0000_0003,
  0x4000_0042,
  0xc000_0043,
  0xc000_0022,
  // Format: Stevens DM_1_2[32..36].
  0x0000_0001,
  0x4000_0002,
  0xc000_0043,
  0x4000_0062,
  // Format: Stevens DM_1_2[36..40].
  0x8000_0001,
  0x4000_0042,
  0x4000_0042,
  0x4000_0002,
  // Format: Stevens DM_1_2[40..44].
  0x0000_0002,
  0x0000_0040,
  0x8000_0002,
  0x8000_0000,
  // Format: Stevens DM_1_2[44..48].
  0x8000_0002,
  0x8000_0040,
  0x0000_0000,
  0x8000_0040,
  // Format: Stevens DM_1_2[48..52].
  0x8000_0000,
  0x0000_0040,
  0x8000_0000,
  0x0000_0040,
  // Format: Stevens DM_1_2[52..56].
  0x8000_0002,
  0x0000_0000,
  0x8000_0000,
  0x8000_0000,
  // Format: Stevens DM_1_2[56..60].
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  // Format: Stevens DM_1_2[60..64].
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  // Format: Stevens DM_1_2[64..68].
  0x0000_0000,
  0x0000_0000,
  0x0000_0004,
  0x0000_0080,
  // Format: Stevens DM_1_2[68..72].
  0x0000_0004,
  0x0000_0009,
  0x0000_0101,
  0x0000_0009,
  // Format: Stevens DM_1_2[72..76].
  0x0000_0012,
  0x0000_0202,
  0x0000_001a,
  0x0000_0124,
  // Format: Stevens DM_1_2[76..80].
  0x0000_040c,
  0x0000_0026,
  0x0000_004a,
  0x0000_080a,
  // Format: Stevens DM_1_2[80..84].
  0x0000_0060,
  0x0000_0590,
  0x0000_1020,
  0x0000_039a,
  // Format: Stevens DM_1_2[84..85].
  0x0000_0132,
];

/// Format: Stevens sha1collisiondetection disturbance-vector XOR words for type II, `b = 0` (`k = 45..=56`).
const DM_2_0: [u32; 91] = [
  // Format: Stevens DM_2_0[0..4].
  0x2600_001a,
  0x0000_0010,
  0x0400_001c,
  0xcc00_0014,
  // Format: Stevens DM_2_0[4..8].
  0x0c00_0002,
  0xc000_0010,
  0xb400_001c,
  0x3c00_0004,
  // Format: Stevens DM_2_0[8..12].
  0xbc00_001a,
  0x2000_0010,
  0x2400_001c,
  0xec00_0014,
  // Format: Stevens DM_2_0[12..16].
  0x0c00_0002,
  0xc000_0010,
  0xb400_001c,
  0x2c00_0004,
  // Format: Stevens DM_2_0[16..20].
  0xbc00_0018,
  0xb000_0010,
  0x0000_000c,
  0xb800_0010,
  // Format: Stevens DM_2_0[20..24].
  0x0800_0018,
  0x7800_0010,
  0x0800_0014,
  0x7000_0010,
  // Format: Stevens DM_2_0[24..28].
  0xb800_001c,
  0xe800_0000,
  0xb000_0004,
  0x5800_0010,
  // Format: Stevens DM_2_0[28..32].
  0xb000_000c,
  0x4800_0000,
  0xb000_0000,
  0xb800_0010,
  // Format: Stevens DM_2_0[32..36].
  0x9800_0010,
  0xa000_0000,
  0x0000_0000,
  0x0000_0000,
  // Format: Stevens DM_2_0[36..40].
  0x2000_0000,
  0x8000_0000,
  0x0000_0010,
  0x0000_0000,
  // Format: Stevens DM_2_0[40..44].
  0x2000_0010,
  0x2000_0000,
  0x0000_0010,
  0x6000_0000,
  // Format: Stevens DM_2_0[44..48].
  0x0000_0018,
  0xe000_0000,
  0x9000_0000,
  0x3000_0010,
  // Format: Stevens DM_2_0[48..52].
  0xb000_0000,
  0x2000_0000,
  0x2000_0000,
  0xa000_0000,
  // Format: Stevens DM_2_0[52..56].
  0x0000_0010,
  0x8000_0000,
  0x2000_0000,
  0x2000_0000,
  // Format: Stevens DM_2_0[56..60].
  0x2000_0000,
  0x8000_0000,
  0x0000_0010,
  0x0000_0000,
  // Format: Stevens DM_2_0[60..64].
  0x2000_0010,
  0xa000_0000,
  0x0000_0000,
  0x2000_0000,
  // Format: Stevens DM_2_0[64..68].
  0x2000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  // Format: Stevens DM_2_0[68..72].
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0001,
  // Format: Stevens DM_2_0[72..76].
  0x0000_0020,
  0x0000_0001,
  0x4000_0002,
  0x4000_0041,
  // Format: Stevens DM_2_0[76..80].
  0x4000_0022,
  0x8000_0005,
  0xc000_0082,
  0xc000_0046,
  // Format: Stevens DM_2_0[80..84].
  0x4000_004b,
  0x8000_0107,
  0x0000_0089,
  0x0000_0014,
  // Format: Stevens DM_2_0[84..88].
  0x8000_024b,
  0x0000_011b,
  0x8000_016d,
  0x8000_041a,
  // Format: Stevens DM_2_0[88..91].
  0x0000_02e4,
  0x8000_0054,
  0x0000_0967,
];

/// Format: Stevens sha1collisiondetection disturbance-vector XOR words for type II, `b = 2` (`k = 46, 49..=51`).
const DM_2_2: [u32; 85] = [
  // Format: Stevens DM_2_2[0..4].
  0x0000_0043,
  0xd000_0072,
  0xf000_0010,
  0xf000_006a,
  // Format: Stevens DM_2_2[4..8].
  0x8000_0040,
  0x9000_0070,
  0xb000_0053,
  0x3000_0008,
  // Format: Stevens DM_2_2[8..12].
  0x0000_0043,
  0xd000_0072,
  0xb000_0010,
  0xf000_0062,
  // Format: Stevens DM_2_2[12..16].
  0xc000_0042,
  0x0000_0030,
  0xe000_0042,
  0x2000_0060,
  // Format: Stevens DM_2_2[16..20].
  0xe000_0041,
  0x2000_0050,
  0xc000_0041,
  0xe000_0072,
  // Format: Stevens DM_2_2[20..24].
  0xa000_0003,
  0xc000_0012,
  0x6000_0041,
  0xc000_0032,
  // Format: Stevens DM_2_2[24..28].
  0x2000_0001,
  0xc000_0002,
  0xe000_0042,
  0x6000_0042,
  // Format: Stevens DM_2_2[28..32].
  0x8000_0002,
  0x0000_0000,
  0x0000_0000,
  0x8000_0000,
  // Format: Stevens DM_2_2[32..36].
  0x0000_0002,
  0x0000_0040,
  0x0000_0000,
  0x8000_0040,
  // Format: Stevens DM_2_2[36..40].
  0x8000_0000,
  0x0000_0040,
  0x8000_0001,
  0x0000_0060,
  // Format: Stevens DM_2_2[40..44].
  0x8000_0003,
  0x4000_0002,
  0xc000_0040,
  0xc000_0002,
  // Format: Stevens DM_2_2[44..48].
  0x8000_0000,
  0x8000_0000,
  0x8000_0002,
  0x0000_0040,
  // Format: Stevens DM_2_2[48..52].
  0x0000_0002,
  0x8000_0000,
  0x8000_0000,
  0x8000_0000,
  // Format: Stevens DM_2_2[52..56].
  0x0000_0002,
  0x0000_0040,
  0x0000_0000,
  0x8000_0040,
  // Format: Stevens DM_2_2[56..60].
  0x8000_0002,
  0x0000_0000,
  0x8000_0000,
  0x8000_0000,
  // Format: Stevens DM_2_2[60..64].
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  0x0000_0000,
  // Format: Stevens DM_2_2[64..68].
  0x0000_0000,
  0x0000_0000,
  0x0000_0004,
  0x0000_0080,
  // Format: Stevens DM_2_2[68..72].
  0x0000_0004,
  0x0000_0009,
  0x0000_0105,
  0x0000_0089,
  // Format: Stevens DM_2_2[72..76].
  0x0000_0016,
  0x0000_020b,
  0x0000_011b,
  0x0000_012d,
  // Format: Stevens DM_2_2[76..80].
  0x0000_041e,
  0x0000_0224,
  0x0000_0050,
  0x0000_092e,
  // Format: Stevens DM_2_2[80..84].
  0x0000_046c,
  0x0000_05b6,
  0x0000_106a,
  0x0000_0b90,
  // Format: Stevens DM_2_2[84..85].
  0x0000_0152,
];

/// Constructs a [`Dv`] descriptor concisely so each table row fits on one formatted line.
const fn dv(group: Group, offset: usize, test_step: TestStep) -> Dv {
  Dv {
    group,
    offset,
    test_step,
  }
}

/// Format: Marc Stevens's 32 SHA-1 disturbance vectors, indexed by `ubc_mask` bit position (`0..32`).
const DVS: [Dv; 32] = [
  // Format: Stevens disturbance-vector table (bits 0..=3).
  dv(Group::G1_0, 9, TestStep::T58),
  dv(Group::G1_0, 8, TestStep::T58),
  dv(Group::G1_0, 7, TestStep::T58),
  dv(Group::G1_0, 6, TestStep::T58),
  // Format: Stevens disturbance-vector table (bits 4..=7).
  dv(Group::G1_2, 5, TestStep::T58),
  dv(Group::G1_0, 5, TestStep::T58),
  dv(Group::G1_2, 4, TestStep::T58),
  dv(Group::G1_0, 4, TestStep::T58),
  // Format: Stevens disturbance-vector table (bits 8..=11).
  dv(Group::G1_2, 3, TestStep::T58),
  dv(Group::G1_0, 3, TestStep::T58),
  dv(Group::G1_2, 2, TestStep::T58),
  dv(Group::G1_0, 2, TestStep::T65),
  // Format: Stevens disturbance-vector table (bits 12..=15).
  dv(Group::G1_2, 1, TestStep::T65),
  dv(Group::G1_0, 1, TestStep::T65),
  dv(Group::G1_2, 0, TestStep::T65),
  dv(Group::G1_0, 0, TestStep::T65),
  // Format: Stevens disturbance-vector table (bits 16..=19).
  dv(Group::G2_0, 11, TestStep::T58),
  dv(Group::G2_0, 10, TestStep::T58),
  dv(Group::G2_2, 5, TestStep::T58),
  dv(Group::G2_0, 9, TestStep::T58),
  // Format: Stevens disturbance-vector table (bits 20..=23).
  dv(Group::G2_0, 8, TestStep::T58),
  dv(Group::G2_0, 7, TestStep::T58),
  dv(Group::G2_2, 2, TestStep::T58),
  dv(Group::G2_0, 6, TestStep::T65),
  // Format: Stevens disturbance-vector table (bits 24..=27).
  dv(Group::G2_2, 1, TestStep::T65),
  dv(Group::G2_0, 5, TestStep::T65),
  dv(Group::G2_2, 0, TestStep::T65),
  dv(Group::G2_0, 4, TestStep::T65),
  // Format: Stevens disturbance-vector table (bits 28..=31).
  dv(Group::G2_0, 3, TestStep::T65),
  dv(Group::G2_0, 2, TestStep::T65),
  dv(Group::G2_0, 1, TestStep::T65),
  dv(Group::G2_0, 0, TestStep::T65),
];

/// Derived: compile-time proof that all 32 [`DVS`] entries yield exact 80-word [`Dv::dm_words`] windows.
const _ALL_DVS_IN_BOUNDS: [(); 1] = [(); {
  let mut remaining = DVS.as_slice();
  let mut ok = 1usize;
  while let Some((entry, rest)) = remaining.split_first() {
    if entry.dm_words().is_none() {
      ok = 0;
    }
    remaining = rest;
  }
  ok
}];
