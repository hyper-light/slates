//! The HTTP bodies' tests (A-609; §4.17.4 piece 13): every request body stock svn 1.15.0 sent (recorded on the Mac
//! Studio, 2026-11-19, by the commands each fixture names, against a recorded responder) decoded as `mod_dav_svn` reads
//! it; `get-deleted-rev-report`'s body as ra_serf's own writer writes it (`get_deleted_rev.c`), since no command of the
//! recording sent one; every response written byte for byte as `mod_dav_svn`'s format strings write it; and the hostile
//! inputs: a DTD and a billion laughs, unknown entities, every cut of every captured body, names, depths, counts and
//! bodies past their bounds, foreign encodings and bytes that are not UTF-8.

use super::request::{self, *};
use super::response::{self, *};
use super::xml::{self, XmlRefusal};
use super::*;
use crate::lock::{AprTime, Lock, LockToken};

/// Bounds no captured body comes near, for the tests that are not about them.
const ROOMY: Bounds = Bounds {
  body_bytes: 1 << 20,
  depth: 64,
  name_bytes: 256,
  attributes: 32,
  elements: 10_000,
};

/// Room no response here comes near.
const ROOM: usize = 1 << 20;

/// Every captured request body, by the fixture's name: the command that sent it is in the module documentation.
const CAPTURED: [(&str, &[u8]); 23] = [
  ("options", include_bytes!("fixtures/options.xml")),
  (
    "options-empty",
    include_bytes!("fixtures/options-empty.xml"),
  ),
  (
    "propfind-props",
    include_bytes!("fixtures/propfind-props.xml"),
  ),
  (
    "propfind-lockdiscovery",
    include_bytes!("fixtures/propfind-lockdiscovery.xml"),
  ),
  (
    "propfind-allprop",
    include_bytes!("fixtures/propfind-allprop.xml"),
  ),
  (
    "propfind-resourcetype",
    include_bytes!("fixtures/propfind-resourcetype.xml"),
  ),
  (
    "log-report-verbose",
    include_bytes!("fixtures/log-report-verbose.xml"),
  ),
  (
    "log-report-peg",
    include_bytes!("fixtures/log-report-peg.xml"),
  ),
  (
    "log-report-merged",
    include_bytes!("fixtures/log-report-merged.xml"),
  ),
  (
    "dated-rev-report",
    include_bytes!("fixtures/dated-rev-report.xml"),
  ),
  (
    "get-locations",
    include_bytes!("fixtures/get-locations.xml"),
  ),
  (
    "get-location-segments",
    include_bytes!("fixtures/get-location-segments.xml"),
  ),
  (
    "get-locks-report-immediates",
    include_bytes!("fixtures/get-locks-report-immediates.xml"),
  ),
  (
    "get-locks-report-infinity",
    include_bytes!("fixtures/get-locks-report-infinity.xml"),
  ),
  (
    "mergeinfo-report",
    include_bytes!("fixtures/mergeinfo-report.xml"),
  ),
  (
    "inherited-props-report",
    include_bytes!("fixtures/inherited-props-report.xml"),
  ),
  (
    "replay-report",
    include_bytes!("fixtures/replay-report.xml"),
  ),
  (
    "update-report-checkout",
    include_bytes!("fixtures/update-report-checkout.xml"),
  ),
  (
    "update-report-dump",
    include_bytes!("fixtures/update-report-dump.xml"),
  ),
  (
    "update-report-update",
    include_bytes!("fixtures/update-report-update.xml"),
  ),
  (
    "update-report-status",
    include_bytes!("fixtures/update-report-status.xml"),
  ),
  ("merge", include_bytes!("fixtures/merge.xml")),
  (
    "get-deleted-rev-report",
    include_bytes!("fixtures/get-deleted-rev-report.xml"),
  ),
];

/// The captured body named `name`.
fn captured(name: &str) -> &'static [u8] {
  CAPTURED
    .iter()
    .find(|(fixture, _)| *fixture == name)
    .map(|(_, body)| *body)
    .unwrap()
}

/// A captured report, decoded.
fn report_of(name: &str) -> Report {
  request::report(captured(name), &ROOMY).unwrap()
}

/// What a captured body decodes to, whatever its method, as one comparable value.
fn decoded(name: &str, body: &[u8]) -> Result<String, DavRefusal> {
  let decoded = if name.starts_with("options") {
    format!("{:?}", request::options(body, &ROOMY)?)
  } else if name.starts_with("propfind") {
    format!("{:?}", request::propfind(body, &ROOMY)?)
  } else if name == "merge" {
    format!("{:?}", request::merge(body, &ROOMY)?)
  } else {
    format!("{:?}", request::report(body, &ROOMY)?)
  };
  Ok(decoded)
}

/// The three revision properties svn 1.15 asks a log for.
fn standard_revprops() -> Revprops {
  Revprops::Named(vec![
    "svn:author".to_owned(),
    "svn:date".to_owned(),
    "svn:log".to_owned(),
  ])
}

