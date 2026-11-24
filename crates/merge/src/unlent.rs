//! Reads of a file held by its content-plane object's name alone (A-760 (b) 4d), noted where a verdict meets one.
//!
//! A named block holds no bytes: its object's are the content store's, lent to the green only while a verdict reads
//! them ([`crate::engine::Green::lend`]). Every read of a block's bytes (`dims::Blocks::bytes`, under the engine's and
//! the history's readers) answers none for an unlent named block and notes its object here; every verdict is computed
//! inside [`scope`], and one that noted any object is never used: it is refused typed
//! ([`crate::engine::ComputeRefused::ContentUnread`]), its caller lends the objects
//! ([`crate::engine::Green::unlent_reads`] names them all) and judges again. So no verdict is ever computed over bytes the green did not hold, without the engine reading
//! a store.
//!
//! The noting is thread-local (a green is not shared across threads: its counters are cells), and scopes nest: an
//! inner scope's objects are its own and its caller's.

use std::cell::RefCell;
use std::collections::BTreeSet;

use crate::named::Identity;

thread_local! {
  static UNLENT: RefCell<Option<BTreeSet<Identity>>> = const { RefCell::new(None) };
}

/// Notes `identity` as read unlent, inside a scope; nothing outside one (a reader outside a verdict refuses by type).
pub(crate) fn note(identity: Identity) {
  UNLENT.with(|unlent| {
    if let Some(set) = unlent.borrow_mut().as_mut() {
      set.insert(identity);
    }
  });
}

/// Puts the caller's set back however the scope ends.
struct Restore(Option<Option<BTreeSet<Identity>>>);

impl Drop for Restore {
  fn drop(&mut self) {
    if let Some(outer) = self.0.take() {
      let _ = UNLENT.try_with(|unlent| {
        let _ = unlent.replace(outer);
      });
    }
  }
}

/// `f`'s answer and the objects it read unlent; the caller's scope, if any, notes them too.
pub(crate) fn scope<T>(f: impl FnOnce() -> T) -> (T, BTreeSet<Identity>) {
  let outer = UNLENT.with(|unlent| unlent.replace(Some(BTreeSet::new())));
  let mut restore = Restore(Some(outer));
  let answer = f();
  let mine = UNLENT
    .with(|unlent| unlent.replace(restore.0.take().unwrap_or_default()))
    .unwrap_or_default();
  for identity in &mine {
    note(*identity);
  }
  (answer, mine)
}

#[cfg(test)]
mod tests {
  use super::{note, scope};
  use crate::named::Identity;

  fn identity(byte: u8) -> Identity {
    Identity {
      hash: [byte; 32],
      len: u64::from(byte),
    }
  }

  #[test]
  fn a_scope_keeps_its_own_and_its_callers_notes() {
    note(identity(9));
    let ((), outer) = scope(|| {
      note(identity(1));
      let ((), inner) = scope(|| note(identity(2)));
      assert_eq!(inner.into_iter().collect::<Vec<_>>(), vec![identity(2)]);
    });
    assert_eq!(
      outer.into_iter().collect::<Vec<_>>(),
      vec![identity(1), identity(2)]
    );
    let ((), after) = scope(|| ());
    assert!(after.is_empty());
  }
}
