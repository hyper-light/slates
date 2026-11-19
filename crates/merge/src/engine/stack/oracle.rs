//! The oracles of works over works (A-659; the module's docs).
//!
//! **Landing a stack** is held to a history in which the design's rule is plain. The rule: an upper "is mapped through
//! the green's deltas except the lower work's own, which its base already holds" (§4.17.1 "Works over works"). In the
//! replica, the stack's works below the one judged land first, one after another at the stack's base, each on the
//! verdict's fast path; then the works that landed in between land after them, each mapped by the verdict over the
//! stack's works; then the judged work lands as a work based on a version, the one its lower's post-state is there, so
//! the verdict judges it against exactly the green's deltas except its lowers' own. Disjoint changes commute (§4.16
//! "Position mapping"), so after each work of a stack lands the green must hold what that replica holds, and a work must
//! land exactly where the replica's lands. A bottom the green refuses is one some work in between meets, so the replica
//! of the bottom refuses one of those. Every byte a generated work adds is fresh, so no two works ever make the same
//! change (an identical change is judged by its own tests). Works put an overwrite or a truncate and an insert at one
//! offset too, the shape whose insert the verdict's walk met in one order only until A-668. The census counts stacks by
//! depth, uppers whose operations moved and those that did not, and every refusal.
//!
//! **A work over a tree** is held to a model of whole files: at each file the work edits, the tree's bytes equal to the
//! head's mean its edits apply to the head's bytes as they stand, and any other bytes are a conflict unless the work
//! makes the head's. The census counts files applied and files refused.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::tests::{Work, green_of};
use super::{Landed, TreeBase, TreeNode, Upper};
use crate::engine::{Green, Increment, Outcome};

/// Shape: the files every generated green starts with.
const FILES: [&str; 2] = ["a", "b"];
/// Shape: the bytes each starts with (fresh tokens, two bytes each).
const START_BYTES: usize = 16;
/// Shape: the deepest stack generated (its bottom and three uppers), so a stack's every level is met often.
const DEEPEST: usize = 4;

/// One splice of a text: `removed` bytes at `at` replaced by `inserted`, of a pure kind (an insert, a delete or an
/// overwrite of equal length), then `tail` inserted at the same offset (after an overwrite, or after a truncate): the
/// shape whose insert the verdict's walk met in one order only until A-668.
#[derive(Clone, Debug)]
struct Splice {
  at: usize,
  removed: usize,
  inserted: Vec<u8>,
  tail: Vec<u8>,
}

/// One work's edits: for each file it edits (chosen among its base's files), raw splices `(offset, kind, size)`; and
/// whether it makes a fresh file.
#[derive(Clone, Debug)]
struct Spec {
  edits: Vec<(u8, Vec<(u8, u8, u8)>)>,
  make: bool,
}

/// One generated step.
#[derive(Clone, Debug)]
enum Step {
  /// A work based `back` versions behind the head.
  Plain { back: u8, spec: Spec },
  /// A stack based on the head (its works bottom first), the works landing between its base and its landing (each
  /// based on that base), then the stack landed.
  Stack {
    works: Vec<Spec>,
    between: Vec<Spec>,
  },
}

/// Fresh bytes: runs of two-byte tokens no other run repeats (a token's first byte has its high bit set and its
/// second's clear), so no two works ever write the same bytes.
#[derive(Debug, Default)]
struct Fresh {
  serial: u16,
}

impl Fresh {
  fn run(&mut self, len: usize) -> Vec<u8> {
    let mut run = Vec::with_capacity(len.saturating_add(1));
    while run.len() < len {
      self.serial = self.serial.wrapping_add(1);
      let [low, high] = self.serial.to_le_bytes();
      run.push(0x80 | (high << 1) | (low >> 7));
      run.push(low & 0x7f);
    }
    run.truncate(len);
    run
  }

  /// A name no other file has.
  fn name(&mut self) -> String {
    self.serial = self.serial.wrapping_add(1);
    format!("n{}", self.serial)
  }
}

