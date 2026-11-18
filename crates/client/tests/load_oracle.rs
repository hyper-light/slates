//! The load test's oracle against the serial reference it replaced, and against the failures it must find (§4.16 "the
//! deterministic verdict"; AC-6.1, AC-6.2; Phase 9, AC-9.1). The oracle (`crates/client/examples/support/load_oracle.rs`)
//! judges every submit of a load level offline; at 100,000 clients its first form, kept here as [`reference`], scanned
//! every accepted submit for every conflict and tried every head between a conflict's bounds. The faster oracle must
//! answer exactly what the reference answers, every failure named in the same words and order, over histories a
//! serial green generates (each submit judged by the design's rule at the head it meets), some of them made wrong on
//! purpose. Non-vacuity: generated histories pass, each kind of failure is found, and some conflict fits only at a head
//! past its lower bound, the case the faster walk must not skip.
// Test code: an unwrap or an index here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

// A submit's `round` and `classes` are the harness's (its own-file oracle and its event dump); the rule reads neither.
#[allow(dead_code)]
#[path = "../examples/support/load_oracle.rs"]
mod load_oracle;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use load_oracle::{Outcome, Slot, SubmitEvent};
use proptest::strategy::Strategy;
use proptest::test_runner::{Config, TestRunner};

/// Shape: the files of a generated green, few so generated submits collide.
const FILES: usize = 3;
/// Shape: the slots of each file a submit picks among.
const SLOTS: u64 = 3;
/// Shape: a slot's length in bytes.
const SLOT_LEN: usize = 8;
/// Shape: the distance between slot starts, two slots, so no two slots are adjacent (the harness's own stride).
const STRIDE: u64 = 2 * SLOT_LEN as u64;
/// Shape: the time between two generated verdicts, in nanoseconds; a submit is sent up to two and a half of them
/// before its verdict and answered up to two and a half after, so its bounds span several heads.
const VERDICT_GAP_NS: u64 = 100;
/// Shape: the generated instants start here, so a send before the first verdict is still a positive instant.
const EPOCH_NS: u64 = 1_000;
/// Shape: the corruption values that leave a generated submit as the green judged it (of 0 to 16).
const TRUTHFUL_BELOW: u8 = 10;

/// The serial oracle the faster one replaced, as the harness had it until 2026-11-17: both bounds counted by a scan of
/// every accepted submit, every head between them tried, a slot's writes scanned, and the replay checked against a
/// whole copy of the head. It is the specification the faster oracle must equal. Three details differ from the
/// harness's copy, each reached only by a slot shorter than the harness's 64 bytes (these are 8) or a version 0: a
/// window's length is its slot's own (the harness wrote the 64-byte constant), a quote stops at the slot's end, and an
/// accept numbered 0 is judged at head 0.
mod reference {
  use super::load_oracle::{Outcome, SubmitEvent};
  use std::collections::{BTreeMap, BTreeSet};

  /// Shape: the failures named, as the harness printed them.
  const NAMED: usize = 20;
  /// Format: the bytes a failure quotes.
  const QUOTED_BYTES: usize = 16;

