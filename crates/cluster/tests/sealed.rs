//! A-92 piece 3 (hyper-raft `docs/seal.md` §4, §7, §12): a chunk sealed for a holder opens only under its lineage key,
//! only whole and in place, only as the chunk its keyed name says; and a holder sees neither its content nor its
//! plaintext identity. Driven through `slates_cluster::sealed`, the form held replicas take.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use hyper_seal::keys::WrappingKey;
use hyper_seal::name::Namer;
use slates_archive::Archive;
use slates_archive::format::Chunk;
use slates_cluster::sealed::{SealedChunk, SealedError, open_chunk, seal_chunk};

/// Shape: keys the tests may hold at once in the process's locked region (a few per test, tests in parallel).
const KEYS: usize = 256;
/// Shape: the segment the tests seal in: one 4 KiB base page, the smallest slates seals at (seal.md §4).
const SEGMENT: u32 = 4096;
/// Format: the generation the test keys are made at.
const GENERATION: u32 = 1;

/// The process's locked key region: made by whichever test asks first, and answered by the region made for the rest.
fn keys() {
  hyper_seal::lock_keys(KEYS).unwrap();
}

/// A tenant's keys for the tests: a lineage key and a naming key under one tenant.
struct Tenant {
  lineage: WrappingKey,
  namer: Namer,
}

fn new_tenant() -> Tenant {
  keys();
  let tenant = WrappingKey::generate(GENERATION).unwrap();
  let (naming, _) = tenant.make_child().unwrap();
  Tenant {
    lineage: WrappingKey::generate(GENERATION).unwrap(),
    namer: Namer::new(&naming).unwrap(),
  }
}

/// Format: the prime the fill pattern's position is reduced by, so a byte's value cycles with no 256-byte period.
const PATTERN_MODULUS: usize = 251;

/// A raw chunk of `len` bytes, each a function of its position and `seed`, so no two chunks share a window.
fn chunk(len: usize, seed: u8) -> Chunk {
  Archive::raw_chunk(
    (0..len)
      .map(|at| {
        let low = u8::try_from(at % PATTERN_MODULUS).unwrap();
        let high = u8::try_from((at / PATTERN_MODULUS) % PATTERN_MODULUS).unwrap();
        low.wrapping_mul(31).wrapping_add(seed).wrapping_add(high)
      })
      .collect(),
  )
}

/// A-92: do seal chunks of one byte, exactly one segment, one segment and a byte, and three segments and a part,
/// then open each; expect the very chunk back, every time.
#[test]
fn a_sealed_chunk_opens_to_the_same_chunk_at_every_length() {
  let tenant = new_tenant();
  let segment = SEGMENT as usize;
  for len in [1, segment - 72, segment, segment + 1, 3 * segment + 100] {
    let original = chunk(len, 7);
    let sealed = seal_chunk(&tenant.lineage, &tenant.namer, &original, SEGMENT).unwrap();
    assert_eq!(
      open_chunk(&tenant.lineage, &tenant.namer, &sealed).unwrap(),
      original,
      "length {len}"
    );
  }
}

/// A-92 (seal.md §7, §13): do seal a chunk; expect its sealed bytes to hold no 16-byte window of its content and not
/// its plaintext identity, and its name to be neither the identity nor a prefix of it.
#[test]
fn a_holder_sees_neither_the_content_nor_the_plaintext_identity() {
  let tenant = new_tenant();
  let original = chunk(3 * SEGMENT as usize, 11);
  let sealed = seal_chunk(&tenant.lineage, &tenant.namer, &original, SEGMENT).unwrap();
  let window = 16;
  for at in (0..original.payload.len() - window).step_by(window) {
    let piece = &original.payload[at..at + window];
    assert!(
      !sealed.bytes.windows(window).any(|w| w == piece),
      "content at {at} is visible"
    );
  }
  assert!(
    !sealed.bytes.windows(32).any(|w| w == original.identity),
    "the identity is visible"
  );
  assert_ne!(
    &sealed.name[..],
    &original.identity[..sealed.name.len()],
    "the name is not the identity"
  );
}

/// A-92 (seal.md §12, hostile input): do flip every single bit of a sealed chunk in turn, and open each; expect every
/// one refused with a typed error, never opened and never a panic.
#[test]
fn every_flipped_bit_is_refused_typed() {
  let tenant = new_tenant();
  let original = chunk(SEGMENT as usize + 300, 3);
  let sealed = seal_chunk(&tenant.lineage, &tenant.namer, &original, SEGMENT).unwrap();
  for byte in 0..sealed.bytes.len() {
    for bit in 0..8 {
      let mut flipped = sealed.clone();
      flipped.bytes[byte] ^= 1 << bit;
      assert!(
        open_chunk(&tenant.lineage, &tenant.namer, &flipped).is_err(),
        "a flip at byte {byte} bit {bit} opened"
      );
    }
  }
}

