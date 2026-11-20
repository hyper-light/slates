//! Task labels (A-740): what a task is doing, named so a stuck shard's report can say which task it was polling.
//!
//! A watchdog that finds a shard spinning ("N ms of CPU without a step") knows the shard but not the work, and three
//! agents were left guessing which task spun (main, 2026-11-20: hg's whole push at 194 s, svnrdump's replay-range, a
//! forming run's 5.7 s). So every task carries a label, and the shard publishes the label of the task it is polling,
//! with that task's poll count, in its registry pulse before each poll: one plain store of one word on the line the
//! shard already writes each step, which the watchdog's thread reads when it judges the shard.
//!
//! A label is either where the task was spawned ([`TaskLabel::Site`], taken by `#[track_caller]` at every spawn
//! entry, so no spawn site can go unlabelled) or the work it names itself while it runs ([`TaskLabel::Named`], through
//! [`crate::futures::doing`], for an engine step that runs inside a longer task, such as a push's landing inside its
//! connection's task). A named label stands while its guard lives and the spawn site's returns after.
//!
//! The pulse word cannot hold a string, so a label is interned once into a process-wide table and the word holds its
//! index: an open-addressed table of [`LABEL_SLOTS`] write-once cells, probed from the label's hash, safe to read
//! from any thread and never locked. Interning costs a hash and, after a label's first use, one load; a full table
//! answers "unlabelled" (index 0) rather than refuse a spawn.

use std::panic::Location;
use std::sync::OnceLock;

/// Shape: the distinct labels one process can name: every spawn site and every named step of the workspace, with
/// room to spare. The workspace has 243 spawn calls (2026-11-20); a label past the table's room reads "unlabelled".
pub const LABEL_SLOTS: usize = 4096;

/// The labels named so far, each in the cell its hash probes to first that was free.
static LABELS: [OnceLock<TaskLabel>; LABEL_SLOTS] = [const { OnceLock::new() }; LABEL_SLOTS];

/// Format: FNV-1a's 64-bit offset basis (Fowler, Noll, Vo; `draft-eastlake-fnv`), the hash's starting state.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Format: FNV-1a's 64-bit prime, the hash's multiplier.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// What a task is labelled with.
#[derive(Clone, Copy, Debug)]
pub enum TaskLabel {
  /// Where it was spawned: a file, a line and a column of the workspace.
  Site(&'static Location<'static>),
  /// The work it named itself ([`crate::futures::doing`]), such as `"hg_push::land"`.
  Named(&'static str),
}

impl TaskLabel {
  /// Whether two labels name the same thing: one site, or one name.
  fn same(self, other: TaskLabel) -> bool {
    match (self, other) {
      (TaskLabel::Site(first), TaskLabel::Site(second)) => {
        first.file() == second.file()
          && first.line() == second.line()
          && first.column() == second.column()
      }
      (TaskLabel::Named(first), TaskLabel::Named(second)) => first == second,
      _ => false,
    }
  }

  /// The label's FNV-1a hash: a site's file, line and column, or a name's bytes.
  fn hash(self) -> u64 {
    let fold = |state: u64, bytes: &[u8]| {
      bytes.iter().fold(state, |state, byte| {
        (state ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
      })
    };
    match self {
      TaskLabel::Site(site) => {
        let state = fold(FNV_OFFSET, site.file().as_bytes());
        let state = fold(state, &site.line().to_le_bytes());
        fold(state, &site.column().to_le_bytes())
      }
      TaskLabel::Named(name) => fold(fold(FNV_OFFSET, b"named"), name.as_bytes()),
    }
  }
}

impl std::fmt::Display for TaskLabel {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TaskLabel::Site(site) => write!(formatter, "spawned at {}:{}", site.file(), site.line()),
      TaskLabel::Named(name) => formatter.write_str(name),
    }
  }
}

/// `label`'s index in the table, one past its cell; 0 when the table has no room for it.
pub(crate) fn intern(label: TaskLabel) -> u32 {
  let slots = u64::try_from(LABEL_SLOTS).unwrap_or(u64::MAX);
  let start = label
    .hash()
    .checked_rem(slots)
    .and_then(|at| usize::try_from(at).ok())
    .unwrap_or(0);
  for probe in 0..LABEL_SLOTS {
    let at = start
      .wrapping_add(probe)
      .checked_rem(LABEL_SLOTS)
      .unwrap_or(0);
    let Some(cell) = LABELS.get(at) else {
      return 0;
    };
    let held = match cell.get() {
      Some(held) => *held,
      None => *cell.get_or_init(|| label),
    };
    if held.same(label) {
      return u32::try_from(at.saturating_add(1)).unwrap_or(0);
    }
  }
  0
}

/// The calling site's label index: what a `#[track_caller]` spawn entry labels its task with.
#[track_caller]
pub(crate) fn caller() -> u32 {
  intern(TaskLabel::Site(Location::caller()))
}

/// The label interned at `index`; none for 0 (unlabelled) or an index no label holds.
pub fn label_of(index: u32) -> Option<TaskLabel> {
  let at = usize::try_from(index).ok()?.checked_sub(1)?;
  LABELS.get(at)?.get().copied()
}

/// What a shard was polling, as its pulse holds it ([`crate::registry::Pulse::polling`]): the task's label and how
/// many polls it has begun, this one included.
#[derive(Clone, Copy, Debug)]
pub struct Polling {
  /// The task's label, none when it is unlabelled.
  pub label: Option<TaskLabel>,
  /// Its polls begun, this one included; saturates at `u32::MAX`, the half of the word it is packed in.
  pub polls: u64,
}

impl Polling {
  /// The pulse word for task label `label` at its `polls`th poll: the label's index in the high half, the polls,
  /// saturated, in the low.
  pub(crate) fn pack(label: u32, polls: u64) -> u64 {
    (u64::from(label) << 32) | u64::from(u32::try_from(polls).unwrap_or(u32::MAX))
  }

  /// What a pulse word says.
  pub(crate) fn unpack(word: u64) -> Self {
    Self {
      label: label_of(u32::try_from(word >> u32::BITS).unwrap_or(0)),
      polls: word & u64::from(u32::MAX),
    }
  }
}

impl std::fmt::Display for Polling {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self.label {
      Some(label) => write!(formatter, "task {label}, at its poll {}", self.polls),
      None => write!(formatter, "an unlabelled task, at its poll {}", self.polls),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-740. Do: intern a site and a name twice each, and pack one with a poll count. Expect: each label keeps its
  /// index, the two differ, and the word gives back the label and the count.
  #[test]
  fn a_label_keeps_its_index_and_its_word_gives_it_back() {
    let site = caller();
    let again = intern(label_of(site).unwrap());
    let named = intern(TaskLabel::Named("label::test"));
    assert_eq!(site, again);
    assert_ne!(site, named);
    assert_eq!(named, intern(TaskLabel::Named("label::test")));
    let polling = Polling::unpack(Polling::pack(named, 7));
    assert_eq!(polling.polls, 7);
    assert_eq!(polling.to_string(), "task label::test, at its poll 7");
    assert!(
      label_of(site)
        .unwrap()
        .to_string()
        .starts_with("spawned at crates/rt/src/label.rs:")
    );
    assert!(label_of(0).is_none());
  }
}
