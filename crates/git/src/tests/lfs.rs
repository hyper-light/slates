//! A-610: what the LFS codecs share (object ids, timestamps, error bodies, routes), and the fixtures every LFS test
//! reads: git-lfs v3.8.1's documents, verbatim (`fixtures/lfs/fetch-git-lfs-docs.sh`), and the stock git-lfs 3.8.1
//! client's exchange with a recorded responder on the Mac Studio, 2026-11-19 (`fixtures/lfs/make-lfs-exchange.sh`).
//! Compiled in, never read at run time.

use proptest::prelude::*;
use serde_json::Value;

use super::{ErrorBody, Oid, Route, Timestamp, route, write_error};

/// git-lfs v3.8.1's `docs/api/batch.md`.
pub(crate) const BATCH_MD: &str = include_str!("../../fixtures/lfs/git-lfs-v3.8.1/batch.md");
/// Its `docs/api/basic-transfers.md`.
pub(crate) const BASIC_TRANSFERS_MD: &str =
  include_str!("../../fixtures/lfs/git-lfs-v3.8.1/basic-transfers.md");
/// Its `docs/api/locking.md`.
pub(crate) const LOCKING_MD: &str = include_str!("../../fixtures/lfs/git-lfs-v3.8.1/locking.md");
/// Its `docs/spec.md`.
pub(crate) const SPEC_MD: &str = include_str!("../../fixtures/lfs/git-lfs-v3.8.1/spec.md");
/// Every request the stock client sent the responder and every answer it read, one JSON object a line.
pub(crate) const EXCHANGE: &str = include_str!("../../fixtures/lfs/exchange/exchange.jsonl");
/// The client's words when the responder refused a second lock on one path with 409.
pub(crate) const CONFLICT: &str = include_str!("../../fixtures/lfs/exchange/conflict.txt");

/// An oid for examples whose own is illustrative ("12345678", "1111111"): SHA-256's of "hello world\n".
pub(crate) const EXAMPLE_OID: &str =
  "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447";

/// The JSON examples of a git-lfs document: each fenced `json` or `json5` block, its comment lines dropped and a
/// comment after a value cut, so `// POST ...` and `"limit": 100, // also optional` read as JSON.
pub(crate) fn json_examples(markdown: &str) -> Vec<String> {
  let mut examples = Vec::new();
  let mut current: Option<String> = None;
  for line in markdown.lines() {
    let trimmed = line.trim();
    match current.as_mut() {
      None if trimmed == "```json" || trimmed == "```json5" => current = Some(String::new()),
      None => {}
      Some(_) if trimmed == "```" => examples.push(current.take().unwrap()),
      Some(text) => {
        if trimmed.starts_with("//") {
          continue;
        }
        let code = line.split(" //").next().unwrap();
        text.push_str(code);
        text.push('\n');
      }
    }
  }
  examples
}

/// One exchange of the recording: the request's method, path, query and body, and the answer's status and body.
pub(crate) struct Exchange {
  /// `GET`, `PUT` or `POST`.
  pub(crate) method: String,
  /// The path, without the query.
  pub(crate) path: String,
  /// The query, without the `?`.
  pub(crate) query: String,
  /// The request's body.
  pub(crate) request: String,
  /// The answer's status.
  pub(crate) status: u64,
  /// The answer's body.
  pub(crate) response: String,
}

/// Every exchange of the recording, in order.
pub(crate) fn exchanges() -> Vec<Exchange> {
  EXCHANGE
    .lines()
    .map(|line| {
      let entry: Value = serde_json::from_str(line).unwrap();
      let text = |side: &str| entry[side]["body"]["text"].as_str().unwrap().to_string();
      Exchange {
        method: entry["request"]["method"].as_str().unwrap().to_string(),
        path: entry["request"]["path"].as_str().unwrap().to_string(),
        query: entry["request"]["query"].as_str().unwrap().to_string(),
        request: text("request"),
        status: entry["response"]["status"].as_u64().unwrap(),
        response: text("response"),
      }
    })
    .collect()
}

/// Format: where the responder served the LFS API, the prefix every API path of the recording has.
pub(crate) const LFS_PREFIX: &str = "/repo.git/info/lfs";

/// A-610: do read an oid from 64 lower-case hexadecimal digits and write it back. Expect the same digits, and expect
/// upper case, 63 or 65 digits and a non-digit each refused.
#[test]
fn an_oid_is_64_lower_case_hexadecimal_digits_both_ways() {
  let oid = Oid::from_hex(EXAMPLE_OID.as_bytes()).unwrap();
  assert_eq!(oid.to_string(), EXAMPLE_OID);
  let mut written = Vec::new();
  oid.put_hex(&mut written);
  assert_eq!(written, EXAMPLE_OID.as_bytes());
  assert_eq!(Oid::from_hex(EXAMPLE_OID.to_uppercase().as_bytes()), None);
  assert_eq!(Oid::from_hex(&EXAMPLE_OID.as_bytes()[..63]), None);
  assert_eq!(Oid::from_hex(format!("{EXAMPLE_OID}0").as_bytes()), None);
  assert_eq!(
    Oid::from_hex(format!("{}g", &EXAMPLE_OID[..63]).as_bytes()),
    None
  );
}

