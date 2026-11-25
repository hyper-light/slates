//! Marc Stevens unavoidable bit-condition (UBC) filter for SHA-1 collision detection (§4.17.2, §4.17.3; A-893).
//!
//! Translates the bit-relation equations from `cr-marcstevens/sha1collisiondetection` (`lib/ubc_check.c`, generated
//! from `tools/data/3565`) into an eight-lane (`[u32; 8]`, two 128-bit NEON / SSE registers per word) auto-vectorized
//! two-stage filter with zero `unsafe`:
//! 1. **Eight-lane schedule expansion**: reuses one caller-owned `[Octet; 65]` stack scratch across every octet in an
//!    `update()` call (zero per-octet zeroing), expanding `W[16..65]` via tight `[u32; 8]` slice loops that LLVM
//!    compiles to ARMv8.2 `ldp q` + `eor3.16b` + `add.4s` + `usra.4s` + `stp q` (13 instructions per 8 blocks).
//! 2. **Table-driven SIMD Stage 1**: evaluates the 71 high-leverage equations grouped by bit shift (`(29,29)`, `(4,29)`,
//!    `(6,6)`, `(1,6)`, `(1,1)`, `(2,7)`) so every group runs as a tight L1I-resident 8-lane NEON/SSE loop, dropping
//!    per-block survival from 53% to ~6%.
//! 3. **Guarded scalar Stage 2**: runs ONLY on individual lanes whose Stage 1 mask is non-zero (~6% of blocks),
//!    testing each remaining group/single-DV equation behind `if mask & bits != 0`.
//!
//! Because every UBC equation clears bits with bitwise AND, reordering equations between Stage 1 and Stage 2 produces
//! the bit-for-bit identical 32-bit `ubc_mask` on every possible 64-byte block.

use super::{BLOCK_LEN, OCTET_BLOCKS, WORD_BYTES};

/// Eight-lane SOA word vector (`2 × u32x4` for NEON / SSE2 or `1 × u32x8` for AVX2).
type Octet = [u32; OCTET_BLOCKS];

/// Format: first SHA-1 schedule word read by any Stevens UBC equation (`W[35]`).
const FIRST_UBC_WORD: usize = 35;

/// Format: number of schedule words read by Stevens UBC equations (`W[35..=64]`, 30 words).
const TAIL_WORDS: usize = 30;

/// Format: total SHA-1 schedule words through `W[64]` (`35 + 30 = 65`).
const UBC_WORDS: usize = FIRST_UBC_WORD + TAIL_WORDS;

/// Format: input 32-bit big-endian words in one 64-byte SHA-1 block (FIPS 180-4 §5.2.1).
const INPUT_WORDS: usize = 16;

/// Format: schedule expansion steps from `W[16]` through `W[64]` (`65 - 16 = 49`).
const EXPAND_STEPS: usize = UBC_WORDS - INPUT_WORDS;

/// Reusable stack scratch for eight-lane message-schedule expansion, allocated once per `process_blocks` call.
#[repr(align(32))]
pub(super) struct OctetScratch {
  w: [Octet; UBC_WORDS],
}

impl OctetScratch {
  /// Allocates a fresh scratch buffer (zero-initialized once per `update` call, overwritten on every octet).
  #[inline]
  pub(super) const fn new() -> Self {
    Self {
      w: [[0; OCTET_BLOCKS]; UBC_WORDS],
    }
  }
}

/// Format: Stevens disturbance-vector mask bit positions (`lib/ubc_check.c`, `tools/data/3565`).
const DV_I_43_0: u32 = 1 << 0;
const DV_I_44_0: u32 = 1 << 1;
const DV_I_45_0: u32 = 1 << 2;
const DV_I_46_0: u32 = 1 << 3;
// Format: Stevens disturbance-vector mask bit positions (continued).
const DV_I_46_2: u32 = 1 << 4;
const DV_I_47_0: u32 = 1 << 5;
const DV_I_47_2: u32 = 1 << 6;
const DV_I_48_0: u32 = 1 << 7;
// Format: Stevens disturbance-vector mask bit positions (continued).
const DV_I_48_2: u32 = 1 << 8;
const DV_I_49_0: u32 = 1 << 9;
const DV_I_49_2: u32 = 1 << 10;
const DV_I_50_0: u32 = 1 << 11;
// Format: Stevens disturbance-vector mask bit positions (continued).
const DV_I_50_2: u32 = 1 << 12;
const DV_I_51_0: u32 = 1 << 13;
const DV_I_51_2: u32 = 1 << 14;
const DV_I_52_0: u32 = 1 << 15;
// Format: Stevens disturbance-vector mask bit positions (continued).
const DV_II_45_0: u32 = 1 << 16;
const DV_II_46_0: u32 = 1 << 17;
const DV_II_46_2: u32 = 1 << 18;
const DV_II_47_0: u32 = 1 << 19;
// Format: Stevens disturbance-vector mask bit positions (continued).
const DV_II_48_0: u32 = 1 << 20;
const DV_II_49_0: u32 = 1 << 21;
const DV_II_49_2: u32 = 1 << 22;
const DV_II_50_0: u32 = 1 << 23;
// Format: Stevens disturbance-vector mask bit positions (continued).
const DV_II_50_2: u32 = 1 << 24;
const DV_II_51_0: u32 = 1 << 25;
const DV_II_51_2: u32 = 1 << 26;
const DV_II_52_0: u32 = 1 << 27;
// Format: Stevens disturbance-vector mask bit positions (continued).
const DV_II_53_0: u32 = 1 << 28;
const DV_II_54_0: u32 = 1 << 29;
const DV_II_55_0: u32 = 1 << 30;
const DV_II_56_0: u32 = 1 << 31;

