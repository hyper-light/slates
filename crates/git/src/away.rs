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
//! A stop is keyed by the let-go it met, the content store's number for it (`ContentStore::away_number`), never by the
//! window's place alone: a window brought back by another reader and let go again before this one reads it is a new
//! let-go, waited for again, and only the same let-go met twice ends typed. Keyed by place, B1's red failed once in
//! the parallel run of the server's `git_push` tests (declined `missing necessary objects`), passing alone; the
//! window's read and its re-read lie either side of the driver's bring-back, and a derivation behind the first push,
//! which lets go at its end the windows it brought back, is the reader that can let it go between. An unpack's outside
//! bases key it by row and number too, since two rows may wait on one window and each is taken again in its turn.

use core::cell::Cell;

use crate::store::packed::Waited;

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

/// The let-go window a reader met, noted in its owner's own slot for that owner's driver alone to bring back (B1's
/// second cause under load: a push noted its window on the repository's one slot, which a derivation's turn clears at
/// its start, so its driver found nothing to bring back and the push was refused; main's lines of
/// agents/w-git-b1-debug, run 15). A reader given one notes there and nowhere else; one given none notes on its
/// repository's slot, which the owners without one of their own share (a green build, a replay, a derivation).
#[derive(Debug, Default)]
pub struct Waits {
  /// The window noted first since it was last taken.
  noted: Cell<Option<Waited>>,
}

impl Waits {
  /// Notes `waited`, where nothing is noted since the slot was last taken: a turn stops at its first.
  pub fn note(&self, waited: Waited) {
    if self.noted.get().is_none() {
      self.noted.set(Some(waited));
    }
  }

  /// The window noted, taken: what the owner's driver brings back after the turn.
  pub fn take(&self) -> Option<Waited> {
    self.noted.take()
  }

  /// The window noted, left as it is.
  pub fn peek(&self) -> Option<Waited> {
    self.noted.get()
  }
}

#[cfg(test)]
#[path = "tests/away.rs"]
mod tests;
