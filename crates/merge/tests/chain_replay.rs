//! A green recovered from its chain equals the green that wrote it (§4.16 "Commit", §4.8 recovery, A-257). Each generated
//! history runs one live green: works submit over bases behind the head, conflicts are met, accepted increments retried,
//! checkpoints taken at arbitrary versions at or above every live base, and at arbitrary points the process "crashes": a
//! green is rebuilt from the origin, the last checkpoint and the chain entries after it, through their encoding. The rebuilt
//! green must equal the live one whole: the head, its identity, every retained version's content, the identity recorded
//! by every accepted increment (a retry's answer), and, from then on, the verdict on every further increment. Oracle: the
//! live green. Over every dimension (the shared generators of `tests/identity.rs`).
// Test harness code: an unwrap or a panic in a helper is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use common::generated::{Raw, increment_of, origin_of, path, raw};
use proptest::prelude::*;
use slates_merge::chain_entry;
use slates_merge::checkpoint::Checkpoint;
use slates_merge::engine::{Green, Increment, Outcome};
use slates_merge::ops_doc::{DocDecodeError, OpKind};

/// Shape: the alphabet's indices the content comparison walks: every path `common::generated::path` can name.
const PATH_INDICES: u8 = 7;

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
  /// A work submits this increment over a base this far behind the head.
  Submit(Raw, u8),
  /// A work creates a file of its own, over a base this far behind the head: accepted, whatever else has happened.
  Fresh(u8),
  /// A work edits one of those files at this offset, over a base this far behind the head.
  Edit(u8, u8, u8),
  /// Two works make one identical edit of a file over one base: the second is accepted with nothing to do.
  Convergent(u8),
  /// A path changes, and another work then changes it over the base before that change, through one of the verdict's
  /// reads at the base ([`BaseRead`]).
  OverAChange(u8),
  /// An accepted increment, by its position among the accepted, is retried.
  Retry(u8),
  /// A checkpoint is taken this far above the held one, within the live bases.
  Checkpoint(u8),
  /// The process dies and restarts.
  Crash,
}

fn step() -> impl Strategy<Value = Step> {
  (0u8..14, raw(), any::<u8>(), any::<u8>(), any::<u8>()).prop_map(
    |(kind, raw, first, second, third)| match kind {
      0..=2 => Step::Submit(raw, first % 6),
      3..=6 => Step::Fresh(first % 8),
      7..=9 => Step::Edit(first, second % 16, third % 8),
      10 => Step::Convergent(first),
      11 if second.is_multiple_of(2) => Step::OverAChange(first),
      11 => Step::Retry(first),
      12 => Step::Checkpoint(first),
      _ => Step::Crash,
    },
  )
}

fn id(n: usize) -> [u8; 32] {
  let mut id = [0xC4; 32];
  id[..8].copy_from_slice(&(n as u64).to_le_bytes());
  id
}

/// The durable state a crash leaves: the last checkpoint, and the entries after it.
#[derive(Default)]
struct Durable {
  checkpoint: Option<Vec<u8>>,
  held: u64,
  chain: Vec<Vec<u8>>,
}

/// A green rebuilt from what is durable, as recovery rebuilds it: every entry through its encoding, applied, and compared
/// with any identity it or the checkpoint recorded.
fn recovered(
  origin_rows: &[Raw],
  durable: &Durable,
  identities: &std::collections::BTreeMap<u64, [u8; 32]>,
) -> Green {
  let mut green = match &durable.checkpoint {
    Some(bytes) => Green::restore(origin_of(origin_rows), &Checkpoint::decode(bytes).unwrap()),
    None => Green::with_origin(origin_of(origin_rows)),
  };
  for bytes in &durable.chain {
    let entry = chain_entry::decode(bytes).unwrap();
    let version = green.apply_recorded(&entry.increment, &entry.applied);
    assert_eq!(
      identities.get(&version).copied(),
      Some(green.head_identity()),
      "version {version}'s identity, against the live green's at that version"
    );
    if let Some(recorded) = entry.identity {
      assert_eq!(
        recorded,
        green.head_identity(),
        "version {version}'s recorded identity"
      );
    }
  }
  green
}

