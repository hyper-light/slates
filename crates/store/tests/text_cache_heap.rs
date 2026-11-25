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

/// Shape: the reads at once a CI runner's owner derives room for (`clients_per_shard` x shards): far more than are ever
/// reading at once.
const MANY_READS: usize = 1_000;

/// Goal 93 (main 01a20182's arena-13, CI 45193998198: 646,410,240 bytes of mapped blocks resident after push-all and
/// after idle, against 68,661,248 before; the bound was the distinct texts of the last `waiting` reads, and on a CI
/// runner that is hundreds). Do: on a cache made for [`MANY_READS`] reads at once, read every version of the chain on a
/// cold cache, each a walk of one piece finished by its read, as a push's reads of its bases are. Expect: the cache's
/// reserved bytes never past one chunk and a text, since no walk stays in flight; the reads lending every version's
/// bytes as landed. Failing first: the bound grown with every distinct text read, the whole chain's 6.4 MB.
#[test]
fn the_store_holds_the_walks_in_flight_not_the_reads_that_could_be_waiting() {
  let dir = scratch("walks");
  let mut store = open(&dir);
  let identities = land_chain(&mut store);
  let staging = Staging::new(0);
  let mut cache = TextCache::for_reads(MANY_READS);
  let most = slates_mem::large::floor() as u64 + TEXT as u64;
  let mut largest = 0;
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
    largest = largest.max(cache.reserved_bytes());
  }
  println!(
    "text_cache_heap: {MANY_READS} reads' room, every version read: reserved at most {largest} (bound {most})"
  );
  assert!(
    largest <= most,
    "reserved {largest} past one chunk and a text, {most}, with no walk in flight"
  );
}

/// Shape: a text walked in several pieces: past three of the owner's pieces.
const BIG_TEXT: usize = 3 * slates_store::store::PIECE_BYTES + 1_000;

