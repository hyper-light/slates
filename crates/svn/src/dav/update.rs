//! The update-report (§4.17.4 item 5 "HTTP" and piece 13; A-681): what stock svn's `checkout`, `update`, `switch`,
//! `export`, `diff` and `merge` ask of the DAV site, answered as `mod_dav_svn`'s `reports/update.c`
//! (`dav_svn__update_report`) answers it, from the drive ra_svn answers a report with.
//!
//! **The report.** Its parameters read as `update.c` reads them: the target revision, the youngest where none is given,
//! refused past the youngest ("No such revision", 160006, `404`); the depth, `S:depth`'s where it is sent, else
//! `files` for `S:recursive` `no` and `infinity` otherwise; the anchor from `S:src-path` and a switch's destination from
//! `S:dst-path`, each a URL whose path is beneath the repository's root (any scheme and host, as `dav_svn_split_uri`
//! reads only the path); the texts sent only in send-all mode. Its working-copy state, each `S:entry` (its `rev`
//! required) and `S:missing`, is recorded as ra_svn's `set-path`, `link-path` and `delete-path` are
//! (`crate::report::whole_report`), so one reporter checks both protocols' reports.
//!
//! **The answer.** A-616's drive over the index (`crate::report::drive::Drive`) makes the editor's calls, and A-627's
//! writer ([`super::drive::Drive`]) writes each as `update.c`'s editor does, in send-all mode with the texts inline when
//! the client asked for it; this site advertises bulk updates as `Prefer` (`SVN-Allow-Bulk-Updates`), so a stock client
//! asks for it. The answer is streamed: each resume takes the drive's calls until the slice's room is written or its
//! steps are spent, and hands what is written out ([`super::drive::Drive::take_written`]). A drive refused before its
//! first call is written is answered with its error, as `mod_dav` answers an error met before a byte is sent; one
//! refused later ends the connection.
//!
//! Stricter than `mod_dav_svn`, each stated: a report on a resource other than HTTPv2's `me` or a node's URL is
//! `501`; a `resource-walk` is not written (GAPS, as A-627 states).
//!
//! No I/O, no clock, no randomness, no panic.

use super::drive::{Drive as Writer, Update, Uris};
use super::locking::{HttpAnswer, convert_status, error_answer};
use super::request::{Reported, SvnDepth, UpdateReport};
use super::site::{ReportProgress, SPECIAL};
use crate::authz::PrincipalRules;
use crate::index::{Index, Revision};
use crate::lock::{AprTime, LockTable};
use crate::reads::Repository;
use crate::report::drive::{Drive, Next, Sources};
use crate::report::{
  Depth, RequestKind, WholeCommand, WholeItem, WholeRefusal, uri_decode, whole_report,
};
use crate::svndiff::Version;

/// `SVN_ERR_FS_NO_SUCH_REVISION`.
/// Format: `svn_error_codes.h`.
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;
/// `SVN_ERR_XML_ATTRIB_NOT_FOUND`: an entry without its `rev`.
/// Format: `svn_error_codes.h`.
const SVN_ERR_XML_ATTRIB_NOT_FOUND: u32 = 130_000;
/// `SVN_ERR_RA_DAV_REQUEST_FAILED`: a report's paths past the room it is held in.
/// Format: `svn_error_codes.h`.
const SVN_ERR_RA_DAV_REQUEST_FAILED: u32 = 175_002;
/// `SVN_ERR_RA_ILLEGAL_URL`: a source or destination outside the repository.
/// Format: `svn_error_codes.h`.
const SVN_ERR_RA_ILLEGAL_URL: u32 = 170_000;
/// The status of an invalid revision (`validate_input_revision`, `HTTP_NOT_FOUND`).
/// Format: RFC 9110 §15.5.5.
const HTTP_NOT_FOUND: u16 = 404;
/// The status of a report `update.c` refuses as malformed (`HTTP_BAD_REQUEST`).
/// Format: RFC 9110 §15.5.1.
const HTTP_BAD_REQUEST: u16 = 400;
/// The status of a refusal met recording the report's state (`HTTP_INTERNAL_SERVER_ERROR`).
/// Format: RFC 9110 §15.6.1.
const HTTP_INTERNAL_SERVER_ERROR: u16 = 500;
/// The drive's steps a resume takes before it hands back what it wrote, at the least: one call's step each, so a slice
/// takes as many calls as its room allows and no slice holds the shard past one call's bounded step for long.
/// Derived: a step of A-616's drive is one bounded thing (its module doc), and a slice is charged its room in bytes,
/// so a slice's steps are its room's bytes, each step writing at least one byte or deciding one entry.
const STEPS_PER_BYTE: usize = 1;

/// Why a slice of the walk stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
  /// The drive made its last call.
  Ended,
  /// The slice's room was written or its calls taken.
  Full,
  /// The writer refused a call.
  Refused,
}

