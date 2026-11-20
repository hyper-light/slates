//! The rest of a streaming answer (§4.17.4 "4. Reads": long reads are sliced under the shard's budget; item 11: no file
//! is held whole anywhere on the path; A-613 part 2): what a command writes after its head, resumed a slice at a time.
//!
//! A command whose answer grows with the repository (a file's text, a listing, a node's location segments, a log, a
//! file's revisions) writes its head into the caller's output and hands back a
//! [`Stream`]. The caller writes that output to the connection, then calls [`Stream::resume`] with the room it can give
//! the next slice, and so on until it answers [`Progress::Done`]. Each call makes progress, so a stream always ends: it
//! writes at least one item, or takes at least one step of a walk. It stops once the bytes it wrote, and the steps it
//! took, reach the room it was given, so what one slice writes is bounded by the room and one item (for a text, one
//! string of `svnserve`'s 4,096 bytes; for a listing, one dirent; for segments, a segment and a gap), and what it reads
//! by the room. The room is the caller's to derive from its shard's budget; the stream has no number of its own.
//!
//! A stream reads only fixed revisions, which never change, so the index and the green may grow between slices.

use super::file::TextStream;
use super::file_revs::FileRevsStream;
use super::list::ListStream;
use super::log::LogStream;
use super::segments::SegmentStream;
use super::{Reads, Repository, Session};
use crate::index::Index;
use crate::ra_svn::ItemError;
use crate::report::exchange::Budget;

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
  /// `list`'s walk, its `done` and its answer.
  List(ListStream),
  /// `get-location-segments`' walk, its `done` and its answer.
  Segments(SegmentStream),
  /// `log`'s walk, its `done` and its answer, boxed: its frames make it the largest.
  Log(Box<LogStream>),
  /// `get-file-revs`' revisions, their deltas, `done` and the answer, boxed beside the log.
  FileRevs(Box<FileRevsStream>),
}

/// Whether a streaming answer has more to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// More follows: resume the stream again.
  More,
  /// The answer is whole.
  Done,
  /// The connection ends here without a further word, as `svnserve`'s does where an assertion aborts its process
  /// (`get-file-revs` of a file whose first location the session may not read); nothing more is written, and the
  /// caller ends the connection.
  Close,
}

impl Stream {
  /// A file's text to stream.
  pub(super) fn text(text: TextStream) -> Self {
    Self {
      rest: Rest::Text(text),
    }
  }

  /// A listing to stream.
  pub(super) fn list(list: ListStream) -> Self {
    Self {
      rest: Rest::List(list),
    }
  }

  /// A log to stream.
  pub(super) fn log(log: LogStream) -> Self {
    Self {
      rest: Rest::Log(Box::new(log)),
    }
  }

  /// A file's revisions to stream.
  pub(super) fn file_revisions(revisions: FileRevsStream) -> Self {
    Self {
      rest: Rest::FileRevs(Box::new(revisions)),
    }
  }

  /// Location segments to stream.
  pub(super) fn segments(segments: SegmentStream) -> Self {
    Self {
      rest: Rest::Segments(segments),
    }
  }

  /// Writes the answer's next items into `out`, until what this slice wrote and visited reaches the budget's room or the
  /// answer is whole; at least one step; `get-file-revs` within the budget's quantum too (A-751).
  ///
  /// # Errors
  /// [`ItemError`] when an item cannot be written, which no item here can give: every word written is the protocol's.
  pub fn resume<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    session: &Session<'_>,
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Progress, ItemError> {
    let room = budget.room;
    let mut reads = Reads {
      index,
      repository,
      base: session.base,
      uuid: session.uuid,
      authz: session.authz,
      authentication: session.authentication,
      out,
    };
    match &mut self.rest {
      Rest::Text(text) => text.resume(reads.repository, reads.out, room),
      Rest::List(list) => list.resume(&mut reads, room),
      Rest::Segments(segments) => segments.resume(&mut reads, room),
      Rest::Log(log) => log.resume(&mut reads, room),
      Rest::FileRevs(revisions) => revisions.resume(&mut reads, budget),
    }
  }
}
