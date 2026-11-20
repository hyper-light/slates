//! The room a decode may use (A-582, as slates-hg's A-514; §4.2, A-16): what a request's budget admits for the
//! structures a decode builds beside the input it borrows. A decode counts what it will build before it builds anything
//! and charges the exact total here first, so a hostile stream that would need more than its request was admitted is
//! refused before one byte of it is held.
//!
//! **Held for a while (A-653 continued (4)).** A charge lasts as long as the request. Work that holds its bytes only
//! for a while inside the request, such as a pack stream's delta search over an object past a window, charges them with
//! [`Admission::hold`] instead: the [`Hold`] it answers gives the bytes back through [`Admission::release`] when the
//! work lets them go, so the next such work finds the room again. A hold is neither `Clone` nor `Copy`, so it is given
//! back once at most. One dropped without a release stays charged, which errs toward the request's room and never past
//! it. An admission gives back at most what its holds still keep: an admission is `Copy`, so a hold made on one copy
//! and released into another can never raise that copy's room past what it was admitted.

/// The bytes a decode may still allocate. The caller derives it from the request's budget; nothing here sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
  /// The bytes left to charge.
  left: u64,
  /// The bytes its holds keep that are not yet released: the most a release can give back.
  held: u64,
}

/// Bytes an admission lends for a while (module docs): what [`Admission::hold`] charged, until
/// [`Admission::release`] gives them back.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a hold's bytes stay charged until it is released"]
pub struct Hold {
  /// The bytes held.
  bytes: u64,
}

impl Hold {
  /// The bytes held.
  pub fn bytes(&self) -> u64 {
    self.bytes
  }
}

/// A decode that needed more room than its admission had left: refused before anything was built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PastAdmission {
  /// The bytes the decode asked for.
  pub asked: u64,
  /// The bytes the admission had left.
  pub left: u64,
}

impl Admission {
  /// An admission of `bytes`.
  pub fn new(bytes: u64) -> Self {
    Self {
      left: bytes,
      held: 0,
    }
  }

  /// The bytes left to charge.
  pub fn left(&self) -> u64 {
    self.left
  }

  /// Charges `bytes`, or refuses and charges nothing.
  pub fn charge(&mut self, bytes: u64) -> Result<(), PastAdmission> {
    match self.left.checked_sub(bytes) {
      Some(left) => {
        self.left = left;
        Ok(())
      }
      None => Err(PastAdmission {
        asked: bytes,
        left: self.left,
      }),
    }
  }

  /// Charges `count` values of `T`, each at its size.
  pub(crate) fn charge_items<T>(&mut self, count: usize) -> Result<(), PastAdmission> {
    self.charge(crate::to_u64(core::mem::size_of::<T>()).saturating_mul(crate::to_u64(count)))
  }

  /// Charges `bytes` for a while (module docs): the hold that gives them back, or refused with nothing charged.
  pub fn hold(&mut self, bytes: u64) -> Result<Hold, PastAdmission> {
    self.charge(bytes)?;
    self.held = self.held.saturating_add(bytes);
    Ok(Hold { bytes })
  }

  /// The bytes charged since this admission had `before` left, kept as a hold to give back once what they bought is
  /// dropped (A-745: a request's parsed lists are needed only until its answer is made, so they never pin the room
  /// its answer is written within).
  pub(crate) fn held_since(&mut self, before: u64) -> Hold {
    let bytes = before.saturating_sub(self.left);
    self.held = self.held.saturating_add(bytes);
    Hold { bytes }
  }

  /// Gives `hold`'s bytes back, at most what this admission's holds still keep (module docs).
  pub fn release(&mut self, hold: Hold) {
    let back = hold.bytes.min(self.held);
    self.held = self.held.saturating_sub(back);
    self.left = self.left.saturating_add(back);
  }
}

#[cfg(test)]
mod tests {
  use super::{Admission, PastAdmission};

  /// A-653 continued (4): do hold bytes and release them, then hold, charge others while the hold lasts, and release
  /// it. Expect: a hold and its release with nothing between them leave the admission exactly as it was; a hold past
  /// the room refused with nothing taken; the release giving back the hold's bytes alone, the charge staying.
  #[test]
  fn a_hold_and_its_release_leave_the_admission_as_it_was() {
    let mut admission = Admission::new(100);
    let before = admission;
    let hold = admission.hold(60).unwrap();
    admission.release(hold);
    assert_eq!(admission, before);
    let hold = admission.hold(30).unwrap();
    let refused = admission.hold(71);
    admission.charge(20).unwrap();
    let during = admission.left();
    admission.release(hold);
    assert_eq!(
      (refused, during, admission.left()),
      (
        Err(PastAdmission {
          asked: 71,
          left: 70
        }),
        50,
        80
      )
    );
  }

  /// A-653 continued (4): do hold bytes on one admission and release the hold into another that holds fewer, and
  /// into a fresh one that holds none. Expect: each release gives back no more than the admission released into still
  /// holds, so no admission's room ever passes what it was admitted.
  #[test]
  fn a_release_gives_back_no_more_than_was_held() {
    let mut lender = Admission::new(100);
    let mut other = Admission::new(100);
    let large = lender.hold(60).unwrap();
    let small = other.hold(10).unwrap();
    other.release(large);
    let after_large = other.left();
    other.release(small);
    let mut fresh = Admission::new(100);
    fresh.release(lender.hold(40).unwrap());
    assert_eq!((after_large, other.left()), (100, 100));
    assert_eq!(fresh, Admission::new(100));
  }

  /// A-582: do charge within an admission and past it. Expect the first charged and the second refused with nothing
  /// taken, so a refused decode leaves the request's room as it was.
  #[test]
  fn a_charge_past_the_admission_is_refused_and_takes_nothing() {
    let mut admission = Admission::new(100);
    assert_eq!(admission.charge(60), Ok(()));
    assert_eq!(
      admission.charge(41),
      Err(PastAdmission {
        asked: 41,
        left: 40
      })
    );
    assert_eq!(admission.left(), 40);
    assert_eq!(admission.charge_items::<u64>(5), Ok(()));
    assert_eq!(admission.left(), 0);
  }
}