/// All `b = 0` type-I disturbance-vector mask bits.
const TYPE_I_B0: u32 = DV_I_43_0
  | DV_I_44_0
  | DV_I_45_0
  | DV_I_46_0
  | DV_I_47_0
  | DV_I_48_0
  | DV_I_49_0
  | DV_I_50_0
  | DV_I_51_0
  | DV_I_52_0;

/// All `b = 0` type-II disturbance-vector mask bits.
const TYPE_II_B0: u32 = DV_II_45_0
  | DV_II_46_0
  | DV_II_47_0
  | DV_II_48_0
  | DV_II_49_0
  | DV_II_50_0
  | DV_II_51_0
  | DV_II_52_0
  | DV_II_53_0
  | DV_II_54_0
  | DV_II_55_0
  | DV_II_56_0;

/// All `b = 0` disturbance-vector mask bits.
const ALL_B0: u32 = TYPE_I_B0 | TYPE_II_B0;

/// All `b = 2` disturbance-vector mask bits with residual Stage 2 checks.
const RESIDUAL_B2: u32 = DV_I_46_2
  | DV_I_47_2
  | DV_I_48_2
  | DV_I_49_2
  | DV_I_50_2
  | DV_I_51_2
  | DV_II_46_2
  | DV_II_49_2
  | DV_II_50_2
  | DV_II_51_2;

/// One table-driven UBC equation: `(word_a, word_b, inverted_dv_bits)` where `!dv_bits` is pre-negated so the
/// vector loop needs zero runtime bitwise NOT.
type Eq = (usize, usize, u32);

/// Format: Stevens `ubc_check` Stage 1 `eq_same(..., shift = 29)` equations (`tools/data/3565`).
const EQ_SAME_29: &[Eq] = &[
  // Format: Stevens `eq_same(shift = 29)` equations.
  (44, 45, !(DV_I_48_0 | DV_I_51_0 | DV_I_52_0 | DV_II_45_0 | DV_II_46_0 | DV_II_50_0 | DV_II_51_0)),
  (49, 50, !(DV_I_46_0 | DV_II_45_0 | DV_II_50_0 | DV_II_51_0 | DV_II_55_0 | DV_II_56_0)),
  (48, 49, !(DV_I_45_0 | DV_I_52_0 | DV_II_49_0 | DV_II_50_0 | DV_II_54_0 | DV_II_55_0)),
  (47, 48, !(DV_I_44_0 | DV_I_51_0 | DV_II_48_0 | DV_II_49_0 | DV_II_53_0 | DV_II_54_0)),
  // Format: Stevens `eq_same(shift = 29)` equations (continued).
  (46, 47, !(DV_I_43_0 | DV_I_50_0 | DV_II_47_0 | DV_II_48_0 | DV_II_52_0 | DV_II_53_0)),
  (45, 46, !(DV_I_49_0 | DV_I_52_0 | DV_II_46_0 | DV_II_47_0 | DV_II_51_0 | DV_II_52_0)),
  (43, 44, !(DV_I_47_0 | DV_I_50_0 | DV_I_51_0 | DV_II_45_0 | DV_II_49_0 | DV_II_50_0)),
  (40, 41, !(DV_I_44_0 | DV_I_47_0 | DV_I_48_0 | DV_II_46_0 | DV_II_47_0 | DV_II_56_0)),
  // Format: Stevens `eq_same(shift = 29)` equations (continued).
  (54, 55, !(DV_I_51_0 | DV_II_47_0 | DV_II_50_0 | DV_II_55_0 | DV_II_56_0)),
  (53, 54, !(DV_I_50_0 | DV_II_46_0 | DV_II_49_0 | DV_II_54_0 | DV_II_55_0)),
  (52, 53, !(DV_I_49_0 | DV_II_45_0 | DV_II_48_0 | DV_II_53_0 | DV_II_54_0)),
  (50, 51, !(DV_I_47_0 | DV_II_46_0 | DV_II_51_0 | DV_II_52_0 | DV_II_56_0)),
  // Format: Stevens `eq_same(shift = 29)` equations (continued).
  (42, 43, !(DV_I_46_0 | DV_I_49_0 | DV_I_50_0 | DV_II_48_0 | DV_II_49_0)),
  (41, 42, !(DV_I_45_0 | DV_I_48_0 | DV_I_49_0 | DV_II_47_0 | DV_II_48_0)),
  (51, 52, !(DV_I_48_0 | DV_II_47_0 | DV_II_52_0 | DV_II_53_0)),
];

