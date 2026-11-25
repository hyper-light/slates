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