/// The repository as an update-report's walk reads it, lent for one slice.
pub struct Walked<'a, R: ?Sized> {
  /// The index.
  pub index: &'a Index,
  /// The green.
  pub repository: &'a R,
  /// The locks.
  pub locks: &'a LockTable,
  /// The instant a lock's expiry is judged at.
  pub now: AprTime,
  /// The principal's read rules.
  pub rules: Option<&'a PrincipalRules>,
}

/// An update-report's walk still to write (the module doc).
#[derive(Debug)]
pub struct UpdateWalk {
  /// The drive making the editor's calls.
  drive: Drive,
  /// The writer of the answer.
  writer: Option<Writer>,
  /// Whether a call has been written, so a refusal can no longer be answered whole.
  begun: bool,
}

impl UpdateWalk {
  /// The walk answering `report` at the repository rooted at the URL path `root`, whose youngest revision is
  /// `youngest` and whose UUID is `uuid`, asked of HTTPv2's `me` resource when `v2`, its paths held within `room`;
  /// refused with `update.c`'s answer where its parameters or state are.
  ///
  /// # Errors
  /// The answer to send instead (the module doc).
  pub fn new(
    report: &UpdateReport,
    root: &str,
    youngest: Revision,
    uuid: &str,
    v2: bool,
    room: usize,
  ) -> Result<Self, HttpAnswer> {
    let target_revision = report.target_revision.unwrap_or(youngest);
    if target_revision > youngest {
      return Err(no_such_revision(target_revision));
    }
    let anchor =
      repository_path(root, &report.src_path).ok_or_else(|| illegal(&report.src_path))?;
    let switch_path = match &report.dst_path {
      Some(dst) => Some(repository_path(root, dst).ok_or_else(|| illegal(dst))?),
      None => None,
    };
    let command = WholeCommand {
      kind: if switch_path.is_some() {
        RequestKind::Switch
      } else {
        RequestKind::Update
      },
      anchor,
      operand: report.update_target.clone().unwrap_or_default(),
      switch_path,
      target_revision,
      depth: requested_depth(report),
      text_deltas: report.text_deltas && report.send_all,
      send_copyfrom_args: report.send_copyfrom_args,
      ignore_ancestry: report.ignore_ancestry,
    };
    let items = items(report, root, youngest)?;
    let whole = whole_report(command, items, room).map_err(|refused| match refused {
      WholeRefusal::Failed(failure) => error_answer(
        convert_status(failure.subversion_code(), HTTP_INTERNAL_SERVER_ERROR),
        true,
        failure.subversion_code(),
        &failure.message(),
      ),
      WholeRefusal::TooLarge => error_answer(
        HTTP_BAD_REQUEST,
        true,
        SVN_ERR_RA_DAV_REQUEST_FAILED,
        "The report's working-copy state is past the room it is read within",
      ),
    })?;
    let update = Update {
      send_all: report.send_all,
      inline_props: report.include_props,
      targeted: report
        .update_target
        .as_deref()
        .is_some_and(|target| !target.is_empty()),
      uris: Uris {
        root_path: root.to_owned(),
        special_uri: SPECIAL.to_owned(),
        v2,
      },
    };
    let writer = Writer::update(update, Version::Zero, room).map_err(|_| {
      error_answer(
        HTTP_INTERNAL_SERVER_ERROR,
        true,
        SVN_ERR_RA_ILLEGAL_URL,
        "unwritable root",
      )
    })?;
    Ok(Self {
      drive: Drive::new(whole, uuid.as_bytes()),
      writer: Some(writer),
      begun: false,
    })
  }

  /// Writes the answer's next slice into `out` from `walked`: the drive's calls until it has written `room` bytes or
  /// taken `room` calls and pauses, or the answer ends.
  pub fn resume<R: Repository + ?Sized>(
    &mut self,
    walked: &Walked<'_, R>,
    out: &mut Vec<u8>,
    room: usize,
  ) -> ReportProgress {
    let sources = Sources {
      index: walked.index,
      repository: walked.repository,
      locks: walked.locks,
      now: walked.now,
      rules: walked.rules,
    };
    let Some(mut writer) = self.writer.take() else {
      return ReportProgress::Done;
    };
    let mut written = Vec::new();
    let mut turns = 0usize;
    let stop = loop {
      match self.drive.next(&sources, room.max(1)) {
        Next::Call(call) => {
          if writer.call(call).is_err() {
            break Stop::Refused;
          }
        }
        Next::Paused => {}
        Next::Ended => break Stop::Ended,
      }
      writer.take_written(&mut written);
      turns = turns.saturating_add(STEPS_PER_BYTE);
      if written.len() >= room || turns >= room {
        break Stop::Full;
      }
    };
    let failure = self
      .drive
      .failure()
      .map(|failure| (failure.code, failure.message.clone()));
    match (stop, failure) {
      (Stop::Refused, failure) | (Stop::Ended, failure @ Some(_)) => {
        self.refused_or_broken(failure)
      }
      (Stop::Full, _) => {
        self.writer = Some(writer);
        self.begun = true;
        out.extend_from_slice(&written);
        ReportProgress::More
      }
      (Stop::Ended, None) => {
        writer.take_written(&mut written);
        out.extend_from_slice(&written);
        match writer.finish() {
          Ok(rest) => {
            out.extend_from_slice(&rest);
            ReportProgress::Done
          }
          Err(_) => self.refused_or_broken(None),
        }
      }
    }
  }

