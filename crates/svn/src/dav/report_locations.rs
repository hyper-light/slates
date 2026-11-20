//! The `get-locations` report stock `svn` sends over HTTP to resolve a URL's peg to an operative revision (`svn blame
//! -r N URL`, `svn cat -r N URL`; §4.17.4 item 5 "HTTP"; A-726): answered as `mod_dav_svn`'s `reports/get-locations.c`
//! answers it (Subversion 1.15.0), from the svn:// command's own trace (`crate::reads::trace_node_locations`,
//! `svn_repos_trace_node_locations`).
//!
//! The request (`super::request::GetLocationsReport`): `S:path` joined to the repository path of the resource the
//! REPORT names, `S:peg-revision`, and each `S:location-revision`. The answer, whole, since it holds one line for each
//! revision the request names: `get-locations.c`'s header, an `S:location` for each location found, its path quoted,
//! and the report's end. A trace refused is answered as `dav_svn__convert_err(serr, HTTP_INTERNAL_SERVER_ERROR, ...)`
//! answers it: Subversion's error mapped to its status, the error in `D:error`.
//!
//! Departure, stated: the locations are written in the trace's order, youngest first, where `mod_dav_svn` walks an APR
//! hash, whose order its seed varies; a client reads them into a hash of its own.

use super::locking::{DAV_XML_CONTENT_TYPE, HttpAnswer, convert_status, error_answer};
use super::request::GetLocationsReport;
use super::site::Served;
use super::xml::quote;
use crate::reads::{Repository, full_path_of, trace_node_locations};

/// The status a refused trace is answered with, where `dav_svn__convert_err` maps no other.
/// Format: `reports/get-locations.c`, `HTTP_INTERNAL_SERVER_ERROR`.
const LOCATIONS_REFUSED_STATUS: u16 = 500;

/// The status of an answered report.
/// Format: RFC 9110 §15.3.1, 200 OK.
const HTTP_OK: u16 = 200;

/// The start of the answer.
/// Format: `get-locations.c` `send_get_locations_report`: `DAV_XML_HEADER`, `DEBUG_CR`, the report's element.
const LOCATIONS_START: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:get-locations-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n";

/// The end of the answer.
/// Format: `send_get_locations_report`'s last write.
const LOCATIONS_END: &str = "</S:get-locations-report>\n";

/// The answer to `report` on the node whose repository path is `target`, read from `served`.
pub(super) fn answer<R: Repository + ?Sized>(
  served: &Served<'_, R>,
  report: &GetLocationsReport,
  target: &str,
) -> HttpAnswer {
  let path = full_path_of(target, &report.path);
  let traced = trace_node_locations(
    served.index,
    served.repository,
    served.rules,
    (&path, report.peg_revision),
    &report.location_revisions,
  );
  match traced {
    Ok(locations) => {
      let mut body = String::from(LOCATIONS_START);
      for (revision, place) in locations {
        body.push_str(&format!(
          "<S:location rev=\"{revision}\" path=\"{}\"/>\n",
          quote(&place, true)
        ));
      }
      body.push_str(LOCATIONS_END);
      HttpAnswer {
        status: HTTP_OK,
        headers: vec![("Content-Type", String::from(DAV_XML_CONTENT_TYPE))],
        body: body.into_bytes(),
      }
    }
    Err((code, message)) => {
      let status = convert_status(code, LOCATIONS_REFUSED_STATUS);
      error_answer(status, true, code, &message)
    }
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

  /// The answer anonymous gets to a REPORT with `body` on `path` below the root, which must be whole.
  fn reported(path: &str, body: &[u8]) -> (u16, String) {
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
    let request = Request {
      method: "REPORT",
      path,
      depth: None,
      body,
    };
    let Answer::Whole(answered) = answer(&served, &request, &BOUNDS, 1 << 20) else {
      panic!("a get-locations answer streamed");
    };
    (answered.status, String::from_utf8(answered.body).unwrap())
  }

  /// A-726 (`reports/get-locations.c`): do REPORT `get-locations` on the file revision 4 copied to `/trunk/renamed.txt`
  /// from `/trunk/new.txt`, its peg 5, asking where it stood in revisions 5 and 3, then a peg past the youngest. Expect:
  /// `200`, the header, revision 5 at its own path and revision 3 at the copy's source, the report's end; and the second
  /// refused, `404`, "No such revision".
  #[test]
  fn get_locations_traces_a_file_back_across_its_copy() {
    let body = b"<S:get-locations xmlns:S=\"svn:\" xmlns:D=\"DAV:\"><S:path></S:path>\
<S:peg-revision>5</S:peg-revision><S:location-revision>5</S:location-revision>\
<S:location-revision>3</S:location-revision></S:get-locations>";
    let past = b"<S:get-locations xmlns:S=\"svn:\" xmlns:D=\"DAV:\"><S:path></S:path>\
<S:peg-revision>9</S:peg-revision><S:location-revision>3</S:location-revision></S:get-locations>";
    let expected = format!(
      "{}<S:location rev=\"5\" path=\"/trunk/renamed.txt\"/>\n<S:location rev=\"3\" path=\"/trunk/new.txt\"/>\n{}",
      super::LOCATIONS_START,
      super::LOCATIONS_END
    );
    let (refused, words) = reported("/trunk/renamed.txt", past);
    assert_eq!(
      (
        reported("/trunk/renamed.txt", body),
        refused,
        words.contains("No such revision 9")
      ),
      ((200, expected), 404, true),
      "{words}"
    );
  }
}