/// Format: Stevens `ubc_check` Stage 1 `eq_diff(..., 4, ..., 29)` equations (`tools/data/3565`).
const EQ_DIFF_4_29: &[Eq] = &[
  // Format: Stevens `eq_diff(4, 29)` equations.
  (47, 50, !(DV_I_47_0 | DV_I_49_0 | DV_I_51_0 | DV_II_45_0 | DV_II_51_0 | DV_II_56_0)),
  (46, 49, !(DV_I_46_0 | DV_I_48_0 | DV_I_50_0 | DV_I_52_0 | DV_II_50_0 | DV_II_55_0)),
  (45, 48, !(DV_I_45_0 | DV_I_47_0 | DV_I_49_0 | DV_I_51_0 | DV_II_49_0 | DV_II_54_0)),
  (44, 47, !(DV_I_44_0 | DV_I_46_0 | DV_I_48_0 | DV_I_50_0 | DV_II_48_0 | DV_II_53_0)),
  // Format: Stevens `eq_diff(4, 29)` equations (continued).
  (43, 46, !(DV_I_43_0 | DV_I_45_0 | DV_I_47_0 | DV_I_49_0 | DV_II_47_0 | DV_II_52_0)),
  (42, 45, !(DV_I_44_0 | DV_I_46_0 | DV_I_48_0 | DV_I_52_0 | DV_II_46_0 | DV_II_51_0)),
  (41, 44, !(DV_I_43_0 | DV_I_45_0 | DV_I_47_0 | DV_I_51_0 | DV_II_45_0 | DV_II_50_0)),
  (50, 53, !(DV_I_50_0 | DV_I_52_0 | DV_II_46_0 | DV_II_48_0 | DV_II_54_0)),
  // Format: Stevens `eq_diff(4, 29)` equations (continued).
  (49, 52, !(DV_I_49_0 | DV_I_51_0 | DV_II_45_0 | DV_II_47_0 | DV_II_53_0)),
  (48, 51, !(DV_I_48_0 | DV_I_50_0 | DV_I_52_0 | DV_II_46_0 | DV_II_52_0)),
  (40, 43, !(DV_I_44_0 | DV_I_46_0 | DV_I_50_0 | DV_II_49_0 | DV_II_56_0)),
  (39, 42, !(DV_I_43_0 | DV_I_45_0 | DV_I_49_0 | DV_II_48_0 | DV_II_55_0)),
  // Format: Stevens `eq_diff(4, 29)` equations (continued).
  (37, 40, !(DV_I_43_0 | DV_I_47_0 | DV_II_46_0 | DV_II_53_0 | DV_II_55_0)),
  (38, 41, !(DV_I_44_0 | DV_I_48_0 | DV_II_47_0 | DV_II_54_0 | DV_II_56_0)),
];

/// Format: Stevens `ubc_check` Stage 1 `eq_same(..., shift = 6)` equations (`tools/data/3565`).
const EQ_SAME_6: &[Eq] = &[
  // Format: Stevens `eq_same(shift = 6)` equations.
  (45, 47, !(DV_I_47_2 | DV_I_49_2 | DV_I_51_2)),
  (44, 46, !(DV_I_46_2 | DV_I_48_2 | DV_I_50_2)),
  (48, 50, !(DV_I_50_2 | DV_II_46_2)),
  (47, 49, !(DV_I_49_2 | DV_I_51_2)),
  // Format: Stevens `eq_same(shift = 6)` equations (continued).
  (46, 48, !(DV_I_48_2 | DV_I_50_2)),
  (43, 45, !(DV_I_47_2 | DV_I_49_2)),
  (42, 44, !(DV_I_46_2 | DV_I_48_2)),
  (51, 53, !DV_II_49_2),
  // Format: Stevens `eq_same(shift = 6)` equations (continued).
  (52, 54, !DV_II_50_2),
  (53, 55, !DV_II_51_2),
  (49, 51, !DV_I_51_2),
];

/// Format: Stevens `ubc_check` Stage 1 `eq_diff(..., 1, ..., 6)` equations (`tools/data/3565`).
const EQ_DIFF_1_6: &[Eq] = &[
  // Format: Stevens `eq_diff(1, 6)` equations.
  (51, 50, !(DV_I_50_2 | DV_II_46_2)),
  (48, 47, !(DV_I_47_2 | DV_II_51_2)),
  (47, 46, !(DV_I_46_2 | DV_II_50_2)),
  (43, 42, !(DV_II_46_2 | DV_II_51_2)),
  // Format: Stevens `eq_diff(1, 6)` equations (continued).
  (42, 41, !(DV_I_51_2 | DV_II_50_2)),
  (41, 40, !(DV_I_50_2 | DV_II_49_2)),
  (54, 53, !DV_II_49_2),
  (46, 45, !DV_II_49_2),
  // Format: Stevens `eq_diff(1, 6)` equations (continued).
  (55, 54, !DV_II_50_2),
  (56, 55, !DV_II_51_2),
  (49, 48, !DV_I_48_2),
  (50, 49, !DV_I_49_2),
  // Format: Stevens `eq_diff(1, 6)` equations (continued).
  (40, 39, !DV_I_49_2),
  (52, 51, !DV_I_51_2),
];

/// Format: Stevens `ubc_check` Stage 1 `ne_diff(..., 1, ..., 6)` equations (`tools/data/3565`).
const NE_DIFF_1_6: &[Eq] = &[
  // Format: Stevens `ne_diff(1, 6)` equations.
  (41, 42, !(DV_I_48_2 | DV_II_46_2 | DV_II_51_2)),
  (40, 41, !(DV_I_47_2 | DV_I_51_2 | DV_II_50_2)),
  (39, 40, !(DV_I_46_2 | DV_I_50_2 | DV_II_49_2)),
  (36, 37, !(DV_I_47_2 | DV_I_50_2 | DV_II_46_2)),
  // Format: Stevens `ne_diff(1, 6)` equations (continued).
  (44, 45, !(DV_I_51_2 | DV_II_49_2)),
  (35, 36, !(DV_I_46_2 | DV_I_49_2)),
  (37, 38, !(DV_I_48_2 | DV_I_51_2)),
];

/// Format: Stevens `ubc_check` Stage 1 `ne_same(..., shift = 1)` equations (`tools/data/3565`).
const NE_SAME_1: &[Eq] = &[
  // Format: Stevens `ne_same(shift = 1)` equations.
  (50, 54, !DV_II_49_2),
  (51, 55, !DV_II_50_2),
  (45, 47, !DV_II_50_2),
  (52, 56, !DV_II_51_2),
  // Format: Stevens `ne_same(shift = 1)` equations (continued).
  (46, 48, !DV_II_51_2),
  (42, 50, !DV_I_49_2),
  (38, 40, !DV_I_49_2),
];

