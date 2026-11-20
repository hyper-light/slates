//! The deriver names the node a hard link names (A-646, §4.16 "Data model"): the node at the target
//! at the work's base, or the one the work made there before the link (a file it created, an IPC
//! node, one it renamed there, a link it made there); a link to a node the work made and then
//! moved or removed is a composition owed, refused by type.

use super::{Base, DeriveError, VolumeOp, compose_volume};
use crate::ops_doc::{NodeOrigin, OpKind, OpsDoc};

/// The links of `doc`, each with its node record's path and origin.
fn links_of(doc: &OpsDoc) -> Vec<(String, String, NodeOrigin)> {
  doc
    .ops
    .iter()
    .filter(|op| op.kind == OpKind::Link)
    .filter_map(|op| {
      let node = doc.nodes.node(op.src)?;
      Some((
        doc.paths.path(op.path)?.to_owned(),
        doc.paths.path(node.path)?.to_owned(),
        node.origin,
      ))
    })
    .collect()
}

/// A link at `path` to `target`.
fn link(path: &str, target: &str) -> VolumeOp {
  VolumeOp::Link {
    path: path.to_owned(),
    target: target.to_owned(),
  }
}

/// A-646. Do: compose a link to a base file; a link to a file the work created first; and a link
/// to the path a base file was renamed to first. Expect the first to name the node at its target at
/// the base, and the other two the node the work made there.
#[test]
fn a_link_names_the_node_at_its_target_at_the_base_or_the_one_the_work_made() -> Result<(), String>
{
  let base = Base::of_files(vec![("a".to_owned(), 4)]);
  let cases = [
    (vec![link("b", "a")], ("b", "a", NodeOrigin::Base)),
    (
      vec![
        VolumeOp::Create {
          path: "c".to_owned(),
        },
        link("d", "c"),
      ],
      ("d", "c", NodeOrigin::Made),
    ),
    (
      vec![
        VolumeOp::Rename {
          from: "a".to_owned(),
          to: "e".to_owned(),
        },
        link("f", "e"),
      ],
      ("f", "e", NodeOrigin::Made),
    ),
  ];
  for (journal, (path, target, origin)) in cases {
    let doc = compose_volume(&base, &journal).map_err(|error| format!("{error:?}"))?;
    let expected = vec![(path.to_owned(), target.to_owned(), origin)];
    if links_of(&doc) != expected {
      return Err(format!("{journal:?}: {:?}", links_of(&doc)));
    }
  }
  Ok(())
}

/// A-646. Do: compose a link to a file the work created, then a rename of that file; and the same
/// with the file removed instead. Expect each refused `Unsupported` on the file: the link's node
/// record names the path the node was made at, which the work then left.
#[test]
fn a_link_to_a_node_the_work_made_then_moved_is_owed() -> Result<(), String> {
  let base = Base::of_files(Vec::new());
  let made = || VolumeOp::Create {
    path: "c".to_owned(),
  };
  let journals = [
    vec![
      made(),
      link("d", "c"),
      VolumeOp::Rename {
        from: "c".to_owned(),
        to: "x".to_owned(),
      },
    ],
    vec![
      made(),
      link("d", "c"),
      VolumeOp::Unlink {
        path: "c".to_owned(),
      },
    ],
  ];
  for journal in journals {
    match compose_volume(&base, &journal) {
      Err(DeriveError::Unsupported(path)) if path == "c" => {}
      other => return Err(format!("{journal:?}: {other:?}")),
    }
  }
  Ok(())
}

/// A copy of `from` at `version` to `to`, replacing or not (A-626 part 4).
fn copy(from: &str, to: &str, version: u64, replace: bool) -> VolumeOp {
  VolumeOp::Copy {
    from: from.to_owned(),
    to: to.to_owned(),
    version,
    replace,
  }
}

