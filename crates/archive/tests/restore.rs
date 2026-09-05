//! Tests for restore (§2.6; T-7.1, AC-7.3). Restoring a volume from an archive reproduces every
//! file's bytes: a normal extent reads from its chunk, a zero-chunk extent is a hole of zeros, a
//! multi-extent file concatenates its pieces. A missing chunk is a typed refusal, and restore
//! round-trips through the archive's own encode/decode.

use std::collections::BTreeMap;

use slates_archive::archive::Archive;
use slates_archive::format::ArchiveError;
use slates_archive::manifest::{Entry, Extent, Node, NodeMeta};
use slates_archive::restore::restore;

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
    manifest: Node::Directory(entries),
    chunks,
  }
}

/// A single file restores to its bytes.
#[test]
fn a_file_restores_to_its_bytes() {
  let archive = archive_of(&[("readme", b"hello, archive".to_vec())]);
  let restored = restore(&archive).expect("restores");
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
    manifest: Node::Directory(vec![Entry {
      name: "sparse".to_owned(),
      meta: NodeMeta::default(),
      node: Node::File(vec![Extent {
        offset: 0,
        len: 64,
        chunk: [0u8; 32],
        chunk_offset: 0,
      }]),
    }]),
    chunks: Vec::new(),
  };
  let restored = restore(&archive).expect("restores");
  assert_eq!(restored.files.get("sparse"), Some(&vec![0u8; 64]));
}

/// A file spanning two chunks concatenates them, reading a sub-range of each.
#[test]
fn a_multi_extent_file_concatenates_its_chunks() {
  let first = Archive::raw_chunk(b"ABCDEFGH".to_vec());
  let second = Archive::raw_chunk(b"wxyz".to_vec());
  let manifest = Node::Directory(vec![Entry {
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
  }]);
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    manifest,
    chunks: vec![first, second],
  };
  let restored = restore(&archive).expect("restores");
  assert_eq!(restored.files.get("joined"), Some(&b"CDEwx".to_vec()));
}

/// A directory tree restores its files by path and records its directories.
#[test]
fn a_tree_restores_files_and_directories() {
  let lib = Archive::raw_chunk(b"pub fn f() {}".to_vec());
  let manifest = Node::Directory(vec![Entry {
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
  }]);
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    manifest,
    chunks: vec![lib],
  };
  let restored = restore(&archive).expect("restores");
  assert_eq!(
    restored.files.get("src/lib.rs"),
    Some(&b"pub fn f() {}".to_vec())
  );
  assert!(restored.directories.contains("src"));
}

/// An extent naming a chunk the archive does not hold is refused.
#[test]
fn a_missing_chunk_is_refused() {
  let manifest = Node::Directory(vec![Entry {
    name: "orphan".to_owned(),
    meta: NodeMeta::default(),
    node: Node::File(vec![Extent {
      offset: 0,
      len: 4,
      chunk: [0x7fu8; 32],
      chunk_offset: 0,
    }]),
  }]);
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    manifest,
    chunks: Vec::new(),
  };
  assert_eq!(restore(&archive), Err(ArchiveError::MissingChunk));
}

/// Restore works after an encode/decode round-trip through the archive stream.
#[test]
fn restore_after_a_round_trip() {
  let archive = archive_of(&[("a", b"first".to_vec()), ("b", b"second file".to_vec())]);
  let bytes = archive.encode();
  let decoded = Archive::decode(&bytes).expect("decodes");
  let restored = restore(&decoded).expect("restores");
  assert_eq!(restored.files.get("a"), Some(&b"first".to_vec()));
  assert_eq!(restored.files.get("b"), Some(&b"second file".to_vec()));
}

/// A restored file whose chunk was LZ4-compressed decodes correctly.
#[test]
fn restore_decodes_compressed_chunks() {
  let raw = vec![0x5au8; 500];
  let chunk = Archive::compressed_chunk(raw.clone());
  assert_eq!(chunk.encoding, slates_archive::format::Encoding::Lz4);
  let manifest = Node::Directory(vec![Entry {
    name: "z".to_owned(),
    meta: NodeMeta::default(),
    node: Node::File(vec![Extent {
      offset: 0,
      len: raw.len() as u64,
      chunk: chunk.identity,
      chunk_offset: 0,
    }]),
  }]);
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    manifest,
    chunks: vec![chunk],
  };
  let restored = restore(&archive).expect("restores");
  assert_eq!(restored.files.get("z"), Some(&raw));
}