/// A-609 (`version.c` `get_option`; `mod_dav.c` `dav_method_options`): do decode the OPTIONS bodies svn 1.15 sent (the
/// first of every session, and the chunked probe's `<D:options/>`) and no body; expect the first to ask for the
/// activity collection and the others for nothing.
#[test]
fn the_captured_options_bodies_ask_for_the_activity_collection_or_nothing() {
  let asked = [
    request::options(captured("options"), &ROOMY).unwrap(),
    request::options(captured("options-empty"), &ROOMY).unwrap(),
    request::options(b"", &ROOMY).unwrap(),
  ];
  let wants: Vec<bool> = asked
    .iter()
    .map(|asked| asked.activity_collection_set)
    .collect();
  assert_eq!(wants, vec![true, false, false]);
}

/// A-609 (`mod_dav.c` `dav_method_propfind`): do decode the PROPFIND bodies svn 1.15 sent (`svn info`'s six
/// properties, `svn info`'s lock discovery, `svn cat`'s allprop, the kind check's resourcetype); expect each property
/// in its namespace, `mod_dav_svn`'s own among them.
#[test]
fn the_captured_propfind_bodies_name_their_properties() {
  let named = |namespace: &str, name: &str| PropertyName {
    namespace: namespace.to_owned(),
    name: name.to_owned(),
  };
  let props = vec![
    named(DAV, "resourcetype"),
    named(DAV, "getcontentlength"),
    named(SVN_DAV, "deadprop-count"),
    named(DAV, "version-name"),
    named(DAV, "creationdate"),
    named(DAV, "creator-displayname"),
  ];
  let found = [
    request::propfind(captured("propfind-props"), &ROOMY).unwrap(),
    request::propfind(captured("propfind-lockdiscovery"), &ROOMY).unwrap(),
    request::propfind(captured("propfind-allprop"), &ROOMY).unwrap(),
    request::propfind(captured("propfind-resourcetype"), &ROOMY).unwrap(),
  ];
  let expected = [
    PropfindRequest::Prop(props),
    PropfindRequest::Prop(vec![named(DAV, "lockdiscovery")]),
    PropfindRequest::AllProp,
    PropfindRequest::Prop(vec![named(DAV, "resourcetype")]),
  ];
  assert_eq!(found, expected);
}

/// A-609 (`reports/log.c`): do decode the log-reports `svn log -v --limit 2 -r 5:1`, `svn log -r 2:1 URL@5` and `svn
/// log -g -l 1` sent; expect their revisions, limits, flags, the three standard revision properties and the target's
/// own path.
#[test]
fn the_captured_log_reports_decode_as_log_c_reads_them() {
  let log = |start, end, limit| LogReport {
    start: Some(start),
    end: Some(end),
    limit,
    discover_changed_paths: false,
    strict_node_history: false,
    include_merged_revisions: false,
    encode_binary_props: true,
    revprops: standard_revprops(),
    paths: vec![String::new()],
  };
  let verbose = LogReport {
    discover_changed_paths: true,
    ..log(5, 1, 2)
  };
  let merged = LogReport {
    include_merged_revisions: true,
    ..log(5, 0, 1)
  };
  let found = [
    report_of("log-report-verbose"),
    report_of("log-report-peg"),
    report_of("log-report-merged"),
  ];
  assert_eq!(
    found,
    [
      Report::Log(verbose),
      Report::Log(log(2, 1, 0)),
      Report::Log(merged)
    ]
  );
}

/// A-609 (`reports/update.c`): do decode the update-reports `svn checkout`, `svnrdump dump -r 1:1`, `svn update -r 4`
/// and `svn status -u` sent; expect send-all asked, the anchor's path, the target revision when given, the depth word
/// (`unknown` from a working copy), `text-deltas` off for status, and one entry each, start-empty for a checkout.
#[test]
fn the_captured_update_reports_decode_as_update_c_reads_them() {
  let entry = |revision, start_empty| Reported::Entry {
    path: String::new(),
    revision: Some(revision),
    depth: SvnDepth::Infinity,
    start_empty,
    linkpath: None,
    lock_token: None,
  };
  let update = |src: &str, target, depth, state| UpdateReport {
    send_all: true,
    src_path: src.to_owned(),
    target_revision: target,
    depth: Some(depth),
    text_deltas: true,
    state: vec![state],
    ..UpdateReport::default()
  };
  let status = UpdateReport {
    text_deltas: false,
    ..update("/repo/trunk", None, SvnDepth::Unknown, entry(5, false))
  };
  let found = [
    report_of("update-report-checkout"),
    report_of("update-report-dump"),
    report_of("update-report-update"),
    report_of("update-report-status"),
  ];
  let expected = [
    Report::Update(update(
      "/repo/trunk",
      Some(5),
      SvnDepth::Unknown,
      entry(5, true),
    )),
    Report::Update(update("/repo", Some(1), SvnDepth::Infinity, entry(1, true))),
    Report::Update(update(
      "/repo/trunk",
      Some(4),
      SvnDepth::Unknown,
      entry(5, false),
    )),
    Report::Update(status),
  ];
  assert_eq!(found, expected);
}