/// Format: Stevens `ubc_check` Stage 1 `ne_diff(..., 2, ..., 7)` equations (`tools/data/3565`).
const NE_DIFF_2_7: &[Eq] = &[
  // Format: Stevens `ne_diff(2, 7)` equations.
  (61, 62, !(DV_I_46_2 | DV_II_46_2)),
  (63, 64, !DV_I_48_2),
];

/// Format: Stevens `ubc_check` Stage 1 `ne_same(..., shift = 29)` equation (`tools/data/3565`).
const NE_SAME_29: &[Eq] = &[
  // Format: Stevens `ne_same(shift = 29)` equation.
  (44, 46, !(DV_I_43_0 | DV_I_45_0 | DV_I_47_0)),
];

/// Format: bit shift constants used by the Stage 1 table groups (`tools/data/3565`).
const SHIFT_1: u32 = 1;
const SHIFT_2: u32 = 2;
const SHIFT_4: u32 = 4;
const SHIFT_6: u32 = 6;
// Format: bit shift constants (continued).
const SHIFT_7: u32 = 7;
const SHIFT_29: u32 = 29;

/// Computes the exact 32-bit Stevens UBC mask for eight 64-byte blocks in parallel using auto-vectorized NEON/SSE.
#[inline]
pub(super) fn check_octet(
  blocks: &[[u8; BLOCK_LEN]; OCTET_BLOCKS],
  scratch: &mut OctetScratch,
) -> Octet {
  expand_65_x8(blocks, &mut scratch.w);
  let mut masks = [!0u32; OCTET_BLOCKS];
  apply_eq_same_x8(&mut masks, &scratch.w, SHIFT_29, EQ_SAME_29);
  apply_eq_diff_x8(&mut masks, &scratch.w, SHIFT_4, SHIFT_29, EQ_DIFF_4_29);
  apply_eq_same_x8(&mut masks, &scratch.w, SHIFT_6, EQ_SAME_6);
  apply_eq_diff_x8(&mut masks, &scratch.w, SHIFT_1, SHIFT_6, EQ_DIFF_1_6);
  apply_ne_diff_x8(&mut masks, &scratch.w, SHIFT_1, SHIFT_6, NE_DIFF_1_6);
  apply_ne_same_x8(&mut masks, &scratch.w, SHIFT_1, NE_SAME_1);
  apply_ne_diff_x8(&mut masks, &scratch.w, SHIFT_2, SHIFT_7, NE_DIFF_2_7);
  apply_ne_same_x8(&mut masks, &scratch.w, SHIFT_29, NE_SAME_29);

  let any = masks.iter().fold(0u32, |acc, &m| acc | m);
  if any == 0 {
    return [0; OCTET_BLOCKS];
  }
  for (lane, slot) in masks.iter_mut().enumerate() {
    if *slot != 0 {
      *slot = refine_lane(*slot, &scratch.w, lane);
    }
  }
  masks
}

/// Computes the exact 32-bit Stevens UBC mask for one 64-byte block.
#[inline]
pub(super) fn check_one(block: &[u8; BLOCK_LEN]) -> u32 {
  let w = expand_65_one(block);
  let mut tail = [0u32; TAIL_WORDS];
  if let Some(src) = w.get(FIRST_UBC_WORD..UBC_WORDS) {
    tail.copy_from_slice(src);
  }
  let mut mask = !0u32;
  mask = apply_eq_same_one(mask, &tail, SHIFT_29, EQ_SAME_29);
  mask = apply_eq_diff_one(mask, &tail, SHIFT_4, SHIFT_29, EQ_DIFF_4_29);
  mask = apply_eq_same_one(mask, &tail, SHIFT_6, EQ_SAME_6);
  mask = apply_eq_diff_one(mask, &tail, SHIFT_1, SHIFT_6, EQ_DIFF_1_6);
  mask = apply_ne_diff_one(mask, &tail, SHIFT_1, SHIFT_6, NE_DIFF_1_6);
  mask = apply_ne_same_one(mask, &tail, SHIFT_1, NE_SAME_1);
  mask = apply_ne_diff_one(mask, &tail, SHIFT_2, SHIFT_7, NE_DIFF_2_7);
  mask = apply_ne_same_one(mask, &tail, SHIFT_29, NE_SAME_29);
  if mask == 0 {
    return 0;
  }
  refine_tail(mask, &tail)
}

/// Loads eight 64-byte big-endian blocks into SOA `[u32; 8]` lanes and expands `W[16..65]` via NEON/SSE vector ops.
#[inline]
fn expand_65_x8(blocks: &[[u8; BLOCK_LEN]; OCTET_BLOCKS], w: &mut [Octet; UBC_WORDS]) {
  if let Some(head) = w.get_mut(..INPUT_WORDS) {
    for (word_idx, dst) in head.iter_mut().enumerate() {
      let off = word_idx.saturating_mul(WORD_BYTES);
      for (slot, block) in dst.iter_mut().zip(blocks.iter()) {
        if let Some(chunk) = block
          .get(off..off.saturating_add(WORD_BYTES))
          .and_then(|s| <[u8; WORD_BYTES]>::try_from(s).ok())
        {
          *slot = u32::from_be_bytes(chunk);
        }
      }
    }
  }
  for step in 0..EXPAND_STEPS {
    if let Some([w0, _, w2, _, _, _, _, _, w8, _, _, _, _, w13, _, _, dst, ..]) = w.get_mut(step..) {
      for (out, (((a, b), c), d)) in dst
        .iter_mut()
        .zip(w0.iter().zip(w2.iter()).zip(w8.iter()).zip(w13.iter()))
      {
        *out = (*a ^ *b ^ *c ^ *d).rotate_left(1);
      }
    }
  }
}

