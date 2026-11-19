//! `get-location-segments` (§4.17.4 "4. Reads": `get-location-segments` reads the history lists and copy sources;
//! A-613 part 2): where the node at a path in a peg revision lived across a range of revisions, as runs of revisions
//! at one path, youngest first, with the gaps between them, as `svnserve`'s `get_location_segments` and
//! `svn_repos_node_location_segments` write them (`serve.c`, `libsvn_repos/rev_hunt.c`, Subversion 1.15.0), held to
//! stock `svnserve`'s transcripts (`crates/svn/fixtures/reads`, session `get-location-segments`):
//! - The parameters are `c(?r)(?r)(?r)`: the path, the peg, start and end revisions. After the authentication request a
//!   missing or negative peg or start is the youngest, a missing or negative end 0. An end younger than the start, or a
//!   start younger than the peg, is refused after `done` ("Get-location-segments end revision must not be younger than
//!   start revision", "... start revision must not be younger than peg revision", 160006), and a peg past the youngest
//!   too ("No such revision N").
//! - The walk is `svn_repos_node_location_segments`' loop from the peg down to the end: each turn takes the closest
//!   copy of the path in the current revision (`svn_repos__prev_location`, [`super::Reads`]'s `previous_location`), the
//!   segment from that copy's revision (or the node's origin, `svn_fs_node_origin_rev`, when no copy is relevant) to
//!   the current revision, at the path, and moves to the copy's source; a segment is sent only where it meets the
//!   range, cropped to it, and a hole between a segment and its copy's source is sent as a gap, its path absent. An
//!   absent path is refused after the segments before it and `done`: "File not found: revision N, path 'P'".
//! - A segment is `( start end ( path ) )`, its path without the leading separator (the root's empty); a gap
//!   `( start end ( ) )`; then `done`, then `( success ( ) )` or the refusal.
//!
//! The walk is a [`super::Stream`], one turn a step, each slice ending once the bytes it wrote and the turns it took
//! reach the caller's room; what it keeps is the path and revision it moves to.

use super::stream::{Progress, Stream};
use super::{
  DONE, Failure, Outcome, Reads, Repository, SEPARATOR, SVN_ERR_FS_NO_SUCH_REVISION,
  optional_revnum, path_and_revision,
};
use crate::index::Revision;
use crate::ra_svn::{ItemError, Value, write};

/// `serve.c`'s refusal of an end revision younger than the start.
/// Format: `get_location_segments` (`serve.c`); recorded (fixture `get-location-segments`).
const END_YOUNGER: &str =
  "Get-location-segments end revision must not be younger than start revision";

/// `serve.c`'s refusal of a start revision younger than the peg.
/// Format: `get_location_segments` (`serve.c`); recorded (fixture `get-location-segments`).
const START_YOUNGER: &str =
  "Get-location-segments start revision must not be younger than peg revision";

/// One location segment: a run of revisions at one path, or a gap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Segment {
  /// Its oldest revision.
  pub(super) start: Revision,
  /// Its youngest revision.
  pub(super) end: Revision,
  /// The path, absolute without its leading separator; `None` for a gap.
  pub(super) path: Option<String>,
}

impl Segment {
  /// `( start end ( path ) )`, the path list empty for a gap (`gls_receiver`'s `"rr(?c)"`).
  fn value(&self) -> Value<'_> {
    Value::List(vec![
      Value::Number(self.start),
      Value::Number(self.end),
      Value::List(
        self
          .path
          .iter()
          .map(|path| Value::String(path.as_bytes()))
          .collect(),
      ),
    ])
  }
}

/// `maybe_crop_and_send_segment`: the segment cropped to `end..=start` when some of it lies there, else `None`.
fn cropped(segment: &Segment, start: Revision, end: Revision) -> Option<Segment> {
  if segment.start > start || segment.end < end {
    return None;
  }
  Some(Segment {
    start: segment.start.max(end),
    end: segment.end.min(start),
    path: segment.path.clone(),
  })
}

/// What one turn of the walk sends, and where the walk goes next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Turn {
  /// The segments it sends, at most a segment and a gap.
  pub(super) sent: Vec<Segment>,
  /// The path and revision the walk moves to: the copy's source; `None` when the node began at its path.
  pub(super) next: Option<(String, Revision)>,
}

/// A parsed `get-location-segments` request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SegmentsRequest {
  /// The path, the session's base joined with the given path.
  path: String,
  /// The peg revision as given.
  peg: Option<i64>,
  /// The start revision as given.
  start: Option<i64>,
  /// The end revision as given.
  end: Option<i64>,
}

impl SegmentsRequest {
  /// `svn_ra_svn__parse_tuple(params, "c(?r)(?r)(?r)")`.
  fn parse(params: &[Value<'_>], base: &str) -> Option<Self> {
    let (path, peg) = path_and_revision(params, base)?;
    Some(Self {
      path,
      peg,
      start: optional_revnum(params.get(2))?,
      end: optional_revnum(params.get(3))?,
    })
  }
}

impl<'reads, R: Repository + ?Sized> Reads<'reads, R> {
  /// `get-location-segments ( path [ peg ] [ start ] [ end ] )` (`serve.c` `get_location_segments`): the
  /// authentication request, the range checked, then a [`Stream`] of the walk; a range refused after `done`.
  pub(super) fn location_segments(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(request) = SegmentsRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    match self.segments_range(&request) {
      Ok((peg, start, end)) => Ok(Outcome::Streaming(Stream::segments(SegmentStream {
        start,
        end,
        next: Some((request.path, peg)),
        ended: false,
      }))),
      Err(failure) => {
        write(&Value::Word(DONE), self.out)?;
        self.failure(&failure)
      }
    }
  }