/// A-609 (`reports/get-locations.c`, `get-location-segments.c`, `get-locks.c`, `deleted-rev.c`, `dated-rev.c`): do
/// decode the location, lock, deleted-revision and dated-revision reports; expect their paths and revisions, the
/// depth attribute, and `{2026-01-01}` as 08:00 UTC from the Studio's Pacific time.
#[test]
fn the_captured_location_lock_and_date_reports_decode_as_their_handlers_read_them() {
  let found = [
    report_of("get-locations"),
    report_of("get-location-segments"),
    report_of("get-locks-report-immediates"),
    report_of("get-locks-report-infinity"),
    report_of("get-deleted-rev-report"),
    report_of("dated-rev-report"),
  ];
  let expected = [
    Report::GetLocations(GetLocationsReport {
      path: String::new(),
      peg_revision: 5,
      location_revisions: vec![Some(3)],
    }),
    Report::GetLocationSegments(GetLocationSegmentsReport {
      path: String::new(),
      peg_revision: Some(5),
      start: Some(5),
      end: Some(0),
    }),
    Report::GetLocks(GetLocksReport {
      depth: SvnDepth::Immediates,
    }),
    Report::GetLocks(GetLocksReport {
      depth: SvnDepth::Infinity,
    }),
    Report::GetDeletedRev(GetDeletedRevReport {
      path: "trunk/gone".to_owned(),
      peg_revision: 5,
      end_revision: 3,
    }),
    Report::DatedRev(DatedRevReport {
      date: AprTime(1_767_254_400_000_000),
    }),
  ];
  assert_eq!(found, expected);
}

/// A-609 (`reports/mergeinfo.c`, `inherited-props.c`, `replay.c`): do decode `svn mergeinfo`'s, `svn propget
/// --show-inherited-props`'s and `svnrdump dump -r 1:2`'s reports; expect the revision, `inherited`, descendants, the
/// path, and the 1.8 replay syntax's include path, low-water mark and deltas.
#[test]
fn the_captured_mergeinfo_inherited_props_and_replay_reports_decode() {
  let found = [
    report_of("mergeinfo-report"),
    report_of("inherited-props-report"),
    report_of("replay-report"),
  ];
  let expected = [
    Report::Mergeinfo(MergeinfoReport {
      revision: Some(5),
      inherit: Inherit::Inherited,
      include_descendants: true,
      paths: vec![String::new()],
    }),
    Report::InheritedProps(InheritedPropsReport {
      revision: Some(5),
      path: String::new(),
    }),
    Report::Replay(ReplayReport {
      revision: None,
      low_water_mark: 0,
      send_deltas: true,
      include_path: Some(String::new()),
    }),
  ];
  assert_eq!(found, expected);
}

/// A-609 (`mod_dav.c` `dav_method_merge`; `version.c` `merge`): do decode the MERGE `svn mkdir URL` sent, then one with
/// ra_serf's lock-token list, whose `lock-path` and `lock-token` carry no namespace; expect the transaction, the two
/// flags, the five properties, and each lock's path and token paired.
#[test]
fn the_captured_merge_names_its_transaction_and_a_lock_token_list_pairs_its_locks() {
  let merged = request::merge(captured("merge"), &ROOMY).unwrap();
  let props: Vec<&str> = merged.props.iter().map(|prop| prop.name.as_str()).collect();
  assert_eq!(
    (
      merged.source.as_str(),
      merged.no_auto_merge,
      merged.no_checkout,
      props
    ),
    (
      "/repo/!svn/txn/5-1",
      true,
      true,
      vec![
        "checked-in",
        "version-name",
        "resourcetype",
        "creationdate",
        "creator-displayname"
      ]
    )
  );
  let locked = b"<D:merge xmlns:D=\"DAV:\"><D:source><D:href>/repo/!svn/txn/5-1</D:href></D:source>\
    <S:lock-token-list xmlns:S=\"svn:\"><S:lock><lock-path>trunk/f.txt</lock-path>\
    <lock-token>opaquelocktoken:1</lock-token></S:lock><S:lock><lock-token>opaquelocktoken:2</lock-token>\
    <lock-path>trunk/g.txt</lock-path></S:lock></S:lock-token-list></D:merge>";
  let tokens = request::merge(locked, &ROOMY).unwrap().lock_tokens;
  let pair = |path: &str, token: &str| (path.to_owned(), token.to_owned());
  assert_eq!(
    tokens,
    vec![
      pair("trunk/f.txt", "opaquelocktoken:1"),
      pair("trunk/g.txt", "opaquelocktoken:2")
    ]
  );
}