/// `live` and `other` agree whole, for every version from `from` on.
fn assert_same(
  live: &mut Green,
  other: &mut Green,
  from: u64,
  accepted: &[(Increment, u64)],
  fresh: &[String],
) {
  assert_eq!(live.head(), other.head(), "the head");
  assert_eq!(
    live.head_identity(),
    other.head_identity(),
    "the head's identity"
  );
  assert_eq!(
    other.head_identity(),
    other.head_identity_from_scratch(),
    "the rebuilt green's identity is its state's"
  );
  for version in from..=live.head() {
    let alphabet = (0..PATH_INDICES).map(|index| path(index).to_owned());
    for name in alphabet.chain(fresh.iter().cloned()) {
      assert_eq!(
        live.content_at(&name, version),
        other.content_at(&name, version),
        "{name} at version {version}"
      );
    }
  }
  for (increment, version) in accepted {
    assert_eq!(
      other.submit(increment),
      Outcome::Accepted { version: *version },
      "a retry is answered its original version"
    );
  }
}

/// A verdict's read of the green at an increment's base (`crates/merge/src/engine.rs`), each of which a green restored
/// from a checkpoint above that base cannot answer as the live green did.
#[derive(Clone, Copy, Debug)]
enum BaseRead {
  /// `content_at`: a length-changing op overlapping an intervening change, accepted when it ends where the head is.
  OverlapBytes,
  /// `content_len_at` in `fits_base`: an op that fits the file's length at its base, not at the head.
  FitsLength,
  /// `history_at` in `special_kind_changed`: a mode set on an IPC node whose metadata changed since its base.
  SpecialKind,
}

impl BaseRead {
  const ALL: [BaseRead; 3] = [
    BaseRead::OverlapBytes,
    BaseRead::FitsLength,
    BaseRead::SpecialKind,
  ];

  fn of(seed: u8) -> BaseRead {
    Self::ALL[usize::from(seed) % Self::ALL.len()]
  }

  /// The increments that make `name`, change it, and change it again over the base before that change.
  fn builds(self, name: &str) -> (common::Build, common::Build, common::Build) {
    let (mut made, mut changed, mut late) = (
      common::Build::new(),
      common::Build::new(),
      common::Build::new(),
    );
    match self {
      BaseRead::OverlapBytes => {
        made.create(name, b"0123456789");
        changed.overwrite(name, 0, b"ab");
        late
          .content(OpKind::Delete, name, 0, b"01")
          .content(OpKind::Insert, name, 0, b"ab");
      }
      BaseRead::FitsLength => {
        made.create(name, b"0123456789");
        changed.content(OpKind::Extend, name, 10, b"XY");
        late.overwrite(name, 8, b"zz");
      }
      BaseRead::SpecialKind => {
        made.special(name, common::generated::node(2));
        changed.special(name, common::generated::node(4));
        late.setmode(name, 0o600);
      }
    }
    (made, changed, late)
  }
}

/// What a history did, for the non-vacuity assertions.
#[derive(Default)]
struct Ran {
  crashes: u64,
  entries_below_a_checkpoint: u64,
  /// The kinds of [`chain_entry::Applied`] the history's entries recorded.
  kinds: std::collections::BTreeSet<&'static str>,
}

/// An applied effect's kind, for the non-vacuity assertion.
fn kind_of(applied: &chain_entry::Applied) -> &'static str {
  use chain_entry::Applied as A;
  match applied {
    A::Special(..) => "special",
    A::RemoveSpecial(..) => "remove-special",
    A::Spliced(..) => "spliced",
    A::Created(..) => "created",
    A::Copied { .. } => "copied",
    A::RemoveContent(..) => "remove-content",
    A::MakeDir(..) => "mkdir",
    A::RemoveDir(..) => "rmdir",
    A::Mode(..) => "mode",
    A::Symlink(..) => "symlink",
    A::RemoveSymlink(..) => "remove-symlink",
    A::Hardlink(..) => "hardlink",
    A::RemoveHardlink(..) => "remove-hardlink",
    A::SetXattr(..) => "set-xattr",
    A::RemoveXattr(..) => "remove-xattr",
  }
}

