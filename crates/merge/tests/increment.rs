//! Oracle and worked-case tests for the whole-volume deriver (§4.16 "Composition at seal";
//! T-6.x). A journal of content, create and unlink operations composes into one ops document. The
//! property: reconstructing the final filesystem from the base version and the document — creating
//! where a `Create` says, removing where an `Unlink` says, applying content ops and drawing added
//! bytes from the post-state — reproduces the model filesystem the journal actually produced.
//!
//! The oracle keeps a byte-level model of the filesystem, applies the journal to it (only ever
//! generating valid operations, so the deriver never refuses), lays out the post-state as the
//! deriver does, then reconstructs from the document and compares. Base bytes (values 0–127) and
//! added bytes (values 128–255) are disjoint, so a misplaced op, a wrong length, a wrong source,
//! or a missing create/unlink diverges.

use std::collections::BTreeMap;

use slates_merge::increment::{VolumeOp, compose_volume};
use slates_merge::ops_doc::{Op, OpKind, OpsDoc};

use proptest::prelude::*;

/// Shape: the largest run a generated add contributes.
const MAX_ADD: u64 = 16;

/// The fixed paths a journal touches.
const PATHS: [&str; 3] = ["a", "b", "c"];

/// A base byte for path `seed` at position `index`: a value in 0..=127.
fn base_byte(seed: u64, index: u64) -> u8 {
  u8::try_from((seed.wrapping_mul(31).wrapping_add(index.wrapping_mul(7))) & 0x7f).unwrap_or(0)
}

/// The next added byte: a value in 128..=255, varying by the global counter.
fn add_byte(counter: &mut u64) -> u8 {
  let value = 0x80 | (*counter & 0x7f);
  *counter = counter.wrapping_add(1);
  u8::try_from(value).unwrap_or(0x80)
}

/// The seed (and base-byte pattern) for a path name.
fn seed_of(path: &str) -> u64 {
  u64::try_from(PATHS.iter().position(|p| *p == path).unwrap_or(0)).unwrap_or(0)
}

/// A raw op the strategy generates.
#[derive(Clone, Copy, Debug)]
struct Raw {
  path: u8,
  kind: u8,
  a: u64,
  b: u64,
}

/// The oracle's model: the present files and their bytes.
type Model = BTreeMap<String, Vec<u8>>;

/// Applies one raw op to a present file, mutating its bytes, and returns the content `VolumeOp`
/// it stands for (or `None` when the raw op is skipped, e.g. an overwrite of an empty file).
fn content_op(file: &mut Vec<u8>, path: String, raw: &Raw, counter: &mut u64) -> Option<VolumeOp> {
  let current = file.len() as u64;
  match raw.kind % 6 {
    0 if current > 0 => {
      let at = raw.a % current;
      let len = 1 + raw.b % (current - at);
      for offset in 0..len {
        let index = usize::try_from(at + offset).unwrap_or(0);
        file[index] = add_byte(counter);
      }
      Some(VolumeOp::Overwrite { path, at, len })
    }
    1 => {
      let len = 1 + raw.b % MAX_ADD;
      let at = current;
      for _ in 0..len {
        let byte = add_byte(counter);
        file.push(byte);
      }
      Some(VolumeOp::Extend { path, at, len })
    }
    2 => {
      let len = raw.a % (current + MAX_ADD + 1);
      if len < current {
        file.truncate(usize::try_from(len).unwrap_or(0));
      } else if len > current {
        file.resize(usize::try_from(len).unwrap_or(0), 0);
      }
      Some(VolumeOp::Truncate { path, len })
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
      Some(VolumeOp::Insert { path, at, len })
    }
    4 if current > 0 => {
      let at = raw.a % current;
      let len = 1 + raw.b % (current - at);
      let start = usize::try_from(at).unwrap_or(0);
      let end = usize::try_from(at + len).unwrap_or(start);
      file.drain(start..end);
      Some(VolumeOp::Delete { path, at, len })
    }
    _ => None,
  }
}

