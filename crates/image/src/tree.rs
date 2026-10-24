//! The tree an image holds, as the writer is given it and the reader returns it: nodes with their kind and
//! attributes, and the entries that name them. A node is named by one entry or several (hard links), except that a
//! directory is named by exactly one, and the root by none. Content is not in the tree: the writer asks a
//! [`crate::write::ImageIo`] for a file's bytes, and the reader reads them from the image on request.
//!
//! Every tree is checked in one place, [`TreeGrower`], which grows a tree a directory at a time and refuses, by kind,
//! everything the format cannot represent: a bad name, a repeated name, a directory named twice or not at all (which
//! is what a cycle comes to), a node no entry reaches, too many links. It checks each entry as it comes, so a caller
//! walking a large tree in bounded steps (the daemon reading a snapshot, or a guest's image) never pays for one
//! whole-tree check at the end (§4.3, bounded work). [`TreeBuilder`] takes a whole tree at once and grows it from the
//! root. Each directory's entries are kept in name order, so one tree always writes the same image (the determinism
//! gate).

use std::collections::BTreeMap;

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

/// Where a node stands in a [`TreeGrower`]: a directory before it is named, named and waiting to be filled, being
/// filled, or filled; or a node that is not a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fill {
  /// Not a directory: it holds no entries.
  Leaf,
  /// A directory no entry names yet.
  Unnamed,
  /// A directory named (or the root), not yet filled.
  Awaiting,
  /// The directory being filled.
  Open,
  /// A directory filled.
  Filled,
}

/// Grows a [`Tree`] a directory at a time, checking each node and entry as it comes: the one place a tree is checked.
///
/// The order is the walk's: add nodes as they are met ([`TreeGrower::add_node`]), then fill each directory once it is
/// named — [`TreeGrower::open`] it, add its entries ([`TreeGrower::add_entry`]), [`TreeGrower::close`] it — and
/// [`TreeGrower::finish`]. Every check is made when its entry comes: the name, the repeat (each open directory's
/// entries are kept by name), the directory named twice, the links. A directory can be opened only once named, so
/// every directory filled hangs from the root by one chain of entries; [`TreeGrower::finish`] then needs only two
/// counts, the nodes no entry named and the directories named but not filled, to know the tree is whole. Each step
/// costs at most its directory's entries (`close` moves them into the tree in name order), never the whole tree.
#[derive(Debug)]
pub struct TreeGrower {
  nodes: Vec<Node>,
  children: Vec<Vec<(Vec<u8>, NodeId)>>,
  named: Vec<u64>,
  parents: Vec<NodeId>,
  fill: Vec<Fill>,
  /// For each directory, its subdirectories so far: its links past its own entry and its `.`.
  subdirectories: Vec<u64>,
  /// The directory being filled, and its entries so far by name.
  open: Option<(NodeId, BTreeMap<Vec<u8>, NodeId>)>,
  /// Nodes added that no entry names yet.
  unnamed: u64,
  /// Directories named, or the root, not yet filled.
  unfilled: u64,
}

impl TreeGrower {
  /// A tree whose root is `root`, which must be a directory the format can hold.
  pub fn new(root: Node) -> Result<TreeGrower, ImageRefusal> {
    if root.kind != Kind::Directory {
      return Err(ImageRefusal::RootNotDirectory);
    }
    check_node(&root)?;
    Ok(TreeGrower {
      nodes: vec![root],
      children: vec![Vec::new()],
      named: vec![0],
      parents: vec![NodeId::ROOT],
      fill: vec![Fill::Awaiting],
      subdirectories: vec![0],
      open: None,
      unnamed: 0,
      unfilled: 1,
    })
  }

  /// Adds `node` and returns its id; an entry must name it before [`TreeGrower::finish`].
  pub fn add_node(&mut self, node: Node) -> Result<NodeId, ImageRefusal> {
    check_node(&node)?;
    let fill = if node.kind == Kind::Directory {
      Fill::Unnamed
    } else {
      Fill::Leaf
    };
    self.nodes.push(node);
    self.children.push(Vec::new());
    self.named.push(0);
    self.parents.push(NodeId::ROOT);
    self.fill.push(fill);
    self.subdirectories.push(0);
    self.unnamed = self.unnamed.saturating_add(1);
    Ok(NodeId(self.nodes.len().saturating_sub(1)))
  }

