//! The dump writer (the module documentation's writing rules). RED: a stand-in that writes nothing, so the tests can be
//! seen failing before the writer is written.

use super::{DumpError, Event};

/// Writes a dump an event at a time, as `svnadmin dump` writes it.
#[derive(Clone, Debug, Default)]
pub struct Writer {
  written: u64,
}

impl Writer {
  /// A writer that has written nothing.
  pub fn new() -> Self {
    Self::default()
  }

  /// Appends `event`'s bytes to `out`.
  ///
  /// # Errors
  /// The refusal of an event the stream cannot take where it stands; nothing is appended then.
  pub fn write(&mut self, event: &Event<'_>, out: &mut Vec<u8>) -> Result<(), DumpError> {
    let _ = (event, out, self.written);
    Err(DumpError::OutOfOrder)
  }

  /// Ends the stream.
  ///
  /// # Errors
  /// [`DumpError::OutOfOrder`] when the stream would end inside a record.
  pub fn finish(&self) -> Result<(), DumpError> {
    Err(DumpError::OutOfOrder)
  }
}