/// One history in progress: the live green, its twin (rebuilt at every crash and given every later increment), what is
/// durable, and what it has accepted.
struct History {
  origin_rows: Vec<Raw>,
  live: Green,
  twin: Green,
  durable: Durable,
  /// The live green's identity at every accepted version.
  identities: std::collections::BTreeMap<u64, [u8; 32]>,
  accepted: Vec<(Increment, u64)>,
  fresh: Vec<String>,
  ran: Ran,
  next_id: usize,
}

impl History {
  fn new(origin_rows: &[Raw]) -> History {
    History {
      origin_rows: origin_rows.to_vec(),
      live: Green::with_origin(origin_of(origin_rows)),
      twin: Green::with_origin(origin_of(origin_rows)),
      durable: Durable::default(),
      identities: std::collections::BTreeMap::new(),
      accepted: Vec::new(),
      fresh: Vec::new(),
      ran: Ran::default(),
      next_id: 0,
    }
  }

  /// The base a work this far behind the head has: never below the checkpoint, which no live work can be.
  fn base(&self, behind: u8) -> u64 {
    self
      .live
      .head()
      .saturating_sub(u64::from(behind))
      .max(self.durable.held)
  }

  /// Submits `build` over `base` to the live green and its twin, records what was accepted, and returns the outcome.
  fn submit(&mut self, build: &common::Build, base: u64) -> Outcome {
    self.next_id += 1;
    let increment = build.with_id(id(self.next_id), base);
    let (outcome, applied) = self.live.submit_resolved(&increment);
    assert_eq!(
      outcome,
      self.twin.submit(&increment),
      "the verdict on a further increment"
    );
    if let (Outcome::Accepted { version }, Some(applied)) = (&outcome, applied) {
      self.ran.kinds.extend(applied.iter().map(kind_of));
      self.identities.insert(*version, self.live.head_identity());
      let recorded = self
        .next_id
        .is_multiple_of(3)
        .then(|| self.live.head_identity());
      self.durable.chain.push(chain_entry::encode(
        &increment.encode(),
        &increment,
        &applied,
        recorded,
      ));
      self.accepted.push((increment, *version));
    }
    outcome
  }

  fn checkpoint(&mut self, at: u64) {
    if at <= self.durable.held || at > self.live.head() {
      return;
    }
    let Some(mut checkpoint) = self.live.checkpoint(at) else {
      return;
    };
    checkpoint.head_check = Some((self.live.head(), self.live.head_identity()));
    let folded = usize::try_from(at - self.durable.held).unwrap();
    let kept = self
      .durable
      .chain
      .split_off(folded.min(self.durable.chain.len()));
    self.ran.entries_below_a_checkpoint += kept
      .iter()
      .filter(|bytes| chain_entry::decode(bytes).unwrap().increment.base < at)
      .count() as u64;
    self.durable.chain = kept;
    self.durable.checkpoint = Some(checkpoint.encode());
    self.durable.held = at;
  }

  fn crash(&mut self) {
    self.ran.crashes += 1;
    self.twin = recovered(&self.origin_rows, &self.durable, &self.identities);
    assert_same(
      &mut self.live,
      &mut self.twin,
      self.durable.held,
      &self.accepted,
      &self.fresh,
    );
  }

