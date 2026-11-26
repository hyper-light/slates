//! Multi-repository `Store::open` recovery isolation tests (SEAM-1): a bit-flip inside one
//! repository's record in `changesets` must never block indexing or serving healthy repositories
//! written after it, while never indexing or serving corrupt bytes and still stopping at torn
//! uncommitted tails.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};

use super::{Contents, LOG_FILE, Sizing, Store, StoreError};
use crate::log::{Header, LogError, SectionKind, Tool};
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
  let hg_rows = store.changesets(2, 0, 10).unwrap();
  assert_eq!(hg_rows.len(), 1);
  assert_eq!(hg_rows[0].identity, r2_id);
  assert_eq!(
    read_piece_whole(&mut store, &hg_bytes),
    Some(hg_bytes.clone())
  );
  let svn_rows = store.changesets(3, 0, 10).unwrap();
  assert_eq!(svn_rows.len(), 1);
  assert_eq!(svn_rows[0].identity, r3_id);
  assert_eq!(
    read_piece_whole(&mut store, &svn_bytes),
    Some(svn_bytes.clone())
  );
  assert_eq!(store.log().end(), r3_end);

  let r4_bytes = b"fresh git push after isolated recovery".to_vec();
  let r4 = land_for(&mut store, Tool::Git, 4, 1, &r4_bytes);
  assert_eq!(r4.offset, r3_end);
  drop(store);

  let mut reopened = open_store(dir.path());
  assert!(reopened.changesets(1, 0, 10).unwrap().is_empty());
  assert_eq!(read_piece_whole(&mut reopened, &hg_bytes), Some(hg_bytes));
  assert_eq!(read_piece_whole(&mut reopened, &svn_bytes), Some(svn_bytes));
  assert_eq!(read_piece_whole(&mut reopened, &r4_bytes), Some(r4_bytes));
  assert_eq!(reopened.log().end(), r4.end());
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

/// SEAM-1 item 3: Do: land Git (1) and Hg (2), checkpoint `Store` so `rows` holds pre-corruption
/// `C`/`P` rows at `applied`, flip 1 bit in Git's payload on disk, and reopen `Store`.
/// Expect: reading Git's payload through `Store::piece` fails closed with `LogError::Corrupt`
/// instead of returning corrupt bytes, while healthy Hg still reads exact bytes.
#[test]
fn checkpointed_stale_row_does_not_mask_corrupt_payload_on_piece_read() {
  let dir = ScratchDir::new("seam1-checkpointed");
  let git_bytes = b"checkpointed git payload before disk bit flip".to_vec();
  let hg_bytes = b"checkpointed hg payload staying intact".to_vec();
  let r1_payload_offset = {
    let mut store = open_store(dir.path());
    let r1 = land_for(&mut store, Tool::Git, 1, 1, &git_bytes);
    let _r2 = land_for(&mut store, Tool::Mercurial, 2, 1, &hg_bytes);
    store.checkpoint().unwrap();
    store.log().sections(&r1).unwrap()[1].offset
  };
  flip_bit_in_file(&dir.path().join(LOG_FILE), r1_payload_offset + 5, 4);

  let mut store = open_store(dir.path());
  let staging = Staging::new(0);
  let git_id = *blake3::hash(&git_bytes).as_bytes();
  let git_len = u64::try_from(git_bytes.len()).unwrap();
  let git_read = store.piece(&staging, &git_id, git_len, 0);
  assert!(
    matches!(git_read, Err(StoreError::Log(LogError::Corrupt(_)))),
    "corrupt checkpointed payload must refuse Corrupt, got {git_read:?}"
  );
  assert_eq!(read_piece_whole(&mut store, &hg_bytes), Some(hg_bytes));
}
