//! The waker: a `RawWaker` whose data pointer is the task's packed handle word, with a vtable
//! whose `clone` and `drop` are no-ops (§4.3; [B: `RawWakerVTable` docs]: "these functions must
//! all be thread-safe", which a `Copy` word satisfies with no reference count).

use std::task::{RawWaker, RawWakerVTable, Waker};

use slates_mem::Encoded;

use crate::registry;

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

/// The waker for a task's packed word.
pub fn waker_for(word: Encoded) -> Waker {
  // SAFETY: the vtable's functions are thread-safe (they only read the word and route it) and
  // the data pointer is an integer, never dereferenced.
  unsafe { Waker::from_raw(raw(word)) }
}

/// The packed word a waker made by `waker_for` carries, or `None` for a foreign waker.
pub fn word_of(waker: &Waker) -> Option<Encoded> {
  if std::ptr::eq(waker.vtable(), &VTABLE) {
    Some(Encoded::from_word(
      u64::try_from(waker.data().addr()).unwrap_or(0),
    ))
  } else {
    None
  }
}

fn raw(word: Encoded) -> RawWaker {
  let addr = usize::try_from(word.word()).unwrap_or(0);
  RawWaker::new(std::ptr::without_provenance(addr), &VTABLE)
}

unsafe fn clone(data: *const ()) -> RawWaker {
  RawWaker::new(data, &VTABLE)
}

unsafe fn wake(data: *const ()) {
  registry::wake(Encoded::from_word(u64::try_from(data.addr()).unwrap_or(0)));
}

unsafe fn wake_by_ref(data: *const ()) {
  registry::wake(Encoded::from_word(u64::try_from(data.addr()).unwrap_or(0)));
}

unsafe fn drop(_data: *const ()) {}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_waker_carries_its_word_and_clones_for_free() {
    let word = Encoded::pack(9, 1234, 56).unwrap();
    let waker = waker_for(word);
    assert_eq!(word_of(&waker), Some(word));
    let cloned = waker.clone();
    assert!(waker.will_wake(&cloned));
    assert_eq!(word_of(&cloned), Some(word));
    assert_eq!(word_of(Waker::noop()), None);
  }
}