/// The splices `raw` make of a text `len` bytes long: pure kinds, each run fresh, sorted, and each at least one
/// untouched byte from the one before, so the work puts no two operations at one offset. With `adds`, a file no splice
/// adds bytes to is left alone, so the work's file never equals one another work made.
fn splices(len: usize, raw: &[(u8, u8, u8)], fresh: &mut Fresh, adds: bool) -> Vec<Splice> {
  let mut wanted: Vec<(usize, u8, usize)> = raw
    .iter()
    .map(|(at, kind, size)| {
      let at = usize::from(*at) % (len + 1);
      (at, kind % 5, usize::from(size % 3) + 1)
    })
    .collect();
  wanted.sort_by_key(|(at, ..)| *at);
  let mut chosen = Vec::new();
  let mut free = 0;
  for (at, kind, size) in wanted {
    if at < free {
      continue;
    }
    let splice = match kind {
      0 => Splice {
        at,
        removed: 0,
        inserted: fresh.run(2 * size),
        tail: Vec::new(),
      },
      1 if at < len => Splice {
        at,
        removed: size.min(len - at),
        inserted: Vec::new(),
        tail: Vec::new(),
      },
      2 | 3 if at + 2 <= len => {
        let removed = (size + 1).min(len - at);
        Splice {
          at,
          removed,
          inserted: fresh.run(removed),
          tail: if kind == 3 { fresh.run(2) } else { Vec::new() },
        }
      }
      4 if at < len => Splice {
        at,
        removed: len - at,
        inserted: Vec::new(),
        tail: fresh.run(2 * size),
      },
      _ => continue,
    };
    free = splice.at + splice.removed + 1;
    chosen.push(splice);
  }
  if adds
    && chosen
      .iter()
      .all(|splice| splice.inserted.is_empty() && splice.tail.is_empty())
  {
    chosen.clear();
  }
  chosen
}

/// `text` with `splices` applied.
fn spliced(text: &[u8], splices: &[Splice]) -> Vec<u8> {
  let mut out = text.to_vec();
  for splice in splices.iter().rev() {
    out.splice(
      splice.at..splice.at + splice.removed,
      splice.inserted.iter().chain(&splice.tail).copied(),
    );
  }
  out
}

/// `work` with `splices` of `path` (a text `len` bytes long) added, in the deriver's canonical forms: an insert at the
/// end an extend, a delete to the end a truncate, a tail an insert after its overwrite or truncate, as canonical order
/// puts them.
fn with_splices(mut work: Work, path: &str, len: usize, splices: &[Splice]) -> Work {
  for splice in splices {
    let at = splice.at as u64;
    let to_end = splice.at + splice.removed == len;
    work = match (splice.removed, splice.inserted.is_empty()) {
      (0, _) if splice.at == len => work.extend(path, at, &splice.inserted),
      (0, _) => work.insert(path, at, &splice.inserted),
      (removed, true) if to_end => work.truncate(path, at, removed as u64),
      (removed, true) => work.delete(path, at, removed as u64),
      _ => work.overwrite(path, at, &splice.inserted),
    };
    if !splice.tail.is_empty() {
      work = work.insert(path, at, &splice.tail);
    }
  }
  work
}

/// The work `spec` makes over `base` (each file's bytes), and the files after it.
fn work_over(
  base: &BTreeMap<String, Vec<u8>>,
  spec: &Spec,
  (fresh, adds): (&mut Fresh, bool),
) -> (Work, BTreeMap<String, Vec<u8>>) {
  let names: Vec<&String> = base.keys().collect();
  let mut work = Work::default();
  let mut after = base.clone();
  let mut edited = Vec::new();
  for (choice, raw) in &spec.edits {
    let Some(path) = names
      .get(usize::from(*choice) % names.len().max(1))
      .copied()
    else {
      continue;
    };
    if edited.contains(&path) {
      continue;
    }
    edited.push(path);
    let text = base.get(path).cloned().unwrap_or_default();
    let cut = splices(text.len(), raw, fresh, adds);
    work = with_splices(work, path, text.len(), &cut);
    after.insert(path.clone(), spliced(&text, &cut));
  }
  if spec.make {
    let (name, bytes) = (fresh.name(), fresh.run(4));
    work = work.create(&name, &bytes);
    after.insert(name, bytes);
  }
  (work, after)
}

/// Every file `green` holds at `version`, with its bytes.
fn files_at(green: &Green, version: u64) -> BTreeMap<String, Vec<u8>> {
  green
    .files_at(version)
    .into_iter()
    .map(|(path, bytes)| (path, bytes.into_owned()))
    .collect()
}

