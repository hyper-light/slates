//! The rest of a streaming answer (§4.17.4 "4. Reads": long reads are sliced under the shard's budget; item 11: no file
//! is held whole anywhere on the path; A-613 part 2): what a command writes after its head, resumed a slice at a time.
//!
//! A command whose answer grows with the repository (a file's text, and with the commands that follow, a listing, a
//! log, a file's revisions) writes its head into the caller's output and hands back a [`Stream`]. The caller writes
//! that output to the connection, then calls [`Stream::resume`] with the room it can give the next slice, and so on
//! until it answers [`Progress::Done`]. Each call writes at least one item, so a stream always ends, and stops once the
//! output has grown by the room it was given, so what the caller holds is bounded by the room it chose and one item
//! (for a text, one string of `svnserve`'s 4,096 bytes). The room is the caller's to derive from its shard's budget;
//! the stream has no number of its own.
//!
//! A stream reads only fixed revisions, which never change, so the index and the green may grow between slices.

use super::file::TextStream;
use super::{Repository, Session};
use crate::index::Index;
use crate::ra_svn::ItemError;

/// What remains of a streaming answer after its head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
  /// The command's own state.
  rest: Rest,
}

/// The commands that stream.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Rest {
  /// `get-file`'s text, its empty string and its second response.
  Text(TextStream),
}

/// Whether a streaming answer has more to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// More follows: resume the stream again.
  More,
  /// The answer is whole.
  Done,
}

impl Stream {
  /// A file's text to stream.
  pub(super) fn text(text: TextStream) -> Self {
    Self {
      rest: Rest::Text(text),
    }
  }

  /// Writes the answer's next items into `out`: at least one, then on until `out` has grown by `room` bytes or the
  /// answer is whole.
  ///
  /// # Errors
  /// [`ItemError`] when an item cannot be written, which no item here can give: every word written is the protocol's.
  pub fn resume<R: Repository + ?Sized>(
    &mut self,
    _index: &Index,
    repository: &R,
    _session: &Session<'_>,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<Progress, ItemError> {
    match &mut self.rest {
      Rest::Text(text) => text.resume(repository, out, room),
    }
  }
}
