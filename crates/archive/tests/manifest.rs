//! Tests for the archive's manifest tree (§2.6 item 4; determinism, golden identity,
//! hostile-input). A tree round-trips through its canonical encoding; its Merkle identity is
//! independent of the order entries were added and changes when any node changes; a malformed
//! encoding is a typed refusal, never a panic.

use slates_archive::manifest::{Entry, Extent, ManifestError, Node, NodeMeta, Xattr};

/// The tree in the canonical form the decoder accepts (§2.6 D-17; AUD-29-14): every file entry's recorded
/// size is the length its extents tile, as the exporter records it.
fn canonical(node: Node) -> Node {
  match node {
    Node::File(extents) => Node::File(extents),
    Node::Directory(entries) => Node::Directory(
      entries
        .into_iter()
        .map(|entry| {
          let node = canonical(entry.node);
          let meta = match &node {
            Node::File(extents) => NodeMeta {
              // A fixture that does not tile records an impossible size, which the decoder refuses loudly.
              size: slates_archive::manifest::tiled_length(extents).unwrap_or(u64::MAX),
              ..entry.meta
            },
            Node::Directory(_) => entry.meta,
          };
          Entry {
            name: entry.name,
            meta,
            node,
          }
        })
        .collect(),
    ),
  }
}

use proptest::prelude::*;

/// A small file node with one extent.
fn file(chunk: u8, len: u64) -> Node {
  // An empty file tiles with no extent at all (a zero-length extent is not canonical).
  if len == 0 {
    return Node::File(Vec::new());
  }
  Node::File(vec![Extent {
    offset: 0,
    len,
    chunk: [chunk; 32],
    chunk_offset: 0,
  }])
}

