//! Multi-repository `Store::open` recovery isolation tests (SEAM-1): a bit-flip inside one
//! repository's record in `changesets` must never block indexing or serving healthy repositories
//! written after it, while never indexing or serving corrupt bytes and still stopping at torn
//! uncommitted tails.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use hyper_block::block::BlockFile;
use slates_land::store_root::{StoreGrant, StoreRoot};

use super::{
  CHANGESET_ENTRY_BYTES, Contents, LOG_FILE, LogHead, PIECE_BYTES, PieceMetrics, ROWS_FILE, Sizing,
  Store, StoreError, block_key, index_payload_from_log, inspect_log_file, keyed_block_tag,
  payloads, prepare_damaged_preamble_log, prepare_empty_log, prepare_sealed_log,
  put_changeset_counted, read_seal_row, read_verified_span_with, validate_sizing_bounds,
};
use crate::log::{
  ChangesetLog, FORMAT, HEADER_BYTES, Header, LogError, SEAL_BYTES, SEAL_PREAMBLE_BYTES,
  SECTION_HEAD_BYTES, SectionKind, TRAILER_BYTES, Tool, encode_seal_preamble,
};
use crate::staging::Staging;
use crate::texts::{self, Place, TextCache, TextEntry};

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

/// Encodes a checksummed `SLSE` preamble for a foreign format version `format` with valid CRC-32C
/// and bitwise complement so `decode_seal_preamble` classifies it as `UnsupportedFormat(format)`.
fn encode_future_preamble(format: u16, seal: &[u8; SEAL_BYTES]) -> [u8; SEAL_PREAMBLE_BYTES] {
  let mut out = encode_seal_preamble(seal);
  out[4..6].copy_from_slice(&format.to_le_bytes());
  let crc = mantle_crc::crc32c(&out[..40]);
  out[40..44].copy_from_slice(&crc.to_le_bytes());
  out[44..48].copy_from_slice(&(!crc).to_le_bytes());
  out
}

/// Lands `payload` and a healthy sibling, checkpoints, flips one bit at `payload + flip_at`,
/// reopens, and asserts that walking from `start_offset` refuses `LogError::Corrupt` (and when
/// `expect_first_call_refused`, asserts the very first `Store::piece` call refuses with 0 verified
/// reads).
fn assert_corrupt_checkpointed_piece_refused(
  name: &str,
  payload_len: usize,
  flip_at: u64,
  start_offset: u64,
  expect_first_call_refused: bool,
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
  if expect_first_call_refused {
    let staging = Staging::new(0);
    let id = *blake3::hash(&git_bytes).as_bytes();
    let first = store.piece(&staging, &id, to_u64(git_bytes.len()), start_offset);
    assert!(
      matches!(first, Err(StoreError::Log(LogError::Corrupt(_)))),
      "{name}: first Store::piece(offset={start_offset}) must refuse Corrupt, got {first:?}"
    );
    assert_eq!(store.payload_verifications(), 0);
  } else {
    let walk = read_all_pieces(&mut store, &git_bytes, start_offset);
    assert!(
      matches!(walk, Err(StoreError::Log(LogError::Corrupt(_)))),
      "{name}: full walk from offset={start_offset} must refuse Corrupt, got {walk:?}"
    );
  }
  assert_eq!(read_piece_whole(&mut store, &hg_bytes), Some(hg_bytes));
}

/// SEAM-1 item 3 / Blocker 3 / Round-4 B-rows A5: Do: land and checkpoint payloads of (a) 1 piece
/// (1,000 B), (b) exact 65,536 B read from suffix offset 1, (c) 200,000 B with a bit-flip in
/// piece 1 (+5) and in piece 3 (+150,000) requested at block 2, unaligned across block 2, and
/// walked from offset 0/1; and (d) a healthy 131,089-byte 3-piece payload.
/// Expect: every read touching a flipped block refuses `LogError::Corrupt` before returning any
/// corrupt byte while healthy Hg reads intact; (d) returns exact bytes in 3 pieces hashing each
/// block once (`payload_verifications() == 3`, `piece_bytes_hashed() == 131_089`).
#[test]
fn checkpointed_stale_row_does_not_mask_corrupt_payload_on_piece_read() {
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-1piece", 1_000, 5, 0, true);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-suffix", PIECE_BYTES, 5, 1, true);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-first", 200_000, 5, 0, true);
  assert_corrupt_checkpointed_piece_refused(
    "seam1-ckpt-mp-third-direct",
    200_000,
    150_000,
    to_u64(2 * PIECE_BYTES),
    true,
  );
  assert_corrupt_checkpointed_piece_refused(
    "seam1-ckpt-mp-third-unaligned",
    200_000,
    150_000,
    to_u64(2 * PIECE_BYTES - 16),
    true,
  );
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-third-off0", 200_000, 150_000, 0, false);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-third-off1", 200_000, 150_000, 1, false);

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
  assert_eq!(store.payload_verifications(), 3);
  assert_eq!(store.piece_bytes_hashed(), 131_089);
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

/// Encodes a same-grant `SLCS..SLCC` record image for repository 2 without the store's secret seal,
/// targeting `target_offset` (or offset 0 when `target_offset == 0`).
fn forge_unsealed_same_grant_record(
  grant: u64,
  target_offset: u64,
  forged_payload: &[u8],
) -> Vec<u8> {
  let sim = hyper_block::sim::SimFile::new(
    hyper_block::buf::Alignment::BYTE,
    hyper_block::buf::Alignment::BYTE,
    99,
  )
  .unwrap();
  if target_offset > 0 {
    let pad = vec![0u8; usize::try_from(target_offset).unwrap()];
    sim.write_all_at(&pad, 0).unwrap();
    sim.sync_data().unwrap();
  }
  let mut log = ChangesetLog::recover(sim, target_offset, |_, _| {}).unwrap();
  let mut record = log
    .begin(Header {
      tool: Tool::Mercurial,
      repository: 2,
      sequence: 1,
      judged_over: 0,
      grant,
      work: 77,
      time_ns: 77,
      sections: 2,
    })
    .unwrap();
  record.section(SectionKind::Record, 4).unwrap();
  record.piece(b"refs").unwrap();
  record
    .section(SectionKind::Payload, to_u64(forged_payload.len()))
    .unwrap();
  record.piece(forged_payload).unwrap();
  let located = record.commit().unwrap();
  let image = log.into_file().durable_image().unwrap();
  image[usize::try_from(located.offset).unwrap()..usize::try_from(located.end()).unwrap()].to_vec()
}

/// Runs C1 (mid-file broken header followed by healthy repo 3) over a real `Store` with either a
/// verbatim copy of the first record sealed by the SAME store or an unsealed exact-offset forgery.
fn assert_c1_store_nested_rejected(exact_offset: bool, forged_payload: &[u8]) {
  let healthy_r3 = b"healthy repository 3 sibling after broken carrier".to_vec();
  let dir = ScratchDir::new(if exact_offset {
    "seam1-c1-exact-offset"
  } else {
    "seam1-c1-verbatim"
  });
  let (carrier_offset, r3_id, r3_end) = {
    let mut store = open_store(dir.path());
    let seed = land_for(&mut store, Tool::Git, 99, 1, b"seed at preamble end");
    let nested_base =
      seed.end() + to_u64(HEADER_BYTES + SECTION_HEAD_BYTES + 4 + SECTION_HEAD_BYTES);
    let forged = if exact_offset {
      forge_unsealed_same_grant_record(store.grant().id, nested_base, forged_payload)
    } else {
      let raw = std::fs::read(dir.path().join(LOG_FILE)).unwrap();
      raw[usize::try_from(seed.offset).unwrap()..usize::try_from(seed.end()).unwrap()].to_vec()
    };
    let carrier = land_for(&mut store, Tool::Git, 1, 1, &forged);
    assert_eq!(
      store.log().sections(&carrier).unwrap()[1].offset,
      nested_base
    );
    let r3 = land_for(&mut store, Tool::Subversion, 3, 1, &healthy_r3);
    (carrier.offset, r3.identity, r3.end())
  };
  flip_bit_in_file(&dir.path().join(LOG_FILE), carrier_offset + 8, 2);
  let mut store = open_store(dir.path());
  let seed_rows = store.changesets(99, 0, 10).unwrap();
  assert_eq!(seed_rows.len(), 1);
  assert_eq!(seed_rows[0].offset, to_u64(SEAL_PREAMBLE_BYTES));
  assert!(
    store.changesets(2, 0, 10).unwrap().is_empty(),
    "C1 (exact_offset={exact_offset}): forged repo 2 record must never be indexed"
  );
  assert_eq!(read_piece_whole(&mut store, forged_payload), None);
  assert_single_changeset_intact(&mut store, 3, r3_id, &healthy_r3);
  assert_eq!(store.log().end(), r3_end);
}

