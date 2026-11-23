//! A-767 C3. A checkpoint written before payload places were indexed can already have lost a live
//! lookup. The surviving changeset rows identify precisely which places to rebuild.

use hyper_block::buf::Alignment;
use hyper_block::sim::SimFile;
use mantle_engine::store::{Config, IntoFile, MIN_PAGE};
use mantle_engine::trunk::TrunkConfig;

use super::*;
use crate::log::{ChangesetLog, Header, Located, Tool};
use crate::store::put_changeset;

fn file(seed: u64) -> SimFile {
  SimFile::new(Alignment::BYTE, Alignment::BYTE, seed).unwrap()
}

fn append(log: &mut ChangesetLog<SimFile>, sequence: u64, bytes: &[u8]) -> Located {
  let mut record = log
    .begin(Header {
      tool: Tool::Git,
      repository: 1,
      sequence,
      judged_over: 0,
      grant: 1,
      work: sequence,
      time_ns: sequence,
      sections: 1,
    })
    .unwrap();
  record
    .section(SectionKind::Payload, bytes.len() as u64)
    .unwrap();
  record.piece(bytes).unwrap();
  record.commit().unwrap()
}

/// The bytes `identity`'s payload lookup names in `log`, or `None` where `rows` holds no lookup for it. A refused read
/// fails the test ([`lookup`], [`bytes_at`]), never reads as no lookup.
fn read(
  rows: &mut ShardDb<SimFile>,
  log: &ChangesetLog<SimFile>,
  identity: &Identity,
) -> Option<Vec<u8>> {
  let (found, value) = lookup(rows, identity);
  found.then(|| bytes_at(log, &value))
}

/// Whether `rows` holds `identity`'s payload lookup, and its value.
fn lookup(rows: &mut ShardDb<SimFile>, identity: &Identity) -> (bool, Vec<u8>) {
  let mut value = Vec::new();
  let found = rows.get(&content_key(identity), &mut value).unwrap();
  (found, value)
}

/// The bytes a payload lookup's `value` names in `log`.
fn bytes_at(log: &ChangesetLog<SimFile>, value: &[u8]) -> Vec<u8> {
  let (offset, len) = decode_content(value).unwrap();
  let mut bytes = vec![0; usize::try_from(len).unwrap()];
  log.read_at(&mut bytes, offset).unwrap();
  bytes
}

/// A-767 C3. Rebuild a checkpoint missing a shared payload's lookup and holding a replaced
/// record's stale lookup. Reopen it and forget each surviving copy. Only still-owned bytes read.
#[test]
fn an_old_payload_checkpoint_recovers_only_the_surviving_copies_once() {
  let config = Config {
    page_size: MIN_PAGE,
    extent_pages: 32,
    max_extents: 256,
  };
  let trunk = TrunkConfig {
    fanout: 8,
    leaf_entries: 1024,
  };
  let memory = 1 << 20;
  let mut rows = ShardDb::create(file(1), config, memory, trunk).unwrap();
  let mut log = ChangesetLog::recover(file(2), 0, |_, _| {}).unwrap();
  let shared = b"owned by both surviving changesets";
  let replaced = b"only in the replaced changeset";
  let first = append(&mut log, 1, shared);
  let second = append(&mut log, 2, shared);
  let _forgotten = append(&mut log, 3, shared);
  let orphan = append(&mut log, 4, replaced);
  let mut value = Vec::new();
  for located in [&first, &second] {
    put_changeset(&mut rows, log.file(), located, &mut value).unwrap();
  }
  let shared_id = blake3::hash(shared);
  let orphan_id = blake3::hash(replaced);
  let orphan_place = log.sections(&orphan).unwrap().first().copied().unwrap();
  publish(
    &mut rows,
    &content_key(orphan_id.as_bytes()),
    orphan_place.offset,
    orphan_place.len,
  )
  .unwrap();
  rows.checkpoint(log.end()).unwrap();
  assert_eq!(read(&mut rows, &log, shared_id.as_bytes()), None);

  // One changeset per page exercises the continuation while the index is modified.
  let page = CHANGESET_ENTRY_BYTES;
  index_checkpoint(&mut rows, log.file(), log.end(), page).unwrap();
  let IntoFile::Finished {
    file: saved,
    result,
  } = rows.into_file()
  else {
    panic!("the checkpointed rows must close");
  };
  result.unwrap();
  let (mut rows, applied) = ShardDb::open(saved, config, memory, trunk).unwrap();
  // A second open has the marker. An empty log here makes any repeated read of its prefix fail.
  index_checkpoint(&mut rows, &file(3), applied, page).unwrap();
  assert_eq!(read(&mut rows, &log, orphan_id.as_bytes()), None);
  for located in [&first, &second] {
    assert_eq!(
      read(&mut rows, &log, shared_id.as_bytes()).as_deref(),
      Some(shared.as_slice())
    );
    let offset = log.sections(located).unwrap().first().unwrap().offset;
    assert!(forget(&mut rows, shared_id.as_bytes(), offset, &mut value).unwrap() > 0);
  }
  assert_eq!(read(&mut rows, &log, shared_id.as_bytes()), None);
}
