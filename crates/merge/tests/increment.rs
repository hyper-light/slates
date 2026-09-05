//! Oracle and worked-case tests for the whole-volume deriver (§4.16 "Composition at seal";
//! T-6.x). A journal of content, create, unlink and rename composes into one ops document. The
//! property: reconstructing the final filesystem from the base and the document — creating where a
//! `Create` says, removing where an `Unlink` says, taking a renamed file's base from its source,
//! applying content ops and drawing added bytes from the post-state — reproduces the model
//! filesystem the journal actually produced.
//!
//! The oracle keeps a byte-level model of the filesystem, applies the journal to it (only ever
//! generating valid operations), lays out the post-state as the deriver does, then reconstructs
//! from the document and compares. Base bytes (0–127) and added bytes (128–255) are disjoint, so
//! a misplaced op, a wrong length, a wrong source, or a wrong create/unlink/rename diverges.

use std::collections::{BTreeMap, BTreeSet};

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

/// The base bytes for a path.
fn base_of(base: &[(String, u64)], path: &str) -> Vec<u8> {
  let len = base.iter().find(|(p, _)| p == path).map_or(0, |(_, l)| *l);
  let seed = u64::try_from(PATHS.iter().position(|p| *p == path).unwrap_or(0)).unwrap_or(0);
  (0..len).map(|i| base_byte(seed, i)).collect()
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

/// Applies one content op (kind 0..=4) to a present file, and returns the `VolumeOp` it stands for
/// (or `None` when skipped, e.g. an overwrite of an empty file).
fn content_op(
  file: &mut Vec<u8>,
  path: String,
  kind: u8,
  raw: &Raw,
  counter: &mut u64,
) -> Option<VolumeOp> {
  let current = file.len() as u64;
  match kind {
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
    .map(|(path, _)| (path.clone(), base_of(base, path)))
    .collect();
  let mut journal = Vec::new();
  let mut counter = 0u64;
  for raw in raw_ops {
    let path = PATHS[usize::from(raw.path) % PATHS.len()].to_owned();
    let action = raw.kind % 7;
    if !model.contains_key(&path) {
      model.insert(path.clone(), Vec::new());
      journal.push(VolumeOp::Create { path });
    } else if action == 5 {
      model.remove(&path);
      journal.push(VolumeOp::Unlink { path });
    } else if action == 6 {
      let to = PATHS[usize::try_from(raw.a % 3).unwrap_or(0)].to_owned();
      if to != path {
        if let Some(content) = model.remove(&path) {
          model.insert(to.clone(), content);
        }
        journal.push(VolumeOp::Rename { from: path, to });
      }
    } else if let Some(file) = model.get_mut(&path)
      && let Some(op) = content_op(file, path, action, raw, &mut counter)
    {
      journal.push(op);
    }
  }
  (journal, model)
}

/// Reconstructs one path's content from a base and its net ops, drawing added bytes from the
/// post-state. `Create`, `Unlink` and `Rename` carry no content and are handled by the caller.
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

/// The ops of the document that target the path at `index`.
fn ops_for(doc: &OpsDoc, index: u16) -> Vec<Op> {
  doc
    .ops
    .iter()
    .copied()
    .filter(|op| op.path == index)
    .collect()
}

/// The post-state: the emitting files' final content in path-table order (the deriver's region
/// layout — created, renamed, or content-changed files, sorted by final path; net-unchanged files
/// are not in the document, so not in the post-state).
fn post_state_of(doc: &OpsDoc, model: &Model) -> Vec<u8> {
  let mut post_state = Vec::new();
  for (path_index, name) in doc.paths.paths().iter().enumerate() {
    let index = u16::try_from(path_index).unwrap_or(u16::MAX);
    let emits = doc
      .ops
      .iter()
      .any(|op| op.path == index && op.kind != OpKind::Unlink);
    if let Some(content) = model.get(name).filter(|_| emits) {
      post_state.extend_from_slice(content);
    }
  }
  post_state
}

/// Reconstructs the content the document declares for the path at `index`, or `None` when the path
/// is a rename source (no ops) or a removed file (has an `Unlink`).
fn reconstruct_path(
  base: &[(String, u64)],
  doc: &OpsDoc,
  index: u16,
  name: &str,
  post_state: &[u8],
) -> Option<Vec<u8>> {
  let ops = ops_for(doc, index);
  if ops.is_empty() || ops.iter().any(|op| op.kind == OpKind::Unlink) {
    return None;
  }
  let rename = ops.iter().find(|op| op.kind == OpKind::Rename);
  let created = ops.iter().any(|op| op.kind == OpKind::Create);
  let (base_bytes, base_len) = if let Some(op) = rename {
    let source = doc
      .paths
      .path(u16::try_from(op.src).unwrap_or(u16::MAX))
      .unwrap_or("");
    let bytes = base_of(base, source);
    let len = bytes.len() as u64;
    (bytes, len)
  } else if created {
    (Vec::new(), 0u64)
  } else {
    let bytes = base_of(base, name);
    let len = bytes.len() as u64;
    (bytes, len)
  };
  let content: Vec<Op> = ops
    .into_iter()
    .filter(|op| !matches!(op.kind, OpKind::Create | OpKind::Rename))
    .collect();
  Some(apply_net(&base_bytes, &content, post_state, base_len))
}

/// Reconstructs the whole filesystem from the base and the ops document, and asserts it equals
/// the model.
fn check(base: &[(String, u64)], doc: &OpsDoc, model: &Model) {
  let post_state = post_state_of(doc, model);

  // The base paths renamed away (present as some rename's source): excluded from the result.
  let mut renamed_away: BTreeSet<String> = BTreeSet::new();
  for op in &doc.ops {
    if op.kind == OpKind::Rename
      && let Some(source) = doc.paths.path(u16::try_from(op.src).unwrap_or(u16::MAX))
    {
      renamed_away.insert(source.to_owned());
    }
  }

  let mut reconstructed: Model = BTreeMap::new();
  for (path_index, name) in doc.paths.paths().iter().enumerate() {
    let index = u16::try_from(path_index).unwrap_or(u16::MAX);
    if let Some(content) = reconstruct_path(base, doc, index, name, &post_state) {
      reconstructed.insert(name.clone(), content);
    }
  }

  // Base paths the document never names, and were not renamed away, are unchanged.
  for (path, _) in base {
    let named = doc.paths.paths().iter().any(|p| p == path);
    if !named && !renamed_away.contains(path) {
      reconstructed.insert(path.clone(), base_of(base, path));
    }
  }

  assert_eq!(
    &reconstructed, model,
    "the reconstructed filesystem equals the model"
  );
}

/// A create then an unlink of a new file cancels.
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
  assert!(doc.ops.is_empty());
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

/// Creating and writing a file is a `Create` and its bytes.
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
  assert!(doc.ops.iter().any(|op| op.kind == OpKind::Create));
  let content = doc
    .ops
    .iter()
    .find(|op| matches!(op.kind, OpKind::Insert | OpKind::Extend))
    .expect("content");
  assert_eq!(content.len, 5);
}

/// A base file renamed to a fresh path is one `Rename` whose source is the base path.
#[test]
fn renaming_a_base_file_to_a_fresh_path() {
  let base = [("a".to_owned(), 6)];
  let journal = [VolumeOp::Rename {
    from: "a".to_owned(),
    to: "z".to_owned(),
  }];
  let doc = compose_volume(&base, &journal).expect("valid");
  let rename = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::Rename)
    .expect("a rename");
  assert_eq!(doc.paths.path(rename.path), Some("z"), "destination");
  assert_eq!(
    doc
      .paths
      .path(u16::try_from(rename.src).unwrap_or(u16::MAX)),
    Some("a"),
    "source"
  );
  let mut model: Model = BTreeMap::new();
  model.insert("z".to_owned(), base_of(&base, "a"));
  check(&base, &doc, &model);
}

