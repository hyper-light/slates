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
      form: slates_store::texts::Form::Raw,
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

/// P7's finding (2), main 01a20182's routing of 2026-11-22 00:57 (a sliced landing took a fresh `Vec` a part, each
/// part's bytes copied into it: the import's churn of sub-1 MiB blocks the system allocator keeps). Do: open a sliced
/// landing and send a 200,000-byte payload a piece at a time through one buffer, filling again the buffer each part
/// gives back; commit; read the payload back by identity. Expect: every part answered with the same buffer (its
/// allocation, its capacity kept) emptied for the next piece, never a new one; the payload landed whole.
#[test]
fn a_sliced_landings_part_gives_its_buffer_back_to_be_filled_again() {
  use slates_store::store::PIECE_BYTES;
  let dir = scratch("sliced-buffer");
  let payload: Vec<u8> = (0..200_000u32).map(|at| (at % 241) as u8).collect();
  let identity = *blake3::hash(&payload).as_bytes();
  let len = payload.len() as u64;
  let owner = start(&dir);
  let handle = owner.handle();
  let runtime = runtime();
  let to_land = payload.clone();
  let (kept, read) = runtime
    .block_on(async move {
      let mut landing = handle
        .begin(Header {
          sections: 1,
          ..header(1)
        })
        .await
        .unwrap();
      landing.section(SectionKind::Payload, len).await.unwrap();
      let mut buffer = Vec::with_capacity(PIECE_BYTES);
      let (at, capacity) = (buffer.as_ptr(), buffer.capacity());
      let mut kept = true;
      for piece in to_land.chunks(PIECE_BYTES) {
        buffer.extend_from_slice(piece);
        buffer = landing.part(buffer).await.unwrap();
        kept &= buffer.as_ptr() == at && buffer.capacity() == capacity && buffer.is_empty();
      }
      landing.commit().await.unwrap();
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
      (kept, read)
    })
    .unwrap();
  assert_eq!((kept, read == payload), (true, true));
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

/// Main 01a20182's ruling of 2026-11-22 12:00 (an abandoned run's rows forgotten at its abandonment). Do: from a shard's
/// task, land two changesets through the owner, forget the first, forget it again; stop the owner and start a new one.
/// Expect the first forgetting to forget rows and the second none; the owner then listing only the second changeset,
/// the first's payload unread; and the same after the restart.
#[test]
fn a_changeset_forgotten_through_the_owner_stays_forgotten_across_a_restart() {
  let dir = scratch("forget");
  let payload = b"an abandoned segment's payload".to_vec();
  let identity = *blake3::hash(&payload).as_bytes();
  let len = payload.len() as u64;
  let owner = start(&dir);
  let handle = owner.handle();
  let runtime = runtime();
  let (forgot, again, listed, read) = runtime
    .block_on(async move {
      handle
        .land(
          header(1),
          vec![
            (SectionKind::Record, b"segment".to_vec()),
            (SectionKind::Payload, payload),
          ],
        )
        .await
        .unwrap();
      handle
        .land(header(2), vec![(SectionKind::Record, b"named".to_vec())])
        .await
        .unwrap();
      let forgot = handle.forget(11, 1).await.unwrap();
      let again = handle.forget(11, 1).await.unwrap();
      let listed = handle.changesets(11, 0, 10).await.unwrap();
      let read = handle.piece(identity, len, 0).await.unwrap();
      (forgot, again, listed, read)
    })
    .unwrap();
  assert!(forgot >= 2, "{forgot} rows forgotten");
  assert_eq!(again, 0);
  assert_eq!(
    listed.iter().map(|row| row.sequence).collect::<Vec<_>>(),
    [2]
  );
  assert_eq!(read, None);
  owner.stop().unwrap();
  let owner = start(&dir);
  let handle = owner.handle();
  let (listed, read) = runtime
    .block_on(async move {
      (
        handle.changesets(11, 0, 10).await.unwrap(),
        handle.piece(identity, len, 0).await.unwrap(),
      )
    })
    .unwrap();
  assert_eq!(
    listed.iter().map(|row| row.sequence).collect::<Vec<_>>(),
    [2]
  );
  assert_eq!(read, None);
  owner.stop().unwrap();
}

/// Shape: how long the test waits for a stop before it calls the stop blind: far past the checkpoint of one row on a
/// loaded machine, short of a test that hangs (the fleet test that met this joined for 75 minutes).
const STOP_PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// The owner's stop with a handle still held (main 01a20182's ruling of 2026-11-23: a blind join is a bug; the fleet's
/// `GitFleet::stop_store` joined for 75 minutes, its sample in `StoreOwner::stop` waiting on a daemon's handle). Do: land
/// a changeset through the owner, keep a handle, stop the owner on a plain thread, as the daemon does after its shards
/// stop; then ask through the kept handle; start a new owner. Expect: the stop answers within [`STOP_PATIENCE`], Ok (its
/// checkpoint written); the kept handle's request refused `Gone`; the new owner lists the changeset. Failing first: the
/// stop joined an owner that ends only once every handle is gone, and never answered.
#[test]
fn a_stop_with_a_handle_still_held_ends_the_owner_and_the_handle_is_refused_gone() {
  let dir = scratch("stop-held");
  let owner = start(&dir);
  let kept = owner.handle();
  let handle = owner.handle();
  let runtime = runtime();
  runtime
    .block_on(async move {
      handle
        .land(header(1), vec![(SectionKind::Record, b"heads".to_vec())])
        .await
        .unwrap();
    })
    .unwrap();
  let (stopped, answered) = std::sync::mpsc::channel();
  let stopper = std::thread::spawn(move || {
    let _ = stopped.send(owner.stop().map_err(|error| error.to_string()));
  });
  let outcome = answered.recv_timeout(STOP_PATIENCE);
  assert_eq!(
    outcome,
    Ok(Ok(())),
    "the stop answered while a handle was held, its checkpoint written"
  );
  stopper.join().unwrap();
  let refused = runtime
    .block_on(async move { kept.changesets(11, 0, 10).await.map(|rows| rows.len()) })
    .unwrap();
  assert!(
    matches!(refused, Err(slates_store::owner::OwnerError::Gone)),
    "the kept handle's request after the stop: {refused:?}"
  );
  let owner = start(&dir);
  let handle = owner.handle();
  let listed = runtime
    .block_on(async move { handle.changesets(11, 0, 10).await.unwrap().len() })
    .unwrap();
  assert_eq!(listed, 1, "the changeset landed before the stop");
  owner.stop().unwrap();
}

/// Goal 93 (main's order of 2026-11-25: hg's 57k clone waited 121.35 s on 6,232 store asks, the bulk of the clone, its
/// parts unknown). Do: from a shard's task, land a changeset, then read its payload back by places, a batch an ask, as a
/// stream does; take the thread's read waits twice. Expect: every read ask counted, the landing not; each part (queued
/// behind other work, the owner's wake, its service, its disk reads, the hop back) summing to the whole wait, nanosecond
/// for nanosecond; the disk's share moved by the reads (non-vacuity); none left after the take.
#[test]
fn a_reads_wait_on_the_owner_is_split_into_parts_that_sum_to_it() {
  let dir = scratch("waits");
  let payload: Vec<u8> = (0..200_000u32).map(|at| (at % 251) as u8).collect();
  let identity = *blake3::hash(&payload).as_bytes();
  let owner = start(&dir);
  let handle = owner.handle();
  let runtime = runtime();
  let (read, waits, left) = runtime
    .block_on(async move {
      handle
        .land(header(1), vec![(SectionKind::Payload, payload)])
        .await
        .unwrap();
      let mut read = Vec::new();
      for offset in (0..200_000u64).step_by(50_000) {
        let place = slates_store::texts::Place {
          payload: identity,
          offset,
          len: 50_000,
        };
        read.extend(handle.places(vec![place]).await.unwrap().concat());
      }
      (
        read,
        slates_store::owner::take_read_waits(),
        slates_store::owner::take_read_waits(),
      )
    })
    .unwrap();
  owner.stop().unwrap();
  assert_eq!(read.len(), 200_000, "the payload read back");
  assert_eq!(waits.asks, 4, "every read counted, the landing not");
  let parts = waits.behind_ns + waits.wake_ns + waits.serve_ns + waits.disk_ns + waits.back_ns;
  assert_eq!(
    parts, waits.total_ns,
    "the parts sum to the whole: {waits:?}"
  );
  assert!(waits.disk_ns > 0, "the reads' disk time counted: {waits:?}");
  assert!(waits.most_ns > 0 && waits.most_ns <= waits.total_ns);
  assert_eq!(left.asks, 0, "none left after the take");
}
