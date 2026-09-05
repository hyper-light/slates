//! The inbound message word: one `u64` per message so the rings carry `Copy` words only
//! (§4.3). The top two bits select the kind; the rest is a packed handle word or a pointer.
//!
//! Shard ids are bounded well below 2^14 (see `registry::MAX_SHARDS`), so a packed handle word
//! never sets the top two bits, and a pointer never exceeds 62 bits on any target we build for.

use slates_mem::Encoded;

use crate::task::SpawnRequest;

/// Format: the kind field sits in the top two bits of the word.
const KIND_SHIFT: u32 = 62;
/// Format: kind values.
const KIND_WAKE: u64 = 0;
const KIND_SPAWN: u64 = 1;
const KIND_CANCEL: u64 = 2;
const KIND_SHUTDOWN: u64 = 3;
/// Format: the payload mask (62 bits).
const PAYLOAD_MASK: u64 = (1 << KIND_SHIFT) - 1;

/// A message to a shard.
#[derive(Debug)]
pub enum Msg {
  /// Wake the task named by the packed word.
  Wake(Encoded),
  /// Take ownership of a spawn request (a boxed future and its placement).
  Spawn(Box<SpawnRequest>),
  /// Cancel the task named by the packed word.
  Cancel(Encoded),
  /// Finish every task and exit the loop.
  Shutdown,
}

impl Msg {
  /// Packs the message into a word; a `Spawn` leaks its box into the word, which `from_word`
  /// takes back exactly once.
  pub fn into_word(self) -> u64 {
    match self {
      Msg::Wake(e) => (KIND_WAKE << KIND_SHIFT) | (e.word() & PAYLOAD_MASK),
      Msg::Spawn(request) => {
        let addr = u64::try_from(Box::into_raw(request).expose_provenance()).unwrap_or(0);
        (KIND_SPAWN << KIND_SHIFT) | (addr & PAYLOAD_MASK)
      }
      Msg::Cancel(e) => (KIND_CANCEL << KIND_SHIFT) | (e.word() & PAYLOAD_MASK),
      Msg::Shutdown => KIND_SHUTDOWN << KIND_SHIFT,
    }
  }

  /// Unpacks a word produced by `into_word`.
  ///
  /// # Safety
  /// A `Spawn` word must have come from `into_word` on this process and be unpacked exactly
  /// once, because it carries ownership of the boxed request.
  pub unsafe fn from_word(word: u64) -> Msg {
    let payload = word & PAYLOAD_MASK;
    match word >> KIND_SHIFT {
      KIND_SPAWN => {
        let addr = usize::try_from(payload).unwrap_or(0);
        // SAFETY: by the caller's contract the address is a leaked `Box<SpawnRequest>` that no
        // one else will unpack; provenance is recovered through the exposed address.
        let request =
          unsafe { Box::from_raw(std::ptr::with_exposed_provenance_mut::<SpawnRequest>(addr)) };
        Msg::Spawn(request)
      }
      KIND_CANCEL => Msg::Cancel(Encoded::from_word(payload)),
      KIND_SHUTDOWN => Msg::Shutdown,
      _ => Msg::Wake(Encoded::from_word(payload)),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn wake_cancel_and_shutdown_round_trip() {
    let e = Encoded::pack(3, 77, 5).unwrap();
    // SAFETY: neither word carries a box.
    unsafe {
      assert!(matches!(Msg::from_word(Msg::Wake(e).into_word()), Msg::Wake(x) if x == e));
      assert!(matches!(Msg::from_word(Msg::Cancel(e).into_word()), Msg::Cancel(x) if x == e));
      assert!(matches!(
        Msg::from_word(Msg::Shutdown.into_word()),
        Msg::Shutdown
      ));
    }
  }

  #[test]
  fn a_spawn_word_carries_the_box_exactly_once() {
    let request = Box::new(SpawnRequest::new(Box::pin(async {}), None));
    let word = Msg::Spawn(request).into_word();
    // SAFETY: the word came from `into_word` and is unpacked once.
    let back = unsafe { Msg::from_word(word) };
    assert!(matches!(back, Msg::Spawn(_)));
  }
}
