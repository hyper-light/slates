//! The load test's oracle (§4.16 "Submission" and "the deterministic verdict"; AC-6.1, AC-6.2; Phase 9, AC-9.1): what
//! every submit of a load level must satisfy, judged offline once the level is done, and the green's final bytes. The
//! harness (`crates/client/examples/load_bench.rs`) records each submit as a [`SubmitEvent`]; this module states the
//! design's rule over them and nothing else, so its own test (`crates/client/tests/load_oracle.rs`) holds it to the
//! serial reference it replaced, over generated histories, and to the failures it must find.
//!
//! **The rule.** The accepted versions are exactly `1..=V`, each once. An accepted submit at `v` has no slot that an
//! accepted version in `(base, v)` touched while the green's bytes at `v - 1` differ from the slot's. A conflict's
//! windows equal the rule's expected set at some head the verdict could have been judged against: no lower than the
//! count of accepted submits whose reply came back before this submit was sent, and no higher than the count of
//! accepted submits sent before this reply came back. Both bounds are sound: a version whose reply came back before
//! the send was judged before this verdict, and every version judged before this verdict was sent before its reply.
//! And the green's head bytes of every file the level touched equal the version-ordered replay of the accepted
//! submits over the files' bytes before the level.
//!
//! **Why its shape (the 10,000-client rung of 2026-11-07, BENCHMARKS "The load test's quiet arm after its walls").**
//! - The first form counted both bounds by scanning every accepted submit for every conflict: that rung's 111,708
//!   conflicts against its 26,598 accepts are 5.9 × 10⁹ comparisons, and at 100,000 clients, with about ten times of
//!   each, about 6 × 10¹¹, hours of the harness's own CPU after its clients were done (an extrapolation). The bounds
//!   are now two binary searches over the accepted instants, sorted once ([`Instants`]).
//! - It tried every head between the bounds, though the expected set reads a slot only through whether a version in
//!   `(base, head]` wrote it and what the slot holds at the head, both of which move only at a version that wrote one
//!   of the submit's slots. It now tries the first head and each such version up to the last ([`candidate_heads`]),
//!   which the every-head walk equals by construction. A slot's writes are looked up by binary search, not a scan.
//! - It held three whole copies of every file the level touched: the bytes before the level, the head's and the
//!   replay. Over the whole kernel (1.67 GB) that is about 5 GB in the harness. The replay now runs in place over the
//!   bytes before the level once the submits are judged ([`judge_heads`]), and each head file is read, compared and
//!   dropped one at a time.
// The harness and its test both name this file with `#[path]`.
use std::collections::{BTreeMap, BTreeSet};

/// Shape: the failures a level names in full; the rest are counted, so a level that fails everywhere still prints a
/// bounded report.
pub(crate) const FAILURES_NAMED: usize = 20;
/// Format: the bytes of a window a failure quotes, the head's and the submit's: enough to read the client tag
/// (`c000123r002`) each slot's bytes begin with.
const QUOTED_BYTES: usize = 16;

/// One slot an edit overwrites: a slot-aligned range of one file, and the bytes a client writes there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
  /// The file, relative to the green's root.
  pub(crate) path: String,
  /// The slot's offset in the file.
  pub(crate) at: u64,
  /// The bytes written over the slot: the slot's length.
  pub(crate) bytes: Vec<u8>,
}

/// What a submit came back with.
#[derive(Clone, Debug)]
pub(crate) enum Outcome {
  /// Accepted as this version.
  Accepted(u64),
  /// Refused, with each window the verdict named: its path, offset and length.
  Conflict(Vec<(String, u64, u64)>),
  /// The client gave up with the submit in flight (the daemon gone past its reconnect budget, a reply lost): the green
  /// may or may not have taken it, which the harness settles from the green itself before the oracle judges.
  Unknown,
}

/// The conflict classes a conflict reply named, kept beside the windows for the event dump.
pub(crate) type Classes = Vec<u8>;

/// One submit, as the oracle judges it.
#[derive(Clone, Debug)]
pub(crate) struct SubmitEvent {
  /// The green version the submit's work was based on.
  pub(crate) base: u64,
  /// When the submit was sent, nanoseconds from the level's start.
  pub(crate) sent_ns: u64,
  /// When its reply was taken, nanoseconds from the level's start.
  pub(crate) replied_ns: u64,
  /// The slots the submit's round wrote.
  pub(crate) slots: Vec<Slot>,
  /// What it came back with.
  pub(crate) outcome: Outcome,
  /// The logical client that sent it.
  pub(crate) agent: usize,
  /// The client's round.
  pub(crate) round: usize,
  /// The conflict classes its reply named.
  pub(crate) classes: Classes,
}

/// The failures of one judgment: every one counted, the first [`FAILURES_NAMED`] named.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Failures {
  /// Every failure found.
  pub(crate) count: u64,
  /// The first failures found, in the order the judgment found them.
  pub(crate) named: Vec<String>,
}

