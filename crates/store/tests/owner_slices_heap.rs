//! A text the store's owner answers in slices, assembled in the asker's own buffer (goal 93, main's churn ruling of
//! 2026-11-25: hg's 57k clone allocated 6.2 GB in the owner's answers, 15.3% of its system churn, each step-sized slice
//! copied into a buffer of its own and copied again into the text it was part of). Its own binary, so the slates
//! binary's allocator ([`LargeMapped`]) is the global one and the process's allocated bytes are the read's: its threads
//! are the test's and the owner's.
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
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::owner::StoreOwner;
use slates_store::store::Sizing;
use slates_store::texts::{self, Place, TextEntry};

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: the text read in slices: several of them, under the allocator's mapped floor.
const TEXT: usize = 300 << 10;
/// Shape: a slice, as a fetch's step asks.
const SLICE: u64 = 64 << 10;
/// Shape: the repository the text is landed in.
const REPOSITORY: u128 = 11;

fn scratch(name: &str) -> PathBuf {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
    .join(format!("owner-slices-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

/// The owner of a test's store in `dir`, sized as the daemon sizes one.
fn start(dir: &Path) -> StoreOwner {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  let sizing = Sizing {
    memory: 512 << 10,
    disk: 1 << 30,
    extent_bytes: 64 << 10,
    fanout: 8,
  };
  StoreOwner::start(StoreRoot::open(dir, &grant).unwrap(), sizing, (16, 0)).unwrap()
}

/// Shape: one shard, a few tasks.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .unwrap()
}

/// Goal 93 (main's churn ruling of 2026-11-25). Do: land a text of [`TEXT`] bytes whole through the owner; read it once
/// (the owner's cache made warm); then read it again in slices of [`SLICE`], each answered appended to the buffer the
/// last answered, reserved whole at the start, reading the process's allocated bytes across. Expect the text as landed,
/// and the read allocating under a quarter of the text: the slices laid in the one buffer, none in a buffer of its own.
/// Red before: each slice copied into a buffer of its own, the whole text allocated again.
#[test]
fn a_text_read_in_slices_through_the_owner_is_assembled_in_the_askers_buffer() {
  let dir = scratch("assembled");
  let payload: Vec<u8> = (0..TEXT).map(|at| (at % 241) as u8).collect();
  let identity = *blake3::hash(&payload).as_bytes();
  let section = texts::encode(&[TextEntry {
    path: b"long.txt".to_vec(),
    tool_id: Vec::new(),
    text: identity,
    len: payload.len() as u64,
    base: None,
    place: Place {
      payload: identity,
      offset: 0,
      len: payload.len() as u64,
    },
    form: texts::Form::Raw,
  }]);
  let owner = start(&dir);
  let handle = owner.handle();
  let to_land = payload.clone();
  let (read, allocated) = runtime()
    .block_on(async move {
      handle
        .land(
          Header {
            tool: Tool::Mercurial,
            repository: REPOSITORY,
            sequence: 1,
            judged_over: 0,
            grant: 1,
            work: 1,
            time_ns: 1,
            sections: 0,
          },
          vec![
            (SectionKind::Record, b"heads".to_vec()),
            (SectionKind::Payload, to_land),
            (SectionKind::Texts, section),
          ],
        )
        .await
        .unwrap();
      let warm = handle
        .text_slice_into(REPOSITORY, identity, (0, SLICE), Vec::new())
        .await
        .unwrap()
        .unwrap();
      assert_eq!(warm.len() as u64, SLICE);
      let mut text = Vec::with_capacity(TEXT);
      let before = heap().system_allocated;
      loop {
        let at = text.len();
        text = handle
          .text_slice_into(REPOSITORY, identity, (at as u64, SLICE), text)
          .await
          .unwrap()
          .unwrap();
        if ((text.len() - at) as u64) < SLICE {
          break;
        }
      }
      let allocated = heap().system_allocated - before;
      (text, allocated)
    })
    .unwrap();
  owner.stop().unwrap();
  println!(
    "owner_slices_heap: a text of {TEXT} B read in slices of {SLICE} B allocated {allocated} B"
  );
  assert!(read == payload, "the text as landed");
  assert!(
    allocated < TEXT as u64 / 4,
    "the read allocated {allocated} B for a text of {TEXT} B"
  );
}

/// Shape: the texts the walk test lands and leaves part-way, each [`TEXT`] bytes.
const LEFT_TEXTS: usize = 3;

/// The texts of `count` distinct payloads of [`TEXT`] bytes landed as changeset `sequence`'s, with their identities.
fn left_texts(count: usize) -> (Vec<[u8; 32]>, Vec<(SectionKind, Vec<u8>)>) {
  let mut identities = Vec::new();
  let mut entries = Vec::new();
  let mut payload = Vec::new();
  for at in 0..count {
    let text: Vec<u8> = (0..TEXT).map(|byte| ((byte + at) % 239) as u8).collect();
    let identity = *blake3::hash(&text).as_bytes();
    entries.push(TextEntry {
      path: format!("left-{at}.txt").into_bytes(),
      tool_id: Vec::new(),
      text: identity,
      len: text.len() as u64,
      base: None,
      place: Place {
        payload: identity,
        offset: 0,
        len: text.len() as u64,
      },
      form: texts::Form::Raw,
    });
    identities.push(identity);
    payload.push((SectionKind::Payload, text));
  }
  let mut sections = vec![(SectionKind::Record, b"heads".to_vec())];
  sections.extend(payload);
  sections.push((SectionKind::Texts, texts::encode(&entries)));
  (identities, sections)
}

/// Goal 93 (main's ruling of 2026-11-25: after hg's 57k push the text cache's chunks held 32 MB live, `texts.rs:708`, and
/// nothing reported its reserved bytes, bound or walks). Do: land [`LEFT_TEXTS`] texts of [`TEXT`] bytes; through the
/// owner, read each one's first slice of [`SLICE`] and leave it, as a caller gone mid-walk; read one more slice and note
/// what the owner's cache was as it took it (`owner::text_cache_seen`); then read each text to its end and note it
/// again. Expect the figures to reach the asker (non-vacuity of the memory status's rows): after the reads left
/// part-way, every one a walk in flight, their texts' bytes walked and the bound holding them; once each is read to
/// its end, no walk in flight. The lead it shows: a read its caller leaves part-way holds the cache's bound up until
/// more walks than the owner's room for waiting reads evict it.
#[test]
fn the_owners_text_cache_is_seen_by_its_asker_and_a_read_left_partway_holds_its_bound() {
  let dir = scratch("left");
  let (identities, sections) = left_texts(LEFT_TEXTS);
  let owner = start(&dir);
  let handle = owner.handle();
  let (left, ended) = runtime()
    .block_on(async move {
      handle
        .land(
          Header {
            tool: Tool::Mercurial,
            repository: REPOSITORY,
            sequence: 1,
            judged_over: 0,
            grant: 1,
            work: 1,
            time_ns: 1,
            sections: 0,
          },
          sections,
        )
        .await
        .unwrap();
      for identity in &identities {
        handle
          .text_slice_into(REPOSITORY, *identity, (0, SLICE), Vec::new())
          .await
          .unwrap()
          .unwrap();
      }
      let look = identities[0];
      let _ = handle
        .text_slice_into(REPOSITORY, look, (0, SLICE), Vec::new())
        .await;
      let left = slates_store::owner::text_cache_seen();
      for identity in &identities {
        let _ = handle
          .text_slice_into(REPOSITORY, *identity, (SLICE, TEXT as u64), Vec::new())
          .await;
      }
      let _ = handle
        .text_slice_into(REPOSITORY, look, (0, 1), Vec::new())
        .await;
      let _ = handle
        .text_slice_into(REPOSITORY, look, (1, TEXT as u64), Vec::new())
        .await;
      (left, slates_store::owner::text_cache_seen())
    })
    .unwrap();
  owner.stop().unwrap();
  println!(
    "owner_slices_heap: {LEFT_TEXTS} reads left part-way: {left:?}; each read to its end: {ended:?}"
  );
  assert_eq!(
    left.walks, LEFT_TEXTS as u64,
    "each read left part-way a walk in flight"
  );
  assert_eq!(
    left.walk_bytes,
    (LEFT_TEXTS * TEXT) as u64,
    "their texts walked"
  );
  assert!(
    left.bound >= left.walk_bytes,
    "the bound holding them: {left:?}"
  );
  assert_eq!(
    ended.walks, 1,
    "the walks read to their end gone, the look's own begun: {ended:?}"
  );
}
