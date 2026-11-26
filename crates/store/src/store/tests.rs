//! Multi-repository `Store::open` recovery isolation tests (SEAM-1): a bit-flip inside one
//! repository's record in `changesets` must never block indexing or serving healthy repositories
//! written after it, while never indexing or serving corrupt bytes and still stopping at torn
//! uncommitted tails.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use hyper_block::block::BlockFile;
use slates_land::store_root::{StoreGrant, StoreRoot};

use super::{
  CHANGESET_ENTRY_BYTES, Contents, LOG_FILE, LogHead, PIECE_BYTES, PieceDigests, ROWS_FILE, Sizing,
  Store, StoreError, inspect_log_file, payloads, prepare_damaged_preamble_log, prepare_empty_log,
  prepare_sealed_log, put_changeset_counted, read_seal_row, read_verified_piece,
};
use crate::log::{
  ChangesetLog, FORMAT, HEADER_BYTES, Header, LogError, SEAL_BYTES, SEAL_PREAMBLE_BYTES,
  SECTION_HEAD_BYTES, SectionKind, TRAILER_BYTES, Tool, encode_seal_preamble,
};
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
/// reopens, and asserts the VERY FIRST `Store::piece` call at `start_offset` refuses
/// `LogError::Corrupt` (even when `flip_at` lies outside the requested window).
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
  let staging = Staging::new(0);
  let id = *blake3::hash(&git_bytes).as_bytes();
  let first = store.piece(&staging, &id, to_u64(git_bytes.len()), start_offset);
  assert!(
    matches!(first, Err(StoreError::Log(LogError::Corrupt(_)))),
    "{name}: very first Store::piece(offset={start_offset}) must refuse Corrupt, got {first:?}"
  );
  assert_eq!(store.payload_verifications(), 0);
  assert_eq!(read_piece_whole(&mut store, &hg_bytes), Some(hg_bytes));
}

/// SEAM-1 item 3 / Blocker 3 / A5: Do: land and checkpoint payloads of (a) 1 piece (1,000 B),
/// (b) exact 65,536 B read from suffix offset 1, (c) 200,000 B with a bit-flip in piece 1 (+5)
/// and in piece 3 (+150,000) requested at offset 0 and offset 1; and (d) a healthy 131,089-byte
/// 3-piece payload.
/// Expect: (a)-(c) all refuse `LogError::Corrupt` on the very first `Store::piece` call before
/// returning any byte while healthy Hg reads intact; (d) returns exact bytes in 3 pieces, verifying
/// every disk piece read against BLAKE3 (`payload_verifications() == 3`).
#[test]
fn checkpointed_stale_row_does_not_mask_corrupt_payload_on_piece_read() {
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-1piece", 1_000, 5, 0);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-suffix", PIECE_BYTES, 5, 1);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-first", 200_000, 5, 0);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-third-off0", 200_000, 150_000, 0);
  assert_corrupt_checkpointed_piece_refused("seam1-ckpt-mp-third-off1", 200_000, 150_000, 1);

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

  let mut future = encode_seal_preamble(&[0x33u8; SEAL_BYTES]);
  future[4..6].copy_from_slice(&3u16.to_le_bytes());
  assert_open_refused_without_mutation("seam3-refuse-v3", &[(LOG_FILE, &future)], Some(3));

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