/// A sample tree: a directory with a file, a hole file, and a subdirectory.
fn sample() -> Node {
  canonical(Node::Directory(vec![
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
  ]))
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
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// T-7.4-style (hostile): arbitrary bytes decode to a typed error or a tree, never a panic.
  #[test]
  fn arbitrary_bytes_do_not_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
    let _ = Node::decode(&bytes);
    prop_assert!(true);
  }

  /// A generated tree round-trips and its identity is stable across two computations.
  #[test]
  fn generated_trees_round_trip(
    names in proptest::collection::btree_set("[a-c]{1,3}", 0..6),
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
    let tree = canonical(Node::Directory(entries));
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
    uid: 501,
    gid: 20,
    ..NodeMeta::default()
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

/// Golden vector: the sample tree hashes to a pinned Merkle root, so any change to the node
/// encoding (including the per-node metadata) is caught across versions, not only within a run.
/// Regenerate deliberately on a format change (the archive minor version tracks it): minor 1 pinned
/// `1a1634ff…`; minor 2 (2026-09-15, the owner appended to every node's metadata) pinned `34bfced9…` over
/// a sample whose files recorded size zero. On 2026-09-30 the decoder began requiring the canonical form
/// (AUD-29-14: a file's recorded size is the length its extents tile), so the sample records its files'
/// real sizes (pinned `3a98564d…`). Minor 3 (2026-09-30, AUD-29-56: signed times, access and birth times,
/// the attributes in place of the flag) pins the value below; the attribute encoding has its own pin in
/// [`the_attribute_encoding_matches_its_golden_vector`].
#[test]
fn the_manifest_identity_matches_its_golden_vector() {
  let hex: String = sample()
    .identity()
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect();
  assert_eq!(
    hex, "d1895ea257bc94691ad4b6cecf0f556ea1f791b1ed5dfee6f4c9ef1a875845a8",
    "the manifest identity changed; regenerate the golden vector only for a deliberate format change"
  );
}

/// Format minor 2: an entry's owner round-trips through the canonical encoding and, like every other
/// metadata field, changes the tree's Merkle identity — a chown is a change the archive's
/// self-verification sees.
#[test]
fn an_entrys_owner_round_trips_and_changes_the_identity() {
  let owned = NodeMeta {
    uid: 1234,
    gid: 4321,
    ..meta(0o644)
  };
  let tree = one_file_with_meta(owned.clone());
  let decoded = Node::decode(&tree.encode()).expect("decodes");
  let Node::Directory(entries) = &decoded else {
    panic!("expected a directory");
  };
  assert_eq!(entries[0].meta, owned, "the owner survives the round trip");
  assert_ne!(
    tree.identity(),
    one_file_with_meta(meta(0o644)).identity(),
    "another owner is another identity"
  );
}

/// Format minor 2: the root directory's own metadata rides ahead of the tree and is covered by the
/// manifest identity the header pins — so a root chown changes the archive's identity while the tree's
/// Merkle root, which names no root, stays the same.
#[test]
fn the_roots_own_metadata_round_trips_and_is_in_the_manifest_identity() {
  use slates_archive::manifest::{decode_root_meta, encode_root_meta, manifest_identity};
  let root = NodeMeta {
    mode: 0o750,
    uid: 1000,
    gid: 2000,
    ..meta(0o750)
  };
  let tree = sample();
  let mut section = encode_root_meta(&root);
  section.extend_from_slice(&tree.encode());
  let (decoded_root, tree_bytes) = decode_root_meta(&section).expect("the root's metadata decodes");
  assert_eq!(decoded_root, root);
  assert_eq!(
    Node::decode(tree_bytes)
      .expect("the tree follows")
      .identity(),
    tree.identity()
  );
  let other_root = NodeMeta {
    uid: 1001,
    ..root.clone()
  };
  assert_ne!(
    manifest_identity(&root, &tree),
    manifest_identity(&other_root, &tree),
    "the root's owner is part of the manifest identity"
  );
  assert_eq!(
    decode_root_meta(&section[..8]),
    Err(ManifestError::Truncated),
    "a truncated root record is refused, typed"
  );
}

/// A directory of `names`, each an empty file, in the order given (the encoder sorts them).
fn empty_files(names: &[&str]) -> Node {
  Node::Directory(
    names
      .iter()
      .map(|name| Entry {
        name: (*name).to_owned(),
        meta: NodeMeta::default(),
        node: Node::File(Vec::new()),
      })
      .collect(),
  )
}

/// AUD-29-15: do: decode encodings with a byte after the tree, entry names that are `.`, `..`, empty,
/// hold a separator or a NUL, or run past `NAME_MAX`, a repeated name, and names out of order (the
/// encoder sorts, so the order is broken in the bytes); expect each refused typed, and a valid encoding
/// still decoding to the same identity.
#[test]
fn non_canonical_encodings_are_refused_typed() {
  let valid = empty_files(&["a", "b"]);
  let mut trailing = valid.encode();
  trailing.push(0);
  assert_eq!(Node::decode(&trailing), Err(ManifestError::TrailingBytes));
  let long = "n".repeat(slates_archive::manifest::NAME_MAX_BYTES + 1);
  for name in [".", "..", "", "a/b", "a\\b", "a\0b", long.as_str()] {
    assert_eq!(
      Node::decode(&empty_files(&[name]).encode()),
      Err(ManifestError::BadName),
      "{name:?}"
    );
  }
  assert_eq!(
    Node::decode(&empty_files(&["same", "same"]).encode()),
    Err(ManifestError::Unordered)
  );
  let mut unsorted = valid.encode();
  let first_name = [1u8, 0, 0, 0, b'a'];
  let at = unsorted
    .windows(first_name.len())
    .position(|window| window == first_name)
    .unwrap();
  unsorted[at + 4] = b'c';
  assert_eq!(Node::decode(&unsorted), Err(ManifestError::Unordered));
  assert_eq!(
    Node::decode(&valid.encode()).map(|node| node.identity()),
    Ok(valid.identity())
  );
}

/// AUD-29-13: do: decode a tree one directory deeper than `MAX_DEPTH`, and one exactly at it; expect the
/// deeper refused `TooDeep` and the bound itself decoding.
#[test]
fn a_tree_past_the_depth_bound_is_refused() {
  let nest = |depth: usize| {
    let mut node = Node::Directory(Vec::new());
    for _ in 0..depth {
      node = Node::Directory(vec![Entry {
        name: "d".to_owned(),
        meta: NodeMeta::default(),
        node,
      }]);
    }
    node
  };
  let bound = slates_archive::manifest::MAX_DEPTH;
  assert_eq!(
    Node::decode(&nest(bound + 1).encode()),
    Err(ManifestError::TooDeep)
  );
  assert!(Node::decode(&nest(bound).encode()).is_ok());
}

/// AUD-29-14: do: decode files whose extents leave a gap, overlap, start past zero, are empty, or give a
/// hole a chunk offset, and one whose recorded size is not its tiled length; expect `BadExtents` and
/// `SizeMismatch` — a manifest cannot describe bytes restore would lay out differently.
#[test]
fn extents_that_do_not_tile_are_refused() {
  let extent = |offset: u64, len: u64, chunk: u8, chunk_offset: u64| Extent {
    offset,
    len,
    chunk: [chunk; 32],
    chunk_offset,
  };
  let one_file = |extents: Vec<Extent>, size: u64| {
    Node::Directory(vec![Entry {
      name: "f".to_owned(),
      meta: NodeMeta {
        size,
        ..NodeMeta::default()
      },
      node: Node::File(extents),
    }])
  };
  for (label, extents) in [
    ("a gap", vec![extent(0, 4, 1, 0), extent(6, 2, 1, 0)]),
    ("an overlap", vec![extent(0, 4, 1, 0), extent(2, 6, 1, 0)]),
    ("past zero", vec![extent(8, 8, 1, 0)]),
    (
      "an empty extent",
      vec![extent(0, 0, 1, 0), extent(0, 8, 1, 0)],
    ),
    ("a hole with a chunk offset", vec![extent(0, 8, 0, 4)]),
  ] {
    assert_eq!(
      Node::decode(&one_file(extents, 8).encode()),
      Err(ManifestError::BadExtents),
      "{label}"
    );
  }
  assert_eq!(
    Node::decode(&one_file(vec![extent(0, 8, 1, 0)], 9).encode()),
    Err(ManifestError::SizeMismatch)
  );
}

/// A node's metadata with two attributes (one value of two extents, one empty) and pre-epoch times.
fn meta_with_attributes() -> NodeMeta {
  NodeMeta {
    atime_ns: -1,
    mtime_ns: -2,
    ctime_ns: -3,
    btime_ns: -4,
    xattrs: vec![
      Xattr {
        name: b"user.empty".to_vec(),
        extents: Vec::new(),
      },
      Xattr {
        name: b"user.value".to_vec(),
        extents: vec![
          Extent {
            offset: 0,
            len: 3,
            chunk: [0x51; 32],
            chunk_offset: 0,
          },
          Extent {
            offset: 3,
            len: 2,
            chunk: [0x52; 32],
            chunk_offset: 1,
          },
        ],
      },
    ],
    ..meta(0o644)
  }
}

/// The encoded root metadata of `meta`, through the manifest section's head.
fn encoded_meta(meta: &NodeMeta) -> Vec<u8> {
  slates_archive::manifest::encode_root_meta(meta)
}

/// Decodes a root-metadata record, requiring it to be the whole input.
fn decoded_meta(bytes: &[u8]) -> Result<NodeMeta, ManifestError> {
  let (meta, rest) = slates_archive::manifest::decode_root_meta(bytes)?;
  assert!(rest.is_empty(), "a root record is the whole input here");
  Ok(meta)
}

/// Format: the bytes ahead of the attribute count in an encoded metadata record — the inode number,
/// mode, four times, size, link count, owner and group (`write_meta`).
const META_PREFIX_BYTES: usize = 8 + 4 + 8 * 4 + 8 + 4 + 4 + 4;

/// AUD-29-56, golden vector: a node carrying attributes and pre-epoch times hashes to a pinned identity, so
/// the attribute encoding (names, value extents, signed times) cannot drift unnoticed across versions.
#[test]
fn the_attribute_encoding_matches_its_golden_vector() {
  let hex: String = one_file_with_meta(meta_with_attributes())
    .identity()
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect();
  assert_eq!(
    hex, "d1adc5b11ddbec1b19c68a816e29bab97affb9776ee59ff5cc7ba7f91eb80d47",
    "the attribute encoding changed; regenerate the golden vector only for a deliberate format change"
  );
}

/// AUD-29-56: do: round-trip metadata carrying attributes (written in any order) and pre-epoch times;
/// expect it back with its attributes in name order and its times unchanged, and the root identity to
/// change when only one attribute's value extents, only its name, or only one time changes.
#[test]
fn attributes_and_signed_times_round_trip_and_change_the_identity() {
  let mut shuffled = meta_with_attributes();
  shuffled.xattrs.reverse();
  let decoded = decoded_meta(&encoded_meta(&shuffled)).expect("decodes");
  assert_eq!(
    decoded,
    meta_with_attributes(),
    "attributes come back in name order"
  );
  let base = one_file_with_meta(meta_with_attributes()).identity();
  assert_eq!(one_file_with_meta(shuffled).identity(), base);
  let mut value = meta_with_attributes();
  value.xattrs[1].extents[1].chunk = [0x53; 32];
  let mut name = meta_with_attributes();
  name.xattrs[0].name = b"user.other".to_vec();
  let mut time = meta_with_attributes();
  time.btime_ns -= 1;
  for (what, changed) in [("value", value), ("name", name), ("birth time", time)] {
    assert_ne!(
      one_file_with_meta(changed).identity(),
      base,
      "an attribute {what} change is an identity change"
    );
  }
}

/// Metadata carrying empty-valued attributes under `names`, in the order given (the writer sorts them).
fn with_attribute_names(names: &[&[u8]]) -> NodeMeta {
  NodeMeta {
    xattrs: names
      .iter()
      .map(|name| Xattr {
        name: name.to_vec(),
        extents: Vec::new(),
      })
      .collect(),
    ..meta(0o644)
  }
}

/// AUD-29-56, hostile input: do: decode metadata whose attribute names are empty, NUL-holding, past the
/// longest name or repeated; expect each refused typed, and the longest valid name accepted.
#[test]
fn malformed_attribute_names_are_refused_typed() {
  use slates_archive::manifest::XATTR_NAME_MAX_BYTES;
  let with = with_attribute_names;
  let too_long = vec![b'n'; XATTR_NAME_MAX_BYTES + 1];
  let longest = vec![b'n'; XATTR_NAME_MAX_BYTES];
  assert!(decoded_meta(&encoded_meta(&with(&[&longest]))).is_ok());
  // A lone empty-named attribute is shorter than the smallest valid one, so its count is refused first;
  // inside a tree, with bytes after it, the name rule itself refuses it.
  assert_eq!(
    decoded_meta(&encoded_meta(&with(&[b""]))),
    Err(ManifestError::BadNode)
  );
  assert_eq!(
    Node::decode(&one_file_with_meta(with(&[b""])).encode()),
    Err(ManifestError::BadXattr)
  );
  for (what, names) in [
    ("a NUL in a name", vec![b"user.\0x".as_slice()]),
    ("a name past the longest", vec![too_long.as_slice()]),
    (
      "a repeated name",
      vec![b"user.a".as_slice(), b"user.a".as_slice()],
    ),
  ] {
    assert_eq!(
      decoded_meta(&encoded_meta(&with(&names))),
      Err(ManifestError::BadXattr),
      "{what}"
    );
  }
}

/// AUD-29-56, hostile input: do: decode metadata whose attributes are out of order, untiled, counted past
/// the bytes present or truncated; expect each refused typed, before any allocation the input does not back.
#[test]
fn malformed_attribute_structure_is_refused_typed() {
  // Out of order: two names of one length, their bytes swapped in place after the writer sorted them.
  let mut unordered = encoded_meta(&with_attribute_names(&[b"user.a", b"user.b"]));
  let first = unordered
    .windows(6)
    .position(|window| window == b"user.a")
    .expect("the first name is present");
  let second = unordered
    .windows(6)
    .position(|window| window == b"user.b")
    .expect("the second name is present");
  unordered.swap(first + 5, second + 5);
  assert_eq!(decoded_meta(&unordered), Err(ManifestError::BadXattr));
  // Untiled: a value whose second extent leaves a gap.
  let mut gap = meta_with_attributes();
  gap.xattrs[1].extents[1].offset += 1;
  assert_eq!(
    decoded_meta(&encoded_meta(&gap)),
    Err(ManifestError::BadExtents)
  );
  // A count past the bytes present.
  let mut counted = encoded_meta(&meta(0o644));
  counted[META_PREFIX_BYTES..META_PREFIX_BYTES + 4].copy_from_slice(&u32::MAX.to_le_bytes());
  assert_eq!(decoded_meta(&counted), Err(ManifestError::BadNode));
  // Truncated inside an attribute's last extent: its declared extent count now outruns the bytes left, so
  // it is refused by the bound before a field is read past the end.
  let whole = encoded_meta(&meta_with_attributes());
  assert_eq!(
    decoded_meta(&whole[..whole.len() - 1]),
    Err(ManifestError::BadNode)
  );
  // Truncated before the attribute count.
  assert_eq!(
    decoded_meta(&whole[..META_PREFIX_BYTES + 1]),
    Err(ManifestError::Truncated)
  );
}
