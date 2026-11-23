//! The `replay-report` `svnsync synchronize` sends over HTTP (§4.17.4 item 5 "HTTP"; goal 93's svnsync rows; w-svn's
//! GAPS entry "svnsync from slates over the HTTP edge"): answered as `mod_dav_svn`'s `reports/replay.c` answers it
//! (Subversion 1.15.0), from the one replay the svn:// site drives. The revision is replayed by
//! [`crate::report::drive::replay::Replay`] (`svn_repos_replay2`, what `svnserve`'s `replay` and `replay-range` drive)
//! a call at a time, and each call is written into XML by [`super::drive::Drive::replay`] (`replay.c`'s editor), so
//! there is one path from the store to both edges and no second encoder of the drive (main 01a20182's ruling of
//! 2026-11-23: the store is the truth; w-svn's C2 moving the replay onto the changeset store carries this edge with it).
//!
//! The request (`super::request::ReplayReport`): `S:low-water-mark`, `S:send-deltas`, and the revision either in
//! `S:revision`, the old syntax's, sent to the session's URL, whose repository path is the replay's base; or, in the 1.8
//! syntax, named by the `!svn/rev/REV` resource the report is sent to, `S:include-path` its base (the root when
//! absent). An old-syntax report naming no revision is refused `400` "Request was missing the revision argument", with
//! no Subversion error number, as `replay.c`'s `dav_svn__replay_report` refuses it.
//!
//! **Bounded work.** The answer is streamed a slice at a time under the shard's budget, as the update-report's
//! ([`super::update`]): each slice takes the replay's calls a step budget at a time until it has written its room, or its
//! quantum has run where the budget names one, at least one step; a call that would take a slice past its room after
//! others is held whole for the next. A text the store reads asynchronously waits as the drive does
//! (`ReportProgress::Waiting`). Never a whole revision in one turn: svnrdump's one report of everything spun shard 0
//! before a469a6b85.
//!
//! A replay refused before its first byte (a revision past the youngest, a disagreeing index) is answered as
//! `dav_svn__convert_err` answers it, its error mapped to a status (`500` where none is), the error in `D:error`; one
//! refused after it ends the connection.
//!
//! Departure, stated: deltas are written in svndiff 0 whatever the request's `Accept-Encoding` prefers, as this site's
//! update-report writes them; a client decodes any version, so a mirror is the same.

use super::drive::Drive;
use super::locking::{HttpAnswer, convert_status, error_answer};
use super::request::ReplayReport;
use super::site::{ReportProgress, Served};
use crate::index::Revision;
use crate::reads::Repository;
use crate::report::drive::replay::{Replay, Stepped};
use crate::report::exchange::{Budget, Slice};
use crate::svndiff::Version;

/// The status a replay refused before its first byte is answered with, where `dav_svn__convert_err` maps no other.
/// Format: `reports/replay.c` `dav_svn__replay_report`, `HTTP_INTERNAL_SERVER_ERROR`.
const REPLAY_REFUSED_STATUS: u16 = 500;

/// The status an old-syntax report naming no revision is refused with.
/// Format: `reports/replay.c`, `HTTP_BAD_REQUEST`.
const HTTP_BAD_REQUEST: u16 = 400;

/// `replay.c`'s words for an old-syntax report naming no revision.
/// Format: `reports/replay.c` `dav_svn__replay_report`.
const MISSING_REVISION: &str = "Request was missing the revision argument";

/// Derived: a step of the replay is one bounded thing (its module doc), and a slice is charged its room in bytes, so a
/// slice's steps are its room's bytes, each step writing at least one byte or deciding one path, as the update-report's.
const STEPS_PER_BYTE: usize = 1;

/// Why a slice of the replay stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
  /// The replay made its last call.
  Ended,
  /// The slice wrote its room, or its quantum ran.
  Full,
  /// The drive was refused, or the replay failed.
  Refused,
}

/// A `replay-report`'s answer still to write (module docs).
#[derive(Debug)]
pub(super) struct ReplayWalk {
  /// The replay, a call at a time.
  replay: Replay,
  /// The answer's XML; none once it has ended or broken.
  drive: Option<Drive>,
  /// What a slice wrote past its room, sent first by the next.
  held: Vec<u8>,
  /// Whether any of the answer has been sent.
  begun: bool,
  /// The replay's failure, where it failed.
  failure: Option<(u32, String)>,
}