/// Vectorized `eq_same` table runner over eight lanes (`keep = !bits`).
#[inline]
fn apply_eq_same_x8(masks: &mut Octet, w: &[Octet; UBC_WORDS], shift: u32, eqs: &[Eq]) {
  for &(i1, i2, keep) in eqs {
    if let (Some(left), Some(right)) = (w.get(i1), w.get(i2)) {
      for ((m, &l), &r) in masks.iter_mut().zip(left).zip(right) {
        let bit = ((l ^ r).wrapping_shr(shift)) & 1;
        *m &= bit.wrapping_sub(1) | keep;
      }
    }
  }
}

/// Vectorized `eq_diff` table runner over eight lanes (`keep = !bits`).
#[inline]
fn apply_eq_diff_x8(
  masks: &mut Octet,
  w: &[Octet; UBC_WORDS],
  shift_l: u32,
  shift_r: u32,
  eqs: &[Eq],
) {
  for &(i1, i2, keep) in eqs {
    if let (Some(left), Some(right)) = (w.get(i1), w.get(i2)) {
      for ((m, &l), &r) in masks.iter_mut().zip(left).zip(right) {
        let bit = (l.wrapping_shr(shift_l) ^ r.wrapping_shr(shift_r)) & 1;
        *m &= bit.wrapping_sub(1) | keep;
      }
    }
  }
}

/// Vectorized `ne_diff` table runner over eight lanes (`keep = !bits`).
#[inline]
fn apply_ne_diff_x8(
  masks: &mut Octet,
  w: &[Octet; UBC_WORDS],
  shift_l: u32,
  shift_r: u32,
  eqs: &[Eq],
) {
  for &(i1, i2, keep) in eqs {
    if let (Some(left), Some(right)) = (w.get(i1), w.get(i2)) {
      for ((m, &l), &r) in masks.iter_mut().zip(left).zip(right) {
        let bit = (l.wrapping_shr(shift_l) ^ r.wrapping_shr(shift_r)) & 1;
        *m &= 0u32.wrapping_sub(bit) | keep;
      }
    }
  }
}

/// Vectorized `ne_same` table runner over eight lanes (`keep = !bits`).
#[inline]
fn apply_ne_same_x8(masks: &mut Octet, w: &[Octet; UBC_WORDS], shift: u32, eqs: &[Eq]) {
  for &(i1, i2, keep) in eqs {
    if let (Some(left), Some(right)) = (w.get(i1), w.get(i2)) {
      for ((m, &l), &r) in masks.iter_mut().zip(left).zip(right) {
        let bit = ((l ^ r).wrapping_shr(shift)) & 1;
        *m &= 0u32.wrapping_sub(bit) | keep;
      }
    }
  }
}

/// Scalar `eq_same` table runner for single-block path.
#[inline]
fn apply_eq_same_one(mut m: u32, t: &[u32; TAIL_WORDS], shift: u32, eqs: &[Eq]) -> u32 {
  for &(i1, i2, keep) in eqs {
    let bit = ((wt1(t, i1) ^ wt1(t, i2)).wrapping_shr(shift)) & 1;
    m &= bit.wrapping_sub(1) | keep;
  }
  m
}

/// Scalar `eq_diff` table runner for single-block path.
#[inline]
fn apply_eq_diff_one(
  mut m: u32,
  t: &[u32; TAIL_WORDS],
  shift_l: u32,
  shift_r: u32,
  eqs: &[Eq],
) -> u32 {
  for &(i1, i2, keep) in eqs {
    let bit = (wt1(t, i1).wrapping_shr(shift_l) ^ wt1(t, i2).wrapping_shr(shift_r)) & 1;
    m &= bit.wrapping_sub(1) | keep;
  }
  m
}

/// Scalar `ne_diff` table runner for single-block path.
#[inline]
fn apply_ne_diff_one(
  mut m: u32,
  t: &[u32; TAIL_WORDS],
  shift_l: u32,
  shift_r: u32,
  eqs: &[Eq],
) -> u32 {
  for &(i1, i2, keep) in eqs {
    let bit = (wt1(t, i1).wrapping_shr(shift_l) ^ wt1(t, i2).wrapping_shr(shift_r)) & 1;
    m &= 0u32.wrapping_sub(bit) | keep;
  }
  m
}

/// Scalar `ne_same` table runner for single-block path.
#[inline]
fn apply_ne_same_one(mut m: u32, t: &[u32; TAIL_WORDS], shift: u32, eqs: &[Eq]) -> u32 {
  for &(i1, i2, keep) in eqs {
    let bit = ((wt1(t, i1) ^ wt1(t, i2)).wrapping_shr(shift)) & 1;
    m &= 0u32.wrapping_sub(bit) | keep;
  }
  m
}

/// Loads one 64-byte big-endian block and expands `W[0..65]` (FIPS 180-4 §6.1.2).
#[inline]
fn expand_65_one(block: &[u8; BLOCK_LEN]) -> [u32; UBC_WORDS] {
  let mut w = [0u32; UBC_WORDS];
  if let Some(head) = w.get_mut(..INPUT_WORDS) {
    for (slot, chunk) in head.iter_mut().zip(block.as_chunks::<WORD_BYTES>().0) {
      *slot = u32::from_be_bytes(*chunk);
    }
  }
  for step in 0..EXPAND_STEPS {
    if let Some([w0, _, w2, _, _, _, _, _, w8, _, _, _, _, w13, _, _, dst, ..]) = w.get_mut(step..) {
      *dst = (*w13 ^ *w8 ^ *w2 ^ *w0).rotate_left(1);
    }
  }
  w
}

