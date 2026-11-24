//! A-760's tests: an increment whose content is named by identity, read in bounded steps, then judged and committed.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::{Green, Increment, Outcome};
use crate::increment::{Base, VolumeOp, compose_volume_planned};
use crate::named::{Identity, NamedRun, ObjectMissing, Objects};

/// A content plane as a map of objects by identity, counting its reads and the largest.
#[derive(Default)]
struct Plane {
  objects: BTreeMap<[u8; 32], Vec<u8>>,
  reads: Cell<u64>,
  largest: Cell<usize>,
}

impl Plane {
  /// Holds `bytes`; their identity.
  fn put(&mut self, bytes: Vec<u8>) -> Identity {
    let identity = Identity {
      hash: *blake3::hash(&bytes).as_bytes(),
      len: bytes.len() as u64,
    };
    self.objects.insert(identity.hash, bytes);
    identity
  }
}

impl Objects for Plane {
  fn read(&self, identity: &Identity, at: u64, into: &mut [u8]) -> Result<(), ObjectMissing> {
    self.reads.set(self.reads.get() + 1);
    self.largest.set(self.largest.get().max(into.len()));
    let missing = ObjectMissing {
      identity: *identity,
    };
    let bytes = self.objects.get(&identity.hash).ok_or(missing)?;
    let at = usize::try_from(at).map_err(|_| missing)?;
    let from = bytes.get(at..at + into.len()).ok_or(missing)?;
    into.copy_from_slice(from);
    Ok(())
  }
}

/// `path`'s bytes in `green` read through `plane` as a reader does (A-760 (b) 4d): the green's own, or its object's for
/// a file held by name alone, which then holds no byte.
fn read_through(green: &Green, path: &str, plane: &Plane) -> Option<Vec<u8>> {
  match green.content(path)? {
    super::Content::Bytes(bytes) => Some(bytes.to_vec()),
    super::Content::Named(identity) => {
      assert_eq!(green.named_bytes_held(), 0, "a named block holds bytes");
      plane.objects.get(&identity.hash).cloned()
    }
  }
}

