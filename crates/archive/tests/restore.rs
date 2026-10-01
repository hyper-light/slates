//! Tests for restore (§2.6; T-7.1, AC-7.3). Restoring a volume from an archive reproduces every
//! file's bytes: a normal extent reads from its chunk, a zero-chunk extent is a hole of zeros, a
//! multi-extent file concatenates its pieces. A missing chunk is a typed refusal, and restore
//! round-trips through the archive's own encode/decode.

use std::collections::BTreeMap;

use slates_archive::archive::Archive;
use slates_archive::format::ArchiveError;
use slates_archive::manifest::{Entry, Extent, Node, NodeMeta};
use slates_archive::restore::restore;

/// Shape: the bytes a restore here is admitted — far past any fixture, so only the budget tests meet it.
const ADMITTED: u64 = 1 << 30;

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

/// Builds a flat archive: one raw chunk per file, and a manifest directory with one whole-file
/// extent per file.
fn archive_of(files: &[(&str, Vec<u8>)]) -> Archive {
  let mut chunks = Vec::new();
  let mut entries = Vec::new();
  for (name, bytes) in files {
    let chunk = Archive::raw_chunk(bytes.clone());
    let extent = Extent {
      offset: 0,
      len: bytes.len() as u64,
      chunk: chunk.identity,
      chunk_offset: 0,
    };
    entries.push(Entry {
      name: (*name).to_owned(),
      meta: NodeMeta::default(),
      node: Node::File(vec![extent]),
    });
    chunks.push(chunk);
  }
  Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest: canonical(Node::Directory(entries)),
    chunks,
  }
}

/// A single file restores to its bytes.
#[test]
fn a_file_restores_to_its_bytes() {
  let archive = archive_of(&[("readme", b"hello, archive".to_vec())]);
  let restored = restore(&archive, ADMITTED).expect("restores");
  let mut expected: BTreeMap<String, Vec<u8>> = BTreeMap::new();
  expected.insert("readme".to_owned(), b"hello, archive".to_vec());
  assert_eq!(restored.files, expected);
}

/// A zero-chunk extent restores as a hole of zeros.
#[test]
fn a_hole_restores_as_zeros() {
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest: canonical(Node::Directory(vec![Entry {
      name: "sparse".to_owned(),
      meta: NodeMeta::default(),
      node: Node::File(vec![Extent {
        offset: 0,
        len: 64,
        chunk: [0u8; 32],
        chunk_offset: 0,
      }]),
    }])),
    chunks: Vec::new(),
  };
  let restored = restore(&archive, ADMITTED).expect("restores");
  assert_eq!(restored.files.get("sparse"), Some(&vec![0u8; 64]));
}

/// A file spanning two chunks concatenates them, reading a sub-range of each.
#[test]
fn a_multi_extent_file_concatenates_its_chunks() {
  let first = Archive::raw_chunk(b"ABCDEFGH".to_vec());
  let second = Archive::raw_chunk(b"wxyz".to_vec());
  let manifest = canonical(Node::Directory(vec![Entry {
    name: "joined".to_owned(),
    meta: NodeMeta::default(),
    node: Node::File(vec![
      Extent {
        offset: 0,
        len: 3,
        chunk: first.identity,
        chunk_offset: 2,
      }, // "CDE"
      Extent {
        offset: 3,
        len: 2,
        chunk: second.identity,
        chunk_offset: 0,
      }, // "wx"
    ]),
  }]));
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest,
    chunks: vec![first, second],
  };
  let restored = restore(&archive, ADMITTED).expect("restores");
  assert_eq!(restored.files.get("joined"), Some(&b"CDEwx".to_vec()));
}

/// A directory tree restores its files by path and records its directories.
#[test]
fn a_tree_restores_files_and_directories() {
  let lib = Archive::raw_chunk(b"pub fn f() {}".to_vec());
  let manifest = canonical(Node::Directory(vec![Entry {
    name: "src".to_owned(),
    meta: NodeMeta::default(),
    node: Node::Directory(vec![Entry {
      name: "lib.rs".to_owned(),
      meta: NodeMeta::default(),
      node: Node::File(vec![Extent {
        offset: 0,
        len: lib.raw_len,
        chunk: lib.identity,
        chunk_offset: 0,
      }]),
    }]),
  }]));
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest,
    chunks: vec![lib],
  };
  let restored = restore(&archive, ADMITTED).expect("restores");
  assert_eq!(
    restored.files.get("src/lib.rs"),
    Some(&b"pub fn f() {}".to_vec())
  );
  assert!(restored.directories.contains("src"));
}

/// An extent naming a chunk the archive does not hold is refused.
#[test]
fn a_missing_chunk_is_refused() {
  let manifest = canonical(Node::Directory(vec![Entry {
    name: "orphan".to_owned(),
    meta: NodeMeta::default(),
    node: Node::File(vec![Extent {
      offset: 0,
      len: 4,
      chunk: [0x7fu8; 32],
      chunk_offset: 0,
    }]),
  }]));
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest,
    chunks: Vec::new(),
  };
  assert_eq!(restore(&archive, ADMITTED), Err(ArchiveError::MissingChunk));
}