/// A-609 (each handler's refusals): do send a report with no `svn:` root, an unknown report, a get-locations without
/// its peg revision, a log path out of canonical form, a revision of garbage, a get-locks depth of `exclude`, a replay
/// with two revisions, and an update-report without its source; expect each refused by kind, the unknown report 501.
#[test]
fn a_report_missing_what_its_handler_requires_is_refused_by_kind() {
  let refused = |body: &str| request::report(body.as_bytes(), &ROOMY).unwrap_err();
  let statuses: Vec<u16> = [
    "<D:log-report xmlns:D=\"DAV:\"/>",
    "<S:file-revs-report xmlns:S=\"svn:\"/>",
    "<S:get-locations xmlns:S=\"svn:\"><S:path></S:path></S:get-locations>",
    "<S:log-report xmlns:S=\"svn:\"><S:path>a//b</S:path></S:log-report>",
    "<S:log-report xmlns:S=\"svn:\"><S:start-revision>5x</S:start-revision></S:log-report>",
    "<S:get-locks-report xmlns:S=\"svn:\" depth=\"exclude\"/>",
    "<S:replay-report xmlns:S=\"svn:\"><S:revision>1</S:revision><S:revision>2</S:revision></S:replay-report>",
    "<S:update-report xmlns:S=\"svn:\"><S:target-revision>5</S:target-revision></S:update-report>",
  ]
  .iter()
  .map(|body| refused(body).http_status())
  .collect();
  assert_eq!(statuses, vec![501, 501, 400, 400, 400, 400, 400, 400]);
}

/// A-609 (no DTD, ever): do send a billion laughs, an external entity, and a reference to an undeclared entity; expect
/// the first two refused at the DTD's first byte and the third as an unknown entity, nothing expanded.
#[test]
fn a_doctype_a_billion_laughs_and_an_unknown_entity_are_refused_before_any_expansion() {
  let laughs = "<?xml version=\"1.0\"?><!DOCTYPE lolz [<!ENTITY lol \"lol\"><!ENTITY lol2 \
    \"&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;\">]><lolz>&lol2;</lolz>";
  let external = "<!DOCTYPE r [<!ENTITY x SYSTEM \"file:///etc/passwd\">]><r>&x;</r>";
  let unknown = "<S:log-report xmlns:S=\"svn:\"><S:path>&lol;</S:path></S:log-report>";
  let refusals: Vec<XmlRefusal> = [laughs, external, unknown]
    .iter()
    .map(|body| xml::parse(body.as_bytes(), &ROOMY).unwrap_err())
    .collect();
  assert_eq!(
    refusals,
    vec![
      XmlRefusal::Doctype { at: 21 },
      XmlRefusal::Doctype { at: 0 },
      XmlRefusal::UnknownEntity { at: 37 }
    ]
  );
}

/// A-609 (a body cut anywhere): do decode every proper prefix of every captured body; expect each refused, or, where
/// the cut only drops whitespace after the root, the whole body's reading, never a different one and never a panic.
#[test]
fn every_cut_of_every_captured_body_is_refused_or_reads_the_same() {
  let mut cuts = 0_usize;
  for (name, body) in CAPTURED {
    let whole = decoded(name, body).unwrap();
    for cut in 1..body.len() {
      let prefix = &body[..cut];
      match decoded(name, prefix) {
        Ok(reading) => assert_eq!((name, cut, reading), (name, cut, whole.clone())),
        Err(_) => cuts += 1,
      }
    }
  }
  assert!(cuts > 3_000, "{cuts} cuts refused");
}

/// A-609 (the caller's bounds): do read a body one byte past its bound, a name one byte past its bound, a nesting one
/// level past, one element past the count and one attribute past theirs; expect each refused by its bound, each 413.
#[test]
fn a_body_past_any_of_its_bounds_is_refused_by_that_bound() {
  let tight = Bounds {
    body_bytes: 64,
    depth: 2,
    name_bytes: 8,
    attributes: 1,
    elements: 3,
  };
  let refusals: Vec<XmlRefusal> = [
    format!("<a>{}</a>", "x".repeat(58)),
    "<abcdefghi/>".to_owned(),
    "<a><b><c/></b></a>".to_owned(),
    "<a><b/><b/><b/></a>".to_owned(),
    "<a x=\"1\" y=\"2\"/>".to_owned(),
  ]
  .iter()
  .map(|body| xml::parse(body.as_bytes(), &tight).unwrap_err())
  .collect();
  let past: Vec<bool> = refusals
    .iter()
    .map(|refusal| DavRefusal::Xml(refusal.clone()).http_status() == 413)
    .collect();
  assert_eq!(past, vec![true; 5], "{refusals:?}");
  assert_eq!(
    refusals,
    vec![
      XmlRefusal::TooLarge {
        bytes: 65,
        bound: 64
      },
      XmlRefusal::NameTooLong { at: 1 },
      XmlRefusal::TooDeep { at: 6 },
      XmlRefusal::TooManyElements { at: 11 },
      XmlRefusal::TooManyAttributes { at: 9 }
    ]
  );
}