/// Runs C2 (zeroed header at EOF) or C3 (zeroed header + trailer at EOF) over a real `Store` with
/// an exact-offset same-grant unsealed forgery embedded inside the carrier payload.
fn assert_c2_c3_store_nested_rejected(zero_trailer: bool, forged_payload: &[u8]) {
  let dir = ScratchDir::new(if zero_trailer {
    "seam1-c3-zero-head-trailer"
  } else {
    "seam1-c2-zero-head"
  });
  let (r1_end, carrier_offset, carrier_end) = {
    let mut store = open_store(dir.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, b"legit git push one");
    let nested_offset =
      r1.end() + to_u64(HEADER_BYTES + SECTION_HEAD_BYTES + 4 + SECTION_HEAD_BYTES);
    let forged = forge_unsealed_same_grant_record(store.grant().id, nested_offset, forged_payload);
    let carrier = land_for(&mut store, Tool::Git, 1, 2, &forged);
    assert_eq!(
      store.log().sections(&carrier).unwrap()[1].offset,
      nested_offset
    );
    (r1.end(), carrier.offset, carrier.end())
  };
  let log_path = dir.path().join(LOG_FILE);
  let mut bytes = std::fs::read(&log_path).unwrap();
  let head_start = usize::try_from(carrier_offset).unwrap();
  bytes[head_start..head_start + HEADER_BYTES].fill(0);
  if zero_trailer {
    let tail_start = usize::try_from(carrier_end).unwrap() - TRAILER_BYTES;
    bytes[tail_start..tail_start + TRAILER_BYTES].fill(0);
  }
  overwrite_file(&log_path, &bytes);
  assert_forged_tail_rejected(dir.path(), forged_payload, r1_end);
}

/// Round-2 Blocker 1 (C1/C2/C3): Do: embed a complete same-grant `SLCS..SLCC` record image naming
/// repository 2 inside repository 1's carrier payload — both (a) a verbatim copy of the first
/// record sealed by the same store and (b) a record freshly encoded for the exact nested offset
/// with the victim store's `grant` but without its per-store secret — and test C1 (mid-file broken
/// header + healthy sibling 3), C2 (zeroed header at EOF), and C3 (zeroed header + trailer torn
/// tail at EOF).
/// Expect: repository 2 is NEVER indexed (`changesets(2)` empty, `piece` returns `None`),
/// repository 3 recovers intact on C1, and `log.end()` stays at the last genuine committed record
/// on C2 and C3.
#[test]
fn nested_same_grant_forged_record_never_indexes_on_broken_header_or_torn_tail() {
  let forged_payload = b"FORGED same-grant repo 2 bytes never pushed by owner";
  assert_c1_store_nested_rejected(false, forged_payload);
  assert_c1_store_nested_rejected(true, forged_payload);
  assert_c2_c3_store_nested_rejected(false, forged_payload);
  assert_c2_c3_store_nested_rejected(true, forged_payload);
}

