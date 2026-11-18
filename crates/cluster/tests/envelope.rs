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
use slates_cluster::envelope::{EnvelopeError, Opening, Wrapping, is_envelope, open, wrap};

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

/// Shape: the distinct chunks of the many-chunk snapshot: enough that a scan per chunk, the shape the wrap and the open
/// had until 2026-11-17, is the most of their work, and few enough to seal in a test's time.
const MANY_CHUNKS: u32 = 2048;
/// Shape: the files of the many-chunk snapshot that repeat a chunk an earlier file holds.
const REPEATED_FILES: u32 = 512;
/// Shape: a small chunk's length: a quarter of a segment, so thousands seal quickly.
const SMALL_CHUNK_LEN: usize = 1024;

/// A raw chunk of `SMALL_CHUNK_LEN` bytes distinct for `seed`.
fn small_chunk(seed: u32) -> Chunk {
  Archive::raw_chunk(
    seed
      .to_le_bytes()
      .iter()
      .copied()
      .cycle()
      .take(SMALL_CHUNK_LEN)
      .enumerate()
      .map(|(at, byte)| byte ^ u8::try_from(at % 251).unwrap())
      .collect(),
  )
}

/// A-92 3b at scale: do wrap a snapshot of 2,048 distinct chunks, 512 more files repeating some of them, then open the
/// envelope after it travels as an encoded archive. Expect the very archive back, every chunk in manifest order; and
/// the envelope to store each sealed chunk once: the distinct chunks and the manifest, no identity twice. The wrap and
/// the open find a sealed chunk by a set and an index, where a scan per chunk made each `n²` in the chunks.
#[test]
fn an_envelope_of_thousands_of_chunks_opens_to_the_very_archive() {
  let tenant = new_tenant();
  let chunks: Vec<Chunk> = (0..MANY_CHUNKS).map(small_chunk).collect();
  let mut entries: Vec<Entry> = (0..MANY_CHUNKS)
    .map(|at| {
      file(
        &format!("f{at:05}"),
        &chunks[usize::try_from(at).unwrap()],
        i64::from(at),
      )
    })
    .chain((0..REPEATED_FILES).map(|at| {
      let repeated = &chunks[usize::try_from(at * 3 % MANY_CHUNKS).unwrap()];
      file(&format!("r{at:05}"), repeated, i64::from(at))
    }))
    .collect();
  entries.sort_by(|a, b| a.name.cmp(&b.name));
  let original = Archive {
    manifest: Node::Directory(entries),
    chunks: chunks.clone(),
    ..snapshot(&[chunk(1), chunk(2)], 7)
  };
  let envelope = wrap(&original, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  let carried = Archive::decode(&envelope.encode()).unwrap();
  let identities: std::collections::BTreeSet<[u8; 32]> =
    carried.chunks.iter().map(|chunk| chunk.identity).collect();
  assert_eq!(
    (carried.chunks.len(), identities.len()),
    (
      usize::try_from(MANY_CHUNKS).unwrap() + 1,
      usize::try_from(MANY_CHUNKS).unwrap() + 1
    ),
    "each distinct chunk and the manifest, stored once"
  );
  assert_eq!(
    open(&carried, &tenant.lineage, &tenant.namer).unwrap(),
    Archive {
      created_unix: 0,
      ..original
    }
  );
}

/// Shape: the distinct chunks of the sliced-wrap snapshot: enough that the slices fall in many places.
const SLICED_CHUNKS: u32 = 64;

/// A snapshot of `count` files, each over its own distinct small chunk, and one file repeating the first chunk.
fn many_chunk_snapshot(count: u32) -> Archive {
  let chunks: Vec<Chunk> = (0..count).map(small_chunk).collect();
  let mut entries: Vec<Entry> = chunks
    .iter()
    .enumerate()
    .map(|(at, chunk)| file(&format!("f{at:05}"), chunk, i64::try_from(at).unwrap()))
    .chain(std::iter::once(file("repeat", &chunks[0], 0)))
    .collect();
  entries.sort_by(|a, b| a.name.cmp(&b.name));
  Archive {
    manifest: Node::Directory(entries),
    chunks,
    ..snapshot(&[chunk(1), chunk(2)], 11)
  }
}

/// A-323: do wrap one snapshot of 64 distinct chunks a slice at a time, at budgets of one byte (a chunk a slice), a
/// chunk and a half, five chunks, more than the whole archive, and the whole budget; expect every one the very envelope
/// `wrap` makes, byte for byte, and the one-byte budget to take a slice a chunk: 64 slices, the chunks left falling by
/// one each, the manifest sealed in the slice that seals the last chunk. A wrap's slices change where the work falls,
/// never what it makes.
#[test]
fn an_envelope_wrapped_in_slices_is_the_whole_wraps_to_the_byte() {
  let tenant = new_tenant();
  let original = many_chunk_snapshot(SLICED_CHUNKS);
  let whole = wrap(&original, &tenant.lineage, &tenant.namer, SEGMENT)
    .unwrap()
    .encode();
  let chunk_bytes = u64::try_from(SMALL_CHUNK_LEN).unwrap();
  for budget in [
    1,
    chunk_bytes * 3 / 2,
    chunk_bytes * 5,
    chunk_bytes * 1000,
    u64::MAX,
  ] {
    let mut wrapping = Wrapping::new(original.clone(), SEGMENT);
    let mut slices = 0u32;
    let mut left = Vec::new();
    let envelope = loop {
      slices += 1;
      let made = wrapping
        .advance(&tenant.lineage, &tenant.namer, budget)
        .unwrap();
      left.push(wrapping.chunks_left());
      if let Some(envelope) = made {
        break envelope;
      }
    };
    assert_eq!(
      envelope.encode(),
      whole,
      "the envelope wrapped in slices of {budget} bytes is the whole wrap's"
    );
    if budget == 1 {
      assert_eq!(slices, SLICED_CHUNKS, "a slice a chunk");
      let expected: Vec<usize> = (0..usize::try_from(SLICED_CHUNKS).unwrap()).rev().collect();
      assert_eq!(left, expected, "the chunks left fall by one a slice");
    }
  }
}

/// A-323: do advance a wrap past its envelope, and a wrap whose seal refuses (a segment of no bytes, which the seal
/// refuses); expect the envelope once and then `Spent`, and the refusal once and then `Spent`: a wrap that ended is never
/// advanced into a second envelope or a half-sealed one.
#[test]
fn a_wrap_ends_at_its_envelope_or_its_refusal() {
  let tenant = new_tenant();
  let original = snapshot(&[chunk(1), chunk(2)], 1);
  let mut wrapping = Wrapping::new(original.clone(), SEGMENT);
  assert!(
    wrapping
      .advance(&tenant.lineage, &tenant.namer, u64::MAX)
      .unwrap()
      .is_some(),
    "the whole budget makes the envelope in one advance"
  );
  assert_eq!(
    wrapping.advance(&tenant.lineage, &tenant.namer, u64::MAX),
    Err(EnvelopeError::Spent),
    "a wrap past its envelope"
  );
  let mut refused = Wrapping::new(original, 0);
  assert!(
    matches!(
      refused.advance(&tenant.lineage, &tenant.namer, 1),
      Err(EnvelopeError::Sealed(_))
    ),
    "a segment of no bytes is refused by the seal"
  );
  assert_eq!(
    refused.advance(&tenant.lineage, &tenant.namer, 1),
    Err(EnvelopeError::Spent),
    "a wrap past its refusal"
  );
}

/// A-347: do open one envelope of 64 distinct chunks a slice at a time, at budgets of one byte (a chunk a slice), a
/// chunk and a half, five chunks, more than the whole archive, and the whole budget; expect every one the very archive
/// `open` makes, byte for byte, and the one-byte budget to take a slice a chunk: 64 slices, the chunks left falling by
/// one each. An open's slices change where the work falls, never what it makes.
#[test]
fn an_envelope_opened_in_slices_is_the_whole_opens_archive() {
  let tenant = new_tenant();
  let original = many_chunk_snapshot(SLICED_CHUNKS);
  let envelope = wrap(&original, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  let whole = open(&envelope, &tenant.lineage, &tenant.namer)
    .unwrap()
    .encode();
  let chunk_bytes = u64::try_from(SMALL_CHUNK_LEN).unwrap();
  for budget in [
    1,
    chunk_bytes * 3 / 2,
    chunk_bytes * 5,
    chunk_bytes * 1000,
    u64::MAX,
  ] {
    let mut opening = Opening::new(envelope.clone(), &tenant.lineage, &tenant.namer).unwrap();
    let mut slices = 0u32;
    let mut left = Vec::new();
    let archive = loop {
      slices += 1;
      let opened = opening
        .advance(&tenant.lineage, &tenant.namer, budget)
        .unwrap();
      left.push(opening.chunks_left());
      if let Some(archive) = opened {
        break archive;
      }
    };
    assert_eq!(
      archive.encode(),
      whole,
      "the archive opened in slices of {budget} bytes is the whole open's"
    );
    if budget == 1 {
      assert_eq!(slices, SLICED_CHUNKS, "a slice a chunk");
      let expected: Vec<usize> = (0..usize::try_from(SLICED_CHUNKS).unwrap()).rev().collect();
      assert_eq!(left, expected, "the chunks left fall by one a slice");
    }
  }
}

/// A-347: do advance an open past its archive, and an open under another tenant's keys, which its sealed manifest
/// refuses; expect the archive once and then `Spent`, and the refusal at the start; and an open whose first chunk is
/// refused (the envelope's chunk entries named under another tenant's namer) refused once and then `Spent`: an open
/// that ended never hands back a second archive or a partial one.
#[test]
fn an_open_ends_at_its_archive_or_its_refusal() {
  let tenant = new_tenant();
  let stranger = new_tenant();
  let original = snapshot(&[chunk(1), chunk(2)], 1);
  let envelope = wrap(&original, &tenant.lineage, &tenant.namer, SEGMENT).unwrap();
  let mut opening = Opening::new(envelope.clone(), &tenant.lineage, &tenant.namer).unwrap();
  assert_eq!(
    opening
      .advance(&tenant.lineage, &tenant.namer, u64::MAX)
      .map(|archive| archive.map(|archive| archive.encode())),
    Ok(Some(
      Archive {
        created_unix: 0,
        ..original.clone()
      }
      .encode()
    )),
    "the whole budget opens the archive, its creation time zeroed, in one advance"
  );
  assert_eq!(
    opening.advance(&tenant.lineage, &tenant.namer, u64::MAX),
    Err(EnvelopeError::Spent),
    "an open past its archive"
  );
  assert!(
    Opening::new(envelope.clone(), &stranger.lineage, &stranger.namer).is_err(),
    "another tenant's keys do not open the sealed manifest"
  );
  let mut misnamed = Opening::new(envelope, &tenant.lineage, &tenant.namer).unwrap();
  assert!(
    misnamed
      .advance(&tenant.lineage, &stranger.namer, 1)
      .is_err(),
    "a chunk looked up under another tenant's namer is refused"
  );
  assert_eq!(
    misnamed.advance(&tenant.lineage, &tenant.namer, 1),
    Err(EnvelopeError::Spent),
    "an open past its refusal"
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
