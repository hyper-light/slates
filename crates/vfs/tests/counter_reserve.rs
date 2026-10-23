//! Inode numbers go out only under a published reservation (A-150; AC-1.6, D-4; §4.8, D-18). A publication records a
//! volume's counter plus the store's reserve, and the volume's ceiling rises when that publication commits. A number
//! issued at or past the committed ceiling is unreserved until the next commit, and a transport's reply that would hand
//! it out waits for that publication. Checked here as a model over random histories of creates, committed and abandoned
//! publications, and crashes: every number handed out before a crash lies below the next number the recovered volume
//! issues, so a successor never issues a number a client still holds
//! (`docs/bugs/2026-10-22-a-takeover-reissued-inode-numbers-the-kernel-still-held.md`).

// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::panic
)]

mod common;

use proptest::prelude::*;
use slates_vfs::clock::StepClock;
use slates_vfs::delta::VolumeRecord;
use slates_vfs::recover::VolumeImage;
use slates_vfs::volume::{Store, Volume};
use slates_wire::Wire;

/// Shape: the fixture volume's quota in bytes, room for every empty file a history creates.
const QUOTA: u64 = 1 << 30;
/// Shape: the numbers the model's store reserves ahead of the counter: small, so histories cross the ceiling often.
const RESERVE: u64 = 4;
/// Shape: the journal a rebuilt volume keeps, in bytes, as the recovery tests use.
const JOURNAL_BYTES: usize = 1 << 16;
/// Shape: the permission bits of the files a history creates.
const FILE_MODE: u32 = 0o644;

/// One volume, the store under it, and what the transport rule has sent: the numbers handed out to clients, and the
/// numbers whose replies wait for a publication.
struct Model {
  store: Store,
  volume: Volume,
  committed: VolumeImage,
  handed_out: Vec<u64>,
  waiting: Vec<u64>,
  names: u64,
  sent_at_once: u64,
}

/// The publication the daemon writes for `volume` now (`Volume::encode_publication`, the streamed encoders), decoded:
/// the path a successor recovers through, rather than the struct builders the daemon never calls.
fn streamed_publication(volume: &mut Volume, store: &Store) -> VolumeRecord {
  let mut streamed = Vec::new();
  volume
    .encode_publication(store, None, &mut streamed)
    .unwrap();
  VolumeRecord::decode(&mut streamed.as_slice()).unwrap()
}

impl Model {
  /// A fresh volume whose creation published (the create verb's barrier), as the daemon's does.
  fn new() -> Model {
    let mut store = common::store();
    store.inode_reserve = RESERVE;
    let mut volume = common::volume(&mut store, QUOTA);
    let VolumeRecord::Full { image: committed } = streamed_publication(&mut volume, &store) else {
      panic!("a volume's first publication is its whole image");
    };
    volume.mark_published(&store);
    Model {
      store,
      volume,
      committed,
      handed_out: Vec::new(),
      waiting: Vec::new(),
      names: 0,
      sent_at_once: 0,
    }
  }

  /// Creates a file, and sends its number as a transport would: at once while it is reserved, else once a
  /// publication commits.
  fn create(&mut self) -> u64 {
    let root = self.volume.root_inode(&self.store).unwrap();
    let name = format!("f{}", self.names);
    self.names += 1;
    let counter = self
      .volume
      .create_file_no(&mut self.store, root, &name, FILE_MODE)
      .unwrap()
      .counter();
    if self.volume.unreserved() {
      self.waiting.push(counter);
    } else {
      self.handed_out.push(counter);
      self.sent_at_once += 1;
    }
    counter
  }

  /// A barrier: the publication commits, everything issued is reserved, and the waiting replies go out.
  fn commit(&mut self) {
    match streamed_publication(&mut self.volume, &self.store) {
      VolumeRecord::Full { image } => self.committed = image,
      VolumeRecord::Delta { delta } => self.committed.apply(&delta).unwrap(),
    }
    self.volume.mark_published(&self.store);
    assert!(
      !self.volume.unreserved(),
      "a committed publication reserves every number issued before it"
    );
    self.handed_out.append(&mut self.waiting);
  }

  /// A refused publication: imaged, never committed, so nothing is reserved and nothing is sent.
  fn abandon(&mut self) {
    let before = self.volume.unreserved();
    let _ = streamed_publication(&mut self.volume, &self.store);
    assert_eq!(
      self.volume.unreserved(),
      before,
      "an abandoned publication reserves nothing"
    );
  }

