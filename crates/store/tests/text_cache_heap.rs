//! The store owner's texts made from their chains kept off the system heap (goal 93, the 410 MiB bar; main 01a20182's
//! order of 2026-11-23 on arena-11 and arena-12): its own binary, so the slates binary's allocator ([`LargeMapped`]) is
//! the global one and its live figures read what the cache holds. A daemon's push of Mercurial's own history put 83% of
//! the bytes under the mapped floor that lived a second or more in this cache (`crates/server/tests/hg_push_lifetimes.rs`:
//! 3.47 GB sampled, 195 MB of it alive when the push ended), each text its own buffer, freed oldest first as newer came:
//! the blocks glibc keeps.
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::print_stdout
)]

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_mem::large::{LargeMapped, heap};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::staging::Staging;
use slates_store::store::{Contents, Sizing, Store};
use slates_store::texts::{self, Place, TextCache, TextEntry};

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: a text's length, under every host's mapped floor (256 KiB on x86_64, 1 MiB on Apple silicon), as most of a
/// history's files are.
const TEXT: usize = 16 << 10;
/// Shape: the versions of the one path, each an edit of the one before.
const VERSIONS: u64 = 400;
/// Shape: the off-heap test's fixed bound, 8 MiB: past the chain's 6.4 MB, so the cache holds it all.
const CHAIN_BOUND: u64 = 8 << 20;

