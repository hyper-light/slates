//! A stack's upper that hands a write on to a link (A-669), and one that removes a file another work linked while the
//! stack's works were in flight (A-670): the question A-669 left of A-659's stacks, answered by use.

use super::Landed;
use super::tests::{Work, text, versions};
use crate::engine::{Green, Outcome};
use crate::ops_doc::{NodeOrigin, NodeRef, Op, OpKind};
use crate::origin::Origin;
use crate::verdict::MergeConflictClass;

/// A green whose origin holds `f` and `g`, each holding its name, and, when `linked`, `h` a hard link to `g`.
fn green(linked: bool) -> Green {
  let mut origin = Origin::default();
  for path in ["f", "g"] {
    origin
      .files
      .push((path.to_owned(), format!("<{path}>\n").into_bytes()));
  }
  if linked {
    origin.hardlinks.push(("h".to_owned(), "g".to_owned()));
  }
  Green::with_origin(origin)
}

/// `work` with a hard link at `path` naming the node at `target` at the work's base (A-646's node table).
fn linked(mut work: Work, path: &str, target: &str) -> Work {
  let link = work.doc.paths.intern(path);
  let target = work.doc.paths.intern(target);
  let node = work.doc.nodes.intern(NodeRef {
    path: target,
    origin: NodeOrigin::Base,
  });
  work.doc.ops.push(Op {
    kind: OpKind::Link,
    flags: 0,
    path: link,
    at: 0,
    len: 0,
    src: node,
  });
  work
}

/// Holds `held`, else says what was found.
fn check(held: bool, found: impl FnOnce() -> String) -> Result<(), String> {
  if held { Ok(()) } else { Err(found()) }
}

/// Holds that `outcome` is a conflict with a window of `class` at `path`.
fn conflicts_at(
  outcome: Option<&Landed>,
  (path, class): (&str, MergeConflictClass),
) -> Result<(), String> {
  match outcome {
    Some(Landed::Conflict { windows })
      if windows
        .iter()
        .any(|window| window.path == path && window.class == class) =>
    {
      Ok(())
    }
    other => Err(format!("landed {other:?}, not {class:?} at {path}")),
  }
}

/// A-670, A-669's question of A-659's stacks: an upper handing a write on to a link. Do: land a stack on a green holding
/// `f`, `g` and `h` a link to `g`: a bottom appending to `f`, and an upper over it that appends to `g` and removes it,
/// as the deriver declares that (the write at `h`, `g` removed). Expect both landed, `h` holding `g`'s bytes and the
/// write, nothing at `g`, and `f` appended to: the restatement carries the hand-over to the verdict, which makes `h`
/// the node's home with the write.
#[test]
fn an_upper_s_write_handed_on_to_a_link_lands_with_its_node() -> Result<(), String> {
  let mut green = green(true);
  let bottom = Work::default().extend("f", 4, b"<+b>\n").increment(1, 0);
  let upper = Work::default()
    .extend("h", 4, b"<+w>\n")
    .unlink("g")
    .upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  check(versions(&landed) == [Some(1), Some(2)], || {
    format!("landed {landed:?}")
  })?;
  let (handed, gone) = (text(&green, "h"), text(&green, "g"));
  check(
    handed.as_deref() == Some(&b"<g>\n<+w>\n"[..]) && gone.is_none(),
    || format!("h holds {handed:?} and g {gone:?}"),
  )?;
  check(
    text(&green, "f").as_deref() == Some(&b"<f>\n<+b>\n"[..]),
    || "f lost its append".to_owned(),
  )
}

/// A-670: an upper handing a write on meets a change another work made to the node's home. Do: on that green, land a
/// work appending to `g` as version 1, then a stack based at 0 whose upper hands a write on from `g` to `h`. Expect the
/// bottom landed and the upper refused delete against modify at `g`, `g` holding the other work's append and `h` still
/// a link: the upper's write was declared over bytes the head no longer holds, and is never placed over them.
#[test]
fn an_upper_s_write_handed_on_meets_a_change_to_its_node_and_conflicts() -> Result<(), String> {
  let mut green = green(true);
  let other = Work::default().extend("g", 4, b"<+o>\n").increment(9, 0);
  let landed_other = green.submit(&other);
  check(
    matches!(landed_other, Outcome::Accepted { version: 1 }),
    || format!("the other work was answered {landed_other:?}"),
  )?;
  let bottom = Work::default().extend("f", 4, b"<+b>\n").increment(1, 0);
  let upper = Work::default()
    .extend("h", 4, b"<+w>\n")
    .unlink("g")
    .upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  conflicts_at(landed.get(1), ("g", MergeConflictClass::DeleteModify))?;
  let (home, link) = (text(&green, "g"), text(&green, "h"));
  check(
    home.as_deref() == Some(&b"<g>\n<+o>\n"[..]) && link.is_none(),
    || format!("g holds {home:?} and h {link:?}"),
  )
}

/// A-670 in a stack: an upper removes a file another work linked after the stack's base, a link its base, its lower's
/// post-state, never held. Do: on a green holding `f` and `g`, land a work linking `k` to `f` as version 1, then a stack
/// based at 0 whose upper removes `f`. Expect the bottom landed and the upper refused delete against modify at `f`:
/// had it written to `f` first, the write went with the node in its own volume while `k` keeps the node at the head.
/// Before, the removal was accepted and `k` took the head's bytes.
#[test]
fn an_upper_removing_a_file_another_work_linked_under_the_stack_is_refused() -> Result<(), String> {
  let mut green = green(false);
  let link = linked(Work::default(), "k", "f").increment(9, 0);
  let linked_first = green.submit(&link);
  check(
    matches!(linked_first, Outcome::Accepted { version: 1 }),
    || format!("the link was answered {linked_first:?}"),
  )?;
  let bottom = Work::default().extend("g", 4, b"<+b>\n").increment(1, 0);
  let upper = Work::default().unlink("f").upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  conflicts_at(landed.get(1), ("f", MergeConflictClass::DeleteModify))?;
  check(text(&green, "f").as_deref() == Some(&b"<f>\n"[..]), || {
    "f was removed".to_owned()
  })
}

/// A-670 in a stack, kept: a link the stack's own bottom made is in the upper's base. Do: on a green holding `f` and
/// `g`, land a stack based at 0 whose bottom links `k` to `f` and whose upper removes `f`. Expect both landed, `k` the
/// node's home holding `f`'s bytes and nothing at `f`.
#[test]
fn an_upper_removing_a_file_its_lower_linked_lands() -> Result<(), String> {
  let mut green = green(false);
  let bottom = linked(Work::default(), "k", "f").increment(1, 0);
  let upper = Work::default().unlink("f").upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  check(versions(&landed) == [Some(1), Some(2)], || {
    format!("landed {landed:?}")
  })?;
  let (kept, gone) = (text(&green, "k"), text(&green, "f"));
  check(
    kept.as_deref() == Some(&b"<f>\n"[..]) && gone.is_none(),
    || format!("k holds {kept:?} and f {gone:?}"),
  )
}