  /// The daemon dies: the volume is rebuilt from the last committed image into the store the anchor's RAM keeps, and
  /// the replies that were waiting are never sent.
  fn crash(&mut self) {
    let mut fresh = common::surviving(&self.store);
    fresh.inode_reserve = RESERVE;
    let claims = common::claims(&mut fresh, &[&self.committed]);
    self.volume = Volume::from_image(
      &mut fresh,
      &self.committed,
      &claims,
      Box::new(StepClock::new(0, 1)),
      JOURNAL_BYTES,
      None,
    )
    .unwrap();
    self.store = fresh;
    self.waiting.clear();
  }

  /// The property: the recovered volume's next number lies past every number handed out before.
  fn next_issued_is_past_every_handed_out_number(&mut self) {
    let next = self.create();
    let highest = self.handed_out.iter().copied().filter(|n| *n != next).max();
    if let Some(highest) = highest {
      assert!(
        next > highest,
        "the recovered volume issued {next}, at or below {highest}, a number handed out before the crash"
      );
    }
  }
}

/// A-150, AC-1.6. Do: on a volume whose creation published, create files until the counter passes the committed
/// ceiling, then commit. Expect: the numbers below the ceiling go out at once (no barrier), the first one past it
/// waits, and the commit reserves it.
#[test]
fn a_number_below_the_committed_ceiling_goes_out_at_once_and_one_past_it_waits_for_the_commit() {
  let mut model = Model::new();
  for _ in 0..RESERVE {
    model.create();
  }
  assert_eq!(
    model.sent_at_once, RESERVE,
    "every number the reserve covers goes out without a barrier"
  );
  assert!(model.waiting.is_empty());
  let past = model.create();
  assert_eq!(
    model.waiting,
    [past],
    "the number past the ceiling waits for a publication"
  );
  model.commit();
  assert!(model.waiting.is_empty() && model.handed_out.contains(&past));
}

/// A-150, AC-1.6 (the bug at the volume's own level). Do: create files whose numbers go out at once under the reserve,
/// then crash and create one more on the rebuilt volume. Expect: its number lies past every number handed out. Before
/// A-150 the image recorded the live counter as of its publication, every number issued since went out with no
/// barrier, and the rebuilt volume issued the first of them again.
#[test]
fn a_crash_never_lets_the_rebuilt_volume_issue_a_number_handed_out_before_it() {
  let mut model = Model::new();
  for _ in 0..RESERVE {
    model.create();
  }
  assert_eq!(model.sent_at_once, RESERVE);
  model.crash();
  model.next_issued_is_past_every_handed_out_number();
}

/// One step of a random history.
#[derive(Clone, Debug)]
enum Step {
  /// Create a file: one number issued.
  Create,
  /// A barrier: publish and commit.
  Commit,
  /// A refused publication.
  Abandon,
  /// The daemon dies and its successor rebuilds the volume.
  Crash,
}

/// Creates outweigh the rest four to one each, so histories cross the reserve between barriers.
#[allow(clippy::disallowed_types)] // proptest's `prop_oneof!` holds its arms in an `Arc` (D-8's test-harness exception).
fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    4 => Just(Step::Create),
    1 => Just(Step::Commit),
    1 => Just(Step::Abandon),
    1 => Just(Step::Crash),
  ]
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(200),
    ..ProptestConfig::default()
  }))]

  /// A-150, AC-1.6, as a model over histories. Do: run a random history of creates, committed and abandoned
  /// publications, and crashes. Expect: after every crash the rebuilt volume's next number lies past every number
  /// handed out before it; a commit leaves nothing unreserved; an abandoned publication changes nothing.
  #[test]
  fn no_history_lets_a_rebuilt_volume_issue_a_number_handed_out_before_its_crash(
    steps in proptest::collection::vec(step(), 1..64)
  ) {
    let mut model = Model::new();
    for step in steps {
      match step {
        Step::Create => {
          model.create();
        }
        Step::Commit => model.commit(),
        Step::Abandon => model.abandon(),
        Step::Crash => {
          model.crash();
          model.next_issued_is_past_every_handed_out_number();
        }
      }
    }
  }
}