  fn step(&mut self, step: &Step) {
    match step {
      Step::Submit(raw, behind) => {
        let base = self.base(*behind);
        self.submit(&increment_of(raw), base);
      }
      Step::Fresh(behind) => {
        let name = format!("fresh-{}", self.fresh.len());
        let mut build = common::Build::new();
        build.create(&name, &[0xF0; 32]);
        let base = self.base(*behind);
        if matches!(self.submit(&build, base), Outcome::Accepted { .. }) {
          self.fresh.push(name);
        }
      }
      Step::Edit(file, at, behind) => {
        if !self.fresh.is_empty() {
          let name = self.fresh[usize::from(*file) % self.fresh.len()].clone();
          let mut build = common::Build::new();
          build.overwrite(&name, u64::from(*at), &[*at; 2]);
          let base = self.base(*behind);
          self.submit(&build, base);
        }
      }
      Step::Convergent(file) => {
        if !self.fresh.is_empty() {
          let name = self.fresh[usize::from(*file) % self.fresh.len()].clone();
          let mut build = common::Build::new();
          build.overwrite(&name, 0, &[*file; 2]);
          let base = self.base(0);
          self.submit(&build, base);
          self.submit(&build, base);
        }
      }
      Step::OverAChange(seed) => {
        self.over_a_change(BaseRead::of(*seed));
      }
      Step::Retry(which) => {
        if !self.accepted.is_empty() {
          let (increment, version) = &self.accepted[usize::from(*which) % self.accepted.len()];
          assert_eq!(
            self.live.submit(increment),
            Outcome::Accepted { version: *version }
          );
          assert_eq!(
            self.twin.submit(increment),
            Outcome::Accepted { version: *version }
          );
        }
      }
      Step::Checkpoint(above) => {
        let at = (self.durable.held + u64::from(*above) + 1).min(self.live.head());
        self.checkpoint(at);
      }
      Step::Crash => self.crash(),
    }
  }

  /// A fresh path made, changed once, then changed again by a work based before that change, through `read`. Returns the
  /// base the last work was based on. Each of the three is accepted live.
  fn over_a_change(&mut self, read: BaseRead) -> u64 {
    let name = format!("over-{}", self.next_id);
    let (made, changed, late) = read.builds(&name);
    let base = self.base(0);
    assert!(matches!(self.submit(&made, base), Outcome::Accepted { .. }));
    if !matches!(read, BaseRead::SpecialKind) {
      self.fresh.push(name);
    }
    let before = self.base(0);
    assert!(matches!(
      self.submit(&changed, before),
      Outcome::Accepted { .. }
    ));
    let outcome = self.submit(&late, before);
    assert!(
      matches!(outcome, Outcome::Accepted { .. }),
      "{read:?} over a change is accepted live: {outcome:?}"
    );
    before
  }

  /// The final crash, and what the history did.
  fn finish(mut self) -> Ran {
    self.crash();
    self.ran
  }
}

fn run(origin_rows: &[Raw], steps: &[Step]) -> Ran {
  let mut history = History::new(origin_rows);
  for step in steps {
    history.step(step);
  }
  history.finish()
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(256)))]

  /// A-257. Do: a generated history of concurrent submits, retries, checkpoints and crashes over a generated origin.
  /// Expect: at every crash and at the end the rebuilt green equals the live one whole, and every verdict after a crash
  /// is the one the live green gave.
  #[test]
  fn a_green_rebuilt_from_its_chain_is_the_green_that_wrote_it(
    origin_rows in prop::collection::vec(raw(), 0..8),
    steps in prop::collection::vec(step(), 1..64),
  ) {
    let ran = run(&origin_rows, &steps);
    prop_assert!(ran.crashes >= 1);
  }
}

