//! A-726: the `file-revs-report` answered from the report fixtures' repository (`report::drive::green`, six revisions),
//! its deltas replayed and held to the texts the site's own GET serves at each revision.

use super::super::Bounds;
use super::super::site::{Answer, Budget, ReportProgress, Request, Served, answer};
use super::super::xml::{self, Element};
use crate::index::Index;
use crate::lock::{AprTime, LockTable};
use crate::reads::Repository;
use crate::report::drive::green::{Green, repository};
use crate::svndiff::{apply_all, decode_all};

/// Shape: the repository root's URL path the tests' URLs name.
const ROOT: &str = "/repo";
/// Shape: the UUID the tests' repository carries.
const UUID: &str = "6b4a5e2c-1d3f-4a7b-9c8e-0f1a2b3c4d5e";
/// Shape: room enough for every answer these tests ask for.
const ROOM: usize = 1 << 20;
/// Shape: bounds wide enough for every answer these tests read.
const BOUNDS: Bounds = Bounds {
  body_bytes: 1 << 20,
  depth: 16,
  name_bytes: 256,
  attributes: 32,
  elements: 4096,
};
/// Shape: the instant locks are judged at, after every fixture revision (2027-01-15).
const NOW: AprTime = AprTime(1_800_000_000_000_000);
/// Shape: the `file-revs-report` `svn blame` 1.15.0 sent (A-627's fixture): revisions 0 to 5, an empty path beneath
/// the file's stable URL.
const BLAME: &[u8] = include_bytes!("../fixtures/file-revs-report.xml");

/// The fixtures' repository and its empty lock table.
struct Fixture {
  index: Index,
  green: Green,
  locks: LockTable,
}

impl Fixture {
  fn new() -> Self {
    let (index, green) = repository();
    Self {
      index,
      green,
      locks: LockTable::new(),
    }
  }

  fn served(&self) -> Served<'_, Green> {
    Served {
      index: &self.index,
      repository: &self.green,
      root: ROOT,
      uuid: UUID,
      locks: &self.locks,
      rules: None,
      now: NOW,
    }
  }

  /// The answer to `method` on `path` below the root.
  fn ask(&self, method: &str, path: &str, body: &[u8]) -> Answer {
    let request = Request {
      method,
      path,
      depth: None,
      body,
    };
    answer(&self.served(), &request, &BOUNDS, ROOM)
  }

  /// A REPORT's answer streamed a slice of `room` at a time: what it wrote and how it ended.
  fn reported(&self, path: &str, body: &[u8], room: usize) -> (Vec<u8>, ReportProgress) {
    let Answer::Report(mut report) = self.ask("REPORT", path, body) else {
      panic!("not a report");
    };
    let mut out = Vec::new();
    loop {
      match report.resume(&self.served(), &mut out, untimed(room)) {
        ReportProgress::More => {}
        ended => return (out, ended),
      }
    }
  }

  /// A file's text as the site's GET serves it.
  fn got(&self, path: &str) -> Vec<u8> {
    let Answer::Text { text, .. } = self.ask("GET", path, b"") else {
      panic!("no text at {path}");
    };
    let mut bytes = Vec::new();
    if let Some(text) = text {
      while let Some(piece) = self
        .green
        .text(&text.identity, u64::try_from(bytes.len()).unwrap())
        .filter(|piece| !piece.is_empty())
      {
        bytes.extend_from_slice(&piece);
      }
    }
    bytes
  }
}

/// `text` decoded from base64 (RFC 4648 §4), for reading a delta back.
fn unbase64(text: &str) -> Vec<u8> {
  let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut bits = 0u32;
  let mut held = 0u32;
  let mut out = Vec::new();
  for character in text.bytes().filter(|character| *character != b'=') {
    let value = alphabet
      .iter()
      .position(|known| *known == character)
      .unwrap();
    bits = (bits << 6) | u32::try_from(value).unwrap();
    held += 6;
    if held >= 8 {
      held -= 8;
      out.push(u8::try_from((bits >> held) & 0xff).unwrap());
    }
  }
  out
}

