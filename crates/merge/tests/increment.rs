//! Oracle and worked-case tests for the increment assembler (§4.16 "Composition at seal";
//! T-6.x). A whole-volume content journal composes into one ops document whose per-path ops name
//! added bytes by their offset in the increment's post-state (the paths' final contents
//! concatenated in sorted order). The property: reconstructing each path from its base version
//! and its net ops — drawing added bytes from the post-state at the global source offset —
//! reproduces that path's final content, for every path.
//!
//! The oracle simulates each file's journal at the byte level to get its final content, lays out
//! the post-state exactly as the assembler does, then reconstructs from the ops document and
//! compares. Base bytes (values 0–127) and added bytes (values 128–255) are drawn from disjoint
//! ranges, so any op placed on the wrong path, at the wrong offset, or with the wrong source
//! offset diverges.

use slates_merge::increment::{FileOp, compose_increment};
use slates_merge::ops_doc::{Op, OpKind};

use proptest::prelude::*;

/// Shape: the largest run a generated add contributes.
const MAX_ADD: u64 = 16;

/// A base byte for a path `seed` at position `index`: a value in 0..=127.
fn base_byte(seed: u64, index: u64) -> u8 {
  u8::try_from((seed.wrapping_mul(31).wrapping_add(index.wrapping_mul(7))) & 0x7f).unwrap_or(0)
}

/// The next added byte: a value in 128..=255, varying by the global counter so a wrong source
/// offset picks up visibly different bytes.
fn add_byte(counter: &mut u64) -> u8 {
  let value = 0x80 | (*counter & 0x7f);
  *counter = counter.wrapping_add(1);
  u8::try_from(value).unwrap_or(0x80)
}

/// A raw op the strategy generates for one path, interpreted against that path's running length.
#[derive(Clone, Copy, Debug)]
struct Raw {
  kind: u8,
  a: u64,
  b: u64,
}

/// Simulates one path's journal at the byte level, returning its declared content ops and its
/// final content. `counter` is shared across paths so added bytes are globally distinct.
fn simulate_path(
  seed: u64,
  base_len: u64,
  raw_ops: &[Raw],
  counter: &mut u64,
) -> (Vec<FileOp>, Vec<u8>) {
  let path = format!("f{seed}");
  let mut file: Vec<u8> = (0..base_len).map(|i| base_byte(seed, i)).collect();
  let mut ops = Vec::new();
  for raw in raw_ops {
    let current = file.len() as u64;
    match raw.kind % 5 {
      0 => {
        if current == 0 {
          continue;
        }
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        for offset in 0..len {
          let index = usize::try_from(at + offset).unwrap_or(0);
          file[index] = add_byte(counter);
        }
        ops.push(FileOp::Overwrite {
          path: path.clone(),
          at,
          len,
        });
      }
      1 => {
        let len = 1 + raw.b % MAX_ADD;
        let at = current;
        for _ in 0..len {
          let byte = add_byte(counter);
          file.push(byte);
        }
        ops.push(FileOp::Extend {
          path: path.clone(),
          at,
          len,
        });
      }
      2 => {
        let len = raw.a % (current + MAX_ADD + 1);
        if len < current {
          file.truncate(usize::try_from(len).unwrap_or(0));
        } else if len > current {
          file.resize(usize::try_from(len).unwrap_or(0), 0);
        }
        ops.push(FileOp::Truncate {
          path: path.clone(),
          len,
        });
      }
      3 => {
        let at = raw.a % (current + 1);
        let len = 1 + raw.b % MAX_ADD;
        let mut bytes = Vec::new();
        for _ in 0..len {
          bytes.push(add_byte(counter));
        }
        let tail = file.split_off(usize::try_from(at).unwrap_or(0));
        file.extend_from_slice(&bytes);
        file.extend_from_slice(&tail);
        ops.push(FileOp::Insert {
          path: path.clone(),
          at,
          len,
        });
      }
      _ => {
        if current == 0 {
          continue;
        }
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        let start = usize::try_from(at).unwrap_or(0);
        let end = usize::try_from(at + len).unwrap_or(start);
        file.drain(start..end);
        ops.push(FileOp::Delete {
          path: path.clone(),
          at,
          len,
        });
      }
    }
  }
  (ops, file)
}

/// Reconstructs one path's final content from its base bytes and its net ops (those of the ops
/// document with this path index), drawing added bytes from the global post-state at each op's
/// source offset. The ops are in base coordinates and non-decreasing by `at`.
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

/// Round-robins the per-path op lists into one journal, preserving each path's own order (which is
/// all the assembler depends on).
fn interleave(per_path: &[Vec<FileOp>]) -> Vec<FileOp> {
  let mut journal = Vec::new();
  let longest = per_path.iter().map(Vec::len).max().unwrap_or(0);
  for step in 0..longest {
    for ops in per_path {
      if let Some(op) = ops.get(step) {
        journal.push(op.clone());
      }
    }
  }
  journal
}