/// Reads schedule word `W[index]` (`index >= 35`) from the 30-word scalar tail.
#[inline(always)]
fn wt1(tail: &[u32; TAIL_WORDS], index: usize) -> u32 {
  tail
    .get(index.saturating_sub(FIRST_UBC_WORD))
    .copied()
    .unwrap_or(0)
}

/// Scalar `eq_same` check for Stage 2 refinement.
#[inline(always)]
fn eq_same(mask: u32, left: u32, right: u32, shift: u32, bits: u32) -> u32 {
  if ((left ^ right) & 1u32.wrapping_shl(shift)) != 0 {
    mask & !bits
  } else {
    mask
  }
}

/// Scalar `eq_diff` check (`shift_r >= shift_l`) for Stage 2 refinement.
#[inline(always)]
fn eq_diff(mask: u32, left: u32, shift_l: u32, right: u32, shift_r: u32, bits: u32) -> u32 {
  let delta = shift_r.saturating_sub(shift_l);
  if ((left ^ right.wrapping_shr(delta)) & 1u32.wrapping_shl(shift_l)) != 0 {
    mask & !bits
  } else {
    mask
  }
}

/// Scalar `ne_diff` check (`shift_r >= shift_l`) for Stage 2 refinement.
#[inline(always)]
fn ne_diff(mask: u32, left: u32, shift_l: u32, right: u32, shift_r: u32, bits: u32) -> u32 {
  let delta = shift_r.saturating_sub(shift_l);
  if ((left ^ right.wrapping_shr(delta)) & 1u32.wrapping_shl(shift_l)) == 0 {
    mask & !bits
  } else {
    mask
  }
}

/// Scalar `ne_same` check for Stage 2 refinement.
#[inline(always)]
fn ne_same(mask: u32, left: u32, right: u32, shift: u32, bits: u32) -> u32 {
  if ((left ^ right) & 1u32.wrapping_shl(shift)) == 0 {
    mask & !bits
  } else {
    mask
  }
}

/// Extracts only the 30-word tail `W[35..65]` of one surviving lane and runs guarded Stage 2 refinement.
#[cold]
#[inline(never)]
fn refine_lane(mask: u32, w: &[Octet; UBC_WORDS], lane: usize) -> u32 {
  let mut scalar = [0u32; TAIL_WORDS];
  if let Some(tail) = w.get(FIRST_UBC_WORD..UBC_WORDS) {
    for (dst, words) in scalar.iter_mut().zip(tail.iter()) {
      *dst = words.get(lane).copied().unwrap_or(0);
    }
  }
  refine_tail(mask, &scalar)
}

/// Guarded Stage 2 refinement over a 30-word scalar tail: skips entire `b = 0` or `b = 2` families when none of
/// their bits survived Stage 1.
fn refine_tail(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  if (m & ALL_B0) != 0 {
    m = refine_b0_groups_a(m, t);
    m = refine_b0_groups_b(m, t);
    if (m & TYPE_I_B0) != 0 {
      m = refine_b0_single_i(m, t);
    }
    if (m & TYPE_II_B0) != 0 {
      m = refine_b0_single_ii_low(m, t);
      m = refine_b0_single_ii_high(m, t);
    }
  }
  if (m & RESIDUAL_B2) != 0 {
    m = refine_b2_i(m, t);
    m = refine_b2_ii(m, t);
  }
  m
}

/// Guarded `b = 0` multi-DV equations (part 1).
fn refine_b0_groups_a(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  // Format: Stevens `ubc_check` multi-DV `b = 0` equations (`tools/data/3565`).
  m = eq_same(m, wt1(t, 55), wt1(t, 56), 29, DV_I_52_0 | DV_II_48_0 | DV_II_51_0 | DV_II_56_0);
  m = eq_diff(m, wt1(t, 52), 4, wt1(t, 55), 29, DV_I_52_0 | DV_II_48_0 | DV_II_50_0 | DV_II_56_0);
  m = eq_diff(m, wt1(t, 51), 4, wt1(t, 54), 29, DV_I_51_0 | DV_II_47_0 | DV_II_49_0 | DV_II_55_0);
  m = eq_diff(m, wt1(t, 36), 4, wt1(t, 40), 29, DV_I_46_0 | DV_I_49_0 | DV_II_45_0 | DV_II_48_0);
  // Format: Stevens `ubc_check` multi-DV `b = 0` equations (continued).
  m = ne_same(m, wt1(t, 53), wt1(t, 56), 29, DV_I_52_0 | DV_II_48_0 | DV_II_49_0);
  m = ne_same(m, wt1(t, 51), wt1(t, 54), 29, DV_I_50_0 | DV_II_46_0 | DV_II_47_0);
  m = ne_same(m, wt1(t, 50), wt1(t, 52), 29, DV_I_49_0 | DV_I_51_0 | DV_II_45_0);
  m = ne_same(m, wt1(t, 49), wt1(t, 51), 29, DV_I_48_0 | DV_I_50_0 | DV_I_52_0);
  // Format: Stevens `ubc_check` multi-DV `b = 0` equations (continued).
  m = ne_same(m, wt1(t, 48), wt1(t, 50), 29, DV_I_47_0 | DV_I_49_0 | DV_I_51_0);
  m = ne_same(m, wt1(t, 47), wt1(t, 49), 29, DV_I_46_0 | DV_I_48_0 | DV_I_50_0);
  m = ne_same(m, wt1(t, 46), wt1(t, 48), 29, DV_I_45_0 | DV_I_47_0 | DV_I_49_0);
  m = ne_same(m, wt1(t, 45), wt1(t, 47), 29, DV_I_44_0 | DV_I_46_0 | DV_I_48_0);
  // Format: Stevens `ubc_check` multi-DV `b = 0` equations (continued).
  m = ne_same(m, wt1(t, 40), wt1(t, 42), 4, DV_I_44_0 | DV_I_46_0 | DV_II_56_0);
  m = ne_same(m, wt1(t, 39), wt1(t, 41), 4, DV_I_43_0 | DV_I_45_0 | DV_II_55_0);
  m = ne_same(m, wt1(t, 38), wt1(t, 40), 4, DV_I_44_0 | DV_II_54_0 | DV_II_56_0);
  m = ne_same(m, wt1(t, 37), wt1(t, 39), 4, DV_I_43_0 | DV_II_53_0 | DV_II_55_0);
  // Format: Stevens `ubc_check` multi-DV `b = 0` equations (continued).
  eq_diff(m, wt1(t, 35), 4, wt1(t, 39), 29, DV_I_45_0 | DV_I_48_0 | DV_II_47_0)
}

