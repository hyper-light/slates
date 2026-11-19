//! The room a decode may use (A-514; §4.2, A-16): what a request's budget admits for the structures a decode builds
//! beside the input it borrows. A decode counts what it will build before it builds anything and charges the exact
//! total here first, so a hostile bundle that would need more than its request was admitted is refused before one
//! byte of it is held.

use crate::read::to_u64;

/// The bytes a decode may still allocate. The caller derives it from the request's budget; nothing here sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
  /// The bytes left to charge.
  left: u64,
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
    Self { left: bytes }
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
    self.charge(to_u64(core::mem::size_of::<T>()).saturating_mul(to_u64(count)))
  }
}

#[cfg(test)]
mod tests {
  use super::{Admission, PastAdmission};

  /// A-514: do charge within an admission and past it. Expect the first charged and the second refused with nothing
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
