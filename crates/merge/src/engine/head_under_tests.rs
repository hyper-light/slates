//! A-792 (goal 86b): a green listed at its head reads one directory's subtree ([`Green::head_under`]), not every path
//! of the green ([`Green::current_base`]), which a listing copied whole for every page.

use std::collections::BTreeMap;

use super::{Green, Increment, Outcome};
use crate::increment::{VolumeOp, compose_volume_planned};
use crate::ops_doc::OpKind;

type Files = BTreeMap<String, Vec<u8>>;

/// The increment composing `journal` over `green`'s version `base`, its runs read from `files` (the seal's reading).
fn compose(
  green: &Green,
  (base, id): (u64, u8),
  journal: &[VolumeOp],
  files: &Files,
) -> Result<Increment, String> {
  let (doc, plan) = compose_volume_planned(&green.base_for(base, journal), journal)
    .map_err(|e| format!("{e:?}"))?;
  let mut post_state = Vec::new();
  for op in doc
    .ops
    .iter()
    .filter(|op| matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend))
  {
    let path = doc.paths.path(op.path).ok_or("an op names no path")?;
    let from = plan
      .get(&op.src)
      .and_then(|from| usize::try_from(*from).ok())
      .ok_or("no plan")?;
    let len = usize::try_from(op.len).map_err(|e| e.to_string())?;
    let run = files
      .get(path)
      .and_then(|bytes| bytes.get(from..from.saturating_add(len)))
      .ok_or("no run")?;
    let start = usize::try_from(op.src).map_err(|e| e.to_string())?;
    if post_state.len() < start.saturating_add(len) {
      post_state.resize(start.saturating_add(len), 0);
    }
    post_state
      .get_mut(start..start.saturating_add(len))
      .ok_or("outside")?
      .copy_from_slice(run);
  }
  Ok(Increment {
    id: [id; 32],
    base,
    doc,
    post_state,
    evidence: Vec::new(),
    named: Default::default(),
  })
}

/// Submits `journal` (its files `files`) at `green`'s head, which must accept it.
fn accept(green: &mut Green, id: u8, journal: &[VolumeOp], files: &Files) -> Result<(), String> {
  let increment = compose(green, (green.head(), id), journal, files)?;
  match green.submit(&increment) {
    Outcome::Accepted { .. } | Outcome::Identical { .. } => Ok(()),
    other => Err(format!("a fixture's work was answered {other:?}")),
  }
}

/// The fixture: directories `a`, `a/b`, `a/b/c` and `ab` (a sibling whose name starts with `a`, the prefix trap),
/// files in each and at the top, and a symlink under `a`.
fn fixture() -> Result<Green, String> {
  let mut green = Green::new();
  let mut journal: Vec<VolumeOp> = ["a", "a/b", "a/b/c", "ab"]
    .into_iter()
    .map(|path| VolumeOp::Mkdir {
      path: path.to_owned(),
    })
    .collect();
  let mut files = Files::new();
  for (path, bytes) in [
    ("a/x", &b"ex"[..]),
    ("a/b/y", b"why"),
    ("a/b/c/z", b"zed!"),
    ("ab/w", b"w"),
    ("top", b"top file"),
  ] {
    let len = u64::try_from(bytes.len()).map_err(|e| e.to_string())?;
    journal.push(VolumeOp::Create {
      path: path.to_owned(),
    });
    journal.push(VolumeOp::Extend {
      path: path.to_owned(),
      at: 0,
      len,
    });
    files.insert(path.to_owned(), bytes.to_vec());
  }
  journal.push(VolumeOp::Symlink {
    path: "a/link".to_owned(),
    target: "x".to_owned(),
  });
  accept(&mut green, 1, &journal, &files)?;
  Ok(green)
}

/// A-792. Do: over the fixture, take [`Green::head_under`] for the root, `a`, `a/b`, `a/b/c`, `ab`, the file `top`
/// and a missing `q`; take the whole base once ([`Green::current_base`]) and keep, of each dimension, the paths that are
/// the directory or lie under it (every path for the root). Expect them equal, dimension by dimension, a file's length
/// with it; and `a/b`'s far fewer than the whole green's, so the subtree is all a listing of it reads.
#[test]
fn a_greens_head_under_a_directory_is_its_subtree_of_the_whole_base() -> Result<(), String> {
  let green = fixture()?;
  let base = green.current_base();
  for dir in ["", "a", "a/b", "a/b/c", "ab", "top", "q"] {
    let prefix = format!("{dir}/");
    let keep = |path: &str| dir.is_empty() || path == dir || path.starts_with(&prefix);
    let under = green.head_under(dir);
    let files: Vec<(String, u64)> = base
      .files
      .iter()
      .filter(|(path, _)| keep(path))
      .cloned()
      .collect();
    let dirs: Vec<String> = base
      .dirs
      .iter()
      .filter(|path| keep(path))
      .cloned()
      .collect();
    let symlinks: Vec<String> = base
      .symlinks
      .iter()
      .filter(|(path, _)| keep(path))
      .map(|(path, _)| path.clone())
      .collect();
    let specials: Vec<String> = base
      .specials
      .iter()
      .filter(|(path, _)| keep(path))
      .map(|(path, _)| path.clone())
      .collect();
    assert_eq!(under.files, files, "files under {dir:?}");
    assert_eq!(under.dirs, dirs, "directories under {dir:?}");
    assert_eq!(under.symlinks, symlinks, "symlinks under {dir:?}");
    assert_eq!(under.specials, specials, "special nodes under {dir:?}");
  }
  let sub = green.head_under("a/b");
  assert_eq!(sub.files.len(), 2, "a/b/y and a/b/c/z: {:?}", sub.files);
  assert!(
    sub.files.len() < base.files.len(),
    "the subtree, not the green"
  );
  assert!(
    green
      .head_under("a")
      .files
      .iter()
      .all(|(path, _)| !path.starts_with("ab")),
    "`ab` is no part of `a`"
  );
  Ok(())
}