impl ReplayWalk {
  /// The replay `report` asks for: of `named`, the revision the `!svn/rev/REV` resource it is sent to names (the 1.8
  /// syntax), with `S:include-path` its base; else of `S:revision` with `base`, the repository path of the session URL
  /// it is sent to. Refused as `replay.c` refuses an old-syntax report naming no revision.
  pub(super) fn new(
    report: &ReplayReport,
    named: Option<Revision>,
    base: &str,
    room: usize,
  ) -> Result<Self, HttpAnswer> {
    let (revision, base) = match named {
      Some(revision) => (revision, report.include_path.as_deref().unwrap_or("/")),
      None => match report.revision {
        Some(revision) => (revision, base),
        None => return Err(error_answer(HTTP_BAD_REQUEST, true, 0, MISSING_REVISION)),
      },
    };
    Ok(Self {
      replay: Replay::new(revision, base, report.low_water_mark, report.send_deltas),
      drive: Some(Drive::replay(Version::Zero, room)),
      held: Vec::new(),
      begun: false,
      failure: None,
    })
  }

  /// Writes the answer's next slice into `out` from `served` within `budget` (module docs, "Bounded work").
  pub(super) fn resume<R: Repository + ?Sized>(
    &mut self,
    served: &Served<'_, R>,
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> ReportProgress {
    let room = budget.room.max(1);
    let slice = Slice::begin(budget.quantum);
    let mut written = std::mem::take(&mut self.held);
    let Some(mut drive) = self.drive.take() else {
      out.append(&mut written);
      return ReportProgress::Done;
    };
    let mut taken = !written.is_empty();
    let mut turns = 0usize;
    let stop = loop {
      if taken && slice.spent() {
        break Stop::Full;
      }
      let before = written.len();
      match self.replay.next_within(
        (served.index, served.repository),
        served.rules,
        budget.steps.max(1),
      ) {
        Ok(Stepped::Call(call)) => {
          if drive.call(call).is_err() {
            break Stop::Refused;
          }
        }
        Ok(Stepped::Paused) => {}
        Ok(Stepped::Waiting(wanted)) => {
          drive.take_written(&mut written);
          self.drive = Some(drive);
          self.begun |= !written.is_empty();
          out.extend_from_slice(&written);
          return ReportProgress::Waiting(wanted);
        }
        Ok(Stepped::Ended) => break Stop::Ended,
        Err(failure) => {
          self.failure = Some((failure.code, failure.message));
          break Stop::Refused;
        }
      }
      drive.take_written(&mut written);
      if written.len() > room && before > 0 {
        self.held = written.split_off(before);
        break Stop::Full;
      }
      turns = turns.saturating_add(STEPS_PER_BYTE);
      if written.len() >= room || (!slice.timed() && turns >= room) {
        break Stop::Full;
      }
      taken = true;
    };
    match stop {
      Stop::Refused => self.refused_or_broken(),
      Stop::Full => {
        self.drive = Some(drive);
        self.begun = true;
        out.extend_from_slice(&written);
        ReportProgress::More
      }
      Stop::Ended => {
        drive.take_written(&mut written);
        match drive.finish() {
          Ok(mut rest)
            if written.len().saturating_add(rest.len()) > room && !written.is_empty() =>
          {
            self.begun = true;
            self.held.append(&mut rest);
            out.extend_from_slice(&written);
            ReportProgress::More
          }
          Ok(rest) => {
            out.extend_from_slice(&written);
            out.extend_from_slice(&rest);
            ReportProgress::Done
          }
          Err(_) => self.refused_or_broken(),
        }
      }
    }
  }

  /// The replay's refusal: answered whole before anything was sent, else the connection ends.
  fn refused_or_broken(&mut self) -> ReportProgress {
    self.drive = None;
    if self.begun {
      return ReportProgress::Broken;
    }
    let (code, message) = self
      .failure
      .take()
      .unwrap_or((0, String::from("The replay's drive could not be written")));
    ReportProgress::Refused(error_answer(
      convert_status(code, REPLAY_REFUSED_STATUS),
      true,
      code,
      &message,
    ))
  }
}