/// Non-vacuity, A-257: the entries a history records cover the dimensions the verdict resolves, and a checkpoint passed a
/// retained entry's base, so a re-decided replay would have been refused. Do: one fixed history over each. Expect: every
/// kind named below was recorded, and the entries outlived a checkpoint above their base.
#[test]
fn a_history_reaches_every_dimension_across_a_checkpoint() {
  let mut history = History::new(&[]);
  let mut each = |build: &dyn Fn(&mut common::Build)| {
    let mut built = common::Build::new();
    build(&mut built);
    let base = history.base(0);
    let outcome = history.submit(&built, base);
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
  };
  each(&|b| {
    b.create("file", b"0123456789");
  });
  each(&|b| {
    b.overwrite("file", 0, b"ab");
  });
  each(&|b| {
    b.rename("file", "moved");
  });
  each(&|b| {
    b.setmode("moved", 0o640);
  });
  each(&|b| {
    b.setxattr("moved", "user.k", b"v");
  });
  each(&|b| {
    b.removexattr("moved", "user.k");
  });
  each(&|b| {
    b.symlink("sym", "moved");
  });
  each(&|b| {
    b.link("hard", "moved");
  });
  each(&|b| {
    b.remove("sym");
  });
  each(&|b| {
    b.remove("hard");
  });
  each(&|b| {
    b.mkdir("dir");
  });
  each(&|b| {
    b.rmdir("dir");
  });
  each(&|b| {
    b.remove("moved");
  });
  // A special is declared the way the service declares one.
  let mut special = common::Build::new();
  special.special("pipe", common::generated::node(2));
  let base = history.base(0);
  assert!(matches!(
    history.submit(&special, base),
    Outcome::Accepted { .. }
  ));
  let mut gone = common::Build::new();
  gone.remove("pipe");
  let base = history.base(0);
  assert!(matches!(
    history.submit(&gone, base),
    Outcome::Accepted { .. }
  ));
  // Two works' identical edit of one file over one base, and an edit that outlives a checkpoint above its base.
  history.step(&Step::Fresh(0));
  history.step(&Step::Convergent(0));
  history.step(&Step::Fresh(0));
  history.step(&Step::Edit(0, 3, 2));
  let head = history.live.head();
  history.checkpoint(head - 1);
  let ran = history.finish();
  for kind in [
    "special",
    "remove-special",
    "spliced",
    "created",
    "copied",
    "remove-content",
    "mkdir",
    "rmdir",
    "mode",
    "symlink",
    "remove-symlink",
    "hardlink",
    "remove-hardlink",
    "set-xattr",
    "remove-xattr",
  ] {
    assert!(
      ran.kinds.contains(kind),
      "no entry recorded a {kind}: {:?}",
      ran.kinds
    );
  }
  assert!(
    ran.entries_below_a_checkpoint > 0,
    "an entry outlived a checkpoint above its base"
  );
}

/// Non-vacuity, A-257: an entry based below a checkpoint survives it and is replayed, the case that fenced a green. Do: two
/// works off version 0 write different files, one accepted as version 1 and the other as 2; checkpoint at 1, rebuild.
/// Expect: the checkpoint folded (one entry left, its base below it) and the rebuilt green equals the live one.
#[test]
fn an_entry_based_below_a_checkpoint_replays_across_it() {
  let (one, two): (Raw, Raw) = ((0, 0, 0, 3, 7), (0, 1, 0, 4, 9));
  let steps = [
    Step::Submit(one, 0),
    Step::Submit(two, 1),
    Step::Checkpoint(0),
    Step::Crash,
  ];
  let ran = run(&[], &steps);
  assert_eq!(
    ran.crashes, 2,
    "the history crashed, and once more at its end"
  );
  assert_eq!(
    ran.entries_below_a_checkpoint, 1,
    "version 2's entry, based at 0, outlived the checkpoint at 1"
  );
}

/// A-257: an entry based below a checkpoint, on a path that changed between its base and the checkpoint, through each of
/// the verdict's three reads at the base (`content_at` for a length-changing overlap, `content_len_at` in `fits_base`,
/// `history_at` in `special_kind_changed`). Do: for each, make the path, change it, have another work change it over the
/// base before that change (accepted live), checkpoint at the version between, crash. Expect: the entry outlived the
/// checkpoint and the rebuilt green equals the live one. Judged again at recovery, a restored green answers each read
/// as of the checkpoint, not the base: a different verdict.
#[test]
fn an_entry_over_a_change_below_its_checkpoint_replays_through_each_base_read() {
  for read in BaseRead::ALL {
    let mut history = History::new(&[]);
    let base = history.over_a_change(read);
    let checkpoint = base + 1;
    assert!(
      checkpoint < history.live.head(),
      "{read:?}: the late entry is above the checkpoint"
    );
    history.checkpoint(checkpoint);
    assert_eq!(
      history.durable.held, checkpoint,
      "{read:?}: the checkpoint was taken"
    );
    let ran = history.finish();
    assert_eq!(
      ran.entries_below_a_checkpoint, 1,
      "{read:?}: the late entry, based below the checkpoint, outlived it"
    );
  }
}

