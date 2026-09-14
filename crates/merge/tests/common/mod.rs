//! Shared fixtures of the merge engine tests: the increment builder and the block oracle, kept in
//! one place so the serial tests (`tests/engine.rs`) and the randomized-schedule test
//! (`tests/shuttle_green.rs`, T-6.7) decide against one and the same reference (the CLAUDE.md rule:
//! reuse the serial reference the model tests already keep; never a second oracle).

#![allow(dead_code)]

use slates_merge::engine::Increment;
use slates_merge::ops_doc::{Op, OpKind, OpsDoc};

/// Builds an increment: an ops document with a consistent post-state (content and xattr bytes are
/// appended to the post-state and named by their offset). Paths are interned in use order; the
/// document is left un-canonicalized (the engine resolves by index, which the order does not
/// affect), and the identity is supplied by the test.
#[derive(Default)]
pub(crate) struct Build {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Build {
  pub(crate) fn new() -> Build {
    Build::default()
  }

  fn idx(&mut self, path: &str) -> u16 {
    self.doc.paths.intern(path)
  }

  fn stash(&mut self, bytes: &[u8]) -> u64 {
    let offset = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    offset
  }

  pub(crate) fn content(&mut self, kind: OpKind, path: &str, at: u64, bytes: &[u8]) -> &mut Build {
    let path_idx = self.idx(path);
    let src = self.stash(bytes);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path: path_idx,
      at,
      len: bytes.len() as u64,
      src,
    });
    self
  }

  pub(crate) fn create(&mut self, path: &str, bytes: &[u8]) -> &mut Build {
    let path_idx = self.idx(path);
    self.doc.ops.push(Op {
      kind: OpKind::Create,
      flags: 0,
      path: path_idx,
      at: 0,
      len: 0,
      src: u64::MAX,
    });
    self.content(OpKind::Insert, path, 0, bytes)
  }

  pub(crate) fn overwrite(&mut self, path: &str, at: u64, bytes: &[u8]) -> &mut Build {
    self.content(OpKind::Overwrite, path, at, bytes)
  }

  pub(crate) fn insert(&mut self, path: &str, at: u64, bytes: &[u8]) -> &mut Build {
    self.content(OpKind::Insert, path, at, bytes)
  }

  fn edge(&mut self, kind: OpKind, path: &str, target: &str) -> &mut Build {
    let path_idx = self.idx(path);
    let target_idx = self.idx(target);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path: path_idx,
      at: 0,
      len: 0,
      src: u64::from(target_idx),
    });
    self
  }

  pub(crate) fn rename(&mut self, from: &str, to: &str) -> &mut Build {
    // The rename op is keyed at the destination; its source is the `src` path index.
    self.edge(OpKind::Rename, to, from)
  }

  pub(crate) fn symlink(&mut self, path: &str, target: &str) -> &mut Build {
    self.edge(OpKind::Symlink, path, target)
  }

  pub(crate) fn link(&mut self, path: &str, target: &str) -> &mut Build {
    self.edge(OpKind::Link, path, target)
  }

  fn name_op(&mut self, kind: OpKind, path: &str) -> &mut Build {
    let path_idx = self.idx(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path: path_idx,
      at: 0,
      len: 0,
      src: u64::MAX,
    });
    self
  }

  pub(crate) fn remove(&mut self, path: &str) -> &mut Build {
    self.name_op(OpKind::Unlink, path)
  }

  pub(crate) fn mkdir(&mut self, path: &str) -> &mut Build {
    self.name_op(OpKind::Mkdir, path)
  }

  pub(crate) fn rmdir(&mut self, path: &str) -> &mut Build {
    self.name_op(OpKind::Rmdir, path)
  }

  pub(crate) fn setmode(&mut self, path: &str, mode: u32) -> &mut Build {
    let path_idx = self.idx(path);
    self.doc.ops.push(Op {
      kind: OpKind::SetMode,
      flags: 0,
      path: path_idx,
      at: 0,
      len: u64::from(mode),
      src: u64::MAX,
    });
    self
  }

  pub(crate) fn setxattr(&mut self, path: &str, name: &str, value: &[u8]) -> &mut Build {
    let path_idx = self.idx(path);
    let name_idx = self.idx(name);
    let src = self.stash(value);
    self.doc.ops.push(Op {
      kind: OpKind::SetXattr,
      flags: 0,
      path: path_idx,
      at: u64::from(name_idx),
      len: value.len() as u64,
      src,
    });
    self
  }

  pub(crate) fn removexattr(&mut self, path: &str, name: &str) -> &mut Build {
    let path_idx = self.idx(path);
    let name_idx = self.idx(name);
    self.doc.ops.push(Op {
      kind: OpKind::RemoveXattr,
      flags: 0,
      path: path_idx,
      at: u64::from(name_idx),
      len: 0,
      src: u64::MAX,
    });
    self
  }

  /// The increment with the identity `[id; 32]`, based on `base`.
  pub(crate) fn at(&self, id: u8, base: u64) -> Increment {
    self.with_id([id; 32], base)
  }

  /// The increment with a full identity, based on `base` (for many distinct submissions).
  pub(crate) fn with_id(&self, id: [u8; 32], base: u64) -> Increment {
    Increment {
      id,
      base,
      doc: self.doc.clone(),
      post_state: self.post.clone(),
      evidence: Vec::new(),
    }
  }
}

