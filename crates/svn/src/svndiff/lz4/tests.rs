//! The resumable LZ4 block's tests (A-751): `lz4_flex`'s own block is the oracle, over generated inputs, inputs that
//! repeat (long matches, matches past the 16-bit table's reach, runs of one byte), both tables' sizes, and every unit
//! size down to one.

use super::*;
use proptest::prelude::*;

/// `lz4_flex`'s block of `input`.
fn oracle(input: &[u8]) -> Vec<u8> {
  let mut block = vec![0u8; lz4_flex::block::get_maximum_output_size(input.len())];
  let written = lz4_flex::block::compress_into(input, &mut block).unwrap();
  block.truncate(written);
  block
}

/// The port's block of `input`, `work` units a step, never given up; and the steps it took.
fn made(input: &[u8], work: usize) -> (Vec<u8>, usize) {
  let mut making = Compressing::new(input.len(), usize::MAX);
  let mut steps = 0usize;
  while !making.over() {
    making.step(input, work);
    steps += 1;
  }
  (making.block().unwrap().to_vec(), steps)
}

/// Bytes that repeat in long runs and short ones: a text that LZ4 finds many matches in.
fn repetitive(count: usize, seed: u32) -> Vec<u8> {
  let words: [&[u8]; 6] = [
    b"window ",
    b"svndiff ",
    b"delta ",
    b"\n",
    b"source view ",
    b"0123456789",
  ];
  let mut out = Vec::with_capacity(count);
  let mut state = seed.wrapping_add(1);
  while out.len() < count {
    state ^= state << 13;
    state ^= state >> 17;
    state ^= state << 5;
    out.extend_from_slice(words[(state % 6) as usize]);
  }
  out.truncate(count);
  out
}

/// A-751. Do: make the block of inputs from empty to past the 16-bit table's 65,535 bytes (all literals, one byte run,
/// repetitive text, a text repeated at a distance past 65,535), a unit a step and in larger steps. Expect: every block
/// `lz4_flex`'s byte for byte; the one-unit run taking more steps than the input has bytes over 64.
#[test]
fn every_block_is_lz4_flexs_under_any_unit() {
  let mut far = repetitive(40_000, 7);
  far.extend(repetitive(40_000, 9));
  far.extend_from_slice(&far.clone()[..30_000]);
  let inputs: Vec<Vec<u8>> = vec![
    Vec::new(),
    b"short".to_vec(),
    vec![b'a'; 13],
    vec![b'z'; 102_400],
    repetitive(4_096, 1),
    repetitive(65_534, 2),
    repetitive(65_535, 3),
    repetitive(102_400, 4),
    far,
  ];
  for input in &inputs {
    let expected = oracle(input);
    for work in [1, 7, 64, 1024, usize::MAX] {
      let (block, steps) = made(input, work);
      assert!(block == expected, "{} bytes, work {work}", input.len());
      if work == 1 {
        assert!(
          steps >= input.len() / 64,
          "{} bytes: {steps} steps",
          input.len()
        );
      }
    }
  }
}

/// A-751. Do: make a block with the most it is worth below what it comes to. Expect: no block, the making over.
#[test]
fn a_block_past_what_it_is_worth_is_given_up() {
  let input = repetitive(10_000, 5);
  let whole = oracle(&input).len();
  let mut making = Compressing::new(input.len(), whole);
  while !making.over() {
    making.step(&input, 64);
  }
  assert_eq!(making.block(), None);
  let mut making = Compressing::new(input.len(), whole + 1);
  while !making.over() {
    making.step(&input, 64);
  }
  assert_eq!(making.block().map(<[u8]>::len), Some(whole));
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-751. Do: make the block of generated inputs, some from a small alphabet so they repeat, under a generated unit.
  /// Expect: `lz4_flex`'s block.
  #[test]
  fn a_generated_inputs_block_is_lz4_flexs(
    input in prop_oneof![
      proptest::collection::vec(any::<u8>(), 0..3000),
      proptest::collection::vec(0u8..4, 0..20_000),
    ],
    work in 1usize..300,
  ) {
    let (block, _) = made(&input, work);
    prop_assert!(block == oracle(&input));
  }
}