/// Attempts `Store::open` on `dir`, returning the full `Result`.
fn try_open_store(dir: &Path) -> Result<Store, StoreError> {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir)?,
    issued_ns: 0,
  };
  let root = StoreRoot::open(dir, &grant)?;
  Store::open(
    root,
    Sizing {
      memory: 8 << 20,
      disk: 1 << 30,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
}

/// Writes `bytes` to a brand-new file at `path` without calling disallowed convenience helpers.
fn create_file_with(path: &Path, bytes: &[u8]) {
  let mut file = std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .open(path)
    .unwrap();
  file.write_all(bytes).unwrap();
  file.sync_all().unwrap();
}

/// Snapshot of every regular file name and its exact bytes in `dir`.
fn snapshot_dir(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
  let mut map = std::collections::BTreeMap::new();
  for entry in std::fs::read_dir(dir).unwrap() {
    let entry = entry.unwrap();
    if entry.file_type().unwrap().is_file() {
      let name = entry.file_name().to_string_lossy().into_owned();
      map.insert(name, std::fs::read(entry.path()).unwrap());
    }
  }
  map
}

/// Round-3 Blocker 2 (`upgrade.py::rebuild_index`): Do: land Git, Hg, and SVN records into a
/// `Store` and checkpoint; copy only `changesets` into a fresh store directory without `rows`;
/// open `Store`.
/// Expect: `Store::open` extracts the seal from `changesets`'s `SLSE` preamble, rebuilds `rows`
/// with 100% of records/sections/pieces recovered and `log.end()` preserved, and leaves
/// `changesets` byte-for-byte identical.
#[test]
fn rebuilding_deleted_rows_recovers_all_records_with_byte_identical_log() {
  let src = ScratchDir::new("seam3-rebuild-src");
  let git_bytes = b"git commit payload for rebuild".to_vec();
  let hg_bytes = b"hg bundle payload for rebuild".to_vec();
  let svn_bytes = b"svn revision payload for rebuild".to_vec();
  let (r1, r2, r3, expected_seal) = {
    let mut store = open_store(src.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &git_bytes);
    let r2 = land_for(&mut store, Tool::Mercurial, 2, 1, &hg_bytes);
    let r3 = land_for(&mut store, Tool::Subversion, 3, 1, &svn_bytes);
    store.checkpoint().unwrap();
    (r1, r2, r3, store.seal())
  };
  let log_before = std::fs::read(src.path().join(LOG_FILE)).unwrap();

  let dst = ScratchDir::new("seam3-rebuild-dst");
  create_file_with(&dst.path().join(LOG_FILE), &log_before);
  assert!(!dst.path().join(ROWS_FILE).exists());

  let mut rebuilt = open_store(dst.path());
  assert_eq!(rebuilt.seal(), expected_seal);
  assert_eq!(rebuilt.log().end(), r3.end());
  assert_single_changeset_intact(&mut rebuilt, 1, r1.identity, &git_bytes);
  assert_single_changeset_intact(&mut rebuilt, 2, r2.identity, &hg_bytes);
  assert_single_changeset_intact(&mut rebuilt, 3, r3.identity, &svn_bytes);
  drop(rebuilt);

  let log_after = std::fs::read(dst.path().join(LOG_FILE)).unwrap();
  assert_eq!(log_before, log_after, "rebuild must not mutate changesets");
  assert!(dst.path().join(ROWS_FILE).exists());
}

/// Creates `files` in a fresh scratch directory, verifies `Store::open` refuses with the expected
/// format error (`Some(fmt)`) or `LogError::Corrupt` (`None`), and asserts zero files changed.
fn assert_open_refused_without_mutation(
  name: &str,
  files: &[(&str, &[u8])],
  expected_unsupported: Option<u16>,
) {
  let dir = ScratchDir::new(name);
  for (file_name, bytes) in files {
    create_file_with(&dir.path().join(file_name), bytes);
  }
  let before = snapshot_dir(dir.path());
  let err = try_open_store(dir.path());
  match expected_unsupported {
    Some(fmt) => assert!(
      matches!(err, Err(StoreError::Log(LogError::UnsupportedFormat(found))) if found == fmt),
      "{name}: expected UnsupportedFormat({fmt}), got {err:?}"
    ),
    None => assert!(
      matches!(err, Err(StoreError::Log(LogError::Corrupt(_)))),
      "{name}: expected Corrupt, got {err:?}"
    ),
  }
  assert_eq!(snapshot_dir(dir.path()), before, "{name}: mutated store");
}

/// Round-3 Blocker 2 (`upgrade.py` refusal & damaged preamble fallback): Do: open (a) a legacy
/// `FORMAT = 1` `SLCS` store (without and with existing `rows`), (b) a future `FORMAT = 3` `SLSE`
/// store, (c) a damaged `SLSE` preamble without `rows`, and (d) a damaged `SLSE` preamble with
/// existing `rows` holding backup `SEAL_ROW`.
/// Expect: (a)-(b) refuse `LogError::UnsupportedFormat(1 | 3)` with ZERO files created or mutated;
/// (c) refuses `LogError::Corrupt` without creating `rows`; (d) recovers 100% of records using
/// backup `SEAL_ROW`.
#[test]
fn opening_unsupported_format_store_refuses_without_mutating_files() {
  let mut legacy = [0u8; HEADER_BYTES];
  legacy[..4].copy_from_slice(b"SLCS");
  legacy[4..6].copy_from_slice(&1u16.to_le_bytes());
  assert_open_refused_without_mutation("seam3-refuse-v1", &[(LOG_FILE, &legacy)], Some(1));
  assert_open_refused_without_mutation(
    "seam3-refuse-v1-rows",
    &[(LOG_FILE, &legacy), (ROWS_FILE, b"legacy-rows-sentinel")],
    Some(1),
  );

  let future = encode_future_preamble(FORMAT + 1, &[0x33u8; SEAL_BYTES]);
  assert_open_refused_without_mutation("seam3-refuse-v3", &[(LOG_FILE, &future)], Some(FORMAT + 1));

  let mut damaged = encode_seal_preamble(&[0x44u8; SEAL_BYTES]);
  damaged[12] ^= 0xFF;
  assert_open_refused_without_mutation("seam3-damaged-no-rows", &[(LOG_FILE, &damaged)], None);

  // Damaged SLSE preamble WITH existing rows recovers via backup SEAL_ROW.
  let dir_bak = ScratchDir::new("seam3-damaged-with-rows");
  let body = b"survives damaged preamble via backup SEAL_ROW".to_vec();
  let r1 = {
    let mut store = open_store(dir_bak.path());
    land_for(&mut store, Tool::Git, 1, 1, &body)
  };
  flip_bit_in_file(&dir_bak.path().join(LOG_FILE), 16, 0);
  let mut recovered = open_store(dir_bak.path());
  assert_single_changeset_intact(&mut recovered, 1, r1.identity, &body);
  assert_eq!(recovered.log().end(), r1.end());
}

/// Round-3 Blocker 1 (`disk_sector`): Do: zero an aligned 512 B sector over the middle committed
/// record's section head in a real `Store` (sparing the middle record's header), reopen, and land
/// a 5th record.
/// Expect: trailing Hg and SVN records recover intact, `log.end() == svn.end()`, and the 5th
/// record lands at `svn.end()` leaving Hg and SVN bytes intact on disk.
#[test]
fn disk_sector_zeroed_over_middle_record_recovers_siblings_and_preserves_on_next_land() {
  /// Format: standard 512-byte storage sector width.
  const SECTOR: usize = 512;
  let dir = ScratchDir::new("seam3-disk-sector");
  let overhead = HEADER_BYTES + SECTION_HEAD_BYTES + TRAILER_BYTES;
  let hg_bytes = b"healthy Hg after zeroed 512B sector".to_vec();
  let svn_bytes = b"healthy SVN after zeroed 512B sector".to_vec();
  let (hg, svn) = {
    let mut store = open_store(dir.path());
    let staging = Staging::new(0);
    let mut land_one = |tool: Tool, repo: u128, body: &[u8]| {
      store
        .land(
          Header {
            tool,
            repository: repo,
            sequence: 1,
            judged_over: 0,
            grant: store.grant().id,
            work: 1,
            time_ns: 1,
            sections: 1,
          },
          &[(SectionKind::Payload, Contents::Bytes(body))],
          &staging,
        )
        .unwrap()
    };
    // Position record 2 so its section head sits at exact offset 512 (sparing record 2's header!).
    let pad = SECTOR - SEAL_PREAMBLE_BYTES - overhead - HEADER_BYTES;
    land_one(Tool::Git, 1, &vec![b'p'; pad]);
    let middle = land_one(Tool::Git, 2, &vec![b'm'; 2 * SECTOR]);
    assert_eq!(middle.offset + to_u64(HEADER_BYTES), to_u64(SECTOR));
    let hg = land_one(Tool::Mercurial, 3, &hg_bytes);
    let svn = land_one(Tool::Subversion, 4, &svn_bytes);
    (hg, svn)
  };
  let log_path = dir.path().join(LOG_FILE);
  let before = std::fs::read(&log_path).unwrap();
  let mut faulted = before.clone();
  faulted[SECTOR..2 * SECTOR].fill(0);
  overwrite_file(&log_path, &faulted);

  let mut store = open_store(dir.path());
  assert_single_changeset_intact(&mut store, 3, hg.identity, &hg_bytes);
  assert_single_changeset_intact(&mut store, 4, svn.identity, &svn_bytes);
  assert_eq!(store.log().end(), svn.end());
  let fifth = land_for(&mut store, Tool::Git, 5, 1, &[b'n'; 4 * SECTOR]);
  assert_eq!(fifth.offset, svn.end());
  drop(store);

  let after = std::fs::read(&log_path).unwrap();
  let range = usize::try_from(hg.offset).unwrap()..usize::try_from(svn.end()).unwrap();
  assert_eq!(before[range.clone()], after[range]);
}

/// Round-3 A5 (`Store::piece`): Do: land and checkpoint a 2-piece payload, reopen, read piece 0
/// cleanly, then flip 1 bit on disk inside piece 1, and call `Store::piece(offset = PIECE_BYTES)`.
/// Expect: piece 1 refuses `Err(StoreError::Log(LogError::Corrupt(_)))` instead of serving corrupt
/// bytes from a prior verification cache hit.
#[test]
fn a5_disk_bitflip_after_piece_zero_is_caught_on_piece_one() {
  let dir = ScratchDir::new("seam3-a5-piece1");
  let payload = pattern_bytes(131_089, 29, 11);
  let payload_offset = {
    let mut store = open_store(dir.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &payload);
    store.checkpoint().unwrap();
    store.log().sections(&r1).unwrap()[1].offset
  };
  let mut store = open_store(dir.path());
  let staging = Staging::new(0);
  let id = *blake3::hash(&payload).as_bytes();
  let len = to_u64(payload.len());
  let piece0 = store.piece(&staging, &id, len, 0).unwrap().unwrap();
  assert_eq!(piece0.as_ref(), &payload[..PIECE_BYTES]);
  assert_eq!(store.payload_verifications(), 1);

  flip_bit_in_file(
    &dir.path().join(LOG_FILE),
    payload_offset + to_u64(PIECE_BYTES) + 19,
    3,
  );
  let piece1 = store.piece(&staging, &id, len, to_u64(PIECE_BYTES));
  assert!(
    matches!(piece1, Err(StoreError::Log(LogError::Corrupt(_)))),
    "A5: piece 1 after disk bit-flip must refuse Corrupt, got {piece1:?}"
  );
  assert_eq!(store.payload_verifications(), 1);
}

/// Opens a store in `dir` with caller-chosen `sizing`, returning the full `Result`.
fn try_open_store_with(dir: &Path, sizing: Sizing) -> Result<Store, StoreError> {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir)?,
    issued_ns: 0,
  };
  let root = StoreRoot::open(dir, &grant)?;
  Store::open(root, sizing)
}

/// Round-4 Item 1 (`Store` end-to-end): Do: land two oversized 512 KiB payloads under small
/// `Sizing`, interleave piece reads across both payloads, copy only `changesets` into a fresh
/// directory without `rows`, reopen, and re-read.
/// Expect: every `Store::piece` call hashes at most `2 * PIECE_BYTES` (O(1) bytes hashed, zero
/// cache cliff) both before and after rebuilding `rows` from `changesets`.
#[test]
fn oversized_and_interleaved_piece_reads_stay_o1_before_and_after_rows_rebuild() {
  let dir = ScratchDir::new("rt4-o1-interleaved");
  let sizing = Sizing {
    memory: 1 << 20,
    disk: 1 << 30,
    extent_bytes: 128 << 10,
    fanout: 8,
  };
  let payload_a = pattern_bytes(8 * PIECE_BYTES + 37, 13, 5);
  let payload_b = pattern_bytes(8 * PIECE_BYTES + 91, 29, 7);
  {
    let mut store = try_open_store_with(dir.path(), sizing).unwrap();
    land_for(&mut store, Tool::Git, 1, 1, &payload_a);
    land_for(&mut store, Tool::Mercurial, 2, 1, &payload_b);
    store.checkpoint().unwrap();
  }
  assert_interleaved_piece_o1(dir.path(), sizing, &payload_a, &payload_b);
  let rebuilt = ScratchDir::new("rt4-o1-interleaved-rebuilt");
  let log_bytes = std::fs::read(dir.path().join(LOG_FILE)).unwrap();
  create_file_with(&rebuilt.path().join(LOG_FILE), &log_bytes);
  assert!(!rebuilt.path().join(ROWS_FILE).exists());
  assert_interleaved_piece_o1(rebuilt.path(), sizing, &payload_a, &payload_b);
}

/// Reads one piece at `off` from `store` and asserts exact bytes and `<= 2 * PIECE_BYTES` hashed.
fn assert_one_o1_piece(
  store: &mut Store,
  staging: &Staging,
  (id, expected): ([u8; blake3::OUT_LEN], &[u8]),
  off: u64,
) {
  let cap = to_u64(PIECE_BYTES);
  let hashed_before = store.piece_bytes_hashed();
  let got = store
    .piece(staging, &id, to_u64(expected.len()), off)
    .unwrap()
    .unwrap();
  let hashed_delta = store.piece_bytes_hashed() - hashed_before;
  let start = usize::try_from(off).unwrap();
  let end = (start + PIECE_BYTES).min(expected.len());
  assert_eq!(got.as_ref(), &expected[start..end]);
  assert!(
    hashed_delta <= 2 * cap,
    "piece(off={off}) hashed {hashed_delta} bytes (> 2 * PIECE_BYTES)"
  );
}

