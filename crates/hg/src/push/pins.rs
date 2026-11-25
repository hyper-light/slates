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
//! [`Counted`] pins each revision's objects as the intake keeps it ([`Counted::count_kept`], over the revisions kept
//! since its last look, `Intake::kept_newest`), unpins a segment's own just before that segment's let-go ([`Counted::count_out`]: never at
//! its taking, since its record is written over turns before it lands), and gives back the rest however the push ends
//! ([`Counted::release`]). A window a later revision still names keeps its count, and goes with that one's landing.
//!
//! **Bounded.** A window's count is the store's (its width refused typed, `CountExhausted`); the ledger holds the push's
//! kept-but-unlanded revisions, the open segment and those cut and not landed.

use std::collections::BTreeMap;

use slates_vcs::{ContentId, ContentRefused, ContentStore};

use crate::Node;
use crate::filerev::FileContents;
use crate::push::intake::Intake;

/// One revision a push pinned: its log's path and the objects it pinned.
type Pinned = (Vec<u8>, Vec<ContentId>);

/// What one push pinned in the content store, by revision, so it unpins exactly what it pinned (module docs).
#[derive(Debug, Default)]
pub struct Counted {
  /// Each revision pinned, by node: its log's path and its objects (a node's revisions in two logs kept apart).
  counted: BTreeMap<Node, Vec<Pinned>>,
  /// The kept revisions [`Counted::count_kept`] has looked at, all told: its work, which must grow with the push's
  /// revisions, never with its slices times its kept ones (main's 57k push at 2dd25afe6).
  visited: u64,
  /// The intake's revisions kept that it has looked at ([`Intake::kept_total`]): the next look starts after them.
  seen: u64,
}

/// What one [`Counted::count_kept`] did: the kept revisions it looked at, and the windows it pinned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinLook {
  /// The kept revisions looked at: those kept since the last look.
  pub revisions: u64,
  /// The windows pinned.
  pub windows: u64,
}

impl Counted {
  /// Pins the objects of each revision `intake` kept since the last look ([`Intake::kept_newest`]; asked before the
  /// slice's segments are taken), each once: refused typed where a window's count would pass its width, what it pinned
  /// before kept. Its work is the slice's new revisions, never every kept one (main's 57k push at 2dd25afe6). What it
  /// looked at and pinned.
  pub fn count_kept(
    &mut self,
    store: &mut ContentStore,
    intake: &Intake,
  ) -> Result<PinLook, ContentRefused> {
    let total = intake.kept_total();
    let mut look = PinLook::default();
    for contents in intake.kept_newest(total.saturating_sub(self.seen)) {
      self.visited = self.visited.saturating_add(1);
      look.revisions = look.revisions.saturating_add(1);
      let pinned = self
        .counted
        .get(&contents.node)
        .is_some_and(|revisions| revisions.iter().any(|(path, _)| *path == contents.path));
      if pinned {
        continue;
      }
      let objects: Vec<ContentId> = contents.identities().copied().collect();
      for (at, object) in objects.iter().enumerate() {
        match store.pin_unlanded(object) {
          Ok(windows) => {
            look.windows = look
              .windows
              .saturating_add(u64::try_from(windows).unwrap_or(u64::MAX));
          }
          Err(refused) => {
            for done in objects.get(..at).unwrap_or_default() {
              store.unpin_unlanded(done);
            }
            return Err(refused);
          }
        }
      }
      self
        .counted
        .entry(contents.node)
        .or_default()
        .push((contents.path.clone(), objects));
    }
    self.seen = total;
    Ok(look)
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

  /// The kept revisions it has looked at, all told.
  pub fn visited(&self) -> u64 {
    self.visited
  }
}