/// Restore works after an encode/decode round-trip through the archive stream.
#[test]
fn restore_after_a_round_trip() {
  let archive = archive_of(&[("a", b"first".to_vec()), ("b", b"second file".to_vec())]);
  let bytes = archive.encode();
  let decoded = Archive::decode(&bytes).expect("decodes");
  let restored = restore(&decoded, ADMITTED).expect("restores");
  assert_eq!(restored.files.get("a"), Some(&b"first".to_vec()));
  assert_eq!(restored.files.get("b"), Some(&b"second file".to_vec()));
}

/// A restored file whose chunk was LZ4-compressed decodes correctly.
#[test]
fn restore_decodes_compressed_chunks() {
  let raw = vec![0x5au8; 500];
  let chunk = Archive::compressed_chunk(raw.clone());
  assert_eq!(chunk.encoding, slates_archive::format::Encoding::Lz4);
  let manifest = canonical(Node::Directory(vec![Entry {
    name: "z".to_owned(),
    meta: NodeMeta::default(),
    node: Node::File(vec![Extent {
      offset: 0,
      len: raw.len() as u64,
      chunk: chunk.identity,
      chunk_offset: 0,
    }]),
  }]));
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest,
    chunks: vec![chunk],
  };
  let restored = restore(&archive, ADMITTED).expect("restores");
  assert_eq!(restored.files.get("z"), Some(&raw));
}

/// Restore surfaces each named node's metadata by path (for a granted landing to apply).
#[test]
fn restore_surfaces_node_metadata() {
  let chunk = Archive::raw_chunk(b"hi".to_vec());
  let meta = NodeMeta {
    ino: 7,
    mode: 0o600,
    mtime_ns: 123,
    ctime_ns: 456,
    size: 2,
    nlink: 1,
    uid: 1234,
    gid: 4321,
    ..NodeMeta::default()
  };
  let manifest = canonical(Node::Directory(vec![Entry {
    name: "secret".to_owned(),
    meta: meta.clone(),
    node: Node::File(vec![Extent {
      offset: 0,
      len: 2,
      chunk: chunk.identity,
      chunk_offset: 0,
    }]),
  }]));
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest,
    chunks: vec![chunk],
  };
  let restored = restore(&archive, ADMITTED).expect("restores");
  assert_eq!(restored.metadata.get("secret"), Some(&meta));
  // The metadata survives an encode/decode round trip through the archive stream.
  let decoded = Archive::decode(&archive.encode()).expect("decodes");
  let restored = restore(&decoded, ADMITTED).expect("restores");
  assert_eq!(
    restored.metadata.get("secret"),
    Some(&meta),
    "metadata survives the stream"
  );
}

/// Format minor 2: the root directory's own metadata (mode, owner, times) comes back from a restore,
/// through the archive's byte stream, so a clone or a takeover successor rebuilds the root as the origin
/// held it — the entries name every node but the root.
#[test]
fn the_roots_metadata_is_restored_through_the_byte_stream() {
  let mut archive = archive_of(&[("f", b"x".to_vec())]);
  archive.root_meta = NodeMeta {
    ino: 1,
    mode: 0o750,
    mtime_ns: 11,
    ctime_ns: 12,
    size: 0,
    nlink: 2,
    uid: 1000,
    gid: 2000,
    ..NodeMeta::default()
  };
  let decoded = Archive::decode(&archive.encode()).expect("the archive decodes");
  assert_eq!(decoded.root_meta, archive.root_meta);
  let restored = restore(&decoded, ADMITTED).expect("restores");
  assert_eq!(
    restored.root, archive.root_meta,
    "the root's metadata is restored"
  );
  assert_eq!(restored.files.get("f"), Some(&b"x".to_vec()));
}

/// An archive holding `manifest` over `chunks`, with the fixture header.
fn archive_with(manifest: Node, chunks: Vec<slates_archive::Chunk>) -> Archive {
  Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest: canonical(manifest),
    chunks,
  }
}

/// One entry named `name` over `node`, its recorded size left to [`canonical`].
fn entry(name: &str, node: Node) -> Entry {
  Entry {
    name: name.to_owned(),
    meta: NodeMeta::default(),
    node,
  }
}

/// AUD-29-13: do: restore a file that is one hole of a terabyte, admitted a gigabyte; expect `OverBudget`
/// naming what it needed — refused from the plan, before any byte is allocated (the test would abort on
/// the allocation otherwise).
#[test]
fn a_huge_hole_is_refused_before_allocation() {
  let terabyte = 1u64 << 40;
  let archive = archive_with(
    Node::Directory(vec![entry(
      "sparse",
      Node::File(vec![Extent {
        offset: 0,
        len: terabyte,
        chunk: [0u8; 32],
        chunk_offset: 0,
      }]),
    )]),
    Vec::new(),
  );
  assert_eq!(
    restore(&archive, ADMITTED),
    Err(ArchiveError::OverBudget {
      needed: terabyte,
      budget: ADMITTED
    })
  );
}

