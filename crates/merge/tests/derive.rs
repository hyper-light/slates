//! Oracle and canonical-case tests for the content deriver (§4.16 "Composition at seal";
//! T-6.x). The deriver composes one path's declared operations into a net op set relative to
//! the base content, referencing added bytes by their offset in the sealed post-state (the
//! final file). The property it must hold: applying the net ops to the base version, drawing
//! added bytes from the post-state, reproduces the post-state exactly — and it must do so from
//! the declared ranges alone, never by comparing bytes.
//!
//! The oracle simulates a real journal at the byte level to obtain the post-state, then a
//! reference applier (the splice's semantics) reconstructs the file from the net ops and asserts
//! it equals the post-state. Base bytes and added bytes are drawn from two different
//! position-varying patterns, so a misplaced op, a wrong length, or a wrong source offset makes
//! the reconstruction diverge and the test fail.

use slates_merge::derive::{ContentOp, compose_content};
use slates_merge::ops_doc::{Op, OpKind};

use proptest::prelude::*;

/// Shape: the largest run of bytes a single generated add (insert, extend, or truncate-grow)
/// contributes, kept small so a proptest case explores many overlapping operations rather than
/// a few huge ones.
const MAX_ADD: u64 = 24;

/// A base byte's value at position `index`: one position-varying pattern.
fn base_byte(index: u64) -> u8 {
  // A cheap scramble so adjacent base bytes differ and misplacement is caught; the constant is
  // an odd multiplier (Knuth's) and the value is the low byte.
  u8::try_from(index.wrapping_mul(2_654_435_761).wrapping_shr(11) & 0xff).unwrap_or(0)
}

/// The `count` added bytes starting at global add-counter `counter`, from a different
/// position-varying pattern than [`base_byte`], so added bytes never look like base bytes.
fn add_bytes(counter: &mut u64, count: u64) -> Vec<u8> {
  let mut out = Vec::new();
  for _ in 0..count {
    // A different multiplier and an offset keep this sequence disjoint from base's in practice.
    let value = counter.wrapping_mul(40_503).wrapping_add(0x9e37) & 0xff;
    out.push(u8::try_from(value).unwrap_or(0));
    *counter = counter.wrapping_add(1);
  }
  out
}

/// A raw operation the strategy generates; it is interpreted against the running file length so
/// every operation is in bounds (`kind` selects the operation, `a` and `b` its coordinates).
#[derive(Clone, Copy, Debug)]
struct Raw {
  kind: u8,
  a: u64,
  b: u64,
}

/// Simulates the journal at the byte level, returning both the declared operations (ranges only,
/// as the deriver sees them) and the resulting post-state bytes (the final file). The two are
/// produced together so they can never disagree.
fn simulate(base_len: u64, raw_ops: &[Raw]) -> (Vec<ContentOp>, Vec<u8>) {
  let mut file: Vec<u8> = (0..base_len).map(base_byte).collect();
  let mut ops = Vec::new();
  let mut counter = 0u64;
  for raw in raw_ops {
    let current = file.len() as u64;
    match raw.kind % 5 {
      0 => {
        // Overwrite in place (needs at least one byte to overwrite).
        if current == 0 {
          continue;
        }
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        let bytes = add_bytes(&mut counter, len);
        let start = usize::try_from(at).unwrap_or(0);
        file[start..start + bytes.len()].copy_from_slice(&bytes);
        ops.push(ContentOp::Overwrite { at, len });
      }
      1 => {
        // Extend past the end.
        let len = 1 + raw.b % MAX_ADD;
        let bytes = add_bytes(&mut counter, len);
        file.extend_from_slice(&bytes);
        ops.push(ContentOp::Extend { at: current, len });
      }
      2 => {
        // Truncate (shrink, or grow with zero bytes).
        let len = raw.a % (current + MAX_ADD + 1);
        if len < current {
          file.truncate(usize::try_from(len).unwrap_or(0));
        } else if len > current {
          file.resize(usize::try_from(len).unwrap_or(0), 0);
        }
        ops.push(ContentOp::Truncate { len });
      }
      3 => {
        // Insert within (or at either end of) the file.
        let at = raw.a % (current + 1);
        let len = 1 + raw.b % MAX_ADD;
        let bytes = add_bytes(&mut counter, len);
        let tail = file.split_off(usize::try_from(at).unwrap_or(0));
        file.extend_from_slice(&bytes);
        file.extend_from_slice(&tail);
        ops.push(ContentOp::Insert { at, len });
      }
      _ => {
        // Delete a range (needs at least one byte).
        if current == 0 {
          continue;
        }
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        let start = usize::try_from(at).unwrap_or(0);
        let end = usize::try_from(at + len).unwrap_or(start);
        file.drain(start..end);
        ops.push(ContentOp::Delete { at, len });
      }
    }
  }
  (ops, file)
}

