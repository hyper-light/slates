//! The pack adapter over a real store's owner (A-767 C5). The store's directory is a real one beside the test binary,
//! in the build output, never `/tmp` (R1).

use std::path::PathBuf;

use slates_git::store::packed::PackSource;
use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::owner::StoreOwner;
use slates_store::store::{PIECE_BYTES, Sizing};
use slates_vcs::ContentId;

use super::{PackFillRefused, StorePacks};

fn store_directory(name: &str) -> PathBuf {
  let binary = std::env::current_exe().unwrap();
  let dir = binary
    .parent()
    .unwrap()
    .join(format!("store-packs-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

/// Shape: a store as the daemon sizes one (a memtable of one trunk node: eight 64 KiB extents), 1 GiB of disk.
fn owner(dir: &std::path::Path) -> StoreOwner {
  let grant = StoreGrant {
    repository: 0,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  let sizing = Sizing {
    memory: 512 << 10,
    disk: 1 << 30,
    extent_bytes: 64 << 10,
    fanout: 8,
  };
  StoreOwner::start(StoreRoot::open(dir, &grant).unwrap(), sizing, 16).unwrap()
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

/// A-767 C5. Do: land a 300,000-byte pack as a changeset's payload; through the adapter (bound: three pieces) read it
/// whole forward as a replay does (a `piece`, on a miss a fill of the next step's range, then the `piece` again); fill
/// the first piece again; ask a range of four pieces; ask a pack the store does not hold. Expect: the bytes equal the
/// pack; every miss is answered by one fill; at most three pieces are held (the first piece let go by the time the end
/// is read, so its fill fetches it again, and a fill of what is held fetches nothing); the four-piece range refused
/// `PastBound`; the unknown pack refused `Missing`.
#[test]
fn a_pack_on_the_store_is_read_forward_a_bounded_set_of_pieces_at_a_time() {
  let dir = store_directory("forward");
  let owner = owner(&dir);
  let handle = owner.handle();
  let pack: Vec<u8> = (0..300_000u32)
    .map(|at| (at.wrapping_mul(31) % 253) as u8)
    .collect();
  let identity = ContentId::of(&pack);
  let len = pack.len() as u64;
  let piece = PIECE_BYTES as u64;
  let landing = pack.clone();
  let outcome = runtime()
    .block_on(async move {
      handle
        .land(
          Header {
            tool: Tool::Git,
            repository: 5,
            sequence: 1,
            judged_over: 0,
            grant: 0,
            work: 0,
            time_ns: 1,
            sections: 0,
          },
          vec![(SectionKind::Payload, landing)],
        )
        .await
        .unwrap();
      let mut packs = StorePacks::new(handle, 3 * PIECE_BYTES);
      let step = 40_000u64;
      let mut read = Vec::new();
      let mut misses = 0u64;
      while (read.len() as u64) < len {
        let at = read.len() as u64;
        let held = packs.piece(&identity, len, at).map(|bytes| {
          let take = bytes.len().min(step as usize);
          bytes[..take].to_vec()
        });
        match held {
          Some(bytes) => read.extend(bytes),
          None => {
            misses += 1;
            packs.fill(&identity, len, at, at + step).await.unwrap();
          }
        }
      }
      let streamed = packs.counts();
      assert!(
        packs.piece(&identity, len, 0).is_none(),
        "the first piece is let go past the bound"
      );
      packs.fill(&identity, len, 0, 1).await.unwrap();
      packs.fill(&identity, len, 0, 1).await.unwrap();
      let refilled = packs.counts();
      let past = packs.fill(&identity, len, 0, 3 * piece + 1).await;
      let unknown = packs.fill(&ContentId::of(b"no such pack"), len, 0, 1).await;
      (read, misses, streamed, refilled, past, unknown)
    })
    .unwrap();
  let (read, misses, streamed, refilled, past, unknown) = outcome;
  assert_eq!(read, pack, "the pack's bytes, read forward");
  let pieces = len.div_ceil(piece);
  assert_eq!(streamed.0, pieces, "each piece fetched once going forward");
  assert_eq!(misses, streamed.0, "every miss answered by one fill");
  assert_eq!(
    (refilled.0 - streamed.0, refilled.1 - streamed.1),
    (1, 1),
    "the let-go piece fetched again, then found held"
  );
  assert!(
    matches!(past, Err(PackFillRefused::PastBound { asked, .. }) if asked == 4 * piece),
    "{past:?}"
  );
  assert!(
    matches!(unknown, Err(PackFillRefused::Missing { at: 0 })),
    "{unknown:?}"
  );
  owner.stop().unwrap();
  let _ = std::fs::remove_dir_all(&dir);
}