/// How often each answer was met over every case: the non-vacuity census.
#[derive(Debug, Default)]
struct Census {
  /// Stacks whose works landed through each depth (index 0: the bottom alone).
  depths: [u64; DEEPEST],
  /// Uppers restated with an operation moved from where they declared it.
  remapped: u64,
  /// Uppers restated with every operation where they declared it.
  unmoved: u64,
  /// Uppers refused for a change they met.
  uppers_refused: u64,
  /// Bottoms the verdict refused, their uppers never judged.
  bottoms_refused: u64,
  /// Works that landed between a stack's base and its landing.
  between: u64,
  /// Files a work over a tree edited where the head held the tree's bytes, applied.
  tree_applied: u64,
  /// Files a work over a tree edited where the head held other bytes, refused.
  tree_refused: u64,
}

/// A history under test: the green, the files it started with, and every increment it committed, in order.
struct History {
  green: Green,
  log: Vec<Increment>,
  fresh: Fresh,
  works: u64,
}

impl History {
  fn new() -> History {
    let mut fresh = Fresh::default();
    let start: Vec<(String, Vec<u8>)> = FILES
      .iter()
      .map(|path| ((*path).to_owned(), fresh.run(START_BYTES)))
      .collect();
    let files: Vec<(&str, &[u8])> = start
      .iter()
      .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
      .collect();
    History {
      green: green_of(&files),
      log: Vec::new(),
      fresh,
      works: 0,
    }
  }

  /// The green as it was before any step, rebuilt by replaying the log's first `prefix` increments.
  fn replica(&self, prefix: usize) -> Result<Green, TestCaseError> {
    let start = files_at(&self.green, 0);
    let files: Vec<(&str, &[u8])> = start
      .iter()
      .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
      .collect();
    let mut green = green_of(&files);
    for increment in self.log.get(..prefix).unwrap_or_default() {
      let outcome = green.submit(increment);
      prop_assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
    }
    Ok(green)
  }

  fn next_id(&mut self) -> u64 {
    self.works += 1;
    self.works
  }

  /// A work `back` versions behind the head.
  fn plain(&mut self, back: u8, spec: &Spec) {
    let base = self.green.head().saturating_sub(u64::from(back));
    let files = files_at(&self.green, base);
    let (work, _) = work_over(&files, spec, (&mut self.fresh, false));
    let increment = work.increment(self.next_id(), base);
    if let Outcome::Accepted { .. } = self.green.submit(&increment) {
      self.log.push(increment);
    }
  }

  /// The stack's works over the head, bottom first, as increments based on the head and the versions above it.
  fn stack_works(&mut self, specs: &[Spec]) -> Vec<Increment> {
    let base = self.green.head();
    let mut files = files_at(&self.green, base);
    let mut works = Vec::new();
    for (level, spec) in specs.iter().take(DEEPEST).enumerate() {
      let (work, after) = work_over(&files, spec, (&mut self.fresh, false));
      works.push(work.increment(self.next_id(), base + level as u64));
      files = after;
    }
    works
  }

  /// A stack, the works between its base and its landing, then the stack landed and held to its replicas.
  fn stack(
    &mut self,
    specs: &[Spec],
    between: &[Spec],
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let (prefix, base) = (self.log.len(), self.green.head());
    let works = self.stack_works(specs);
    let files = files_at(&self.green, base);
    let mut landed_between = Vec::new();
    for spec in between {
      let (work, _) = work_over(&files, spec, (&mut self.fresh, true));
      let increment = work.increment(self.next_id(), base);
      if let Outcome::Accepted { .. } = self.green.submit(&increment) {
        landed_between.push(increment.clone());
        self.log.push(increment);
      }
    }
    census.between += landed_between.len() as u64;
    let Some((bottom, rest)) = works.split_first() else {
      return Ok(());
    };
    let uppers: Vec<Upper> = rest.iter().map(as_upper).collect();
    let landed = self.green.land_stack(bottom, &uppers);
    let stack = Stack {
      prefix,
      works: &works,
      between: &landed_between,
    };
    for (level, outcome) in landed.iter().enumerate() {
      if !self.held_to_replica(&stack, (level, outcome), census)? {
        break;
      }
    }
    Ok(())
  }

