//! A push's kept-but-unlanded revisions pin the windows they name against a landed segment's let-go (goal 93; main's
//! ruling of 2026-11-24, w-perf's design: the write path's form of the rule a named or home read already follows).
//!
//! **Why.** A push lands as a run of segment changesets, each let go once durable: its texts' windows let go whatever
//! their holders (`let_go_object_landed`), since a landed window is brought back by its record. A window is shared by
//! content, so a window a landed revision A shares with a revision B the push kept and has not landed yet (an unchanged
//! run of a `.po` file between revisions) went with A's let-go, while B's segment is not on the node's store: a revision
//! C whose base is B then found B's window away and asked the store for B, which keeps no row of it, and the push was
//! refused `UnknownDeltaBase` (main's 57k push at 2ed3eb139, `i18n/da.po`). The windowed intake (C1 (2)) reads a base's
//! windows where it once held the base whole, which exposed it.
//!
//! **How.** [`Pins`] counts each window the push's kept-but-unlanded revisions name, a multiset; a let-go asks
//! [`Pins::holds`] of each window and keeps one held (counted [`Pins::skipped`]). A push's [`Counted`] adds the windows of
//! each revision as the intake keeps it ([`Counted::count_kept`], over `Intake::kept_contents`), takes a segment's own
//! out just before that segment's let-go ([`Counted::count_out`]), and gives back the rest at the push's end, whatever
//! it was ([`Counted::release`]); a window a later revision still names keeps its count, and goes with that one's
//! landing.
//!
//! **Bounded.** The windows of the revisions kept and not landed: never more than the content store holds windows, its
//! budget over its window ([`Pins::for_store`]); a count past that is refused typed ([`PinsFull`]), never grown.

use std::collections::BTreeMap;

use slates_vcs::{ContentId, ContentStore};

use crate::Node;
use crate::filerev::FileContents;

/// Why a pin was refused: the windows it would count pass the bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinsFull {
  /// The most windows counted at once.
  pub most: usize,
}

/// Windows pinned by revisions kept and not landed, each with how many name it (module docs).
#[derive(Debug, Default)]
pub struct Pins {
  /// Each window, and how many revisions' contents name it.
  counts: BTreeMap<ContentId, u32>,
  /// The most windows counted at once.
  most: usize,
  /// The windows a let-go kept because a count held them: the non-vacuity counter.
  skipped: core::cell::Cell<u64>,
}

impl Pins {
  /// Pins of at most `most` windows.
  pub fn within(most: usize) -> Pins {
    Pins {
      most,
      ..Pins::default()
    }
  }

  /// Pins bounded by what `store` can hold: its budget over its window.
  pub fn for_store(store: &ContentStore) -> Pins {
    let window = u64::try_from(store.window().max(1)).unwrap_or(u64::MAX);
    let most = usize::try_from(store.budget() / window).unwrap_or(usize::MAX);
    Pins::within(most.max(1))
  }

  /// Counts `windows` in, each once more: refused, nothing counted, where they would pass the bound.
  pub fn count_in(&mut self, windows: &[ContentId]) -> Result<(), PinsFull> {
    let new = windows
      .iter()
      .filter(|window| !self.counts.contains_key(window))
      .count();
    if self.counts.len().saturating_add(new) > self.most {
      return Err(PinsFull { most: self.most });
    }
    for window in windows {
      let count = self.counts.entry(*window).or_insert(0);
      *count = count.saturating_add(1);
    }
    Ok(())
  }

  /// Counts `windows` out, each once less, a window no longer named forgotten.
  pub fn count_out(&mut self, windows: &[ContentId]) {
    for window in windows {
      if let Some(count) = self.counts.get_mut(window) {
        *count = count.saturating_sub(1);
        if *count == 0 {
          self.counts.remove(window);
        }
      }
    }
  }

  /// Whether a revision kept and not landed names `window`: a let-go keeps it, counted.
  pub fn holds(&self, window: &ContentId) -> bool {
    let held = self.counts.contains_key(window);
    if held {
      self.skipped.set(self.skipped.get().saturating_add(1));
    }
    held
  }

  /// The windows a let-go kept because a count held them.
  pub fn skipped(&self) -> u64 {
    self.skipped.get()
  }

  /// The windows counted now.
  pub fn len(&self) -> usize {
    self.counts.len()
  }

  /// Whether none is counted.
  pub fn is_empty(&self) -> bool {
    self.counts.is_empty()
  }
}

/// The windows `contents`' objects are cut into in `store`, let go or not.
pub fn windows_of(store: &ContentStore, contents: &FileContents) -> Vec<ContentId> {
  contents
    .identities()
    .flat_map(|identity| store.object_layout(identity))
    .map(|(window, _)| window)
    .collect()
}

/// What one push counted into [`Pins`], by revision, so it takes out exactly what it put in (module docs).
#[derive(Debug, Default)]
pub struct Counted {
  /// Each revision counted, by its log's path and node, with the windows it counted.
  counted: BTreeMap<(Vec<u8>, Node), Vec<ContentId>>,
}

impl Counted {
  /// Counts in the windows of each of `kept` (the intake's kept and not yet taken) it has not counted: refused typed
  /// past the bound, what it counted before kept.
  pub fn count_kept<'k>(
    &mut self,
    (pins, store): (&mut Pins, &ContentStore),
    kept: impl Iterator<Item = &'k FileContents>,
  ) -> Result<(), PinsFull> {
    for contents in kept {
      let key = (contents.path.clone(), contents.node);
      if self.counted.contains_key(&key) {
        continue;
      }
      let windows = windows_of(store, contents);
      pins.count_in(&windows)?;
      self.counted.insert(key, windows);
    }
    Ok(())
  }

  /// Counts out the windows of `segment`'s revisions, before its let-go runs: theirs alone, a window a revision counted
  /// later still names kept.
  pub fn count_out(&mut self, pins: &mut Pins, segment: &[FileContents]) {
    for contents in segment {
      if let Some(windows) = self.counted.remove(&(contents.path.clone(), contents.node)) {
        pins.count_out(&windows);
      }
    }
  }

  /// Gives back everything it counted: the push's end, landed, refused or dropped.
  pub fn release(&mut self, pins: &mut Pins) {
    for (_, windows) in core::mem::take(&mut self.counted) {
      pins.count_out(&windows);
    }
  }

  /// The revisions it counts now.
  pub fn len(&self) -> usize {
    self.counted.len()
  }

  /// Whether it counts none.
  pub fn is_empty(&self) -> bool {
    self.counted.is_empty()
  }
}