  type SlotWrites<'a> = BTreeMap<(&'a str, u64), Vec<(u64, &'a [u8])>>;

  /// What the reference found: the failures counted, the first named, the accepted, conflicting and unknown
  /// submits, and the files checked.
  #[derive(Debug, PartialEq, Eq)]
  pub(crate) struct Found {
    pub(crate) failures: u64,
    pub(crate) named: Vec<String>,
    pub(crate) accepted: usize,
    pub(crate) conflicts: usize,
    pub(crate) unknown: usize,
    pub(crate) files_checked: usize,
  }

  fn slot_writes<'a>(accepted: &[&'a SubmitEvent]) -> SlotWrites<'a> {
    let mut writes: SlotWrites<'a> = BTreeMap::new();
    for (index, event) in accepted.iter().enumerate() {
      let version = index as u64 + 1;
      for slot in &event.slots {
        writes
          .entry((slot.path.as_str(), slot.at))
          .or_default()
          .push((version, slot.bytes.as_slice()));
      }
    }
    writes
  }

  fn slot_at<'a>(
    writes: &SlotWrites<'a>,
    base: &'a BTreeMap<String, Vec<u8>>,
    (path, at, len): (&str, u64, usize),
    version: u64,
  ) -> &'a [u8] {
    let written = writes.get(&(path, at)).and_then(|history| {
      history
        .iter()
        .rev()
        .find(|(written, _)| *written <= version)
        .map(|(_, bytes)| *bytes)
    });
    written.unwrap_or_else(|| {
      let start = usize::try_from(at).unwrap();
      &base[path][start..start + len]
    })
  }

  fn replay(
    base: &BTreeMap<String, Vec<u8>>,
    writes: &SlotWrites<'_>,
  ) -> BTreeMap<String, Vec<u8>> {
    let mut state = base.clone();
    for ((path, at), history) in writes {
      if let Some((_, bytes)) = history.last() {
        let start = usize::try_from(*at).unwrap();
        state.get_mut(*path).unwrap()[start..start + bytes.len()].copy_from_slice(bytes);
      }
    }
    state
  }

  pub(crate) fn expected_windows(
    event: &SubmitEvent,
    (writes, base): (&SlotWrites<'_>, &BTreeMap<String, Vec<u8>>),
    head: u64,
  ) -> BTreeSet<(String, u64, u64)> {
    event
      .slots
      .iter()
      .filter(|slot| {
        writes
          .get(&(slot.path.as_str(), slot.at))
          .is_some_and(|history| history.iter().any(|(v, _)| *v > event.base && *v <= head))
      })
      .filter(|slot| {
        slot_at(writes, base, (&slot.path, slot.at, slot.bytes.len()), head)
          != slot.bytes.as_slice()
      })
      .map(|slot| (slot.path.clone(), slot.at, slot.bytes.len() as u64))
      .collect()
  }

  fn explain(
    event: &SubmitEvent,
    (writes, base): (&SlotWrites<'_>, &BTreeMap<String, Vec<u8>>),
    (path, at): (&str, u64),
    head: u64,
  ) -> String {
    let touched: Vec<u64> = writes
      .get(&(path, at))
      .map(|history| history.iter().map(|(v, _)| *v).collect())
      .unwrap_or_default();
    let mine = event
      .slots
      .iter()
      .find(|slot| slot.path == path && slot.at == at);
    let held = slot_at(writes, base, (path, at, QUOTED_BYTES), head);
    format!(
      "{path}@{at}: touched by versions {touched:?}; head {:?}; mine {:?}",
      String::from_utf8_lossy(held),
      mine.map(|slot| {
        String::from_utf8_lossy(slot.bytes.get(..QUOTED_BYTES).unwrap_or(&slot.bytes)).into_owned()
      })
    )
  }

  /// The accepted submits in version order, as the harness sorted them.
  pub(crate) fn accepted(submits: &[SubmitEvent]) -> Vec<&SubmitEvent> {
    let mut accepted: Vec<&SubmitEvent> = submits
      .iter()
      .filter(|e| matches!(e.outcome, Outcome::Accepted(_)))
      .collect();
    accepted.sort_by_key(|e| match e.outcome {
      Outcome::Accepted(v) => v,
      Outcome::Conflict(_) | Outcome::Unknown => 0,
    });
    accepted
  }

  /// A conflict's bounds, counted by scanning every accepted submit.
  pub(crate) fn bounds(accepted: &[&SubmitEvent], event: &SubmitEvent) -> (u64, u64) {
    let low = accepted
      .iter()
      .filter(|e| e.replied_ns < event.sent_ns)
      .count() as u64;
    let high = accepted
      .iter()
      .filter(|e| e.sent_ns < event.replied_ns)
      .count() as u64;
    (low, high)
  }

  /// The whole judgment, the head given as a whole copy.
  pub(crate) fn oracle(
    submits: &[SubmitEvent],
    base: &BTreeMap<String, Vec<u8>>,
    head: &BTreeMap<String, Vec<u8>>,
  ) -> Found {
    let mut failures = 0u64;
    let mut named = Vec::new();
    let mut fail = |what: String| {
      failures += 1;
      if named.len() < NAMED {
        named.push(what);
      }
    };
    let accepted = accepted(submits);
    for (index, event) in accepted.iter().enumerate() {
      if let Outcome::Accepted(v) = event.outcome
        && v != index as u64 + 1
      {
        fail(format!(
          "accepted versions are not 1..=V: position {index} holds {v}"
        ));
      }
    }
    let writes = slot_writes(&accepted);
    let judged = (&writes, base);
    for event in submits {
      match &event.outcome {
        Outcome::Accepted(v) => {
          let expected = expected_windows(event, judged, v.saturating_sub(1));
          if !expected.is_empty() {
            fail(format!(
              "version {v} (base {}) was accepted over conflicts {expected:?}",
              event.base
            ));
          }
        }
        Outcome::Unknown => {}
        Outcome::Conflict(windows) => {
          let reported: BTreeSet<(String, u64, u64)> = windows.iter().cloned().collect();
          let (low, high) = bounds(&accepted, event);
          let fits =
            (low.max(event.base)..=high).any(|h| expected_windows(event, judged, h) == reported);
          if !fits {
            let top = expected_windows(event, judged, high);
            let why: Vec<String> = reported
              .difference(&top)
              .map(|(path, at, _)| explain(event, judged, (path, *at), high))
              .collect();
            fail(format!(
              "conflict (agent {}, base {}) reported {reported:?}; no head in {low}..={high} expects that; at {high} \
               expected {top:?}; unexpected: {why:?}",
              event.agent, event.base
            ));
          }
        }
      }
    }
    let replayed = replay(base, &writes);
    for (path, bytes) in &replayed {
      if head.get(path) != Some(bytes) {
        fail(format!(
          "the green's head of {path} differs from the replay of the accepted submits"
        ));
      }
    }
    Found {
      failures,
      named,
      accepted: accepted.len(),
      conflicts: submits
        .iter()
        .filter(|e| matches!(e.outcome, Outcome::Conflict(_)))
        .count(),
      unknown: submits
        .iter()
        .filter(|e| matches!(e.outcome, Outcome::Unknown))
        .count(),
      files_checked: replayed.len(),
    }
  }
}

/// The oracle under test over the same inputs, its head read one file at a time from `head`.
fn judged_by_the_oracle(
  submits: &[SubmitEvent],
  base: &BTreeMap<String, Vec<u8>>,
  head: &BTreeMap<String, Vec<u8>>,
) -> reference::Found {
  let (judged, writes) = load_oracle::judge_submits(submits, base);
  let mut failures = judged.failures;
  let files_checked = load_oracle::judge_heads(
    base.clone(),
    &writes,
    |path| head.get(path).cloned().unwrap_or_default(),
    &mut failures,
  );
  reference::Found {
    failures: failures.count,
    named: failures.named,
    accepted: judged.accepted,
    conflicts: judged.conflicts,
    unknown: judged.unknown,
    files_checked,
  }
}

/// One generated submit: who sends it, how far its base trails the head it meets, the slots it writes (file, slot,
/// whether its bytes are the shared ones every client writes alike), when it is sent and answered around its verdict,
/// and whether it is made wrong after the green judged it.
#[derive(Clone, Debug)]
struct Step {
  agent: usize,
  lag: u64,
  picks: Vec<(usize, u64, bool)>,
  sent_before_ns: u64,
  replied_after_ns: u64,
  corruption: u8,
}

fn step() -> impl Strategy<Value = Step> {
  (
    0usize..5,
    0u64..4,
    proptest::collection::vec(
      (0usize..FILES, 0u64..SLOTS, proptest::bool::weighted(0.2)),
      1..4,
    ),
    1u64..250,
    1u64..250,
    0u8..17,
  )
    .prop_map(
      |(agent, lag, picks, sent_before_ns, replied_after_ns, corruption)| Step {
        agent,
        lag,
        picks,
        sent_before_ns,
        replied_after_ns,
        corruption,
      },
    )
}

fn path(file: usize) -> String {
  format!("dir/file-{file}")
}

/// A generated file's bytes before the level: letters, so a quoted window reads.
fn file_bytes(file: usize) -> Vec<u8> {
  let len = usize::try_from(SLOTS * STRIDE).unwrap();
  (0..len)
    .map(|at| b'a' + u8::try_from((file * 7 + at) % 26).unwrap())
    .collect()
}

/// A client's bytes for one slot in one step, or the shared bytes every client writes alike.
fn slot_bytes(agent: usize, step: usize, shared: bool) -> Vec<u8> {
  let mut bytes = if shared {
    b"SHARED".to_vec()
  } else {
    format!("{agent}:{step}").into_bytes()
  };
  bytes.resize(SLOT_LEN, b'.');
  bytes
}

/// A level as a serial green would answer it: each submit judged, in order, at the head it meets by the design's rule
/// (each of its slots a version past its base wrote and the head holds otherwise is a window); an accept applies its
/// slots. Then each submit's corruption, if any, and the head's. Returns the submits, the files before the level and
/// the green's head.
#[allow(clippy::type_complexity)]
fn generate(
  steps: &[Step],
) -> (
  Vec<SubmitEvent>,
  BTreeMap<String, Vec<u8>>,
  BTreeMap<String, Vec<u8>>,
) {
  let base: BTreeMap<String, Vec<u8>> = (0..FILES).map(|f| (path(f), file_bytes(f))).collect();
  let mut green = base.clone();
  let mut wrote: Vec<BTreeSet<(String, u64)>> = Vec::new();
  let mut submits = Vec::new();
  for (index, step) in steps.iter().enumerate() {
    let verdict_ns = EPOCH_NS + VERDICT_GAP_NS * (index as u64 + 1);
    let head = wrote.len() as u64;
    let event_base = head.saturating_sub(step.lag);
    let mut picked: BTreeMap<(usize, u64), bool> = BTreeMap::new();
    for (file, slot, shared) in &step.picks {
      picked.entry((*file, *slot)).or_insert(*shared);
    }
    let slots: Vec<Slot> = picked
      .into_iter()
      .map(|((file, slot), shared)| Slot {
        path: path(file),
        at: slot * STRIDE,
        bytes: slot_bytes(step.agent, index, shared),
      })
      .collect();
    let windows: Vec<(String, u64, u64)> = slots
      .iter()
      .filter(|slot| {
        wrote
          .iter()
          .skip(usize::try_from(event_base).unwrap())
          .any(|keys| keys.contains(&(slot.path.clone(), slot.at)))
      })
      .filter(|slot| {
        let at = usize::try_from(slot.at).unwrap();
        green[&slot.path][at..at + SLOT_LEN] != slot.bytes[..]
      })
      .map(|slot| (slot.path.clone(), slot.at, SLOT_LEN as u64))
      .collect();
    let outcome = if windows.is_empty() {
      for slot in &slots {
        let at = usize::try_from(slot.at).unwrap();
        green.get_mut(&slot.path).unwrap()[at..at + SLOT_LEN].copy_from_slice(&slot.bytes);
      }
      wrote.push(
        slots
          .iter()
          .map(|slot| (slot.path.clone(), slot.at))
          .collect(),
      );
      Outcome::Accepted(head + 1)
    } else {
      Outcome::Conflict(windows)
    };
    submits.push(SubmitEvent {
      base: event_base,
      sent_ns: verdict_ns - step.sent_before_ns,
      replied_ns: verdict_ns + step.replied_after_ns,
      slots,
      outcome,
      agent: step.agent,
      round: index,
      classes: Vec::new(),
    });
  }
  for (event, step) in submits.iter_mut().zip(steps) {
    corrupt(event, step.corruption);
  }
  if steps.iter().any(|step| step.corruption == 15) {
    green.get_mut(&path(0)).unwrap()[0] ^= 1;
  }
  (submits, base, green)
}

/// Makes `event` wrong in the way `corruption` names (below [`TRUTHFUL_BELOW`], none; 15 is the head's, in
/// [`generate`]): a conflict missing a window or naming one more, an accept turned into a conflict or numbered one past
/// its version, an outcome made unknown, or a base one version lower than the submit's own.
fn corrupt(event: &mut SubmitEvent, corruption: u8) {
  if corruption < TRUTHFUL_BELOW {
    return;
  }
  match (corruption, &mut event.outcome) {
    (10, Outcome::Conflict(windows)) if !windows.is_empty() => {
      windows.remove(0);
    }
    (11, Outcome::Conflict(windows)) => {
      if let Some(slot) = event.slots.iter().find(|slot| {
        !windows
          .iter()
          .any(|(path, at, _)| *path == slot.path && *at == slot.at)
      }) {
        windows.push((slot.path.clone(), slot.at, SLOT_LEN as u64));
      }
    }
    (12, Outcome::Accepted(_)) => {
      event.outcome = Outcome::Conflict(
        event
          .slots
          .iter()
          .map(|slot| (slot.path.clone(), slot.at, SLOT_LEN as u64))
          .collect(),
      );
    }
    (13, Outcome::Accepted(version)) => *version += 1,
    (14, _) => event.outcome = Outcome::Unknown,
    (16, _) => event.base = event.base.saturating_sub(1),
    _ => {}
  }
}

/// The kinds of failure the reference named in one history, for the non-vacuity tally.
#[derive(Default)]
struct Seen {
  passed: u64,
  conflict: u64,
  accepted_over: u64,
  order: u64,
  head: u64,
  fits_past_low: u64,
}

impl Seen {
  fn tally(&mut self, found: &reference::Found) {
    if found.failures == 0 {
      self.passed += 1;
    }
    for what in &found.named {
      if what.starts_with("conflict (agent") {
        self.conflict += 1;
      } else if what.contains("was accepted over conflicts") {
        self.accepted_over += 1;
      } else if what.starts_with("accepted versions are not") {
        self.order += 1;
      } else if what.starts_with("the green's head of") {
        self.head += 1;
      }
    }
  }
}

/// Holds, for every conflict of `submits`, that the heads the oracle tries expect exactly the sets every head between
/// the bounds expects; counts into `fits_past_low` each conflict that fits at some head but not at its range's first.
fn candidate_heads_cover_the_range(
  submits: &[SubmitEvent],
  base: &BTreeMap<String, Vec<u8>>,
  fits_past_low: &mut u64,
) -> Result<(), String> {
  let accepted = load_oracle::accepted_in_order(submits);
  let writes = load_oracle::slot_writes(&accepted);
  let instants = load_oracle::Instants::of(&accepted);
  let reference_accepted = reference::accepted(submits);
  for event in submits {
    let Outcome::Conflict(windows) = &event.outcome else {
      continue;
    };
    let (low, high) = instants.bounds(event);
    if (low, high) != reference::bounds(&reference_accepted, event) {
      return Err(format!("bounds of {event:?}"));
    }
    let from = low.max(event.base);
    if from > high {
      continue;
    }
    let every: BTreeSet<BTreeSet<(String, u64, u64)>> = (from..=high)
      .map(|head| load_oracle::expected_windows(event, (&writes, base), head))
      .collect();
    let tried: BTreeSet<BTreeSet<(String, u64, u64)>> =
      load_oracle::candidate_heads(event, &writes, (from, high))
        .into_iter()
        .map(|head| load_oracle::expected_windows(event, (&writes, base), head))
        .collect();
    if every != tried {
      return Err(format!(
        "heads {from}..={high} expect {every:?}, the heads tried {tried:?}"
      ));
    }
    let reported: BTreeSet<(String, u64, u64)> = windows.iter().cloned().collect();
    if every.contains(&reported)
      && load_oracle::expected_windows(event, (&writes, base), from) != reported
    {
      *fits_past_low += 1;
    }
  }
  Ok(())
}

/// AC-6.1, AC-6.2 (the load test's oracle, AC-9.1). Do: generate levels a serial green judges by the design's rule,
/// some submits made wrong after, and judge each with the oracle and with the serial reference it replaced. Expect: the
/// same failures, named in the same words and order, and the same counts; the heads the oracle tries expect what every
/// head between a conflict's bounds expects, and its bounds are the reference's. Non-vacuity: histories pass, each
/// kind of failure is named, and conflicts that fit only past their range's first head occur.
#[test]
fn the_oracle_judges_every_generated_level_as_the_serial_reference_does() {
  let seen = RefCell::new(Seen::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(512),
    ..Config::default()
  }));
  let outcome = runner.run(&proptest::collection::vec(step(), 1..40), |steps| {
    let (submits, base, head) = generate(&steps);
    let expected = reference::oracle(&submits, &base, &head);
    let found = judged_by_the_oracle(&submits, &base, &head);
    proptest::prop_assert_eq!(&found, &expected);
    let mut seen = seen.borrow_mut();
    seen.tally(&expected);
    let checked = candidate_heads_cover_the_range(&submits, &base, &mut seen.fits_past_low);
    proptest::prop_assert!(checked.is_ok(), "{:?}", checked);
    Ok(())
  });
  if let Err(failure) = outcome {
    panic!("{failure}");
  }
  let seen = seen.borrow();
  assert!(seen.passed > 0, "no generated level passed");
  assert!(seen.conflict > 0, "no wrong conflict was named");
  assert!(
    seen.accepted_over > 0,
    "no accept over a conflict was named"
  );
  assert!(seen.order > 0, "no version out of order was named");
  assert!(seen.head > 0, "no head differing from the replay was named");
  assert!(
    seen.fits_past_low > 0,
    "no conflict fit only past its range's first head"
  );
}