/// The text RFC 3339 writes for `seconds`, by a calendar walked a year and a month at a time: the oracle for
/// [`Timestamp::put_rfc3339`].
fn naive_rfc3339(seconds: i64) -> String {
  let leap = |year: i64| (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
  let mut days = seconds.div_euclid(86_400);
  let of_day = seconds.rem_euclid(86_400);
  let mut year = 1970;
  while days < 0 {
    year -= 1;
    days += if leap(year) { 366 } else { 365 };
  }
  while days >= if leap(year) { 366 } else { 365 } {
    days -= if leap(year) { 366 } else { 365 };
    year += 1;
  }
  let lengths = [
    31,
    if leap(year) { 29 } else { 28 },
    31,
    30,
    31,
    30,
    31,
    31,
    30,
    31,
    30,
    31,
  ];
  let mut month = 0;
  while days >= lengths[month] {
    days -= lengths[month];
    month += 1;
  }
  format!(
    "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
    month + 1,
    days + 1,
    of_day / 3_600,
    of_day % 3_600 / 60,
    of_day % 60
  )
}

/// The text a timestamp writes.
fn rfc3339(timestamp: Timestamp) -> String {
  let mut out = Vec::new();
  timestamp.put_rfc3339(&mut out);
  String::from_utf8(out).unwrap()
}

/// A-610: do write instants named by Python's `calendar.timegm` (2026-11-19): the epoch, git-lfs's documented examples,
/// the recording's lock time and expiry, leap days in 2000 and 1600, the first day of 1900, and the first and last
/// seconds of the four-digit years. Expect RFC 3339's text for each, and the seconds just outside those years refused.
#[test]
fn a_timestamp_is_written_as_rfc_3339_to_the_second() {
  let cases = [
    (0, "1970-01-01T00:00:00Z"),
    (1_463_500_146, "2016-05-17T15:49:06Z"),
    (1_478_791_747, "2016-11-10T15:29:07Z"),
    (1_795_046_400, "2026-11-19T00:00:00Z"),
    (4_070_908_800, "2099-01-01T00:00:00Z"),
    (951_782_400, "2000-02-29T00:00:00Z"),
    (-11_670_953_104, "1600-02-29T12:34:56Z"),
    (-2_203_891_200, "1900-03-01T00:00:00Z"),
    (-62_135_596_800, "0001-01-01T00:00:00Z"),
    (-62_167_219_200, "0000-01-01T00:00:00Z"),
    (253_402_300_799, "9999-12-31T23:59:59Z"),
  ];
  for (seconds, text) in cases {
    assert_eq!(
      rfc3339(Timestamp::from_unix(seconds).unwrap()),
      text,
      "{seconds}"
    );
  }
  assert_eq!(Timestamp::from_unix(-62_167_219_201), None);
  assert_eq!(Timestamp::from_unix(253_402_300_800), None);
}

/// A-610: do route each path the LFS API names, and paths it does not. Expect the batch, the locks, the verification
/// and an unlock by its id, and nothing for an empty id, an id with a slash, or a path beside them.
#[test]
fn each_endpoint_is_routed_by_its_path() {
  assert_eq!(route(b"/objects/batch"), Some(Route::Batch));
  assert_eq!(route(b"/locks"), Some(Route::Locks));
  assert_eq!(route(b"/locks/verify"), Some(Route::LocksVerify));
  assert_eq!(
    route(b"/locks/lock-1/unlock"),
    Some(Route::Unlock { id: b"lock-1" })
  );
  for other in [
    &b""[..],
    b"/",
    b"/objects",
    b"/objects/batch/",
    b"/locks/",
    b"/locks//unlock",
    b"/locks/a/b/unlock",
  ] {
    assert_eq!(route(other), None, "{}", String::from_utf8_lossy(other));
  }
}

/// A-610: do route every path of the recording under the LFS API. Expect each to name its endpoint by its method: the
/// batch and verification by POST, the locks by POST and GET, each unlock by its lock's id.
#[test]
fn every_api_path_the_stock_client_used_is_routed() {
  let mut routed = 0;
  for exchange in exchanges() {
    let Some(rest) = exchange.path.strip_prefix(LFS_PREFIX) else {
      continue;
    };
    let expected = match (exchange.method.as_str(), rest) {
      ("POST", "/objects/batch") => Route::Batch,
      ("POST" | "GET", "/locks") => Route::Locks,
      ("POST", "/locks/verify") => Route::LocksVerify,
      ("POST", "/locks/lock-1/unlock") => Route::Unlock { id: b"lock-1" },
      ("POST", "/locks/lock-2/unlock") => Route::Unlock { id: b"lock-2" },
      other => panic!("unexpected {other:?}"),
    };
    assert_eq!(route(rest.as_bytes()), Some(expected));
    routed += 1;
  }
  assert_eq!(routed, 18);
}

/// A-610: do write batch.md's two error bodies (404 and 401) from their fields. Expect each, read back as JSON, equal
/// to the document's.
#[test]
fn an_error_body_is_written_as_the_document_shows() {
  let examples = json_examples(BATCH_MD);
  let errors: Vec<&String> = examples
    .iter()
    .filter(|text| text.contains("\"message\"") && !text.contains("\"objects\""))
    .collect();
  assert_eq!(errors.len(), 2);
  for (example, message) in errors.iter().zip(["Not found", "Credentials needed"]) {
    let mut out = Vec::new();
    write_error(
      &ErrorBody {
        message,
        request_id: Some("123"),
        documentation_url: Some("https://lfs-server.com/docs/errors"),
      },
      &mut out,
    );
    let ours: Value = serde_json::from_slice(&out).unwrap();
    let theirs: Value = serde_json::from_str(example).unwrap();
    assert_eq!(ours, theirs);
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-610, oracle: do write any instant of the four-digit years. Expect the text a calendar walked year by year
  /// writes for it.
  #[test]
  fn any_timestamp_is_written_as_the_calendar_walks_it(seconds in -62_167_219_200i64..=253_402_300_799) {
    prop_assert_eq!(rfc3339(Timestamp::from_unix(seconds).unwrap()), naive_rfc3339(seconds));
  }
}