/// Lands one more changeset of repository 1 at `sequence`, a whole text of [`BIG_TEXT`] bytes on its own path: its
/// identity.
fn land_big(store: &mut Store, sequence: u64) -> [u8; 32] {
  let staging = Staging::new(1 << 20);
  let text: Vec<u8> = (0..BIG_TEXT).map(|at| (at % 241) as u8).collect();
  let identity = *blake3::hash(&text).as_bytes();
  let section = texts::encode(&[TextEntry {
    path: b"big.bin".to_vec(),
    tool_id: Vec::new(),
    text: identity,
    len: text.len() as u64,
    base: None,
    place: Place {
      payload: *blake3::hash(&text).as_bytes(),
      offset: 0,
      len: text.len() as u64,
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
        (SectionKind::Payload, Contents::Bytes(&text)),
        (SectionKind::Texts, Contents::Bytes(&section)),
      ],
      &staging,
    )
    .unwrap();
  identity
}

/// Goal 93 (the walks-in-flight bound). Do: land the chain and a text of several pieces; begin the big text's walk at
/// its first piece, read every version of the chain between, then read the big text's next pieces and its last. Expect:
/// the big text found at each later piece without being made again (its walk held it through the other reads), every
/// piece's bytes the text's; and once its last piece is read, the cache back within one chunk and a text.
#[test]
fn a_walk_in_flight_keeps_its_text_through_other_reads() {
  let dir = scratch("walk");
  let mut store = open(&dir);
  let identities = land_chain(&mut store);
  let big = land_big(&mut store, VERSIONS + 1);
  let staging = Staging::new(0);
  let mut cache = TextCache::for_reads(MANY_READS);
  let piece = slates_store::store::PIECE_BYTES as u64;
  let first = store
    .text_at(&staging, 1, &big, 0, &mut cache)
    .unwrap()
    .unwrap()
    .len();
  assert_eq!(first, BIG_TEXT);
  for identity in &identities {
    store
      .text_at(&staging, 1, identity, 0, &mut cache)
      .unwrap()
      .unwrap();
  }
  let made = cache.made;
  let mut offset = piece;
  while offset < BIG_TEXT as u64 {
    let rest = store
      .text_at(&staging, 1, &big, offset, &mut cache)
      .unwrap()
      .unwrap();
    assert_eq!(
      rest.len() as u64,
      BIG_TEXT as u64 - offset,
      "the walk's piece from {offset}"
    );
    assert_eq!(
      rest[0],
      (offset % 241) as u8,
      "the walk's bytes at {offset}"
    );
    offset += piece;
  }
  assert_eq!(
    cache.made, made,
    "the big text, walked, found without being made again"
  );
  let most = slates_mem::large::floor() as u64 + BIG_TEXT as u64;
  assert!(
    cache.reserved_bytes() <= most,
    "reserved {} past one chunk and a text, {most}, once the walk ended",
    cache.reserved_bytes()
  );
}

/// Shape: the distinct texts of several pieces the whole-read test lands, each [`BIG_TEXT`] bytes: past a chunk and a
/// text together, so walks left in flight show past the bound.
const WHOLE_TEXTS: u64 = 24;

/// Lands [`WHOLE_TEXTS`] more changesets of repository 1 from `sequence` on, each a whole text of [`BIG_TEXT`] bytes of
/// its own on its own path, as a history's manifests are: their identities, in order.
fn land_whole_texts(store: &mut Store, sequence: u64) -> Vec<[u8; 32]> {
  let staging = Staging::new(1 << 20);
  (0..WHOLE_TEXTS)
    .map(|nth| {
      let text: Vec<u8> = (0..BIG_TEXT)
        .map(|at| ((at as u64 + nth) % 239) as u8)
        .collect();
      let identity = *blake3::hash(&text).as_bytes();
      let section = texts::encode(&[TextEntry {
        path: format!("manifest-{nth}").into_bytes(),
        tool_id: Vec::new(),
        text: identity,
        len: text.len() as u64,
        base: None,
        place: Place {
          payload: identity,
          offset: 0,
          len: text.len() as u64,
        },
      }]);
      let at = sequence + nth;
      store
        .land(
          Header {
            tool: Tool::Mercurial,
            repository: 1,
            sequence: at,
            judged_over: at - 1,
            grant: 1,
            work: at,
            time_ns: at,
            sections: 0,
          },
          &[
            (SectionKind::Record, Contents::Bytes(b"heads")),
            (SectionKind::Payload, Contents::Bytes(&text)),
            (SectionKind::Texts, Contents::Bytes(&section)),
          ],
          &staging,
        )
        .unwrap();
      identity
    })
    .collect()
}

/// Goal 93 (main's round at 15b4f1061: hg's 57k clone peaked at 10,416 MiB and held 10,013 MiB after it, 4,792 mapped
/// blocks of about a chunk each still live; at 2ed3eb139 it peaked at 5.7-6.0 GB). Since 9a5587e78 the owner makes a
/// fetch's batch of texts whole in one request, each read once at its start and taken whole, so for a text longer than
/// a piece (a manifest of Mercurial's history, 100-250 KB) the cache took that read for a walk's first piece, kept it in
/// flight with no later piece ever to end it, and grew its bound by the text, up to the owner's room for waiting reads
/// (`clients_per_shard` x shards). Do: land the chain and [`WHOLE_TEXTS`] distinct texts of several pieces; on a cache
/// made for [`MANY_READS`] reads at once, read each whole ([`Store::text_whole`]) as the owner's batch does. Expect:
/// every text's bytes as landed, whole; and the cache's bound and reserved bytes never past one chunk and a text, since a
/// text read whole leaves no walk in flight. Failing first: the bound grown by every text read whole, about 4.7 MB here.
#[test]
fn a_text_read_whole_leaves_no_walk_in_flight() {
  let dir = scratch("whole");
  let mut store = open(&dir);
  land_chain(&mut store);
  let wholes = land_whole_texts(&mut store, VERSIONS + 1);
  let staging = Staging::new(0);
  let mut cache = TextCache::for_reads(MANY_READS);
  let most = slates_mem::large::floor() as u64 + BIG_TEXT as u64;
  let (mut largest_bound, mut largest_reserved) = (0, 0);
  for identity in &wholes {
    let bytes = store
      .text_whole(&staging, 1, identity, &mut cache)
      .unwrap()
      .unwrap();
    assert_eq!(bytes.len(), BIG_TEXT, "the text whole");
    assert_eq!(
      *blake3::hash(bytes).as_bytes(),
      *identity,
      "a text's bytes as landed"
    );
    largest_bound = largest_bound.max(cache.bound());
    largest_reserved = largest_reserved.max(cache.reserved_bytes());
  }
  println!(
    "text_cache_heap: {WHOLE_TEXTS} texts of {BIG_TEXT} B read whole: bound at most {largest_bound}, reserved at most \
     {largest_reserved} (at most {most})"
  );
  assert!(
    largest_bound <= most && largest_reserved <= most,
    "bound {largest_bound} or reserved {largest_reserved} past one chunk and a text, {most}, with every text read whole"
  );
}