/// A-609 (the grammar): do read bytes that are not UTF-8, a declared Latin-1 body, a processing instruction, an
/// unbound prefix, a mismatched end tag, a duplicate attribute, a control character, a bad character reference and text
/// after the root; expect each refused by kind.
#[test]
fn malformed_xml_is_refused_by_kind() {
  let refusals: Vec<String> = [
    &b"<a>\xff</a>"[..],
    b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><a/>",
    b"<?xml version=\"1.0\"?><?pi x?><a/>",
    b"<p:a/>",
    b"<a></b>",
    b"<a x=\"1\" x=\"2\"/>",
    b"<a>\x01</a>",
    b"<a>&#0;</a>",
    b"<a/>text",
  ]
  .iter()
  .map(|body| {
    let refusal = format!("{:?}", xml::parse(body, &ROOMY).unwrap_err());
    refusal.split([' ', '{']).next().unwrap_or("").to_owned()
  })
  .collect();
  let expected = [
    "NotUtf8",
    "Encoding",
    "ProcessingInstruction",
    "UnboundPrefix",
    "MismatchedEnd",
    "DuplicateAttribute",
    "IllegalCharacter",
    "BadCharacterReference",
    "TrailingContent",
  ];
  assert_eq!(refusals, expected);
}

/// A-609 (XML 1.0 §2.11, §3.3.3, §4.1): do read text with CRLF and a lone CR, an attribute with a literal tab and a
/// referenced line feed, references to all five entities and two characters, and a CDATA section; expect line feeds in
/// text, a space and a line feed in the attribute, and the characters named.
#[test]
fn line_breaks_attributes_references_and_cdata_read_as_expat_reads_them() {
  let body = "<a v=\"x\ty&#10;z\">one\r\ntwo\rthree &lt;&gt;&amp;&quot;&apos;&#65;&#x42;<![CDATA[<raw>]]></a>";
  let root = xml::parse(body.as_bytes(), &ROOMY).unwrap();
  assert_eq!(
    (root.attribute("v"), root.text()),
    (Some("x y\nz"), "one\ntwo\nthree <>&\"'AB<raw>".to_owned())
  );
}

/// A-609 (`svn_base64_encode_string2` with line breaks): do encode RFC 4648's test vectors and 58 bytes; expect each
/// line ended by a line feed, the last too, 76 characters a full line.
#[test]
fn base64_is_written_in_svn_lines() {
  let vectors: Vec<String> = ["", "f", "fo", "foo", "foob", "fooba", "foobar"]
    .iter()
    .map(|text| xml::base64_lines(text.as_bytes()))
    .collect();
  assert_eq!(
    vectors,
    vec![
      "",
      "Zg==\n",
      "Zm8=\n",
      "Zm9v\n",
      "Zm9vYg==\n",
      "Zm9vYmE=\n",
      "Zm9vYmFy\n"
    ]
  );
  let long = xml::base64_lines(&[0_u8; 58]);
  assert_eq!(long, format!("{}\nAA==\n", "A".repeat(76)));
}

/// A-609 (`svn_time_to_cstring`, `svn_time_from_cstring`): do write the epoch, a moment of 2023 and a leap day, read
/// each back, and read a 29th of February in a common year and a time of 24 o'clock; expect the six-digit form, the
/// same instants, and the last two refused.
#[test]
fn dates_are_written_and_read_in_svn_form() {
  let instants = [0, 1_700_000_000_123_456, 1_709_164_800_000_001];
  let written: Vec<String> = instants
    .iter()
    .map(|micros| response::svn_time(AprTime(*micros)).unwrap())
    .collect();
  assert_eq!(
    written,
    vec![
      "1970-01-01T00:00:00.000000Z",
      "2023-11-14T22:13:20.123456Z",
      "2024-02-29T00:00:00.000001Z"
    ]
  );
  let read: Vec<Option<AprTime>> = written.iter().map(|text| request::svn_time(text)).collect();
  assert_eq!(
    read,
    instants
      .iter()
      .map(|micros| Some(AprTime(*micros)))
      .collect::<Vec<_>>()
  );
  let refused = [
    request::svn_time("2023-02-29T00:00:00.000000Z"),
    request::svn_time("2023-01-01T24:00:00.000000Z"),
  ];
  assert_eq!(refused, [None, None]);
}

