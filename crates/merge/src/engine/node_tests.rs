//! A hard link names its node, never a path (A-646; A-643's second sibling; §4.16 "The verdict",
//! POSIX `link`, `unlink` and `rename`). A node is its home, the one path holding its content or
//! IPC node, and the hard links naming it there. Removing or replacing the home leaves the node to
//! its links, the least of them its new home with its content, mode and attributes; renaming the
//! home moves its links with it; and a later file at a path the node left is another node, which
//! no link names. A link a work makes names the node its base held at the target.

use super::replaced_node_tests::{
  FILE, HARDLINK, Kind, NAME, SPECIAL, TARGET, Work, accept, classes_on, green, kinds_at,
  removal_of,
};
use super::{Green, Outcome};
use crate::ops_doc::OpKind;
use crate::verdict::MergeConflictClass;

/// Shape: a second hard link to `TARGET` the tests make.
const SECOND: &str = "second";

/// A green whose `TARGET` (mode 0o600, `NAME` set to `w`) is named by `HARDLINK` and `SECOND`.
fn linked() -> Result<Green, String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Link, SECOND, TARGET),
    1,
  )?;
  accept(&mut green, Work::default().mode(TARGET, 0o600), 2)?;
  accept(&mut green, Work::default().set_xattr(TARGET, NAME, b"w"), 3)?;
  Ok(green)
}

/// What a green answers of `path` as a node's home: its kinds, bytes, mode and `NAME`'s value.
type Home = (Vec<Kind>, Option<Vec<u8>>, Option<u32>, Option<Vec<u8>>);

/// What `green` answers of `path` as a node's home.
fn home(green: &Green, path: &str) -> Home {
  (
    kinds_at(green, path),
    green.content(path).map(<[u8]>::to_vec),
    green.mode(path),
    green.xattr(path, NAME).map(<[u8]>::to_vec),
  )
}

/// `TARGET`'s node as `linked` made it, held at a home.
fn targets_node() -> Home {
  (
    vec![Kind::File],
    Some(b"<g>\n".to_vec()),
    Some(0o600),
    Some(b"w".to_vec()),
  )
}

