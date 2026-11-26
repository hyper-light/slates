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
  open_with(dir, 8 << 20)
}

/// Opens the store in `dir` lent `memory` bytes.
fn open_with(dir: &Path, memory: usize) -> Store {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  let root = StoreRoot::open(dir, &grant).unwrap();
  Store::open(
    root,
    Sizing {
      memory,
      disk: 1 << 30,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
  .unwrap()
}

/// Lands one changeset of `repository` at `sequence`: a record section and a payload.
fn land(store: &mut Store, repository: u128, sequence: u64, payload: &[u8]) -> [u8; 32] {
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
  let mut read = vec![0u8; usize::try_from(payload.len).unwrap()];
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

/// Reads the whole content `identity` of `len` bytes through `piece`, a piece at a time.
fn read_whole(
  store: &mut Store,
  staging: &slates_store::staging::Staging,
  identity: &[u8; 32],
  len: u64,
) -> Vec<u8> {
  let mut whole = Vec::new();
  while (whole.len() as u64) < len {
    let piece = store
      .piece(staging, identity, len, whole.len() as u64)
      .unwrap()
      .unwrap();
    whole.extend_from_slice(&piece);
  }
  whole
}

/// Do: stage a 300,001-byte pack, read it through `piece`, land it as the payload, release it, read it again, reopen
/// the store with and without a checkpoint and read it again. Expect: the staged read is borrowed (no copy) and every
/// read is the same bytes, the landed ones from the log by identity, a piece at a time.
#[test]
fn a_staged_content_reads_the_same_in_flight_landed_and_recovered() {
  use slates_store::staging::Staging;
  use slates_store::store::Contents;
  let dir = scratch("seam");
  let pack: Vec<u8> = (0..300_001u32)
    .map(|at| (at.wrapping_mul(2_654_435_761) >> 24) as u8)
    .collect();
  let len = pack.len() as u64;
  let mut staging = Staging::new(1 << 20);
  let identity = {
    let mut store = open(&dir);
    let identity = staging.stage(pack.clone()).unwrap();
    let staged = store.piece(&staging, &identity, len, 0).unwrap().unwrap();
    assert!(
      matches!(staged, std::borrow::Cow::Borrowed(_)),
      "in flight: lent, not copied"
    );
    drop(staged);
    assert_eq!(read_whole(&mut store, &staging, &identity, len), pack);
    let header = Header {
      tool: Tool::Git,
      repository: 5,
      sequence: 1,
      judged_over: 0,
      grant: 1,
      work: 9,
      time_ns: 1,
      sections: 0,
    };
    store
      .land(
        header,
        &[
          (SectionKind::Record, Contents::Bytes(b"refs")),
          (SectionKind::Payload, Contents::Staged(identity)),
        ],
        &staging,
      )
      .unwrap();
    staging.release(&identity);
    assert_eq!(staging.bytes(), 0, "landed: RAM holds none of it");
    assert_eq!(
      read_whole(&mut store, &staging, &identity, len),
      pack,
      "landed: read from the log"
    );
    identity
  };
  let mut store = open(&dir);
  assert_eq!(
    read_whole(&mut store, &staging, &identity, len),
    pack,
    "recovered from the log alone"
  );
  store.checkpoint().unwrap();
  drop(store);
  let mut store = open(&dir);
  assert_eq!(
    read_whole(&mut store, &staging, &identity, len),
    pack,
    "recovered from the checkpoint"
  );
  assert!(
    store.piece(&staging, &[7u8; 32], 3, 0).unwrap().is_none(),
    "an identity nothing holds"
  );
}

/// w-perf's finding. Do: land a payload through `begin` a slice at a time, then `applied`; read it by its identity.
/// Expect: the bytes, as a changeset landed whole through `land` gives them (its `P` row put from the log).
#[test]
fn a_changeset_landed_in_slices_is_indexed_as_one_landed_whole() {
  use slates_store::staging::Staging;
  let dir = scratch("slices");
  let mut store = open(&dir);
  let payload: Vec<u8> = (0..150_000u32).map(|at| (at % 241) as u8).collect();
  let identity = *blake3::hash(&payload).as_bytes();
  let mut record = store
    .begin(Header {
      tool: Tool::Mercurial,
      repository: 6,
      sequence: 1,
      judged_over: 0,
      grant: 1,
      work: 1,
      time_ns: 1,
      sections: 1,
    })
    .unwrap();
  record
    .section(SectionKind::Payload, payload.len() as u64)
    .unwrap();
  for slice in payload.chunks(40_000) {
    record.piece(slice).unwrap();
  }
  let located = record.commit().unwrap();
  store.applied(&located).unwrap();
  let staging = Staging::new(0);
  let first = store
    .piece(&staging, &identity, payload.len() as u64, 0)
    .unwrap();
  assert!(
    first.is_some(),
    "the slices' payload is found by its identity"
  );
  assert_eq!(
    read_whole(&mut store, &staging, &identity, payload.len() as u64),
    payload
  );
}

/// Opens the store in `dir` under a standing grant issued at `issued_ns`.
fn open_issued(dir: &Path, issued_ns: u64) -> Store {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns,
  };
  Store::open(
    StoreRoot::open(dir, &grant).unwrap(),
    Sizing {
      memory: 8 << 20,
      disk: 1 << 30,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
  .unwrap()
}

/// A-767's `G` row (R10: the standing grant recorded). Do: open a store under a grant issued at 5 and drop it with no
/// stop (nothing checkpointed after its open, as a `kill -9` leaves it); open it again under a grant issued at 9; open
/// a store in another directory. Expect: the grant recorded at the first open is the one found after the crash (issued
/// at 5, same id, the directory's device and inode); the other directory's grant is another id.
#[test]
fn a_stores_standing_grant_is_recorded_once_and_found_after_a_crash() {
  let dir = scratch("grant");
  let target = StoreRoot::identify(&dir).unwrap();
  let first = *open_issued(&dir, 5).grant();
  assert_eq!(first.issued_ns, 5);
  assert_eq!((first.device, first.inode), (target.device, target.inode));
  let again = *open_issued(&dir, 9).grant();
  assert_eq!(again, first, "the grant as first recorded, after a crash");
  let other = scratch("grant-other");
  let elsewhere = *open_issued(&other, 9).grant();
  assert_ne!(elsewhere.id, first.id, "another directory, another grant");
  assert_eq!(elsewhere.issued_ns, 9);
}

/// A-767 item 7's sizing on a large machine (found by use, A-645 (a), 2026-11-20: every `repository create` with
/// `--vcs-store` refused `Overloaded` on a Mac Studio, the store's engine answering "a memtable past 4 GiB" since a
/// shard's reserve there is tens of GiB). Do: open a store lent 8 GiB of memory, past what a memtable's 32-bit offsets
/// address; land a changeset; read it back. Expect the landing taken and its row read back: the memtable is bounded by
/// what its offsets address, the rest of the loan unused by it.
#[test]
fn a_store_lent_more_memory_than_a_memtable_addresses_lands_and_reads() {
  let dir = scratch("past-4-gib");
  let mut store = open_with(&dir, 8 << 30);
  let landed = land(&mut store, 9, 0, b"changeset");
  let rows = store.changesets(9, 0, 10).unwrap();
  assert_eq!(
    rows.iter().map(|row| row.identity).collect::<Vec<_>>(),
    [landed]
  );
}

/// What a restart keeps after one of two repositories sharing a payload is forgotten (A-767 C3): the `survivor`
/// still lists its changeset and reads the payload's bytes, `expected`, from its `whole`, by piece and by place;
/// the `forgotten` repository lists none.
fn assert_the_survivor_reads_the_shared_payload(
  store: &mut Store,
  staging: &slates_store::staging::Staging,
  whole: slates_store::texts::Place,
  expected: &[u8],
  survivor: u128,
  forgotten: u128,
) {
  assert_eq!(store.changesets(survivor, 1, 1).unwrap().len(), 1);
  assert!(store.changesets(forgotten, 1, 1).unwrap().is_empty());
  assert_eq!(
    store
      .piece(staging, &whole.payload, whole.len, 0)
      .unwrap()
      .as_deref(),
    Some(expected),
    "the surviving repository still names this payload"
  );
  assert_eq!(
    store.places_bytes(staging, &[whole]).unwrap(),
    vec![expected.to_vec()]
  );
}

/// A-767 C3. Land identical payloads in two repositories, including two occurrences in one
/// changeset. Forget either repository first. The other still serves the bytes after a restart.
#[test]
fn forgetting_a_duplicate_payload_keeps_the_surviving_changesets_bytes() {
  use slates_store::staging::Staging;
  use slates_store::store::Contents;

  let payload = b"the bytes of both changesets";
  let identity = *blake3::hash(payload).as_bytes();
  let len = u64::try_from(payload.len()).unwrap();
  let staging = Staging::new(0);
  for (forgotten, survivor) in [(2, 1), (1, 2)] {
    let dir = scratch(&format!("shared-payload-{forgotten}"));
    let mut store = open(&dir);
    land(&mut store, 1, 1, payload);
    store
      .land(
        Header {
          tool: Tool::Mercurial,
          repository: 2,
          sequence: 1,
          judged_over: 0,
          grant: 1,
          work: 7,
          time_ns: 0,
          sections: 2,
        },
        &[
          (SectionKind::Payload, Contents::Bytes(payload)),
          (SectionKind::Payload, Contents::Bytes(payload)),
        ],
        &staging,
      )
      .unwrap();
    assert_eq!(
      store.piece(&staging, &identity, len, 0).unwrap().as_deref(),
      Some(payload.as_slice())
    );
    assert!(store.forget(forgotten, 1).unwrap() > 0);
    drop(store);
    let mut store = open(&dir);
    let whole = slates_store::texts::Place {
      payload: identity,
      offset: 0,
      len,
    };
    assert_the_survivor_reads_the_shared_payload(
      &mut store, &staging, whole, payload, survivor, forgotten,
    );
    assert_eq!(store.forget(forgotten, 1).unwrap(), 0);
    assert!(store.forget(survivor, 1).unwrap() > 0);
    drop(store);
    assert!(
      open(&dir)
        .piece(&staging, &identity, len, 0)
        .unwrap()
        .is_none()
    );
  }
}

/// A-767 C3. Replay two copies and a replacement for one at the same sequence. Forget the
/// other copy. Only the replacement's new payload remains readable after the next checkpoint.
#[test]
fn a_replaced_sequence_releases_its_duplicate_payload_during_replay() {
  use slates_store::staging::Staging;

  let dir = scratch("replaced-payload");
  let shared = b"also in the other repository";
  let new = b"replacement";
  let staging = Staging::new(0);
  {
    let mut store = open(&dir);
    let header = Header {
      tool: Tool::Git,
      repository: 1,
      sequence: 1,
      judged_over: 0,
      grant: 1,
      work: 1,
      time_ns: 1,
      sections: 1,
    };
    let mut appending = store.begin(header).unwrap();
    appending
      .section(SectionKind::Payload, shared.len() as u64)
      .unwrap();
    appending.piece(shared).unwrap();
    let original = appending.commit().unwrap();
    store.applied(&original).unwrap();
    land(&mut store, 2, 1, shared);
    store.checkpoint().unwrap();
    let newest = land(&mut store, 1, 1, new);
    store.applied(&original).unwrap();
    assert_eq!(store.changesets(1, 1, 1).unwrap()[0].identity, newest);
  }
  let read = |store: &mut Store, bytes: &[u8]| {
    store
      .piece(
        &staging,
        blake3::hash(bytes).as_bytes(),
        bytes.len() as u64,
        0,
      )
      .unwrap()
      .map(|piece| piece.into_owned())
  };
  let mut store = open(&dir);
  assert_eq!(read(&mut store, shared).as_deref(), Some(shared.as_slice()));
  assert!(store.forget(2, 1).unwrap() > 0);
  drop(store);
  let mut store = open(&dir);
  assert_eq!(read(&mut store, shared), None);
  assert_eq!(read(&mut store, new).as_deref(), Some(new.as_slice()));
}

/// The header of the oracle tests' changeset at `sequence` of repository 7.
fn oracle_header(sequence: u64) -> Header {
  Header {
    tool: Tool::Git,
    repository: 7,
    sequence,
    judged_over: sequence.saturating_sub(1),
    grant: 1,
    work: 500 + sequence,
    time_ns: sequence,
    sections: 2,
  }
}

/// A payload no two pieces of which are alike, `len` bytes.
fn oracle_payload(len: usize) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from((at * 31 + at / 251) % 256).unwrap())
    .collect()
}

/// Lands changeset `sequence` (a record section and `payload`) whole, one call ([`Store::land`]).
fn land_whole(store: &mut Store, sequence: u64, payload: &[u8]) -> slates_store::log::Located {
  store
    .land(
      oracle_header(sequence),
      &[
        (
          SectionKind::Record,
          slates_store::store::Contents::Bytes(b"refs"),
        ),
        (
          SectionKind::Payload,
          slates_store::store::Contents::Bytes(payload),
        ),
      ],
      &slates_store::staging::Staging::new(0),
    )
    .unwrap()
}

/// Opens changeset `sequence`'s landing in pieces and writes its record section and the first `pieces` parts of
/// `payload`, `piece` bytes a part: what a landing has written when it is cut off there.
fn land_pieces_to(
  store: &mut Store,
  (sequence, payload): (u64, &[u8]),
  (piece, pieces): (usize, usize),
) {
  store.open_landing(oracle_header(sequence)).unwrap();
  store.landing_section(SectionKind::Record, 4).unwrap();
  store.landing_part(b"refs").unwrap();
  store
    .landing_section(SectionKind::Payload, payload.len() as u64)
    .unwrap();
  for part in payload.chunks(piece).take(pieces) {
    store.landing_part(part).unwrap();
  }
}

/// The payload `identity` of `len` bytes read back whole from `store`, a piece at a time.
fn read_back(store: &mut Store, identity: &[u8; 32], len: u64) -> Option<Vec<u8>> {
  let staging = slates_store::staging::Staging::new(0);
  let mut read = Vec::new();
  while (read.len() as u64) < len {
    let piece = store
      .piece(&staging, identity, len, read.len() as u64)
      .unwrap()?;
    read.extend_from_slice(&piece);
  }
  Some(read)
}

/// Main's ruling of 2026-11-26 (b2b; CLAUDE.md §2 item 7: one landing path, whole equal to pieces by construction).
/// Do: land a changeset of a record section and a 200,000-byte payload whole (`Store::land`) in one store, and in
/// pieces of 997 bytes and of `PIECE_BYTES` in two others. Expect: the three stores' logs the same bytes, each listing
/// the one changeset at the same place, its payload read back byte for byte.
#[test]
fn a_changeset_landed_in_pieces_is_the_changeset_landed_whole_byte_for_byte() {
  use slates_store::store::{LOG_FILE, PIECE_BYTES};
  let payload = oracle_payload(200_000);
  let whole_dir = scratch("oracle-whole");
  let mut whole = open(&whole_dir);
  let landed = land_whole(&mut whole, 1, &payload);
  let reference = std::fs::read(whole_dir.join(LOG_FILE)).unwrap();
  for piece in [997, PIECE_BYTES] {
    let dir = scratch(&format!("oracle-pieces-{piece}"));
    let mut store = open(&dir);
    land_pieces_to(&mut store, (1, &payload), (piece, usize::MAX));
    let located = store.commit_landing().unwrap();
    assert_eq!(
      located, landed,
      "landed at the same place, pieces of {piece}"
    );
    assert_eq!(
      std::fs::read(dir.join(LOG_FILE)).unwrap(),
      reference,
      "the same log bytes, pieces of {piece}"
    );
    let rows = store.changesets(7, 0, 10).unwrap();
    assert_eq!(rows, whole.changesets(7, 0, 10).unwrap(), "the same rows");
    let identity = *blake3::hash(&payload).as_bytes();
    let read = read_back(&mut store, &identity, payload.len() as u64);
    assert_eq!(
      read.as_deref(),
      Some(payload.as_slice()),
      "the payload, pieces of {piece}"
    );
  }
}

/// Main's ruling of 2026-11-26 (b2b's oracle; `crates/land/tests/oracle.rs`'s crash at every instruction is the
/// pattern). Do: for every count of pieces a landing can have written, from none to all (and a last with every piece
/// written but no commit), land changeset 1 whole, write changeset 2's record and that many pieces of its payload, and
/// crash (the store dropped with no stop, as a `kill -9` leaves it); open the store again, then land changeset 2 in
/// pieces and open it again. Expect: after each crash the store lists changeset 1 alone and names no payload of 2;
/// after the second landing and its restart, both listed and 2's payload read back byte for byte, its log the
/// reference's (both landed whole in one store, no crash).
#[test]
fn a_landing_crashed_at_each_piece_recovers_to_the_changeset_before_and_lands_again_whole() {
  use slates_store::store::{LOG_FILE, PIECE_BYTES};
  let first = oracle_payload(1_000);
  let payload = oracle_payload(4 * PIECE_BYTES + 123);
  let pieces = payload.len().div_ceil(PIECE_BYTES);
  let reference_dir = scratch("crash-reference");
  let mut reference = open(&reference_dir);
  land_whole(&mut reference, 1, &first);
  land_whole(&mut reference, 2, &payload);
  drop(reference);
  // A payload is named by the plain BLAKE3 of its bytes.
  let identity = *blake3::hash(&payload).as_bytes();
  let reference_log = std::fs::read(reference_dir.join(LOG_FILE)).unwrap();
  for cut in 0..=pieces {
    let dir = scratch(&format!("crash-at-{cut}"));
    let mut store = open(&dir);
    land_whole(&mut store, 1, &first);
    land_pieces_to(&mut store, (2, &payload), (PIECE_BYTES, cut));
    drop(store);
    let mut store = open(&dir);
    let listed: Vec<u64> = store
      .changesets(7, 0, 10)
      .unwrap()
      .iter()
      .map(|row| row.sequence)
      .collect();
    assert_eq!(listed, [1], "crashed after {cut} pieces: changeset 1 alone");
    assert_eq!(
      read_back(&mut store, &identity, payload.len() as u64),
      None,
      "crashed after {cut} pieces: no payload of 2 named"
    );
    land_pieces_to(&mut store, (2, &payload), (PIECE_BYTES, usize::MAX));
    store.commit_landing().unwrap();
    drop(store);
    let mut store = open(&dir);
    let listed: Vec<u64> = store
      .changesets(7, 0, 10)
      .unwrap()
      .iter()
      .map(|row| row.sequence)
      .collect();
    assert_eq!(listed, [1, 2], "landed again after a crash at {cut}");
    assert_eq!(
      read_back(&mut store, &identity, payload.len() as u64).as_deref(),
      Some(payload.as_slice()),
      "the payload after a crash at {cut}"
    );
    assert_eq!(
      std::fs::read(dir.join(LOG_FILE)).unwrap()[..reference_log.len()],
      reference_log[..],
      "the reference's log after a crash at {cut}"
    );
  }
}
