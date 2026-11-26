//! The one rule every reader a push drives keeps when a read meets a let-go window (goal 93 item 2b, the landed
//! let-go's readers; main's order of 2026-11-26 on w-git's read sweep, preconditions B1 to B7).
//!
//! A landed pack's windows may be let go: held on the node's store, not in RAM (`vcs_store::LANDED_LET_GO`). A read of
//! an object in such a window answers none, and the reader's repository names the window it met
//! ([`crate::receive::Repository::away`], [`crate::unpack::Contents::content_away`]). The reader is then not refused:
//! it stops, so the push's driver brings the window back (the server's `upload::brought_back_waited`, after every turn)
//! and takes the read again. A window that cannot be brought back must not hold the push for ever, so the same window
//! met again with no read answered between is not waited for: the reader goes on to its typed refusal, as it would for
//! an object nothing holds. [`WaitOnce`] is that rule, kept once for every reader (CLAUDE.md §2 item 7: one path, not a
//! retry copied into each site).
//!
//! What a stop is keyed by is the reader's: a window's offset in its pack for a peel or a check, and an unpack's row and
//! offset for its outside bases, since two rows may wait on one window and each is taken again in its turn.

use core::cell::Cell;

/// The window a reader last stopped for, so it stops at most once a window before its refusal (the module docs).
/// Kept by the reader across its turns; `K` is what names a stop.
#[derive(Clone, Debug)]
pub struct WaitOnce<K: Copy + PartialEq> {
  /// The stop last waited for, none once a read answered.
  last: Cell<Option<K>>,
}

impl<K: Copy + PartialEq> Default for WaitOnce<K> {
  fn default() -> Self {
    Self {
      last: Cell::new(None),
    }
  }
}

impl<K: Copy + PartialEq> WaitOnce<K> {
  /// The rule, its last stop `last` carried from a reader made again each turn (a push's verdicts, `Verdicts::peeled`).
  pub fn after(last: Option<K>) -> Self {
    Self {
      last: Cell::new(last),
    }
  }

  /// Whether a read that answered none waits for `stop`, the let-go window its repository named (none where it named
  /// none): yes the first time, the stop noted; no where it is the stop waited for last with no read answered since, or
  /// where no window was named, so the reader goes on to its refusal.
  pub fn waits(&self, stop: Option<K>) -> bool {
    let Some(stop) = stop else {
      return false;
    };
    if self.last.get() == Some(stop) {
      return false;
    }
    self.last.set(Some(stop));
    true
  }

  /// A read answered: the window waited for was brought back, so a later stop at it is waited for again.
  pub fn answered(&self) {
    self.last.set(None);
  }

  /// The stop last waited for, for a reader made again next turn to carry ([`WaitOnce::after`]).
  pub fn last(&self) -> Option<K> {
    self.last.get()
  }
}

#[cfg(test)]
#[path = "tests/away.rs"]
mod tests;
