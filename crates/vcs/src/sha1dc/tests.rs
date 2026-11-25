//! Differential oracle, SHAttered attack vectors, boundary lengths and non-vacuity tests for [`super`] (A-893).

use proptest::prelude::*;
use sha1_checked::{Digest, Sha1};

use super::{BLOCK_LEN, CollisionAttack, DIGEST_LEN, Hasher, hash, ubc};

/// Oracle: RustCrypto's `sha1-checked` configured identically to git (`sha1dc_git.c`) and Mercurial 7.2
/// (`thirdparty/sha1dc/cext.c`): detection on, safe hash off.
fn oracle(bytes: &[u8]) -> Result<[u8; DIGEST_LEN], CollisionAttack> {
  let mut state = Sha1::builder()
    .detect_collision(true)
    .safe_hash(false)
    .build();
  Digest::update(&mut state, bytes);
  let result = state.try_finalize();
  let mut sha1 = [0u8; DIGEST_LEN];
  sha1.copy_from_slice(result.hash());
  if result.has_collision() {
    Err(CollisionAttack { sha1 })
  } else {
    Ok(sha1)
  }
}

/// Deterministic LCG byte generator for test vectors without truncation casts.
fn next_byte(seed: &mut u64) -> u8 {
  *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
  u8::try_from((*seed >> 33) & 0xff).unwrap_or(0)
}

