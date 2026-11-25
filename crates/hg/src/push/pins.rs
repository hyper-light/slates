//! A push's kept-but-unlanded revisions pin the windows they name against a landed segment's let-go (goal 93; main's
//! ruling (B) of 2026-11-24: one rule at the one let-go every path uses, `ContentStore::pin_unlanded`; w-perf's design
//! for the push's side).
//!
//! **Why.** A push lands as a run of segment changesets, each let go once durable by `let_go_object_landed`, whatever a
//! window's holders. A window is shared by content, so one a landed revision A shared with a revision B the push had kept
//! and not landed (an unchanged run of a `.po` file between revisions) went with A's let-go while B was on no store: a
//! revision C on B then found B's window away and asked the store for B, which keeps no row of it, and the push was
//! refused `UnknownDeltaBase` (main's 57k push at 2ed3eb139, `i18n/da.po`). The windowed intake (C1 (2)) exposed it,
//! reading a base's windows where it once held the base whole.
//!
//! **How.** The content store keeps, beside each window, how many unlanded objects name it, and no let-go takes a window
//! while any does (`ContentStore::pin_unlanded`, `LetGoRefused::Unlanded`, counted `kept_unlanded`). A push's
//! [`Counted`] pins each revision's objects as the intake keeps it ([`Counted::count_kept`], over
//! `Intake::kept_contents`), unpins a segment's own just before that segment's let-go ([`Counted::count_out`]: never at
//! its taking, since its record is written over turns before it lands), and gives back the rest however the push ends
//! ([`Counted::release`]). A window a later revision still names keeps its count, and goes with that one's landing.
//!
//! **Bounded.** A window's count is the store's (its width refused typed, `CountExhausted`); the ledger holds the push's
//! kept-but-unlanded revisions, the open segment and those cut and not landed.

use std::collections::BTreeMap;

use slates_vcs::{ContentId, ContentRefused, ContentStore};

use crate::Node;
use crate::filerev::FileContents;

/// One revision a push pinned: its log's path and the objects it pinned.
type Pinned = (Vec<u8>, Vec<ContentId>);

/// What one push pinned in the content store, by revision, so it unpins exactly what it pinned (module docs).
#[derive(Debug, Default)]
pub struct Counted {
  /// Each revision pinned, by node: its log's path and its objects (a node's revisions in two logs kept apart).
  counted: BTreeMap<Node, Vec<Pinned>>,
}

impl Counted {
  /// Pins the objects of each of `kept` (the intake's kept and not yet taken) it has not pinned: refused typed where a
  /// window's count would pass its width, what it pinned before kept.
  pub fn count_kept<'k>(
    &mut self,
    store: &mut ContentStore,
    kept: impl Iterator<Item = &'k FileContents>,
  ) -> Result<(), ContentRefused> {
    for contents in kept {
      let pinned = self
        .counted
        .get(&contents.node)
        .is_some_and(|revisions| revisions.iter().any(|(path, _)| *path == contents.path));
      if pinned {
        continue;
      }
      let objects: Vec<ContentId> = contents.identities().copied().collect();
      for (at, object) in objects.iter().enumerate() {
        if let Err(refused) = store.pin_unlanded(object) {
          for done in objects.get(..at).unwrap_or_default() {
            store.unpin_unlanded(done);
          }
          return Err(refused);
        }
      }
      self
        .counted
        .entry(contents.node)
        .or_default()
        .push((contents.path.clone(), objects));
    }
    Ok(())
  }

  /// Unpins the objects of `segment`'s revisions, just before its let-go: theirs alone, a window a revision pinned later
  /// still names kept.
  pub fn count_out(&mut self, store: &mut ContentStore, segment: &[FileContents]) {
    for contents in segment {
      let Some(revisions) = self.counted.get_mut(&contents.node) else {
        continue;
      };
      if let Some(at) = revisions
        .iter()
        .position(|(path, _)| *path == contents.path)
      {
        let (_, objects) = revisions.swap_remove(at);
        for object in &objects {
          store.unpin_unlanded(object);
        }
      }
      if revisions.is_empty() {
        self.counted.remove(&contents.node);
      }
    }
  }

  /// Unpins everything it pinned: the push's end, landed, refused or dropped.
  pub fn release(&mut self, store: &mut ContentStore) {
    for (_, revisions) in core::mem::take(&mut self.counted) {
      for (_, objects) in revisions {
        for object in &objects {
          store.unpin_unlanded(object);
        }
      }
    }
  }

  /// Whether it pins none.
  pub fn is_empty(&self) -> bool {
    self.counted.is_empty()
  }
}
