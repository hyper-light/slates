//! The tree an image holds, as the writer is given it and the reader returns it: nodes with their kind and
//! attributes, and the entries that name them. A node is named by one entry or several (hard links), except that a
//! directory is named by exactly one, and the root by none. Content is not in the tree: the writer asks a
//! [`crate::write::ContentSource`] for a file's bytes, and the reader reads them from the image on request.
//!
//! The builder validates everything the format cannot represent before a byte is written, each a typed refusal:
//! a bad name, a repeated name, a directory named twice or not at all (which is what a cycle comes to), a node no
//! entry reaches, too many links. Each directory's entries are kept in name order, so one tree always writes the
//! same image (the determinism gate).

use crate::error::ImageRefusal;
use crate::format::{LINK_MAX, NAME_MAX};

/// A time: seconds since the Unix epoch and nanoseconds within the second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
  /// Seconds since 1970-01-01T00:00:00Z.
  pub secs: i64,
  /// Nanoseconds, below one second.
  pub nanos: u32,
}

/// What a node is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
  /// A directory.
  Directory,
  /// A regular file of `size` bytes.
  File {
    /// Its size in bytes.
    size: u64,
  },
  /// A symbolic link to `target`.
  Symlink {
    /// The target, as bytes.
    target: Vec<u8>,
  },
  /// A character device.
  CharDevice {
    /// Its device number, in the kernel's new encoding (12 bits of major, 20 of minor).
    rdev: u32,
  },
  /// A block device.
  BlockDevice {
    /// Its device number, in the kernel's new encoding.
    rdev: u32,
  },
  /// A named pipe.
  Fifo,
  /// A socket.
  Socket,
}

/// One node of a tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Node {
  /// What it is.
  pub kind: Kind,
  /// Its permission bits, with set-user-id, set-group-id and sticky ([`crate::format::PERMISSION_BITS`]).
  pub permissions: u16,
  /// Its owner.
  pub uid: u32,
  /// Its group.
  pub gid: u32,
  /// Its last access.
  pub atime: Timestamp,
  /// Its last content change.
  pub mtime: Timestamp,
  /// Its last attribute change.
  pub ctime: Timestamp,
}

/// A node's place in its tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(usize);

impl NodeId {
  /// The root's id: the first node a tree holds.
  pub const ROOT: NodeId = NodeId(0);

  /// The node's index in its tree.
  pub fn index(self) -> usize {
    self.0
  }
}

/// A validated tree: every node reachable from the root, every directory named once, every directory's entries in
/// name order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
  nodes: Vec<Node>,
  /// For each node, its entries (name, child) in name order; empty for a node that is not a directory.
  children: Vec<Vec<(Vec<u8>, NodeId)>>,
  /// For each node, the entries that name it (its link count, before a directory's own).
  named: Vec<u64>,
  /// For each directory, its parent; the root is its own parent.
  parents: Vec<NodeId>,
}

impl Tree {
  /// The nodes, the root first.
  pub fn nodes(&self) -> &[Node] {
    &self.nodes
  }

  /// The node `id` names, if the tree holds it.
  pub fn node(&self, id: NodeId) -> Option<&Node> {
    self.nodes.get(id.0)
  }

  /// The entries of directory `id`, in name order (none for a node that is not a directory).
  pub fn children(&self, id: NodeId) -> &[(Vec<u8>, NodeId)] {
    self.children.get(id.0).map_or(&[], Vec::as_slice)
  }

  /// The entries that name node `id`.
  pub fn named(&self, id: NodeId) -> u64 {
    self.named.get(id.0).copied().unwrap_or(0)
  }

  /// The parent of directory `id` (the root's is itself).
  pub fn parent(&self, id: NodeId) -> NodeId {
    self.parents.get(id.0).copied().unwrap_or(NodeId::ROOT)
  }

  /// The id of node `index`, if the tree holds it.
  pub fn id(&self, index: usize) -> Option<NodeId> {
    (index < self.nodes.len()).then_some(NodeId(index))
  }

  /// The node ids, the root first.
  pub fn ids(&self) -> impl Iterator<Item = NodeId> {
    (0..self.nodes.len()).map(NodeId)
  }

  /// The links node `id` carries in the image: the entries naming it, and for a directory its `.` and each child
  /// directory's `..` as well (the root's `..` is its own).
  pub fn links(&self, id: NodeId) -> u64 {
    match self.node(id).map(|node| &node.kind) {
      Some(Kind::Directory) => {
        let subdirectories = self
          .children(id)
          .iter()
          .filter(|(_, child)| {
            self
              .node(*child)
              .is_some_and(|node| node.kind == Kind::Directory)
          })
          .count();
        // The root has no entry naming it, but its `..` names itself: two links before its subdirectories, as any
        // directory has (its entry and its `.`).
        let named = if id == NodeId::ROOT {
          1
        } else {
          self.named(id)
        };
        named
          .saturating_add(1)
          .saturating_add(u64::try_from(subdirectories).unwrap_or(u64::MAX))
      }
      Some(_) => self.named(id),
      None => 0,
    }
  }
}

/// Builds a [`Tree`]: nodes added, entries added, then validated whole by [`TreeBuilder::build`].
#[derive(Debug, Default)]
pub struct TreeBuilder {
  nodes: Vec<Node>,
  entries: Vec<(NodeId, Vec<u8>, NodeId)>,
}

impl TreeBuilder {
  /// A builder whose root is `root` (which must be a directory, checked at [`TreeBuilder::build`]).
  pub fn new(root: Node) -> TreeBuilder {
    TreeBuilder {
      nodes: vec![root],
      entries: Vec::new(),
    }
  }