/// Formats a 20-byte digest as lower-case hexadecimal.
fn to_hex(bytes: &[u8; DIGEST_LEN]) -> String {
  let mut out = String::with_capacity(DIGEST_LEN * 2);
  for byte in bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// Checks both one-shot and 17-byte streamed hashing of `slice` against `sha1-checked`.
fn assert_matches_oracle(slice: &[u8]) {
  let expected = oracle(slice);
  assert_eq!(hash(slice), expected, "one-shot mismatch at len {}", slice.len());
  let mut streamed = Hasher::new();
  for piece in slice.chunks(17) {
    streamed.update(piece);
  }
  assert_eq!(
    streamed.finish(),
    expected,
    "streamed mismatch at len {}",
    slice.len()
  );
}

/// A-893 (golden + boundary lengths): do hash empty, FIPS 180-4 `"abc"`, and every byte length `0..=600` (crossing
/// 55/56/63/64/119/120/255/256/257/512 block and quad boundaries) both one-shot and streamed. Expect bit-for-bit
/// equality with `sha1-checked`.
#[test]
fn golden_and_boundary_lengths_match_oracle() {
  assert_eq!(
    to_hex(&hash(b"").unwrap()),
    "da39a3ee5e6b4b0d3255bfef95601890afd80709"
  );
  assert_eq!(
    to_hex(&hash(b"abc").unwrap()),
    "a9993e364706816aba3e25717850c26c9cd0d89d"
  );

  let mut buf = Vec::with_capacity(600);
  let mut seed: u64 = 0x51a7_e501_2026_1125;
  for _ in 0..600 {
    buf.push(next_byte(&mut seed));
  }
  for len in 0..=600 {
    assert_matches_oracle(&buf[..len]);
  }
}

/// A-893 (non-vacuity): do hash 4,096 pseudorandom blocks (256 KiB). Expect BOTH the hardware fast path
/// (`fast_blocks > 3000`) AND the Stevens recompression slow path (`recompressed_blocks > 20`) to execute and match
/// `sha1-checked`.
#[test]
fn both_hardware_fast_path_and_recompression_path_execute() {
  let mut seed: u64 = 0x51a7_e501_2026_1125;
  let mut corpus = vec![0u8; 4096 * BLOCK_LEN];
  for byte in &mut corpus {
    *byte = next_byte(&mut seed);
  }
  let mut hasher = Hasher::new();
  hasher.update(&corpus);
  let fast = hasher.fast_blocks();
  let recomp = hasher.recompressed_blocks();
  assert!(fast > 3000, "hardware fast path did not fire: fast={fast}");
  assert!(recomp > 20, "recompression path did not fire: recomp={recomp}");
  assert_eq!(hasher.finish(), oracle(&corpus));
}

/// Verifies `cut` triggers `expected_err` both one-shot and under prime chunk splits.
fn assert_shattered_cut(cut: &[u8], expected_err: CollisionAttack) {
  assert_eq!(hash(cut), Err(expected_err));
  for step in [1usize, 7, 31, 63, 64, 65, 192, 256] {
    let mut hasher = Hasher::new();
    for piece in cut.chunks(step) {
      hasher.update(piece);
    }
    assert_eq!(hasher.finish(), Err(expected_err), "missed collision at step {step}");
  }
}

/// A-893 (SHAttered attack vectors, git t0013 / hg sha1dc): do hash both SHAttered 320-byte prefixes across
/// alignments and all-zero dense-UBC blocks. Expect `Err(CollisionAttack)` carrying the exact plain SHA-1 digest
/// `f92d74e3…`, and `Ok` for the 192-byte shared prefix before the colliding blocks.
#[test]
fn shattered_pair_detected_across_all_alignments() {
  const SHATTERED_1: &[u8] = include_bytes!("../../../hg/fixtures/sha1dc/shattered-1.prefix.bin");
  const SHATTERED_2: &[u8] = include_bytes!("../../../hg/fixtures/sha1dc/shattered-2.prefix.bin");
  assert_ne!(SHATTERED_1, SHATTERED_2);

  let expected_err = oracle(SHATTERED_1).unwrap_err();
  assert_eq!(
    to_hex(&expected_err.sha1),
    "f92d74e3874587aaf443d1db961d4e26dde13e9c"
  );
  assert_eq!(oracle(SHATTERED_2), Err(expected_err));
  assert_shattered_cut(SHATTERED_1, expected_err);
  assert_shattered_cut(SHATTERED_2, expected_err);

  assert_eq!(hash(&SHATTERED_1[..192]), oracle(&SHATTERED_1[..192]));
  assert!(hash(&SHATTERED_1[..192]).is_ok());

  let zeros = [0u8; 640];
  assert_eq!(hash(&zeros), oracle(&zeros));
}

/// A-893 (octet vs scalar UBC equivalence): do compare `ubc::check_octet` against eight calls to `ubc::check_one`
/// across pseudorandom blocks and all-zero / all-0xff edge blocks. Expect exact 32-bit mask equality and broad
/// disturbance-vector coverage.
#[test]
fn ubc_octet_matches_scalar_on_every_lane() {
  let mut seed: u64 = 0xdead_beef_cafe_babe;
  let mut octet = [[0u8; BLOCK_LEN]; 8];
  let mut scratch = ubc::OctetScratch::new();
  let mut seen_nonzero_mask: u32 = 0;

  for round in 0..2048u32 {
    for (lane, block) in octet.iter_mut().enumerate() {
      if round == 0 && lane == 0 {
        block.fill(0);
      } else if round == 0 && lane == 1 {
        block.fill(0xff);
      } else {
        for byte in block.iter_mut() {
          *byte = next_byte(&mut seed);
        }
      }
    }
    let om = ubc::check_octet(&octet, &mut scratch);
    for (lane, block) in octet.iter().enumerate() {
      let sm = ubc::check_one(block);
      assert_eq!(om[lane], sm, "UBC mask mismatch at round {round} lane {lane}");
      seen_nonzero_mask |= sm;
    }
  }
  assert_ne!(
    seen_nonzero_mask, 0,
    "expected multiple UBC bits to trigger across 16,384 blocks"
  );
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-893 (differential oracle): do hash any byte sequence in arbitrary chunk splits through `Hasher` and through
  /// `sha1-checked`. Expect identical digest and collision verdict every time.
  #[test]
  fn arbitrary_inputs_and_splits_match_sha1_checked(
    data in proptest::collection::vec(any::<u8>(), 0..1024),
    splits in proptest::collection::vec(1usize..128, 1..16),
  ) {
    let expected = oracle(&data);
    prop_assert_eq!(hash(&data), expected);

    let mut streamed = Hasher::new();
    let mut pos = 0usize;
    let mut idx = 0usize;
    while pos < data.len() {
      let step = splits[idx % splits.len()];
      let end = pos.saturating_add(step).min(data.len());
      streamed.update(&data[pos..end]);
      pos = end;
      idx += 1;
    }
    prop_assert_eq!(streamed.finish(), expected);
  }
}
