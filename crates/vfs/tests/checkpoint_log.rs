//! The shard recovery journal (A-68; `slates_vfs::checkpoint_log`): a checkpoint plus deltas recovers exactly the
//! shard's full image; a delta frame torn at any byte recovers the state before it or after it, never another; a
//! frame left over from before a checkpoint is never replayed over it; a full log refuses. Memories are plain
//! buffers, as the content object's slices are byte ranges.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_vfs::checkpoint_log::{Journal, KeyedRecord, ShardDelta};
use slates_vfs::recover::{ImageWrite, KeyedImage, ShardImage};
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{store, volume};

/// Shape: each memory's length: room for a small shard's checkpoint and several deltas.
const MEMORY: usize = 1 << 20;
/// Format: the key the test files its one volume under.
const KEY: [u8; 16] = [7; 16];

fn create(vol: &mut Volume, store: &mut Store, name: &str, bytes: &[u8]) {
  let root = vol.root_inode(store).unwrap();
  let file = vol.create_file_no(store, root, name, 0o644).unwrap();
  vol.write(store, file, 0, bytes).unwrap();
}

/// Shape: files a test volume holds before its first checkpoint: enough that one change's delta is smaller than
/// the volume (below that, the volume publishes in full, which a full image is no larger than).
const SEEDED: usize = 64;

/// A volume holding [`SEEDED`] files.
fn seeded(store: &mut Store) -> Volume {
  let mut vol = volume(store, 1 << 20);
  for at in 0..SEEDED {
    create(&mut vol, store, &format!("seed{at}"), b"seed");
  }
  vol
}

fn full(vol: &Volume, store: &Store) -> ShardImage {
  ShardImage::new(vec![KeyedImage {
    key: KEY,
    image: vol.to_image(store, None).unwrap(),
  }])
}

/// The volume's publication as a one-volume delta, marked published.
fn delta_of(vol: &mut Volume, store: &Store) -> ShardDelta {
  let record = vol.publication(store, None).unwrap();
  assert!(
    matches!(record, slates_vfs::delta::VolumeRecord::Delta { .. }),
    "a published volume's next record is a delta"
  );
  vol.mark_published(store);
  ShardDelta::new(
    vec![KeyedRecord { key: KEY, record }],
    Vec::new(),
    (Vec::new(), Vec::new()),
    None,
  )
}

/// A-68: do checkpoint a volume, then change it and log a delta after each change; expect a restarted journal to
/// recover the full image after every step, and its generation to be the last frame's.
#[test]
fn a_checkpoint_and_its_deltas_recover_the_full_image() {
  let mut store = store();
  let mut vol = seeded(&mut store);
  let (mut checkpoints, mut log) = (vec![0u8; MEMORY], vec![0u8; MEMORY]);
  let mut journal = Journal::default();
  journal
    .checkpoint(&mut checkpoints, &full(&vol, &store))
    .unwrap();
  vol.mark_published(&store);
  for at in 0..20 {
    create(
      &mut vol,
      &mut store,
      &format!("f{at}"),
      format!("bytes {at}").as_bytes(),
    );
    journal
      .append(&mut log, &delta_of(&mut vol, &store), &mut Vec::new())
      .unwrap();
    let (recovered, resumed) = Journal::recover(&checkpoints, &log).unwrap();
    assert_eq!(recovered, Some(full(&vol, &store)), "after change {at}");
    assert_eq!(resumed.generation(), journal.generation());
  }
}

/// A writer that stops after `budget` bytes: a crash mid-write, at any byte of the frame.
struct Torn<'a> {
  memory: &'a mut Vec<u8>,
  budget: usize,
}

impl slates_vfs::recover::ImageRead for Torn<'_> {
  fn image_len(&self) -> usize {
    self.memory.len()
  }
  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), slates_vfs::error::VfsError> {
    self.memory.image_read(offset, into)
  }
}

impl ImageWrite for Torn<'_> {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), slates_vfs::error::VfsError> {
    let take = from.len().min(self.budget);
    self.budget -= take;
    self.memory.image_write(offset, &from[..take])
  }
}