/// Interleaves piece reads across `payload_a` and `payload_b`, asserting every call hashes
/// `<= 2 * PIECE_BYTES` and returns exact bytes.
fn assert_interleaved_piece_o1(dir: &Path, sizing: Sizing, payload_a: &[u8], payload_b: &[u8]) {
  let mut store = try_open_store_with(dir, sizing).unwrap();
  let staging = Staging::new(0);
  let id_a = *blake3::hash(payload_a).as_bytes();
  let id_b = *blake3::hash(payload_b).as_bytes();
  let cap = to_u64(PIECE_BYTES);
  for block in 0..9u64 {
    let off = block * cap;
    assert_one_o1_piece(&mut store, &staging, (id_a, payload_a), off);
    assert_one_o1_piece(&mut store, &staging, (id_b, payload_b), off);
  }
}

/// Round-4 Item 1 (A5 on `places_bytes` & `text_at`/`make_chain`): Do: land a multi-piece payload
/// carrying two full texts and a `Texts` section, checkpoint, flip 1 bit in the second text on
/// disk, and read through both `places_bytes` and `text_at`.
/// Expect: both `places_bytes` and `text_at` refuse `LogError::Corrupt`, and `TextCache` is NOT
/// poisoned (the corrupted text identity stays absent from `cache`).
#[test]
fn a5_disk_bitflip_caught_by_places_bytes_and_text_at_without_poisoning_cache() {
  let dir = ScratchDir::new("rt4-a5-texts");
  let first_text = pattern_bytes(PIECE_BYTES + 64, 17, 3);
  let second_text = pattern_bytes(4_096, 31, 9);
  let payload = [first_text.as_slice(), second_text.as_slice()].concat();
  let payload_id = *blake3::hash(&payload).as_bytes();
  let place1 = Place {
    payload: payload_id,
    offset: 0,
    len: to_u64(first_text.len()),
  };
  let place2 = Place {
    payload: payload_id,
    offset: to_u64(first_text.len()),
    len: to_u64(second_text.len()),
  };
  let id1 = *blake3::hash(&first_text).as_bytes();
  let id2 = *blake3::hash(&second_text).as_bytes();
  let section = texts::encode(&[
    TextEntry {
      path: b"a.bin".to_vec(),
      tool_id: b"n1".to_vec(),
      text: id1,
      len: place1.len,
      base: None,
      place: place1,
    },
    TextEntry {
      path: b"b.bin".to_vec(),
      tool_id: b"n2".to_vec(),
      text: id2,
      len: place2.len,
      base: None,
      place: place2,
    },
  ]);
  let payload_offset = {
    let mut store = open_store(dir.path());
    let staging = Staging::new(0);
    let located = store
      .land(
        Header {
          tool: Tool::Mercurial,
          repository: 1,
          sequence: 1,
          judged_over: 0,
          grant: store.grant().id,
          work: 1,
          time_ns: 1,
          sections: 0,
        },
        &[
          (SectionKind::Payload, Contents::Bytes(&payload)),
          (SectionKind::Texts, Contents::Bytes(&section)),
        ],
        &staging,
      )
      .unwrap();
    store.checkpoint().unwrap();
    store.log().sections(&located).unwrap()[0].offset
  };
  flip_bit_in_file(
    &dir.path().join(LOG_FILE),
    payload_offset + place2.offset + 77,
    2,
  );
  let mut store = open_store(dir.path());
  let staging = Staging::new(0);
  let places_err = store.places_bytes(&staging, &[place2]);
  assert!(matches!(
    places_err,
    Err(StoreError::Log(LogError::Corrupt(_)))
  ));
  let mut cache = TextCache::new(1 << 20);
  let text_err = store.text_at(&staging, 1, &id2, 0, &mut cache);
  assert!(matches!(
    text_err,
    Err(StoreError::Log(LogError::Corrupt(_)))
  ));
  assert!(
    cache.get(&id2).is_none(),
    "corrupt text must never poison TextCache"
  );
}

/// Round-4 Items 2 & S1 (`P_FORMAT` bit-flip recovery + preamble self-repair): Do: land and
/// checkpoint a record, flip bit 0 of `P_FORMAT` (offset 4) in `changesets`, reopen `Store` (which
/// must recover via `rows`'s backup `SEAL_ROW` AND repair the 48-byte preamble on disk), then
/// copy only the repaired `changesets` into a fresh directory without `rows` and reopen again.
/// Expect: both reopens succeed with 100% of records intact and `changesets`'s 48-byte preamble
/// repaired cleanly on disk.
#[test]
fn p_format_bitflip_recovers_repairs_preamble_and_survives_rows_deletion() {
  let dir = ScratchDir::new("rt4-pformat-repair");
  let body = b"survives P_FORMAT bit-flip and subsequent rows loss".to_vec();
  let (r1, seal) = {
    let mut store = open_store(dir.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &body);
    store.checkpoint().unwrap();
    (r1, store.seal())
  };
  flip_bit_in_file(&dir.path().join(LOG_FILE), 4, 0);
  {
    let mut repaired = open_store(dir.path());
    assert_eq!(repaired.seal(), seal);
    assert_single_changeset_intact(&mut repaired, 1, r1.identity, &body);
  }
  let log_bytes = std::fs::read(dir.path().join(LOG_FILE)).unwrap();
  assert_eq!(
    &log_bytes[..SEAL_PREAMBLE_BYTES],
    &encode_seal_preamble(&seal),
    "damaged preamble must be durably repaired on disk"
  );
  let rebuilt_dir = ScratchDir::new("rt4-pformat-rebuilt");
  create_file_with(&rebuilt_dir.path().join(LOG_FILE), &log_bytes);
  assert!(!rebuilt_dir.path().join(ROWS_FILE).exists());
  let mut rebuilt = open_store(rebuilt_dir.path());
  assert_eq!(rebuilt.seal(), seal);
  assert_single_changeset_intact(&mut rebuilt, 1, r1.identity, &body);
}

/// Round-4 Item 4 (Truncated `changesets` refusal): Do: checkpoint a Format-2 store, then truncate
/// `changesets` to (a) 48 bytes (`48 < applied`) and (b) 20 bytes (`1..47` with `SEAL_ROW` in
/// `rows`), and call `Store::open`.
/// Expect: both refuse `Err(StoreError::Log(LogError::Corrupt(_)))` without mutating `changesets`
/// or `rows`.
#[test]
fn truncated_changesets_log_refuses_without_mutating_files() {
  for (name, cut) in [("rt4-trunc-48", SEAL_PREAMBLE_BYTES), ("rt4-trunc-20", 20)] {
    let dir = ScratchDir::new(name);
    {
      let mut store = open_store(dir.path());
      land_for(&mut store, Tool::Git, 1, 1, b"checkpointed record");
      store.checkpoint().unwrap();
    }
    let log_path = dir.path().join(LOG_FILE);
    let mut truncated = std::fs::read(&log_path).unwrap();
    truncated.truncate(cut);
    overwrite_file(&log_path, &truncated);
    let before = snapshot_dir(dir.path());
    let err = try_open_store(dir.path());
    assert!(
      matches!(err, Err(StoreError::Log(LogError::Corrupt(_)))),
      "{name} (cut={cut}): expected Corrupt, got {err:?}"
    );
    assert_eq!(snapshot_dir(dir.path()), before, "{name}: mutated files");
  }
}

/// Round-4 Item 5 (`expected_grant: None` on resync across directory copy): Do: land 3 records in
/// `src`, copy `changesets` to a fresh directory `dst` (different inode -> different `grant_id`),
/// flip 1 bit in record 1's header, and open `Store` on `dst`.
/// Expect: `dst`'s grant id differs from `src`, record 1 is skipped, and records 2 & 3 recover
/// 100% intact.
#[test]
fn copied_changesets_with_flipped_first_header_resyncs_trailing_records() {
  let src = ScratchDir::new("rt4-copy-resync-src");
  let git_bytes = b"record 1 damaged after copy".to_vec();
  let hg_bytes = b"record 2 healthy Hg after copy".to_vec();
  let svn_bytes = b"record 3 healthy SVN after copy".to_vec();
  let (src_grant, r1, r2, r3) = {
    let mut store = open_store(src.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &git_bytes);
    let r2 = land_for(&mut store, Tool::Mercurial, 2, 1, &hg_bytes);
    let r3 = land_for(&mut store, Tool::Subversion, 3, 1, &svn_bytes);
    (store.grant().id, r1, r2, r3)
  };
  let dst = ScratchDir::new("rt4-copy-resync-dst");
  let copied = std::fs::read(src.path().join(LOG_FILE)).unwrap();
  create_file_with(&dst.path().join(LOG_FILE), &copied);
  flip_bit_in_file(&dst.path().join(LOG_FILE), r1.offset + 12, 1);

  let mut recovered = open_store(dst.path());
  assert_ne!(recovered.grant().id, src_grant);
  assert!(recovered.changesets(1, 0, 10).unwrap().is_empty());
  assert_single_changeset_intact(&mut recovered, 2, r2.identity, &hg_bytes);
  assert_single_changeset_intact(&mut recovered, 3, r3.identity, &svn_bytes);
  assert_eq!(recovered.log().end(), r3.end());
}