/// Runs the oracle. The document's path table is authoritative: it holds exactly the paths the
/// journal touched, in sorted order, which is also the post-state layout. A path with no
/// operations is absent from the document, and its final content equals its base.
fn check(paths: &[(u64, u64, Vec<u8>)], journal: &[FileOp]) {
  let base: Vec<(String, u64)> = paths
    .iter()
    .map(|(seed, base_len, _)| (format!("f{seed}"), *base_len))
    .collect();
  let doc = compose_increment(&base, journal);
  // Look up a path's model record by its name.
  let record = |name: &str| paths.iter().find(|(seed, _, _)| format!("f{seed}") == name);

  // The post-state: the final content of every path in the document, in its path-table order.
  let mut post_state = Vec::new();
  for name in doc.paths.paths() {
    if let Some((_, _, final_bytes)) = record(name) {
      post_state.extend_from_slice(final_bytes);
    }
  }
  // Each path in the document reconstructs from its base and its net ops.
  for (path_index, name) in doc.paths.paths().iter().enumerate() {
    let Some((seed, base_len, final_bytes)) = record(name) else {
      continue;
    };
    let base_bytes: Vec<u8> = (0..*base_len).map(|i| base_byte(*seed, i)).collect();
    let index = u16::try_from(path_index).unwrap_or(u16::MAX);
    let net: Vec<Op> = doc
      .ops
      .iter()
      .copied()
      .filter(|op| op.path == index)
      .collect();
    let reconstructed = apply_net(&base_bytes, &net, &post_state, *base_len);
    assert_eq!(&reconstructed, final_bytes, "path {name} reconstructs");
  }
  // A path the journal never touched is absent from the document, and its final equals its base.
  for (seed, base_len, final_bytes) in paths {
    let name = format!("f{seed}");
    if !doc.paths.paths().iter().any(|p| p == &name) {
      let base_bytes: Vec<u8> = (0..*base_len).map(|i| base_byte(*seed, i)).collect();
      assert_eq!(
        final_bytes, &base_bytes,
        "untouched path {name} equals its base"
      );
    }
  }
}

/// Two independent files, each edited, assemble into one document; both reconstruct.
#[test]
fn two_files_assemble_and_each_reconstructs() {
  let mut counter = 0u64;
  let (ops_a, final_a) = simulate_path(
    0,
    10,
    &[Raw {
      kind: 0,
      a: 2,
      b: 3,
    }],
    &mut counter,
  );
  let (ops_b, final_b) = simulate_path(
    1,
    6,
    &[Raw {
      kind: 3,
      a: 3,
      b: 4,
    }],
    &mut counter,
  );
  let journal = interleave(&[ops_a, ops_b]);
  check(&[(0, 10, final_a), (1, 6, final_b)], &journal);
}

/// The assembler names each path; the document's path table holds them sorted.
#[test]
fn the_path_table_holds_the_touched_paths_sorted() {
  let journal = vec![
    FileOp::Overwrite {
      path: "z".to_owned(),
      at: 0,
      len: 1,
    },
    FileOp::Overwrite {
      path: "a".to_owned(),
      at: 0,
      len: 1,
    },
  ];
  let doc = compose_increment(&[("z".to_owned(), 4), ("a".to_owned(), 4)], &journal);
  assert_eq!(doc.paths.paths(), &["a".to_owned(), "z".to_owned()]);
}

/// The identity is independent of the order paths (and their operations) were declared in: the
/// same per-path work in two interleavings yields the same ops document identity (the sorted
/// post-state layout is what makes this hold).
#[test]
fn the_identity_is_independent_of_declaration_order() {
  let mut counter = 0u64;
  let (ops_a, _fa) = simulate_path(
    0,
    8,
    &[
      Raw {
        kind: 3,
        a: 2,
        b: 5,
      },
      Raw {
        kind: 0,
        a: 1,
        b: 2,
      },
    ],
    &mut counter,
  );
  let (ops_b, _fb) = simulate_path(
    1,
    5,
    &[Raw {
      kind: 1,
      a: 0,
      b: 3,
    }],
    &mut counter,
  );
  let base = vec![("f0".to_owned(), 8), ("f1".to_owned(), 5)];
  let forward = interleave(&[ops_a.clone(), ops_b.clone()]);
  let backward = interleave(&[ops_b, ops_a]);
  let first = compose_increment(&base, &forward);
  let second = compose_increment(&base, &backward);
  assert_eq!(first.identity(), second.identity());
}

proptest! {
  /// T-6.6 (the assembler oracle): for any three files with any base lengths and any in-bounds
  /// journals, every path reconstructs from its base and its net ops drawing from the post-state.
  #[test]
  fn every_path_reconstructs_from_the_increment(
    base_lens in prop::array::uniform3(0u64..40),
    raws in prop::array::uniform3(proptest::collection::vec(
      (any::<u8>(), 0u64..80, 0u64..80).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..12,
    )),
  ) {
    let mut counter = 0u64;
    let mut per_path = Vec::new();
    let mut paths = Vec::new();
    for seed in 0u64..3 {
      let index = usize::try_from(seed).unwrap_or(0);
      let (ops, final_bytes) = simulate_path(seed, base_lens[index], &raws[index], &mut counter);
      per_path.push(ops);
      paths.push((seed, base_lens[index], final_bytes));
    }
    let journal = interleave(&per_path);
    check(&paths, &journal);
  }
}
