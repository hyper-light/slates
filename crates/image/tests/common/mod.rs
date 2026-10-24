//! What the image tests share: an image held in memory as both the writer's sink and the reader's source, file
//! contents kept by node, a writer that runs a builder to the end, and the canonical form two trees are compared in.
#![allow(dead_code)]
// Test harness code: a failed unwrap or assertion inside a sink or source is a failed test.
#![allow(clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_image::{
  Builder, ContentSource, Identity, ImageSink, ImageSource, Kind, Node, NodeId, Progress,
  Timestamp, Tree, Written,
};

/// An image held in memory.
#[derive(Clone, Debug, Default)]
pub(crate) struct Memory {
  pub(crate) bytes: Vec<u8>,
}

impl ImageSink<Infallible> for Memory {
  fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Infallible> {
    let start = usize::try_from(offset).unwrap();
    let end = start + bytes.len();
    assert!(
      end <= self.bytes.len(),
      "a write past the image's length: {start}..{end} of {}",
      self.bytes.len()
    );
    self.bytes[start..end].copy_from_slice(bytes);
    Ok(())
  }
}

impl ImageSource<Infallible> for Memory {
  fn image_len(&self) -> u64 {
    u64::try_from(self.bytes.len()).unwrap()
  }

  fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Infallible> {
    let start = usize::try_from(offset).unwrap();
    buf.copy_from_slice(&self.bytes[start..start + buf.len()]);
    Ok(())
  }
}

/// File contents by node index; the source counts its reads so a test can see it was asked.
#[derive(Clone, Debug, Default)]
pub(crate) struct Contents {
  pub(crate) files: BTreeMap<usize, Vec<u8>>,
}

impl ContentSource<Infallible> for Contents {
  fn read(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, Infallible> {
    let bytes = self.files.get(&node.index()).map_or(&[][..], Vec::as_slice);
    let start = usize::try_from(offset).unwrap().min(bytes.len());
    let take = buf.len().min(bytes.len() - start);
    buf[..take].copy_from_slice(&bytes[start..start + take]);
    Ok(take)
  }
}

/// The identity every test image carries, so two writes of one tree compare byte for byte.
/// Its time is in the past (2023-11-14), so no e2fsck run sees a superblock written in the future.
pub(crate) const IDENTITY: Identity = Identity {
  uuid: [0x5a; 16],
  created: Timestamp {
    secs: 1_700_000_000,
    nanos: 0,
  },
};

/// Writes `tree` with `contents` into an image of `capacity` bytes, in steps of `budget` bytes; returns the image,
/// what was written, and the steps taken.
pub(crate) fn write(
  tree: &Tree,
  contents: &Contents,
  capacity: u64,
  budget: u64,
) -> (Memory, Written, usize) {
  let mut builder = Builder::new(tree, capacity, IDENTITY).unwrap();
  let mut image = Memory {
    bytes: vec![0; usize::try_from(builder.image_bytes()).unwrap()],
  };
  let mut source = contents.clone();
  let mut steps = 0;
  loop {
    steps += 1;
    match builder.step(&mut source, &mut image, budget).unwrap() {
      Progress::More => {}
      Progress::Done(written) => return (image, written, steps),
    }
  }
}

/// A node with `kind` and fixed attributes.
pub(crate) fn node(kind: Kind) -> Node {
  Node {
    kind,
    permissions: 0o755,
    uid: 1000,
    gid: 1000,
    atime: Timestamp {
      secs: 1_700_000_000,
      nanos: 1,
    },
    mtime: Timestamp {
      secs: 1_700_000_001,
      nanos: 2,
    },
    ctime: Timestamp {
      secs: 1_700_000_002,
      nanos: 3,
    },
  }
}

/// One entry of a tree's canonical form: its path, the path that first named its node when another did (a hard
/// link), the node, and its bytes for a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Canonical {
  pub(crate) path: Vec<u8>,
  pub(crate) linked_to: Option<Vec<u8>>,
  pub(crate) node: Node,
  pub(crate) bytes: Vec<u8>,
}