/// A slot of `file` at slot index `slot`, written with `bytes` padded to the slot.
fn slot(file: usize, slot: u64, bytes: &[u8]) -> Slot {
  let mut bytes = bytes.to_vec();
  bytes.resize(SLOT_LEN, b'.');
  Slot {
    path: path(file),
    at: slot * STRIDE,
    bytes,
  }
}

/// A submit of `slots` from `base`, sent and answered at the given instants.
fn submit(
  base: u64,
  (sent_ns, replied_ns): (u64, u64),
  slots: Vec<Slot>,
  outcome: Outcome,
) -> SubmitEvent {
  SubmitEvent {
    base,
    sent_ns,
    replied_ns,
    slots,
    outcome,
    agent: 0,
    round: 0,
    classes: Vec::new(),
  }
}

/// The files before the level, and the head after `accepted` (in version order) applied their slots.
fn files_after(
  accepted: &[&SubmitEvent],
) -> (BTreeMap<String, Vec<u8>>, BTreeMap<String, Vec<u8>>) {
  let base: BTreeMap<String, Vec<u8>> = (0..FILES).map(|f| (path(f), file_bytes(f))).collect();
  let mut head = base.clone();
  for event in accepted {
    for slot in &event.slots {
      let at = usize::try_from(slot.at).unwrap();
      head.get_mut(&slot.path).unwrap()[at..at + SLOT_LEN].copy_from_slice(&slot.bytes);
    }
  }
  (base, head)
}