/// A work creating `path` whose bytes are the whole of `object`, named rather than carried.
fn named_create(path: &str, object: Identity) -> Increment {
  let journal = vec![
    VolumeOp::Create {
      path: path.to_owned(),
    },
    VolumeOp::Extend {
      path: path.to_owned(),
      at: 0,
      len: object.len,
    },
  ];
  let (doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
  let mut work = Increment {
    id: [7; 32],
    base: 0,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  let src = work.push_named(NamedRun {
    identity: object,
    offset: 0,
    len: object.len,
  });
  assert_eq!(src, Some(0), "the run is the post-state's first bytes");
  work
}

/// A-760 (main's ruling ~09:10: a large text's landing applied in bounded steps). Do: name a 4 MiB text a work creates;
/// submit it unread; read its run 64 KiB a step from the plane; submit it again; encode and decode it. Expect: refused
/// `ContentUnread` before any verdict while unread; read in 64 steps, no read past 64 KiB; then accepted, the green's
/// file the text's name alone (A-760 (b) 4d); its encoding the names alone (shorter than the text), decoded with its run
/// unread again.
#[test]
fn a_named_text_is_read_a_slice_a_step_then_lands_whole() {
  const TEXT: usize = 4 << 20;
  const SLICE: u64 = 64 << 10;
  let bytes: Vec<u8> = (0..TEXT)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let mut work = named_create("big", object);
  let mut green = Green::new();
  assert_eq!(
    green.submit(&work),
    Outcome::ContentUnread { identity: object }
  );
  let steps = read_in_steps(&mut work, &plane, SLICE);
  assert_eq!(steps, (TEXT as u64).div_ceil(SLICE));
  assert!(
    plane.largest.get() <= usize::try_from(SLICE).unwrap(),
    "{}",
    plane.largest.get()
  );
  assert!(work.unread().is_none());
  let encoded = work.encode();
  assert!(encoded.len() < 4096, "{} bytes encoded", encoded.len());
  assert_eq!(green.submit(&work), Outcome::Accepted { version: 1 });
  assert_eq!(green.content("big"), Some(super::Content::Named(object)));
  assert_eq!(read_through(&green, "big", &plane), Some(bytes));
  round_trips_by_name(&work, &encoded, object);
}

/// `encoded`, `work`'s encoding, decodes to its names alone: no post-state, the run of `object` unread again.
fn round_trips_by_name(work: &Increment, encoded: &[u8], object: Identity) {
  let decoded = Increment::decode(encoded).unwrap();
  assert_eq!(decoded.named, work.named);
  assert!(decoded.post_state.is_empty());
  assert_eq!(decoded.unread().map(|run| run.identity), Some(object));
}

/// Reads `work`'s unread run from `plane` `slice` bytes a step until it is read whole: the steps taken.
fn read_in_steps(work: &mut Increment, plane: &Plane, slice: u64) -> u64 {
  let mut steps = 1u64;
  while !work.read_step(plane, slice).unwrap() {
    steps += 1;
  }
  steps
}

/// A-760's hostile input. Do: decode a named work's encoding with its run count raised past the bytes left, and with a
/// run reaching past its object's length. Expect: each refused `Truncated`, nothing allocated for the wild count.
#[test]
fn a_named_runs_table_past_its_bytes_is_refused() {
  let mut plane = Plane::default();
  let object = plane.put(vec![1; 10]);
  let work = named_create("f", object);
  let encoded = work.encode();
  let table = encoded.len() - crate::named::RUN_BYTES - 8;
  let mut wild = encoded.clone();
  wild[table..table + 8].copy_from_slice(&u64::MAX.to_le_bytes());
  assert_eq!(
    Increment::decode(&wild),
    Err(crate::ops_doc::DocDecodeError::Truncated)
  );
  let mut past = encoded;
  let run_len = past.len() - 8;
  past[run_len..].copy_from_slice(&11u64.to_le_bytes());
  assert_eq!(
    Increment::decode(&past),
    Err(crate::ops_doc::DocDecodeError::Truncated)
  );
}

/// A-760. Do: ask a step of a work whose object the plane does not hold. Expect: `ObjectMissing` naming it, nothing
/// appended to the post-state.
#[test]
fn a_named_run_the_plane_lacks_is_refused_typed() {
  let plane = Plane::default();
  let object = Identity {
    hash: [9; 32],
    len: 100,
  };
  let mut work = named_create("f", object);
  assert_eq!(
    work.read_step(&plane, 64),
    Err(ObjectMissing { identity: object })
  );
  assert!(work.post_state.is_empty());
}

/// A-760 part 2 (Ada's sizing rule: a landing holds a large text once, beside the plane), and A-760 (b) 4d (a named block
/// holds no bytes). Do: name a 4 MiB text a work creates, read it, judge it with `judge_named` and commit the verdict.
/// Expect: accepted as version 1, the work left holding none of the text, and the green's file the text's name alone:
/// no byte held of it, one hold of its object taken, its bytes the plane's.
#[test]
fn a_named_texts_file_is_its_name_alone() {
  const TEXT: usize = 4 << 20;
  let bytes: Vec<u8> = (0..TEXT)
    .map(|at| u8::try_from(at % 241).unwrap())
    .collect();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let mut work = named_create("big", object);
  while !work.read_step(&plane, 1 << 20).unwrap() {}
  let mut green = Green::new();
  assert!(green.track_name_holds().is_empty());
  let judged = match green.judge_named(&mut work) {
    super::Judgment::Accepted(judged) => judged,
    other => panic!("not accepted: {other:?}"),
  };
  assert!(
    work.post().runs.iter().all(Vec::is_empty),
    "the work kept its bytes"
  );
  assert_eq!(
    green.commit_judged(judged).map(|(version, _)| version),
    Ok(1)
  );
  assert_eq!(green.content("big"), Some(super::Content::Named(object)));
  assert_eq!(green.named_bytes_held(), 0, "the file holds bytes");
  assert_eq!(green.take_name_holds(), vec![super::NameHold::Take(object)]);
  assert_eq!(read_through(&green, "big", &plane), Some(bytes));
  assert_eq!(
    green.give_back_name_holds(),
    vec![super::NameHold::Give(object)]
  );
}

/// A-760 part 2. Do: create two files from one named run (the same text twice) and commit. Expect: both files the text's
/// bytes: a run two files make is copied for them, never moved out from under the other.
#[test]
fn a_run_two_files_make_is_copied_never_moved_twice() {
  let bytes = b"the same text".to_vec();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let journal = vec![
    VolumeOp::Create {
      path: "a".to_owned(),
    },
    VolumeOp::Extend {
      path: "a".to_owned(),
      at: 0,
      len: object.len,
    },
    VolumeOp::Create {
      path: "b".to_owned(),
    },
    VolumeOp::Extend {
      path: "b".to_owned(),
      at: 0,
      len: object.len,
    },
  ];
  let (mut doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
  // Both files' runs read from the one named run: each adding op's source is its offset.
  for op in &mut doc.ops {
    op.src = 0;
  }
  let mut work = Increment {
    id: [3; 32],
    base: 0,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  work.push_named(NamedRun {
    identity: object,
    offset: 0,
    len: object.len,
  });
  while !work.read_step(&plane, 4).unwrap() {}
  let mut green = Green::new();
  let super::Judgment::Accepted(judged) = green.judge_named(&mut work) else {
    panic!("not accepted");
  };
  green.commit_judged(judged).unwrap();
  assert_eq!(read_through(&green, "a", &plane), Some(bytes.clone()));
  assert_eq!(read_through(&green, "b", &plane), Some(bytes));
}

/// A-760 part 2's bug, found 2026-11-20 closing A-760 (C1). Do: a work whose file `a` is the whole of named run 0 (its
/// buffer moved into the file at the commit) and whose file `b` is the first five bytes of run 1, addressed past run 0
/// in the post-state's one address space; judge it with `judge_named` and commit. Expect: `a` run 0's bytes and `b` run
/// 1's first five. Before the fix the commit moved run 0's buffer out before making `b`, so every later run's bytes moved
/// down by run 0's length and `b` was read from the wrong place.
#[test]
fn a_run_moved_into_its_file_leaves_later_runs_where_they_were() {
  let first = b"the first text, moved whole".to_vec();
  let second = b"second text, of which five bytes".to_vec();
  let mut plane = Plane::default();
  let first_object = plane.put(first.clone());
  let second_object = plane.put(second.clone());
  let journal = vec![
    VolumeOp::Create {
      path: "a".to_owned(),
    },
    VolumeOp::Extend {
      path: "a".to_owned(),
      at: 0,
      len: first_object.len,
    },
    VolumeOp::Create {
      path: "b".to_owned(),
    },
    VolumeOp::Extend {
      path: "b".to_owned(),
      at: 0,
      len: 5,
    },
  ];
  let (doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
  let mut work = Increment {
    id: [5; 32],
    base: 0,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  assert_eq!(
    work.push_named(NamedRun {
      identity: first_object,
      offset: 0,
      len: first_object.len,
    }),
    Some(0)
  );
  assert_eq!(
    work.push_named(NamedRun {
      identity: second_object,
      offset: 0,
      len: second_object.len,
    }),
    Some(first_object.len)
  );
  while !work.read_step(&plane, 8).unwrap() {}
  let mut green = Green::new();
  let super::Judgment::Accepted(judged) = green.judge_named(&mut work) else {
    panic!("not accepted");
  };
  green.commit_judged(judged).unwrap();
  assert_eq!(read_through(&green, "a", &plane), Some(first));
  assert_eq!(
    read_through(&green, "b", &plane).as_deref(),
    second.get(..5)
  );
}

/// A green at version 1 holding `base` as the file `big`.
fn green_holding_big(base: &[u8]) -> Green {
  let mut green = Green::new();
  let (doc, _) = compose_volume_planned(
    &Base::of_files(Vec::new()),
    &[
      VolumeOp::Create {
        path: "big".to_owned(),
      },
      VolumeOp::Extend {
        path: "big".to_owned(),
        at: 0,
        len: base.len() as u64,
      },
    ],
  )
  .unwrap();
  let seed = Increment {
    id: [1; 32],
    base: 0,
    doc,
    post_state: base.to_vec(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  let super::Judgment::Accepted(seeded) = green.judge_named(&mut seed.clone()) else {
    panic!("the seed was not accepted");
  };
  green.commit_judged(seeded).unwrap();
  green
}

/// `judged`'s files made by `green` a `slice` a step to the end, no step copying more than its slice: the bytes copied
/// and the steps taken.
fn made_in_slices(green: &Green, judged: &mut super::Judged, slice: u64) -> (u64, u64) {
  let mut copied = 0u64;
  let mut steps = 0u64;
  loop {
    let made = green.make_step(judged, slice).unwrap();
    assert!(made.copied <= slice, "a step copied {} bytes", made.copied);
    copied += made.copied;
    steps += 1;
    if made.done {
      return (copied, steps);
    }
  }
}

/// A-760's last piece (C1, 2026-11-20: "a partial splice of a run copies at the commit"). Do: a green holding a 1 MiB
/// file; a work that overwrites 5 bytes in its middle from named run 0 and creates a file of the whole of named run 1;
/// judge it with `judge_named`, make its files with `make_step` a 64 KiB slice a step, then commit. Expect: no step
/// copies more than its slice, and the steps together copy the spliced file once (1 MiB); the commit makes no file
/// (`mapped_files_made` unmoved, `files_made_in_steps` one more); the spliced file is the base with run 0's five bytes
/// in its middle, the created one run 1's bytes in run 1's own buffer.
#[test]
fn a_splice_into_a_large_file_is_made_a_slice_a_step_before_its_commit() {
  const FILE: usize = 1 << 20;
  const SLICE: u64 = 64 << 10;
  let base: Vec<u8> = (0..FILE)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let mut green = green_holding_big(&base);

  let whole = b"a whole text, moved".to_vec();
  let splice = b"FIVE!".to_vec();
  let mut plane = Plane::default();
  let whole_object = plane.put(whole.clone());
  let splice_object = plane.put(splice.clone());
  let middle = (FILE / 2) as u64;
  let (doc, _) = compose_volume_planned(
    &Base::of_files(vec![("big".to_owned(), FILE as u64)]),
    &[
      VolumeOp::Create {
        path: "new".to_owned(),
      },
      VolumeOp::Extend {
        path: "new".to_owned(),
        at: 0,
        len: whole_object.len,
      },
      VolumeOp::Overwrite {
        path: "big".to_owned(),
        at: middle,
        len: 5,
      },
    ],
  )
  .unwrap();
  let mut work = Increment {
    id: [2; 32],
    base: 1,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  // The composed post-state holds `big`'s five bytes first, then `new`'s (its ops' sources): the runs in that order.
  assert_eq!(
    work.push_named(NamedRun {
      identity: splice_object,
      offset: 0,
      len: splice_object.len,
    }),
    Some(0)
  );
  assert_eq!(
    work.push_named(NamedRun {
      identity: whole_object,
      offset: 0,
      len: whole_object.len,
    }),
    Some(splice_object.len)
  );
  while !work.read_step(&plane, 8).unwrap() {}
  let super::Judgment::Accepted(mut judged) = green.judge_named(&mut work) else {
    panic!("not accepted");
  };
  let (made_before, stepped_before) = (green.mapped_files_made(), green.files_made_in_steps());
  let (copied, steps) = made_in_slices(&green, &mut judged, SLICE);
  assert_eq!(copied, FILE as u64, "the spliced file copied once");
  assert!(steps >= FILE as u64 / SLICE, "{steps} steps");
  assert_eq!(green.commit_judged(judged).map(|(v, _)| v), Ok(2));
  assert_eq!(
    green.mapped_files_made(),
    made_before,
    "the commit made a file"
  );
  assert_eq!(green.files_made_in_steps(), stepped_before + 1);
  spliced_and_created(&green, (base, &splice), (&whole, &plane));
}

/// `big` is `base` with `splice` in its middle, and `new` is `whole`, held by its name alone (A-760 (b) 4d).
fn spliced_and_created(
  green: &Green,
  (base, splice): (Vec<u8>, &[u8]),
  (whole, plane): (&[u8], &Plane),
) {
  let middle = base.len() / 2;
  let mut expected = base;
  expected[middle..middle + splice.len()].copy_from_slice(splice);
  assert!(
    green.content_bytes("big") == Some(expected.as_slice()),
    "the spliced file differs"
  );
  assert!(
    matches!(green.content("new"), Some(super::Content::Named(_))),
    "the whole run's file is not named"
  );
  assert_eq!(read_through(green, "new", plane).as_deref(), Some(whole));
}

/// Goal 93, A-760 for a recorded version (main 01a20182's ruling of 2026-11-22 ~13:35, the Subversion import's batch:
/// its real green applies each version as its entry records it, after the scratch member judged it). Do: judge and
/// commit a work creating `big` from a 4 MiB named run on one green; on a fresh green, apply the version as its entry
/// records it (`Green::apply_recorded`) from the same work read again. Expect: the replayed file the text's name alone,
/// holding no byte (A-760 (b) 4d; before, the very buffer its run was read into), its head identity the judged green's.
/// Failing first: the recorded apply made the file with `apply`, a copy of the run's bytes (the import's 12.3 MB `apply`
/// ← `apply_recorded` site).
#[test]
fn a_recorded_versions_whole_run_becomes_the_file_by_name() {
  const TEXT: usize = 4 << 20;
  let bytes: Vec<u8> = (0..TEXT)
    .map(|at| u8::try_from(at % 239).unwrap())
    .collect();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let mut judged_work = named_create("big", object);
  while !judged_work.read_step(&plane, 1 << 20).unwrap() {}
  let mut judging = Green::new();
  let judged = match judging.judge_named(&mut judged_work) {
    super::Judgment::Accepted(judged) => judged,
    other => panic!("not accepted: {other:?}"),
  };
  let (version, applied) = judging.commit_judged(judged).unwrap();
  let mut work = named_create("big", object);
  while !work.read_step(&plane, 1 << 20).unwrap() {}
  let mut replaying = Green::new();
  assert_eq!(replaying.apply_recorded(&mut work, &applied), version);
  assert_eq!(replaying.head_identity(), judging.head_identity());
  assert_eq!(
    replaying.content("big"),
    Some(super::Content::Named(object))
  );
  assert_eq!(read_through(&replaying, "big", &plane), Some(bytes));
}

/// Shape: the paths a generated history changes.
const PATHS: [&str; 3] = ["a", "b", "c"];

/// Shape: the objects a generated history's named runs come from.
const OBJECTS: usize = 4;

/// Object `k` of a generated history's plane: distinct bytes, at least two long, so a part of it is never the whole.
fn object_bytes(k: usize) -> Vec<u8> {
  format!("object {k}; ").repeat(k + 2).into_bytes()
}

/// What a generated version does to one path (A-760 (b), item 3's first piece).
#[derive(Clone, Debug)]
enum Change {
  /// Leaves it alone.
  Keep,
  /// Makes it the whole of object `k`, named.
  Whole(usize),
  /// Makes it a part of object `k` (where and how long, as fractions of the object), named but never the whole.
  Part(usize, u8, u8),
  /// Makes it these bytes, carried.
  Inline(Vec<u8>),
  /// Overwrites these bytes into it at a fraction of its length, carried (`Inline` where no file stands).
  Splice(u8, Vec<u8>),
  /// Takes it away (nothing where no file stands).
  Remove,
}

/// A generated version: a change for each of [`PATHS`]; whether the files made whole of one object share one run;
/// whether its verdict's files are made a slice a step before its commit (`Green::make_step`).
#[derive(Clone, Debug)]
struct Version {
  changes: Vec<Change>,
  share: bool,
  stepped: bool,
  /// After the version, the replaying green is replaced by one restored from the judging green's checkpoint at an
  /// earlier version (this picks which, among those committed), then caught up by the recorded versions after it.
  restore: Option<u8>,
}

/// A version's bytes added to a path: carried, or a run of object `k` (offset, length).
enum Adding {
  Inline(Vec<u8>),
  Run(usize, u64, u64),
}

/// What the model holds at a path: the file's bytes and the object it is the whole of, by the design's rule.
type Model = BTreeMap<String, (Vec<u8>, Option<Identity>)>;

/// What the oracle saw, so a history that checked nothing fails: files named and moved, named and sharing a run,
/// named and then changed away from the name, named and kept across a later version, and versions made in steps.
#[derive(Debug, Default)]
struct NameCensus {
  moved: u64,
  shared: u64,
  changed_away: u64,
  kept: u64,
  stepped: u64,
  /// Restores, those below the head (caught up by recorded versions), and the files their checkpoints named.
  restored: u64,
  restored_below: u64,
  named_restored: u64,
  /// Judges and replays refused over an unlent named text, then lent its object and run again (A-760 (b) 4d).
  judges_lent: u64,
  replays_lent: u64,
}

impl NameCensus {
  fn met(&self) -> bool {
    self.moved > 0
      && self.shared > 0
      && self.changed_away > 0
      && self.kept > 0
      && self.stepped > 0
      && self.restored > 0
      && self.restored_below > 0
      && self.named_restored > 0
      && self.judges_lent > 0
      && self.replays_lent > 0
  }
}

/// A version committed in a generated history, kept to apply it again as its entry records it: its index, the version,
/// the model before it, its number and what its entry recorded.
struct Committed {
  index: usize,
  version: Version,
  before: Model,
  number: u64,
  applied: Vec<crate::chain_entry::Applied>,
}

/// What a version does, planned over the model: its journal, the bytes it adds to each path, and the model after it.
#[derive(Default)]
struct Planned {
  journal: Vec<VolumeOp>,
  adds: Vec<(String, Adding)>,
  after: Model,
}

/// The work `version` makes over `model` as increment `index`, judged at `base`, and the model after it; none when it
/// changes nothing.
fn version_work(
  index: usize,
  base: u64,
  model: &Model,
  version: &Version,
  identities: &[Identity],
) -> Option<(Increment, Model)> {
  let planned = planned(model, version, identities);
  if planned.journal.is_empty() {
    return None;
  }
  Some(work_of(
    (index, base),
    model,
    version.share,
    identities,
    planned,
  ))
}

/// `version`'s changes over `model`, a path at a time ([`plan_path`]).
fn planned(model: &Model, version: &Version, identities: &[Identity]) -> Planned {
  let mut planned = Planned {
    after: model.clone(),
    ..Planned::default()
  };
  for (path, change) in PATHS.iter().zip(&version.changes) {
    plan_path(path, change, (model, identities), &mut planned);
  }
  planned
}

/// One path's change, planned: a removal, a splice into the file standing, or the file made new ([`made_of`]).
fn plan_path(
  path: &str,
  change: &Change,
  (model, identities): (&Model, &[Identity]),
  planned: &mut Planned,
) {
  let path = path.to_owned();
  let standing = model.get(&path).map(|(bytes, _)| bytes.clone());
  match (change, &standing) {
    (Change::Keep, _) | (Change::Remove, None) => {}
    (Change::Remove, Some(_)) => {
      planned
        .journal
        .push(VolumeOp::Unlink { path: path.clone() });
      planned.after.remove(&path);
    }
    (Change::Splice(at, bytes), Some(old)) if !old.is_empty() => {
      plan_splice(path, old, (*at, bytes), planned);
    }
    _ => {
      let (made, name, adding) = made_of(change, identities);
      match &standing {
        None => planned
          .journal
          .push(VolumeOp::Create { path: path.clone() }),
        Some(old) if !old.is_empty() => planned.journal.push(VolumeOp::Truncate {
          path: path.clone(),
          len: 0,
        }),
        Some(_) => {}
      }
      planned.journal.push(VolumeOp::Extend {
        path: path.clone(),
        at: 0,
        len: made.len() as u64,
      });
      planned.adds.push((path.clone(), adding));
      planned.after.insert(path, (made, name));
    }
  }
}

/// `bytes` overwritten into `old`, the file at `path`, at a fraction `at` of its length, carried.
fn plan_splice(path: String, old: &[u8], (at, bytes): (u8, &[u8]), planned: &mut Planned) {
  let at = usize::from(at) % old.len();
  let len = bytes.len().min(old.len() - at);
  let mut made = old.to_vec();
  made[at..at + len].copy_from_slice(&bytes[..len]);
  planned.journal.push(VolumeOp::Overwrite {
    path: path.clone(),
    at: at as u64,
    len: len as u64,
  });
  planned
    .adds
    .push((path.clone(), Adding::Inline(bytes[..len].to_vec())));
  planned.after.insert(path, (made, None));
}

/// A file `change` makes new: its bytes, the object they are the whole of by the design's rule, and how they are added.
fn made_of(change: &Change, identities: &[Identity]) -> (Vec<u8>, Option<Identity>, Adding) {
  match change {
    Change::Whole(k) => {
      let bytes = object_bytes(*k);
      let len = bytes.len() as u64;
      (bytes, Some(identities[*k]), Adding::Run(*k, 0, len))
    }
    Change::Part(k, from, take) => {
      let bytes = object_bytes(*k);
      let offset = usize::from(*from) % bytes.len();
      let mut len = 1 + usize::from(*take) % (bytes.len() - offset);
      if offset == 0 && len == bytes.len() {
        len -= 1;
      }
      let part = bytes[offset..offset + len].to_vec();
      (part, None, Adding::Run(*k, offset as u64, len as u64))
    }
    Change::Inline(bytes) | Change::Splice(_, bytes) => {
      (bytes.clone(), None, Adding::Inline(bytes.clone()))
    }
    Change::Keep | Change::Remove => (Vec::new(), None, Adding::Inline(Vec::new())),
  }
}

/// The increment `planned` composes over `model` as increment `index` at `base`, its post-state laid out
/// ([`lay_out`]) and each adding op pointed at its bytes ([`point_ops`]); and the model after it.
fn work_of(
  (index, base): (usize, u64),
  model: &Model,
  share: bool,
  identities: &[Identity],
  planned: Planned,
) -> (Increment, Model) {
  let files: Vec<(String, u64)> = model
    .iter()
    .map(|(path, (bytes, _))| (path.clone(), bytes.len() as u64))
    .collect();
  let (doc, _) = compose_volume_planned(&Base::of_files(files), &planned.journal).unwrap();
  let mut work = Increment {
    id: *blake3::hash(&index.to_le_bytes()).as_bytes(),
    base,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  let srcs = lay_out(&mut work, &planned.adds, share, identities);
  point_ops(&mut work, &srcs);
  (work, planned.after)
}

/// The post-state's one address space: the carried bytes first, then each run in order, two files whole of one object
/// sharing its run where `share`. Each path's bytes' offset there.
fn lay_out(
  work: &mut Increment,
  adds: &[(String, Adding)],
  share: bool,
  identities: &[Identity],
) -> BTreeMap<String, u64> {
  let mut srcs: BTreeMap<String, u64> = BTreeMap::new();
  for (path, adding) in adds {
    if let Adding::Inline(bytes) = adding {
      srcs.insert(path.clone(), work.post_state.len() as u64);
      work.post_state.extend_from_slice(bytes);
    }
  }
  let mut shared: BTreeMap<usize, u64> = BTreeMap::new();
  for (path, adding) in adds {
    let Adding::Run(k, offset, len) = adding else {
      continue;
    };
    let whole = *offset == 0 && *len == identities[*k].len;
    let src = match shared.get(k) {
      Some(src) if share && whole => *src,
      _ => {
        let run = NamedRun {
          identity: identities[*k],
          offset: *offset,
          len: *len,
        };
        let src = work.push_named(run).unwrap();
        if share && whole {
          shared.insert(*k, src);
        }
        src
      }
    };
    srcs.insert(path.clone(), src);
  }
  srcs
}

/// Points each adding op of `work` at its path's bytes in `srcs`: one adding op a path.
fn point_ops(work: &mut Increment, srcs: &BTreeMap<String, u64>) {
  let mut seen: BTreeMap<String, u32> = BTreeMap::new();
  for op in &mut work.doc.ops {
    if matches!(
      op.kind,
      crate::ops_doc::OpKind::Overwrite
        | crate::ops_doc::OpKind::Insert
        | crate::ops_doc::OpKind::Extend
    ) {
      let path = work.doc.paths.path(op.path).unwrap().to_owned();
      op.src = srcs[&path];
      *seen.entry(path).or_default() += 1;
    }
  }
  assert!(
    seen.values().all(|count| *count == 1),
    "a path took two adding ops: {seen:?}"
  );
}

/// `green`'s files and their names are `model`'s, for every one of [`PATHS`].
fn holds_model(green: &Green, model: &Model, which: &str) -> Result<(), TestCaseError> {
  // A-760 (b) 4d: a named file holds its object by name alone, so no named block holds a byte.
  prop_assert_eq!(
    green.named_bytes_held(),
    0,
    "{}'s named blocks hold bytes",
    which
  );
  for path in PATHS {
    let held = model.get(path);
    // Read through the seam: a file's bytes, or the object it is held by, whose bytes are the model's.
    let read = match green.content(path) {
      None => None,
      Some(super::Content::Bytes(bytes)) => Some(bytes.to_vec()),
      Some(super::Content::Named(identity)) => held
        .filter(|(bytes, _)| identity.hash == *blake3::hash(bytes).as_bytes())
        .map(|(bytes, _)| bytes.clone()),
    };
    prop_assert_eq!(
      read.as_deref(),
      held.map(|(bytes, _)| bytes.as_slice()),
      "{}'s bytes at {}",
      which,
      path
    );
    prop_assert_eq!(
      green.name_of(path),
      held.and_then(|(_, name)| *name),
      "{}'s name at {}",
      which,
      path
    );
  }
  Ok(())
}

/// The holds a content store keeps of each object (A-760 (b) 4d): `green`'s drained into it, checked in step with the
/// green's named blocks, one hold each, never one given back that was not taken.
fn holds_in_step(
  green: &mut Green,
  holds: &mut BTreeMap<Identity, u64>,
  which: &str,
) -> Result<(), TestCaseError> {
  for hold in green.take_name_holds() {
    match hold {
      super::NameHold::Take(identity) => *holds.entry(identity).or_default() += 1,
      super::NameHold::Give(identity) => {
        let held = holds.get_mut(&identity).filter(|held| **held > 0);
        let Some(held) = held else {
          return Err(TestCaseError::fail(format!(
            "{which} gave back a hold of {identity:?} it never took"
          )));
        };
        *held -= 1;
        if *held == 0 {
          holds.remove(&identity);
        }
      }
    }
  }
  prop_assert_eq!(
    holds.values().sum::<u64>(),
    green.named_blocks() as u64,
    "{}'s holds out of step with its named blocks",
    which
  );
  Ok(())
}

/// Lends `green` each of `objects` whole from `plane` (A-760 (b) 4d, a refused verdict's or replay's objects).
fn lend_all(green: &mut Green, objects: &BTreeSet<Identity>, plane: &Plane) {
  for identity in objects {
    let bytes = plane
      .objects
      .get(&identity.hash)
      .expect("the plane holds it")
      .clone();
    assert!(
      green.lend(*identity, bytes) > 0,
      "a block took {identity:?}"
    );
  }
}

/// `work` judged on `green`, its unlent named texts lent from `plane` and judged again where it was refused for them
/// (bounded: once lent, a verdict reads nothing more of them).
fn judge_lending(
  green: &mut Green,
  work: &mut Increment,
  plane: &Plane,
  census: &mut NameCensus,
) -> super::Judgment {
  for _ in 0..3 {
    let judgment = green.judge_named(work);
    let super::Judgment::ContentUnread { identity } = judgment else {
      return judgment;
    };
    let unlent = green.unlent_reads(work);
    assert!(
      unlent.contains(&identity),
      "unlent_reads names {identity:?}"
    );
    lend_all(green, &unlent, plane);
    census.judges_lent += 1;
  }
  panic!("a judge refused again with its objects lent");
}

/// `applied` applied to `green` for `work` as its entry records it, its unlent named texts lent from `plane` first,
/// then let go: the version it committed.
fn apply_lending(
  green: &mut Green,
  work: &mut Increment,
  applied: &[crate::chain_entry::Applied],
  (plane, census): (&Plane, &mut NameCensus),
) -> u64 {
  let unlent = green.recorded_unlent(work, applied);
  if !unlent.is_empty() {
    // Applied unlent, the entry commits nothing.
    let head = green.head();
    assert_eq!(
      green.apply_recorded(&mut work.clone(), applied),
      head,
      "applied over an unlent text"
    );
    lend_all(green, &unlent, plane);
    census.replays_lent += 1;
  }
  let version = green.apply_recorded(work, applied);
  green.let_named_bytes_go();
  version
}

/// Reads `work`'s runs from `plane` a few bytes a step, so a run is read over several steps.
fn read_all(work: &mut Increment, plane: &Plane) {
  while !work.read_step(plane, 7).unwrap() {}
}

/// A generated history as it runs: the plane and its objects, the judging green, the green applying each version as its
/// entry records it, the model, and the versions committed so far.
struct History {
  plane: Plane,
  identities: Vec<Identity>,
  judging: Green,
  replaying: Green,
  /// Each green's content store's holds of the objects (A-760 (b) 4d).
  judging_holds: BTreeMap<Identity, u64>,
  replaying_holds: BTreeMap<Identity, u64>,
  model: Model,
  committed: Vec<Committed>,
}

impl History {
  /// A history at version 0 over a plane holding the [`OBJECTS`].
  fn new() -> History {
    let mut plane = Plane::default();
    let identities = (0..OBJECTS).map(|k| plane.put(object_bytes(k))).collect();
    // Each green's holds tracked by its owner's store from the start.
    let (mut judging, mut replaying) = (Green::new(), Green::new());
    assert!(judging.track_name_holds().is_empty() && replaying.track_name_holds().is_empty());
    History {
      plane,
      identities,
      judging,
      replaying,
      judging_holds: BTreeMap::new(),
      replaying_holds: BTreeMap::new(),
      model: Model::new(),
      committed: Vec::new(),
    }
  }

  /// `version`, increment `index`: judged and committed on the judging green (made a slice a step first where it says),
  /// applied as its entry records it on the replaying green, both then held to the model; nothing for a version that
  /// changes nothing.
  fn run(
    &mut self,
    index: usize,
    version: &Version,
    census: &mut NameCensus,
  ) -> Result<(), TestCaseError> {
    let Some((mut work, after)) = version_work(
      index,
      self.judging.head(),
      &self.model,
      version,
      &self.identities,
    ) else {
      return Ok(());
    };
    read_all(&mut work, &self.plane);
    let judgment = judge_lending(&mut self.judging, &mut work, &self.plane, census);
    let super::Judgment::Accepted(mut judged) = judgment else {
      return Err(TestCaseError::fail(format!("version {index} not accepted")));
    };
    let moved =
      |moved: super::CommitRefused| TestCaseError::fail(format!("version {index}: {moved:?}"));
    if version.stepped {
      while !self.judging.make_step(&mut judged, 5).map_err(moved)?.done {}
      census.stepped += 1;
    }
    let (number, applied) = self.judging.commit_judged(judged).map_err(moved)?;
    // Lent for the verdict and its commit, let go once it is committed.
    self.judging.let_named_bytes_go();
    let (mut recorded, _) = version_work(
      index,
      self.replaying.head(),
      &self.model,
      version,
      &self.identities,
    )
    .ok_or_else(|| TestCaseError::fail(format!("version {index} changed nothing again")))?;
    read_all(&mut recorded, &self.plane);
    prop_assert_eq!(
      apply_lending(
        &mut self.replaying,
        &mut recorded,
        &applied,
        (&self.plane, census)
      ),
      number
    );
    count_names(&self.model, &after, version, census);
    let before = std::mem::replace(&mut self.model, after);
    self.committed.push(Committed {
      index,
      version: version.clone(),
      before,
      number,
      applied,
    });
    holds_model(&self.judging, &self.model, "the judging green")?;
    holds_model(&self.replaying, &self.model, "the replaying green")?;
    holds_in_step(
      &mut self.judging,
      &mut self.judging_holds,
      "the judging green",
    )?;
    holds_in_step(
      &mut self.replaying,
      &mut self.replaying_holds,
      "the replaying green",
    )?;
    prop_assert_eq!(self.replaying.head_identity(), self.judging.head_identity());
    Ok(())
  }

  /// The replaying green replaced by one restored from the judging green's checkpoint at the committed version `pick`
  /// chooses ([`decoded`]), caught up by every later committed version applied as its entry records it, then
  /// held to the model (A-869).
  fn restore(&mut self, pick: u8, census: &mut NameCensus) -> Result<(), TestCaseError> {
    if self.committed.is_empty() {
      return Ok(());
    }
    let from = usize::from(pick) % self.committed.len();
    let at = self.committed[from].number;
    let taken = self
      .judging
      .checkpoint(at)
      .ok_or_else(|| TestCaseError::fail(format!("no checkpoint at {at}")))?;
    let checkpoint = decoded(&taken);
    census.named_restored += checkpoint.named.len() as u64;
    let mut green = Green::restore(crate::origin::Origin::default(), &checkpoint);
    // The restored green's holds tracked from its restore: one taken for each named block it restored; the replaying
    // green it replaces gives back every one of its before it is dropped.
    let restored_holds = green.track_name_holds();
    census.restored += 1;
    for later in &self.committed[from + 1..] {
      census.restored_below += 1;
      let (mut work, _) = version_work(
        later.index,
        green.head(),
        &later.before,
        &later.version,
        &self.identities,
      )
      .ok_or_else(|| TestCaseError::fail(format!("version {} changed nothing", later.index)))?;
      read_all(&mut work, &self.plane);
      prop_assert_eq!(
        apply_lending(&mut green, &mut work, &later.applied, (&self.plane, census)),
        later.number
      );
    }
    let named_before = self.replaying.named_blocks();
    let given = self.replaying.give_back_name_holds();
    drop(std::mem::replace(&mut self.replaying, green));
    prop_assert_eq!(
      given.len(),
      named_before,
      "the replaced green gave back one hold a named block"
    );
    for hold in given {
      let super::NameHold::Give(identity) = hold else {
        return Err(TestCaseError::fail("a dropped green took a hold"));
      };
      let held = self
        .replaying_holds
        .get_mut(&identity)
        .expect("given back what it took");
      *held -= 1;
      if *held == 0 {
        self.replaying_holds.remove(&identity);
      }
    }
    prop_assert!(
      self.replaying_holds.is_empty(),
      "holds left after the drop: {:?}",
      self.replaying_holds
    );
    for hold in restored_holds {
      if let super::NameHold::Take(identity) = hold {
        *self.replaying_holds.entry(identity).or_default() += 1;
      }
    }
    holds_in_step(
      &mut self.replaying,
      &mut self.replaying_holds,
      "the restored green",
    )?;
    holds_model(&self.replaying, &self.model, "the restored green")?;
    prop_assert_eq!(self.replaying.head_identity(), self.judging.head_identity());
    Ok(())
  }
}

/// `taken`'s encoding decoded, equal to it: its named files read by nothing (A-760 (b) 4d).
fn decoded(taken: &crate::checkpoint::Checkpoint) -> crate::checkpoint::Checkpoint {
  let checkpoint = crate::checkpoint::Checkpoint::decode(&taken.encode()).unwrap();
  assert_eq!(&checkpoint, taken, "the checkpoint's encoding round trip");
  checkpoint
}

/// Runs `versions` through a [`History`], restoring where a version says, against the model.
fn run_names(versions: &[Version], census: &mut NameCensus) -> Result<(), TestCaseError> {
  let mut history = History::new();
  for (index, version) in versions.iter().enumerate() {
    history.run(index, version, census)?;
    if let Some(pick) = version.restore {
      history.restore(pick, census)?;
    }
  }
  // Every green given back its holds before it is dropped, as an owner does.
  let _ = history.judging.give_back_name_holds();
  let _ = history.replaying.give_back_name_holds();
  Ok(())
}

/// Counts what one version did to the names, from the model before and after it ([`NameCensus`]).
fn count_names(before: &Model, after: &Model, version: &Version, census: &mut NameCensus) {
  for (path, change) in PATHS.iter().zip(&version.changes) {
    let was = before.get(*path).and_then(|(_, name)| *name);
    let now = after.get(*path).and_then(|(_, name)| *name);
    match change {
      Change::Whole(k) => {
        let sharing = version
          .changes
          .iter()
          .filter(|other| matches!(other, Change::Whole(j) if j == k))
          .count();
        if version.share && sharing > 1 {
          census.shared += 1;
        } else {
          census.moved += 1;
        }
      }
      Change::Keep if was.is_some() => census.kept += 1,
      _ if was.is_some() && now.is_none() && after.contains_key(*path) => census.changed_away += 1,
      _ => {}
    }
  }
}

/// A generated change ([`Change`]), by a selector's weights (no `prop_oneof!`, whose arms are held in an `Arc`, CLAUDE.md
/// R2): leave alone 2 in 9, whole 3, part 1, carried 1, splice 1, remove 1.
fn generated_change() -> impl Strategy<Value = Change> {
  (
    0u8..9,
    0..OBJECTS,
    (any::<u8>(), any::<u8>()),
    prop::collection::vec(any::<u8>(), 1..8),
  )
    .prop_map(|(selector, k, (from, take), bytes)| match selector {
      0..2 => Change::Keep,
      2..5 => Change::Whole(k),
      5 => Change::Part(k, from, take),
      6 => Change::Inline(bytes),
      7 => Change::Splice(from, bytes),
      _ => Change::Remove,
    })
}

/// A generated version ([`Version`]).
fn generated_version() -> impl Strategy<Value = Version> {
  let change = generated_change();
  (
    prop::collection::vec(change, PATHS.len()),
    any::<bool>(),
    any::<bool>(),
    prop::option::weighted(0.3, any::<u8>()),
  )
    .prop_map(|(changes, share, stepped, restore)| Version {
      changes,
      share,
      stepped,
      restore,
    })
}

/// A-760 (b), item 3's first piece (main 01a20182's order of 2026-11-22 17:00: the green's block names the plane's
/// object; each piece with its oracle against `apply_recorded` on generated histories). Do: run a fixed history, then
/// generated ones, of versions that make files whole of a named object (alone, or two files sharing one run), of part of
/// one, of carried bytes, splice carried bytes into them, leave them alone or take them away, judged and committed on
/// one green (some made a slice a step first) and applied as their entries record them on another. Expect: after every
/// version both greens hold the model's bytes and names: a file is named for the object exactly when the version that
/// last changed it made it the whole of that object's run, and the two greens' heads are one. Failing first: no file was
/// named (the commit made every block unnamed).
///
/// A-869, piece 2 (main 01a20182's order of 2026-11-22 19:28: restores included). Some versions are followed by a
/// restore: the judging green's checkpoint at an earlier committed version (or the head), encoded and decoded, its named
/// files read from the plane a few bytes a step, restored, and caught up by every later version as its entry records it,
/// in place of the replaying green. Expect it the model's bytes and names, and the judging green's head identity.
/// Failing first: a restored green named no file (`left: None`, the checkpoint carried every file's bytes).
///
/// A-760 (b) 4c+4d (main's order of 2026-11-23): a named block holds no bytes and one hold of its object. Every judge,
/// commit and replay reads a named text only lent it from the plane: a judge or replay that would read one unlent is
/// refused, its objects lent ([`Green::unlent_reads`], [`Green::recorded_unlent`]), and run again, the bytes let go
/// once it is committed; a restore reads nothing. Expect, after every step, no byte held by a named block and each
/// green's holds (drained into a model store) in step with its named blocks, a restore's taken afresh. Failing first:
/// 80 bytes held by named blocks.
#[test]
fn a_file_made_whole_of_a_named_object_is_named_for_it_until_changed() {
  let fixed = vec![
    Version {
      changes: vec![Change::Whole(0), Change::Whole(1), Change::Whole(1)],
      share: true,
      stepped: false,
      restore: None,
    },
    Version {
      changes: vec![
        Change::Keep,
        Change::Inline(b"carried".to_vec()),
        Change::Part(2, 1, 3),
      ],
      share: false,
      stepped: true,
      // Restored at version 1, then caught up by version 2's entry.
      restore: Some(0),
    },
    Version {
      changes: vec![
        Change::Splice(1, b"xy".to_vec()),
        Change::Whole(3),
        Change::Remove,
      ],
      share: false,
      stepped: false,
      // Restored at its head.
      restore: Some(2),
    },
  ];
  let leaked_before = super::dropped_with_name_holds();
  let mut census = NameCensus::default();
  if let Err(failure) = run_names(&fixed, &mut census) {
    panic!("the fixed history: {failure}");
  }
  assert!(
    census.met(),
    "the fixed history checks too little: {census:?}"
  );
  let census = std::cell::RefCell::new(census);
  let config = slates_test_seeds::unseeded(proptest::test_runner::Config {
    cases: slates_test_seeds::cases(64),
    ..proptest::test_runner::Config::default()
  });
  let mut runner = proptest::test_runner::TestRunner::new(config);
  let result = runner.run(
    &prop::collection::vec(generated_version(), 1..10),
    |versions| run_names(&versions, &mut census.borrow_mut()),
  );
  if let Err(failure) = result {
    panic!("{failure}");
  }
  assert_eq!(
    super::dropped_with_name_holds(),
    leaked_before,
    "a tracked green was dropped with a hold live"
  );
  eprintln!("name oracle census: {:?}", census.into_inner());
}