  /// Adds `node` and returns its id; it is part of the tree once an entry names it.
  pub fn add_node(&mut self, node: Node) -> NodeId {
    self.nodes.push(node);
    NodeId(self.nodes.len().saturating_sub(1))
  }

  /// Names `child` `name` in directory `parent`. Checked whole at [`TreeBuilder::build`].
  pub fn add_entry(&mut self, parent: NodeId, name: Vec<u8>, child: NodeId) {
    self.entries.push((parent, name, child));
  }

  /// The tree, or the first reason it cannot be an image.
  pub fn build(self) -> Result<Tree, ImageRefusal> {
    let TreeBuilder { nodes, entries } = self;
    if !matches!(nodes.first().map(|node| &node.kind), Some(Kind::Directory)) {
      return Err(ImageRefusal::RootNotDirectory);
    }
    for node in &nodes {
      check_node(node)?;
    }
    let count = nodes.len();
    let mut children: Vec<Vec<(Vec<u8>, NodeId)>> = vec![Vec::new(); count];
    let mut named = vec![0u64; count];
    let mut parents = vec![NodeId::ROOT; count];
    for (parent, name, child) in entries {
      check_name(&name)?;
      let parent_node = nodes
        .get(parent.0)
        .ok_or(ImageRefusal::NodeUnknown { node: parent.0 })?;
      if parent_node.kind != Kind::Directory {
        return Err(ImageRefusal::ParentNotDirectory);
      }
      let child_node = nodes
        .get(child.0)
        .ok_or(ImageRefusal::NodeUnknown { node: child.0 })?;
      let count_of = named
        .get_mut(child.0)
        .ok_or(ImageRefusal::NodeUnknown { node: child.0 })?;
      *count_of = count_of.saturating_add(1);
      if child_node.kind == Kind::Directory {
        if child == NodeId::ROOT || *count_of > 1 {
          return Err(ImageRefusal::DirectoryLinkedTwice {
            node: u64::try_from(child.0).unwrap_or(u64::MAX),
          });
        }
        if let Some(slot) = parents.get_mut(child.0) {
          *slot = parent;
        }
      }
      children
        .get_mut(parent.0)
        .ok_or(ImageRefusal::NodeUnknown { node: parent.0 })?
        .push((name, child));
    }
    for entries in &mut children {
      entries.sort_by(|left, right| left.0.cmp(&right.0));
      if entries.windows(2).any(|pair| match pair {
        [left, right] => left.0 == right.0,
        _ => false,
      }) {
        return Err(ImageRefusal::NameRepeated);
      }
    }
    let tree = Tree {
      nodes,
      children,
      named,
      parents,
    };
    check_reachable(&tree)?;
    for id in tree.ids() {
      let links = tree.links(id);
      if links > LINK_MAX {
        return Err(ImageRefusal::TooManyLinks { links });
      }
    }
    Ok(tree)
  }
}

/// Every node must be reachable from the root. With every directory named once, an unreachable directory is the mark
/// of a cycle (a directory inside itself is named once, by itself, and so never by the root's descendants).
fn check_reachable(tree: &Tree) -> Result<(), ImageRefusal> {
  let mut reached = vec![false; tree.nodes.len()];
  let mut stack = vec![NodeId::ROOT];
  if let Some(root) = reached.first_mut() {
    *root = true;
  }
  while let Some(directory) = stack.pop() {
    for (_, child) in tree.children(directory) {
      let Some(seen) = reached.get_mut(child.0) else {
        return Err(ImageRefusal::NodeUnknown { node: child.0 });
      };
      if !*seen {
        *seen = true;
        stack.push(*child);
      }
    }
  }
  match reached.iter().position(|seen| !seen) {
    Some(node) => {
      let is_directory = tree
        .node(NodeId(node))
        .is_some_and(|node| node.kind == Kind::Directory);
      if is_directory && tree.named(NodeId(node)) > 0 {
        Err(ImageRefusal::DirectoryLinkedTwice {
          node: u64::try_from(node).unwrap_or(u64::MAX),
        })
      } else {
        Err(ImageRefusal::Unreachable { node })
      }
    }
    None => Ok(()),
  }
}

/// A node the format can hold: a symlink target that fits one block and is not empty, a file no larger than an inode
/// maps.
fn check_node(node: &Node) -> Result<(), ImageRefusal> {
  match &node.kind {
    Kind::Symlink { target } => {
      if target.is_empty()
        || target.len() >= usize::try_from(crate::layout::BLOCK_BYTES).unwrap_or(usize::MAX)
      {
        return Err(ImageRefusal::SymlinkInvalid { len: target.len() });
      }
    }
    Kind::File { size } => {
      crate::layout::Map::new(size.div_ceil(crate::layout::BLOCK_BYTES))
        .map_err(|_| ImageRefusal::FileTooLarge { size: *size })?;
    }
    Kind::Directory
    | Kind::CharDevice { .. }
    | Kind::BlockDevice { .. }
    | Kind::Fifo
    | Kind::Socket => {}
  }
  for time in [node.atime, node.mtime, node.ctime] {
    crate::layout::encode_time(time)?;
  }
  Ok(())
}

/// A name a directory entry can hold: 1 to 255 bytes, not `.` or `..`, with no `/` and no NUL.
pub(crate) fn check_name(name: &[u8]) -> Result<(), ImageRefusal> {
  let invalid = name.is_empty()
    || name.len() > NAME_MAX
    || name == b"."
    || name == b".."
    || name.iter().any(|byte| *byte == b'/' || *byte == 0);
  if invalid {
    return Err(ImageRefusal::NameInvalid { len: name.len() });
  }
  Ok(())
}