/// Guarded `b = 0` multi-DV equations (part 2).
fn refine_b0_groups_b(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  // Format: Stevens `ubc_check` two-DV `b = 0` equations (`tools/data/3565`).
  m = ne_diff(m, wt1(t, 63), 0, wt1(t, 64), 5, DV_I_48_0 | DV_II_48_0);
  m = ne_diff(m, wt1(t, 63), 1, wt1(t, 64), 6, DV_I_45_0 | DV_II_45_0);
  m = ne_diff(m, wt1(t, 62), 0, wt1(t, 63), 5, DV_I_47_0 | DV_II_47_0);
  m = ne_diff(m, wt1(t, 61), 0, wt1(t, 62), 5, DV_I_46_0 | DV_II_46_0);
  // Format: Stevens `ubc_check` two-DV `b = 0` equations (continued).
  m = ne_diff(m, wt1(t, 60), 0, wt1(t, 61), 5, DV_I_45_0 | DV_II_45_0);
  m = eq_same(m, wt1(t, 58), wt1(t, 59), 29, DV_II_51_0 | DV_II_54_0);
  m = eq_same(m, wt1(t, 57), wt1(t, 58), 29, DV_II_50_0 | DV_II_53_0);
  m = eq_diff(m, wt1(t, 56), 4, wt1(t, 59), 29, DV_II_52_0 | DV_II_54_0);
  // Format: Stevens `ubc_check` two-DV `b = 0` equations (continued).
  m = ne_same(m, wt1(t, 56), wt1(t, 59), 29, DV_II_51_0 | DV_II_52_0);
  m = eq_same(m, wt1(t, 56), wt1(t, 57), 29, DV_II_49_0 | DV_II_52_0);
  m = eq_diff(m, wt1(t, 55), 4, wt1(t, 58), 29, DV_II_51_0 | DV_II_53_0);
  m = eq_diff(m, wt1(t, 54), 4, wt1(t, 57), 29, DV_II_50_0 | DV_II_52_0);
  // Format: Stevens `ubc_check` two-DV `b = 0` equations (continued).
  m = eq_diff(m, wt1(t, 53), 4, wt1(t, 56), 29, DV_II_49_0 | DV_II_51_0);
  m = ne_same(m, wt1(t, 48), wt1(t, 55), 29, DV_I_51_0 | DV_I_52_0);
  m = eq_diff(m, wt1(t, 39), 4, wt1(t, 43), 29, DV_I_52_0 | DV_II_51_0);
  m = eq_diff(m, wt1(t, 38), 4, wt1(t, 42), 29, DV_I_51_0 | DV_II_50_0);
  // Format: Stevens `ubc_check` two-DV `b = 0` equations (continued).
  m = eq_diff(m, wt1(t, 37), 4, wt1(t, 41), 29, DV_I_50_0 | DV_II_49_0);
  m = ne_same(m, wt1(t, 36), wt1(t, 38), 4, DV_II_52_0 | DV_II_54_0);
  eq_diff(m, wt1(t, 35), 3, wt1(t, 39), 28, DV_I_51_0 | DV_II_47_0)
}