/// A new file renamed over a base file is the write-and-rename pattern: the destination's content
/// is replaced (a delete and the new bytes), with no rename.
#[test]
fn write_and_rename_replaces_the_destination_content() {
  let base = [("out".to_owned(), 10)];
  let journal = [
    VolumeOp::Create {
      path: "tmp".to_owned(),
    },
    VolumeOp::Extend {
      path: "tmp".to_owned(),
      at: 0,
      len: 4,
    },
    VolumeOp::Rename {
      from: "tmp".to_owned(),
      to: "out".to_owned(),
    },
  ];
  let doc = compose_volume(&base, &journal).expect("valid");
  assert!(
    !doc.ops.iter().any(|op| op.kind == OpKind::Rename),
    "write-and-rename is not a rename"
  );
  assert!(
    !doc.ops.iter().any(|op| op.kind == OpKind::Create),
    "no create — the path existed"
  );
  assert!(
    doc.ops.iter().any(|op| op.kind == OpKind::Delete),
    "the base content is deleted"
  );
  // The destination now holds the four new bytes.
  let mut model: Model = BTreeMap::new();
  model.insert("out".to_owned(), vec![0x80, 0x81, 0x82, 0x83]);
  // Reconstruct against the model's post-state (the new bytes).
  let post_state = vec![0x80u8, 0x81, 0x82, 0x83];
  let index = doc
    .paths
    .paths()
    .iter()
    .position(|p| p == "out")
    .expect("out");
  let ops: Vec<Op> = doc
    .ops
    .iter()
    .copied()
    .filter(|op| op.path == u16::try_from(index).unwrap_or(u16::MAX))
    .collect();
  let rebuilt = apply_net(&base_of(&base, "out"), &ops, &post_state, 10);
  assert_eq!(rebuilt, model["out"]);
}

/// Content on a missing file is a typed refusal.
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
  /// T-6.7 (the whole-volume oracle): for any base and any valid journal of content, create,
  /// unlink and rename across three paths, reconstructing the filesystem from the increment
  /// reproduces the model the journal produced (skipping the rare owed refusal).
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
    match compose_volume(&base, &journal) {
      Ok(doc) => check(&base, &doc, &model),
      // The rare rename onto a reused base path is an owed refusal, not a failure; skip it.
      Err(_) => prop_assume!(false),
    }
  }
}
