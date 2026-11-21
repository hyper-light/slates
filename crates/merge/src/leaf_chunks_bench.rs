//! Option (i) of A-818, measured beside the engine (main's condition of 2026-11-21 10:35: "the A/B's rows include ...
//! against (i) on the generic leaf"): a path's versions as persistent piece trees ([`crate::piece_tree`]) whose runs
//! name immutable chunks, VS Code's choice: the first value's bytes and each change's added bytes, never re-pointed. A
//! bench's model only, not the engine's history: it holds what (i) would hold, so `history_layers_bench` prints its
//! cost beside the engine's. Public for the example alone (`#[doc(hidden)]` in `lib.rs`).

use std::time::Instant;

use crate::ops_doc::{Op, OpKind};
use crate::piece_tree::{NODE_BYTES, Part, PieceTrees, Run, Version};

/// What a stream of changes cost under option (i).
#[derive(Clone, Copy, Debug)]
pub struct Cost {
  /// The mean nanoseconds of the last `timed` changes.
  pub ns_per_change: u128,
  /// The nanoseconds of reading version 1 whole after them.
  pub read_first_ns: u128,
  /// The bytes the history holds beyond the current file: every chunk (the first value's and each change's adds) and
  /// every live tree node. The current file is the files arena's, held apart, so (i) keeps the first value whole.
  pub history_bytes: usize,
}

/// One path's history under option (i).
struct Path {
  trees: PieceTrees,
  chunks: Vec<Vec<u8>>,
  versions: Vec<Version>,
  len: u64,
}

impl Path {
  fn new(first: Vec<u8>) -> Path {
    let mut trees = PieceTrees::default();
    let len = u64::try_from(first.len()).unwrap_or(u64::MAX);
    let run = Run {
      source: 0,
      at: 0,
      len,
    };
    let version = trees.of_runs(&[run]).unwrap_or_default();
    Path {
      trees,
      chunks: vec![first],
      versions: vec![version],
      len,
    }
  }

  /// The change `ops` over the latest, its added bytes `post`.
  fn change(&mut self, ops: &[Op], post: &[u8]) {
    let source = self.chunks.len();
    let parts: Vec<Part> = crate::engine::segments(self.len, ops)
      .into_iter()
      .map(|segment| match segment {
        crate::engine::Segment::Base { start, len } => Part::Kept { from: start, len },
        crate::engine::Segment::Post { src, len } => Part::Added(Run {
          source,
          at: src,
          len,
        }),
      })
      .collect();
    self.chunks.push(post.to_vec());
    let Some(latest) = self.versions.last() else {
      return;
    };
    if let Ok(made) = self.trees.rebuild(latest, &parts) {
      self.len = self.trees.len(&made).unwrap_or(0);
      self.versions.push(made);
    }
  }

  /// Version `index` whole.
  fn read(&self, index: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(version) = self.versions.get(index) else {
      return out;
    };
    let len = self.trees.len(version).unwrap_or(0);
    let _ = self.trees.read(version, (0, len), &mut |run| {
      let start = usize::try_from(run.at).unwrap_or(usize::MAX);
      let end = start.saturating_add(usize::try_from(run.len).unwrap_or(usize::MAX));
      if let Some(bytes) = self
        .chunks
        .get(run.source)
        .and_then(|chunk| chunk.get(start..end))
      {
        out.extend_from_slice(bytes);
      }
    });
    out
  }

  fn history_bytes(&self) -> usize {
    self
      .chunks
      .iter()
      .map(Vec::len)
      .sum::<usize>()
      .saturating_add(self.trees.live().saturating_mul(NODE_BYTES))
  }
}

/// One op of `kind` (`OpKind` as the bench names it) of `len` bytes at `at`, its bytes the post-state's first.
fn op(kind: OpKind, at: u64, len: u64) -> Op {
  Op {
    kind,
    flags: 0,
    path: 0,
    at,
    len,
    src: 0,
  }
}

/// A stream's change at step `n`: its ops for a change of `slot` bytes at `at`.
pub fn ops_of(kind: u8, at: u64, slot: u64) -> Vec<Op> {
  match kind {
    0 => vec![op(OpKind::Overwrite, at, slot)],
    1 => vec![op(OpKind::Insert, at, slot)],
    _ => vec![op(OpKind::Delete, at, slot), op(OpKind::Insert, at, slot)],
  }
}

/// Option (i)'s cost for one path of `first` bytes after `history` changes then `timed` more, change `n` made by
/// `stream(n)` (a kind for [`ops_of`] and an offset) of `slot` bytes.
pub fn one_path(
  first: Vec<u8>,
  (history, timed, slot): (u64, u64, u64),
  stream: fn(u64) -> (u8, u64),
) -> Cost {
  let mut path = Path::new(first);
  let change = |path: &mut Path, n: u64| {
    let (kind, at) = stream(n);
    let post = vec![u8::try_from(n % 251).unwrap_or(0); usize::try_from(slot).unwrap_or(0)];
    path.change(&ops_of(kind, at, slot), &post);
  };
  for n in 0..history {
    change(&mut path, n);
  }
  let started = Instant::now();
  for n in history..history.saturating_add(timed) {
    change(&mut path, n);
  }
  let ns_per_change = started
    .elapsed()
    .as_nanos()
    .checked_div(u128::from(timed))
    .unwrap_or(0);
  let started = Instant::now();
  let bytes = path.read(1);
  let read_first_ns = started.elapsed().as_nanos();
  std::hint::black_box(bytes);
  Cost {
    ns_per_change,
    read_first_ns,
    history_bytes: path.history_bytes(),
  }
}

/// Option (i)'s history bytes for `paths` paths of `first` bytes each, each changed once by an overwrite of `slot`
/// bytes at `at`: each keeps its first value whole beside the files arena's new block.
pub fn many_paths_once(first: &[u8], paths: u64, (at, slot): (u64, u64)) -> usize {
  (0..paths)
    .map(|n| {
      let mut path = Path::new(first.to_vec());
      let post = vec![u8::try_from(n % 251).unwrap_or(0); usize::try_from(slot).unwrap_or(0)];
      path.change(&ops_of(0, at, slot), &post);
      path.history_bytes()
    })
    .fold(0usize, usize::saturating_add)
}
