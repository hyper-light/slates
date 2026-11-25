//! Differential oracle, 32-DV table/UBC/recompression verification, SHAttered attack vectors, boundary lengths,
//! and non-vacuity tests for [`super`] (A-893).

use proptest::prelude::*;
use sha1_checked::{Digest, Sha1};

use super::{
  BLOCK_LEN, CollisionAttack, DIGEST_LEN, Hasher, OCTET_BLOCKS, hash, oracle, recompress, ubc,
};

/// Format: first 320 bytes of Google/CWI's SHAttered PDF #1 (`git` t0013 / `hg` sha1dc collision prefix).
const SHATTERED_1: &[u8] = include_bytes!("../../../hg/fixtures/sha1dc/shattered-1.prefix.bin");

/// Format: first 320 bytes of Google/CWI's SHAttered PDF #2 (differs at bytes `192..320`, same plain SHA-1).
const SHATTERED_2: &[u8] = include_bytes!("../../../hg/fixtures/sha1dc/shattered-2.prefix.bin");

/// Oracle: RustCrypto's `sha1-checked` configured identically to git (`sha1dc_git.c`) and Mercurial 7.2
/// (`thirdparty/sha1dc/cext.c`): detection on, safe hash off.
fn sha1_checked_oracle(bytes: &[u8]) -> Result<[u8; DIGEST_LEN], CollisionAttack> {
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

/// Deterministic SplitMix64 generator for reproducible random blocks and states.
struct SplitMix64(u64);

impl SplitMix64 {
  /// Advances the generator and returns the next 64-bit word.
  fn next_u64(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = self.0;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
  }

  /// Returns the low 32 bits of the next word.
  fn next_u32(&mut self) -> u32 {
    u32::try_from(self.next_u64() & 0xffff_ffff).unwrap_or(0)
  }

  /// Returns one pseudorandom 64-byte SHA-1 block.
  fn next_block(&mut self) -> [u8; BLOCK_LEN] {
    let mut block = [0u8; BLOCK_LEN];
    for chunk in block.as_chunks_mut::<8>().0 {
      *chunk = self.next_u64().to_le_bytes();
    }
    block
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
  let expected = sha1_checked_oracle(slice);
  assert_eq!(
    hash(slice),
    expected,
    "one-shot mismatch at len {}",
    slice.len()
  );
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
/// 55/56/63/64/119/120/255/256/257/512 block and octet boundaries) both one-shot and streamed. Expect bit-for-bit
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

/// A-893 (non-vacuity): do hash 4,096 pseudorandom blocks (256 KiB). Expect the 8-lane SIMD path
/// (`octet_batches == 512`), the hardware fast path (`fast_blocks > 3000`), and the Stevens recompression slow path
/// (`recompressed_blocks > 20`) all to execute and match `sha1-checked`.
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
  let octets = hasher.octet_batches();
  assert_eq!(octets, 512, "8-lane octet path count mismatch: {octets}");
  assert!(fast > 3000, "hardware fast path did not fire: fast={fast}");
  assert!(
    recomp > 20,
    "recompression path did not fire: recomp={recomp}"
  );
  assert_eq!(hasher.finish(), sha1_checked_oracle(&corpus));
}

/// Verifies `cut` triggers `expected_err` both one-shot and under prime chunk splits.
fn assert_shattered_cut(cut: &[u8], expected_err: CollisionAttack) {
  assert_eq!(hash(cut), Err(expected_err));
  for step in [1usize, 7, 31, 63, 64, 65, 192, 256] {
    let mut hasher = Hasher::new();
    for piece in cut.chunks(step) {
      hasher.update(piece);
    }
    assert_eq!(
      hasher.finish(),
      Err(expected_err),
      "missed collision at step {step}"
    );
  }
}

/// A-893 (SHAttered attack vectors, git t0013 / hg sha1dc): do hash both SHAttered 320-byte prefixes across
/// alignments and all-zero dense-UBC blocks. Expect `Err(CollisionAttack)` carrying the exact plain SHA-1 digest
/// `f92d74e3…`, and `Ok` for the 192-byte shared prefix before the colliding blocks.
#[test]
fn shattered_pair_detected_across_all_alignments() {
  assert_ne!(SHATTERED_1, SHATTERED_2);

  let expected_err = sha1_checked_oracle(SHATTERED_1).unwrap_err();
  assert_eq!(
    to_hex(&expected_err.sha1),
    "f92d74e3874587aaf443d1db961d4e26dde13e9c"
  );
  assert_eq!(sha1_checked_oracle(SHATTERED_2), Err(expected_err));
  assert_shattered_cut(SHATTERED_1, expected_err);
  assert_shattered_cut(SHATTERED_2, expected_err);

  assert_eq!(
    hash(&SHATTERED_1[..192]),
    sha1_checked_oracle(&SHATTERED_1[..192])
  );
  assert!(hash(&SHATTERED_1[..192]).is_ok());

  let zeros = [0u8; 640];
  assert_eq!(hash(&zeros), sha1_checked_oracle(&zeros));
}

/// Builds a `320 + suffix_len` colliding pair from `SHATTERED_1`/`SHATTERED_2` and asserts both trigger
/// `CollisionAttack` through the 8-lane SIMD octet path.
fn assert_shattered_octet_collision(suffix_len: usize) {
  let mut suffix = vec![0u8; suffix_len];
  let mut seed: u64 = 0x2026_1125_54a7_7e01;
  for byte in &mut suffix {
    *byte = next_byte(&mut seed);
  }

  let mut msg_1 = Vec::with_capacity(SHATTERED_1.len() + suffix_len);
  msg_1.extend_from_slice(SHATTERED_1);
  msg_1.extend_from_slice(&suffix);

  let mut msg_2 = Vec::with_capacity(SHATTERED_2.len() + suffix_len);
  msg_2.extend_from_slice(SHATTERED_2);
  msg_2.extend_from_slice(&suffix);

  assert!(msg_1.len() >= OCTET_BLOCKS * BLOCK_LEN);
  assert_ne!(msg_1, msg_2);

  let expected = sha1_checked_oracle(&msg_1).unwrap_err();
  assert_eq!(sha1_checked_oracle(&msg_2), Err(expected));

  for msg in [&msg_1, &msg_2] {
    let mut hasher = Hasher::new();
    hasher.update(msg);
    let octets = hasher.octet_batches();
    let fast = hasher.fast_blocks();
    let recomp = hasher.recompressed_blocks();
    assert_eq!(hasher.finish(), Err(expected));
    assert!(octets >= 1, "octet path did not fire for len {}", msg.len());
    assert!(fast > 0, "hardware fast path did not fire");
    assert!(recomp > 0, "recompression slow path did not fire");
  }
}

/// A-893 (SHAttered `>= 512` bytes on 8-lane SIMD path): do extend both SHAttered prefixes with identical suffixes
/// to 576 bytes (1 full octet + 1 trailing block) and 1,024 bytes (2 full octets, 0 trailing blocks) so the colliding
/// blocks at bytes `192..320` execute inside `process_octets` / `ubc::check_octet`. Expect `Err(CollisionAttack)`
/// matching `sha1-checked` and non-vacuous execution of the octet, fast, and recompression paths.
#[test]
fn shattered_over_512_bytes_drives_octet_simd_path_and_detects_collision() {
  assert_shattered_octet_collision(256);
  assert_shattered_octet_collision(704);
}

/// A-893 (32-DV table oracle): do compare every one of the 32 compressed `DVS[0..32]` entries (`dm[80]` and test
/// step `58`/`65`) against the full reference `oracle::SHA1_DVS` table. Expect bit-for-bit equality on all 32 DVs.
#[test]
fn all_32_disturbance_vectors_match_reference_tables() {
  for (index, info) in oracle::SHA1_DVS.iter().enumerate() {
    assert_eq!(info.mask_bit, index);
    let (dm, step) = recompress::dv_entry(info.mask_bit).unwrap_or_else(|| {
      panic!(
        "missing DVS entry for bit {} (type {} K={} b={})",
        info.mask_bit, info.dv_type, info.dv_k, info.dv_b
      )
    });
    assert_eq!(
      step, info.test_step,
      "test step mismatch at bit {} (type {} K={} b={})",
      info.mask_bit, info.dv_type, info.dv_k, info.dv_b
    );
    assert_eq!(
      dm, &info.dm,
      "dm[80] mismatch at bit {} (type {} K={} b={})",
      info.mask_bit, info.dv_type, info.dv_k, info.dv_b
    );
  }
}

/// Builds the 1,026 structured UBC edge blocks: all-zero, all-0xff, every single-1-bit, and every single-0-bit block.
fn structured_edge_blocks() -> Vec<[u8; BLOCK_LEN]> {
  let mut blocks = Vec::with_capacity(2 + 2 * 512);
  blocks.push([0u8; BLOCK_LEN]);
  blocks.push([0xffu8; BLOCK_LEN]);
  for bit in 0..512usize {
    let mut one_hot = [0u8; BLOCK_LEN];
    one_hot[bit / 8] = 0x80 >> (bit % 8);
    blocks.push(one_hot);
    let mut one_cold = [0xffu8; BLOCK_LEN];
    one_cold[bit / 8] ^= 0x80 >> (bit % 8);
    blocks.push(one_cold);
  }
  blocks
}

/// A-893 (full 32-bit UBC differential oracle): do compare `ubc::check_one` and `ubc::check_octet` bit-for-bit
/// against the independent reference `oracle::ubc_check` across all-zero, all-0xff, all 1,024 single-bit blocks, and
/// seeded random blocks until every one of the 32 disturbance-vector mask bits (`0..32`) has fired non-vacuously.
#[test]
fn ubc_one_and_octet_match_reference_oracle_covering_all_32_bits() {
  let mut rng = SplitMix64(0x5eed_0005_1dc0_2026);
  let mut structured = structured_edge_blocks().into_iter();
  let mut scratch = ubc::OctetScratch::new();
  let mut seen_bits: u32 = 0;
  let mut blocks_checked: u64 = 0;
  let min_blocks: u64 = 16_384;
  let max_blocks: u64 = 2_000_000;

  while blocks_checked < max_blocks && (blocks_checked < min_blocks || seen_bits != u32::MAX) {
    let mut octet = [[0u8; BLOCK_LEN]; OCTET_BLOCKS];
    for lane in &mut octet {
      *lane = structured.next().unwrap_or_else(|| rng.next_block());
    }
    let octet_masks = ubc::check_octet(&octet, &mut scratch);
    for (lane, block) in octet.iter().enumerate() {
      let ref_mask = oracle::ubc_check(&oracle::expand_ref(block));
      let one_mask = ubc::check_one(block);
      assert_eq!(
        one_mask, ref_mask,
        "check_one mismatch vs reference at block {blocks_checked} lane {lane}"
      );
      assert_eq!(
        octet_masks[lane], ref_mask,
        "check_octet mismatch vs reference at block {blocks_checked} lane {lane}"
      );
      seen_bits |= ref_mask;
      blocks_checked += 1;
    }
  }
  assert_eq!(
    seen_bits,
    u32::MAX,
    "not all 32 UBC mask bits fired within {blocks_checked} blocks: missing {:#010x}",
    !seen_bits
  );
}

/// A-893 (recompression identity & Miri scalar parity): do run unperturbed `recompress_58`, `recompress_65`,
/// `recompress::compress_block`, hardware `sha1::block_api::compress` (outside Miri), and `compress_and_check` with
/// all 32 DVs forced on seeded random `(ihv, block)` pairs. Expect every state to match `oracle::compress_ref` and
/// zero false-positive collisions.
#[test]
fn unperturbed_recompress_58_and_65_rebuild_post_compression_state() {
  let mut rng = SplitMix64(0x7e57_5865_c0de_2026);
  for step in 0..1024usize {
    let ihv = [
      rng.next_u32(),
      rng.next_u32(),
      rng.next_u32(),
      rng.next_u32(),
      rng.next_u32(),
    ];
    let block = rng.next_block();
    let mut expected = ihv;
    oracle::compress_ref(&mut expected, &block);

    let (compressed, r58, r65) = recompress::probe_unperturbed_recompress(ihv, &block);
    assert_eq!(
      compressed, expected,
      "compress_with_states mismatch at {step}"
    );
    assert_eq!(
      r58, expected,
      "unperturbed recompress_58 mismatch at {step}"
    );
    assert_eq!(
      r65, expected,
      "unperturbed recompress_65 mismatch at {step}"
    );

    let mut scalar = ihv;
    recompress::compress_block(&mut scalar, &block);
    assert_eq!(scalar, expected, "scalar compress_block mismatch at {step}");

    #[cfg(not(miri))]
    {
      let mut hardware = ihv;
      sha1::block_api::compress(&mut hardware, core::slice::from_ref(&block));
      assert_eq!(hardware, expected, "hardware compress mismatch at {step}");
    }

    if step < 256 {
      let mut checked = ihv;
      let hit = recompress::compress_and_check(&mut checked, &block, !0);
      assert!(!hit, "false-positive collision at step {step}");
      assert_eq!(
        checked, expected,
        "compress_and_check state mismatch at {step}"
      );
    }
  }
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
    let expected = sha1_checked_oracle(&data);
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