/// A-68: do log one more delta with the write cut after every possible byte count; expect each recovery to be the
/// state before that delta or after it — never anything else — and the whole frame to recover the state after.
#[test]
fn a_delta_torn_at_any_byte_recovers_the_state_before_or_after_it() {
  let mut store = store();
  let mut vol = seeded(&mut store);
  let (mut checkpoints, mut log) = (vec![0u8; MEMORY], vec![0u8; MEMORY]);
  let mut journal = Journal::default();
  journal
    .checkpoint(&mut checkpoints, &full(&vol, &store))
    .unwrap();
  vol.mark_published(&store);
  create(&mut vol, &mut store, "first", b"first bytes");
  journal
    .append(&mut log, &delta_of(&mut vol, &store), &mut Vec::new())
    .unwrap();
  let before = full(&vol, &store);
  create(&mut vol, &mut store, "second", b"second bytes");
  let delta = delta_of(&mut vol, &store);
  let after = full(&vol, &store);
  let mut whole = log.clone();
  let frame = journal
    .clone()
    .append(&mut whole, &delta, &mut Vec::new())
    .unwrap();
  let (mut afters, mut befores) = (0, 0);
  for budget in 0..=frame + 8 {
    let mut memory = log.clone();
    let mut torn = Torn {
      memory: &mut memory,
      budget,
    };
    let _ = journal.clone().append(&mut torn, &delta, &mut Vec::new());
    let (recovered, _) = Journal::recover(&checkpoints, &memory).unwrap();
    let recovered = recovered.unwrap();
    if recovered == after {
      afters += 1;
    } else {
      assert_eq!(
        recovered, before,
        "a write cut after {budget} bytes recovered neither state"
      );
      befores += 1;
    }
  }
  assert!(
    befores > 0 && afters > 0,
    "both outcomes occurred ({befores} before, {afters} after)"
  );
}

/// A-68: do log deltas, take a new checkpoint, and log one short delta over it; expect the recovery to stop at the
/// new delta's end, never replaying the older frames still past it in the log.
#[test]
fn frames_from_before_a_checkpoint_are_never_replayed_over_it() {
  let mut store = store();
  let mut vol = seeded(&mut store);
  let (mut checkpoints, mut log) = (vec![0u8; MEMORY], vec![0u8; MEMORY]);
  let mut journal = Journal::default();
  journal
    .checkpoint(&mut checkpoints, &full(&vol, &store))
    .unwrap();
  vol.mark_published(&store);
  for at in 0..5 {
    create(&mut vol, &mut store, &format!("old{at}"), &[at; 200]);
    journal
      .append(&mut log, &delta_of(&mut vol, &store), &mut Vec::new())
      .unwrap();
  }
  journal
    .checkpoint(&mut checkpoints, &full(&vol, &store))
    .unwrap();
  create(&mut vol, &mut store, "new", b"n");
  journal
    .append(&mut log, &delta_of(&mut vol, &store), &mut Vec::new())
    .unwrap();
  let (recovered, _) = Journal::recover(&checkpoints, &log).unwrap();
  assert_eq!(recovered, Some(full(&vol, &store)));
}

/// A-68: do append deltas to a log too small for them; expect `NoSpace` once the next frame does not fit, with the
/// journal unchanged so the caller's checkpoint still commits.
#[test]
fn a_full_log_refuses_and_a_checkpoint_follows() {
  let mut store = store();
  let mut vol = seeded(&mut store);
  let (mut checkpoints, mut log) = (vec![0u8; MEMORY], vec![0u8; 4096]);
  let mut journal = Journal::default();
  journal
    .checkpoint(&mut checkpoints, &full(&vol, &store))
    .unwrap();
  vol.mark_published(&store);
  let mut refused = false;
  for at in 0..64 {
    create(&mut vol, &mut store, &format!("f{at}"), &[1; 100]);
    let delta = delta_of(&mut vol, &store);
    let generation = journal.generation();
    if journal.append(&mut log, &delta, &mut Vec::new()).is_err() {
      assert_eq!(
        journal.generation(),
        generation,
        "a refused append changes nothing"
      );
      journal
        .checkpoint(&mut checkpoints, &full(&vol, &store))
        .unwrap();
      refused = true;
      break;
    }
  }
  assert!(refused, "the small log filled");
  let (recovered, _) = Journal::recover(&checkpoints, &log).unwrap();
  assert_eq!(recovered, Some(full(&vol, &store)));
}