/// Round-4 Item 5 (0-byte `rows` recovery): Do: call `Store::open` with invalid `memory: 0`, then
/// with invalid `extent_bytes: 1`, then with a manually left 0-byte `rows` file beside a sealed
/// `changesets`, and reopen with valid `Sizing` each time.
/// Expect: invalid sizings refuse `StoreError::Sizing`, and reopening with valid `Sizing` succeeds
/// cleanly every time.
#[test]
fn zero_byte_rows_and_bad_sizing_recover_on_valid_reopen() {
  let valid = Sizing {
    memory: 8 << 20,
    disk: 1 << 30,
    extent_bytes: 128 << 10,
    fanout: 8,
  };
  let dir = ScratchDir::new("rt4-zero-rows");
  let bad_mem = try_open_store_with(dir.path(), Sizing { memory: 0, ..valid });
  assert!(matches!(bad_mem, Err(StoreError::Sizing(_))));
  assert!(!dir.path().join(ROWS_FILE).exists());

  let bad_extent = try_open_store_with(
    dir.path(),
    Sizing {
      extent_bytes: 1,
      ..valid
    },
  );
  assert!(matches!(bad_extent, Err(StoreError::Sizing(_))));

  let body = b"lands cleanly after bad sizing".to_vec();
  let r1 = {
    let mut store = try_open_store_with(dir.path(), valid).unwrap();
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &body);
    store.checkpoint().unwrap();
    r1
  };
  overwrite_file(&dir.path().join(ROWS_FILE), &[]);
  let mut rebuilt = try_open_store_with(dir.path(), valid).unwrap();
  assert_single_changeset_intact(&mut rebuilt, 1, r1.identity, &body);
}

/// Pure `SimFile` unit tests for durable `B`-row piece/span authentication, `SLSE` preamble
/// self-repair & format versioning, truncation refusal, `expected_grant: None` resync, sizing
/// bounds, and corrupt-row accounting (runnable without `StoreRoot::identify`).
mod sim_tests {
  use hyper_block::buf::Alignment;
  use hyper_block::sim::{Crash, Fault, SimFile};
  use mantle_engine::shard_db::ShardDb;
  use mantle_engine::store::{Config, IntoFile, MIN_PAGE};
  use mantle_engine::trunk::TrunkConfig;

  use super::*;

  fn sim_file(seed: u64) -> SimFile {
    SimFile::new(Alignment::BYTE, Alignment::BYTE, seed).unwrap()
  }

  fn sim_db_config() -> (Config, TrunkConfig) {
    (
      Config {
        page_size: MIN_PAGE,
        extent_pages: 32,
        max_extents: 256,
      },
      TrunkConfig {
        fanout: 8,
        leaf_entries: 1024,
      },
    )
  }

  fn create_sim_rows(seed: u64) -> ShardDb<SimFile> {
    let (config, trunk) = sim_db_config();
    ShardDb::create(sim_file(seed), config, 1 << 20, trunk).unwrap()
  }

  fn append_sim(log: &mut ChangesetLog<SimFile>, seq: u64, bytes: &[u8]) -> crate::log::Located {
    append_sim_with_grant(log, seq, 7, bytes)
  }

