//! A-92 piece 3b (hyper-raft `docs/seal.md` §7, §13): a snapshot rides the content plane as an envelope archive. It
//! opens to the very archive under its tenant's keys; it is the same bytes every time it is wrapped, so holders
//! deduplicate across snapshots; a holder can verify it as any archive yet finds no plaintext, plaintext identity or
//! file name in it; and every relabelled, missing, extra or altered part is refused typed. Driven through
//! `slates_cluster::envelope`.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use hyper_seal::keys::WrappingKey;
use hyper_seal::name::Namer;
use slates_archive::format::Chunk;
use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};
use slates_cluster::envelope::{EnvelopeError, is_envelope, open, wrap};

/// Shape: keys the tests may hold at once in the process's locked region (a few per test, tests in parallel).
const KEYS: usize = 256;
/// Shape: the segment the tests seal in: one 4 KiB base page (seal.md §4).
const SEGMENT: u32 = 4096;
/// Format: the generation the test keys are made at.
const GENERATION: u32 = 1;
/// Shape: a chunk's length: a few segments, so a chunk seals in more than one.
const CHUNK_LEN: usize = 10_000;
/// Format: a file's distinctive name, which no holder may find in the envelope.
const SECRET_NAME: &str = "quarterly-forecast.xlsx";

/// A tenant's keys: a lineage key and a naming key.
struct Tenant {
  lineage: WrappingKey,
  namer: Namer,
}

fn new_tenant() -> Tenant {
  hyper_seal::lock_keys(KEYS).unwrap();
  let tenant = WrappingKey::generate(GENERATION).unwrap();
  let (naming, _) = tenant.make_child().unwrap();
  Tenant {
    lineage: WrappingKey::generate(GENERATION).unwrap(),
    namer: Namer::new(&naming).unwrap(),
  }
}

/// A raw chunk of distinctive bytes for `seed`.
fn chunk(seed: u8) -> Chunk {
  Archive::raw_chunk(
    (0..CHUNK_LEN)
      .map(|at| {
        u8::try_from(at % 251)
          .unwrap()
          .wrapping_mul(seed | 1)
          .wrapping_add(seed)
      })
      .collect(),
  )
}

/// A file entry named `name` whose one extent is all of `chunk`.
fn file(name: &str, chunk: &Chunk, mtime_ns: i64) -> Entry {
  Entry {
    name: name.to_owned(),
    meta: NodeMeta {
      mode: 0o100_644,
      mtime_ns,
      size: chunk.raw_len,
      nlink: 1,
      ..NodeMeta::default()
    },
    node: Node::File(vec![Extent {
      offset: 0,
      len: chunk.raw_len,
      chunk: chunk.identity,
      chunk_offset: 0,
    }]),
  }
}

/// A snapshot of three files over `chunks`: the first and last share the first chunk, so the archive holds each
/// distinct chunk once, in the order the manifest first references it.
fn snapshot(chunks: &[Chunk], snapshot_id: u64) -> Archive {
  let mut entries = vec![
    file("a", &chunks[0], 1_700_000_000_000_000_000),
    file(SECRET_NAME, &chunks[1], 1_700_000_000_000_000_001),
    file("z", &chunks[0], 1_700_000_000_000_000_002),
  ];
  entries.sort_by(|a, b| a.name.cmp(&b.name));
  Archive {
    base_page_size: 4096,
    chunk_min: 1024,
    chunk_max: 1 << 20,
    created_unix: 1_700_000_000,
    volume_id: 9,
    snapshot_id,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest: Node::Directory(entries),
    chunks: chunks.to_vec(),
  }
}

/// Whether `needle` occurs anywhere in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
  haystack
    .windows(needle.len())
    .any(|window| window == needle)
}