/// The streamed shard delta (2026-10-06; call records A-127). Do: checkpoint a shard of three volumes and two call
/// records, change one volume (its record is then a delta) and make a fourth never published (its record is its full
/// image), and stream a shard delta in key order with `ShardDelta::encode_start`, each key then
/// `Volume::encode_publication`, and `encode_finish` naming the other two as removed (unsorted), with held bytes,
/// replies, a changed call record and a new one (unsorted) and the other checkpointed record purged; log it with
/// `Journal::append_encoded`, and log the same records as a `ShardDelta` value in a copy of the journal. Expect: the
/// bytes equal `ShardDelta::new(…).with_calls(…).encode()`, both journals recover the same image, and it holds the
/// changed volume brought up to date and the fresh one, without the removed, and the call records changed, added and
/// gone as the delta said.
#[test]
fn a_streamed_shard_delta_equals_the_shard_delta_and_recovers() {
  let mut store = store();
  let mut published = seeded(&mut store);
  let mut fresh = seeded(&mut store);
  let gone = seeded(&mut store);
  let (low, high) = (KEY, [9; 16]);
  let removed = vec![[8; 16], [2; 16]];
  let mut in_checkpoint = vec![KeyedImage {
    key: low,
    image: published.to_image(&store, None).unwrap(),
  }];
  for key in &removed {
    in_checkpoint.push(KeyedImage {
      key: *key,
      image: gone.to_image(&store, None).unwrap(),
    });
  }
  let (mut checkpoints, mut log) = (vec![0u8; MEMORY], vec![0u8; MEMORY]);
  let call = |key: u8, record: &[u8]| slates_vfs::recover::HeldCall {
    key: [key; slates_vfs::recover::HELD_CALL_KEY_BYTES],
    record: record.to_vec(),
  };
  let mut journal = Journal::default();
  journal
    .checkpoint(
      &mut checkpoints,
      &ShardImage::new(in_checkpoint).with_calls(vec![call(1, b"one"), call(5, b"five")]),
    )
    .unwrap();
  published.mark_published(&store);
  create(
    &mut published,
    &mut store,
    "changed",
    b"after the checkpoint",
  );
  create(&mut fresh, &mut store, "fresh", b"never published");
  let replies = vec![slates_vfs::recover::HeldReply {
    attachment: 3,
    unique: 5,
    reply: b"reply".to_vec(),
  }];
  // Held pieces put and removed (A-270), each given out of key order, so the streamed encoder's sort is compared.
  let held_put = vec![
    slates_vfs::recover::HeldPiece {
      key: [6; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
      bytes: b"a later piece".to_vec(),
    },
    slates_vfs::recover::HeldPiece {
      key: [3; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
      bytes: b"an earlier piece".to_vec(),
    },
  ];
  let held_removed = vec![
    [9; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
    [1; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
  ];

  let mut streamed = Vec::new();
  let at = ShardDelta::encode_start(&mut streamed);
  for (key, vol) in [(low, &mut published), (high, &mut fresh)] {
    streamed.extend_from_slice(&key);
    vol.encode_publication(&store, None, &mut streamed).unwrap();
  }
  let mut unsorted = removed.clone();
  let calls = vec![call(7, b"seven"), call(5, b"five, later")];
  let calls_removed = vec![[1; slates_vfs::recover::HELD_CALL_KEY_BYTES]];
  let nfs_replies = vec![
    slates_vfs::recover::HeldNfsReply {
      connection: 8,
      xid: 3,
      accepted: true,
      results: b"mkdir".to_vec(),
    },
    slates_vfs::recover::HeldNfsReply {
      connection: 2,
      xid: 9,
      accepted: true,
      results: b"remove".to_vec(),
    },
  ];
  // A held connection's byte states (A-231), carried whole when they changed.
  let nfs_states = vec![slates_vfs::recover::HeldNfsStates {
    connection: 2,
    volume: [5; 16],
    files: vec![slates_vfs::recover::CarriedFile {
      ino: 7,
      lost: false,
      ranges: vec![slates_vfs::recover::CarriedRange {
        start: 0,
        end: u64::MAX,
        byte: slates_vfs::recover::CarriedByte::Unknown,
      }],
    }],
  }];
  ShardDelta::encode_finish(
    &mut streamed,
    at,
    2,
    &mut unsorted,
    (&mut held_put.clone(), &mut held_removed.clone()),
    Some(&replies),
    (&mut calls.clone(), &mut calls_removed.clone()),
    (
      Some(&mut nfs_replies.clone()),
      Some(&mut nfs_states.clone()),
    ),
  )
  .unwrap();
  let value = ShardDelta::new(
    vec![
      KeyedRecord {
        key: high,
        record: fresh.publication(&store, None).unwrap(),
      },
      KeyedRecord {
        key: low,
        record: published.publication(&store, None).unwrap(),
      },
    ],
    removed,
    (held_put, held_removed),
    Some(replies),
  )
  .with_calls(calls, calls_removed)
  .with_nfs_replies(nfs_replies.clone())
  .with_nfs_states(nfs_states.clone());
  assert!(
    matches!(
      value.volumes[0].record,
      slates_vfs::delta::VolumeRecord::Delta { .. }
    ) && matches!(
      value.volumes[1].record,
      slates_vfs::delta::VolumeRecord::Full { .. }
    ),
    "one delta record and one full record"
  );
  let mut expected = Vec::new();
  slates_wire::Wire::encode(&value, &mut expected);
  assert_eq!(streamed, expected, "the streamed shard delta's bytes");

  let (mut by_value, mut by_value_log) = (journal, log.clone());
  journal.append_encoded(&mut log, &streamed).unwrap();
  by_value
    .append(&mut by_value_log, &value, &mut Vec::new())
    .unwrap();
  let (recovered, _) = Journal::recover(&checkpoints, &log).unwrap();
  let (expected, _) = Journal::recover(&checkpoints, &by_value_log).unwrap();
  assert_eq!(recovered, expected, "what logging the value recovers");
  let recovered = recovered.unwrap();
  let keys: Vec<[u8; 16]> = recovered.volumes.iter().map(|keyed| keyed.key).collect();
  assert_eq!(
    keys,
    vec![low, high],
    "the removed volumes gone, the fresh one added"
  );
  assert_eq!(
    recovered.volumes[0].image,
    published.to_image(&store, None).unwrap(),
    "the changed volume brought up to date"
  );
  assert_recovered_calls(
    &recovered,
    vec![call(5, b"five, later"), call(7, b"seven")],
    nfs_replies,
  );
}

/// The streamed delta's call records, as recovered: the changed one replaced, the new one added, the purged one gone,
/// in key order; and its held NFSv3 replies, whole, in (connection, xid) order.
fn assert_recovered_calls(
  recovered: &ShardImage,
  expected: Vec<slates_vfs::recover::HeldCall>,
  mut nfs_replies: Vec<slates_vfs::recover::HeldNfsReply>,
) {
  assert_eq!(
    recovered.calls, expected,
    "the call records changed and added, the purged one gone, in key order"
  );
  nfs_replies.sort_by_key(|reply| (reply.connection, reply.xid));
  assert_eq!(
    recovered.nfs_replies, nfs_replies,
    "the held NFSv3 replies, whole, in order"
  );
}

/// A memory that remembers which of its bytes were written since they were last given back: the oracle's view of what
/// can be resident. It starts all written, since a memory this journal did not write may hold anything (A-226).
struct Recorded {
  bytes: Vec<u8>,
  written: Vec<bool>,
}

impl Recorded {
  fn new(len: usize) -> Self {
    Recorded {
      bytes: vec![0u8; len],
      written: vec![true; len],
    }
  }

  /// Gives `(offset, len)` back as the content object does: it reads zeros and holds nothing.
  fn give_back(&mut self, (offset, len): (usize, usize)) {
    for at in offset..offset + len {
      self.bytes[at] = 0;
      self.written[at] = false;
    }
  }
}

impl slates_vfs::recover::ImageRead for Recorded {
  fn image_len(&self) -> usize {
    self.bytes.len()
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), slates_vfs::VfsError> {
    self.bytes.image_read(offset, into)
  }
}

impl ImageWrite for Recorded {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), slates_vfs::VfsError> {
    self.bytes.image_write(offset, from)?;
    for flag in &mut self.written[offset..offset + from.len()] {
      *flag = true;
    }
    Ok(())
  }
}

/// The regions whose writes the journal has not seen: every slot (by its offset) and the log since a restart, each until
/// it is given back.
struct Unknown {
  slots: std::collections::BTreeSet<usize>,
  log: bool,
}

impl Unknown {
  fn all(slot_offsets: [usize; 2]) -> Self {
    Unknown {
      slots: slot_offsets.into_iter().collect(),
      log: true,
    }
  }
}

/// One step of a generated journal history.
#[derive(Clone, Copy, Debug)]
enum JournalStep {
  /// A checkpoint of the whole volume.
  Checkpoint,
  /// One file created and its delta appended (a checkpoint instead when the log is full).
  ChangeAndAppend,
  /// The dead ranges given back.
  /// The dead ranges given back from the top down, `quarters` quarters of each (one to four), as a give-back taken a
  /// slice at a time leaves them.
  GiveBack { quarters: usize },
  /// The journal dropped and recovered from its memories, as a restarted daemon's is.
  Restart,
  /// One held piece (A-270) put (`bytes` of the step) or removed, its delta appended (a checkpoint instead when the log
  /// is full): the piece's key is `[key; HELD_PIECE_KEY_BYTES]`, from a few, so puts overwrite and removals find one.
  Held { key: u8, remove: bool },
}

/// The shard's full image with `held`, the model's held pieces (A-270), carried.
fn full_with(
  vol: &Volume,
  store: &Store,
  held: &std::collections::BTreeMap<[u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES], Vec<u8>>,
) -> ShardImage {
  full(vol, store).with_held(
    held
      .iter()
      .map(|(key, bytes)| slates_vfs::recover::HeldPiece {
        key: *key,
        bytes: bytes.clone(),
      })
      .collect(),
  )
}

/// Checks the dead ranges against what a restart reads and what was written: every byte written since its last
/// give-back that no restart reads lies inside them (A-117 still gives back every resident dead page); they lie inside
/// what no restart reads; and each ends where its region's writes do (A-226: a give-back costs what was written),
/// except a region the journal cannot have seen written (`unknown`: since a restart, until its first give-back), which
/// it gives back whole.
fn check_dead_ranges(
  journal: &Journal,
  checkpoints: &Recorded,
  log: &Recorded,
  tail: usize,
  unknown: &Unknown,
) {
  let Some(dead) = journal.dead_ranges(checkpoints.bytes.len(), log.bytes.len()) else {
    return;
  };
  let committed = slates_vfs::recover::committed_slot(checkpoints).unwrap();
  let stale = committed.stale_slot(checkpoints.bytes.len());
  let regions = [
    (
      "the stale slot",
      stale,
      dead.stale_slot,
      &checkpoints.written,
      unknown.slots.contains(&stale.0),
    ),
    (
      "the log past the tail",
      (tail, log.bytes.len() - tail),
      dead.log_past_tail,
      &log.written,
      unknown.log,
    ),
  ];
  for (name, (region_at, region_len), (named_at, named_len), written, unseen) in regions {
    let region_end = region_at + region_len;
    let named_end = named_at + named_len;
    assert!(
      named_len == 0 || (region_at <= named_at && named_end <= region_end),
      "{name}: the named range {named_at}..{named_end} leaves what no restart reads, {region_at}..{region_end}"
    );
    let furthest = (region_at..region_end).rev().find(|at| written[*at]);
    if let Some(furthest) = furthest {
      assert!(
        named_at <= region_at && furthest < named_end,
        "{name}: byte {furthest} was written and no restart reads it, but the named range {named_at}..{named_end} \
         leaves it resident"
      );
    }
    assert!(
      unseen || named_len == 0 || furthest.is_some_and(|furthest| named_end <= furthest + 1),
      "{name}: the named range {named_at}..{named_end} runs past the last write {furthest:?}"
    );
  }
}

proptest::proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::prelude::ProptestConfig {
    cases: slates_test_seeds::cases(48),
    ..proptest::prelude::ProptestConfig::default()
  }))]

  /// A-117, A-226 and A-270, the oracle: do run a generated history of checkpoints, appended deltas, held pieces put and
  /// removed, give-backs and restarts over memories that record each byte written; expect after every step that the
  /// dead ranges name every written byte no restart reads, lie inside what no restart reads and end at the last such
  /// write, and that a restart recovers the volume's full image and the model's held pieces whatever was given back.
  #[test]
  fn a_give_back_names_every_dead_written_byte_and_stops_at_the_last(
    steps in proptest::collection::vec(
      // Weights 1 : 4 : 2 : 1 : 3 over eleven draws (no `prop_oneof!`, which boxes its arms in an `Arc`).
      proptest::strategy::Strategy::prop_map(
        (0u8..11, 1usize..=4, 0u8..4, proptest::bool::ANY),
        |(draw, quarters, key, remove)| match draw {
          0 => JournalStep::Checkpoint,
          1..=4 => JournalStep::ChangeAndAppend,
          5 | 6 => JournalStep::GiveBack { quarters },
          7 => JournalStep::Restart,
          _ => JournalStep::Held { key, remove },
        },
      ),
      1..40,
    )
  ) {
    /// Shape: each memory's length in this oracle: small, so a long history fills the log and checkpoints anew.
    const ORACLE_MEMORY: usize = 1 << 16;
    let mut store = store();
    let mut vol = seeded(&mut store);
    let (mut checkpoints, mut log) = (Recorded::new(ORACLE_MEMORY), Recorded::new(ORACLE_MEMORY));
    let mut journal = Journal::default();
    let mut held = std::collections::BTreeMap::new();
    journal.checkpoint(&mut checkpoints, &full_with(&vol, &store, &held)).unwrap();
    vol.mark_published(&store);
    let mut tail = 0usize;
    let slot_offsets = [0, ORACLE_MEMORY / 2];
    let mut unknown = Unknown::all(slot_offsets);
    for (at, step) in steps.into_iter().enumerate() {
      match step {
        JournalStep::Checkpoint => {
          journal.checkpoint(&mut checkpoints, &full_with(&vol, &store, &held)).unwrap();
          vol.mark_published(&store);
          tail = 0;
        }
        JournalStep::Held { key, remove } => {
          let key = [key; slates_vfs::recover::HELD_PIECE_KEY_BYTES];
          let change = if remove {
            held.remove(&key);
            (Vec::new(), vec![key])
          } else {
            let bytes = format!("piece at step {at}").into_bytes();
            held.insert(key, bytes.clone());
            (vec![slates_vfs::recover::HeldPiece { key, bytes }], Vec::new())
          };
          let delta = ShardDelta::new(Vec::new(), Vec::new(), change, None);
          match journal.append(&mut log, &delta, &mut Vec::new()) {
            Ok(total) => tail += total,
            Err(slates_vfs::VfsError::NoSpace) => {
              journal.checkpoint(&mut checkpoints, &full_with(&vol, &store, &held)).unwrap();
              vol.mark_published(&store);
              tail = 0;
            }
            Err(other) => panic!("append refused: {other:?}"),
          }
        }
        JournalStep::ChangeAndAppend => {
          create(&mut vol, &mut store, &format!("step{at}"), format!("step {at}").as_bytes());
          let delta = delta_of(&mut vol, &store);
          match journal.append(&mut log, &delta, &mut Vec::new()) {
            Ok(total) => tail += total,
            Err(slates_vfs::VfsError::NoSpace) => {
              journal.checkpoint(&mut checkpoints, &full_with(&vol, &store, &held)).unwrap();
              tail = 0;
            }
            Err(other) => panic!("append refused: {other:?}"),
          }
        }
        JournalStep::GiveBack { quarters } => {
          if let Some(dead) = journal.dead_ranges(ORACLE_MEMORY, ORACLE_MEMORY) {
            // The top `quarters` quarters of each range, as one slice of a sliced give-back takes them.
            let top = |(offset, len): (usize, usize)| {
              let piece = len * quarters / 4;
              (offset + len - piece, piece)
            };
            let (stale_at, stale_piece) = top(dead.stale_slot);
            let (log_at, log_piece) = top(dead.log_past_tail);
            checkpoints.give_back((stale_at, stale_piece));
            log.give_back((log_at, log_piece));
            journal.given_back_from(Some(stale_at - dead.stale_slot.0), Some(log_at));
            // A region the journal had not seen written is known once it has been given back whole.
            if quarters == 4 {
              let stale = slates_vfs::recover::committed_slot(&checkpoints).unwrap().stale_slot(ORACLE_MEMORY);
              unknown.slots.remove(&stale.0);
              unknown.log = false;
            }
          }
        }
        JournalStep::Restart => {
          let (image, recovered) = Journal::recover(&checkpoints, &log).unwrap();
          proptest::prop_assert!(image.unwrap() == full_with(&vol, &store, &held), "the restart recovered the full image");
          journal = recovered;
          unknown = Unknown::all(slot_offsets);
        }
      }
      check_dead_ranges(&journal, &checkpoints, &log, tail, &unknown);
      let (image, _) = Journal::recover(&checkpoints, &log).unwrap();
      proptest::prop_assert!(
        image.unwrap() == full_with(&vol, &store, &held),
        "step {at} ({step:?}): a restart recovers the full image, its held pieces with it"
      );
    }
  }
}

