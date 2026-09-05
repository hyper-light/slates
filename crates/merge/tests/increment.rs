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

use slates_merge::increment::{Base, DeriveError, VolumeOp, compose_volume};
use slates_merge::ops_doc::{Op, OpKind, OpsDoc};

use proptest::prelude::*;

/// Derives an increment from a file-only base (the common case in these tests).
fn derive(
  files: &[(String, u64)],
  journal: &[VolumeOp],
) -> Result<slates_merge::ops_doc::OpsDoc, DeriveError> {
  compose_volume(&Base::of_files(files.to_vec()), journal)
}

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
  let doc = derive(
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
  let doc = derive(
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
  let doc = derive(
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
  let doc = derive(&base, &journal).expect("valid");
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
  let doc = derive(&base, &journal).expect("valid");
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
  let result = derive(
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
    match derive(&base, &journal) {
      Ok(doc) => check(&base, &doc, &model),
      // The rare rename onto a reused base path is an owed refusal, not a failure; skip it.
      Err(_) => prop_assume!(false),
    }
  }
}

// --- Directory composition (Mkdir/Rmdir) ---

/// The directory paths a directory journal touches (disjoint from the file paths above).
const DIRS: [&str; 2] = ["d", "e"];

/// Making a new directory is one `Mkdir`.
#[test]
fn making_a_new_directory_is_one_mkdir() {
  let doc = compose_volume(
    &Base::default(),
    &[VolumeOp::Mkdir {
      path: "d".to_owned(),
    }],
  )
  .expect("valid");
  assert_eq!(doc.ops.len(), 1);
  assert_eq!(doc.ops[0].kind, OpKind::Mkdir);
  assert_eq!(doc.paths.path(doc.ops[0].path), Some("d"));
}

/// Removing a base directory is one `Rmdir`.
#[test]
fn removing_a_base_directory_is_one_rmdir() {
  let base = Base {
    files: Vec::new(),
    dirs: vec!["d".to_owned()],
    modes: Vec::new(),
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[VolumeOp::Rmdir {
      path: "d".to_owned(),
    }],
  )
  .expect("valid");
  assert_eq!(doc.ops.len(), 1);
  assert_eq!(doc.ops[0].kind, OpKind::Rmdir);
  assert_eq!(doc.paths.path(doc.ops[0].path), Some("d"));
}

/// A mkdir then an rmdir of a new directory cancels.
#[test]
fn mkdir_then_rmdir_cancels() {
  let doc = compose_volume(
    &Base::default(),
    &[
      VolumeOp::Mkdir {
        path: "d".to_owned(),
      },
      VolumeOp::Rmdir {
        path: "d".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(doc.ops.is_empty());
}

/// Removing then recreating a base directory is nothing (directories have no content).
#[test]
fn removing_then_recreating_a_base_directory_is_nothing() {
  let base = Base {
    files: Vec::new(),
    dirs: vec!["d".to_owned()],
    modes: Vec::new(),
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[
      VolumeOp::Rmdir {
        path: "d".to_owned(),
      },
      VolumeOp::Mkdir {
        path: "d".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(doc.ops.is_empty());
}

/// One path used as both a file and a directory is refused.
#[test]
fn a_file_and_directory_at_one_path_refuses() {
  let result = compose_volume(
    &Base::default(),
    &[
      VolumeOp::Create {
        path: "x".to_owned(),
      },
      VolumeOp::Mkdir {
        path: "x".to_owned(),
      },
    ],
  );
  assert!(matches!(
    result,
    Err(DeriveError::PathIsFileAndDirectory(_))
  ));
}

/// A mkdir over a base directory is refused.
#[test]
fn mkdir_over_a_base_directory_refuses() {
  let base = Base {
    files: Vec::new(),
    dirs: vec!["d".to_owned()],
    modes: Vec::new(),
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  assert!(matches!(
    compose_volume(
      &base,
      &[VolumeOp::Mkdir {
        path: "d".to_owned()
      }]
    ),
    Err(DeriveError::MkdirOverExisting(_))
  ));
}

/// An rmdir of a missing directory is refused.
#[test]
fn rmdir_of_a_missing_directory_refuses() {
  assert!(matches!(
    compose_volume(
      &Base::default(),
      &[VolumeOp::Rmdir {
        path: "d".to_owned()
      }]
    ),
    Err(DeriveError::RmdirMissing(_))
  ));
}

/// Generates a valid directory journal over [`DIRS`] against a set of base directories, tracking
/// the resulting present set.
fn simulate_dirs(base_dirs: &[String], raw_ops: &[Raw]) -> (Vec<VolumeOp>, BTreeSet<String>) {
  let mut present: BTreeSet<String> = base_dirs.iter().cloned().collect();
  let mut journal = Vec::new();
  for raw in raw_ops {
    let path = DIRS[usize::from(raw.path) % DIRS.len()].to_owned();
    if present.contains(&path) {
      present.remove(&path);
      journal.push(VolumeOp::Rmdir { path });
    } else {
      present.insert(path.clone());
      journal.push(VolumeOp::Mkdir { path });
    }
  }
  (journal, present)
}

proptest! {
  /// T-6.10 (the directory oracle): for any base directories and any valid mkdir/rmdir journal,
  /// applying the document's Mkdir and Rmdir ops to the base directory set yields the set the
  /// journal actually produced.
  #[test]
  fn the_directory_increment_reconstructs(
    base_present in prop::array::uniform2(any::<bool>()),
    raw in proptest::collection::vec(
      (0u8..2, any::<u8>(), 0u64..8, 0u64..8).prop_map(|(path, kind, a, b)| Raw { path, kind, a, b }),
      0..12,
    ),
  ) {
    let base_dirs: Vec<String> = DIRS
      .iter()
      .enumerate()
      .filter(|(i, _)| base_present[*i])
      .map(|(_, name)| (*name).to_owned())
      .collect();
    let (journal, model) = simulate_dirs(&base_dirs, &raw);
    let base = Base {
      files: Vec::new(),
      dirs: base_dirs.clone(),
      modes: Vec::new(),
      symlinks: Vec::new(),
      xattrs: Vec::new(),
      hardlinks: Vec::new(),
    };
    let doc = compose_volume(&base, &journal).expect("a valid directory journal composes");
    // Reconstruct: start from the base directories, apply the document's Mkdir/Rmdir.
    let mut reconstructed: BTreeSet<String> = base_dirs.into_iter().collect();
    for op in &doc.ops {
      let name = doc.paths.path(op.path).unwrap_or("").to_owned();
      match op.kind {
        OpKind::Mkdir => { reconstructed.insert(name); }
        OpKind::Rmdir => { reconstructed.remove(&name); }
        _ => {}
      }
    }
    prop_assert_eq!(reconstructed, model);
  }
}

// --- Mode composition (SetMode) ---

/// Setting a base file's mode to a new value is one `SetMode` carrying the mode.
#[test]
fn setting_a_base_file_mode_is_one_set_mode() {
  let base = Base {
    files: vec![("f".to_owned(), 4)],
    dirs: Vec::new(),
    modes: vec![("f".to_owned(), 0o644)],
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[VolumeOp::SetMode {
      path: "f".to_owned(),
      mode: 0o755,
    }],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::SetMode)
    .expect("a set-mode");
  assert_eq!(doc.paths.path(op.path), Some("f"));
  assert_eq!(op.len, 0o755, "the new mode is carried in len");
}

/// Setting a base file's mode to its existing mode declares nothing (minimality).
#[test]
fn setting_a_mode_to_the_base_mode_is_nothing() {
  let base = Base {
    files: vec![("f".to_owned(), 4)],
    dirs: Vec::new(),
    modes: vec![("f".to_owned(), 0o644)],
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[VolumeOp::SetMode {
      path: "f".to_owned(),
      mode: 0o644,
    }],
  )
  .expect("valid");
  assert!(doc.ops.is_empty(), "no net change");
}

/// The last SetMode on a path wins.
#[test]
fn the_last_set_mode_wins() {
  let base = Base {
    files: vec![("f".to_owned(), 4)],
    dirs: Vec::new(),
    modes: vec![("f".to_owned(), 0o644)],
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[
      VolumeOp::SetMode {
        path: "f".to_owned(),
        mode: 0o600,
      },
      VolumeOp::SetMode {
        path: "f".to_owned(),
        mode: 0o640,
      },
    ],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::SetMode)
    .expect("a set-mode");
  assert_eq!(op.len, 0o640);
}

/// Setting the mode of a base directory is one `SetMode`.
#[test]
fn setting_a_base_directory_mode() {
  let base = Base {
    files: Vec::new(),
    dirs: vec!["d".to_owned()],
    modes: vec![("d".to_owned(), 0o755)],
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[VolumeOp::SetMode {
      path: "d".to_owned(),
      mode: 0o700,
    }],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::SetMode)
    .expect("a set-mode");
  assert_eq!(doc.paths.path(op.path), Some("d"));
  assert_eq!(op.len, 0o700);
}

/// Setting the mode of a newly created file emits a `SetMode` (the base had no mode there).
#[test]
fn setting_a_new_file_mode() {
  let doc = compose_volume(
    &Base::default(),
    &[
      VolumeOp::Create {
        path: "n".to_owned(),
      },
      VolumeOp::SetMode {
        path: "n".to_owned(),
        mode: 0o600,
      },
    ],
  )
  .expect("valid");
  assert!(
    doc
      .ops
      .iter()
      .any(|op| op.kind == OpKind::SetMode && op.len == 0o600)
  );
}

/// Setting the mode of a path present nowhere is refused.
#[test]
fn set_mode_on_a_missing_path_refuses() {
  assert!(matches!(
    compose_volume(
      &Base::default(),
      &[VolumeOp::SetMode {
        path: "gone".to_owned(),
        mode: 0o644
      }]
    ),
    Err(DeriveError::SetModeMissing(_))
  ));
}

/// A mode set then the path renamed away is the owed chmod-then-rename case, refused.
#[test]
fn set_mode_then_rename_refuses() {
  let base = Base {
    files: vec![("a".to_owned(), 4)],
    dirs: Vec::new(),
    modes: vec![("a".to_owned(), 0o644)],
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let result = compose_volume(
    &base,
    &[
      VolumeOp::SetMode {
        path: "a".to_owned(),
        mode: 0o600,
      },
      VolumeOp::Rename {
        from: "a".to_owned(),
        to: "b".to_owned(),
      },
    ],
  );
  assert!(matches!(result, Err(DeriveError::Unsupported(_))));
}

// --- Symlink composition ---

/// The target string a Symlink op names (via its `src` index into the path table).
fn symlink_target<'a>(doc: &'a OpsDoc, op: &Op) -> Option<&'a str> {
  doc.paths.path(u16::try_from(op.src).unwrap_or(u16::MAX))
}

/// Creating a symlink is one `Symlink` op naming its target.
#[test]
fn creating_a_symlink_is_one_symlink() {
  let doc = compose_volume(
    &Base::default(),
    &[VolumeOp::Symlink {
      path: "link".to_owned(),
      target: "target/path".to_owned(),
    }],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::Symlink)
    .expect("a symlink");
  assert_eq!(doc.paths.path(op.path), Some("link"));
  assert_eq!(symlink_target(&doc, op), Some("target/path"));
}

/// A symlink created then unlinked cancels.
#[test]
fn symlink_then_unlink_cancels() {
  let doc = compose_volume(
    &Base::default(),
    &[
      VolumeOp::Symlink {
        path: "l".to_owned(),
        target: "t".to_owned(),
      },
      VolumeOp::Unlink {
        path: "l".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(doc.ops.is_empty());
}

/// Removing a base symlink is one `Unlink`.
#[test]
fn removing_a_base_symlink_is_one_unlink() {
  let base = Base {
    files: Vec::new(),
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: vec![("l".to_owned(), "t".to_owned())],
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[VolumeOp::Unlink {
      path: "l".to_owned(),
    }],
  )
  .expect("valid");
  assert_eq!(doc.ops.len(), 1);
  assert_eq!(doc.ops[0].kind, OpKind::Unlink);
  assert_eq!(doc.paths.path(doc.ops[0].path), Some("l"));
}

/// Retargeting a base symlink (unlink then symlink to a new target) is one `Symlink`, not an
/// unlink and a symlink (the removal is covered by the recreation).
#[test]
fn retargeting_a_base_symlink_is_one_symlink() {
  let base = Base {
    files: Vec::new(),
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: vec![("l".to_owned(), "old".to_owned())],
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[
      VolumeOp::Unlink {
        path: "l".to_owned(),
      },
      VolumeOp::Symlink {
        path: "l".to_owned(),
        target: "new".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(
    !doc.ops.iter().any(|op| op.kind == OpKind::Unlink),
    "no unlink — the recreation covers it"
  );
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::Symlink)
    .expect("a symlink");
  assert_eq!(symlink_target(&doc, op), Some("new"));
}

/// A base symlink removed then recreated to the same target is nothing.
#[test]
fn recreating_a_base_symlink_to_the_same_target_is_nothing() {
  let base = Base {
    files: Vec::new(),
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: vec![("l".to_owned(), "t".to_owned())],
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  let doc = compose_volume(
    &base,
    &[
      VolumeOp::Unlink {
        path: "l".to_owned(),
      },
      VolumeOp::Symlink {
        path: "l".to_owned(),
        target: "t".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(doc.ops.is_empty(), "net unchanged");
}

/// A symlink over an existing symlink is refused.
#[test]
fn symlink_over_an_existing_symlink_refuses() {
  let base = Base {
    files: Vec::new(),
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: vec![("l".to_owned(), "t".to_owned())],
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  assert!(matches!(
    compose_volume(
      &base,
      &[VolumeOp::Symlink {
        path: "l".to_owned(),
        target: "u".to_owned()
      }]
    ),
    Err(DeriveError::SymlinkOverExisting(_))
  ));
}

/// A symlink at a base file path is a kind conflict.
#[test]
fn symlink_at_a_base_file_path_conflicts() {
  let base = Base::of_files(vec![("f".to_owned(), 4)]);
  assert!(matches!(
    compose_volume(
      &base,
      &[VolumeOp::Symlink {
        path: "f".to_owned(),
        target: "t".to_owned()
      }]
    ),
    Err(DeriveError::PathKindConflict(_))
  ));
}

/// A write to a base symlink path is a kind conflict (a file operation on a symlink).
#[test]
fn a_write_on_a_base_symlink_conflicts() {
  let base = Base {
    files: Vec::new(),
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: vec![("l".to_owned(), "t".to_owned())],
    xattrs: Vec::new(),
    hardlinks: Vec::new(),
  };
  assert!(matches!(
    compose_volume(
      &base,
      &[VolumeOp::Overwrite {
        path: "l".to_owned(),
        at: 0,
        len: 1
      }]
    ),
    Err(DeriveError::PathIsFileAndDirectory(_))
  ));
}

// --- Xattr composition ---

/// Builds a base with one file and one xattr on it.
fn base_with_xattr(path: &str, name: &str, value: &[u8]) -> Base {
  Base {
    files: vec![(path.to_owned(), 4)],
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: Vec::new(),
    xattrs: vec![(path.to_owned(), name.to_owned(), value.to_vec())],
    hardlinks: Vec::new(),
  }
}

/// Setting an xattr to a new value emits one `SetXattr` (path index in `path`, name index in `at`,
/// value length in `len`).
#[test]
fn setting_an_xattr_to_a_new_value() {
  let base = base_with_xattr("f", "user.a", b"old");
  let doc = compose_volume(
    &base,
    &[VolumeOp::SetXattr {
      path: "f".to_owned(),
      name: "user.a".to_owned(),
      value: b"newer".to_vec(),
    }],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::SetXattr)
    .expect("a set-xattr");
  assert_eq!(doc.paths.path(op.path), Some("f"));
  assert_eq!(
    doc.paths.path(u16::try_from(op.at).unwrap_or(u16::MAX)),
    Some("user.a")
  );
  assert_eq!(op.len, 5, "the value length");
}

/// Setting an xattr to its base value declares nothing (minimality).
#[test]
fn setting_an_xattr_to_the_base_value_is_nothing() {
  let base = base_with_xattr("f", "user.a", b"same");
  let doc = compose_volume(
    &base,
    &[VolumeOp::SetXattr {
      path: "f".to_owned(),
      name: "user.a".to_owned(),
      value: b"same".to_vec(),
    }],
  )
  .expect("valid");
  assert!(doc.ops.is_empty(), "no net change");
}

/// Removing a base xattr emits one `RemoveXattr`.
#[test]
fn removing_a_base_xattr() {
  let base = base_with_xattr("f", "user.a", b"v");
  let doc = compose_volume(
    &base,
    &[VolumeOp::RemoveXattr {
      path: "f".to_owned(),
      name: "user.a".to_owned(),
    }],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::RemoveXattr)
    .expect("a remove-xattr");
  assert_eq!(doc.paths.path(op.path), Some("f"));
  assert_eq!(
    doc.paths.path(u16::try_from(op.at).unwrap_or(u16::MAX)),
    Some("user.a")
  );
}

/// Setting a new xattr not present at base emits a `SetXattr`, and its value is at the declared
/// post-state offset (here 0, the only content).
#[test]
fn setting_a_new_xattr_lays_the_value_in_the_post_state() {
  let base = Base::of_files(vec![("f".to_owned(), 4)]);
  let doc = compose_volume(
    &base,
    &[VolumeOp::SetXattr {
      path: "f".to_owned(),
      name: "user.new".to_owned(),
      value: b"value".to_vec(),
    }],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::SetXattr)
    .expect("a set-xattr");
  assert_eq!(op.src, 0, "the only post-state content, at offset 0");
  assert_eq!(op.len, 5);
}

/// Setting then removing an xattr that the base had emits a `RemoveXattr`.
#[test]
fn set_then_remove_a_base_xattr() {
  let base = base_with_xattr("f", "user.a", b"v");
  let doc = compose_volume(
    &base,
    &[
      VolumeOp::SetXattr {
        path: "f".to_owned(),
        name: "user.a".to_owned(),
        value: b"w".to_vec(),
      },
      VolumeOp::RemoveXattr {
        path: "f".to_owned(),
        name: "user.a".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(doc.ops.iter().any(|op| op.kind == OpKind::RemoveXattr));
  assert!(!doc.ops.iter().any(|op| op.kind == OpKind::SetXattr));
}

/// An xattr on a path present nowhere is refused.
#[test]
fn xattr_on_a_missing_path_refuses() {
  assert!(matches!(
    compose_volume(
      &Base::default(),
      &[VolumeOp::SetXattr {
        path: "gone".to_owned(),
        name: "n".to_owned(),
        value: b"v".to_vec()
      }]
    ),
    Err(DeriveError::XattrMissing(_))
  ));
}

// --- Hard link composition ---

/// Creating a hard link is one `Link` op naming its target.
#[test]
fn creating_a_hard_link_is_one_link() {
  let base = Base::of_files(vec![("a".to_owned(), 4)]);
  let doc = compose_volume(
    &base,
    &[VolumeOp::Link {
      path: "b".to_owned(),
      target: "a".to_owned(),
    }],
  )
  .expect("valid");
  let op = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::Link)
    .expect("a link");
  assert_eq!(doc.paths.path(op.path), Some("b"), "the new name");
  assert_eq!(
    doc.paths.path(u16::try_from(op.src).unwrap_or(u16::MAX)),
    Some("a"),
    "the target"
  );
}

/// A hard link created then unlinked cancels.
#[test]
fn hard_link_then_unlink_cancels() {
  let base = Base::of_files(vec![("a".to_owned(), 4)]);
  let doc = compose_volume(
    &base,
    &[
      VolumeOp::Link {
        path: "b".to_owned(),
        target: "a".to_owned(),
      },
      VolumeOp::Unlink {
        path: "b".to_owned(),
      },
    ],
  )
  .expect("valid");
  assert!(doc.ops.is_empty());
}

/// Removing a base hard link is one `Unlink`.
#[test]
fn removing_a_base_hard_link_is_one_unlink() {
  let base = Base {
    files: vec![("a".to_owned(), 4)],
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: vec![("b".to_owned(), "a".to_owned())],
  };
  let doc = compose_volume(
    &base,
    &[VolumeOp::Unlink {
      path: "b".to_owned(),
    }],
  )
  .expect("valid");
  assert_eq!(doc.ops.len(), 1);
  assert_eq!(doc.ops[0].kind, OpKind::Unlink);
  assert_eq!(doc.paths.path(doc.ops[0].path), Some("b"));
}

/// A hard link over an existing hard link is refused.
#[test]
fn hard_link_over_an_existing_link_refuses() {
  let base = Base {
    files: vec![("a".to_owned(), 4)],
    dirs: Vec::new(),
    modes: Vec::new(),
    symlinks: Vec::new(),
    xattrs: Vec::new(),
    hardlinks: vec![("b".to_owned(), "a".to_owned())],
  };
  assert!(matches!(
    compose_volume(
      &base,
      &[VolumeOp::Link {
        path: "b".to_owned(),
        target: "a".to_owned()
      }]
    ),
    Err(DeriveError::LinkOverExisting(_))
  ));
}

/// A hard link and a file at one path conflict.
#[test]
fn hard_link_at_a_file_path_conflicts() {
  let doc = compose_volume(
    &Base::default(),
    &[
      VolumeOp::Create {
        path: "x".to_owned(),
      },
      VolumeOp::Link {
        path: "x".to_owned(),
        target: "a".to_owned(),
      },
    ],
  );
  assert!(matches!(doc, Err(DeriveError::PathKindConflict(_))));
}