  /// The peg, start and end revisions as `serve.c` and `svn_repos_node_location_segments` settle and refuse them.
  fn segments_range(
    &self,
    request: &SegmentsRequest,
  ) -> Result<(Revision, Revision, Revision), Failure> {
    let youngest = i64::try_from(self.index.youngest()).map_err(|_| Failure::corrupt())?;
    let valid = |revision: Option<i64>| revision.filter(|&revision| revision >= 0);
    let start = valid(request.start).unwrap_or(youngest);
    let peg = valid(request.peg).unwrap_or(youngest);
    let end = valid(request.end).unwrap_or(0);
    if end > start {
      return Err(Failure::new(
        SVN_ERR_FS_NO_SUCH_REVISION,
        String::from(END_YOUNGER),
      ));
    }
    if start > peg {
      return Err(Failure::new(
        SVN_ERR_FS_NO_SUCH_REVISION,
        String::from(START_YOUNGER),
      ));
    }
    let peg = self.existing(peg)?;
    let as_revision = |revision: i64| Revision::try_from(revision).map_err(|_| Failure::corrupt());
    Ok((peg, as_revision(start)?, as_revision(end)?))
  }

  /// One turn of `svn_repos_node_location_segments`' loop at `path` in `revision`: the segment from the closest copy
  /// (or the node's origin) to `revision`, sent where it meets `end..=start`, the gap down to the copy's source sent
  /// likewise, and the copy's source to move to. Refused when the path is absent.
  pub(super) fn segment_turn(
    &self,
    path: &str,
    revision: Revision,
    start: Revision,
    end: Revision,
  ) -> Result<Turn, Failure> {
    let place = path.strip_prefix(SEPARATOR).unwrap_or(path);
    let mut segment = Segment {
      start: end,
      end: revision,
      path: Some(place.to_owned()),
    };
    let previous = self.previous_location(path, signed(revision)?)?;
    let next = match previous {
      None => {
        segment.start = self.origin(path, revision)?.max(end);
        None
      }
      Some((appeared, previous_path, previous_revision)) => {
        segment.start = unsigned(appeared)?;
        Some((previous_path, unsigned(previous_revision)?))
      }
    };
    let mut sent = Vec::new();
    let reached = match cropped(&segment, start, end) {
      Some(segment) => {
        let reached = segment.start;
        sent.push(segment);
        reached
      }
      None => segment.start,
    };
    if let Some((_, source)) = &next
      && reached.saturating_sub(*source) > 1
    {
      let gap = Segment {
        start: source.saturating_add(1),
        end: reached.saturating_sub(1),
        path: None,
      };
      sent.extend(cropped(&gap, start, end));
    }
    Ok(Turn { sent, next })
  }

  /// `svn_fs_node_origin_rev`: the revision the node at `path` in `revision` was born in, its line of history's.
  fn origin(&self, path: &str, revision: Revision) -> Result<Revision, Failure> {
    let node = self
      .node(path, revision)?
      .ok_or_else(|| Failure::not_found(path, revision))?;
    Ok(node.node_id().revision)
  }
}

/// A revision as a C `svn_revnum_t`.
fn signed(revision: Revision) -> Result<i64, Failure> {
  i64::try_from(revision).map_err(|_| Failure::corrupt())
}

/// An `svn_revnum_t` known to be a revision.
fn unsigned(revision: i64) -> Result<Revision, Failure> {
  Revision::try_from(revision).map_err(|_| Failure::corrupt())
}

/// A `get-location-segments` walk still to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SegmentStream {
  /// The range's youngest revision.
  start: Revision,
  /// The range's oldest revision.
  end: Revision,
  /// The path and revision of the next turn; `None` when the walk has ended.
  next: Option<(String, Revision)>,
  /// Whether `done` and the answer are written.
  ended: bool,
}

impl SegmentStream {
  /// Takes turns until the bytes written and the turns taken reach `room`, at least one, or until the walk ends.
  pub(super) fn resume<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    room: usize,
  ) -> Result<Progress, ItemError> {
    let start = reads.out.len();
    let mut turns = 0_usize;
    while !self.ended {
      self.turn(reads)?;
      turns = turns.saturating_add(1);
      let spent = reads.out.len().saturating_sub(start).saturating_add(turns);
      if !self.ended && spent >= room {
        return Ok(Progress::More);
      }
    }
    Ok(Progress::Done)
  }

  /// One turn: its segments written and the walk moved on, or the end written: `done`, then the answer or the
  /// refusal.
  fn turn<R: Repository + ?Sized>(&mut self, reads: &mut Reads<'_, R>) -> Result<(), ItemError> {
    let Some((path, revision)) = self
      .next
      .take()
      .filter(|(_, revision)| *revision >= self.end)
    else {
      return self.finish(reads, None);
    };
    match reads.segment_turn(&path, revision, self.start, self.end) {
      Ok(turn) => {
        for segment in &turn.sent {
          write(&segment.value(), reads.out)?;
        }
        self.next = turn.next;
        Ok(())
      }
      Err(failure) => self.finish(reads, Some(&failure)),
    }
  }

  /// `done`, then `( success ( ) )` or the refusal.
  fn finish<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    failure: Option<&Failure>,
  ) -> Result<(), ItemError> {
    self.ended = true;
    write(&Value::Word(DONE), reads.out)?;
    match failure {
      None => reads.success(Vec::new())?,
      Some(failure) => reads.failure(failure)?,
    };
    Ok(())
  }
}