/// Format: where a recovery image's and a delta's version word sits, after its magic (a little-endian `u32`).
const VERSION_AT: usize = size_of::<u32>();

/// Overwrites the version word of the encoded image or delta `bytes` with `version`.
fn with_version(mut bytes: Vec<u8>, version: u16) -> Vec<u8> {
  bytes
    .get_mut(VERSION_AT..VERSION_AT + size_of::<u16>())
    .unwrap()
    .copy_from_slice(&version.to_le_bytes());
  bytes
}

/// A-270 (§4.8 "Recovery"): do give recovery a shard image written in the version before this build's. Expect it
/// refused `RecoveryVersion`, naming both versions, before any field is decoded: a layout this build does not know is
/// never read as its own.
#[test]
fn an_image_of_another_version_is_refused_by_its_version() {
  let mut store = store();
  let vol = seeded(&mut store);
  let older = slates_vfs::recover::IMAGE_VERSION - 1;
  let bytes = with_version(full(&vol, &store).to_content(), older);
  assert_eq!(
    ShardImage::from_content(&bytes),
    Err(slates_vfs::VfsError::RecoveryVersion {
      found: older,
      expected: slates_vfs::recover::IMAGE_VERSION,
    })
  );
}

/// A-270 (§4.8 "Recovery"): do commit a checkpoint, then append a CRC-valid delta frame written in the delta version
/// before this build's. Expect the recovery refused `RecoveryVersion`. Read as a torn frame instead, the replay would end
/// there and recover the checkpoint without the acknowledged deltas after it.
#[test]
fn a_delta_of_another_version_is_refused_not_read_as_torn() {
  use slates_wire::Wire;
  let mut store = store();
  let mut vol = seeded(&mut store);
  let (mut checkpoints, mut log) = (vec![0u8; MEMORY], vec![0u8; MEMORY]);
  let mut journal = Journal::default();
  journal
    .checkpoint(&mut checkpoints, &full(&vol, &store))
    .unwrap();
  vol.mark_published(&store);
  create(&mut vol, &mut store, "after", b"after the checkpoint");
  let older = slates_vfs::checkpoint_log::DELTA_VERSION - 1;
  let body = with_version(delta_of(&mut vol, &store).to_bytes(), older);
  journal.append_encoded(&mut log, &body).unwrap();
  assert_eq!(
    Journal::recover(&checkpoints, &log).map(|_| ()),
    Err(slates_vfs::VfsError::RecoveryVersion {
      found: older,
      expected: slates_vfs::checkpoint_log::DELTA_VERSION,
    })
  );
}