  /// The drive's refusal: answered whole before anything was sent, else the connection ends.
  fn refused_or_broken(&mut self, failure: Option<(u32, String)>) -> ReportProgress {
    self.writer = None;
    if self.begun {
      return ReportProgress::Broken;
    }
    let (code, message) = failure.unwrap_or((
      SVN_ERR_RA_DAV_REQUEST_FAILED,
      String::from("The update's drive could not be written"),
    ));
    ReportProgress::Refused(error_answer(
      convert_status(code, HTTP_INTERNAL_SERVER_ERROR),
      true,
      code,
      &message,
    ))
  }
}

/// The depth the report asks for (`update.c`: `S:depth`, else `S:recursive`, else infinity).
fn requested_depth(report: &UpdateReport) -> Depth {
  match (report.depth, report.recursive) {
    (Some(depth), _) => depth_of(depth),
    (None, Some(false)) => Depth::Files,
    (None, _) => Depth::Infinity,
  }
}

/// A DAV depth as the reporter's.
fn depth_of(depth: SvnDepth) -> Depth {
  match depth {
    SvnDepth::Exclude => Depth::Exclude,
    SvnDepth::Unknown => Depth::Unknown,
    SvnDepth::Empty => Depth::Empty,
    SvnDepth::Files => Depth::Files,
    SvnDepth::Immediates => Depth::Immediates,
    SvnDepth::Infinity => Depth::Infinity,
  }
}

/// The report's working-copy state as the reporter's items; an entry without its revision, or past the youngest, is
/// refused as `update.c` refuses it.
fn items(
  report: &UpdateReport,
  root: &str,
  youngest: Revision,
) -> Result<Vec<WholeItem>, HttpAnswer> {
  report
    .state
    .iter()
    .map(|reported| match reported {
      Reported::Missing { path } => Ok(WholeItem::Delete { path: path.clone() }),
      Reported::Entry {
        path,
        revision,
        depth,
        start_empty,
        linkpath,
        lock_token,
      } => {
        let revision = revision.ok_or_else(|| {
          error_answer(
            HTTP_INTERNAL_SERVER_ERROR,
            true,
            SVN_ERR_XML_ATTRIB_NOT_FOUND,
            "Missing XML attribute: rev",
          )
        })?;
        if revision > youngest {
          return Err(no_such_revision(revision));
        }
        let lock_token = lock_token.as_ref().map(|token| token.clone().into_bytes());
        Ok(match linkpath {
          Some(link) => WholeItem::Link {
            path: path.clone(),
            link_path: repository_path(root, link).ok_or_else(|| illegal(link))?,
            revision,
            depth: depth_of(*depth),
            start_empty: *start_empty,
            lock_token,
          },
          None => WholeItem::Set {
            path: path.clone(),
            revision,
            depth: depth_of(*depth),
            start_empty: *start_empty,
            lock_token,
          },
        })
      }
    })
    .collect()
}

/// The repository path `location` names beneath the repository rooted at the URL path `root`: a URL's path (its scheme
/// and authority dropped, as `dav_svn_split_uri` reads only the path) or a path as it is, decoded; none outside it.
fn repository_path(root: &str, location: &str) -> Option<String> {
  let path = match location.split_once("://") {
    Some((_, rest)) => rest.find('/').and_then(|at| rest.get(at..)).unwrap_or("/"),
    None => location,
  };
  let decoded = String::from_utf8(uri_decode(path)).ok()?;
  let root = root.trim_end_matches('/');
  let rest = decoded.strip_prefix(root)?;
  if !(rest.is_empty() || rest.starts_with('/')) {
    return None;
  }
  let names: Vec<&str> = rest.split('/').filter(|name| !name.is_empty()).collect();
  Some(format!("/{}", names.join("/")))
}

/// `validate_input_revision`'s refusal of `revision`.
fn no_such_revision(revision: Revision) -> HttpAnswer {
  error_answer(
    HTTP_NOT_FOUND,
    true,
    SVN_ERR_FS_NO_SUCH_REVISION,
    &format!("No such revision {revision}"),
  )
}

/// The refusal of a location outside the repository.
fn illegal(location: &str) -> HttpAnswer {
  error_answer(
    HTTP_BAD_REQUEST,
    true,
    SVN_ERR_RA_ILLEGAL_URL,
    &format!("'{location}' is not in this repository"),
  )
}
