//! One deflate state a shard (A-868; main 01a20182's queue item 4, "deflate reuse"): what deflates an object, a sample
//! or a corpus takes a state its shard thread keeps, reset and set to its level, and gives it back once done.
//!
//! **Why.** A zlib deflate state is a few hundred KiB (its window, hash chains and pending buffer) and making one is an
//! allocation and a clear of all of it. One a deflated object, and one a level for every sample a type's levels are
//! learned on (ten), made a clone's states O(objects × trials).
//!
//! **What.** [`deflater`] hands out a [`Deflater`]: a state kept on this thread (a shard's; the runtime is
//! thread-per-core, so what a thread keeps is its shard's), reset when given back and set to the level asked
//! (`Compress::set_level` on a reset stream, a fresh stream's output byte for byte), or one made when none is kept.
//! Dropping it gives it back, whether its object ended or its stream was dropped part way, so every path gives back.
//! A thread keeps every state given back, so what it keeps is at most the most deflates it had under way at once since
//! it last let them go: states made are O(the deflates under way at once on a shard), never O(objects), with no count to
//! tune (R3).
//! [`let_go`] drops what is kept, for a shard gone idle.
//!
//! **Counted.** [`counts`]: states made, states taken again, and those let go, on this thread since its last
//! [`take_counts`]; the server counts them on its shard.

use std::cell::{Cell, RefCell};

use flate2::{Compress, Compression};

/// The deflate states a thread made, took again and let go (the module doc, "Counted").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeflaterCounts {
  /// States made, none kept to take.
  pub made: u64,
  /// States taken again from those kept.
  pub reused: u64,
  /// States let go idle ([`let_go`]), or given back after the thread's keeping ended, dropped.
  pub dropped: u64,
}

thread_local! {
  static KEPT: RefCell<Vec<Compress>> = const { RefCell::new(Vec::new()) };
  static COUNTS: Cell<DeflaterCounts> = const {
    Cell::new(DeflaterCounts {
      made: 0,
      reused: 0,
      dropped: 0,
    })
  };
}

fn count(change: impl FnOnce(&mut DeflaterCounts)) {
  COUNTS.with(|counts| {
    let mut now = counts.get();
    change(&mut now);
    counts.set(now);
  });
}

/// A deflate state taken from its thread's kept states, given back when dropped (the module doc).
pub(crate) struct Deflater {
  compress: Option<Compress>,
}

/// A deflate state at `level`, with a zlib header: one kept on this thread reset and set to it, or one made.
pub(crate) fn deflater(level: u32) -> Deflater {
  let compression = Compression::new(level);
  let kept = KEPT.with(|kept| kept.borrow_mut().pop());
  let compress = match kept.map(|mut compress| compress.set_level(compression).map(|()| compress)) {
    Some(Ok(compress)) => {
      count(|counts| counts.reused = counts.reused.saturating_add(1));
      compress
    }
    _ => {
      count(|counts| counts.made = counts.made.saturating_add(1));
      Compress::new(compression, true)
    }
  };
  Deflater {
    compress: Some(compress),
  }
}

impl Deflater {
  /// Its state, to feed (`fragment::deflate_more`); refused only were it asked after being given back, which no
  /// caller can, since giving back is its drop.
  pub(crate) fn state(&mut self) -> Result<&mut Compress, crate::store::fragment::FragmentRefused> {
    self
      .compress
      .as_mut()
      .ok_or(crate::store::fragment::FragmentRefused::Deflate)
  }
}

impl Drop for Deflater {
  fn drop(&mut self) {
    let Some(mut compress) = self.compress.take() else {
      return;
    };
    compress.reset();
    // Kept whatever how many are: at most the deflates under way at once since the last let-go (the module doc).
    let kept = KEPT
      .try_with(|kept| {
        kept
          .try_borrow_mut()
          .map(|mut kept| kept.push(compress))
          .is_ok()
      })
      .unwrap_or(false);
    if !kept {
      count(|counts| counts.dropped = counts.dropped.saturating_add(1));
    }
  }
}

/// This thread's counts since its last [`take_counts`].
pub fn counts() -> DeflaterCounts {
  COUNTS.with(Cell::get)
}

/// This thread's counts since its last call, begun again at zero (the server counts them on its shard).
pub fn take_counts() -> DeflaterCounts {
  COUNTS.with(|counts| counts.replace(DeflaterCounts::default()))
}

/// Drops the states this thread keeps (a shard gone idle); how many it dropped, counted.
pub fn let_go() -> u64 {
  let dropped = KEPT.with(|kept| crate::to_u64(kept.take().len()));
  count(|counts| counts.dropped = counts.dropped.saturating_add(dropped));
  dropped
}
