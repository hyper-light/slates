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

/// Shape: a test's store as the daemon sizes it (`slates_api`'s `store_sizing`): a memtable of one trunk node, eight
/// 64 KiB extents (512 KiB), 1 GiB of disk.
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

/// A hunk replacing `start..end` of the base with `data` (Mercurial's `mpatch` form, the store's one edit form).
fn hunk(start: u32, end: u32, data: &[u8]) -> Vec<u8> {
  [
    start.to_be_bytes().as_slice(),
    &end.to_be_bytes(),
    &(data.len() as u32).to_be_bytes(),
    data,
  ]
  .concat()
}

/// A-645 on A-767 through the node's store (main's order of 2026-11-20 19:50: red first on the real store). Do: land a
/// path's whole and three edits as four changesets through a shard's handle, each with its payload and `Texts` section;
/// read its first and second versions back through the handle a piece at a time, and its history; restart the owner
/// (its rows recovered from the log) and read again. Expect: each version's bytes, the history in order, both times.
#[test]
fn a_shard_reads_an_old_file_revision_through_the_owner_across_a_restart() {
  use slates_store::texts::{self, Place, TextEntry};
  let dir = scratch("texts");
  let mut versions: Vec<Vec<u8>> = vec![(0..150_000u32).map(|at| (at % 241) as u8).collect()];
  let mut changesets: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
  for sequence in 1..=4u64 {
    let (payload, base) = if sequence == 1 {
      (versions[0].clone(), None)
    } else {
      let before = versions.last().unwrap().clone();
      let at = ((sequence - 1) * 40_000 + 3) as u32;
      let data = [sequence as u8; 16];
      let mut text = before.clone();
      text[at as usize..at as usize + 16].copy_from_slice(&data);
      versions.push(text);
      (
        hunk(at, at + 16, &data),
        Some(*blake3::hash(&before).as_bytes()),
      )
    };
    let text = versions.last().unwrap();
    let entry = TextEntry {
      path: b"big.bin".to_vec(),
      tool_id: Vec::new(),
      text: *blake3::hash(text).as_bytes(),
      len: text.len() as u64,
      base,
      place: Place {
        payload: *blake3::hash(&payload).as_bytes(),
        offset: 0,
        len: payload.len() as u64,
      },
    };
    changesets.push((payload, texts::encode(&[entry])));
  }
  let read_back = |handle: slates_store::owner::StoreHandle, wanted: Vec<Vec<u8>>| async move {
    let mut read = Vec::new();
    for text in &wanted {
      let identity = *blake3::hash(text).as_bytes();
      let mut bytes = Vec::new();
      loop {
        let piece = handle
          .text(11, identity, bytes.len() as u64)
          .await
          .unwrap()
          .expect("a landed text");
        if piece.is_empty() {
          break;
        }
        bytes.extend(piece);
      }
      read.push(bytes);
    }
    let history = handle
      .path_texts(11, b"big.bin".to_vec(), 0, 10)
      .await
      .unwrap();
    (
      read,
      history
        .into_iter()
        .map(|(sequence, _)| sequence)
        .collect::<Vec<_>>(),
    )
  };
  let owner = start(&dir);
  let handle = owner.handle();
  let runtime = runtime();
  let wanted = vec![versions[0].clone(), versions[1].clone()];
  let first = runtime
    .block_on({
      let wanted = wanted.clone();
      async move {
        for (sequence, (payload, section)) in (1u64..).zip(changesets) {
          handle
            .land(
              header(sequence),
              vec![
                (SectionKind::Payload, payload),
                (SectionKind::Texts, section),
              ],
            )
            .await
            .unwrap();
        }
        read_back(handle, wanted).await
      }
    })
    .unwrap();
  assert_eq!(first, (wanted.clone(), vec![1, 2, 3, 4]));
  owner.stop().unwrap();
  let owner = start(&dir);
  let handle = owner.handle();
  let again = runtime.block_on(read_back(handle, wanted.clone())).unwrap();
  assert_eq!(
    again,
    (wanted, vec![1, 2, 3, 4]),
    "the same after the owner's restart"
  );
  owner.stop().unwrap();
  let _ = std::fs::remove_dir_all(&dir);
}
