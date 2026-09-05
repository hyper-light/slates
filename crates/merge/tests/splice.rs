//! Oracle and worked-case tests for the splice (§4.16 "Splice"; T-6.x). The splice applies a
//! path's accepted content net ops to its base extent list, producing the new version's extent
//! list by reference surgery — no byte is copied. The property: reading the new extent list back
//! (base extents from the base bytes, post-state extents from the post-state bytes) reproduces
//! the file's final content exactly.
//!
//! The oracle simulates a journal at the byte level to obtain the final content (the post-state),
//! composes the net ops with the content deriver, chunks the base into several extents (to
//! exercise splitting), splices, and reads the result back. Base bytes (0–127) and added bytes
//! (128–255) are disjoint, so any misplaced or wrongly-sourced extent diverges. A non-vacuity
//! check asserts that an untouched region keeps a base-sourced extent, so the splice cannot pass
//! by copying everything into the post-state.

use slates_merge::derive::{ContentOp, compose_content};
use slates_merge::splice::{Extent, Source, splice};

use proptest::prelude::*;

/// Shape: the largest run a generated add contributes.
const MAX_ADD: u64 = 20;

/// Shape: the base is chunked into extents of at most this many bytes, so the splice must split
/// them at op boundaries.
const CHUNK: u64 = 5;

/// A base byte at `index`: a value in 0..=127.
fn base_byte(index: u64) -> u8 {
  u8::try_from((index.wrapping_mul(2_654_435_761).wrapping_shr(11)) & 0x7f).unwrap_or(0)
}

/// The next added byte: 128..=255, varying by the counter.
fn add_byte(counter: &mut u64) -> u8 {
  let value = 0x80 | (*counter & 0x7f);
  *counter = counter.wrapping_add(1);
  u8::try_from(value).unwrap_or(0x80)
}

/// A raw op the strategy generates.
#[derive(Clone, Copy, Debug)]
struct Raw {
  kind: u8,
  a: u64,
  b: u64,
}

/// Simulates a single file's journal at the byte level, returning the declared content ops and
/// the final content (the post-state for this path).
fn simulate(base_len: u64, raw_ops: &[Raw]) -> (Vec<ContentOp>, Vec<u8>) {
  let mut file: Vec<u8> = (0..base_len).map(base_byte).collect();
  let mut ops = Vec::new();
  let mut counter = 0u64;
  for raw in raw_ops {
    let current = file.len() as u64;
    match raw.kind % 5 {
      0 if current > 0 => {
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        for offset in 0..len {
          let index = usize::try_from(at + offset).unwrap_or(0);
          file[index] = add_byte(&mut counter);
        }
        ops.push(ContentOp::Overwrite { at, len });
      }
      1 => {
        let len = 1 + raw.b % MAX_ADD;
        for _ in 0..len {
          let byte = add_byte(&mut counter);
          file.push(byte);
        }
        ops.push(ContentOp::Extend { at: current, len });
      }
      2 => {
        let len = raw.a % (current + MAX_ADD + 1);
        if len < current {
          file.truncate(usize::try_from(len).unwrap_or(0));
        } else if len > current {
          file.resize(usize::try_from(len).unwrap_or(0), 0);
        }
        ops.push(ContentOp::Truncate { len });
      }
      3 => {
        let at = raw.a % (current + 1);
        let len = 1 + raw.b % MAX_ADD;
        let mut bytes = Vec::new();
        for _ in 0..len {
          bytes.push(add_byte(&mut counter));
        }
        let tail = file.split_off(usize::try_from(at).unwrap_or(0));
        file.extend_from_slice(&bytes);
        file.extend_from_slice(&tail);
        ops.push(ContentOp::Insert { at, len });
      }
      _ if current > 0 => {
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        let start = usize::try_from(at).unwrap_or(0);
        let end = usize::try_from(at + len).unwrap_or(start);
        file.drain(start..end);
        ops.push(ContentOp::Delete { at, len });
      }
      _ => {}
    }
  }
  (ops, file)
}

