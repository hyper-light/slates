//! The store's owner thread, reached from a shard's task (A-767 item 4): landings answered once durable, changesets
//! and pieces read back, and a stop that checkpoints, so a new owner finds every row. Real directories in the build
//! output (`CARGO_TARGET_TMPDIR`), never `/tmp` (R1).
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::owner::StoreOwner;
use slates_store::store::Sizing;

fn scratch(name: &str) -> PathBuf {
  let dir =
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("owner-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

/// Shape: a test's store: 8 MiB of memory, 1 GiB of disk, 128 KiB extents, fanout 8.
fn start(dir: &Path) -> StoreOwner {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  let sizing = Sizing {
    memory: 8 << 20,
    disk: 1 << 30,
    extent_bytes: 128 << 10,
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

fn header(sequence: u64) -> Header {
  Header {
    tool: Tool::Mercurial,
    repository: 11,
    sequence,
    judged_over: sequence.saturating_sub(1),
    grant: 1,
    work: sequence,
    time_ns: sequence,
    sections: 0,
  }
}

/// Do: from a shard's task, land two changesets through the owner, list them, read a payload back by identity a piece
/// at a time; stop the owner; start a new one and read again. Expect: both landed in order, the payload's bytes, and
/// the same after the restart (the stop's checkpoint and the log).
#[test]
fn a_shard_lands_and_reads_through_the_owner_across_a_restart() {
  let dir = scratch("restart");
  let payload: Vec<u8> = (0..200_000u32).map(|at| (at % 251) as u8).collect();
  let identity = *blake3::hash(&payload).as_bytes();
  let len = payload.len() as u64;
  let owner = start(&dir);
  let handle = owner.handle();
  let runtime = runtime();
  let to_land = payload.clone();
  let (first, listed, read) = runtime
    .block_on(async move {
      let first = handle
        .land(
          header(1),
          vec![
            (SectionKind::Record, b"heads".to_vec()),
            (SectionKind::Payload, to_land),
          ],
        )
        .await
        .unwrap();
      handle
        .land(header(2), vec![(SectionKind::Record, b"more".to_vec())])
        .await
        .unwrap();
      let listed = handle.changesets(11, 0, 10).await.unwrap();
      let mut read = Vec::new();
      while (read.len() as u64) < len {
        read.extend(
          handle
            .piece(identity, len, read.len() as u64)
            .await
            .unwrap()
            .unwrap(),
        );
      }
      // A replay's reading: the first changeset's sections, its record inline, its payload by place a piece at a time.
      let sections = handle.sections(listed[0]).await.unwrap();
      assert_eq!(sections.len(), 2);
      assert_eq!(sections[0].section.kind, SectionKind::Record);
      assert_eq!(sections[0].inline.as_deref(), Some(&b"heads"[..]));
      assert_eq!(sections[1].section.kind, SectionKind::Payload);
      assert!(
        sections[1].inline.is_none(),
        "a payload past one piece is read by its place"
      );
      let mut by_place = Vec::new();
      let payload = sections[1].section;
      while (by_place.len() as u64) < payload.len {
        let at = payload.offset + by_place.len() as u64;
        by_place.extend(
          handle
            .read(at, payload.len - by_place.len() as u64)
            .await
            .unwrap(),
        );
      }
      assert_eq!(by_place, read, "the same bytes by place as by identity");
      (first, listed, read)
    })
    .unwrap();
  assert_eq!(
    listed.iter().map(|row| row.sequence).collect::<Vec<_>>(),
    [1, 2]
  );
  assert_eq!(listed[0].identity, first.identity);
  assert_eq!(read, payload);
  owner.stop().unwrap();

  let owner = start(&dir);
  let handle = owner.handle();
  let again = runtime
    .block_on(async move {
      let listed = handle.changesets(11, 0, 10).await.unwrap();
      let head = handle.piece(identity, len, 0).await.unwrap().unwrap();
      (listed.len(), head)
    })
    .unwrap();
  assert_eq!(again.0, 2);
  assert_eq!(again.1, payload[..again.1.len()]);
  owner.stop().unwrap();
}
