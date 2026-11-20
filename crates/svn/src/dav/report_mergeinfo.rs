//! The `mergeinfo-report` stock `svn` sends over HTTP for a path's mergeinfo (`svn blame -g`, `svn merge`, `svn
//! mergeinfo`; §4.17.4 item 5 "HTTP"; A-726 part 2): answered as `mod_dav_svn`'s `reports/mergeinfo.c` answers it
//! (Subversion 1.15.0), from the svn:// command's own catalog (`crate::reads::repository_mergeinfo`,
//! `svn_repos_fs_get_mergeinfo2`).
//!
//! The request (`super::request::MergeinfoReport`): each `S:path` joined to the repository path of the resource the
//! REPORT names, `S:revision` (the youngest when missing or negative), `S:inherit`'s word (explicit for any word
//! Subversion does not know), and `S:include-descendants` `yes`. The answer, as `mergeinfo_receiver` writes it: the
//! header, then each path's `S:mergeinfo-item`, its path relative to the resource's (`svn_fspath__skip_ancestor`) and its
//! mergeinfo's string, then the report's end; with nothing found, the header and the end. A refused catalog is answered
//! as `dav_svn__convert_err(serr, HTTP_BAD_REQUEST, ...)` answers it: Subversion's error mapped to its status, the error
//! in `D:error`. The answer is written whole within the reply's room (`response::mergeinfo`), refused past it: a
//! catalog with descendants grows with the tree beneath its paths, and streaming it is owed (GAPS).

use super::locking::{DAV_XML_CONTENT_TYPE, HttpAnswer, convert_status, error_answer};
use super::request::{Inherit, MergeinfoReport};
use super::site::Served;
use crate::mergeinfo::MergeinfoInheritance;
use crate::reads::{Repository, full_path_of, repository_mergeinfo};

/// The status a refused catalog is answered with, where `dav_svn__convert_err` maps no other.
/// Format: `reports/mergeinfo.c`, `HTTP_BAD_REQUEST`.
const MERGEINFO_REFUSED_STATUS: u16 = 400;

/// The status of an answered report.
/// Format: RFC 9110 §15.3.1, 200 OK.
const HTTP_OK: u16 = 200;

/// The answer to `report` on the node whose repository path is `target`, read from `served`, written within `room`.
pub(super) fn answer<R: Repository + ?Sized>(
  served: &Served<'_, R>,
  report: &MergeinfoReport,
  target: &str,
  room: usize,
) -> HttpAnswer {
  let inheritance = match report.inherit {
    Inherit::Explicit => MergeinfoInheritance::Explicit,
    Inherit::Inherited => MergeinfoInheritance::Inherited,
    Inherit::NearestAncestor => MergeinfoInheritance::NearestAncestor,
  };
  let paths = report
    .paths
    .iter()
    .map(|path| full_path_of(target, path))
    .collect();
  let found = repository_mergeinfo(
    served.index,
    served.repository,
    served.rules,
    (target, paths),
    report.revision,
    (inheritance, report.include_descendants),
  );
  let refused = |code: u32, message: &str| {
    error_answer(
      convert_status(code, MERGEINFO_REFUSED_STATUS),
      true,
      code,
      message,
    )
  };
  match found {
    Ok(catalog) => match super::response::mergeinfo(&catalog, room) {
      Ok(body) => HttpAnswer {
        status: HTTP_OK,
        headers: vec![("Content-Type", String::from(DAV_XML_CONTENT_TYPE))],
        body,
      },
      // Past the reply's room: refused by its kind's status, bare, as the site refuses any body (`DavRefusal`).
      Err(past_room) => HttpAnswer {
        status: past_room.http_status(),
        headers: Vec::new(),
        body: Vec::new(),
      },
    },
    Err((code, message)) => refused(code, &message),
  }
}

#[cfg(test)]
mod tests {
  use super::super::Bounds;
  use super::super::site::{Answer, Request, Served, answer};
  use crate::lock::{AprTime, LockTable};
  use crate::report::drive::green::repository;

  /// Shape: bounds wide enough for every body these tests send.
  const BOUNDS: Bounds = Bounds {
    body_bytes: 1 << 16,
    depth: 16,
    name_bytes: 256,
    attributes: 32,
    elements: 1024,
  };
  /// Shape: the instant locks are judged at, after every fixture revision (2027-01-15).
  const NOW: AprTime = AprTime(1_800_000_000_000_000);
  /// Format: `mergeinfo.c`'s header (`send_mergeinfo_starting_sequence`) and end.
  const START: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:mergeinfo-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n";
  /// Format: the report's end.
  const END: &str = "</S:mergeinfo-report>\n";

  /// The status and body anonymous gets to a `mergeinfo-report` on `path` at `revision`, inherited, which must be whole.
  fn reported(path: &str, revision: &str) -> (u16, String) {
    let (index, green) = repository();
    let locks = LockTable::new();
    let served = Served {
      index: &index,
      repository: &green,
      root: "/repo",
      uuid: "6b4a5e2c-1d3f-4a7b-9c8e-0f1a2b3c4d5e",
      locks: &locks,
      rules: None,
      now: NOW,
    };
    let body = format!(
      "<S:mergeinfo-report xmlns:S=\"svn:\"><S:revision>{revision}</S:revision><S:inherit>inherited</S:inherit>\
<S:path></S:path></S:mergeinfo-report>"
    );
    let request = Request {
      method: "REPORT",
      path,
      depth: None,
      body: body.as_bytes(),
    };
    let Answer::Whole(answered) = answer(&served, &request, &BOUNDS, 1 << 20) else {
      panic!("a mergeinfo answer streamed");
    };
    (answered.status, String::from_utf8(answered.body).unwrap())
  }

  /// The answer `mergeinfo_receiver` writes for one item.
  fn item(path: &str, info: &str) -> String {
    format!(
      "<S:mergeinfo-item>\n<S:mergeinfo-path>{path}</S:mergeinfo-path>\n<S:mergeinfo-info>{info}</S:mergeinfo-info>\n</S:mergeinfo-item>"
    )
  }

  /// A-726 part 2 (`reports/mergeinfo.c`): do REPORT the inherited mergeinfo of the trunk, which revision 6 set to
  /// `/branches/b1:3-5`, at 6; of a file beneath it at 6; of the trunk at 5, before it was set; and at a revision past the
  /// youngest. Expect: the trunk's own, its path relative to the resource (empty); the file's inherited, its source
  /// extended by the file's name; at 5 the header and the end alone; and `404`, "No such revision".
  #[test]
  fn a_mergeinfo_report_answers_each_paths_mergeinfo_as_svnserve_reads_it() {
    let (refused, words) = reported("/trunk", "9");
    assert_eq!(
      (
        reported("/trunk", "6"),
        reported("/trunk/a.txt", "6"),
        reported("/trunk", "5"),
        refused,
        words.contains("No such revision 9"),
      ),
      (
        (200, format!("{START}{}{END}", item("", "/branches/b1:3-5"))),
        (
          200,
          format!("{START}{}{END}", item("", "/branches/b1/a.txt:3-5"))
        ),
        (200, format!("{START}{END}")),
        404,
        true,
      ),
      "{words}"
    );
  }
}