  /// The node `id` names, if it was added.
  pub fn node(&self, id: NodeId) -> Option<&Node> {
    self.nodes.get(id.0)
  }

  /// The nodes added, the root among them.
  pub fn node_count(&self) -> usize {
    self.nodes.len()
  }

  /// Whether directory `id` is named, or is the root, and waits to be filled.
  pub fn awaits_filling(&self, id: NodeId) -> bool {
    self.fill.get(id.0) == Some(&Fill::Awaiting)
  }

  /// Opens directory `directory` for its entries. It must be named (or be the root) and not yet filled, and no other
  /// directory may be open.
  pub fn open(&mut self, directory: NodeId) -> Result<(), ImageRefusal> {
    if self.open.is_some() {
      return Err(ImageRefusal::GrowthOutOfOrder {
        reason: "a directory opened while another is open",
      });
    }
    let fill = self
      .fill
      .get_mut(directory.0)
      .ok_or(ImageRefusal::NodeUnknown { node: directory.0 })?;
    match *fill {
      Fill::Awaiting => {}
      Fill::Leaf => return Err(ImageRefusal::ParentNotDirectory),
      Fill::Unnamed => return Err(ImageRefusal::Unreachable { node: directory.0 }),
      Fill::Open | Fill::Filled => {
        return Err(ImageRefusal::GrowthOutOfOrder {
          reason: "a directory opened twice",
        });
      }
    }
    *fill = Fill::Open;
    self.open = Some((directory, BTreeMap::new()));
    Ok(())
  }

  /// Names `child` `name` in the open directory.
  pub fn add_entry(&mut self, name: Vec<u8>, child: NodeId) -> Result<(), ImageRefusal> {
    check_name(&name)?;
    let Some((directory, entries)) = self.open.as_mut() else {
      return Err(ImageRefusal::GrowthOutOfOrder {
        reason: "an entry with no directory open",
      });
    };
    let directory = *directory;
    let child_fill = *self
      .fill
      .get(child.0)
      .ok_or(ImageRefusal::NodeUnknown { node: child.0 })?;
    let twice = ImageRefusal::DirectoryLinkedTwice {
      node: u64::try_from(child.0).unwrap_or(u64::MAX),
    };
    if child == NodeId::ROOT {
      return Err(twice);
    }
    if entries.contains_key(&name) {
      return Err(ImageRefusal::NameRepeated);
    }
    let named = self
      .named
      .get_mut(child.0)
      .ok_or(ImageRefusal::NodeUnknown { node: child.0 })?;
    if child_fill == Fill::Leaf {
      let links = named.saturating_add(1);
      if links > LINK_MAX {
        return Err(ImageRefusal::TooManyLinks { links });
      }
    } else {
      if child_fill != Fill::Unnamed {
        return Err(twice);
      }
      // The open directory gains a link: its subdirectory's `..`. Its own entry and its `.` are its other two, the
      // root's `..` standing in for the entry it lacks.
      let subdirectories = self
        .subdirectories
        .get_mut(directory.0)
        .ok_or(ImageRefusal::NodeUnknown { node: directory.0 })?;
      let links = subdirectories.saturating_add(1).saturating_add(2);
      if links > LINK_MAX {
        return Err(ImageRefusal::TooManyLinks { links });
      }
      *subdirectories = subdirectories.saturating_add(1);
      if let Some(fill) = self.fill.get_mut(child.0) {
        *fill = Fill::Awaiting;
      }
      if let Some(parent) = self.parents.get_mut(child.0) {
        *parent = directory;
      }
      self.unfilled = self.unfilled.saturating_add(1);
    }
    if *named == 0 {
      self.unnamed = self.unnamed.saturating_sub(1);
    }
    *named = named.saturating_add(1);
    entries.insert(name, child);
    Ok(())
  }