/// A-92 (STREAM's nonce-based OAE, seal.md §4): do cut a sealed chunk at every length, extend it by a byte and by a
/// whole sealed segment, and splice another chunk's second segment into it; expect each refused.
#[test]
fn a_cut_extended_or_spliced_chunk_is_refused() {
  let tenant = new_tenant();
  let first = seal_chunk(
    &tenant.lineage,
    &tenant.namer,
    &chunk(3 * SEGMENT as usize, 1),
    SEGMENT,
  )
  .unwrap();
  let second = seal_chunk(
    &tenant.lineage,
    &tenant.namer,
    &chunk(3 * SEGMENT as usize, 2),
    SEGMENT,
  )
  .unwrap();
  for len in 0..first.bytes.len() {
    let cut = SealedChunk {
      name: first.name,
      bytes: first.bytes[..len].to_vec(),
    };
    assert!(
      open_chunk(&tenant.lineage, &tenant.namer, &cut).is_err(),
      "cut at {len} opened"
    );
  }
  let mut extended = first.clone();
  extended.bytes.push(0);
  assert!(
    open_chunk(&tenant.lineage, &tenant.namer, &extended).is_err(),
    "an extra byte opened"
  );
  let stored = SEGMENT as usize + 16;
  let header = hyper_seal::stream::HEADER;
  let mut doubled = first.clone();
  doubled
    .bytes
    .extend_from_slice(&first.bytes[header..header + stored]);
  assert!(
    open_chunk(&tenant.lineage, &tenant.namer, &doubled).is_err(),
    "a repeated segment opened"
  );
  let mut spliced = first.clone();
  spliced.bytes[header + stored..header + 2 * stored]
    .copy_from_slice(&second.bytes[header + stored..header + 2 * stored]);
  assert!(
    open_chunk(&tenant.lineage, &tenant.namer, &spliced).is_err(),
    "a spliced segment opened"
  );
}

/// A-92 (seal.md §3, §7): do open a sealed chunk under another lineage key, under another tenant's naming key, and
/// with two chunks' names swapped; expect a key that will not unwrap, and names that do not match, each refused.
#[test]
fn another_key_or_another_name_is_refused() {
  let tenant = new_tenant();
  let other = new_tenant();
  let first = seal_chunk(&tenant.lineage, &tenant.namer, &chunk(500, 5), SEGMENT).unwrap();
  let second = seal_chunk(&tenant.lineage, &tenant.namer, &chunk(500, 6), SEGMENT).unwrap();
  assert_eq!(
    open_chunk(&other.lineage, &tenant.namer, &first),
    Err(SealedError::Seal(hyper_seal::SealError::Unwrap)),
    "another lineage key does not unwrap the data key"
  );
  assert_eq!(
    open_chunk(&tenant.lineage, &other.namer, &first),
    Err(SealedError::Name)
  );
  let relabelled = SealedChunk {
    name: second.name,
    bytes: first.bytes.clone(),
  };
  assert_eq!(
    open_chunk(&tenant.lineage, &tenant.namer, &relabelled),
    Err(SealedError::Name)
  );
}

/// A-92 (seal.md §4, §7): do seal one chunk twice under one tenant, once under another, and the same content stored
/// under another encoding; expect the tenant's two seals to share the keyed name and the very bytes (the data key is
/// keyed by the sealed record, so holders deduplicate ciphertext), the other tenant's name and bytes to differ, and the
/// other encoding — the same identity, another record — to seal under another key to other bytes, so no key ever seals
/// two plaintexts.
#[test]
fn keyed_names_match_within_a_tenant_and_differ_across_tenants() {
  let tenant = new_tenant();
  let other = new_tenant();
  let original = chunk(1000, 9);
  let once = seal_chunk(&tenant.lineage, &tenant.namer, &original, SEGMENT).unwrap();
  let twice = seal_chunk(&tenant.lineage, &tenant.namer, &original, SEGMENT).unwrap();
  let elsewhere = seal_chunk(&other.lineage, &other.namer, &original, SEGMENT).unwrap();
  assert_eq!(once.name, twice.name, "one tenant names one content once");
  assert_eq!(
    once.bytes, twice.bytes,
    "one record seals to one ciphertext"
  );
  assert_ne!(once.name, elsewhere.name, "another tenant's name differs");
  assert_ne!(once.bytes, elsewhere.bytes, "another tenant's bytes differ");
  let recoded = Archive::compressed_chunk(Archive::content(&original).unwrap());
  assert_eq!(recoded.identity, original.identity, "the same content");
  assert_ne!(recoded.encoding, original.encoding, "stored another way");
  let other_record = seal_chunk(&tenant.lineage, &tenant.namer, &recoded, SEGMENT).unwrap();
  assert_eq!(other_record.name, once.name, "named by its content");
  assert_ne!(
    other_record.bytes[..hyper_seal::stream::HEADER],
    once.bytes[..hyper_seal::stream::HEADER],
    "another record, another data key"
  );
  assert_eq!(
    open_chunk(&tenant.lineage, &tenant.namer, &other_record).unwrap(),
    recoded
  );
}