/// A log memory that records every write in order (A-270's crash test), so a crash can be put after any byte of any of
/// them, in the order the journal made them.
struct WriteRecorder {
  memory: Vec<u8>,
  writes: Vec<(usize, Vec<u8>)>,
}

impl slates_vfs::recover::ImageRead for WriteRecorder {
  fn image_len(&self) -> usize {
    self.memory.len()
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), slates_vfs::VfsError> {
    self.memory.image_read(offset, into)
  }
}

impl ImageWrite for WriteRecorder {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), slates_vfs::VfsError> {
    self.memory.image_write(offset, from)?;
    self.writes.push((offset, from.to_vec()));
    Ok(())
  }
}

/// A-270, a crash at every step of a held write (`crates/land/tests/oracle.rs`'s pattern): do commit a checkpoint
/// holding piece A, append the delta that puts B and removes A into a log that records its writes, and for every write
/// the journal made and every byte of it, rebuild the log as a crash just after that byte leaves it, and recover.
/// Expect each recovery to hold A alone (the frame not committed) or B alone (it committed), never both, neither or
/// anything else; A until the journal's last write is whole, and B once it is.
#[test]
fn a_crash_at_every_byte_of_a_held_delta_recovers_the_old_pieces_or_the_new() {
  let mut store = store();
  let vol = seeded(&mut store);
  let piece = |key: u8, bytes: &[u8]| slates_vfs::recover::HeldPiece {
    key: [key; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
    bytes: bytes.to_vec(),
  };
  let before = vec![piece(1, b"piece A")];
  let after = vec![piece(2, b"piece B")];
  let mut checkpoints = vec![0u8; MEMORY];
  let mut journal = Journal::default();
  journal
    .checkpoint(
      &mut checkpoints,
      &full(&vol, &store).with_held(before.clone()),
    )
    .unwrap();
  let delta = ShardDelta::new(
    Vec::new(),
    Vec::new(),
    (
      after.clone(),
      vec![[1; slates_vfs::recover::HELD_PIECE_KEY_BYTES]],
    ),
    None,
  );
  let mut recorder = WriteRecorder {
    memory: vec![0u8; MEMORY],
    writes: Vec::new(),
  };
  journal
    .append(&mut recorder, &delta, &mut Vec::new())
    .unwrap();
  assert!(
    recorder.writes.len() > 1,
    "the append is several writes, so a crash can fall between them"
  );
  let last = recorder.writes.len() - 1;
  for (index, (offset, bytes)) in recorder.writes.iter().enumerate() {
    for kept in 0..=bytes.len() {
      let mut log = vec![0u8; MEMORY];
      for (earlier, earlier_bytes) in &recorder.writes[..index] {
        log[*earlier..*earlier + earlier_bytes.len()].copy_from_slice(earlier_bytes);
      }
      log[*offset..*offset + kept].copy_from_slice(&bytes[..kept]);
      let (image, _) = Journal::recover(&checkpoints, &log).unwrap();
      let held = image.unwrap().held;
      let whole = index == last && kept == bytes.len();
      assert_eq!(
        held,
        if whole { after.clone() } else { before.clone() },
        "a crash after byte {kept} of write {index} of {}",
        last + 1
      );
    }
  }
}