  /// Closes the open directory: its entries move into the tree in name order.
  pub fn close(&mut self) -> Result<(), ImageRefusal> {
    let Some((directory, entries)) = self.open.take() else {
      return Err(ImageRefusal::GrowthOutOfOrder {
        reason: "a close with no directory open",
      });
    };
    let slot = self
      .children
      .get_mut(directory.0)
      .ok_or(ImageRefusal::NodeUnknown { node: directory.0 })?;
    *slot = entries.into_iter().collect();
    if let Some(fill) = self.fill.get_mut(directory.0) {
      *fill = Fill::Filled;
    }
    self.unfilled = self.unfilled.saturating_sub(1);
    Ok(())
  }

  /// The first node no entry names, if any: found only when one is missing, so a whole tree costs no scan.
  fn first_unnamed(&self) -> Option<usize> {
    if self.unnamed == 0 {
      return None;
    }
    self
      .named
      .iter()
      .skip(1)
      .position(|named| *named == 0)
      .map(|index| index.saturating_add(1))
  }

  /// The tree, once every node is named and every directory named is filled.
  pub fn finish(self) -> Result<Tree, ImageRefusal> {
    if self.open.is_some() {
      return Err(ImageRefusal::GrowthOutOfOrder {
        reason: "a directory still open",
      });
    }
    if let Some(node) = self.first_unnamed() {
      return Err(ImageRefusal::Unreachable { node });
    }
    if self.unfilled > 0 {
      return Err(ImageRefusal::GrowthOutOfOrder {
        reason: "a directory named and never filled",
      });
    }
    Ok(Tree {
      nodes: self.nodes,
      children: self.children,
      named: self.named,
      parents: self.parents,
    })
  }
}

/// Builds a [`Tree`] from its nodes and entries given whole, in any order: [`TreeBuilder::build`] grows it from the
/// root through a [`TreeGrower`].
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

  /// The tree, or the first reason it cannot be an image. Each directory is filled once an entry from the root names
  /// it; a directory left unreached though an entry names it lies on a cycle, and is refused as named twice.
  pub fn build(self) -> Result<Tree, ImageRefusal> {
    let TreeBuilder { nodes, entries } = self;
    let mut nodes = nodes.into_iter();
    let root = nodes.next().ok_or(ImageRefusal::RootNotDirectory)?;
    let mut grower = TreeGrower::new(root)?;
    for node in nodes {
      grower.add_node(node)?;
    }
    let mut by_parent: Vec<Vec<(Vec<u8>, NodeId)>> = vec![Vec::new(); grower.node_count()];
    for (parent, name, child) in entries {
      let parent_node = grower
        .node(parent)
        .ok_or(ImageRefusal::NodeUnknown { node: parent.0 })?;
      if parent_node.kind != Kind::Directory {
        return Err(ImageRefusal::ParentNotDirectory);
      }
      if grower.node(child).is_none() {
        return Err(ImageRefusal::NodeUnknown { node: child.0 });
      }
      by_parent
        .get_mut(parent.0)
        .ok_or(ImageRefusal::NodeUnknown { node: parent.0 })?
        .push((name, child));
    }
    let mut reached = vec![NodeId::ROOT];
    while let Some(directory) = reached.pop() {
      grower.open(directory)?;
      let entries = by_parent
        .get_mut(directory.0)
        .map(std::mem::take)
        .unwrap_or_default();
      for (name, child) in entries {
        grower.add_entry(name, child)?;
        if grower.awaits_filling(child) {
          reached.push(child);
        }
      }
      grower.close()?;
    }
    if let Some(node) = grower.first_unnamed() {
      // A directory the root never reached, though an entry names it, is named only from unreached directories: it
      // lies on a cycle.
      let directory = grower
        .node(NodeId(node))
        .is_some_and(|unreached| unreached.kind == Kind::Directory);
      let named_from_unreached = by_parent.iter().flatten().any(|(_, child)| child.0 == node);
      return Err(if directory && named_from_unreached {
        ImageRefusal::DirectoryLinkedTwice {
          node: u64::try_from(node).unwrap_or(u64::MAX),
        }
      } else {
        ImageRefusal::Unreachable { node }
      });
    }
    grower.finish()
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
