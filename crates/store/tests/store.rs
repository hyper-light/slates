//! The store on the host's real disk under a standing grant (A-767 C3): changesets landed, their rows read back, and
//! the rows recovered from the log after a reopen, with and without a checkpoint. The directories are real ones in the
//! build output (`CARGO_TARGET_TMPDIR`), never `/tmp` (R1).
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::store::{Sizing, Store};

fn scratch(name: &str) -> PathBuf {
  let dir =
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("store-{name}-{}", std::process::id()));
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
    },
  )
  .unwrap()
}

/// Lands one changeset of `repository` at `sequence`: a record section and a payload.
fn land(store: &mut Store, repository: u64, sequence: u64, payload: &[u8]) -> [u8; 32] {
  let mut record = store
    .begin(Header {
      tool: Tool::Git,
      repository,
      sequence,
      judged_over: sequence.saturating_sub(1),
      grant: 1,
      work: 500 + sequence,
      time_ns: sequence,
      sections: 2,
    })
    .unwrap();
  record.section(SectionKind::Record, 4).unwrap();
  record.piece(b"refs").unwrap();
  record
    .section(SectionKind::Payload, payload.len() as u64)
    .unwrap();
  record.piece(payload).unwrap();
  let located = record.commit().unwrap();
  store.applied(&located).unwrap();
  located.identity
}

/// Do: land changesets of two repositories, read each repository's rows, read a payload back through its row. Expect:
/// each repository sees its own changesets in sequence order, and the bytes are the ones landed.
#[test]
fn landed_changesets_are_read_back_by_repository() {
  let dir = scratch("read");
  let mut store = open(&dir);
  let a1 = land(&mut store, 1, 1, b"first");
  let b1 = land(&mut store, 2, 1, b"other");
  let a2 = land(&mut store, 1, 2, b"second");
  let rows = store.changesets(1, 0, 10).unwrap();
  assert_eq!(
    rows
      .iter()
      .map(|row| (row.sequence, row.identity))
      .collect::<Vec<_>>(),
    [(1, a1), (2, a2)]
  );
  let others = store.changesets(2, 0, 10).unwrap();
  assert_eq!(
    others.iter().map(|row| row.identity).collect::<Vec<_>>(),
    [b1]
  );
  assert_eq!(
    store.changesets(1, 2, 10).unwrap().len(),
    1,
    "from a sequence on"
  );
  let sections = store.sections(&rows[1]).unwrap();
  let payload = sections
    .iter()
    .find(|section| section.kind == SectionKind::Payload)
    .unwrap();
  let mut read = vec![0u8; payload.len as usize];
  store.log().read_at(&mut read, payload.offset).unwrap();
  assert_eq!(read, b"second");
}

/// Do: land changesets, drop the store without a checkpoint, reopen. Expect: every row is there again, replayed from
/// the log.
#[test]
fn rows_are_recovered_from_the_log_without_a_checkpoint() {
  let dir = scratch("replay");
  let identities: Vec<[u8; 32]> = {
    let mut store = open(&dir);
    (1..=20)
      .map(|sequence| land(&mut store, 3, sequence, &sequence.to_le_bytes()))
      .collect()
  };
  let mut store = open(&dir);
  let rows = store.changesets(3, 0, 100).unwrap();
  assert_eq!(
    rows.iter().map(|row| row.identity).collect::<Vec<_>>(),
    identities
  );
}

/// Do: land, checkpoint, land more, drop, reopen. Expect: the rows before the checkpoint come from the engine and the
/// rows after it from the log's replay: all of them, once each, in order.
#[test]
fn a_checkpoint_and_the_log_after_it_recover_every_row_once() {
  let dir = scratch("checkpoint");
  let identities: Vec<[u8; 32]> = {
    let mut store = open(&dir);
    let mut identities: Vec<[u8; 32]> = (1..=10)
      .map(|sequence| land(&mut store, 4, sequence, b"before"))
      .collect();
    store.checkpoint().unwrap();
    identities.extend((11..=15).map(|sequence| land(&mut store, 4, sequence, b"after")));
    identities
  };
  let mut store = open(&dir);
  let rows = store.changesets(4, 0, 100).unwrap();
  assert_eq!(
    rows.iter().map(|row| row.sequence).collect::<Vec<_>>(),
    (1..=15).collect::<Vec<_>>()
  );
  assert_eq!(
    rows.iter().map(|row| row.identity).collect::<Vec<_>>(),
    identities
  );
}