/// A-609 (the date arithmetic): do turn every 97th day from 1970 to 9999 into a date and back; expect each day back.
#[test]
fn every_sampled_day_survives_a_round_trip_through_its_date() {
  let mut sampled = 0_u64;
  for days in (0..2_932_896_u64).step_by(97) {
    let (year, month, day) = super::time::civil_from_days(days).unwrap();
    assert_eq!(
      super::time::days_from_civil(year, month, day),
      Some(days),
      "{year}-{month}-{day}"
    );
    sampled += 1;
  }
  assert!(sampled > 30_000);
}

/// A-609 (`dated-rev.c`, `deleted-rev.c`, `get-locations.c`, `get-location-segments.c`): do write a dated-revision,
/// a deleted-revision of none, two locations and two segments, one a gap; expect `mod_dav_svn`'s bytes.
#[test]
fn the_small_reports_are_written_byte_for_byte() {
  let header = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";
  let written = [
    response::revision_report("dated-rev-report", Some(4747), ROOM).unwrap(),
    response::revision_report("get-deleted-rev-report", None, ROOM).unwrap(),
    response::locations(&[(1, "/httpd".to_owned()), (2, "/a\"b".to_owned())], ROOM).unwrap(),
    response::location_segments(
      &[
        Segment {
          path: Some("trunk".to_owned()),
          start: 3,
          end: 5,
        },
        Segment {
          path: None,
          start: 1,
          end: 2,
        },
      ],
      ROOM,
    )
    .unwrap(),
  ];
  let expected = [
    format!(
      "{header}<S:dated-rev-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<D:version-name>4747</D:version-name></S:dated-rev-report>"
    ),
    format!(
      "{header}<S:get-deleted-rev-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<D:version-name>-1</D:version-name></S:get-deleted-rev-report>"
    ),
    format!(
      "{header}<S:get-locations-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<S:location rev=\"1\" path=\"/httpd\"/>\n<S:location rev=\"2\" path=\"/a&quot;b\"/>\n</S:get-locations-report>\n"
    ),
    format!(
      "{header}<S:get-location-segments-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<S:location-segment path=\"trunk\" range-start=\"3\" range-end=\"5\"/>\n<S:location-segment range-start=\"1\" range-end=\"2\"/>\n</S:get-location-segments-report>\n"
    ),
  ];
  let written: Vec<String> = written
    .into_iter()
    .map(|bytes| String::from_utf8(bytes).unwrap())
    .collect();
  assert_eq!(written, expected);
}

/// A-609 (`get-locks.c`): do write two locks, one with an expiry, an owner XML cannot carry and a comment; expect the
/// space `"<S:owner %s>"` leaves when nothing is encoded, and base64 with its encoding where it is.
#[test]
fn locks_are_written_as_get_locks_c_writes_them() {
  let lock = |path: &str, owner: &str, comment: Option<&str>, expires: Option<AprTime>| Lock {
    path: path.to_owned(),
    token: LockToken::supplied("opaquelocktoken:706689a6").unwrap(),
    owner: owner.to_owned(),
    comment: comment.map(str::to_owned),
    is_dav_comment: false,
    created: AprTime(1_107_785_828_000_000),
    expires,
  };
  let written = response::locks(
    &[
      lock("/foo/bar", "fred", None, None),
      lock(
        "/baz",
        "b\u{7}d",
        Some("a <note>"),
        Some(AprTime(1_107_872_228_000_000)),
      ),
    ],
    ROOM,
  )
  .unwrap();
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:get-locks-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n\
    <S:lock>\n<S:path>/foo/bar</S:path>\n<S:token>opaquelocktoken:706689a6</S:token>\n\
    <S:creationdate>2005-02-07T14:17:08.000000Z</S:creationdate>\n<S:owner >fred</S:owner>\n</S:lock>\n\
    <S:lock>\n<S:path>/baz</S:path>\n<S:token>opaquelocktoken:706689a6</S:token>\n\
    <S:creationdate>2005-02-07T14:17:08.000000Z</S:creationdate>\n\
    <S:expirationdate>2005-02-08T14:17:08.000000Z</S:expirationdate>\n<S:owner encoding=\"base64\">Ygdk\n</S:owner>\n\
    <S:comment >a &lt;note&gt;</S:comment>\n</S:lock>\n</S:get-locks-report>\n";
  assert_eq!(String::from_utf8(written).unwrap(), expected);
}

