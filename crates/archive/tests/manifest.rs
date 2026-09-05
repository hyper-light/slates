//! Tests for the archive's manifest tree (§2.6 item 4; determinism, golden identity,
//! hostile-input). A tree round-trips through its canonical encoding; its Merkle identity is
//! independent of the order entries were added and changes when any node changes; a malformed
//! encoding is a typed refusal, never a panic.

use slates_archive::manifest::{Entry, Extent, ManifestError, Node, NodeMeta};

use proptest::prelude::*;

/// A small file node with one extent.
fn file(chunk: u8, len: u64) -> Node {
  Node::File(vec![Extent {
    offset: 0,
    len,
    chunk: [chunk; 32],
    chunk_offset: 0,
  }])
}

/// A sample tree: a directory with a file, a hole file, and a subdirectory.
fn sample() -> Node {
  Node::Directory(vec![
    Entry {
      name: "readme".to_owned(),
      meta: NodeMeta::default(),
      node: file(0x11, 20),
    },
    Entry {
      name: "sparse".to_owned(),
      meta: NodeMeta::default(),
      node: Node::File(vec![Extent {
        offset: 0,
        len: 4096,
        chunk: [0u8; 32],
        chunk_offset: 0,
      }]),
    },
    Entry {
      name: "src".to_owned(),
      meta: NodeMeta::default(),
      node: Node::Directory(vec![
        Entry {
          name: "lib.rs".to_owned(),
          meta: NodeMeta::default(),
          node: file(0x22, 100),
        },
        Entry {
          name: "main.rs".to_owned(),
          meta: NodeMeta::default(),
          node: file(0x33, 50),
        },
      ]),
    },
  ])
}

/// A tree round-trips through its canonical encoding.
#[test]
fn a_tree_round_trips() {
  let tree = sample();
  let bytes = tree.encode();
  let decoded = Node::decode(&bytes).expect("a well-formed tree decodes");
  assert_eq!(decoded, tree);
}

/// The encoding and the identity are deterministic and independent of the order entries were added
/// (a directory is canonicalized to sorted-by-name order).
#[test]
fn the_identity_is_independent_of_entry_order() {
  let forward = Node::Directory(vec![
    Entry {
      name: "a".to_owned(),
      meta: NodeMeta::default(),
      node: file(1, 1),
    },
    Entry {
      name: "b".to_owned(),
      meta: NodeMeta::default(),
      node: file(2, 2),
    },
    Entry {
      name: "c".to_owned(),
      meta: NodeMeta::default(),
      node: file(3, 3),
    },
  ]);
  let shuffled = Node::Directory(vec![
    Entry {
      name: "c".to_owned(),
      meta: NodeMeta::default(),
      node: file(3, 3),
    },
    Entry {
      name: "a".to_owned(),
      meta: NodeMeta::default(),
      node: file(1, 1),
    },
    Entry {
      name: "b".to_owned(),
      meta: NodeMeta::default(),
      node: file(2, 2),
    },
  ]);
  assert_eq!(forward.identity(), shuffled.identity());
  assert_eq!(forward.encode(), shuffled.encode());
}

/// The identity is a Merkle fingerprint: changing any node changes the root identity.
#[test]
fn changing_a_leaf_changes_the_root_identity() {
  let before = sample().identity();
  let mut changed = sample();
  if let Node::Directory(entries) = &mut changed
    && let Some(entry) = entries.iter_mut().find(|e| e.name == "readme")
  {
    entry.node = file(0x99, 21);
  }
  assert_ne!(
    changed.identity(),
    before,
    "a changed leaf changes the root"
  );
}

/// A file and a directory with the same-named entry have different identities (the kind is part of
/// the encoding).
#[test]
fn a_file_and_a_directory_differ() {
  let as_file = file(1, 1);
  let as_dir = Node::Directory(Vec::new());
  assert_ne!(as_file.identity(), as_dir.identity());
}

/// An empty or truncated encoding is refused, never a panic.
#[test]
fn a_truncated_tree_is_refused() {
  assert_eq!(Node::decode(&[]), Err(ManifestError::Truncated));
  let bytes = sample().encode();
  // Cut it short partway through.
  assert!(Node::decode(&bytes[..bytes.len() / 2]).is_err());
}

/// An unknown kind byte is refused, naming it.
#[test]
fn an_unknown_kind_is_refused() {
  assert_eq!(
    Node::decode(&[0xff]),
    Err(ManifestError::BadKind { found: 0xff })
  );
}

proptest! {
  /// T-7.4-style (hostile): arbitrary bytes decode to a typed error or a tree, never a panic.
  #[test]
  fn arbitrary_bytes_do_not_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
    let _ = Node::decode(&bytes);
    prop_assert!(true);
  }

  /// A generated tree round-trips and its identity is stable across two computations.
  #[test]
  fn generated_trees_round_trip(
    names in proptest::collection::vec("[a-c]{1,3}", 0..6),
    lens in proptest::collection::vec(0u64..8192, 0..6),
  ) {
    let mut entries = Vec::new();
    for (index, name) in names.iter().enumerate() {
      let len = lens.get(index).copied().unwrap_or(0);
      entries.push(Entry {
        name: name.clone(),
        meta: NodeMeta::default(),
        node: file(u8::try_from(index & 0xff).unwrap_or(0), len),
      });
    }
    let tree = Node::Directory(entries);
    let bytes = tree.encode();
    let decoded = Node::decode(&bytes).expect("round-trips");
    // Decoding yields the canonical (sorted, deduplicated-by-position) form; its identity and
    // re-encoding match the original tree's.
    prop_assert_eq!(decoded.identity(), tree.identity());
    prop_assert_eq!(decoded.encode(), tree.encode());
    prop_assert_eq!(tree.identity(), tree.identity());
  }
}

/// A sample metadata value with distinct, non-default fields.
fn meta(mode: u32) -> NodeMeta {
  NodeMeta {
    ino: 42,
    mode,
    mtime_ns: 1_700_000_000_000_000_000,
    ctime_ns: 1_700_000_000_500_000_000,
    size: 100,
    nlink: 1,
    xattr_flags: 0,
  }
}

/// A directory of one file entry carrying `meta`.
fn one_file_with_meta(meta: NodeMeta) -> Node {
  Node::Directory(vec![Entry {
    name: "f".to_owned(),
    meta,
    node: file(0x44, 100),
  }])
}

/// Per-node metadata round-trips through the canonical encoding.
#[test]
fn metadata_round_trips() {
  let tree = one_file_with_meta(meta(0o640));
  let decoded = Node::decode(&tree.encode()).expect("decodes");
  let Node::Directory(entries) = &decoded else {
    panic!("expected a directory");
  };
  assert_eq!(
    entries[0].meta,
    meta(0o640),
    "the metadata survives the round trip"
  );
}

/// A change to an entry's metadata changes the tree's Merkle identity, exactly as a content change
/// does — so metadata is covered by the archive's self-verification.
#[test]
fn metadata_changes_the_identity() {
  let a = one_file_with_meta(meta(0o644));
  let b = one_file_with_meta(meta(0o600));
  assert_ne!(
    a.identity(),
    b.identity(),
    "a mode change changes the root identity"
  );
  // Identical metadata gives identical identity (determinism).
  let c = one_file_with_meta(meta(0o644));
  assert_eq!(
    a.identity(),
    c.identity(),
    "identical trees have one identity"
  );
}
