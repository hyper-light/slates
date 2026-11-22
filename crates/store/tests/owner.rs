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
    &u32::try_from(data.len()).unwrap().to_be_bytes(),
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
      let at = u32::try_from((sequence - 1) * 40_000 + 3).unwrap();
      let data = [u8::try_from(sequence).unwrap(); 16];
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

/// The sliced landing (A-767; promised to w-svn for an import as one changeset). Do: open a sliced landing and, while
/// it is open, land another changeset whole from a second task and list the changesets; send the first's record and its
/// 200,000-byte payload one piece a part, and commit. Then open a third, write part of it and let it go; land a fourth
/// whole. Then open a fifth and send a part past one piece. Restart the owner. Expect: nothing listed while the first is
/// open and the whole landing unanswered until the commit, then landed after it; the payload read back by identity; the
/// dropped third never listed and the fourth landed; the oversized part refused and its landing gone; the same list
/// after the restart.
#[test]
fn a_changeset_lands_in_parts_alone_and_a_dropped_one_leaves_nothing() {
  use slates_rt::futures::{join, spawn, yield_now};
  use slates_store::owner::OwnerError;
  use slates_store::store::{PIECE_BYTES, StoreError};
  let dir = scratch("sliced");
  let payload: Vec<u8> = (0..200_000u32).map(|at| (at % 239) as u8).collect();
  let identity = *blake3::hash(&payload).as_bytes();
  let len = payload.len() as u64;
  let owner = start(&dir);
  let handle = owner.handle();
  let runtime = runtime();
  let to_land = payload.clone();
  let listed = runtime
    .block_on(async move {
      let mut first = handle
        .begin(Header {
          sections: 2,
          ..header(1)
        })
        .await
        .unwrap();
      let (sent, mut whole) = slates_rt::sync::channel(1).unwrap();
      let other = handle.clone();
      let task = spawn(async move {
        let landed = other
          .land(header(2), vec![(SectionKind::Record, b"whole".to_vec())])
          .await;
        let _ = sent.try_send(landed);
      })
      .unwrap();
      yield_now().await;
      assert!(
        handle.changesets(11, 0, 10).await.unwrap().is_empty(),
        "nothing is visible while a landing is open"
      );
      assert!(
        whole.try_recv().unwrap().is_none(),
        "a whole landing waits for the open one"
      );
      first.section(SectionKind::Record, 5).await.unwrap();
      first.part(b"heads".to_vec()).await.unwrap();
      first.section(SectionKind::Payload, len).await.unwrap();
      for part in to_land.chunks(PIECE_BYTES) {
        first.part(part.to_vec()).await.unwrap();
      }
      let first = first.commit().await.unwrap();
      join(task).await.unwrap();
      let second = whole.recv().await.unwrap().unwrap();
      assert!(second.offset > first.offset, "the held landing lands after");
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
      assert_eq!(read, to_land, "the payload by identity");

      let mut dropped = handle
        .begin(Header {
          sections: 1,
          ..header(3)
        })
        .await
        .unwrap();
      dropped.section(SectionKind::Record, 100).await.unwrap();
      dropped.part(vec![3; 50]).await.unwrap();
      drop(dropped);
      handle
        .land(header(4), vec![(SectionKind::Record, b"after".to_vec())])
        .await
        .unwrap();

      let mut oversized = handle
        .begin(Header {
          sections: 1,
          ..header(5)
        })
        .await
        .unwrap();
      let past = PIECE_BYTES + 1;
      oversized
        .section(SectionKind::Payload, past as u64)
        .await
        .unwrap();
      assert!(matches!(
        oversized.part(vec![5; past]).await,
        Err(OwnerError::Store(StoreError::Sizing(_)))
      ));
      assert!(matches!(
        oversized.commit().await,
        Err(OwnerError::Store(StoreError::NoLanding))
      ));
      handle.changesets(11, 0, 10).await.unwrap()
    })
    .unwrap();
  let sequences = |rows: &[slates_store::store::Changeset]| {
    rows.iter().map(|row| row.sequence).collect::<Vec<_>>()
  };
  assert_eq!(sequences(&listed), [1, 2, 4]);
  owner.stop().unwrap();

  let owner = start(&dir);
  let handle = owner.handle();
  let again = runtime
    .block_on(async move {
      let listed = handle.changesets(11, 0, 10).await.unwrap();
      let head = handle.piece(identity, len, 0).await.unwrap().unwrap();
      (listed, head)
    })
    .unwrap();
  assert_eq!(sequences(&again.0), [1, 2, 4]);
  assert_eq!(again.1, payload[..again.1.len()]);
  owner.stop().unwrap();
}

/// Goal 93, jupiter's red (w-k90-win, 2026-11-21: the owner thread gone with no word, every `repository create` answered
/// `Overloaded`). Do: start an owner on a sizing the engine refuses (no memory, no disk), so its store cannot open; from a
/// shard's task land a changeset and list changesets through it. Expect: each answered the store's own refusal, saying
/// it could not be opened and why (here the sizing), never `Gone`; and the owner's stop answers the open's error.
#[test]
fn an_owner_whose_store_cannot_open_answers_every_request_why() {
  let dir = scratch("unopened");
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(&dir).unwrap(),
    issued_ns: 0,
  };
  let refused = Sizing {
    memory: 0,
    disk: 0,
    extent_bytes: 64 << 10,
    fanout: 8,
  };
  let owner = StoreOwner::start(StoreRoot::open(&dir, &grant).unwrap(), refused, 16).unwrap();
  let handle = owner.handle();
  let runtime = runtime();
  let (landed, listed) = runtime
    .block_on(async move {
      let landed = handle
        .land(header(1), vec![(SectionKind::Record, b"heads".to_vec())])
        .await
        .map(|_| ())
        .map_err(|error| error.to_string());
      let listed = handle
        .changesets(11, 0, 10)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string());
      (landed, listed)
    })
    .unwrap();
  for (what, answer) in [("the landing", landed), ("the listing", listed)] {
    let reason = answer.expect_err(what);
    assert!(
      reason.contains("could not be opened") && reason.contains("sizing"),
      "{what} answered the store's own refusal: {reason}"
    );
  }
  assert!(owner.stop().is_err(), "the stop answers the open's error");
}
