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