/// Applies the raw ops to a model built from `base`, generating only valid `VolumeOp`s, and
/// returns the journal and the final model.
fn simulate(base: &[(String, u64)], raw_ops: &[Raw]) -> (Vec<VolumeOp>, Model) {
  let mut model: Model = base
    .iter()
    .map(|(path, len)| {
      let seed = seed_of(path);
      (
        path.clone(),
        (0..*len).map(|i| base_byte(seed, i)).collect(),
      )
    })
    .collect();
  let mut journal = Vec::new();
  let mut counter = 0u64;
  for raw in raw_ops {
    let path = PATHS[usize::from(raw.path) % PATHS.len()].to_owned();
    if !model.contains_key(&path) {
      // Only a create is valid on an absent path.
      model.insert(path.clone(), Vec::new());
      journal.push(VolumeOp::Create { path });
    } else if raw.kind % 6 == 5 {
      model.remove(&path);
      journal.push(VolumeOp::Unlink { path });
    } else if let Some(file) = model.get_mut(&path)
      && let Some(op) = content_op(file, path, raw, &mut counter)
    {
      journal.push(op);
    }
  }
  (journal, model)
}

/// Reconstructs one path's content from its base bytes and its net ops, drawing added bytes from
/// the post-state. The ops are in base coordinates and non-decreasing by `at`; `Create` and
/// `Unlink` carry no content and are handled by the caller.
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

/// Reconstructs the whole filesystem from the base and the ops document, and asserts it equals
/// the model the journal produced.
fn check(base: &[(String, u64)], doc: &OpsDoc, model: &Model) {
  let base_len_of = |path: &str| base.iter().find(|(p, _)| p == path).map_or(0, |(_, l)| *l);

  // The post-state: the final content of every path in the document that survives, in path-table
  // order (the deriver's region layout).
  let mut post_state = Vec::new();
  for name in doc.paths.paths() {
    if let Some(content) = model.get(name) {
      post_state.extend_from_slice(content);
    }
  }

  // Reconstruct each path the document names.
  let mut reconstructed: Model = BTreeMap::new();
  for (path_index, name) in doc.paths.paths().iter().enumerate() {
    let index = u16::try_from(path_index).unwrap_or(u16::MAX);
    let ops: Vec<Op> = doc
      .ops
      .iter()
      .copied()
      .filter(|op| op.path == index)
      .collect();
    if ops.iter().any(|op| op.kind == OpKind::Unlink) {
      continue; // removed
    }
    let created = ops.iter().any(|op| op.kind == OpKind::Create);
    let content: Vec<Op> = ops
      .into_iter()
      .filter(|op| op.kind != OpKind::Create)
      .collect();
    let (base_bytes, base_len) = if created {
      (Vec::new(), 0u64)
    } else {
      let seed = seed_of(name);
      let len = base_len_of(name);
      ((0..len).map(|i| base_byte(seed, i)).collect(), len)
    };
    reconstructed.insert(
      name.clone(),
      apply_net(&base_bytes, &content, &post_state, base_len),
    );
  }

  // Base paths the document does not name are unchanged from the base.
  for (path, len) in base {
    if !doc.paths.paths().iter().any(|p| p == path) {
      let seed = seed_of(path);
      reconstructed.insert(
        path.clone(),
        (0..*len).map(|i| base_byte(seed, i)).collect(),
      );
    }
  }

  assert_eq!(
    &reconstructed, model,
    "the reconstructed filesystem equals the model"
  );
}

