//! The store holds landed history on disk, not in RAM (A-767 item 8; Ada's memory-sizing ruling; the RSS red test of
//! C4-C6's store side). One test a process, so the process's resident memory is this test's: land three payloads of
//! 64 MiB through the owner and read one back a piece at a time; resident memory afterwards stays within the baseline,
//! two of the change in flight (its bytes and one decode window) and the engine's memory budget. Before A-767 every
//! landed byte stayed resident (w-hg: 8,520 MiB after an 84 MB store), so three landings alone would pass the bound.
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_machine::facts::Facts;
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::owner::StoreOwner;
use slates_store::store::Sizing;

/// Shape: one payload's bytes, the change in flight.
const PAYLOAD: usize = 64 << 20;
/// Shape: the payloads landed.
const LANDINGS: u64 = 3;
/// Shape: the engine's memory budget, the store's lent share.
const ENGINE_MEMORY: usize = 8 << 20;

fn scratch(name: &str) -> PathBuf {
  let dir =
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("memory-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

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

/// A-767 item 8. Do: land three 64 MiB payloads through the owner from a shard's task, each moved in and let go once
/// landed, then read one back whole a piece at a time. Expect: resident memory after is at most the baseline plus two
/// payloads and the engine's budget, though 192 MiB landed; and the bytes read back are the ones landed.
#[test]
fn landed_history_is_not_held_in_memory() {
  let dir = scratch("rss");
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(&dir).unwrap(),
    issued_ns: 0,
  };
  let sizing = Sizing {
    memory: ENGINE_MEMORY,
    disk: 4 << 30,
    extent_bytes: 128 << 10,
    fanout: 8,
  };
  let owner = StoreOwner::start(StoreRoot::open(&dir, &grant).unwrap(), sizing, (4, 0)).unwrap();
  let handle = owner.handle();
  let runtime = runtime();
  let baseline = Facts::resident_now().unwrap();
  let (identity, matches) = runtime
    .block_on(async move {
      let mut last = [0u8; 32];
      for sequence in 1..=LANDINGS {
        let payload: Vec<u8> = (0..PAYLOAD)
          .map(|at| (at as u64).wrapping_mul(sequence).wrapping_mul(0x9E37) as u8)
          .collect();
        last = *blake3::hash(&payload).as_bytes();
        let header = Header {
          tool: Tool::Git,
          repository: 1,
          sequence,
          judged_over: sequence - 1,
          grant: 1,
          work: sequence,
          time_ns: sequence,
          sections: 0,
        };
        handle
          .land(header, vec![(SectionKind::Payload, payload)])
          .await
          .unwrap();
      }
      // Read the last payload back whole, a piece at a time, hashing as it comes (nothing kept but the hasher).
      let mut hasher = blake3::Hasher::new();
      let mut read = 0u64;
      while read < PAYLOAD as u64 {
        let piece = handle
          .piece(last, PAYLOAD as u64, read)
          .await
          .unwrap()
          .unwrap();
        read += piece.len() as u64;
        hasher.update(&piece);
      }
      (last, *hasher.finalize().as_bytes() == last)
    })
    .unwrap();
  let after = Facts::resident_now().unwrap();
  let bound = baseline + 2 * PAYLOAD as u64 + ENGINE_MEMORY as u64;
  eprintln!(
    "resident: baseline {} MiB, after {} MiB, bound {} MiB, landed {} MiB",
    baseline >> 20,
    after >> 20,
    bound >> 20,
    (LANDINGS * PAYLOAD as u64) >> 20
  );
  assert!(
    matches,
    "the last payload reads back as landed ({identity:02x?})"
  );
  assert!(
    after <= bound,
    "resident {after} past the bound {bound}: landed history is held in memory"
  );
  owner.stop().unwrap();
}
