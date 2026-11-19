//! The dump reader (the module documentation's reading rules). RED: a stand-in that reads nothing, so the tests can be
//! seen failing before the reader is written.

use super::{Admission, DumpError, Read};

/// Reads a dump a part at a time from a buffer its caller holds.
#[derive(Clone, Debug)]
pub struct Reader {
  admission: Admission,
}

impl Reader {
  /// A reader that has read nothing, within `admission`.
  pub fn new(admission: Admission) -> Self {
    Self { admission }
  }

  /// Reads on from the start of `buffer`; `ended` says the buffer holds the stream's last bytes.
  ///
  /// # Errors
  /// Every refusal of [`DumpError`] the stream meets.
  pub fn read<'a>(&mut self, buffer: &'a [u8], ended: bool) -> Result<Read<'a>, DumpError> {
    let _ = (buffer, ended, self.admission);
    Err(DumpError::OutOfOrder)
  }
}