/// Reconstructs the file by applying the net ops to the base version, drawing added bytes from
/// the post-state by their source offset — the splice's semantics (§4.16 "Splice"), used here as
/// the serial reference. The net ops are in base coordinates and non-decreasing by `at`; a
/// `Delete` or `Truncate` skips base bytes, the others copy from the post-state.
fn apply_net(base: &[u8], net: &[Op], post_state: &[u8], base_len: u64) -> Vec<u8> {
  let mut out = Vec::new();
  let mut base_pos = 0u64;
  for op in net {
    if op.at > base_pos {
      out.extend_from_slice(
        &base[usize::try_from(base_pos).unwrap_or(0)..usize::try_from(op.at).unwrap_or(0)],
      );
      base_pos = op.at;
    }
    match op.kind {
      OpKind::Delete => base_pos += op.len,
      OpKind::Truncate => base_pos = base_len,
      OpKind::Overwrite => {
        let src = usize::try_from(op.src).unwrap_or(0);
        let len = usize::try_from(op.len).unwrap_or(0);
        out.extend_from_slice(&post_state[src..src + len]);
        base_pos += op.len;
      }
      OpKind::Insert | OpKind::Extend => {
        let src = usize::try_from(op.src).unwrap_or(0);
        let len = usize::try_from(op.len).unwrap_or(0);
        out.extend_from_slice(&post_state[src..src + len]);
      }
      _ => {}
    }
  }
  if base_pos < base_len {
    out.extend_from_slice(
      &base[usize::try_from(base_pos).unwrap_or(0)..usize::try_from(base_len).unwrap_or(0)],
    );
  }
  out
}

/// Builds the base bytes for a length.
fn base_of(base_len: u64) -> Vec<u8> {
  (0..base_len).map(base_byte).collect()
}

/// AC-6.1 (worked): overlapping overwrites merge into one overwrite — the net set is a single
/// operation, so a deriver that failed to compose them (leaving two) would fail here (the
/// non-vacuity check on composition).
#[test]
fn overlapping_overwrites_merge_into_one() {
  let base_len = 20;
  let journal = [
    ContentOp::Overwrite { at: 10, len: 5 },
    ContentOp::Overwrite { at: 12, len: 5 },
  ];
  let net = compose_content(base_len, &journal);
  assert_eq!(net.len(), 1, "two overlapping overwrites compose to one");
  assert_eq!(net[0].kind, OpKind::Overwrite);
  assert_eq!(net[0].at, 10);
  assert_eq!(net[0].len, 7, "base [10,17) is the union of the two writes");
}

/// AC-6.2: a truncate cancels operations beyond the new length — an overwrite past the cut is
/// gone, leaving only the truncate.
#[test]
fn a_truncate_cancels_operations_beyond_it() {
  let base_len = 20;
  let journal = [
    ContentOp::Overwrite { at: 15, len: 5 },
    ContentOp::Truncate { len: 10 },
  ];
  let net = compose_content(base_len, &journal);
  assert_eq!(net.len(), 1, "the overwrite beyond the cut is cancelled");
  assert_eq!(net[0].kind, OpKind::Truncate);
  assert_eq!(net[0].at, 10, "the new length");
  assert_eq!(net[0].len, 10, "ten base bytes removed");
}

/// AC-6.3: an insert followed by an overlapping delete cancels the inserted bytes, leaving only
/// the net removal of base bytes.
#[test]
fn an_insert_then_an_overlapping_delete_cancels() {
  let base_len = 10;
  let journal = [
    ContentOp::Insert { at: 5, len: 3 },
    ContentOp::Delete { at: 2, len: 6 },
  ];
  let net = compose_content(base_len, &journal);
  assert_eq!(
    net.len(),
    1,
    "the inserted bytes are cancelled by the delete"
  );
  assert_eq!(net[0].kind, OpKind::Delete);
  assert_eq!(net[0].at, 2);
  assert_eq!(net[0].len, 3, "only base [2,5) is net removed");
}