/// Each `S:file-rev`'s path, revision and text, its delta applied to the text before it (the client's blame reads
/// them so: `libsvn_ra_serf/blame.c`).
fn replayed(body: &[u8]) -> Vec<(String, String, Vec<u8>)> {
  let report = xml::parse(body, &BOUNDS).unwrap();
  assert!(report.is("svn:", "file-revs-report"));
  let mut text = Vec::new();
  report
    .elements()
    .map(|file_rev: &Element| {
      assert_eq!(file_rev.name, "file-rev");
      if let Some(delta) = file_rev.elements().find(|child| child.name == "txdelta") {
        let (_, windows) = decode_all(&unbase64(&delta.text())).unwrap();
        text = apply_all(&text, &windows).unwrap();
      }
      (
        file_rev.attribute("path").unwrap().to_owned(),
        file_rev.attribute("rev").unwrap().to_owned(),
        text.clone(),
      )
    })
    .collect()
}

/// A-726 (`reports/file-revs.c`): do REPORT `svn blame`'s body on a file's stable URL at revision 5, streamed a slice
/// of the smallest room at a time and again within the whole room. Expect: the two streams the same bytes, between
/// `maybe_send_header`'s start and the report's end; revisions in increasing order, the last 5; and each revision's
/// text, its delta replayed over the one before, the text the site's GET serves at that revision's own URL.
#[test]
fn a_file_revs_report_streams_each_revision_whose_deltas_replay_to_its_text() {
  let fixture = Fixture::new();
  let at = "/!svn/rvr/5/trunk/a.txt";
  let (sliced, ended) = fixture.reported(at, BLAME, 1);
  let (whole, whole_ended) = fixture.reported(at, BLAME, ROOM);
  let text = String::from_utf8(whole.clone()).unwrap();
  assert_eq!(
    (
      ended,
      whole_ended,
      sliced == whole,
      text.starts_with(super::super::listing::FILE_REVS_START),
      text.ends_with(super::super::listing::FILE_REVS_END),
    ),
    (ReportProgress::Done, ReportProgress::Done, true, true, true)
  );
  let revisions = replayed(&whole);
  let numbers: Vec<u64> = revisions
    .iter()
    .map(|(_, revision, _)| revision.parse().unwrap())
    .collect();
  assert!(
    numbers.len() > 1
      && numbers.windows(2).all(|pair| pair[0] < pair[1])
      && numbers.last() == Some(&5),
    "revisions {numbers:?}"
  );
  for (path, revision, replayed_text) in &revisions {
    assert_eq!(
      replayed_text,
      &fixture.got(&format!("/!svn/rvr/{revision}{path}")),
      "{path}@{revision}"
    );
  }
}

/// A-726 (`dav_svn__file_revs_report`'s errors, `dav_svn__convert_err`): do REPORT `svn blame`'s body on a missing
/// file's URL and on a directory's. Expect: the first `404`, no resource; the second refused before its first byte,
/// nothing written, Subversion's "is not a file" in `D:error`.
#[test]
fn a_file_revs_report_on_a_directory_is_refused_before_its_first_byte() {
  let fixture = Fixture::new();
  let Answer::Whole(missing) = fixture.ask("REPORT", "/trunk/missing.txt", BLAME) else {
    panic!("a missing file's report streamed");
  };
  let (written, ended) = fixture.reported("/!svn/rvr/5/trunk", BLAME, ROOM);
  let ReportProgress::Refused(refused) = ended else {
    panic!("not refused before its first byte: {ended:?}");
  };
  let words = String::from_utf8(refused.body.clone()).unwrap();
  assert_eq!(
    (
      missing.status,
      written.len(),
      words.contains("is not a file in revision 5")
    ),
    (404, 0, true),
    "{words}"
  );
}

/// A budget of `room` bytes and as many steps, untimed: a slice as the site's callers gave one before A-745's quantum.
fn untimed(room: usize) -> Budget {
  Budget {
    room,
    steps: room,
    quantum: None,
  }
}