/// Single-DV `b = 0` type-I equations.
fn refine_b0_single_i(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  // Format: Stevens `ubc_check` single-DV `b = 0` type-I equations (`tools/data/3565`).
  m = ne_diff(m, wt1(t, 61), 1, wt1(t, 62), 6, DV_I_43_0);
  m = eq_diff(m, wt1(t, 59), 5, wt1(t, 63), 30, DV_I_43_0);
  m = ne_diff(m, wt1(t, 58), 0, wt1(t, 63), 30, DV_I_43_0);
  m = ne_diff(m, wt1(t, 62), 1, wt1(t, 63), 6, DV_I_44_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-I equations (continued).
  m = eq_diff(m, wt1(t, 60), 5, wt1(t, 64), 30, DV_I_44_0);
  m = ne_diff(m, wt1(t, 59), 0, wt1(t, 64), 30, DV_I_44_0);
  m = ne_same(m, wt1(t, 36), wt1(t, 37), 4, DV_I_50_0);
  m = ne_same(m, wt1(t, 37), wt1(t, 38), 4, DV_I_51_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-I equations (continued).
  ne_same(m, wt1(t, 38), wt1(t, 39), 4, DV_I_52_0)
}

/// Single-DV `b = 0` type-II equations (`48..=52`).
fn refine_b0_single_ii_low(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (`tools/data/3565`).
  m = eq_diff(m, wt1(t, 36), 3, wt1(t, 40), 28, DV_II_48_0);
  m = ne_diff(m, wt1(t, 40), 28, wt1(t, 35), 30, DV_II_48_0);
  m = eq_diff(m, wt1(t, 37), 3, wt1(t, 41), 28, DV_II_49_0);
  m = ne_diff(m, wt1(t, 41), 28, wt1(t, 36), 30, DV_II_49_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (continued).
  m = ne_same(m, wt1(t, 55), wt1(t, 58), 29, DV_II_50_0);
  m = eq_diff(m, wt1(t, 38), 3, wt1(t, 42), 28, DV_II_50_0);
  m = ne_diff(m, wt1(t, 42), 28, wt1(t, 37), 30, DV_II_50_0);
  m = eq_diff(m, wt1(t, 39), 3, wt1(t, 43), 28, DV_II_51_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (continued).
  m = ne_diff(m, wt1(t, 43), 28, wt1(t, 38), 30, DV_II_51_0);
  m = eq_same(m, wt1(t, 59), wt1(t, 60), 29, DV_II_52_0);
  m = eq_diff(m, wt1(t, 40), 3, wt1(t, 44), 28, DV_II_52_0);
  m = eq_diff(m, wt1(t, 40), 4, wt1(t, 44), 29, DV_II_52_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (continued).
  ne_diff(m, wt1(t, 44), 28, wt1(t, 39), 30, DV_II_52_0)
}

/// Single-DV `b = 0` type-II equations (`53..=56`).
fn refine_b0_single_ii_high(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (`tools/data/3565`).
  m = ne_same(m, wt1(t, 58), wt1(t, 61), 29, DV_II_53_0);
  m = eq_diff(m, wt1(t, 57), 4, wt1(t, 61), 29, DV_II_53_0);
  m = eq_diff(m, wt1(t, 41), 3, wt1(t, 45), 28, DV_II_53_0);
  m = eq_diff(m, wt1(t, 41), 4, wt1(t, 45), 29, DV_II_53_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (continued).
  m = eq_diff(m, wt1(t, 58), 4, wt1(t, 62), 29, DV_II_54_0);
  m = eq_diff(m, wt1(t, 42), 3, wt1(t, 46), 28, DV_II_54_0);
  m = eq_diff(m, wt1(t, 42), 4, wt1(t, 46), 29, DV_II_54_0);
  m = eq_diff(m, wt1(t, 59), 4, wt1(t, 63), 29, DV_II_55_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (continued).
  m = eq_diff(m, wt1(t, 57), 4, wt1(t, 59), 29, DV_II_55_0);
  m = eq_diff(m, wt1(t, 43), 3, wt1(t, 47), 28, DV_II_55_0);
  m = eq_diff(m, wt1(t, 43), 4, wt1(t, 47), 29, DV_II_55_0);
  m = eq_diff(m, wt1(t, 60), 4, wt1(t, 64), 29, DV_II_56_0);
  // Format: Stevens `ubc_check` single-DV `b = 0` type-II equations (continued).
  m = eq_diff(m, wt1(t, 44), 3, wt1(t, 48), 28, DV_II_56_0);
  eq_diff(m, wt1(t, 44), 4, wt1(t, 48), 29, DV_II_56_0)
}

/// Residual `b = 2` type-I equations not already evaluated in Stage 1.
fn refine_b2_i(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  if (m & (DV_I_46_2 | DV_I_47_2 | DV_I_50_2)) != 0 {
    // Format: Stevens `ubc_check` residual `b = 2` type-I equations (`tools/data/3565`).
    m = eq_same(m, wt1(t, 40), wt1(t, 42), 6, DV_I_46_2);
    m = ne_diff(m, wt1(t, 62), 2, wt1(t, 63), 7, DV_I_47_2);
    m = eq_same(m, wt1(t, 41), wt1(t, 43), 6, DV_I_47_2);
    m = ne_same(m, wt1(t, 43), wt1(t, 51), 1, DV_I_50_2);
  }
  if (m & DV_I_51_2) != 0 {
    // Format: Stevens `ubc_check` residual `b = 2` type-I equations (continued).
    m = eq_diff(m, wt1(t, 37), 1, wt1(t, 37), 6, DV_I_51_2);
    m = eq_diff(m, wt1(t, 35), 5, wt1(t, 39), 30, DV_I_51_2);
  }
  m
}

/// Residual `b = 2` type-II equations not already evaluated in Stage 1.
fn refine_b2_ii(mut m: u32, t: &[u32; TAIL_WORDS]) -> u32 {
  if (m & DV_II_46_2) != 0 {
    // Format: Stevens `ubc_check` residual `b = 2` type-II equations (`tools/data/3565`).
    m = ne_same(m, wt1(t, 47), wt1(t, 51), 1, DV_II_46_2);
  }
  if (m & (DV_II_49_2 | DV_II_50_2 | DV_II_51_2)) != 0 {
    // Format: Stevens `ubc_check` residual `b = 2` type-II equations (continued).
    m = eq_diff(m, wt1(t, 37), 5, wt1(t, 41), 30, DV_II_49_2);
    m = ne_diff(m, wt1(t, 36), 0, wt1(t, 41), 30, DV_II_49_2);
    m = eq_diff(m, wt1(t, 38), 5, wt1(t, 42), 30, DV_II_50_2);
    m = ne_diff(m, wt1(t, 37), 0, wt1(t, 42), 30, DV_II_50_2);
    // Format: Stevens `ubc_check` residual `b = 2` type-II equations (continued).
    m = eq_diff(m, wt1(t, 39), 5, wt1(t, 43), 30, DV_II_51_2);
    m = ne_diff(m, wt1(t, 38), 0, wt1(t, 43), 30, DV_II_51_2);
  }
  m
}