/// AC-6.2 (AC-9.1). Do: a submit based on version 0 writes slots A and B; version 1, answered before it was sent,
/// wrote A; version 2, sent before its answer, wrote B; it was judged at head 2 and named both windows. Expect: the
/// oracle passes it, though its lowest head (1) expects only A: the walk reaches version 2, where B was written.
#[test]
fn a_conflict_that_fits_only_past_its_lowest_head_passes() {
  let one = submit(0, (10, 20), vec![slot(0, 0, b"one")], Outcome::Accepted(1));
  let two = submit(1, (30, 60), vec![slot(1, 0, b"two")], Outcome::Accepted(2));
  let mine = submit(
    0,
    (25, 70),
    vec![slot(0, 0, b"mine"), slot(1, 0, b"mine")],
    Outcome::Conflict(vec![
      (path(0), 0, SLOT_LEN as u64),
      (path(1), 0, SLOT_LEN as u64),
    ]),
  );
  let (base, head) = files_after(&[&one, &two]);
  let submits = vec![one, two, mine];
  let found = judged_by_the_oracle(&submits, &base, &head);
  assert_eq!(found.failures, 0, "{:?}", found.named);
  assert_eq!(found, reference::oracle(&submits, &base, &head));
}

/// AC-6.2 (AC-9.1). Do: the same level, but the conflict names only B, which no head between its bounds expects
/// alone (A was written at version 1, before it was sent). Expect: one failure, naming the conflict.
#[test]
fn a_conflict_missing_a_window_every_head_expects_fails() {
  let one = submit(0, (10, 20), vec![slot(0, 0, b"one")], Outcome::Accepted(1));
  let two = submit(1, (30, 60), vec![slot(1, 0, b"two")], Outcome::Accepted(2));
  let mine = submit(
    0,
    (25, 70),
    vec![slot(0, 0, b"mine"), slot(1, 0, b"mine")],
    Outcome::Conflict(vec![(path(1), 0, SLOT_LEN as u64)]),
  );
  let (base, head) = files_after(&[&one, &two]);
  let submits = vec![one, two, mine];
  let found = judged_by_the_oracle(&submits, &base, &head);
  assert_eq!(found.failures, 1, "{:?}", found.named);
  assert!(found.named[0].starts_with("conflict (agent 0, base 0)"));
  assert_eq!(found, reference::oracle(&submits, &base, &head));
}