/// AC-6.4: a whole-file rewrite (truncate to zero, then write new bytes of a different length)
/// composes to one delete of the base length and one insert of the new bytes.
#[test]
fn a_whole_file_rewrite_is_a_delete_then_an_insert() {
  let base_len = 20;
  let journal = [
    ContentOp::Truncate { len: 0 },
    ContentOp::Extend { at: 0, len: 12 },
  ];
  let net = compose_content(base_len, &journal);
  assert_eq!(
    net.len(),
    2,
    "a delete of the base and an insert of the new bytes"
  );
  assert_eq!(net[0].kind, OpKind::Delete);
  assert_eq!(net[0].at, 0);
  assert_eq!(net[0].len, 20);
  assert_eq!(net[1].kind, OpKind::Insert);
  assert_eq!(net[1].at, 0);
  assert_eq!(net[1].len, 12);
  assert_eq!(
    net[1].src, 0,
    "the new bytes are at the start of the post-state"
  );
}

/// A pure append past the end is an extend; the reconstruction matches.
#[test]
fn a_pure_append_is_an_extend() {
  let base_len = 8;
  let (ops, post) = simulate(
    base_len,
    &[Raw {
      kind: 1,
      a: 0,
      b: 5,
    }],
  );
  let net = compose_content(base_len, &ops);
  assert_eq!(net.len(), 1);
  assert_eq!(net[0].kind, OpKind::Extend);
  assert_eq!(net[0].at, base_len);
  assert_eq!(apply_net(&base_of(base_len), &net, &post, base_len), post);
}

/// An empty journal derives no operations and reconstructs the base unchanged.
#[test]
fn an_empty_journal_is_the_identity() {
  let base_len = 16;
  let net = compose_content(base_len, &[]);
  assert!(net.is_empty());
  let base = base_of(base_len);
  assert_eq!(apply_net(&base, &net, &base, base_len), base);
}

proptest! {
  /// T-6.1 (the deriver oracle): for any base length and any in-bounds journal, applying the net
  /// ops to the base — drawing added bytes from the post-state by source offset — reproduces the
  /// post-state exactly. This is composition-is-correct without any byte comparison in the
  /// deriver (D-27's never-diff clause): the deriver saw only ranges.
  #[test]
  fn the_net_ops_reproduce_the_post_state(
    base_len in 0u64..64,
    raw in proptest::collection::vec(
      (any::<u8>(), 0u64..128, 0u64..128).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..40,
    ),
  ) {
    let (ops, post) = simulate(base_len, &raw);
    let net = compose_content(base_len, &ops);
    let base = base_of(base_len);
    let reconstructed = apply_net(&base, &net, &post, base_len);
    prop_assert_eq!(reconstructed, post);
  }

  /// T-6.2 (determinism): the same journal composes to the same net ops every time (the identity
  /// gate; the ops document's BLAKE3 then depends only on the declared work).
  #[test]
  fn composition_is_deterministic(
    base_len in 0u64..64,
    raw in proptest::collection::vec(
      (any::<u8>(), 0u64..128, 0u64..128).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..40,
    ),
  ) {
    let (ops, _post) = simulate(base_len, &raw);
    let first = compose_content(base_len, &ops);
    let second = compose_content(base_len, &ops);
    prop_assert_eq!(first, second);
  }

  /// T-6.3 (the net set is in base order): the net ops are non-decreasing by base offset, so the
  /// splice and the verdict can sweep them in one pass.
  #[test]
  fn the_net_ops_are_ordered_by_base_offset(
    base_len in 0u64..64,
    raw in proptest::collection::vec(
      (any::<u8>(), 0u64..128, 0u64..128).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..40,
    ),
  ) {
    let (ops, _post) = simulate(base_len, &raw);
    let net = compose_content(base_len, &ops);
    let mut last = 0u64;
    for op in &net {
      prop_assert!(op.at >= last, "net ops are non-decreasing by base offset");
      last = op.at;
    }
  }
}