/// Two clients taking turns: each is based on its own last version, so every entry's base trails its version, and a
/// checkpoint at each reachable floor must fold anyway (what W3 exists for). Do: alternate 64 versions, checkpointing at the
/// floor after each. Expect: the chain never holds more than the entries the floor has not reached, and a rebuild equals
/// the live green.
#[test]
fn two_clients_taking_turns_keep_folding_and_stay_bounded() {
  let origin_rows = Vec::new();
  let mut live = Green::with_origin(origin_of(&origin_rows));
  let mut durable = Durable::default();
  let mut bases = [0u64; 2];
  let mut accepted = Vec::new();
  let mut identities = std::collections::BTreeMap::new();
  let mut longest = 0;
  let mut folded_past_a_base = 0;
  for n in 0..64u8 {
    let who = usize::from(n % 2);
    let mut build = common::Build::new();
    build.create(&format!("client-{who}-{n}"), &[n]);
    let increment = build.with_id(id(usize::from(n)), bases[who]);
    let (outcome, applied) = live.submit_resolved(&increment);
    let Outcome::Accepted { version } = outcome else {
      panic!("disjoint files are accepted: {outcome:?}");
    };
    bases[who] = version;
    identities.insert(version, live.head_identity());
    durable.chain.push(chain_entry::encode(
      &increment.encode(),
      &increment,
      &applied.unwrap(),
      None,
    ));
    accepted.push((increment, version));
    let floor = bases[0].min(bases[1]);
    if floor > durable.held {
      let checkpoint = live.checkpoint(floor).unwrap();
      let kept = durable
        .chain
        .split_off(usize::try_from(floor - durable.held).unwrap());
      folded_past_a_base += kept
        .iter()
        .filter(|bytes| chain_entry::decode(bytes).unwrap().increment.base < floor)
        .count();
      durable.chain = kept;
      durable.checkpoint = Some(checkpoint.encode());
      durable.held = floor;
    }
    longest = longest.max(durable.chain.len());
  }
  assert!(
    longest <= 2,
    "the chain holds what the floor has not reached: {longest} entries"
  );
  assert!(
    folded_past_a_base > 0,
    "entries really outlived a checkpoint above their base"
  );
  let mut twin = recovered(&origin_rows, &durable, &identities);
  assert_same(&mut live, &mut twin, durable.held, &accepted, &[]);
}

/// Hostile bytes are refused typed: every truncation, a flipped byte, a foreign magic and a bare increment.
#[test]
fn a_hostile_chain_entry_is_refused_typed() {
  let mut green = Green::new();
  let raw: Raw = (0, 0, 0, 3, 7);
  let increment = increment_of(&raw).with_id(id(1), 0);
  let (_, applied) = green.submit_resolved(&increment);
  let bytes = chain_entry::encode(&increment.encode(), &increment, &applied.unwrap(), None);
  assert!(chain_entry::decode(&bytes).is_ok(), "the entry decodes");
  for len in 0..bytes.len() {
    assert!(
      chain_entry::decode(&bytes[..len]).is_err(),
      "truncated at {len}"
    );
  }
  for at in 0..bytes.len() {
    let mut flipped = bytes.clone();
    flipped[at] ^= 0x40;
    assert!(
      chain_entry::decode(&flipped).is_err(),
      "a byte flipped at {at}"
    );
  }
  assert_eq!(
    chain_entry::decode(&increment.encode()),
    Err(DocDecodeError::BadMagic),
    "a bare increment from an earlier layout"
  );
}