/// A-92 3b: do wrap a snapshot and open the envelope, after it travels as an encoded archive; expect the very archive
/// back, chunks in manifest order, its informational creation time zeroed; and the envelope, not the plain archive,
/// to say it is one.
#[test]
fn an_envelope_opens_to_the_very_archive() {
  let tenant = new_tenant();
  let original = snapshot(&[chunk(1), chunk(2)], 1);
  let envelope = wrap(&original, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  let carried = Archive::decode(&envelope.encode()).unwrap();
  assert!(is_envelope(&carried) && !is_envelope(&original));
  assert_eq!(
    open(&carried, &tenant.lineage, &tenant.namer).unwrap(),
    Archive {
      created_unix: 0,
      ..original.clone()
    }
  );
  assert_eq!(
    open(&original, &tenant.lineage, &tenant.namer),
    Err(EnvelopeError::Shape),
    "a plain archive is not opened as an envelope"
  );
}

/// The healer's re-offer: do wrap one snapshot as sealed at two different times; expect one envelope, the same
/// manifest identity the placement recorded.
#[test]
fn a_snapshot_sealed_again_later_is_the_same_envelope() {
  let tenant = new_tenant();
  let chunks = [chunk(1), chunk(2)];
  let first = snapshot(&chunks, 1);
  let later = Archive {
    created_unix: first.created_unix + 3600,
    ..first.clone()
  };
  let a = wrap(&first, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  let b = wrap(&later, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  assert_eq!(a.manifest_identity(), b.manifest_identity());
  assert_eq!(a.encode(), b.encode());
}

/// seal.md §7: do wrap the same snapshot twice, and a second snapshot sharing one chunk with it; expect the same
/// envelope bytes both times, two envelope chunks plus the manifest for two distinct chunks, and the shared chunk under
/// the same sealed identity in both snapshots' envelopes, so holders keep it once.
#[test]
fn equal_content_seals_to_equal_bytes_and_holders_deduplicate_it() {
  let tenant = new_tenant();
  let (one, two, three) = (chunk(1), chunk(2), chunk(3));
  let first = snapshot(&[one.clone(), two], 1);
  let wrapped = wrap(&first, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  let again = wrap(&first, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  assert_eq!(
    wrapped.encode(),
    again.encode(),
    "the same bytes every time"
  );
  assert_eq!(
    wrapped.chunks.len(),
    3,
    "two distinct chunks and the manifest"
  );
  let second = wrap(
    &snapshot(&[one, three], 2),
    &tenant.lineage,
    &tenant.namer,
    SEGMENT,
  )
  .unwrap();
  let shared: Vec<[u8; 32]> = wrapped
    .chunks
    .iter()
    .map(|chunk| chunk.identity)
    .filter(|identity| {
      second
        .chunks
        .iter()
        .any(|other| other.identity == *identity)
    })
    .collect();
  assert_eq!(
    shared.len(),
    1,
    "the shared chunk is one sealed chunk in both"
  );
}

/// seal.md §13: do encode an envelope as a holder receives it; expect it to verify as an archive with no key, and to
/// hold none of the plaintext chunks' bytes, none of their identities, no file name and no time.
#[test]
fn a_holder_verifies_an_envelope_but_finds_no_plaintext_in_it() {
  let tenant = new_tenant();
  let chunks = [chunk(1), chunk(2)];
  let original = snapshot(&chunks, 1);
  let bytes = wrap(&original, &tenant.lineage, &tenant.namer, SEGMENT)
    .unwrap()
    .encode();
  Archive::decode(&bytes).unwrap();
  for chunk in &chunks {
    assert!(
      !contains(&bytes, &chunk.payload[..64]),
      "no plaintext bytes"
    );
    assert!(!contains(&bytes, &chunk.identity), "no plaintext identity");
  }
  assert!(!contains(&bytes, SECRET_NAME.as_bytes()), "no file name");
  assert!(
    !contains(&bytes, &1_700_000_000_000_000_001i64.to_be_bytes()),
    "no file time"
  );
  assert!(
    !contains(&bytes, &original.manifest_identity()),
    "no plaintext manifest identity"
  );
}

/// The envelope's directory entries.
fn entries(envelope: &mut Archive) -> &mut Vec<Entry> {
  let Node::Directory(entries) = &mut envelope.manifest else {
    panic!("an envelope is a directory");
  };
  entries
}

/// seal.md §12: do give the opener an envelope with two chunk entries' extents swapped, a chunk entry removed, the
/// manifest entry removed, a second manifest entry, one sealed chunk's bit flipped, and the right envelope under
/// another tenant's keys; expect each refused typed, and never a wrong archive.
#[test]
fn a_relabelled_missing_or_altered_envelope_is_refused_typed() {
  let tenant = new_tenant();
  let envelope = wrap(
    &snapshot(&[chunk(1), chunk(2)], 1),
    &tenant.lineage,
    &tenant.namer,
    SEGMENT,
  )
  .unwrap();
  let opened = |envelope: &Archive| open(envelope, &tenant.lineage, &tenant.namer);
  let chunk_entries: Vec<usize> = {
    let mut probe = envelope.clone();
    entries(&mut probe)
      .iter()
      .enumerate()
      .filter(|(_, entry)| entry.name.starts_with('c'))
      .map(|(at, _)| at)
      .collect()
  };
  let mut swapped = envelope.clone();
  let list = entries(&mut swapped);
  let first = list[chunk_entries[0]].node.clone();
  list[chunk_entries[0]].node = list[chunk_entries[1]].node.clone();
  list[chunk_entries[1]].node = first;
  assert!(
    matches!(opened(&swapped), Err(EnvelopeError::Sealed(_))),
    "a chunk relabelled under another's name"
  );
  let mut missing = envelope.clone();
  entries(&mut missing).remove(chunk_entries[0]);
  assert_eq!(
    opened(&missing),
    Err(EnvelopeError::Missing),
    "a chunk removed"
  );
  let mut headless = envelope.clone();
  entries(&mut headless).retain(|entry| !entry.name.starts_with('m'));
  assert_eq!(
    opened(&headless),
    Err(EnvelopeError::Manifest),
    "no manifest"
  );
  let mut doubled = envelope.clone();
  let manifest = entries(&mut doubled)
    .iter()
    .find(|entry| entry.name.starts_with('m'))
    .unwrap()
    .clone();
  entries(&mut doubled).push(Entry {
    name: format!("m{}", "0".repeat(32)),
    ..manifest
  });
  assert_eq!(
    opened(&doubled),
    Err(EnvelopeError::Manifest),
    "two manifests"
  );
  for at in 0..envelope.chunks.len() {
    let mut flipped = envelope.clone();
    flipped.chunks[at].payload[100] ^= 1;
    assert!(opened(&flipped).is_err(), "chunk {at} with a bit flipped");
  }
  let stranger = new_tenant();
  assert!(
    open(&envelope, &stranger.lineage, &stranger.namer).is_err(),
    "another tenant's keys"
  );
}