  fn append_sim_with_grant(
    log: &mut ChangesetLog<SimFile>,
    seq: u64,
    grant: u64,
    bytes: &[u8],
  ) -> crate::log::Located {
    let mut rec = log
      .begin(Header {
        tool: Tool::Git,
        repository: 1,
        sequence: seq,
        judged_over: seq.saturating_sub(1),
        grant,
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

  /// Appends one payload section to `log`, indexes its `P` and multi-piece `B` rows into `rows`,
  /// and returns its `(offset, len)` section range.
  fn append_and_index(
    log: &mut ChangesetLog<SimFile>,
    rows: &mut ShardDb<SimFile>,
    seq: u64,
    bytes: &[u8],
  ) -> (u64, u64) {
    let located = append_sim(log, seq, bytes);
    let sec = log.sections(&located).unwrap()[0];
    let seal = log.seal();
    let (mut value, mut buffer) = (Vec::new(), Vec::new());
    index_payload_from_log(
      rows,
      log.file(),
      &seal,
      (sec.offset, sec.len),
      (&mut value, &mut buffer),
    )
    .unwrap();
    (sec.offset, sec.len)
  }

  /// Reads one piece (at most `PIECE_BYTES` from `offset`) via `read_verified_span_with`.
  fn read_sim_piece(
    log: &ChangesetLog<SimFile>,
    rows: &mut ShardDb<SimFile>,
    metrics: &mut PieceMetrics,
    id: &[u8; blake3::OUT_LEN],
    place: (u64, u64),
    offset: u64,
  ) -> Result<Vec<u8>, StoreError> {
    let take = place
      .1
      .checked_sub(offset)
      .ok_or(StoreError::BadRow)?
      .min(to_u64(PIECE_BYTES));
    read_verified_span_with(log, rows, metrics, id, place, (offset, take))
  }

  fn read_sim_all_pieces(
    log: &ChangesetLog<SimFile>,
    rows: &mut ShardDb<SimFile>,
    metrics: &mut PieceMetrics,
    payload: &[u8],
    place: (u64, u64),
    start_offset: u64,
  ) -> Result<(Vec<u8>, usize), StoreError> {
    let id = *blake3::hash(payload).as_bytes();
    let (mut out, mut offset, mut pieces) = (Vec::new(), start_offset, 0usize);
    while offset < place.1 {
      let chunk = read_sim_piece(log, rows, metrics, &id, place, offset)?;
      pieces += 1;
      offset += to_u64(chunk.len());
      out.extend_from_slice(&chunk);
    }
    Ok((out, pieces))
  }

  fn assert_corrupt_piece(res: Result<Vec<u8>, StoreError>, label: &str) {
    assert!(
      matches!(res, Err(StoreError::Log(LogError::Corrupt(_)))),
      "{label}: expected Corrupt, got {res:?}"
    );
  }

  fn assert_sim_flip_refused(
    seed: u64,
    payload_len: usize,
    flip_at: u64,
    start_offset: u64,
    first_call_refused: bool,
  ) {
    let bytes = pattern_bytes(payload_len, 31, 7);
    let mut rows = create_sim_rows(seed + 1_000);
    let mut log = ChangesetLog::recover(sim_file(seed), 0, |_, _| {}).unwrap();
    let place = append_and_index(&mut log, &mut rows, 1, &bytes);
    log
      .file()
      .inject(Fault::BitFlip {
        offset: place.0 + flip_at,
        bit: 4,
        stored: true,
      })
      .unwrap();
    let mut metrics = PieceMetrics::default();
    let id = *blake3::hash(&bytes).as_bytes();
    if first_call_refused {
      let first = read_sim_piece(&log, &mut rows, &mut metrics, &id, place, start_offset);
      assert_corrupt_piece(first, "first piece touching flipped block");
      assert_eq!(metrics.verifications, 0);
    } else {
      let walk = read_sim_all_pieces(&log, &mut rows, &mut metrics, &bytes, place, start_offset);
      assert!(matches!(walk, Err(StoreError::Log(LogError::Corrupt(_)))));
    }
  }

  /// Blocker 3 / Round-4 B-rows A5 (`SimFile` oracle): Do: run `read_verified_span_with` on
  /// (a) 1-piece control (verifying ZERO `B` rows are written for `<= PIECE_BYTES`), (b) suffix
  /// `offset = 1` on exact `PIECE_BYTES`, (c) 200,000-byte payload flipped at +5 and +150,000,
  /// and (d) healthy 131,089-byte 3-piece reads.
  /// Expect: small payloads write zero `B` rows; every read touching a flipped block refuses
  /// `LogError::Corrupt`; healthy 3-piece read hashes exact `131_089` bytes across 3 verified calls.
  #[test]
  fn sim_piece_verification_covers_one_piece_suffix_and_multipiece() {
    assert_sim_flip_refused(90, 1_000, 5, 0, true);
    assert_sim_flip_refused(91, PIECE_BYTES, 5, 1, true);
    assert_sim_flip_refused(92, 200_000, 5, 0, true);
    assert_sim_flip_refused(93, 200_000, 150_000, to_u64(2 * PIECE_BYTES), true);
    assert_sim_flip_refused(98, 200_000, 150_000, 0, false);

    // Small payload <= PIECE_BYTES writes zero B rows.
    let small = pattern_bytes(PIECE_BYTES, 19, 1);
    let mut rows = create_sim_rows(1094);
    let mut log = ChangesetLog::recover(sim_file(94), 0, |_, _| {}).unwrap();
    let small_place = append_and_index(&mut log, &mut rows, 1, &small);
    let mut scratch = Vec::new();
    assert!(
      !rows
        .get(&block_key(small_place.0, 0), &mut scratch)
        .unwrap(),
      "single-piece payload <= PIECE_BYTES must not write B rows"
    );

    // Healthy 3-piece payload verifies in O(1) bytes per piece.
    let healthy = pattern_bytes(131_089, 17, 3);
    let place = append_and_index(&mut log, &mut rows, 2, &healthy);
    let mut metrics = PieceMetrics::default();
    let (got, pieces) =
      read_sim_all_pieces(&log, &mut rows, &mut metrics, &healthy, place, 0).unwrap();
    assert_eq!(
      (got, pieces, metrics.verifications, metrics.bytes_hashed),
      (healthy.clone(), 3, 3, 131_089)
    );
    let (again, pieces2) =
      read_sim_all_pieces(&log, &mut rows, &mut metrics, &healthy, place, 0).unwrap();
    assert_eq!(
      (again, pieces2, metrics.verifications, metrics.bytes_hashed),
      (healthy, 3, 6, 2 * 131_089)
    );
  }

  /// Suggestion 2 (`SimFile` oracle): Do: corrupt an already-indexed record's header in `SimFile`
  /// and run both `payloads::index_checkpoint` and `put_changeset_counted` replacement.
  /// Expect: neither silently swallows `LogError::Corrupt`; each logs and increments the skipped
  /// counter by 1.
  #[test]
  fn sim_corrupt_rows_in_index_checkpoint_and_put_changeset_are_counted() {
    let (config, trunk) = sim_db_config();
    let mut rows = ShardDb::create(sim_file(95), config, 1 << 20, trunk).unwrap();
    let mut log = ChangesetLog::recover(sim_file(96), 0, |_, _| {}).unwrap();
    let seal = log.seal();
    let first = append_sim(&mut log, 1, b"first payload");
    let mut value = Vec::new();
    let mut skipped = 0u64;
    put_changeset_counted(
      &mut rows,
      log.file(),
      &seal,
      &first,
      &mut value,
      &mut skipped,
    )
    .unwrap();
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
    let ckpt_skipped = payloads::index_checkpoint(
      &mut rows,
      log.file(),
      (log.end(), &seal),
      CHANGESET_ENTRY_BYTES,
    )
    .unwrap();
    assert_eq!(ckpt_skipped, 1);

    let replacement = append_sim(&mut log, 1, b"replacement payload");
    put_changeset_counted(
      &mut rows,
      log.file(),
      &seal,
      &replacement,
      &mut value,
      &mut skipped,
    )
    .unwrap();
    assert_eq!(skipped, 1);
  }

  /// Rebuilds a fresh `ShardDb` from `log_file` alone and asserts byte-identical log + full replay.
  fn assert_sim_rebuild_from_sealed_log(
    log_file: SimFile,
    seal: [u8; SEAL_BYTES],
    expected: &[crate::log::Located],
    image_before: &[u8],
  ) -> SimFile {
    let (config, trunk) = sim_db_config();
    assert_eq!(inspect_log_file(&log_file).unwrap(), LogHead::Sealed(seal));
    let fresh_rows = ShardDb::create(sim_file(103), config, 1 << 20, trunk).unwrap();
    let rebuilt = prepare_sealed_log(log_file, seal, (fresh_rows, 0, config.page_size)).unwrap();
    let mut rebuilt_rows = rebuilt.rows;
    assert_eq!(
      (
        rebuilt.replay_from,
        read_seal_row(&mut rebuilt_rows).unwrap()
      ),
      (to_u64(SEAL_PREAMBLE_BYTES), Some(seal))
    );
    let mut found = Vec::new();
    let recovered = ChangesetLog::recover_sealed(
      rebuilt.log_file,
      rebuilt.replay_from,
      rebuilt.seal,
      None,
      |_, entry| found.push(*entry),
    )
    .unwrap();
    assert_eq!(
      (found.as_slice(), recovered.end()),
      (expected, expected.last().unwrap().end())
    );
    let log_file = recovered.into_file();
    assert_eq!(log_file.durable_image().unwrap(), image_before);
    log_file
  }

  /// Flips `P_FORMAT` bit 0 (offset 4) on `log_file`, verifies refusal without `rows`, verifies
  /// backup `SEAL_ROW` recovery repairs the 48-byte preamble durably on disk, and verifies a
  /// subsequent rebuild with fresh empty `rows` succeeds.
  fn assert_sim_damaged_preamble_fallback_and_repair(
    log_file: SimFile,
    saved_rows: ShardDb<SimFile>,
    seal: [u8; SEAL_BYTES],
    expected: &[crate::log::Located],
    image_before: &[u8],
  ) {
    let (config, trunk) = sim_db_config();
    log_file
      .inject(Fault::BitFlip {
        offset: 4,
        bit: 0,
        stored: true,
      })
      .unwrap();
    assert_eq!(
      inspect_log_file(&log_file).unwrap(),
      LogHead::DamagedPreamble
    );
    let no_rows = prepare_damaged_preamble_log(sim_file(104), None);
    assert!(matches!(
      no_rows,
      Err(StoreError::Log(LogError::Corrupt(_)))
    ));

    let IntoFile::Finished {
      file: saved_rows_file,
      result,
    } = saved_rows.into_file()
    else {
      panic!("rows must close cleanly");
    };
    result.unwrap();
    let (backup_rows, applied) = ShardDb::open(saved_rows_file, config, 1 << 20, trunk).unwrap();
    let fallback =
      prepare_damaged_preamble_log(log_file, Some((backup_rows, applied, config.page_size)))
        .unwrap();
    let mut found = Vec::new();
    let recovered = ChangesetLog::recover_sealed(
      fallback.log_file,
      fallback.replay_from,
      fallback.seal,
      None,
      |_, entry| found.push(*entry),
    )
    .unwrap();
    assert_eq!(
      (
        fallback.seal,
        fallback.replay_from,
        found.as_slice(),
        recovered.end(),
      ),
      (
        seal,
        to_u64(SEAL_PREAMBLE_BYTES),
        expected,
        expected.last().unwrap().end(),
      )
    );
    let repaired_log = recovered.into_file();
    repaired_log.crash(Crash::LoseAll).unwrap();
    assert_eq!(
      inspect_log_file(&repaired_log).unwrap(),
      LogHead::Sealed(seal),
      "repaired preamble must survive LoseAll crash and classify as Sealed"
    );
    assert_sim_rebuild_from_sealed_log(repaired_log, seal, expected, image_before);
  }

  /// Round-3 Blocker 2 / Round-4 B2+S1 (`SimFile` oracle): Do: initialize a store log via
  /// `prepare_empty_log`, land records, crash `LoseAll`, rebuild with a fresh `ShardDb`, then flip
  /// `P_FORMAT` bit 0, recover via backup `SEAL_ROW`, crash `LoseAll`, and rebuild again from
  /// scratch without `rows`.
  /// Expect: `P_FORMAT` bit-flip classifies as `DamagedPreamble`, recovery durably self-repairs
  /// the 48-byte preamble on disk, and subsequent deletion of `rows` rebuilds 100% cleanly.
  #[test]
  fn sim_seal_preamble_survives_crash_rebuilds_deleted_rows_and_falls_back_to_backup() {
    let (config, trunk) = sim_db_config();
    let fresh_rows = ShardDb::create(sim_file(101), config, 1 << 20, trunk).unwrap();
    let opened = prepare_empty_log(sim_file(102), (fresh_rows, 0, config.page_size)).unwrap();
    let seal = opened.seal;
    assert_ne!(seal, crate::log::UNSEALED);
    assert_eq!(
      (opened.replay_from, opened.seal_written_to_rows),
      (to_u64(SEAL_PREAMBLE_BYTES), true)
    );

    let mut rows = opened.rows;
    rows.checkpoint(opened.replay_from).unwrap();
    let mut log =
      ChangesetLog::recover_sealed(opened.log_file, opened.replay_from, seal, None, |_, _| {})
        .unwrap();
    let r1 = append_sim(&mut log, 1, b"first sealed changeset");
    let r2 = append_sim(&mut log, 2, b"second sealed changeset");
    let log_file = log.into_file();
    log_file.crash(Crash::LoseAll).unwrap();
    let log_image_before = log_file.durable_image().unwrap();

    let log_file = assert_sim_rebuild_from_sealed_log(log_file, seal, &[r1, r2], &log_image_before);
    assert_sim_damaged_preamble_fallback_and_repair(
      log_file,
      rows,
      seal,
      &[r1, r2],
      &log_image_before,
    );
  }

  /// Writes `bytes` into a fresh `SimFile`, asserts `inspect_log_file` refuses
  /// `UnsupportedFormat(expected_fmt)`, and verifies zero bytes changed.
  fn assert_sim_foreign_format_refused(seed: u64, bytes: &[u8], expected_fmt: u16) {
    let sim = sim_file(seed);
    sim.write_all_at(bytes, 0).unwrap();
    sim.sync_data().unwrap();
    let before = sim.durable_image().unwrap();
    assert!(matches!(
      inspect_log_file(&sim),
      Err(StoreError::Log(LogError::UnsupportedFormat(found))) if found == expected_fmt
    ));
    assert_eq!(sim.durable_image().unwrap(), before);
  }

  /// Verifies truncated `changesets` logs (`48 < applied` and `20 < 48`) are refused without
  /// mutating `log_file` in both `prepare_sealed_log` and `prepare_damaged_preamble_log`.
  fn assert_sim_truncated_logs_refused_without_mutation() {
    let (config, trunk) = sim_db_config();
    let seal = [0xCCu8; SEAL_BYTES];
    let preamble = encode_seal_preamble(&seal);

    // 48-byte valid preamble with applied = 256 -> refused without mutation.
    let sealed_trunc = sim_file(120);
    sealed_trunc.write_all_at(&preamble, 0).unwrap();
    sealed_trunc.sync_data().unwrap();
    let mut rows_a = ShardDb::create(sim_file(121), config, 1 << 20, trunk).unwrap();
    rows_a.put(&[super::super::SEAL_ROW], &seal).unwrap();
    let err_a = prepare_sealed_log(sealed_trunc, seal, (rows_a, 256, config.page_size));
    assert!(matches!(err_a, Err(StoreError::Log(LogError::Corrupt(_)))));

    // 20-byte truncated preamble with backup SEAL_ROW -> refused before rewriting preamble.
    let short_trunc = sim_file(122);
    short_trunc.write_all_at(&preamble[..20], 0).unwrap();
    short_trunc.sync_data().unwrap();
    let mut rows_b = ShardDb::create(sim_file(123), config, 1 << 20, trunk).unwrap();
    rows_b.put(&[super::super::SEAL_ROW], &seal).unwrap();
    let err_b = prepare_damaged_preamble_log(short_trunc, Some((rows_b, 256, config.page_size)));
    assert!(matches!(err_b, Err(StoreError::Log(LogError::Corrupt(_)))));
  }

  /// Round-3 Blocker 2 / Round-4 Item 4 (`SimFile` format-version, seal-mismatch, and truncation
  /// refusal oracle): Do: run `inspect_log_file` on legacy `FORMAT = 1` `SLCS` and checksummed
  /// future `FORMAT + 1` `SLSE`, run `prepare_sealed_log` with a mismatched `SEAL_ROW`, and test
  /// truncated logs (`48 < applied` and `20 < 48`).
  /// Expect: `UnsupportedFormat(1)`, `UnsupportedFormat(FORMAT + 1)`, and `Corrupt` on mismatch
  /// and truncation, with zero mutation.
  #[test]
  fn sim_inspect_log_refuses_foreign_formats_and_mismatched_seal_row() {
    let mut v1_head = [0u8; HEADER_BYTES];
    v1_head[..4].copy_from_slice(b"SLCS");
    v1_head[4..6].copy_from_slice(&1u16.to_le_bytes());
    assert_sim_foreign_format_refused(105, &v1_head, 1);

    let v3_preamble = encode_future_preamble(FORMAT + 1, &[0x33u8; SEAL_BYTES]);
    assert_sim_foreign_format_refused(106, &v3_preamble, FORMAT + 1);

    let (config, trunk) = sim_db_config();
    let mut foreign_rows = ShardDb::create(sim_file(107), config, 1 << 20, trunk).unwrap();
    foreign_rows
      .put(&[super::super::SEAL_ROW], &[0xAAu8; SEAL_BYTES])
      .unwrap();
    let sealed_file = sim_file(108);
    sealed_file
      .write_all_at(&encode_seal_preamble(&[0xBBu8; SEAL_BYTES]), 0)
      .unwrap();
    sealed_file.sync_data().unwrap();
    let mismatch = prepare_sealed_log(
      sealed_file,
      [0xBBu8; SEAL_BYTES],
      (foreign_rows, 0, config.page_size),
    );
    assert!(matches!(
      mismatch,
      Err(StoreError::Log(LogError::Corrupt("a store seal mismatch")))
    ));
    assert_sim_truncated_logs_refused_without_mutation();
  }

  /// Reads one verified piece and returns both its result and the exact `SimFile` read call delta.
  fn sim_read_counted(
    log: &ChangesetLog<SimFile>,
    rows: &mut ShardDb<SimFile>,
    metrics: &mut PieceMetrics,
    id: &[u8; blake3::OUT_LEN],
    place: (u64, u64),
    offset: u64,
  ) -> (Result<Vec<u8>, StoreError>, u64) {
    let before = log.file().stats().unwrap().reads;
    let res = read_sim_piece(log, rows, metrics, id, place, offset);
    let delta = log.file().stats().unwrap().reads - before;
    (res, delta)
  }

  /// Round-4 A5 (`SimFile` oracle): Do: read piece 0 of a 3-piece landed payload cleanly, then
  /// flip 1 stored bit on disk in block 1, delete a `B` row, corrupt a `B` row, and also read a
  /// coalesced multi-block span via `read_verified_span_with` before and after a bit-flip.
  /// Expect: every flipped block, missing `B` row, or tampered `B` row refuses `LogError::Corrupt`
  /// immediately, and a clean coalesced span spanning blocks 0..=2 reads in ONE `SimFile` read.
  #[test]
  fn sim_a5_bitflip_and_missing_block_digest_caught_immediately() {
    let payload = pattern_bytes(131_089, 23, 5);
    let id = *blake3::hash(&payload).as_bytes();
    let mut rows = create_sim_rows(1110);
    let mut log = ChangesetLog::recover(sim_file(110), 0, |_, _| {}).unwrap();
    let place = append_and_index(&mut log, &mut rows, 1, &payload);
    let mut metrics = PieceMetrics::default();

    // Clean coalesced span across all 3 blocks takes exactly 1 disk read call.
    let reads_before = log.file().stats().unwrap().reads;
    let span =
      read_verified_span_with(&log, &mut rows, &mut metrics, &id, place, (100, 130_900)).unwrap();
    assert_eq!(span, payload[100..131_000]);
    assert_eq!(log.file().stats().unwrap().reads - reads_before, 1);
    assert_eq!(metrics.verifications, 1);

    // Missing B row refuses Corrupt("a payload's block digest").
    rows.delete(&block_key(place.0, 1)).unwrap();
    let missing = read_sim_piece(
      &log,
      &mut rows,
      &mut metrics,
      &id,
      place,
      to_u64(PIECE_BYTES),
    );
    assert!(matches!(
      missing,
      Err(StoreError::Log(LogError::Corrupt(
        "a payload's block digest"
      )))
    ));

    // Restore block 1's B row, then flip 1 bit in block 1 on disk -> refuses Corrupt.
    let seal = log.seal();
    let tag1 = keyed_block_tag(
      &seal,
      place.0,
      place.1,
      1,
      &payload[PIECE_BYTES..2 * PIECE_BYTES],
    );
    rows.put(&block_key(place.0, 1), &tag1).unwrap();
    log
      .file()
      .inject(Fault::BitFlip {
        offset: place.0 + to_u64(PIECE_BYTES) + 17,
        bit: 2,
        stored: true,
      })
      .unwrap();
    assert_corrupt_piece(
      read_sim_piece(
        &log,
        &mut rows,
        &mut metrics,
        &id,
        place,
        to_u64(PIECE_BYTES),
      ),
      "A5 flipped block 1 piece read",
    );
    assert_corrupt_piece(
      read_verified_span_with(&log, &mut rows, &mut metrics, &id, place, (100, 130_900)),
      "A5 flipped block 1 coalesced span read",
    );
    assert_eq!(metrics.verifications, 1);
  }

  /// Asserts one aligned `SimFile` piece read takes exactly 1 read call and hashes `<= PIECE_BYTES`.
  fn assert_sim_one_o1_piece(
    log: &ChangesetLog<SimFile>,
    rows: &mut ShardDb<SimFile>,
    metrics: &mut PieceMetrics,
    (id, place, expected): ([u8; blake3::OUT_LEN], (u64, u64), &[u8]),
    off: u64,
  ) {
    let cap = to_u64(PIECE_BYTES);
    let hashed_before = metrics.bytes_hashed;
    let (res, reads) = sim_read_counted(log, rows, metrics, &id, place, off);
    let hashed_delta = metrics.bytes_hashed - hashed_before;
    let start = usize::try_from(off).unwrap();
    let end = (start + PIECE_BYTES).min(expected.len());
    assert_eq!((res.unwrap().as_slice(), reads), (&expected[start..end], 1));
    assert!(hashed_delta <= cap);
  }

  /// Proves O(1) reads (1 read per piece) and O(1) bytes hashed (`<= 2 * PIECE_BYTES` per call)
  /// on oversized and interleaved payloads, plus unaligned boundary reads.
  fn assert_sim_o1_interleaved_and_unaligned_reads() {
    let payload_a = pattern_bytes(8 * PIECE_BYTES + 19, 11, 1);
    let payload_b = pattern_bytes(8 * PIECE_BYTES + 73, 17, 2);
    let (id_a, id_b) = (
      *blake3::hash(&payload_a).as_bytes(),
      *blake3::hash(&payload_b).as_bytes(),
    );
    let mut rows = create_sim_rows(1114);
    let mut log = ChangesetLog::recover(sim_file(114), 0, |_, _| {}).unwrap();
    let place_a = append_and_index(&mut log, &mut rows, 1, &payload_a);
    let place_b = append_and_index(&mut log, &mut rows, 2, &payload_b);
    let mut metrics = PieceMetrics::default();
    let cap = to_u64(PIECE_BYTES);

    for block in 0..9u64 {
      let off = block * cap;
      assert_sim_one_o1_piece(
        &log,
        &mut rows,
        &mut metrics,
        (id_a, place_a, &payload_a),
        off,
      );
      assert_sim_one_o1_piece(
        &log,
        &mut rows,
        &mut metrics,
        (id_b, place_b, &payload_b),
        off,
      );
    }

    // Unaligned read spanning blocks 0 and 1: 1 disk read, <= 2 * PIECE_BYTES hashed.
    let unaligned = PIECE_BYTES - 16;
    let hashed_before = metrics.bytes_hashed;
    let (span, reads) = sim_read_counted(
      &log,
      &mut rows,
      &mut metrics,
      &id_a,
      place_a,
      to_u64(unaligned),
    );
    assert_eq!(
      (
        span.unwrap().as_slice(),
        reads,
        metrics.bytes_hashed - hashed_before
      ),
      (&payload_a[unaligned..unaligned + PIECE_BYTES], 1, 2 * cap)
    );
  }

  /// Proves `payloads::forget` deletes `B` rows of the forgotten multi-piece occurrence while
  /// keeping a duplicate occurrence's `B` rows intact so reads continue verifying after promotion.
  fn assert_sim_forget_cleans_block_digests_and_keeps_duplicate() {
    let dup = pattern_bytes(3 * PIECE_BYTES + 17, 41, 9);
    let id = *blake3::hash(&dup).as_bytes();
    let mut rows = create_sim_rows(1115);
    let mut log = ChangesetLog::recover(sim_file(115), 0, |_, _| {}).unwrap();
    let first = append_and_index(&mut log, &mut rows, 1, &dup);
    let second = append_and_index(&mut log, &mut rows, 2, &dup);

    let mut scratch = Vec::new();
    assert!(rows.get(&block_key(first.0, 0), &mut scratch).unwrap());
    assert!(rows.get(&block_key(second.0, 0), &mut scratch).unwrap());
    payloads::forget(&mut rows, &id, first.0, &mut scratch).unwrap();

    for blk in 0..4u64 {
      assert!(
        !rows.get(&block_key(first.0, blk), &mut scratch).unwrap(),
        "forgotten occurrence block {blk} must be deleted"
      );
      assert!(
        rows.get(&block_key(second.0, blk), &mut scratch).unwrap(),
        "surviving duplicate block {blk} must stay intact"
      );
    }
    let mut metrics = PieceMetrics::default();
    let (got, _) = read_sim_all_pieces(&log, &mut rows, &mut metrics, &dup, second, 0).unwrap();
    assert_eq!(got, dup);
  }

  /// Proves `validate_sizing_bounds` rejects `memory: 0` / `extent_bytes: 0` and
  /// `ChangesetLog::recover_sealed(..., None, ...)` resyncs records carrying another directory's
  /// grant id after a mid-file header bit-flip.
  fn assert_sim_sizing_validation_and_grantless_resync() {
    let valid = Sizing {
      memory: 8 << 20,
      disk: 1 << 30,
      extent_bytes: 128 << 10,
      fanout: 8,
    };
    assert!(validate_sizing_bounds(valid).is_ok());
    assert!(matches!(
      validate_sizing_bounds(Sizing { memory: 0, ..valid }),
      Err(StoreError::Sizing(_))
    ));
    assert!(matches!(
      validate_sizing_bounds(Sizing {
        extent_bytes: 0,
        ..valid
      }),
      Err(StoreError::Sizing(_))
    ));

    let seal = [0x77u8; SEAL_BYTES];
    let sim = sim_file(116);
    sim.write_all_at(&encode_seal_preamble(&seal), 0).unwrap();
    sim.sync_data().unwrap();
    let start = to_u64(SEAL_PREAMBLE_BYTES);
    let mut log = ChangesetLog::recover_sealed(sim, start, seal, None, |_, _| {}).unwrap();
    let r1 = append_sim_with_grant(&mut log, 1, 999, b"old dir record 1");
    let r2 = append_sim_with_grant(&mut log, 2, 999, b"old dir record 2");
    let r3 = append_sim_with_grant(&mut log, 3, 999, b"old dir record 3");
    let sim = log.into_file();
    sim
      .inject(Fault::BitFlip {
        offset: r1.offset + 12,
        bit: 1,
        stored: true,
      })
      .unwrap();
    let mut found = Vec::new();
    let recovered =
      ChangesetLog::recover_sealed(sim, start, seal, None, |_, entry| found.push(*entry)).unwrap();
    assert_eq!((found, recovered.end()), (vec![r2, r3], r3.end()));
  }

  /// Proves `payloads::index_checkpoint` rebuilds multi-piece `B` rows when a checkpoint carries
  /// `[CONTENTS]` without `[BLOCK_DIGESTS]` so subsequent `read_verified_span_with` calls succeed.
  fn assert_sim_index_checkpoint_rebuilds_missing_block_digests() {
    let mut rows = create_sim_rows(1117);
    let mut log = ChangesetLog::recover(sim_file(117), 0, |_, _| {}).unwrap();
    let seal = log.seal();
    let multi = pattern_bytes(3 * PIECE_BYTES + 11, 19, 4);
    let located = append_sim(&mut log, 1, &multi);
    let sec = log.sections(&located).unwrap()[0];
    let mut value = Vec::new();
    let mut skipped = 0u64;
    put_changeset_counted(
      &mut rows,
      log.file(),
      &seal,
      &located,
      &mut value,
      &mut skipped,
    )
    .unwrap();
    // Simulate an older checkpoint that had the `[CONTENTS]` marker but no `B` rows / `[BLOCK_DIGESTS]`.
    rows.put(&[super::super::CONTENTS], &[]).unwrap();
    rows.checkpoint(log.end()).unwrap();
    assert!(!rows.get(&block_key(sec.offset, 0), &mut value).unwrap());

    let ckpt_skipped = payloads::index_checkpoint(
      &mut rows,
      log.file(),
      (log.end(), &seal),
      CHANGESET_ENTRY_BYTES,
    )
    .unwrap();
    assert_eq!(ckpt_skipped, 0);
    let mut metrics = PieceMetrics::default();
    let (got, _) = read_sim_all_pieces(
      &log,
      &mut rows,
      &mut metrics,
      &multi,
      (sec.offset, sec.len),
      0,
    )
    .unwrap();
    assert_eq!((got, metrics.verifications), (multi, 4));
  }

  /// Round-4 (`B`-row O(1) work, `forget` cleanup, `validate_sizing_bounds`, `index_checkpoint`
  /// `B`-row rebuild, and `expected_grant: None` resync `SimFile` oracle): Do: (1) interleave
  /// aligned and unaligned reads across two 512 KiB payloads, (2) forget one of two duplicate
  /// multi-piece occurrences and verify `B`-row cleanup and survivor reads, (3) validate sizing
  /// bounds and resync after directory copy, and (4) rebuild missing `B` rows via
  /// `payloads::index_checkpoint`.
  /// Expect: 1 disk read and `<= 2 * PIECE_BYTES` hashed per call (zero cache cliff), clean `B`-row
  /// deletion without disturbing duplicates, sizing rejection before file creation, full trailing
  /// recovery with `expected_grant: None`, and automatic `B`-row backfill on older checkpoints.
  #[test]
  fn sim_b_rows_o1_work_oversized_interleaved_and_forget_cleanup() {
    assert_sim_o1_interleaved_and_unaligned_reads();
    assert_sim_forget_cleans_block_digests_and_keeps_duplicate();
    assert_sim_sizing_validation_and_grantless_resync();
    assert_sim_index_checkpoint_rebuilds_missing_block_digests();
  }
}