/// Each op of `doc` as its kind, its path and its `src` read as a path (a copy's or a rename's source), with its `at`
/// and its flags.
fn ops_of(doc: &OpsDoc) -> Vec<(OpKind, String, Option<String>, u64, u8)> {
  doc
    .ops
    .iter()
    .map(|op| {
      let path = doc.paths.path(op.path).unwrap_or_default().to_owned();
      let source = matches!(op.kind, OpKind::Copy | OpKind::Rename)
        .then(|| doc.paths.path(op.src).unwrap_or_default().to_owned());
      (op.kind, path, source, op.at, op.flags)
    })
    .collect()
}

/// A-626 part 4. Do: compose a journal that copies `trunk` at version 1 to `b` and overwrites a byte of `b/a`, over a
/// base that holds the copied tree at `b` (as `Green::base_for` lays it in). Expect one `Copy` (its destination `b`,
/// its source `trunk`, the version 1 as its `at`, no flag) and the overwrite of `b/a` in the copied file's own
/// coordinates, nothing for the files the copy brings and the work left alone; and the replace flag set for a copy
/// that replaces.
#[test]
fn a_copy_composes_to_one_copy_and_the_edits_beneath_it() -> Result<(), String> {
  let base = Base::of_files(vec![
    ("trunk/a".to_owned(), 5),
    ("b/a".to_owned(), 5),
    ("b/c".to_owned(), 5),
  ]);
  let overwrite = VolumeOp::Overwrite {
    path: "b/a".to_owned(),
    at: 0,
    len: 1,
  };
  let journal = [copy("trunk", "b", 1, false), overwrite];
  let doc = compose_volume(&base, &journal).map_err(|error| format!("{error:?}"))?;
  let kinds: Vec<(OpKind, String)> = ops_of(&doc)
    .into_iter()
    .map(|(kind, path, ..)| (kind, path))
    .collect();
  let copies: Vec<_> = ops_of(&doc)
    .into_iter()
    .filter(|op| op.0 == OpKind::Copy)
    .collect();
  let expected = vec![(OpKind::Copy, "b".to_owned(), Some("trunk".to_owned()), 1, 0)];
  if copies != expected
    || !kinds.contains(&(OpKind::Overwrite, "b/a".to_owned()))
    || kinds.len() != 2
  {
    return Err(format!("{:?}", ops_of(&doc)));
  }
  let replacing =
    compose_volume(&base, &[copy("trunk", "b", 1, true)]).map_err(|error| format!("{error:?}"))?;
  match ops_of(&replacing).first() {
    Some((OpKind::Copy, _, _, _, flags)) if *flags == crate::ops_doc::REPLACE => Ok(()),
    other => Err(format!("{other:?}")),
  }
}

/// A-626 part 4. Do: compose a journal that makes a file in `b`'s tree and then copies a tree to `b`; and two journals
/// declaring the same two copies in opposite orders. Expect the first refused `Unsupported` naming the file (it would be
/// composed against what the copy brings), and the other two one byte sequence: copies are laid out by destination.
#[test]
fn a_copy_after_its_tree_was_touched_is_refused_and_copies_lay_out_by_destination()
-> Result<(), String> {
  let base = Base::of_files(Vec::new());
  let touched = [
    VolumeOp::Create {
      path: "b/x".to_owned(),
    },
    copy("trunk", "b", 1, false),
  ];
  if compose_volume(&base, &touched) != Err(DeriveError::Unsupported("b/x".to_owned())) {
    return Err(format!("{:?}", compose_volume(&base, &touched)));
  }
  let one = [copy("trunk", "y", 1, false), copy("trunk", "x", 2, false)];
  let other = [copy("trunk", "x", 2, false), copy("trunk", "y", 1, false)];
  let encoded = |journal: &[VolumeOp]| compose_volume(&base, journal).map(|doc| doc.encode());
  match (encoded(&one), encoded(&other)) {
    (Ok(first), Ok(second)) if first == second => Ok(()),
    found => Err(format!("{found:?}")),
  }
}