// ---------------------------------------------------------------------------
// The content verdict's generative oracle (§4.16 "The verdict, two pure passes", D-27; D-20's
// model-based tests). A serial, obviously-correct reference decides the merge block by block; the
// engine must agree on every generated history. To keep the reference free of coordinate reasoning
// (which would just re-implement the engine), every edit is a length-preserving overwrite of a
// whole fixed-size block, so no position ever shifts: the verdict is then purely per-block identity,
// which is exactly the design's per-range rule made trivial to state.

/// The fixed block width; a whole block is overwritten at once (length-preserving, so no shifts).
pub(crate) const BLOCK_LEN: usize = 4;
/// How many blocks the file has.
pub(crate) const BLOCKS: usize = 5;

/// The base file: block `b` is four copies of `b`, so the blocks are distinct and an untouched
/// block is recognisable in the merged result.
pub(crate) fn base_file() -> Vec<u8> {
  let mut file = Vec::with_capacity(BLOCKS * BLOCK_LEN);
  for b in 0..BLOCKS {
    file.extend(std::iter::repeat_n(u8::try_from(b).unwrap_or(0), BLOCK_LEN));
  }
  file
}

/// The bytes an edit with tag `t` writes into a block: four copies of `100 + t`, independent of
/// which side wrote them — so the same tag on both sides is byte-identical (a convergent edit) and
/// different tags differ. Tags are 1..=3; tag 0 means the side left the block untouched.
pub(crate) fn edit_bytes(tag: u8) -> Vec<u8> {
  vec![100 + tag; BLOCK_LEN]
}

/// Overwrites into one `Build`, one op per edited block (tag != 0), at the block's fixed offset,
/// on the file `path`.
pub(crate) fn block_edits_on(path: &str, edits: &[u8]) -> Build {
  let mut build = Build::new();
  for (b, &tag) in edits.iter().enumerate() {
    if tag != 0 {
      build.overwrite(path, (b * BLOCK_LEN) as u64, &edit_bytes(tag));
    }
  }
  build
}

/// Overwrites into one `Build` on the file `f`, one op per edited block (tag != 0).
pub(crate) fn block_edits(edits: &[u8]) -> Build {
  block_edits_on("f", edits)
}

/// The reference merged file when the verdict accepts: per block, the agent's bytes if it edited the
/// block, else the intervening (green) bytes if it did, else the base bytes. Because an accepted
/// merge has no block both sides changed differently, this is well-defined.
pub(crate) fn reference_merge(green: &[u8], agent: &[u8]) -> Vec<u8> {
  let base = base_file();
  let mut out = Vec::with_capacity(base.len());
  for b in 0..BLOCKS {
    let range = b * BLOCK_LEN..(b + 1) * BLOCK_LEN;
    if agent[b] != 0 {
      out.extend_from_slice(&edit_bytes(agent[b]));
    } else if green[b] != 0 {
      out.extend_from_slice(&edit_bytes(green[b]));
    } else {
      out.extend_from_slice(&base[range]);
    }
  }
  out
}

/// The bytes of a file whose blocks carry `tags` (0 = the base block): the block oracle's view of
/// a file after any number of accepted edits.
pub(crate) fn blocks_to_bytes(tags: &[u8]) -> Vec<u8> {
  reference_merge(tags, &vec![0; tags.len()])
}