impl Failures {
  /// Counts a failure, and names it while fewer than [`FAILURES_NAMED`] are named.
  pub(crate) fn fail(&mut self, what: String) {
    self.count = self.count.saturating_add(1);
    if self.named.len() < FAILURES_NAMED {
      self.named.push(what);
    }
  }
}

/// What the judgment of a level's submits found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Judged {
  /// The failures, of the version order and of the submits.
  pub(crate) failures: Failures,
  /// Submits accepted.
  pub(crate) accepted: usize,
  /// Submits refused with conflicts.
  pub(crate) conflicts: usize,
  /// Submits whose outcome no one learned and the green did not take.
  pub(crate) unknown: usize,
}

/// Each slot's writes by the accepted versions, in version order: what the green held at a slot at any version is the
/// last write at or before it, else the slot's bytes before the level. Built once, linear in the accepted slots, so a
/// slot's bytes at a version are a lookup and a binary search.
pub(crate) type SlotWrites<'a> = BTreeMap<(&'a str, u64), Vec<(u64, &'a [u8])>>;

/// The accepted submits in version order, each at its position (`1..=V` when the green took them in order).
pub(crate) fn accepted_in_order(submits: &[SubmitEvent]) -> Vec<&SubmitEvent> {
  let mut accepted: Vec<&SubmitEvent> = submits
    .iter()
    .filter(|event| matches!(event.outcome, Outcome::Accepted(_)))
    .collect();
  accepted.sort_by_key(|event| match event.outcome {
    Outcome::Accepted(version) => version,
    Outcome::Conflict(_) | Outcome::Unknown => 0,
  });
  accepted
}

/// [`SlotWrites`] of `accepted`, each version its position in it, so every history is in version order.
pub(crate) fn slot_writes<'a>(accepted: &[&'a SubmitEvent]) -> SlotWrites<'a> {
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

/// The green's bytes at slot `(path, at)` of `len` bytes as of `version`: the last write at or before it, else the
/// slot's bytes in `base`.
pub(crate) fn slot_at<'a>(
  writes: &SlotWrites<'a>,
  base: &'a BTreeMap<String, Vec<u8>>,
  (path, at, len): (&str, u64, usize),
  version: u64,
) -> &'a [u8] {
  let written = writes.get(&(path, at)).and_then(|history| {
    let after = history.partition_point(|(written, _)| *written <= version);
    after
      .checked_sub(1)
      .and_then(|last| history.get(last))
      .map(|(_, bytes)| *bytes)
  });
  written.unwrap_or_else(|| {
    let start = usize::try_from(at).unwrap();
    &base[path][start..start + len]
  })
}

/// Whether a version in `(base, head]` wrote the slot whose history is `history`.
fn written_between(history: &[(u64, &[u8])], (base, head): (u64, u64)) -> bool {
  let first_after_base = history.partition_point(|(version, _)| *version <= base);
  history
    .get(first_after_base)
    .is_some_and(|(version, _)| *version <= head)
}

/// The windows the verdict's rule expects for `event` judged against head `head`: each slot that a version in
/// `(base, head]` touched, where the green's bytes at `head` differ from the slot's.
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
        .is_some_and(|history| written_between(history, (event.base, head)))
    })
    .filter(|slot| {
      slot_at(writes, base, (&slot.path, slot.at, slot.bytes.len()), head) != slot.bytes.as_slice()
    })
    .map(|slot| (slot.path.clone(), slot.at, slot.bytes.len() as u64))
    .collect()
}

/// The accepted submits' instants, each sorted once, so a conflict's bounds are two binary searches.
pub(crate) struct Instants {
  replied: Vec<u64>,
  sent: Vec<u64>,
}

impl Instants {
  /// The instants of `accepted`.
  pub(crate) fn of(accepted: &[&SubmitEvent]) -> Instants {
    let mut replied: Vec<u64> = accepted.iter().map(|event| event.replied_ns).collect();
    let mut sent: Vec<u64> = accepted.iter().map(|event| event.sent_ns).collect();
    replied.sort_unstable();
    sent.sort_unstable();
    Instants { replied, sent }
  }

  /// The lowest and highest head `event`'s verdict could have been judged against: the accepted submits whose reply
  /// came back before it was sent, and those sent before its reply came back.
  pub(crate) fn bounds(&self, event: &SubmitEvent) -> (u64, u64) {
    let low = self
      .replied
      .partition_point(|replied| *replied < event.sent_ns);
    let high = self.sent.partition_point(|sent| *sent < event.replied_ns);
    (low as u64, high as u64)
  }
}