/// Pure `SimFile` unit tests for piece authentication, `SLSE` preamble / format versioning /
/// index rebuild, and corrupt-row accounting (runnable without `StoreRoot::identify`).
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
    digests: &mut PieceDigests,
    payload: &[u8],
    place: (u64, u64),
    start_offset: u64,
  ) -> Result<(Vec<u8>, usize), StoreError> {
    let id = *blake3::hash(payload).as_bytes();
    let (mut out, mut offset, mut pieces) = (Vec::new(), start_offset, 0usize);
    while offset < place.1 {
      let chunk = read_verified_piece(log, digests, &id, place, offset)?;
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
    let mut digests = PieceDigests::with_budget(PIECE_BYTES);
    let id = *blake3::hash(&bytes).as_bytes();
    let first = read_verified_piece(
      &log,
      &mut digests,
      &id,
      (section.offset, section.len),
      start_offset,
    );
    assert!(
      matches!(first, Err(StoreError::Log(LogError::Corrupt(_)))),
      "very first piece read (flip +{flip_at}, start {start_offset}) must refuse Corrupt, got {first:?}"
    );
    assert_eq!(digests.verifications(), 0);
  }

  /// Blocker 3 / A5 (`SimFile` oracle): Do: run `read_verified_piece` on (a) 1-piece control,
  /// (b) suffix `offset = 1` on 65,536-byte payload with bit-flip, (c) 200,000-byte payload
  /// flipped at +5 and +150,000 requested at offset 0 and offset 1, and (d) healthy 131,089-byte
  /// 3-piece reads.
  /// Expect: (a)-(c) refuse `LogError::Corrupt` on the very first piece call; (d) returns exact
  /// bytes in 3 pieces verifying on EVERY disk piece read (`verifications == 3`, and `6` after a
  /// second full read).
  #[test]
  fn sim_piece_verification_covers_one_piece_suffix_and_multipiece() {
    assert_sim_flip_refused(90, 1_000, 5, 0);
    assert_sim_flip_refused(91, PIECE_BYTES, 5, 1);
    assert_sim_flip_refused(92, 200_000, 5, 0);
    assert_sim_flip_refused(93, 200_000, 150_000, 0);
    assert_sim_flip_refused(98, 200_000, 150_000, 1);

    let healthy = pattern_bytes(131_089, 17, 3);
    let mut log = ChangesetLog::recover(sim_file(94), 0, |_, _| {}).unwrap();
    let r1 = append_sim(&mut log, 1, &healthy);
    let s1 = log.sections(&r1).unwrap()[0];
    let mut digests = PieceDigests::with_budget(PIECE_BYTES);
    let (got, pieces) =
      read_sim_pieces(&log, &mut digests, &healthy, (s1.offset, s1.len), 0).unwrap();
    assert_eq!(
      (got, pieces, digests.verifications()),
      (healthy.clone(), 3, 3)
    );
    let (again, pieces2) =
      read_sim_pieces(&log, &mut digests, &healthy, (s1.offset, s1.len), 0).unwrap();
    assert_eq!((again, pieces2, digests.verifications()), (healthy, 3, 6));
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
      Some(7),
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

  /// Damages `log_file`'s preamble, checks refusal without `rows`, and verifies backup `SEAL_ROW`
  /// recovery with `saved_rows`.
  fn assert_sim_damaged_preamble_fallback(
    log_file: SimFile,
    saved_rows: ShardDb<SimFile>,
    seal: [u8; SEAL_BYTES],
    expected: &[crate::log::Located],
  ) {
    let (config, trunk) = sim_db_config();
    log_file
      .inject(Fault::BitFlip {
        offset: 10,
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
      Some(7),
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
  }

  /// Round-3 Blocker 2 (`SimFile` oracle for `SLSE` preamble, crash durability, `rows` rebuild,
  /// and backup `SEAL_ROW` fallback): Do: initialize a fresh store log via `prepare_empty_log`,
  /// land records, crash `LoseAll`, rebuild with a brand-new `ShardDb`, and then damage the
  /// preamble and recover via backup `SEAL_ROW`.
  /// Expect: preamble survives crash, rebuild recovers 100% of records with `log` byte-identical,
  /// damaged preamble without `rows` fails `Corrupt`, and damaged preamble with backup `SEAL_ROW`
  /// recovers 100% of records.
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
    let mut log = ChangesetLog::recover_sealed(
      opened.log_file,
      opened.replay_from,
      seal,
      Some(7),
      |_, _| {},
    )
    .unwrap();
    let r1 = append_sim(&mut log, 1, b"first sealed changeset");
    let r2 = append_sim(&mut log, 2, b"second sealed changeset");
    let log_file = log.into_file();
    log_file.crash(Crash::LoseAll).unwrap();
    let log_image_before = log_file.durable_image().unwrap();

    let log_file = assert_sim_rebuild_from_sealed_log(log_file, seal, &[r1, r2], &log_image_before);
    assert_sim_damaged_preamble_fallback(log_file, rows, seal, &[r1, r2]);
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

  /// Round-3 Blocker 2 (`SimFile` format-version & seal-mismatch refusal oracle): Do: run
  /// `inspect_log_file` on legacy `FORMAT = 1` `SLCS` and future `FORMAT = 3` `SLSE`, and run
  /// `prepare_sealed_log` when `rows` carries a different store's `SEAL_ROW`.
  /// Expect: `UnsupportedFormat(1)`, `UnsupportedFormat(3)`, and `Corrupt("a store seal mismatch")`,
  /// each with zero bytes written to the log file.
  #[test]
  fn sim_inspect_log_refuses_foreign_formats_and_mismatched_seal_row() {
    let mut v1_head = [0u8; HEADER_BYTES];
    v1_head[..4].copy_from_slice(b"SLCS");
    v1_head[4..6].copy_from_slice(&1u16.to_le_bytes());
    assert_sim_foreign_format_refused(105, &v1_head, 1);

    let mut v3_preamble = encode_seal_preamble(&[0x33u8; SEAL_BYTES]);
    v3_preamble[4..6].copy_from_slice(&(FORMAT + 1).to_le_bytes());
    assert_sim_foreign_format_refused(106, &v3_preamble, FORMAT + 1);

    let (config, trunk) = sim_db_config();
    let mut foreign_rows = ShardDb::create(sim_file(107), config, 1 << 20, trunk).unwrap();
    foreign_rows
      .put(&[super::super::SEAL_ROW], &[0xAAu8; SEAL_BYTES])
      .unwrap();
    let mismatch = prepare_sealed_log(
      sim_file(108),
      [0xBBu8; SEAL_BYTES],
      (foreign_rows, 0, config.page_size),
    );
    assert!(matches!(
      mismatch,
      Err(StoreError::Log(LogError::Corrupt(_)))
    ));
  }

  /// Reads one verified piece and returns both its result and the exact `SimFile` read call delta.
  fn sim_read_counted(
    log: &ChangesetLog<SimFile>,
    digests: &mut PieceDigests,
    id: &[u8; blake3::OUT_LEN],
    place: (u64, u64),
    offset: u64,
  ) -> (Result<Vec<u8>, StoreError>, u64) {
    let before = log.file().stats().unwrap().reads;
    let res = read_verified_piece(log, digests, id, place, offset);
    let delta = log.file().stats().unwrap().reads - before;
    (res, delta)
  }

  /// Asserts a piece read failed closed with [`LogError::Corrupt`].
  fn assert_corrupt_piece(res: Result<Vec<u8>, StoreError>, label: &str) {
    assert!(
      matches!(res, Err(StoreError::Log(LogError::Corrupt(_)))),
      "{label}: expected Corrupt, got {res:?}"
    );
  }

  /// Caches piece 0, injects a bit-flip at `flip_at`, asserts reading the corrupted block at
  /// `corrupt_read` refuses `Corrupt`, and asserts reading the intact sibling block at
  /// `eviction_probe` also refuses `Corrupt` (proving mismatch evicted the cache entry).
  fn assert_sim_a5_flip_caught_and_evicts(
    seed: u64,
    payload: &[u8],
    flip_at: u64,
    corrupt_read: u64,
    eviction_probe: u64,
  ) {
    let id = *blake3::hash(payload).as_bytes();
    let mut log = ChangesetLog::recover(sim_file(seed), 0, |_, _| {}).unwrap();
    let located = append_sim(&mut log, 1, payload);
    let section = log.sections(&located).unwrap()[0];
    let place = (section.offset, section.len);
    let mut digests = PieceDigests::with_budget(PIECE_BYTES);

    let piece0 = read_verified_piece(&log, &mut digests, &id, place, 0).unwrap();
    assert_eq!(
      (piece0.as_slice(), digests.verifications()),
      (&payload[..PIECE_BYTES], 1)
    );

    log
      .file()
      .inject(Fault::BitFlip {
        offset: section.offset + flip_at,
        bit: 2,
        stored: true,
      })
      .unwrap();
    let caught = read_verified_piece(&log, &mut digests, &id, place, corrupt_read);
    assert_corrupt_piece(caught, "A5 post-cache corrupted block read");
    let after_evict = read_verified_piece(&log, &mut digests, &id, place, eviction_probe);
    assert_corrupt_piece(after_evict, "A5 post-eviction sibling probe");
    assert_eq!(digests.verifications(), 1);
  }

  /// Round-3 A5 (`SimFile` oracle): Do: read piece 0 of a 3-piece landed payload cleanly, then
  /// flip 1 stored bit on disk (both inside piece 1 and inside piece 0), read the corrupted piece,
  /// and then probe the intact sibling piece.
  /// Expect: reading the corrupted piece refuses `Err(StoreError::Log(LogError::Corrupt(_)))`
  /// against its per-block digest AND evicts the cached entry so the subsequent sibling read also
  /// refuses `Corrupt` via full-payload re-verification.
  #[test]
  fn sim_a5_bitflip_after_first_piece_is_caught_on_second_piece() {
    let payload = pattern_bytes(131_089, 23, 5);
    assert_sim_a5_flip_caught_and_evicts(
      110,
      &payload,
      to_u64(PIECE_BYTES) + 17,
      to_u64(PIECE_BYTES),
      0,
    );
    assert_sim_a5_flip_caught_and_evicts(111, &payload, 17, 0, to_u64(PIECE_BYTES));
  }

  /// Proves O(N) disk reads across 4 sequential piece reads (`4 + 1 + 1 + 1 = 7 < 16`).
  fn assert_sim_four_piece_linear_reads() {
    let payload = pattern_bytes(4 * PIECE_BYTES, 41, 9);
    let mut log = ChangesetLog::recover(sim_file(112), 0, |_, _| {}).unwrap();
    let located = append_sim(&mut log, 1, &payload);
    let section = log.sections(&located).unwrap()[0];
    let id = *blake3::hash(&payload).as_bytes();
    let mut digests = PieceDigests::with_budget(PIECE_BYTES);
    let (mut per_piece, mut out) = (Vec::new(), Vec::new());
    for piece_idx in 0..4usize {
      let offset = to_u64(piece_idx * PIECE_BYTES);
      let (res, reads) = sim_read_counted(
        &log,
        &mut digests,
        &id,
        (section.offset, section.len),
        offset,
      );
      per_piece.push(reads);
      out.extend_from_slice(&res.unwrap());
    }
    let total: u64 = per_piece.iter().sum();
    assert_eq!(out, payload);
    assert_eq!(
      (per_piece, total, digests.verifications()),
      (vec![4, 1, 1, 1], 7, 4)
    );
    assert!(total < 16);
  }

  /// Proves an unaligned read spanning blocks 0 and 1 verifies only those 2 blocks when clean,
  /// refuses `Corrupt` when block 1 is flipped after piece 0 is cached, and evicts the entry.
  fn assert_sim_unaligned_boundary_read_and_bitflip() {
    let payload = pattern_bytes(4 * PIECE_BYTES, 53, 13);
    let mut log = ChangesetLog::recover(sim_file(113), 0, |_, _| {}).unwrap();
    let located = append_sim(&mut log, 1, &payload);
    let section = log.sections(&located).unwrap()[0];
    let place = (section.offset, section.len);
    let id = *blake3::hash(&payload).as_bytes();
    let mut digests = PieceDigests::with_budget(PIECE_BYTES);

    let (piece0, miss_reads) = sim_read_counted(&log, &mut digests, &id, place, 0);
    assert_eq!(
      (piece0.unwrap().as_slice(), miss_reads),
      (&payload[..PIECE_BYTES], 4)
    );

    let unaligned = PIECE_BYTES - 16;
    let (clean, span_reads) = sim_read_counted(&log, &mut digests, &id, place, to_u64(unaligned));
    assert_eq!(
      (
        clean.unwrap().as_slice(),
        span_reads,
        digests.verifications()
      ),
      (&payload[unaligned..unaligned + PIECE_BYTES], 2, 2)
    );

    log
      .file()
      .inject(Fault::BitFlip {
        offset: section.offset + to_u64(PIECE_BYTES) + 24,
        bit: 5,
        stored: true,
      })
      .unwrap();
    let (corrupt, _) = sim_read_counted(&log, &mut digests, &id, place, to_u64(unaligned));
    assert_corrupt_piece(corrupt, "unaligned read spanning flipped block 1");
    let (evicted, _) = sim_read_counted(&log, &mut digests, &id, place, 0);
    assert_corrupt_piece(evicted, "block 0 read after unaligned mismatch eviction");
    assert_eq!(digests.verifications(), 2);
  }

  /// Appends one payload changeset to `log` and returns its `(offset, len)` section range.
  fn append_sim_place(log: &mut ChangesetLog<SimFile>, seq: u64, bytes: &[u8]) -> (u64, u64) {
    let located = append_sim(log, seq, bytes);
    let sec = log.sections(&located).unwrap()[0];
    (sec.offset, sec.len)
  }

  /// Proves FIFO eviction under a 1-entry budget re-verifies cleanly on re-read without unbounded
  /// growth, and an oversized entry falls back cleanly without evicting resident entries.
  fn assert_sim_digest_budget_eviction_and_oversized_fallback() {
    let a = pattern_bytes(4 * PIECE_BYTES, 11, 1);
    let b = pattern_bytes(4 * PIECE_BYTES, 17, 2);
    let c = pattern_bytes(5 * PIECE_BYTES, 23, 3);
    let mut log = ChangesetLog::recover(sim_file(114), 0, |_, _| {}).unwrap();
    let place_a = append_sim_place(&mut log, 1, &a);
    let place_b = append_sim_place(&mut log, 2, &b);
    let place_c = append_sim_place(&mut log, 3, &c);
    let (id_a, id_b, id_c) = (
      *blake3::hash(&a).as_bytes(),
      *blake3::hash(&b).as_bytes(),
      *blake3::hash(&c).as_bytes(),
    );
    let mut digests = PieceDigests::with_budget(4 * blake3::OUT_LEN);
    let p1 = to_u64(PIECE_BYTES);

    let (_, a_miss) = sim_read_counted(&log, &mut digests, &id_a, place_a, 0);
    let (_, a_hit) = sim_read_counted(&log, &mut digests, &id_a, place_a, p1);
    let (_, b_miss) = sim_read_counted(&log, &mut digests, &id_b, place_b, 0);
    let (_, b_hit) = sim_read_counted(&log, &mut digests, &id_b, place_b, p1);
    assert_eq!((a_miss, a_hit, b_miss, b_hit), (4, 1, 4, 1));

    // A was evicted when B filled the 1-entry budget; re-reading A misses (4 reads) and re-verifies.
    let (a_re, a_re_reads) = sim_read_counted(&log, &mut digests, &id_a, place_a, p1);
    assert_eq!(
      (a_re.unwrap().as_slice(), a_re_reads),
      (&a[PIECE_BYTES..2 * PIECE_BYTES], 4)
    );

    // Oversized 5-piece C exceeds budget: both reads fall back to full stream without evicting A.
    let (c0, c0_reads) = sim_read_counted(&log, &mut digests, &id_c, place_c, 0);
    let (c1, c1_reads) = sim_read_counted(&log, &mut digests, &id_c, place_c, p1);
    let (_, a_still_hit) = sim_read_counted(&log, &mut digests, &id_a, place_a, 2 * p1);
    assert_eq!(
      (c0.unwrap().len(), c1.unwrap().len()),
      (PIECE_BYTES, PIECE_BYTES)
    );
    assert_eq!((c0_reads, c1_reads, a_still_hit), (5, 5, 1));
  }

  /// Proves `PieceDigests::from_sizing` handles `fanout = 0` and floors tiny memory at `PIECE_BYTES`.
  fn assert_sim_sizing_floor_caches_without_division_by_zero() {
    let payload = pattern_bytes(4 * PIECE_BYTES, 29, 7);
    let mut log = ChangesetLog::recover(sim_file(115), 0, |_, _| {}).unwrap();
    let place = append_sim_place(&mut log, 1, &payload);
    let id = *blake3::hash(&payload).as_bytes();
    let mut digests = PieceDigests::from_sizing(Sizing {
      memory: 1,
      disk: 1 << 20,
      extent_bytes: 128 << 10,
      fanout: 0,
    });
    let (_, miss) = sim_read_counted(&log, &mut digests, &id, place, 0);
    let (_, hit) = sim_read_counted(&log, &mut digests, &id, place, to_u64(PIECE_BYTES));
    assert_eq!((miss, hit), (4, 1));
  }

  /// Round-3 A5 (`PieceDigests` oracle): Do: (1) stream a 4-piece payload piece by piece,
  /// (2) perform an unaligned read spanning blocks 0 and 1 before and after flipping block 1,
  /// (3) exercise FIFO eviction and oversized fallback under a 1-entry budget, and (4) verify
  /// `PieceDigests::from_sizing` floors safely when `fanout == 0`.
  /// Expect: O(N) reads (`7 < 16`), clean unaligned hits (`2` reads), immediate `Corrupt` +
  /// eviction on post-cache block-1 bit-flip, bounded FIFO eviction without growth, and safe sizing.
  #[test]
  fn sim_piece_digests_linear_reads_unaligned_flip_and_bounded_eviction() {
    assert_sim_four_piece_linear_reads();
    assert_sim_unaligned_boundary_read_and_bitflip();
    assert_sim_digest_budget_eviction_and_oversized_fallback();
    assert_sim_sizing_floor_caches_without_division_by_zero();
  }
}