fn scratch(name: &str) -> PathBuf {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
    .join(format!("text-cache-heap-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

fn open(dir: &Path) -> Store {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  let root = StoreRoot::open(dir, &grant).unwrap();
  Store::open(
    root,
    Sizing {
      memory: 8 << 20,
      disk: 1 << 30,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
  .unwrap()
}

/// A hunk replacing `start..end` of the base with `data` (Mercurial's `mpatch` form).
fn hunk(start: u32, end: u32, data: &[u8]) -> Vec<u8> {
  [
    start.to_be_bytes().as_slice(),
    &end.to_be_bytes(),
    &(data.len() as u32).to_be_bytes(),
    data,
  ]
  .concat()
}

/// Lands [`VERSIONS`] changesets of repository 1, each a version of one path: a whole first, then ten bytes replaced a
/// version. Each version's identity, in order.
fn land_chain(store: &mut Store) -> Vec<[u8; 32]> {
  let staging = Staging::new(1 << 20);
  let mut text: Vec<u8> = (0..TEXT).map(|at| (at % 251) as u8).collect();
  let mut identities = Vec::new();
  let mut base = None;
  for sequence in 1..=VERSIONS {
    let payload = if base.is_none() {
      text.clone()
    } else {
      let at = ((sequence * 37) % (TEXT as u64 - 10)) as u32;
      let data = [sequence as u8; 10];
      text[at as usize..at as usize + 10].copy_from_slice(&data);
      hunk(at, at + 10, &data)
    };
    let identity = *blake3::hash(&text).as_bytes();
    let section = texts::encode(&[TextEntry {
      path: b"long.txt".to_vec(),
      tool_id: Vec::new(),
      text: identity,
      len: text.len() as u64,
      base,
      place: Place {
        payload: *blake3::hash(&payload).as_bytes(),
        offset: 0,
        len: payload.len() as u64,
      },
    }]);
    store
      .land(
        Header {
          tool: Tool::Mercurial,
          repository: 1,
          sequence,
          judged_over: sequence - 1,
          grant: 1,
          work: sequence,
          time_ns: sequence,
          sections: 0,
        },
        &[
          (SectionKind::Record, Contents::Bytes(b"heads")),
          (SectionKind::Payload, Contents::Bytes(&payload)),
          (SectionKind::Texts, Contents::Bytes(&section)),
        ],
        &staging,
      )
      .unwrap();
    identities.push(identity);
    base = Some(identity);
  }
  identities
}

/// Goal 93's memory bar (main 01a20182, 2026-11-23). Do: land a chain of [`VERSIONS`] versions of one path, 16 KiB
/// each; on a cache bounded past the chain's bytes, read the latest on a cold cache (its whole chain made and kept), then
/// read every version in order, twice, reading the system allocator's figures before and after. Expect: every version's
/// bytes as landed; the cache holding the chain; and the system path's live bytes grown by less than a tenth of what the
/// cache holds, since its texts lie in chunks the mapped path serves, given back whole. Failing first: each text its own
/// buffer on the system heap, the live bytes grown by all the cache holds (6.4 MB here), which, freed oldest first, the
/// system allocator keeps. Measured 2026-11-23 on a MacBook: red 6,606,904 bytes grown with 6,553,600 held; green
/// 90,424 with 6,029,312 held (the index and the counters).
#[test]
fn the_store_keeps_the_texts_it_made_off_the_system_heap() {
  let dir = scratch("off-heap");
  let mut store = open(&dir);
  let identities = land_chain(&mut store);
  let staging = Staging::new(0);
  let mut cache = TextCache::new(CHAIN_BOUND);
  let before = heap();
  let latest = *identities.last().unwrap();
  let len = store
    .text_at(&staging, 1, &latest, 0, &mut cache)
    .unwrap()
    .unwrap()
    .len();
  assert_eq!(len, TEXT);
  for _ in 0..2 {
    for identity in &identities {
      let bytes = store
        .text_at(&staging, 1, identity, 0, &mut cache)
        .unwrap()
        .unwrap();
      assert_eq!(
        *blake3::hash(bytes).as_bytes(),
        *identity,
        "a version's bytes as landed"
      );
    }
  }
  let after = heap();
  let held = cache.held_bytes();
  assert!(
    held >= TEXT as u64 * VERSIONS / 2,
    "the cache holds the chain: {held} bytes"
  );
  let grown = after.system_live.saturating_sub(before.system_live);
  println!(
    "text_cache_heap: the cache holds {held} bytes of texts; the system path's live bytes grew by {grown}"
  );
  assert!(
    grown < held / 10,
    "the system path's live bytes grew by {grown} with the cache holding {held} bytes of texts"
  );
}

/// Shape: the reads at once the bound test's cache is made for: a few seats' walks.
const READS: usize = 4;

/// Goal 93 (main 01a20182's ruling of 2026-11-23: the bound from the work). Do: land the chain; on a cache made for
/// [`READS`] reads at once, read the latest version on a cold cache (its whole chain made), then [`READS`] - 1 other
/// versions, then the latest again. Expect: the cache's reserved bytes never past its bound, and that bound no more than
/// [`READS`] texts and one chunk; texts let go at it (so the cache filled); the latest found again without being made,
/// since it was among the last reads; every version's bytes as landed. Failing first: the bound grown to the chain's
/// bytes, 6.4 MB held after one read.
#[test]
fn the_store_holds_the_texts_its_last_reads_lent_and_no_more() {
  let dir = scratch("bound");
  let mut store = open(&dir);
  let identities = land_chain(&mut store);
  let staging = Staging::new(0);
  let mut cache = TextCache::for_reads(READS);
  let latest = *identities.last().unwrap();
  let read = |store: &mut Store, cache: &mut TextCache, identity: &[u8; 32]| {
    let bytes = store
      .text_at(&staging, 1, identity, 0, cache)
      .unwrap()
      .unwrap();
    assert_eq!(
      *blake3::hash(bytes).as_bytes(),
      *identity,
      "a version's bytes as landed"
    );
    assert!(
      cache.reserved_bytes() <= cache.bound(),
      "reserved {} past the bound {}",
      cache.reserved_bytes(),
      cache.bound()
    );
  };
  read(&mut store, &mut cache, &latest);
  let chunk = slates_mem::large::floor() as u64;
  let most = READS as u64 * TEXT as u64 + chunk;
  println!(
    "text_cache_heap: after the chain's read, bound {} reserved {} let go {} (at most {most})",
    cache.bound(),
    cache.reserved_bytes(),
    cache.let_go
  );
  assert!(
    cache.bound() <= most,
    "the bound {} past {READS} texts and a chunk",
    cache.bound()
  );
  assert!(cache.let_go > 0, "texts let go at the bound");
  for identity in identities.iter().rev().skip(1).take(READS - 1) {
    read(&mut store, &mut cache, identity);
  }
  let made = cache.made;
  read(&mut store, &mut cache, &latest);
  assert_eq!(
    cache.made, made,
    "the latest, among the last reads, found without being made"
  );
}

/// Shape: the chunks the cost measurement makes and rewrites.
const MEASURED_CHUNKS: u32 = 2_000;

/// Goal 93 (main 01a20182's ruling of 2026-11-23: measure a spare chunk before keeping one). Do: make a chunk of the
/// mapped floor, fill it and give it back, [`MEASURED_CHUNKS`] times, as the text cache's churn does; then fill one
/// kept chunk as many times. Print each's nanoseconds a chunk, for the push's count of chunks. `#[ignore]`d: a
/// measurement, run by name.
#[test]
#[ignore = "a measurement: a fresh chunk's cost against a kept one's"]
fn a_fresh_chunk_against_a_kept_one() {
  let floor = slates_mem::large::floor();
  let fill = |chunk: &mut Vec<u8>| {
    chunk.clear();
    chunk.resize(floor, 7);
    std::hint::black_box(chunk.as_slice());
  };
  let began = std::time::Instant::now();
  for _ in 0..MEASURED_CHUNKS {
    let mut chunk = Vec::with_capacity(floor);
    fill(&mut chunk);
    drop(chunk);
  }
  let fresh = began.elapsed() / MEASURED_CHUNKS;
  let mut kept = Vec::with_capacity(floor);
  let began = std::time::Instant::now();
  for _ in 0..MEASURED_CHUNKS {
    fill(&mut kept);
  }
  let reused = began.elapsed() / MEASURED_CHUNKS;
  println!(
    "text_cache_heap: a chunk of {floor} B: fresh (mapped, filled, unmapped) {fresh:?}, kept and refilled {reused:?}"
  );
}
