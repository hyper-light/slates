//! The verdict decides an IPC node's removal by lookups, whatever the green holds (A-646; A-643's
//! third sibling). It had scanned every attribute change and every hard link of the green for each
//! removal, O(changes) for one operation on the verdict's path, where the change index
//! (`latest_change`, through `changed_after`) and the reverse map of the links (`links_to`) answer
//! by lookup. A counter of the entries the verdict examines, kept in test builds, is the proof.

use std::cell::Cell;

use super::{Green, Increment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::special::{SpecialKind, SpecialNode};
use crate::verdict::MergeConflictClass;

thread_local! {
  /// The entries the verdict examined on this thread ([`count_examined`]).
  static EXAMINED: Cell<u64> = const { Cell::new(0) };
}

/// Counts one entry the verdict examined: a lookup of a path's last change, or of the links naming
/// it (test builds).
pub(super) fn count_examined() {
  EXAMINED.with(|count| count.set(count.get().saturating_add(1)));
}

/// The entries examined on this thread since the last read, the count reset.
fn examined() -> u64 {
  EXAMINED.with(|count| count.replace(0))
}

/// Shape: the files of the green, each named by a hard link and with an attribute changed after
/// the origin: the entries a scan of the green examined for every removal.
const FILES: usize = 1_000;
/// Shape: the entries the verdict examines deciding an IPC node's removal: its last change and the
/// links naming it.
const LOOKUPS: u64 = 2;
/// Shape: the IPC node the works remove.
const NODE: &str = "p";
/// Shape: the attribute the works set.
const NAME: &str = "user.k";

/// A FIFO.
fn fifo() -> SpecialNode {
  SpecialNode {
    kind: SpecialKind::Fifo,
    uid: 1,
    gid: 1,
    atime: 1,
    mtime: 1,
    ctime: 1,
    btime: 1,
  }
}

/// The increment `id` of `doc`, its post-state `post`, based at `base`.
fn increment(doc: OpsDoc, post: Vec<u8>, base: u64, id: u8) -> Increment {
  let mut identity = [0u8; 32];
  identity[0] = id;
  Increment {
    id: identity,
    base,
    doc,
    post_state: post,
    evidence: Vec::new(),
  }
}

/// An ops document setting `NAME` to the post-state's one byte on each of `paths`.
fn set_attribute<'a>(paths: impl Iterator<Item = &'a str>) -> OpsDoc {
  let mut doc = OpsDoc::new();
  let name = doc.paths.intern(NAME);
  for path in paths {
    let path = doc.paths.intern(path);
    doc.ops.push(Op {
      kind: OpKind::SetXattr,
      flags: 0,
      path,
      at: name,
      len: 1,
      src: 0,
    });
  }
  doc
}

/// An ops document removing `NODE`.
fn remove_node() -> OpsDoc {
  let mut doc = OpsDoc::new();
  let path = doc.paths.intern(NODE);
  doc.ops.push(Op {
    kind: OpKind::Unlink,
    flags: 0,
    path,
    at: 0,
    len: 0,
    src: u64::MAX,
  });
  doc
}

/// A green of the FIFO `NODE` and of `files` files, each named by a hard link.
fn green_of(files: usize) -> Green {
  let mut origin = Origin::default();
  for number in 0..files {
    origin.files.push((format!("x{number:04}"), b"x".to_vec()));
    origin
      .hardlinks
      .push((format!("y{number:04}"), format!("x{number:04}")));
  }
  origin.specials.push((NODE.to_owned(), fifo()));
  Green::with_origin(origin)
}

/// Submits `increment`, expecting it accepted.
fn accept(green: &mut Green, increment: &Increment) -> Result<(), String> {
  match green.submit(increment) {
    Outcome::Accepted { .. } => Ok(()),
    other => Err(format!("a setup work was answered {other:?}")),
  }
}

/// A-646 (A-643's third sibling). Do: in a green of a thousand files, each named by a hard link,
/// set an attribute on every file; then remove the FIFO by a work based at the head, counting the
/// entries the verdict examines. Expect it accepted, the FIFO gone, and two entries examined, its
/// last change and the links naming it, where a scan of every attribute change and every hard link
/// examined two thousand.
#[test]
fn an_ipc_nodes_removal_examines_two_entries_whatever_the_green_holds() -> Result<(), String> {
  let mut green = green_of(FILES);
  let names: Vec<String> = (0..FILES).map(|number| format!("x{number:04}")).collect();
  let doc = set_attribute(names.iter().map(String::as_str));
  accept(&mut green, &increment(doc, b"v".to_vec(), 0, 1))?;
  let head = green.head();
  examined();
  let answer = green.submit(&increment(remove_node(), Vec::new(), head, 2));
  let count = examined();
  let stands = green.special(NODE).is_some();
  if !matches!(answer, Outcome::Accepted { .. }) || stands || count != LOOKUPS {
    return Err(format!(
      "{answer:?}, the node stands: {stands}, {count} entries examined"
    ));
  }
  Ok(())
}

/// A-646. Do: set an attribute on the FIFO after version 0, then remove it by a work based at 0.
/// Expect `DeleteModify` and the FIFO standing: the lookup of its last change sees the attribute.
#[test]
fn an_ipc_node_changed_after_a_works_base_is_not_removed() -> Result<(), String> {
  let mut green = green_of(0);
  accept(
    &mut green,
    &increment(set_attribute([NODE].into_iter()), b"v".to_vec(), 0, 1),
  )?;
  let answer = green.submit(&increment(remove_node(), Vec::new(), 0, 2));
  let refused = matches!(&answer, Outcome::Conflict { windows }
    if windows.iter().any(|window| window.path == NODE
      && window.class == MergeConflictClass::DeleteModify));
  if !refused || green.special(NODE).is_none() {
    return Err(format!("{answer:?}"));
  }
  Ok(())
}