/// A-609 (`log.c`): do write an item with an added path copied from another, a deleted path, the three standard
/// revision properties (the log message holding a bell), a custom binary one with binary encoding asked for, and
/// merged children; expect `mod_dav_svn`'s elements in its order, the bell fuzzily escaped, the binary value in base64.
#[test]
fn a_log_item_is_written_as_log_c_writes_it() {
  let item = LogItem {
    revision: Some(2),
    changed: vec![
      ChangedPath {
        path: "/trunk/new".to_owned(),
        change: Change::Added,
        kind: "file",
        text_mods: true,
        prop_mods: false,
        copy_from: Some(("/trunk/old".to_owned(), 1)),
      },
      ChangedPath {
        path: "/trunk/old".to_owned(),
        change: Change::Deleted,
        kind: "file",
        text_mods: false,
        prop_mods: false,
        copy_from: None,
      },
    ],
    revprops: vec![
      ("svn:author".to_owned(), b"bob".to_vec()),
      (
        "svn:date".to_owned(),
        b"2006-02-27T18:44:26.149336Z".to_vec(),
      ),
      ("svn:log".to_owned(), b"Add \x07doo-hickey".to_vec()),
      ("x:bin".to_owned(), vec![0xff, 0x00]),
    ],
    has_children: true,
    subtractive_merge: false,
  };
  let written = String::from_utf8(response::log(&[item], true, ROOM).unwrap()).unwrap();
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:log-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n\
    <S:log-item>\n<S:added-path copyfrom-path=\"/trunk/old\" copyfrom-rev=\"1\" node-kind=\"file\" text-mods=\"true\" \
    prop-mods=\"false\">/trunk/new</S:added-path>\n<S:deleted-path node-kind=\"file\" text-mods=\"false\" \
    prop-mods=\"false\">/trunk/old</S:deleted-path>\n<D:version-name>2</D:version-name>\n\
    <D:creator-displayname>bob</D:creator-displayname>\n<S:date>2006-02-27T18:44:26.149336Z</S:date>\n\
    <D:comment encoding=\"base64\">QWRkIAdkb28taGlja2V5\n</D:comment>\n<S:revprop name=\"x:bin\" encoding=\"base64\">/wA=\n\
    </S:revprop>\n<S:has-children/></S:log-item>\n</S:log-report>\n";
  assert_eq!(written, expected);
  let plain = LogItem {
    revision: None,
    changed: Vec::new(),
    revprops: vec![("svn:log".to_owned(), b"Add \x07it".to_vec())],
    has_children: false,
    subtractive_merge: false,
  };
  let written = String::from_utf8(response::log(&[plain], false, ROOM).unwrap()).unwrap();
  assert!(
    written.contains("<D:version-name>-1</D:version-name>\n<D:comment>Add ?\\007it</D:comment>\n"),
    "{written}"
  );
}

/// A-609 (`mergeinfo.c`, `inherited-props.c`): do write a mergeinfo item and an inherited item with a value holding a
/// carriage return and one XML cannot carry; expect `mod_dav_svn`'s bytes, `&#13;` for the carriage return, base64 for
/// the other.
#[test]
fn mergeinfo_and_inherited_props_are_written_byte_for_byte() {
  let mergeinfo = response::mergeinfo(
    &[("/A_COPY/B/E".to_owned(), "/A/B/E:1,3-4".to_owned())],
    ROOM,
  )
  .unwrap();
  let inherited = response::inherited_props(
    &[InheritedItem {
      path: "trunk".to_owned(),
      props: vec![
        ("svn:ignore".to_owned(), b"*.o\r\n".to_vec()),
        ("bin".to_owned(), vec![0x01]),
      ],
    }],
    ROOM,
  )
  .unwrap();
  let header = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";
  assert_eq!(
    (
      String::from_utf8(mergeinfo).unwrap(),
      String::from_utf8(inherited).unwrap()
    ),
    (
      format!(
        "{header}<S:mergeinfo-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<S:mergeinfo-item>\n<S:mergeinfo-path>/A_COPY/B/E</S:mergeinfo-path>\n<S:mergeinfo-info>/A/B/E:1,3-4</S:mergeinfo-info>\n</S:mergeinfo-item></S:mergeinfo-report>\n"
      ),
      format!(
        "{header}<S:inherited-props-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<S:iprop-item>\n<S:iprop-path>trunk</S:iprop-path>\n<S:iprop-propname>svn:ignore</S:iprop-propname>\n<S:iprop-propval>*.o&#13;\n</S:iprop-propval>\n<S:iprop-propname>bin</S:iprop-propname>\n<S:iprop-propval encoding=\"base64\">AQ==\n</S:iprop-propval>\n</S:iprop-item>\n</S:inherited-props-report>\n"
      )
    )
  );
}