/// `tree`'s canonical form: its entries in depth-first name order from the root (the root first, as the empty path),
/// each node's bytes read by `content`, and hard links named by the first path that reached their node. Two trees
/// with one canonical form hold the same thing, whatever order their nodes were made in.
pub(crate) fn canonical(
  tree: &Tree,
  content: &mut dyn FnMut(NodeId, u64) -> Vec<u8>,
) -> Vec<Canonical> {
  let mut out = Vec::new();
  let mut first_path: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
  let root = tree.node(NodeId::ROOT).unwrap().clone();
  out.push(Canonical {
    path: Vec::new(),
    linked_to: None,
    node: root,
    bytes: Vec::new(),
  });
  let mut stack: Vec<(Vec<u8>, NodeId)> = vec![(Vec::new(), NodeId::ROOT)];
  while let Some((prefix, directory)) = stack.pop() {
    // Children in reverse so the depth-first walk visits them in name order.
    for (name, child) in tree.children(directory).iter().rev() {
      let mut path = prefix.clone();
      path.push(b'/');
      path.extend_from_slice(name);
      let node = tree.node(*child).unwrap().clone();
      let linked_to = first_path.get(&child.index()).cloned();
      if linked_to.is_none() {
        first_path.insert(child.index(), path.clone());
      }
      let bytes = match node.kind {
        Kind::File { size } if linked_to.is_none() => content(*child, size),
        _ => Vec::new(),
      };
      if node.kind == Kind::Directory {
        stack.push((path.clone(), *child));
      }
      out.push(Canonical {
        path,
        linked_to,
        node,
        bytes,
      });
    }
  }
  out.sort_by(|left, right| left.path.cmp(&right.path));
  // The first path of a hard-linked node depends on the walk's order; name each link group by its least path.
  normalize_links(&mut out);
  out
}

/// Renames every hard link's target to its group's least path, and moves the bytes there.
fn normalize_links(entries: &mut [Canonical]) {
  let mut groups: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
  for entry in entries.iter() {
    let key = entry
      .linked_to
      .clone()
      .unwrap_or_else(|| entry.path.clone());
    let least = groups.entry(key).or_insert_with(|| entry.path.clone());
    if entry.path < *least {
      *least = entry.path.clone();
    }
  }
  let mut bytes_by_group: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
  for entry in entries.iter() {
    if entry.linked_to.is_none() {
      let least = groups
        .get(&entry.path)
        .cloned()
        .unwrap_or_else(|| entry.path.clone());
      bytes_by_group.insert(least, entry.bytes.clone());
    }
  }
  for entry in entries.iter_mut() {
    let key = entry
      .linked_to
      .clone()
      .unwrap_or_else(|| entry.path.clone());
    let least = groups
      .get(&key)
      .cloned()
      .unwrap_or_else(|| entry.path.clone());
    if least == entry.path {
      entry.linked_to = None;
      entry.bytes = bytes_by_group.get(&least).cloned().unwrap_or_default();
    } else {
      entry.linked_to = Some(least);
      entry.bytes = Vec::new();
    }
  }
}

/// A small fixed tree touching every kind: nested directories, files, a fast and a slow symlink, a hard link, a device,
/// a FIFO and a socket.
pub(crate) fn sample() -> (Tree, Contents) {
  use slates_image::TreeBuilder;
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let mut contents = Contents::default();
  let docs = builder.add_node(node(Kind::Directory));
  builder.add_entry(NodeId::ROOT, b"docs".to_vec(), docs);
  let readme = builder.add_node(node(Kind::File { size: 5 }));
  contents.files.insert(readme.index(), b"hello".to_vec());
  builder.add_entry(NodeId::ROOT, b"README".to_vec(), readme);
  builder.add_entry(docs, b"readme-link".to_vec(), readme);
  let big = builder.add_node(node(Kind::File { size: 70_000 }));
  contents.files.insert(
    big.index(),
    (0..70_000u32)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect(),
  );
  builder.add_entry(docs, b"big.bin".to_vec(), big);
  let fast = builder.add_node(node(Kind::Symlink {
    target: b"../README".to_vec(),
  }));
  builder.add_entry(docs, b"fast".to_vec(), fast);
  let slow = builder.add_node(node(Kind::Symlink {
    target: vec![b'x'; 100],
  }));
  builder.add_entry(docs, b"slow".to_vec(), slow);
  let device = builder.add_node(node(Kind::CharDevice {
    rdev: slates_image::layout::new_encode(1, 3).unwrap(),
  }));
  builder.add_entry(NodeId::ROOT, b"null".to_vec(), device);
  let fifo = builder.add_node(node(Kind::Fifo));
  builder.add_entry(docs, b"pipe".to_vec(), fifo);
  let socket = builder.add_node(node(Kind::Socket));
  builder.add_entry(docs, b"socket".to_vec(), socket);
  (builder.build().unwrap(), contents)
}
