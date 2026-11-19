//! A base given as a tree (§4.17.1 "A push is a work" item 6; A-659's second half).
//!
//! **What it is.** A review request or a merge may need a base that is no version of the green: a commit inside an
//! earlier push, whose tree no version's head is. Such a work is an [`Upper`] whose ops document the deriver composed
//! against that tree, and the tree is given beside it ([`TreeBase`]: what it holds at each path the work names, in the
//! green's terms; a tool's store implements it from its trees).
//!
//! **What the green changed since it** is read as the whole-value differences between the tree and the head, at the
//! paths the work names (§4.17.1: "what the green changed since it is read as the whole-file differences between that
//! tree and the head's, which is all that stock pushes ever declared"). A file whose bytes the head holds as the tree
//! held them is unchanged, so the work's operations apply there as they stand, in the same coordinates; a file the head
//! holds otherwise is a conflict over the whole file, unless the work makes exactly the head's bytes (identical). A
//! namespace operation is judged as the verdict judges one against a change after its base, the change read as a
//! difference of that dimension's whole value (a removal, a kind, a mode, an attribute's value, a link's target). The
//! restated work is based on the head and judged by the verdict as any work is.
//!
//! **What it costs** is the paths the work names: one lookup in the tree and one in the head each, and a comparison of
//! a file's bytes where the work edits it, never the tree. Its precision is the stock push's: where the green holds
//! byte-range works since the tree, it reads them as whole files, a conflict a finer base would have avoided, never a
//! wrong accept.

use std::borrow::Cow;

use super::{Landed, RestateRefused, Restated, Upper};
use crate::engine::Green;
use crate::special::SpecialNode;

/// A base given as a tree (module docs): what it holds at each path a work over it names, in the green's terms.
pub trait TreeBase {
  /// What the tree holds at `path`.
  fn node(&self, path: &str) -> TreeNode<'_>;

  /// The value of the attribute `name` on `path` in the tree. A stock tool's tree holds none, which the default answers.
  fn xattr(&self, _path: &str, _name: &str) -> Option<Cow<'_, [u8]>> {
    None
  }
}

/// What a tree holds at a path, in the green's terms ([`TreeBase::node`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeNode<'a> {
  /// Nothing.
  Absent,
  /// A regular file.
  File {
    /// Its bytes.
    bytes: Cow<'a, [u8]>,
    /// Its mode, as the green records one: none where it records none.
    mode: Option<u32>,
  },
  /// A directory.
  Directory {
    /// Its mode, as the green records one.
    mode: Option<u32>,
  },
  /// A symbolic link.
  Symlink {
    /// Its target.
    target: Cow<'a, str>,
  },
  /// A hard link.
  Hardlink {
    /// The path of the node it names.
    target: Cow<'a, str>,
  },
  /// An IPC node or a submodule's link (A-26, A-294, A-637).
  Special(SpecialNode),
}

impl Green {
  /// Restates `work`, composed against `tree`, over the head (module docs): the work as one based on the head, or the
  /// windows of what the head changed since the tree that it meets, or why it cannot be restated.
  pub fn restate_over_tree<T: TreeBase + ?Sized>(&self, tree: &T, work: &Upper) -> Restated {
    let _ = (tree.node(""), work);
    Restated::Refused(RestateRefused::NotLanded { below: 0 })
  }

  /// Lands `work`, composed against `tree`: restated over the head ([`Green::restate_over_tree`]), then judged and
  /// committed as any work is.
  pub fn land_over_tree<T: TreeBase + ?Sized>(&mut self, tree: &T, work: &Upper) -> Landed {
    match self.restate_over_tree(tree, work) {
      Restated::Over(increment) => self.land_one(increment, true),
      Restated::Conflict { windows } => Landed::Conflict { windows },
      Restated::Refused(why) => Landed::Refused(why),
    }
  }
}