/// Chunks a base of `base_len` bytes into extents of at most [`CHUNK`] bytes each, pointing at the
/// base store at their own offsets.
fn base_extents(base_len: u64) -> Vec<Extent> {
  let mut extents = Vec::new();
  let mut at = 0u64;
  while at < base_len {
    let len = CHUNK.min(base_len - at);
    extents.push(Extent {
      source: Source::Base,
      at,
      len,
    });
    at += len;
  }
  extents
}

/// Reads an extent list back into bytes, taking base extents from `base` and post-state extents
/// from `post_state`.
fn read_back(extents: &[Extent], base: &[u8], post_state: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  for extent in extents {
    let store = match extent.source {
      Source::Base => base,
      Source::PostState => post_state,
    };
    let start = usize::try_from(extent.at).unwrap_or(0);
    let len = usize::try_from(extent.len).unwrap_or(0);
    out.extend_from_slice(&store[start..start + len]);
  }
  out
}

/// A single overwrite keeps the surrounding base bytes as base-sourced extents (no full copy) and
/// reads back to the final content.
#[test]
fn an_overwrite_splits_the_base_and_keeps_the_rest() {
  let base_len = 20;
  let (ops, post) = simulate(
    base_len,
    &[Raw {
      kind: 0,
      a: 8,
      b: 3,
    }],
  );
  let net = compose_content(base_len, &ops);
  let base = base_extents(base_len);
  let spliced = splice(&base, &net);
  let base_bytes: Vec<u8> = (0..base_len).map(base_byte).collect();
  assert_eq!(read_back(&spliced, &base_bytes, &post), post);
  assert!(
    spliced.iter().any(|e| e.source == Source::Base),
    "the untouched bytes stay base-sourced — no full copy"
  );
  assert!(
    spliced.iter().any(|e| e.source == Source::PostState),
    "the overwritten bytes are post-state-sourced"
  );
}

/// An empty op set leaves the base extents exactly as they are.
#[test]
fn no_ops_leaves_the_base_extents() {
  let base = base_extents(12);
  let spliced = splice(&base, &[]);
  assert_eq!(spliced, base);
}

/// A whole-file truncate to zero leaves no extents.
#[test]
fn truncate_to_zero_leaves_no_extents() {
  let base_len = 16;
  let (ops, _post) = simulate(
    base_len,
    &[Raw {
      kind: 2,
      a: 0,
      b: 0,
    }],
  );
  let net = compose_content(base_len, &ops);
  let spliced = splice(&base_extents(base_len), &net);
  assert!(spliced.is_empty());
}

proptest! {
  /// T-6.8 (the splice oracle): for any base length and any journal, reading the spliced extent
  /// list back reproduces the file's final content — the new version's bytes are assembled from
  /// base and post-state references, never copied.
  #[test]
  fn the_spliced_extents_read_back_to_the_final_content(
    base_len in 0u64..64,
    raw in proptest::collection::vec(
      (any::<u8>(), 0u64..128, 0u64..128).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..32,
    ),
  ) {
    let (ops, post) = simulate(base_len, &raw);
    let net = compose_content(base_len, &ops);
    let spliced = splice(&base_extents(base_len), &net);
    let base_bytes: Vec<u8> = (0..base_len).map(base_byte).collect();
    prop_assert_eq!(read_back(&spliced, &base_bytes, &post), post);
  }

  /// T-6.9: the spliced extents cover exactly the final content length, and none is empty.
  #[test]
  fn the_spliced_extents_are_well_formed(
    base_len in 0u64..64,
    raw in proptest::collection::vec(
      (any::<u8>(), 0u64..128, 0u64..128).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..32,
    ),
  ) {
    let (ops, post) = simulate(base_len, &raw);
    let net = compose_content(base_len, &ops);
    let spliced = splice(&base_extents(base_len), &net);
    let covered: u64 = spliced.iter().map(|e| e.len).sum();
    prop_assert_eq!(covered, post.len() as u64);
    prop_assert!(spliced.iter().all(|e| e.len > 0), "no empty extents");
  }
}