  /// Holds the stack's work at `level` to its replica (module docs); whether the stack goes on above it.
  fn held_to_replica(
    &mut self,
    stack: &Stack<'_>,
    (level, outcome): (usize, &Landed),
    census: &mut Census,
  ) -> Result<bool, TestCaseError> {
    let replica = self.replica(stack.prefix)?;
    let (replica, replica_landed) = stack.replay(replica, level)?;
    match outcome {
      Landed::Accepted {
        version, restated, ..
      } => {
        prop_assert!(
          replica_landed,
          "the green landed level {} where its replica refused",
          level
        );
        prop_assert_eq!(
          files_at(&self.green, *version),
          files_at(&replica, replica.head())
        );
        // What the green committed, for the replicas of later steps: the bottom as given, an upper restated.
        let committed = match (level, restated) {
          (0, _) => stack.works.first().cloned(),
          (_, restated) => restated.clone(),
        };
        prop_assert!(committed.is_some(), "level {} committed nothing", level);
        self.log.extend(committed);
        census.depths[level] += 1;
        if let (Some(restated), Some(declared)) = (restated, stack.works.get(level)) {
          if restated.doc.ops == declared.doc.ops {
            census.unmoved += 1;
          } else {
            census.remapped += 1;
          }
        }
        Ok(true)
      }
      Landed::Conflict { .. } => {
        prop_assert!(
          !replica_landed,
          "the green refused level {} where its replica landed",
          level
        );
        if level == 0 {
          census.bottoms_refused += 1;
        } else {
          census.uppers_refused += 1;
        }
        Ok(false)
      }
      other => {
        prop_assert!(false, "level {} answered {:?}", level, other);
        Ok(false)
      }
    }
  }

  fn step(&mut self, step: &Step, census: &mut Census) -> Result<(), TestCaseError> {
    match step {
      Step::Plain { back, spec } => {
        self.plain(*back, spec);
        Ok(())
      }
      Step::Stack { works, between } => self.stack(works, between, census),
    }
  }
}

/// An increment of a stack as the upper it is over the work below it.
fn as_upper(increment: &Increment) -> Upper {
  Upper {
    id: increment.id,
    doc: increment.doc.clone(),
    post_state: increment.post_state.clone(),
    evidence: increment.evidence.clone(),
  }
}

/// A stack under test: where the log stood before it, its works (each based on the version its lower's post-state is
/// in the replica), and the works that landed between its base and its landing.
struct Stack<'a> {
  prefix: usize,
  works: &'a [Increment],
  between: &'a [Increment],
}

impl Stack<'_> {
  /// The replica of the work at `level` (module docs), and whether that work landed there. For the bottom: the bottom,
  /// then the works between, all of which land exactly when the bottom lands in the green. For an upper: the works below
  /// it, the works between (each must land), then it.
  fn replay(&self, mut green: Green, level: usize) -> Result<(Green, bool), TestCaseError> {
    let below = self.works.get(..level.max(1)).unwrap_or_default();
    for work in below {
      let outcome = green.submit(work);
      prop_assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
    }
    let mut between_landed = true;
    for work in self.between {
      between_landed &= matches!(green.submit(work), Outcome::Accepted { .. });
    }
    if level == 0 {
      return Ok((green, between_landed));
    }
    prop_assert!(
      between_landed,
      "a work between was refused over landed lowers"
    );
    let landed = match self.works.get(level) {
      Some(work) => matches!(green.submit(work), Outcome::Accepted { .. }),
      None => false,
    };
    Ok((green, landed))
  }
}

fn spec() -> impl Strategy<Value = Spec> {
  let raw = prop::collection::vec((any::<u8>(), any::<u8>(), any::<u8>()), 1..4);
  (prop::collection::vec((any::<u8>(), raw), 1..3), any::<u8>()).prop_map(|(edits, make)| Spec {
    edits,
    make: make.is_multiple_of(4),
  })
}

fn step() -> impl Strategy<Value = Step> {
  (
    any::<u8>(),
    0..3u8,
    spec(),
    prop::collection::vec(spec(), 1..=DEEPEST),
    prop::collection::vec(spec(), 0..4),
  )
    .prop_map(|(kind, back, spec, works, between)| {
      if kind.is_multiple_of(4) {
        Step::Plain { back, spec }
      } else {
        Step::Stack { works, between }
      }
    })
}

