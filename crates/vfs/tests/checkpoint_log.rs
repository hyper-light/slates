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
    None,
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