/// A create then an unlink of a new file cancels: nothing is declared.
#[test]
fn a_create_then_unlink_cancels() {
  let doc = compose_volume(
    &[],
    &[
      VolumeOp::Create {
        path: "n".to_owned(),
      },
      VolumeOp::Unlink {
        path: "n".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(doc.ops.is_empty(), "create then unlink declares nothing");
  assert!(doc.paths.paths().is_empty());
}

/// Unlinking a base file is one `Unlink`.
#[test]
fn unlinking_a_base_file_is_one_unlink() {
  let doc = compose_volume(
    &[("d".to_owned(), 8)],
    &[VolumeOp::Unlink {
      path: "d".to_owned(),
    }],
  )
  .expect("valid");
  assert_eq!(doc.ops.len(), 1);
  assert_eq!(doc.ops[0].kind, OpKind::Unlink);
  assert_eq!(doc.paths.path(doc.ops[0].path), Some("d"));
}

/// Creating a file and writing it is a `Create` and its bytes as an insert.
#[test]
fn creating_and_writing_a_file_is_create_then_insert() {
  let doc = compose_volume(
    &[],
    &[
      VolumeOp::Create {
        path: "n".to_owned(),
      },
      VolumeOp::Extend {
        path: "n".to_owned(),
        at: 0,
        len: 5,
      },
    ],
  )
  .expect("valid");
  assert_eq!(doc.ops.len(), 2, "a create and one content op");
  assert!(
    doc.ops.iter().any(|op| op.kind == OpKind::Create),
    "the new file is declared with a create"
  );
  let content = doc
    .ops
    .iter()
    .find(|op| matches!(op.kind, OpKind::Insert | OpKind::Extend))
    .expect("the written bytes are an insert or extend");
  assert_eq!(content.len, 5);
}

/// Unlinking a base file then recreating and writing it replaces its content: a delete of the
/// base and the new bytes, with no `Create` (the path existed at base).
#[test]
fn recreating_a_base_file_replaces_its_content() {
  let doc = compose_volume(
    &[("d".to_owned(), 10)],
    &[
      VolumeOp::Unlink {
        path: "d".to_owned(),
      },
      VolumeOp::Create {
        path: "d".to_owned(),
      },
      VolumeOp::Extend {
        path: "d".to_owned(),
        at: 0,
        len: 4,
      },
    ],
  )
  .expect("valid");
  assert!(
    !doc.ops.iter().any(|op| op.kind == OpKind::Create),
    "a replaced base path declares no create"
  );
  assert!(doc.ops.iter().any(|op| op.kind == OpKind::Delete));
  assert!(
    doc
      .ops
      .iter()
      .any(|op| matches!(op.kind, OpKind::Insert | OpKind::Extend))
  );
}

/// Content on a missing file is a typed refusal, not a panic.
#[test]
fn content_on_a_missing_file_refuses() {
  let result = compose_volume(
    &[],
    &[VolumeOp::Overwrite {
      path: "gone".to_owned(),
      at: 0,
      len: 1,
    }],
  );
  assert!(matches!(
    result,
    Err(slates_merge::increment::DeriveError::ContentOnMissing(_))
  ));
}

proptest! {
  /// T-6.7 (the whole-volume oracle): for any base and any valid journal of content, create and
  /// unlink across three paths, reconstructing the filesystem from the increment reproduces the
  /// model the journal produced.
  #[test]
  fn the_increment_reconstructs_the_filesystem(
    base_present in prop::array::uniform3(any::<bool>()),
    base_lens in prop::array::uniform3(0u64..24),
    raw in proptest::collection::vec(
      (0u8..3, any::<u8>(), 0u64..64, 0u64..64)
        .prop_map(|(path, kind, a, b)| Raw { path, kind, a, b }),
      0..24,
    ),
  ) {
    let base: Vec<(String, u64)> = PATHS
      .iter()
      .enumerate()
      .filter(|(i, _)| base_present[*i])
      .map(|(i, name)| ((*name).to_owned(), base_lens[i]))
      .collect();
    let (journal, model) = simulate(&base, &raw);
    let composed = compose_volume(&base, &journal);
    prop_assert!(composed.is_ok(), "a generated journal composes");
    check(&base, &composed.unwrap_or_default(), &model);
  }
}
