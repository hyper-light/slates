//! Tests for resumable transfer by the missing set (§2.6; Phase 7 task 5). The sender ships only
//! the chunks the receiver lacks; combined with what the receiver already holds, they restore the
//! whole archive. Deduplication against the held set is exact.

use std::collections::BTreeSet;

use slates_archive::archive::Archive;
use slates_archive::manifest::{Entry, Extent, Node, NodeMeta};
use slates_archive::restore::restore;
use slates_archive::transfer::{chunks_for, missing_set};

/// A flat archive of one raw chunk per file.
fn archive_of(files: &[(&str, Vec<u8>)]) -> Archive {
  let mut chunks = Vec::new();
  let mut entries = Vec::new();
  for (name, bytes) in files {
    let chunk = Archive::raw_chunk(bytes.clone());
    entries.push(Entry {
      name: (*name).to_owned(),
      meta: NodeMeta::default(),
      node: Node::File(vec![Extent {
        offset: 0,
        len: bytes.len() as u64,
        chunk: chunk.identity,
        chunk_offset: 0,
      }]),
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

/// With nothing held, the missing set is every distinct chunk; with everything held, it is empty.
#[test]
fn missing_set_bounds() {
  let archive = archive_of(&[("a", b"one".to_vec()), ("b", b"two".to_vec())]);
  let all = missing_set(&archive, &BTreeSet::new());
  assert_eq!(all.len(), 2);
  let held: BTreeSet<[u8; 32]> = archive.chunks.iter().map(|c| c.identity).collect();
  assert!(missing_set(&archive, &held).is_empty());
}

/// A receiver holding some chunks receives only the rest, and combining held with received chunks
/// restores the whole archive.
#[test]
fn a_partial_transfer_restores_the_whole_archive() {
  let archive = archive_of(&[
    ("shared", b"the identical build output".to_vec()),
    ("changed", b"a file only this version has".to_vec()),
  ]);
  // The receiver already holds the "shared" chunk from a previous version.
  let shared = archive.chunks[0].clone();
  let held: BTreeSet<[u8; 32]> = [shared.identity].into_iter().collect();

  let missing = missing_set(&archive, &held);
  assert_eq!(missing.len(), 1, "only the changed chunk is missing");

  let wanted: BTreeSet<[u8; 32]> = missing.iter().copied().collect();
  let shipped = chunks_for(&archive, &wanted);
  assert_eq!(shipped.len(), 1);

  // The receiver reconstructs the full archive from its held chunk plus the shipped chunks.
  let mut received = archive.clone();
  received.chunks = vec![shared];
  received.chunks.extend(shipped);
  let restored = restore(&received).expect("restores from held + shipped");
  assert_eq!(
    restored.files.get("shared"),
    Some(&b"the identical build output".to_vec())
  );
  assert_eq!(
    restored.files.get("changed"),
    Some(&b"a file only this version has".to_vec())
  );
}

/// Identical chunks across files are shipped once (dedup against the wanted set).
#[test]
fn duplicate_chunks_ship_once() {
  let archive = archive_of(&[("x", b"same".to_vec()), ("y", b"same".to_vec())]);
  // Both files share one chunk identity.
  assert_eq!(missing_set(&archive, &BTreeSet::new()).len(), 1);
  let wanted: BTreeSet<[u8; 32]> = archive.chunks.iter().map(|c| c.identity).collect();
  assert_eq!(
    chunks_for(&archive, &wanted).len(),
    1,
    "the shared chunk ships once"
  );
}