/// A-646. Do: remove `TARGET`, which `HARDLINK` and `SECOND` name, by a work based at the head.
/// Expect it accepted and the node kept at its links: `HARDLINK`, the least, holds its bytes,
/// mode and attribute, `SECOND` names it there, and nothing stands at `TARGET`. Before, both links
/// named the empty path.
#[test]
fn a_removed_files_links_keep_its_node() -> Result<(), String> {
  let mut green = linked()?;
  accept(&mut green, removal_of(TARGET, Kind::File), 4)?;
  let found = (
    home(&green, HARDLINK),
    green.hardlink(SECOND),
    kinds_at(&green, TARGET),
  );
  if found != (targets_node(), Some(HARDLINK), Vec::new()) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: remove `TARGET`, which `HARDLINK` and `SECOND` name, then make a new file there.
/// Expect the new file another node: `SECOND` still names `HARDLINK`, which keeps the old bytes,
/// and no link names `TARGET`. Before, the links named whatever was made at the path.
#[test]
fn a_file_made_again_where_a_node_was_is_another_node() -> Result<(), String> {
  let mut green = linked()?;
  accept(&mut green, removal_of(TARGET, Kind::File), 4)?;
  accept(&mut green, Work::default().create(TARGET, b"<new>\n"), 5)?;
  let found = (
    green.hardlink(SECOND),
    green.content(HARDLINK).map(<[u8]>::to_vec),
    green.content(TARGET).map(<[u8]>::to_vec),
  );
  let expected = (
    Some(HARDLINK),
    Some(b"<g>\n".to_vec()),
    Some(b"<new>\n".to_vec()),
  );
  if found != expected {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646 (POSIX `rename` moves the node, and its links name it at its new path). Do: rename
/// `TARGET`, which `HARDLINK` and `SECOND` name, to a free path. Expect both links to name the new
/// path, which holds the bytes. Before, both named the path the file had left.
#[test]
fn a_renamed_files_links_name_it_at_its_new_path() -> Result<(), String> {
  let mut green = linked()?;
  accept(
    &mut green,
    Work::default().naming(OpKind::Rename, "moved", TARGET),
    4,
  )?;
  let found = (
    green.hardlink(HARDLINK),
    green.hardlink(SECOND),
    green.content("moved").map(<[u8]>::to_vec),
  );
  if found != (Some("moved"), Some("moved"), Some(b"<g>\n".to_vec())) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: rename `FILE` over `TARGET`, which `HARDLINK` and `SECOND` name. Expect `TARGET` to
/// hold `FILE`'s bytes, a new node, and the old node kept at `HARDLINK` with its bytes, mode and
/// attribute, `SECOND` naming it there. Before, the links named the bytes renamed over the path.
#[test]
fn a_file_replaced_by_a_rename_leaves_its_node_to_its_links() -> Result<(), String> {
  let mut green = linked()?;
  accept(
    &mut green,
    Work::default().naming(OpKind::Rename, TARGET, FILE),
    4,
  )?;
  let found = (
    home(&green, HARDLINK),
    green.hardlink(SECOND),
    green.content(TARGET).map(<[u8]>::to_vec),
  );
  if found != (targets_node(), Some(HARDLINK), Some(b"<f>\n".to_vec())) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: link `pipe` to the FIFO `SPECIAL`, then remove `SPECIAL` by a work based at the
/// head. Expect it accepted and the FIFO kept at `pipe`, nothing at `SPECIAL`. Before, the removal
/// was refused `DeleteModify`, since the link would have named nothing.
#[test]
fn an_ipc_nodes_links_keep_it_when_its_home_is_removed() -> Result<(), String> {
  let mut green = green();
  let fifo = green.special(SPECIAL);
  accept(
    &mut green,
    Work::default().naming(OpKind::Link, "pipe", SPECIAL),
    1,
  )?;
  accept(&mut green, removal_of(SPECIAL, Kind::Special), 2)?;
  let found = (
    kinds_at(&green, "pipe"),
    green.special("pipe"),
    kinds_at(&green, SPECIAL),
  );
  if fifo.is_none() || found != (vec![Kind::Special], fifo, Vec::new()) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: link `third` to `HARDLINK`, itself a link to `TARGET`, by a work based at the head.
/// Expect `third` to name `TARGET`, the node's home, never the link's path. Before, it named the
/// link.
#[test]
fn a_link_to_a_link_names_its_node() -> Result<(), String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Link, "third", HARDLINK),
    1,
  )?;
  match green.hardlink("third") {
    Some(TARGET) => Ok(()),
    other => Err(format!("{other:?}")),
  }
}

/// A-646. Do: rename `TARGET` after version 0, then submit a work based at 0 that links `third` to
/// `TARGET`. Expect `DeleteModify` on `third`: the node the work saw there has moved since, and a
/// link never names whatever stands at a path now (A-643's rule, kept).
#[test]
fn a_link_based_before_its_target_moved_conflicts() -> Result<(), String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Rename, "moved", TARGET),
    1,
  )?;
  let answer = green.submit(
    &Work::default()
      .naming(OpKind::Link, "third", TARGET)
      .at(0, 2),
  );
  let refused = classes_on(&answer, "third") == Some(vec![MergeConflictClass::DeleteModify]);
  if !refused || green.hardlink("third").is_some() {
    return Err(format!("{answer:?}"));
  }
  Ok(())
}

/// Submits `work` at the head of `live` and replays its chain entry onto `replica`.
fn submit_and_replay(
  live: &mut Green,
  replica: &mut Green,
  work: Work,
  id: u64,
) -> Result<(), String> {
  let increment = work.at(live.head(), id);
  match live.submit_resolved(&increment) {
    (Outcome::Accepted { .. }, Some(applied)) => {
      replica.apply_recorded(&increment, &applied);
      Ok(())
    }
    other => Err(format!("{other:?}")),
  }
}

/// A-646 (the node moves are derived, never recorded). Do: link `SECOND` to `TARGET`, rename it,
/// then remove the renamed home, each version's chain entry replayed onto a second green. Expect
/// both greens' identities equal after every version, and each equal to its recount from scratch:
/// a replay derives the same node moves, and every key they change is marked for the identity.
#[test]
fn a_replay_derives_the_same_node_moves() -> Result<(), String> {
  let (mut live, mut replica) = (green(), green());
  let works = [
    Work::default().naming(OpKind::Link, SECOND, TARGET),
    Work::default().naming(OpKind::Rename, "moved", TARGET),
    removal_of("moved", Kind::File),
  ];
  for (id, work) in (1..).zip(works) {
    submit_and_replay(&mut live, &mut replica, work, id)?;
    let identities = (
      live.head_identity(),
      replica.head_identity(),
      live.head_identity_from_scratch(),
    );
    if identities.0 != identities.1 || identities.0 != identities.2 {
      return Err(format!("version {id}: {identities:?}"));
    }
  }
  let found = (live.hardlink(SECOND), live.content(HARDLINK));
  if found != (Some(HARDLINK), Some(&b"<g>\n"[..])) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}