/// The heads in `from..=to` at which `event`'s expected windows can differ from the head before: `from` and each
/// version in `(from, to]` that wrote one of its slots, ascending, once each. Between two of them neither what a slot
/// holds nor whether a version past the base wrote it moves, so every head in the range expects what one of these does.
pub(crate) fn candidate_heads(
  event: &SubmitEvent,
  writes: &SlotWrites<'_>,
  (from, to): (u64, u64),
) -> Vec<u64> {
  let mut heads = vec![from];
  for slot in &event.slots {
    if let Some(history) = writes.get(&(slot.path.as_str(), slot.at)) {
      let past_from = history.partition_point(|(version, _)| *version <= from);
      heads.extend(
        history
          .iter()
          .skip(past_from)
          .map(|(version, _)| *version)
          .take_while(|version| *version <= to),
      );
    }
  }
  heads.sort_unstable();
  heads.dedup();
  heads
}

/// Why the oracle did not expect a window at `(path, at)`: the versions that wrote it, and the head's bytes there
/// against the submit's.
fn explain(
  event: &SubmitEvent,
  (writes, base): (&SlotWrites<'_>, &BTreeMap<String, Vec<u8>>),
  (path, at): (&str, u64),
  head: u64,
) -> String {
  let touched: Vec<u64> = writes
    .get(&(path, at))
    .map(|history| history.iter().map(|(version, _)| *version).collect())
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

/// Judges a conflict: whether its windows are what the rule expects at some head between its bounds, and the failure
/// to name when they are not.
fn judge_conflict(
  event: &SubmitEvent,
  windows: &[(String, u64, u64)],
  judged: (&SlotWrites<'_>, &BTreeMap<String, Vec<u8>>),
  instants: &Instants,
) -> Option<String> {
  let reported: BTreeSet<(String, u64, u64)> = windows.iter().cloned().collect();
  let (low, high) = instants.bounds(event);
  let from = low.max(event.base);
  let fits = from <= high
    && candidate_heads(event, judged.0, (from, high))
      .into_iter()
      .any(|head| expected_windows(event, judged, head) == reported);
  if fits {
    return None;
  }
  let top = expected_windows(event, judged, high);
  let why: Vec<String> = reported
    .difference(&top)
    .map(|(path, at, _)| explain(event, judged, (path, *at), high))
    .collect();
  Some(format!(
    "conflict (agent {}, base {}) reported {reported:?}; no head in {low}..={high} expects that; at {high} \
     expected {top:?}; unexpected: {why:?}",
    event.agent, event.base
  ))
}

/// Judges every submit of a level against the rule over `base`, the touched files' bytes before the level: the version
/// order, each accept and each conflict. Returns what it found and the accepted writes, which [`judge_heads`] replays.
pub(crate) fn judge_submits<'a>(
  submits: &'a [SubmitEvent],
  base: &BTreeMap<String, Vec<u8>>,
) -> (Judged, SlotWrites<'a>) {
  let mut failures = Failures::default();
  let accepted = accepted_in_order(submits);
  for (index, event) in accepted.iter().enumerate() {
    if let Outcome::Accepted(version) = event.outcome
      && version != index as u64 + 1
    {
      failures.fail(format!(
        "accepted versions are not 1..=V: position {index} holds {version}"
      ));
    }
  }
  let writes = slot_writes(&accepted);
  let instants = Instants::of(&accepted);
  let judged = (&writes, base);
  let (mut conflicts, mut unknown) = (0, 0);
  for event in submits {
    match &event.outcome {
      Outcome::Accepted(version) => {
        let expected = expected_windows(event, judged, version.saturating_sub(1));
        if !expected.is_empty() {
          failures.fail(format!(
            "version {version} (base {}) was accepted over conflicts {expected:?}",
            event.base
          ));
        }
      }
      // An outcome still unknown once the harness settled it from the green was not taken by the green: it judges
      // nothing.
      Outcome::Unknown => unknown += 1,
      Outcome::Conflict(windows) => {
        conflicts += 1;
        if let Some(why) = judge_conflict(event, windows, judged, &instants) {
          failures.fail(why);
        }
      }
    }
  }
  let judged = Judged {
    failures,
    accepted: accepted.len(),
    conflicts,
    unknown,
  };
  (judged, writes)
}

/// Judges the green's final bytes: `base`, the touched files' bytes before the level, replayed in place with the last
/// accepted write of every slot, each file then compared with the green's head as `read_head` reads it and dropped,
/// one file at a time. Counts each differing file into `failures`; returns the files checked.
pub(crate) fn judge_heads(
  mut base: BTreeMap<String, Vec<u8>>,
  writes: &SlotWrites<'_>,
  mut read_head: impl FnMut(&str) -> Vec<u8>,
  failures: &mut Failures,
) -> usize {
  for ((path, at), history) in writes {
    if let Some((_, bytes)) = history.last() {
      let start = usize::try_from(*at).unwrap();
      base.get_mut(*path).unwrap()[start..start + bytes.len()].copy_from_slice(bytes);
    }
  }
  let files = base.len();
  for (path, replayed) in base {
    if read_head(&path) != replayed {
      failures.fail(format!(
        "the green's head of {path} differs from the replay of the accepted submits"
      ));
    }
  }
  files
}