/// The references a many-times-named chunk test uses: `count` extents tiling one file, each the whole of
/// the chunk `identity` of `len` bytes.
fn repeated(identity: [u8; 32], len: u64, count: u64) -> Node {
  Node::File(
    (0..count)
      .map(|at| Extent {
        offset: at * len,
        len,
        chunk: identity,
        chunk_offset: 0,
      })
      .collect(),
  )
}

/// AUD-29-13: do: one compressed 4 KiB chunk named by 256 extents of one file; expect the file restored
/// exactly (256 copies) with the chunk decoded **once** (the non-vacuity counter), and the same archive
/// under a budget one byte short of what it needs refused `OverBudget` — a small archive cannot draw an
/// unadmitted expansion by repeating a reference.
#[test]
fn a_chunk_named_many_times_is_decoded_once_and_its_expansion_is_admitted() {
  let bytes = vec![0x5a_u8; 4096];
  let chunk = Archive::compressed_chunk(bytes.clone());
  assert_eq!(chunk.encoding, slates_archive::Encoding::Lz4);
  let (identity, len) = (chunk.identity, chunk.raw_len);
  let copies = 256u64;
  let archive = archive_with(
    Node::Directory(vec![entry("copies", repeated(identity, len, copies))]),
    vec![chunk],
  );
  let restored = restore(&archive, ADMITTED).expect("restores");
  assert_eq!(restored.files["copies"], bytes.repeat(256));
  assert_eq!(restored.chunks_decoded, 1, "the chunk was decoded once");
  let needed = copies * len + len;
  assert_eq!(
    restore(&archive, needed - 1),
    Err(ArchiveError::OverBudget {
      needed,
      budget: needed - 1
    })
  );
  assert!(
    restore(&archive, needed).is_ok(),
    "exactly the need restores"
  );
}

/// AUD-29-14, AUD-29-15 (an in-memory tree reaches restore without the decoder): do: restore trees whose
/// extents leave a gap, overlap, start past zero, or disagree with the recorded size, whose names are
/// `..`, carry a separator or repeat; expect each refused typed, never different bytes.
#[test]
fn a_non_canonical_tree_is_refused_typed() {
  let chunk = Archive::raw_chunk(b"abcdefgh".to_vec());
  let at = |offset: u64, len: u64| Extent {
    offset,
    len,
    chunk: chunk.identity,
    chunk_offset: 0,
  };
  let with_extents = |extents: Vec<Extent>| {
    archive_with(
      Node::Directory(vec![entry("f", Node::File(extents))]),
      vec![chunk.clone()],
    )
  };
  for (label, extents) in [
    ("a gap", vec![at(0, 4), at(6, 2)]),
    ("an overlap", vec![at(0, 4), at(2, 4)]),
    ("past zero", vec![at(8, 1)]),
    ("an empty extent", vec![at(0, 0)]),
  ] {
    assert_eq!(
      restore(&with_extents(extents), ADMITTED),
      Err(ArchiveError::BadExtents),
      "{label}"
    );
  }
  let mut wrong_size = with_extents(vec![at(0, 8)]);
  if let Node::Directory(entries) = &mut wrong_size.manifest {
    entries[0].meta.size = 9;
  }
  assert_eq!(
    restore(&wrong_size, ADMITTED),
    Err(ArchiveError::BadExtents)
  );
  for name in ["..", ".", "a/b", "a\\b", "nul\0", ""] {
    let archive = archive_with(
      Node::Directory(vec![entry(name, Node::File(Vec::new()))]),
      Vec::new(),
    );
    assert_eq!(
      restore(&archive, ADMITTED),
      Err(ArchiveError::BadName),
      "{name:?}"
    );
  }
  let twice = archive_with(
    Node::Directory(vec![
      entry("same", Node::File(Vec::new())),
      entry("same", Node::Directory(Vec::new())),
    ]),
    Vec::new(),
  );
  assert_eq!(restore(&twice, ADMITTED), Err(ArchiveError::DuplicatePath));
}

/// AUD-29-13: do: encode, decode and restore an archive whose tree nests as deep as the decoder admits
/// (`manifest::MAX_DEPTH`), on a test thread's default stack in a debug build; expect every directory
/// restored — the decoder and restore walk iteratively, and the recursive encoder and Merkle identity stay
/// within the stack at the bound.
#[test]
fn a_tree_at_the_depth_bound_restores() {
  let depth = slates_archive::manifest::MAX_DEPTH;
  let mut node = Node::Directory(Vec::new());
  for _ in 0..depth {
    node = Node::Directory(vec![entry("d", node)]);
  }
  // Built without `canonical` (a recursive test helper): a tree of directories is already canonical.
  let archive = Archive {
    manifest: node,
    ..archive_with(Node::Directory(Vec::new()), Vec::new())
  };
  let decoded = Archive::decode(&archive.encode()).expect("the bound decodes");
  let restored = restore(&decoded, ADMITTED).expect("restores");
  assert_eq!(restored.directories.len(), depth);
}