/// AC-6.1 (AC-9.1). Do: version 2, based on version 0, accepted over slot A, which version 1 wrote with other bytes.
/// Expect: one failure, naming version 2 accepted over A's window.
#[test]
fn an_accept_over_a_conflict_fails() {
  let one = submit(0, (10, 20), vec![slot(0, 0, b"one")], Outcome::Accepted(1));
  let two = submit(0, (30, 40), vec![slot(0, 0, b"two")], Outcome::Accepted(2));
  let (base, head) = files_after(&[&one, &two]);
  let submits = vec![one, two];
  let found = judged_by_the_oracle(&submits, &base, &head);
  assert_eq!(found.failures, 1, "{:?}", found.named);
  assert!(found.named[0].starts_with("version 2 (base 0) was accepted over conflicts"));
  assert_eq!(found, reference::oracle(&submits, &base, &head));
}

/// AC-6.1 (AC-9.1). Do: two accepts over the same identical bytes from version 0 (the shared slot every client writes
/// alike). Expect: no failure: the second meets bytes equal to its own, which pass two accepts.
#[test]
fn an_accept_over_identical_bytes_passes() {
  let one = submit(
    0,
    (10, 20),
    vec![slot(0, 0, b"SHARED")],
    Outcome::Accepted(1),
  );
  let two = submit(
    0,
    (30, 40),
    vec![slot(0, 0, b"SHARED")],
    Outcome::Accepted(2),
  );
  let (base, head) = files_after(&[&one, &two]);
  let submits = vec![one, two];
  let found = judged_by_the_oracle(&submits, &base, &head);
  assert_eq!(found.failures, 0, "{:?}", found.named);
  assert_eq!(found, reference::oracle(&submits, &base, &head));
}

/// AC-9.1. Do: two accepts numbered 1 and 3, then a head whose one byte the replay does not hold. Expect: the version
/// order and the head each fail, in that order.
#[test]
fn versions_out_of_order_and_a_head_the_replay_does_not_hold_fail() {
  let one = submit(0, (10, 20), vec![slot(0, 0, b"one")], Outcome::Accepted(1));
  let three = submit(
    1,
    (30, 40),
    vec![slot(1, 0, b"three")],
    Outcome::Accepted(3),
  );
  let (base, mut head) = files_after(&[&one, &three]);
  head.get_mut(&path(2)).unwrap()[0] ^= 1;
  let submits = vec![one, three];
  let found = judged_by_the_oracle(&submits, &base, &head);
  assert_eq!(found.failures, 2, "{:?}", found.named);
  assert!(found.named[0].starts_with("accepted versions are not 1..=V: position 1 holds 3"));
  assert!(found.named[1].starts_with("the green's head of dir/file-2 differs"));
  assert_eq!(found, reference::oracle(&submits, &base, &head));
}