/// A-609 (`version.c` `get_option`, `mod_dav.c` `dav_method_options`): do answer an OPTIONS asking for the activity
/// collection and one asking for nothing; expect the activity collection's href and the HTTPv2 headers only for the
/// first, the stubs built from the root and the special URI, and the commit capabilities last.
#[test]
fn an_options_answer_gives_the_httpv2_headers_only_when_asked() {
  let facts = Httpv2 {
    root: "/repo".to_owned(),
    special: "!svn".to_owned(),
    youngest: Some(5),
    uuid: Some("6b4a5e2c".to_owned()),
    mergeinfo: Some(true),
    bulk_updates: BulkUpdates::Prefer,
  };
  let asked = response::options_headers(&facts, true);
  let unasked = response::options_headers(&facts, false);
  let value = |name: &str| {
    asked
      .iter()
      .find(|(header, _)| *header == name)
      .map(|(_, value)| value.as_str())
  };
  assert_eq!(
    (
      value("SVN-Me-Resource"),
      value("SVN-Rev-Root-Stub"),
      value("SVN-Youngest-Rev"),
      unasked.len()
    ),
    (Some("/repo/!svn/me"), Some("/repo/!svn/rvr"), Some("5"), 12)
  );
  assert_eq!(
    asked.last().map(|(_, value)| value.as_str()),
    Some("http://subversion.tigris.org/xmlns/dav/svn/put-result-checksum")
  );
  let body = String::from_utf8(response::options_body(&facts, true, ROOM).unwrap()).unwrap();
  assert_eq!(
    body,
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:options-response xmlns:D=\"DAV:\">\n<D:activity-collection-set>\
     <D:href>/repo/!svn/act/</D:href></D:activity-collection-set></D:options-response>\n"
  );
}

/// A-609 (`merge.c` `dav_svn__merge_response`): do answer a MERGE with no post-commit failure and one with; expect the
/// baseline's response, the empty line where the failure would stand, and the failure in `S:` when there is one.
#[test]
fn a_merge_answer_is_written_as_merge_c_writes_it() {
  let answer = |post_commit_error: Option<&str>| MergeResponse {
    vcc: "/repo/!svn/vcc/default".to_owned(),
    revision: 6,
    date: Some("2026-11-19T00:00:00.000000Z".to_owned()),
    author: Some("alice".to_owned()),
    post_commit_error: post_commit_error.map(str::to_owned),
  };
  let quiet = String::from_utf8(response::merge(&answer(None), ROOM).unwrap()).unwrap();
  let failed =
    String::from_utf8(response::merge(&answer(Some("hook <failed>")), ROOM).unwrap()).unwrap();
  assert_eq!(
    quiet,
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:merge-response xmlns:D=\"DAV:\">\n<D:updated-set>\n<D:response>\n\
     <D:href>/repo/!svn/vcc/default</D:href>\n<D:propstat><D:prop>\n<D:resourcetype><D:baseline/></D:resourcetype>\n\n\
     <D:version-name>6</D:version-name>\n<D:creationdate>2026-11-19T00:00:00.000000Z</D:creationdate>\n\
     <D:creator-displayname>alice</D:creator-displayname>\n</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n\
     </D:propstat>\n</D:response>\n</D:updated-set>\n</D:merge-response>\n"
  );
  assert!(
    failed.contains(" xmlns:S=\"svn:\">\n")
      && failed.contains("<S:post-commit-err>hook &lt;failed&gt;</S:post-commit-err>\n")
  );
}

/// A-609 (every response is XML a client reads): do read back every response these tests write, a multistatus among
/// them, with the reader; expect each well formed, its root in its namespace.
#[test]
fn every_written_response_reads_back_as_well_formed_xml() {
  let multistatus = response::multistatus(
    &[(
      "/repo/!svn/rvr/5/trunk".to_owned(),
      vec![
        Propstat {
          status: "200 OK",
          props: vec![
            response::live_prop(true, "resourcetype", "<D:collection/>"),
            response::live_prop(true, "version-name", "5"),
            response::live_prop(false, "deadprop-count", "0"),
          ],
        },
        Propstat {
          status: "404 Not Found",
          props: vec!["<g0:missing xmlns:g0=\"urn:x\"/>\n".to_owned()],
        },
      ],
    )],
    ROOM,
  )
  .unwrap();
  let bodies = [
    multistatus,
    response::revision_report("dated-rev-report", Some(1), ROOM).unwrap(),
    response::locations(&[(1, "/a".to_owned())], ROOM).unwrap(),
    response::mergeinfo(&[("/a".to_owned(), "/b:1".to_owned())], ROOM).unwrap(),
  ];
  let roots: Vec<(String, String)> = bodies
    .iter()
    .map(|body| {
      let root = xml::parse(body, &ROOMY).unwrap();
      (root.namespace, root.name)
    })
    .collect();
  let expected = [
    (DAV, "multistatus"),
    (SVN, "dated-rev-report"),
    (SVN, "get-locations-report"),
    (SVN, "mergeinfo-report"),
  ]
  .map(|(namespace, name)| (namespace.to_owned(), name.to_owned()));
  assert_eq!(roots, expected);
}

/// A-609 (the room): do write a log into one byte less room than it needs; expect it refused with what it needed.
#[test]
fn a_response_past_its_room_is_refused() {
  let whole = response::revision_report("dated-rev-report", Some(1), ROOM).unwrap();
  let refused =
    response::revision_report("dated-rev-report", Some(1), whole.len() - 1).unwrap_err();
  assert_eq!(
    (refused.clone(), refused.http_status()),
    (
      DavRefusal::OutputRoom {
        needed: whole.len(),
        room: whole.len() - 1
      },
      500
    )
  );
}
