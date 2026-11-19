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
use std::collections::BTreeSet;

use super::judge::Windows;
use super::{Landed, Landing, RestateRefused, Restated, TREE_CONTEXT, Upper, restated_identity};
use crate::engine::{Green, Increment, Resolved, apply, is_content, len_of, node_exclusive_paths};
use crate::range::Range;
use crate::special::SpecialNode;
use crate::verdict::MergeConflictClass;

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
    let head = self.head();
    let mut restated = Increment {
      id: restated_identity(TREE_CONTEXT, &work.id, head),
      base: head,
      doc: work.doc.clone(),
      post_state: work.post_state.clone(),
      evidence: work.evidence.clone(),
    };
    let resolved = Green::resolve(&restated);
    if let Some(path) = resolved.copies.keys().next() {
      return Restated::Refused(RestateRefused::Copy { path: path.clone() });
    }
    let mut windows = Windows::default();
    let identical = self.tree_content(tree, (&restated, &resolved), &mut windows);
    self.tree_namespace(tree, &resolved, &mut windows);
    self.tree_node_exclusive(tree, &restated, &mut windows);
    if let Err(windows) = windows.or(()) {
      return Restated::Conflict { windows };
    }
    let names = restated.doc.paths.clone();
    restated.doc.ops.retain(|op| {
      let path = names.path(op.path).unwrap_or_default();
      !(is_content(op.kind) && identical.contains(path))
    });
    Restated::Over(restated)
  }

  /// The head's node at `path`, in a tree's terms: the root always a directory, as the green holds it (A-618).
  fn head_node(&self, path: &str) -> TreeNode<'_> {
    if let Some(bytes) = self.content(path) {
      return TreeNode::File {
        bytes: Cow::Borrowed(bytes),
        mode: self.mode(path),
      };
    }
    if let Some(node) = self.special(path) {
      return TreeNode::Special(node);
    }
    if let Some(target) = self.symlink(path) {
      return TreeNode::Symlink {
        target: Cow::Borrowed(target),
      };
    }
    if let Some(target) = self.hardlink(path) {
      return TreeNode::Hardlink {
        target: Cow::Borrowed(target),
      };
    }
    if path.is_empty() || self.is_dir(path) {
      return TreeNode::Directory {
        mode: self.mode(path),
      };
    }
    TreeNode::Absent
  }

  /// The work's content against the tree (module docs): a file the head holds as the tree held it keeps its operations;
  /// one the head holds otherwise is a conflict over the whole file, unless the work makes the head's bytes, whose
  /// paths are answered for their operations to be left out.
  fn tree_content<T: TreeBase + ?Sized>(
    &self,
    tree: &T,
    (work, resolved): (&Increment, &Resolved),
    windows: &mut Windows,
  ) -> BTreeSet<String> {
    let mut identical = BTreeSet::new();
    for (path, ops) in &resolved.content {
      // A file the work makes holds its own bytes.
      if resolved.creates.contains(path) {
        continue;
      }
      let frame = resolved
        .renames
        .get(path)
        .map_or(path.as_str(), String::as_str);
      let TreeNode::File { bytes: base, .. } = tree.node(frame) else {
        windows.whole(path, MergeConflictClass::DeleteModify);
        continue;
      };
      let now = self.content(frame);
      if now == Some(base.as_ref()) {
        continue;
      }
      if frame == path && now == Some(apply(&base, ops, &work.post_state).as_slice()) {
        identical.insert(path.clone());
        continue;
      }
      windows.range(path, Range::new(0, len_of(&base)));
    }
    identical
  }

  /// The work's namespace against the tree: each operation judged as the verdict judges one against a change after its
  /// base (module docs), the change read as a difference between the tree's and the head's whole values.
  fn tree_namespace<T: TreeBase + ?Sized>(
    &self,
    tree: &T,
    resolved: &Resolved,
    windows: &mut Windows,
  ) {
    let differs = |path: &str| !path.is_empty() && tree.node(path) != self.head_node(path);
    for (path, source) in &resolved.renames {
      if differs(source.as_str()) || differs(path.as_str()) {
        windows.whole(path, MergeConflictClass::RenameRename);
      }
    }
    for path in resolved.unlinks.iter().chain(&resolved.rmdirs) {
      if self.head_node(path) != TreeNode::Absent && differs(path.as_str()) {
        windows.whole(path, MergeConflictClass::DeleteModify);
      }
    }
    for (path, mode) in &resolved.modes {
      if let Some(class) = self.tree_mode(tree, path, (*mode, resolved.establishes(path))) {
        windows.whole(path, class);
      }
    }
    self.tree_xattrs(tree, resolved, windows);
    self.tree_links(tree, resolved, windows);
  }

  /// A mode against the tree: delete against modify where the node's kind changed, meta against meta where its mode
  /// changed to another than the work's (`Green::merge_setmode`).
  fn tree_mode<T: TreeBase + ?Sized>(
    &self,
    tree: &T,
    path: &str,
    (mode, established): (u32, bool),
  ) -> Option<MergeConflictClass> {
    let (base, now) = (tree.node(path), self.head_node(path));
    if !established && !path.is_empty() && !same_kind(&base, &now) {
      return Some(MergeConflictClass::DeleteModify);
    }
    let (was, is) = (mode_of(&base), mode_of(&now));
    (was != is && is != Some(mode)).then_some(MergeConflictClass::MetaMeta)
  }

  /// Attributes against the tree (`Green::merge_set_xattr`, `merge_remove_xattr`).
  fn tree_xattrs<T: TreeBase + ?Sized>(
    &self,
    tree: &T,
    resolved: &Resolved,
    windows: &mut Windows,
  ) {
    let kind_changed = |path: &str, established: bool| {
      !established && !path.is_empty() && !same_kind(&tree.node(path), &self.head_node(path))
    };
    for (path, name, value) in &resolved.xattr_sets {
      let (was, now) = (tree.xattr(path, name), self.xattr(path, name));
      if kind_changed(path.as_str(), resolved.establishes(path)) {
        windows.whole(path, MergeConflictClass::DeleteModify);
      } else if was.as_deref() != now && now != Some(value.as_slice()) {
        windows.whole(path, MergeConflictClass::MetaMeta);
      }
    }
    for (path, name) in &resolved.xattr_removes {
      let (was, now) = (tree.xattr(path, name), self.xattr(path, name));
      if kind_changed(path.as_str(), resolved.establishes(path)) {
        windows.whole(path, MergeConflictClass::DeleteModify);
      } else if now.is_some() && was.as_deref() != now {
        windows.whole(path, MergeConflictClass::MetaMeta);
      }
    }
  }

  /// Links and IPC nodes against the tree: one another work set otherwise is create against create, and a hard link to
  /// a node changed is delete against modify (`Green::merge_symlink`, `merge_special`, `merge_hardlink`).
  fn tree_links<T: TreeBase + ?Sized>(&self, tree: &T, resolved: &Resolved, windows: &mut Windows) {
    let differs = |path: &str| tree.node(path) != self.head_node(path);
    for (path, target) in &resolved.symlinks {
      if differs(path.as_str()) && self.symlink(path) != Some(target.as_str()) {
        windows.whole(path, MergeConflictClass::CreateCreate);
      }
    }
    for (path, node) in &resolved.specials {
      if differs(path.as_str()) && self.special(path) != Some(*node) {
        windows.whole(path, MergeConflictClass::CreateCreate);
      }
    }
    for (path, target) in &resolved.hardlinks {
      let (node, made) = resolved.link_node(target);
      if made.is_none() && differs(node) {
        windows.whole(path, MergeConflictClass::DeleteModify);
      } else if differs(path.as_str()) && self.hardlink(path) != Some(made.unwrap_or(node)) {
        windows.whole(path, MergeConflictClass::CreateCreate);
      }
    }
  }

  /// A node-exclusive operation's paths the head holds otherwise than the tree are a conflict (§4.17.4 item 3, A-542).
  fn tree_node_exclusive<T: TreeBase + ?Sized>(
    &self,
    tree: &T,
    work: &Increment,
    windows: &mut Windows,
  ) {
    for op in work.doc.ops.iter().filter(|op| op.is_node_exclusive()) {
      for (index, class) in node_exclusive_paths(op) {
        let path = work.doc.paths.path(index).unwrap_or_default();
        if tree.node(path) != self.head_node(path) {
          windows.whole(path, class);
        }
      }
    }
  }

  /// Lands `work`, composed against `tree`: restated over the head ([`Green::restate_over_tree`]), then judged and
  /// committed as any work is.
  pub fn land_over_tree<T: TreeBase + ?Sized>(&mut self, tree: &T, work: &Upper) -> Landed {
    match self.restate_over_tree(tree, work) {
      Restated::Over(increment) => self.land_one(Landing::Upper(increment)),
      Restated::Conflict { windows } => Landed::Conflict { windows },
      Restated::Refused(why) => Landed::Refused(why),
    }
  }
}

/// Whether two nodes are of one kind.
fn same_kind<'a>(a: &TreeNode<'a>, b: &TreeNode<'a>) -> bool {
  core::mem::discriminant(a) == core::mem::discriminant(b)
}

/// A node's mode, where it has one.
fn mode_of(node: &TreeNode<'_>) -> Option<u32> {
  match node {
    TreeNode::File { mode, .. } | TreeNode::Directory { mode } => *mode,
    _ => None,
  }
}