/// A-659's oracle (§4.17.1 "Works over works"; the module's docs): landing a stack equals applying each work's
/// operations to whole trees in order. Do: run generated histories over a green of two files: works based at the head
/// or behind it, and stacks of one to four works over the head (edits of every pure kind, fresh files made), with
/// works landing between a stack's base and its landing (each based on that base, each adding bytes). After each work of
/// each stack, compare with its replica, in which the stack's works below it landed first and the works between after
/// them. Expect every work of a stack to land exactly where its replica's does, the green then to hold the replica's
/// files byte for byte, and a refused bottom exactly where a work between meets it. The census shows every depth, uppers
/// moved and unmoved, and every refusal met.
#[test]
fn a_stack_lands_as_its_works_applied_in_order_to_whole_trees_over_every_history() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(&prop::collection::vec(step(), 1..10), |steps| {
    let mut history = History::new();
    for step in &steps {
      history.step(step, &mut census.borrow_mut())?;
    }
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  let met = [
    census.remapped,
    census.unmoved,
    census.uppers_refused,
    census.bottoms_refused,
    census.between,
  ];
  assert!(
    met.iter().chain(&census.depths).all(|&count| count > 0),
    "an answer never met: {census:?}"
  );
}

/// A tree of files, as a test's base given as a tree.
struct Files(BTreeMap<String, Vec<u8>>);

impl TreeBase for Files {
  fn node(&self, path: &str) -> TreeNode<'_> {
    match self.0.get(path) {
      Some(bytes) => TreeNode::File {
        bytes: Cow::Borrowed(bytes),
        mode: None,
      },
      None => TreeNode::Absent,
    }
  }
}

/// The whole-file model of a work over a tree (module docs), from the tree, the head and the files the work leaves over
/// the tree: the files after it, or none for a conflict; and how many files it applied and refused.
fn tree_model(
  (tree, head): (&BTreeMap<String, Vec<u8>>, &BTreeMap<String, Vec<u8>>),
  after: &BTreeMap<String, Vec<u8>>,
) -> (Option<BTreeMap<String, Vec<u8>>>, u64, u64) {
  let mut model = head.clone();
  let (mut applied, mut refused) = (0, 0);
  for (path, bytes) in after {
    let (base, now) = (tree.get(path), head.get(path));
    if base == Some(bytes) {
      continue;
    }
    if base.is_none() || base == now {
      applied += u64::from(base.is_some());
      model.insert(path.clone(), bytes.clone());
    } else if now != Some(bytes) {
      refused += 1;
    }
  }
  ((refused == 0).then_some(model), applied, refused)
}

/// A-659's second half (§4.17.1 "A push is a work" item 6): a work over a tree. Do: run generated histories over a
/// green of two files, then a work composed against the tree the green held some versions back (a base no version's
/// head is, as the green moved since), landed over the tree. Expect the green to answer as the whole-file model does:
/// landed with exactly the model's files where the head held the tree's bytes at every file the work edits, refused
/// with nothing applied otherwise. The census shows files applied and files refused.
#[test]
fn a_work_over_a_tree_lands_as_its_whole_file_model_does_over_every_history() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let cases = (
    prop::collection::vec((0..3u8, spec()), 0..8),
    0..4u8,
    spec(),
  );
  let result = runner.run(&cases, |(history_steps, back, spec)| {
    let mut history = History::new();
    for (back, step_spec) in &history_steps {
      history.plain(*back, step_spec);
    }
    let head = history.green.head();
    let tree = files_at(&history.green, head.saturating_sub(u64::from(back)));
    let before = files_at(&history.green, head);
    let (work, after) = work_over(&tree, &spec, (&mut history.fresh, false));
    let upper = work.upper(history.next_id());
    let landed = history.green.land_over_tree(&Files(tree.clone()), &upper);
    let (model, applied, refused) = tree_model((&tree, &before), &after);
    let mut census = census.borrow_mut();
    census.tree_applied += applied;
    census.tree_refused += refused;
    match (model, landed) {
      (Some(model), Landed::Accepted { version, .. }) => {
        prop_assert_eq!(version, head + 1);
        prop_assert_eq!(files_at(&history.green, version), model);
      }
      (None, Landed::Conflict { .. }) => prop_assert_eq!(history.green.head(), head),
      (model, landed) => prop_assert!(false, "model {:?}, green {:?}", model, landed),
    }
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  assert!(
    census.tree_applied > 0 && census.tree_refused > 0,
    "an answer never met: {census:?}"
  );
}
