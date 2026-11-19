//! The room a request may use (A-595, as `slates-git`'s A-582 and `slates-hg`'s A-514; §4.2, A-16): what its budget
//! admits for the bytes a read holds. A read counts what it will hold before it holds anything and charges the exact
//! total here first, so a hostile request that would need more than it was admitted is refused before one byte past its
//! room is held.

/// The bytes a request may still hold. The caller derives it from the request's budget; nothing here sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
  /// The bytes left to charge.
  left: u64,
}

/// A read that needed more room than its admission had left: refused before anything past the room was held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PastAdmission {
  /// The bytes the read asked for.
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
}

#[cfg(test)]
mod tests {
  use super::{Admission, PastAdmission};

  /// A-595: do charge within an admission and past it. Expect the first charged and the second refused with nothing
  /// taken, so a refused read leaves the request's room as it was.
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
    assert_eq!(admission.charge(40), Ok(()));
    assert_eq!(admission.left(), 0);
  }
}
