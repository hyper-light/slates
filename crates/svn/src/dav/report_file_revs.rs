//! The `file-revs-report` `svn blame` sends over HTTP (§4.17.4 item 5 "HTTP", piece 13's reads; A-726): answered as
//! `mod_dav_svn`'s `reports/file-revs.c` answers it (Subversion 1.15.0), streamed a revision a step under the shard's
//! budget by `crate::reads::DavFileRevs` (`svn_repos_get_file_revs2`'s walk, the one the svn:// command runs, with
//! `file-revs.c`'s handler writing each revision through `super::listing::file_rev_element`).
//!
//! The request (`super::listing::file_revs_from`): `S:path` joined to the repository path of the resource the REPORT
//! names (ra_serf's blame names the file's stable URL at the end revision and an empty path), the start and the end
//! (each the youngest when missing or negative), `S:include-merged-revisions` by its presence.
//!
//! A walk refused before its first byte is answered as `dav_svn__file_revs_report` answers it,
//! `dav_svn__convert_err(serr, HTTP_INTERNAL_SERVER_ERROR, ...)`: Subversion's error mapped to its status (a missing
//! path `404`), the error in `D:error`. One refused after it, or a walk that aborts where `svn_repos_get_file_revs2`'s
//! assertion fails (an empty mainline: httpd's child ends), ends the connection.
//!
//! Departure, stated: deltas are written in svndiff 0 whatever the request's `Accept-Encoding` prefers, as this site's
//! update-report writes them; a client decodes any version, so blame's answer is the same.

use super::locking::{convert_status, error_answer};
use super::site::{ReportProgress, Served};
use crate::dav::listing::FileRevsReport;
use crate::reads::{DavFileRevs, DavLogProgress, Repository};

/// The status a walk refused before its first byte is answered with, where `dav_svn__convert_err` maps no other.
/// Format: `reports/file-revs.c` `dav_svn__file_revs_report`, `HTTP_INTERNAL_SERVER_ERROR`.
const FILE_REVS_REFUSED_STATUS: u16 = 500;

/// A `file-revs-report`'s answer still to write.
#[derive(Debug)]
pub(super) struct FileRevsWalk {
  /// The walk.
  revisions: DavFileRevs,
}

impl FileRevsWalk {
  /// The walk `report` asks for, its path beneath `target`, the repository path of the resource the REPORT names.
  pub(super) fn new(report: &FileRevsReport, target: &str) -> Self {
    Self {
      revisions: DavFileRevs::new(report, target),
    }
  }

  /// Writes the answer's next slice into `out` from `served`, until what it wrote and the steps it took reach `room`,
  /// at least one step, or the answer ends.
  pub(super) fn resume<R: Repository + ?Sized>(
    &mut self,
    served: &Served<'_, R>,
    out: &mut Vec<u8>,
    room: usize,
  ) -> ReportProgress {
    let progress = self
      .revisions
      .resume(served.index, served.repository, served.rules, out, room);
    match progress {
      DavLogProgress::More => ReportProgress::More,
      DavLogProgress::Done => ReportProgress::Done,
      DavLogProgress::Refused { .. } if self.revisions.begun() => ReportProgress::Broken,
      DavLogProgress::Refused { code, message } => {
        let status = convert_status(code, FILE_REVS_REFUSED_STATUS);
        ReportProgress::Refused(error_answer(status, true, code, &message))
      }
    }
  }
}

#[cfg(test)]
mod tests;
