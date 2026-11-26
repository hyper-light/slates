//! Multi-repository `Store::open` recovery isolation tests (SEAM-1): a bit-flip inside one
//! repository's record in `changesets` must never block indexing or serving healthy repositories
//! written after it, while never indexing or serving corrupt bytes and still stopping at torn
//! uncommitted tails.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};

use super::{
  CHANGESET_ENTRY_BYTES, Contents, LOG_FILE, PIECE_BYTES, Sizing, Store, StoreError,
  VerifiedPayloads, payloads, put_changeset_counted, read_verified_piece,
};
use crate::log::{ChangesetLog, Header, LogError, SectionKind, TRAILER_BYTES, Tool};
use crate::staging::Staging;

/// Scratch directory beside the test binary, removed on drop (never `/tmp`, R1).
struct ScratchDir {
  path: PathBuf,
}

impl ScratchDir {
  #[allow(clippy::disallowed_methods)] // test code: scratch store directory beside test binary
  fn new(name: &str) -> Self {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::current_exe()
      .unwrap()
      .parent()
      .unwrap()
      .join(format!(
        "slates-store-lib-{name}-{}-{seq}",
        std::process::id()
      ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Self {
      path: dir.canonicalize().unwrap(),
    }
  }

  fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for ScratchDir {
  #[allow(clippy::disallowed_methods)] // test code: clean up scratch store directory
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

fn open_store(dir: &Path) -> Store {
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
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
  .unwrap()
}

fn land_for(
  store: &mut Store,
  tool: Tool,
  repository: u128,
  sequence: u64,
  payload: &[u8],
) -> crate::log::Located {
  let staging = Staging::new(0);
  store
    .land(
      Header {
        tool,
        repository,
        sequence,
        judged_over: sequence.saturating_sub(1),
        grant: store.grant().id,
        work: 500 + sequence,
        time_ns: sequence,
        sections: 0,
      },
      &[
        (SectionKind::Record, Contents::Bytes(b"refs")),
        (SectionKind::Payload, Contents::Bytes(payload)),
      ],
      &staging,
    )
    .unwrap()
}

fn flip_bit_in_file(path: &Path, offset: u64, bit: u8) {
  let mut file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(path)
    .unwrap();
  file.seek(SeekFrom::Start(offset)).unwrap();
  let mut byte = [0u8; 1];
  file.read_exact(&mut byte).unwrap();
  byte[0] ^= 1u8 << bit;
  file.seek(SeekFrom::Start(offset)).unwrap();
  file.write_all(&byte).unwrap();
  file.sync_all().unwrap();
}

fn read_piece_whole(store: &mut Store, payload: &[u8]) -> Option<Vec<u8>> {
  let staging = Staging::new(0);
  let identity = *blake3::hash(payload).as_bytes();
  let len = u64::try_from(payload.len()).unwrap();
  store
    .piece(&staging, &identity, len, 0)
    .unwrap()
    .map(std::borrow::Cow::into_owned)
}

fn pattern_bytes(len: usize, mul: u8, add: u8) -> Vec<u8> {
  (0..len)
    .map(|n| n.to_le_bytes()[0].wrapping_mul(mul).wrapping_add(add))
    .collect()
}

/// Verifies `repository` has one indexed changeset matching `expected_id` and its payload reads
/// back byte-for-byte.
fn assert_single_changeset_intact(
  store: &mut Store,
  repository: u128,
  expected_id: [u8; blake3::OUT_LEN],
  payload: &[u8],
) {
  let rows = store.changesets(repository, 0, 10).unwrap();
  assert_eq!(rows.len(), 1);
  assert_eq!(rows[0].identity, expected_id);
  assert_eq!(read_piece_whole(store, payload).as_deref(), Some(payload));
}

/// Reopens the store at `dir` and verifies repository 1 stays unindexed, Hg/SVN/r4 payloads read
/// back intact, and `log.end()` matches `expected_end`.
fn assert_reopened_after_append(
  dir: &Path,
  hg_bytes: &[u8],
  svn_bytes: &[u8],
  r4_bytes: &[u8],
  expected_end: u64,
) {
  let mut reopened = open_store(dir);
  assert!(reopened.changesets(1, 0, 10).unwrap().is_empty());
  assert_eq!(
    read_piece_whole(&mut reopened, hg_bytes).as_deref(),
    Some(hg_bytes)
  );
  assert_eq!(
    read_piece_whole(&mut reopened, svn_bytes).as_deref(),
    Some(svn_bytes)
  );
  assert_eq!(
    read_piece_whole(&mut reopened, r4_bytes).as_deref(),
    Some(r4_bytes)
  );
  assert_eq!(reopened.log().end(), expected_end);
}

/// SEAM-1: Do: land Git (1), Mercurial (2), and Subversion (3) records into one `Store` without
/// checkpointing `rows`; flip 1 bit in Git's payload on disk; reopen `Store`.
/// Expect: Git's corrupt record is NOT indexed and its payload is NOT served; Hg and SVN ARE
/// indexed and read back with exact bytes; `log.end()` is past record 3; appending record 4
/// preserves Hg and SVN across another reopen.
#[test]
fn multi_protocol_store_recovery_isolates_corrupt_git_record_from_hg_and_svn() {
  let dir = ScratchDir::new("seam1-payload");
  let git_bytes = pattern_bytes(2_048, 7, 1);
  let hg_bytes = pattern_bytes(1_536, 13, 2);
  let svn_bytes = pattern_bytes(1_792, 19, 3);
  let (r1_payload_offset, r2_id, r3_id, r3_end) = {
    let mut store = open_store(dir.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &git_bytes);
    let r2 = land_for(&mut store, Tool::Mercurial, 2, 1, &hg_bytes);
    let r3 = land_for(&mut store, Tool::Subversion, 3, 1, &svn_bytes);
    let r1_sections = store.log().sections(&r1).unwrap();
    (r1_sections[1].offset, r2.identity, r3.identity, r3.end())
  };
  flip_bit_in_file(&dir.path().join(LOG_FILE), r1_payload_offset + 256, 3);

  let mut store = open_store(dir.path());
  assert!(
    store.changesets(1, 0, 10).unwrap().is_empty(),
    "corrupt Git record must never be indexed"
  );
  assert_eq!(read_piece_whole(&mut store, &git_bytes), None);
  assert_single_changeset_intact(&mut store, 2, r2_id, &hg_bytes);
  assert_single_changeset_intact(&mut store, 3, r3_id, &svn_bytes);
  assert_eq!(store.log().end(), r3_end);

  let r4_bytes = b"fresh git push after isolated recovery";
  let r4 = land_for(&mut store, Tool::Git, 4, 1, r4_bytes);
  assert_eq!(r4.offset, r3_end);
  drop(store);

  assert_reopened_after_append(dir.path(), &hg_bytes, &svn_bytes, r4_bytes, r4.end());
}

/// SEAM-1: Do: flip 1 bit in record 1's header mid-file AND leave a torn uncommitted tail after
/// record 3; reopen `Store`, then append record 4.
/// Expect: records 2 (Hg) and 3 (SVN) recover intact, `log.end()` stops at record 3's end
/// (before the torn tail), and record 4 overwrites the torn tail cleanly.
#[test]
fn multi_protocol_store_recovery_isolates_corrupt_header_and_stops_at_torn_tail() {
  let dir = ScratchDir::new("seam1-header-tail");
  let git_bytes = b"git pack with damaged header".to_vec();
  let hg_bytes = b"healthy mercurial bundle after header flip".to_vec();
  let svn_bytes = b"healthy subversion revision after header flip".to_vec();
  let (r1_offset, r3_end) = {
    let mut store = open_store(dir.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &git_bytes);
    let _r2 = land_for(&mut store, Tool::Mercurial, 2, 1, &hg_bytes);
    let r3 = land_for(&mut store, Tool::Subversion, 3, 1, &svn_bytes);
    {
      let mut torn = store
        .begin(Header {
          tool: Tool::Git,
          repository: 9,
          sequence: 1,
          judged_over: 0,
          grant: store.grant().id,
          work: 99,
          time_ns: 9,
          sections: 1,
        })
        .unwrap();
      torn.section(SectionKind::Payload, 256).unwrap();
      torn.piece(&[0xAB; 256]).unwrap();
      // Dropped uncommitted: torn tail past `r3.end()`.
    }
    (r1.offset, r3.end())
  };
  flip_bit_in_file(&dir.path().join(LOG_FILE), r1_offset + 12, 1);

  let mut store = open_store(dir.path());
  assert!(store.changesets(1, 0, 10).unwrap().is_empty());
  assert_eq!(
    read_piece_whole(&mut store, &hg_bytes),
    Some(hg_bytes.clone())
  );
  assert_eq!(
    read_piece_whole(&mut store, &svn_bytes),
    Some(svn_bytes.clone())
  );
  assert_eq!(store.log().end(), r3_end);

  let r4_bytes = b"record 4 overwrites torn tail".to_vec();
  let r4 = land_for(&mut store, Tool::Subversion, 3, 2, &r4_bytes);
  assert_eq!(r4.offset, r3_end);
  drop(store);

  let mut reopened = open_store(dir.path());
  assert_eq!(read_piece_whole(&mut reopened, &hg_bytes), Some(hg_bytes));
  assert_eq!(read_piece_whole(&mut reopened, &svn_bytes), Some(svn_bytes));
  assert_eq!(read_piece_whole(&mut reopened, &r4_bytes), Some(r4_bytes));
}

fn to_u64(len: usize) -> u64 {
  u64::try_from(len).unwrap_or(u64::MAX)
}

/// Reads a landed payload in pieces starting at `start_offset` until EOF or error.
fn read_all_pieces(
  store: &mut Store,
  payload: &[u8],
  start_offset: u64,
) -> Result<(Vec<u8>, usize), StoreError> {
  let staging = Staging::new(0);
  let id = *blake3::hash(payload).as_bytes();
  let len = to_u64(payload.len());
  let (mut out, mut offset, mut pieces) = (Vec::new(), start_offset, 0usize);
  while let Some(chunk) = store.piece(&staging, &id, len, offset)? {
    pieces += 1;
    offset += to_u64(chunk.len());
    out.extend_from_slice(&chunk);
  }
  Ok((out, pieces))
}

/// Lands `payload` and a healthy sibling, checkpoints, flips one bit at `payload + flip_at`,
/// reopens, and asserts `Store::piece` starting at `start_offset` refuses `LogError::Corrupt`.
fn assert_corrupt_checkpointed_piece_refused(
  name: &str,
  payload_len: usize,
  flip_at: u64,
  start_offset: u64,
) {
  let dir = ScratchDir::new(name);
  let git_bytes = pattern_bytes(payload_len, 31, 7);
  let hg_bytes = b"checkpointed hg payload staying intact".to_vec();
  let r1_payload_offset = {
    let mut store = open_store(dir.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &git_bytes);
    let _r2 = land_for(&mut store, Tool::Mercurial, 2, 1, &hg_bytes);
    store.checkpoint().unwrap();
    store.log().sections(&r1).unwrap()[1].offset
  };
  flip_bit_in_file(&dir.path().join(LOG_FILE), r1_payload_offset + flip_at, 4);

  let mut store = open_store(dir.path());
  let read = read_all_pieces(&mut store, &git_bytes, start_offset);
  assert!(
    matches!(read, Err(StoreError::Log(LogError::Corrupt(_)))),
    "{name}: corrupt checkpointed payload must refuse Corrupt, got {read:?}"
  );
  assert_eq!(read_piece_whole(&mut store, &hg_bytes), Some(hg_bytes));
}

/// SEAM-1 item 3 / Blocker 3: Do: land and checkpoint payloads of (a) 1 piece (1,000 B), (b) exact
/// 65,536 B read from suffix offset 1, (c) 200,000 B with a bit-flip in piece 1 (+5) and in piece 3
/// (+150,000); and (d) a healthy 131,089-byte 3-piece payload.
/// Expect: (a)-(c) all refuse `LogError::Corrupt` before returning corrupt bytes while healthy Hg
/// reads intact; (d) returns exact bytes in 3 pieces and hashes the payload exactly once.
#[test]
fn checkpointed_stale_row_does_not_mask_corrupt_payload_on_piece_read() {
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-1piece", 1_000, 5, 0);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-suffix", PIECE_BYTES, 5, 1);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-first", 200_000, 5, 0);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-third", 200_000, 150_000, 0);

  let dir = ScratchDir::new("seam1-ckpt-healthy-3piece");
  let healthy = pattern_bytes(131_089, 17, 3);
  {
    let mut store = open_store(dir.path());
    land_for(&mut store, Tool::Git, 1, 1, &healthy);
    store.checkpoint().unwrap();
  }
  let mut store = open_store(dir.path());
  let (got, pieces) = read_all_pieces(&mut store, &healthy, 0).unwrap();
  assert_eq!((got, pieces), (healthy, 3));
  assert_eq!(store.payload_verifications(), 1);
}

/// Builds raw serialized bytes of a valid committed record for repository 2 from a separate store.
fn build_forged_record_bytes(forged_payload: &[u8]) -> Vec<u8> {
  let source = ScratchDir::new("seam1-forge-source");
  let mut store = open_store(source.path());
  let record = land_for(&mut store, Tool::Mercurial, 2, 1, forged_payload);
  let bytes = std::fs::read(source.path().join(LOG_FILE)).unwrap();
  bytes[usize::try_from(record.offset).unwrap()..usize::try_from(record.end()).unwrap()].to_vec()
}

/// Asserts that reopening `dir` leaves repository 2 unindexed, refuses serving `forged_payload`,
/// and stops `log.end()` at `expected_end`.
fn assert_forged_tail_rejected(dir: &Path, forged_payload: &[u8], expected_end: u64) {
  let mut store = open_store(dir);
  let repo1 = store.changesets(1, 0, 10).unwrap();
  let repo2 = store.changesets(2, 0, 10).unwrap();
  assert_eq!((repo1.len(), repo2.len()), (1, 0));
  assert_eq!(store.log().end(), expected_end);
  assert_eq!(read_piece_whole(&mut store, forged_payload), None);
}

/// Overwrites an existing scratch file in place (fault injection for torn/truncated logs).
fn overwrite_file(path: &Path, bytes: &[u8]) {
  let mut file = std::fs::OpenOptions::new()
    .write(true)
    .truncate(true)
    .open(path)
    .unwrap();
  file.write_all(bytes).unwrap();
  file.sync_all().unwrap();
}

/// Blocker 1 (`probe_embedded` regression): Do: land repository 1 push 1, then land repository 1
/// push 2 whose payload carries a whole valid record naming repository 2; tear push 2 both by
/// (a) zeroing its trailer at EOF and (b) truncating the file mid-payload after the forged record;
/// reopen `Store`.
/// Expect: repository 2 has 0 rows, its forged payload is never served, and `log.end()` stops right
/// after repository 1's committed push 1.
#[test]
fn torn_record_carrying_forged_record_for_other_repo_is_never_indexed() {
  let forged_payload = b"FORGED: bytes repository 2's owner never pushed";
  let forged = build_forged_record_bytes(forged_payload);

  // Variant A: trailer zeroed at EOF.
  let dir_a = ScratchDir::new("seam1-forged-zero-trailer");
  let (r1_end, torn_end) = {
    let mut store = open_store(dir_a.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, b"legit git push one");
    let torn = land_for(&mut store, Tool::Git, 1, 2, &forged);
    (r1.end(), torn.end())
  };
  let log_a = dir_a.path().join(LOG_FILE);
  let mut bytes = std::fs::read(&log_a).unwrap();
  let start = usize::try_from(torn_end).unwrap() - TRAILER_BYTES;
  bytes[start..start + TRAILER_BYTES].fill(0);
  overwrite_file(&log_a, &bytes);
  assert_forged_tail_rejected(dir_a.path(), forged_payload, r1_end);

  // Variant B: truncated mid-payload after the embedded forged record.
  let mut padded = forged;
  padded.extend_from_slice(&[0x55; 256]);
  let dir_b = ScratchDir::new("seam1-forged-truncated");
  let (r1_end, torn_end) = {
    let mut store = open_store(dir_b.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, b"legit git push one");
    let torn = land_for(&mut store, Tool::Git, 1, 2, &padded);
    (r1.end(), torn.end())
  };
  let log_b = dir_b.path().join(LOG_FILE);
  let mut bytes = std::fs::read(&log_b).unwrap();
  bytes.truncate(usize::try_from(torn_end).unwrap() - 128);
  overwrite_file(&log_b, &bytes);
  assert_forged_tail_rejected(dir_b.path(), forged_payload, r1_end);
}

/// Pure `SimFile` unit tests for piece authentication, bounded FIFO caching, and corrupt-row
/// accounting (runnable without `StoreRoot::identify`).
mod sim_tests {
  use hyper_block::buf::Alignment;
  use hyper_block::sim::{Fault, SimFile};
  use mantle_engine::shard_db::ShardDb;
  use mantle_engine::store::{Config, MIN_PAGE};
  use mantle_engine::trunk::TrunkConfig;

  use super::*;

  fn sim_file(seed: u64) -> SimFile {
    SimFile::new(Alignment::BYTE, Alignment::BYTE, seed).unwrap()
  }

  fn append_sim(log: &mut ChangesetLog<SimFile>, seq: u64, bytes: &[u8]) -> crate::log::Located {
    let mut rec = log
      .begin(Header {
        tool: Tool::Git,
        repository: 1,
        sequence: seq,
        judged_over: seq.saturating_sub(1),
        grant: 7,
        work: seq,
        time_ns: seq,
        sections: 1,
      })
      .unwrap();
    rec
      .section(SectionKind::Payload, u64::try_from(bytes.len()).unwrap())
      .unwrap();
    rec.piece(bytes).unwrap();
    rec.commit().unwrap()
  }

  fn read_sim_pieces(
    log: &ChangesetLog<SimFile>,
    verified: &mut VerifiedPayloads,
    payload: &[u8],
    place: (u64, u64),
    start_offset: u64,
  ) -> Result<(Vec<u8>, usize), StoreError> {
    let id = *blake3::hash(payload).as_bytes();
    let (mut out, mut offset, mut pieces) = (Vec::new(), start_offset, 0usize);
    while offset < place.1 {
      let chunk = read_verified_piece(log, verified, &id, place, offset)?;
      pieces += 1;
      offset += to_u64(chunk.len());
      out.extend_from_slice(&chunk);
    }
    Ok((out, pieces))
  }

  fn assert_sim_flip_refused(seed: u64, payload_len: usize, flip_at: u64, start_offset: u64) {
    let bytes = pattern_bytes(payload_len, 31, 7);
    let mut log = ChangesetLog::recover(sim_file(seed), 0, |_, _| {}).unwrap();
    let located = append_sim(&mut log, 1, &bytes);
    let section = log.sections(&located).unwrap()[0];
    log
      .file()
      .inject(Fault::BitFlip {
        offset: section.offset + flip_at,
        bit: 4,
        stored: true,
      })
      .unwrap();
    let mut verified = VerifiedPayloads::new(8 << 20);
    let got = read_sim_pieces(
      &log,
      &mut verified,
      &bytes,
      (section.offset, section.len),
      start_offset,
    );
    assert!(
      matches!(got, Err(StoreError::Log(LogError::Corrupt(_)))),
      "flip at +{flip_at} from start {start_offset} must refuse Corrupt, got {got:?}"
    );
    assert_eq!(verified.verifications(), 0);
  }

  /// Blocker 3 (`SimFile` oracle): Do: run `read_verified_piece` on (a) 1-piece control, (b) suffix
  /// `offset = 1` on 65,536-byte payload with bit-flip, (c) 200,000-byte payload flipped at +5 and
  /// +150,000, (d) healthy 131,089-byte 3-piece read, and (e) FIFO eviction under `capacity = 2`.
  /// Expect: (a)-(c) refuse `LogError::Corrupt`; (d) returns exact bytes in 3 pieces verifying
  /// exactly once; (e) evicting the oldest entry triggers re-verification on next read.
  #[test]
  fn sim_piece_verification_covers_one_piece_suffix_multipiece_and_fifo_cache() {
    assert_sim_flip_refused(90, 1_000, 5, 0);
    assert_sim_flip_refused(91, PIECE_BYTES, 5, 1);
    assert_sim_flip_refused(92, 200_000, 5, 0);
    assert_sim_flip_refused(93, 200_000, 150_000, 0);

    let healthy = pattern_bytes(131_089, 17, 3);
    let mut log = ChangesetLog::recover(sim_file(94), 0, |_, _| {}).unwrap();
    let r1 = append_sim(&mut log, 1, &healthy);
    let r2 = append_sim(&mut log, 2, b"second");
    let r3 = append_sim(&mut log, 3, b"third");
    let s1 = log.sections(&r1).unwrap()[0];
    let s2 = log.sections(&r2).unwrap()[0];
    let s3 = log.sections(&r3).unwrap()[0];
    let mut verified = VerifiedPayloads::new(2 * PIECE_BYTES);
    let (got, pieces) =
      read_sim_pieces(&log, &mut verified, &healthy, (s1.offset, s1.len), 0).unwrap();
    assert_eq!(
      (got, pieces, verified.verifications()),
      (healthy.clone(), 3, 1)
    );
    read_sim_pieces(&log, &mut verified, b"second", (s2.offset, s2.len), 0).unwrap();
    read_sim_pieces(&log, &mut verified, b"third", (s3.offset, s3.len), 0).unwrap();
    // Capacity is 2, so r1 was evicted and reading it again re-verifies once.
    read_sim_pieces(&log, &mut verified, &healthy, (s1.offset, s1.len), 0).unwrap();
    assert_eq!(verified.verifications(), 4);
  }

  /// Suggestion 2 (`SimFile` oracle): Do: corrupt an already-indexed record's header in `SimFile`
  /// and run both `payloads::index_checkpoint` and `put_changeset_counted` replacement.
  /// Expect: neither silently swallows `LogError::Corrupt`; each logs and increments the skipped
  /// counter by 1.
  #[test]
  fn sim_corrupt_rows_in_index_checkpoint_and_put_changeset_are_counted() {
    let config = Config {
      page_size: MIN_PAGE,
      extent_pages: 32,
      max_extents: 256,
    };
    let trunk = TrunkConfig {
      fanout: 8,
      leaf_entries: 1024,
    };
    let mut rows = ShardDb::create(sim_file(95), config, 1 << 20, trunk).unwrap();
    let mut log = ChangesetLog::recover(sim_file(96), 0, |_, _| {}).unwrap();
    let first = append_sim(&mut log, 1, b"first payload");
    let mut value = Vec::new();
    let mut skipped = 0u64;
    put_changeset_counted(&mut rows, log.file(), &first, &mut value, &mut skipped).unwrap();
    assert_eq!(skipped, 0);

    // Corrupt first record's header so both `index_checkpoint` and replacement meet `LogError::Corrupt`.
    log
      .file()
      .inject(Fault::BitFlip {
        offset: first.offset + 2,
        bit: 0,
        stored: true,
      })
      .unwrap();
    let ckpt_skipped =
      payloads::index_checkpoint(&mut rows, log.file(), log.end(), CHANGESET_ENTRY_BYTES).unwrap();
    assert_eq!(ckpt_skipped, 1);

    let replacement = append_sim(&mut log, 1, b"replacement payload");
    put_changeset_counted(
      &mut rows,
      log.file(),
      &replacement,
      &mut value,
      &mut skipped,
    )
    .unwrap();
    assert_eq!(skipped, 1);
  }
}
