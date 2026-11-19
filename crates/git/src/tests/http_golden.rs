//! A-595's golden vectors: stock git 2.50.1's own HTTP traffic with `git http-backend` (Apple Git-155), recorded on
//! 2026-11-19 by `fixtures/http/make-http-fixtures.sh` (its header lists the twelve exchanges), compiled in, never read
//! at run time, and checked against the SHA-256 sums the script wrote. As main ruled with A-607, they hold inflated
//! bytes alone: the gzip request is held inflated and gzipped again here, and no pack is held.

use std::io::Write as _;

use aws_lc_rs::digest::{SHA256, digest};
use flate2::Compression;
use flate2::write::GzEncoder;
use slates_http::body::{BodyReader, Step};
use slates_http::gzip::Gunzip;
use slates_http::head::{Coding, Framing, Head, HeadReader, Progress, Version as HttpVersion};

use super::{Reply, Route, Service, Version, advertisement_prelude, reply_head, route};
use crate::pktline::{self, Packet};

/// A fixture, by its file name.
macro_rules! fixture {
  ($name:literal) => {
    (
      $name,
      include_bytes!(concat!("../../fixtures/http/", $name)).as_slice(),
    )
  };
}

/// The sums `make-http-fixtures.sh` wrote, one `<sha256>  <name>` line a file.
const SUMS: &str = include_str!("../../fixtures/http/SHA256SUMS");

/// Every file the script recorded.
const FIXTURES: [(&str, &[u8]); 38] = [
  fixture!("01-get-info-refs.reply-body.bin"),
  fixture!("01-get-info-refs.reply-head.bin"),
  fixture!("01-get-info-refs.request-head.bin"),
  fixture!("02-get-info-refs.reply-body.bin"),
  fixture!("02-get-info-refs.reply-head.bin"),
  fixture!("02-get-info-refs.request-head.bin"),
  fixture!("03-get-info-refs.reply-body.bin"),
  fixture!("03-get-info-refs.reply-head.bin"),
  fixture!("03-get-info-refs.request-head.bin"),
  fixture!("04-post-git-upload-pack.reply-head.bin"),
  fixture!("04-post-git-upload-pack.request-body.bin"),
  fixture!("04-post-git-upload-pack.request-head.bin"),
  fixture!("05-post-git-upload-pack.reply-head.bin"),
  fixture!("05-post-git-upload-pack.request-head.bin"),
  fixture!("05-post-git-upload-pack.request-inflated.bin"),
  fixture!("06-get-info-refs.reply-body.bin"),
  fixture!("06-get-info-refs.reply-head.bin"),
  fixture!("06-get-info-refs.request-head.bin"),
  fixture!("07-post-git-receive-pack.reply-head.bin"),
  fixture!("07-post-git-receive-pack.request-head.bin"),
  fixture!("08-get-info-refs.reply-body.bin"),
  fixture!("08-get-info-refs.reply-head.bin"),
  fixture!("08-get-info-refs.request-head.bin"),
  fixture!("09-post-git-upload-pack.reply-head.bin"),
  fixture!("09-post-git-upload-pack.request-body.bin"),
  fixture!("09-post-git-upload-pack.request-head.bin"),
  fixture!("10-post-git-upload-pack.reply-head.bin"),
  fixture!("10-post-git-upload-pack.request-chunked.bin"),
  fixture!("10-post-git-upload-pack.request-data.sha256"),
  fixture!("10-post-git-upload-pack.request-head.bin"),
  fixture!("11-post-git-upload-pack.reply-head.bin"),
  fixture!("11-post-git-upload-pack.request-body.bin"),
  fixture!("11-post-git-upload-pack.request-head.bin"),
  fixture!("12-post-git-upload-pack.reply-head.bin"),
  fixture!("12-post-git-upload-pack.request-chunked.bin"),
  fixture!("12-post-git-upload-pack.request-data.sha256"),
  fixture!("12-post-git-upload-pack.request-head.bin"),
  fixture!("versions.txt"),
];

/// Room enough for every request recorded.
const ROOM: u64 = 1 << 24;

/// The fixture `name`.
fn file(name: &str) -> &'static [u8] {
  FIXTURES
    .iter()
    .find(|(known, _)| *known == name)
    .map(|(_, bytes)| *bytes)
    .unwrap_or_else(|| panic!("no fixture {name}"))
}

/// The fixture of exchange `number` with `suffix` (`request-head.bin`, say).
fn part(number: u32, suffix: &str) -> &'static [u8] {
  let prefix = format!("{number:02}-");
  FIXTURES
    .iter()
    .find(|(name, _)| name.starts_with(&prefix) && name.ends_with(suffix))
    .map(|(_, bytes)| *bytes)
    .unwrap_or_else(|| panic!("no fixture {prefix}*{suffix}"))
}

/// The head of exchange `number`'s request, read in one call; it must be exactly the head.
fn request(number: u32) -> Head<'static> {
  let bytes = part(number, "request-head.bin");
  match HeadReader::new().read(bytes, &mut slates_http::Admission::new(ROOM)) {
    Ok(Progress::Complete { value, consumed }) => {
      assert_eq!(consumed, bytes.len(), "exchange {number}");
      value
    }
    other => panic!("exchange {number} read {other:?}"),
  }
}

/// The hex SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
  digest(&SHA256, bytes)
    .as_ref()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// The data a framed body gives, read in `pieces`, and the bytes the body took in all.
fn unframe(framing: Framing, pieces: &[&[u8]]) -> (Vec<u8>, usize) {
  let mut reader = BodyReader::new(framing);
  let mut admission = slates_http::Admission::new(ROOM);
  let (mut data, mut taken) = (Vec::new(), 0usize);
  for piece in pieces {
    let mut at = 0;
    loop {
      match reader.step(&piece[at..], &mut admission).unwrap() {
        Step::Data {
          data: run,
          consumed,
        } => {
          data.extend_from_slice(run);
          at += consumed;
        }
        Step::More { consumed } => {
          taken += at + consumed;
          break;
        }
        Step::End { consumed } => return (data, taken + at + consumed),
      }
    }
  }
  panic!("the body did not end");
}

/// The pkt-lines of `bytes`, which must be whole.
fn packets(bytes: &[u8]) -> Vec<Packet<'_>> {
  pktline::decode(bytes, &mut crate::Admission::new(ROOM)).unwrap()
}

/// A-595: do hash every fixture and read the script's sums. Expect each fixture to be the file the script recorded and
/// every file it recorded to be here.
#[test]
fn every_fixture_is_the_file_the_script_recorded() {
  let lines: Vec<(&str, &str)> = SUMS
    .lines()
    .map(|line| line.split_once("  ").unwrap())
    .collect();
  assert_eq!(lines.len(), FIXTURES.len());
  for (sum, name) in lines {
    assert_eq!(sha256(file(name)), sum, "{name}");
  }
  assert!(file("versions.txt").starts_with(b"git version 2.50.1"));
}

/// A-595: do read every request head stock git sent, whole and a byte more at a time, and route it. Expect each read as
/// one read reads it, framed and coded as git sent it, and routed to the repository, service and version git meant.
#[test]
fn every_request_git_sent_reads_and_routes_as_git_meant_it() {
  let advertise = |repo: &str, service, version| Route::Advertise {
    repo: repo.to_owned(),
    service,
    version,
    head_only: false,
  };
  let serve = |repo: &str, version| Route::Serve {
    repo: repo.to_owned(),
    service: Service::UploadPack,
    version,
  };
  let cases: [(u32, Route, Framing, Coding); 12] = [
    (
      1,
      advertise("small.git", Service::UploadPack, Version::V0),
      Framing::None,
      Coding::Identity,
    ),
    (
      2,
      advertise("small.git", Service::UploadPack, Version::V1),
      Framing::None,
      Coding::Identity,
    ),
    (
      3,
      advertise("small.git", Service::UploadPack, Version::V2),
      Framing::None,
      Coding::Identity,
    ),
    (
      4,
      serve("small.git", Version::V2),
      Framing::Length(182),
      Coding::Identity,
    ),
    (
      5,
      serve("small.git", Version::V2),
      Framing::Length(895),
      Coding::Gzip,
    ),
    (
      6,
      advertise("small.git", Service::ReceivePack, Version::V0),
      Framing::None,
      Coding::Identity,
    ),
    (
      7,
      Route::Serve {
        repo: "small.git".to_owned(),
        service: Service::ReceivePack,
        version: Version::V0,
      },
      Framing::Length(360),
      Coding::Identity,
    ),
    (
      8,
      advertise("many.git", Service::UploadPack, Version::V2),
      Framing::None,
      Coding::Identity,
    ),
    (
      9,
      serve("many.git", Version::V0),
      Framing::Length(4),
      Coding::Identity,
    ),
    (
      10,
      serve("many.git", Version::V2),
      Framing::Chunked,
      Coding::Identity,
    ),
    (
      11,
      serve("many.git", Version::V0),
      Framing::Length(4),
      Coding::Identity,
    ),
    (
      12,
      serve("many.git", Version::V2),
      Framing::Chunked,
      Coding::Identity,
    ),
  ];
  for (number, expected, framing, coding) in cases {
    let head = request(number);
    assert_eq!(
      (head.version, head.framing, head.coding),
      (HttpVersion::Http11, framing, coding),
      "exchange {number}"
    );
    assert!(head.keep_alive, "exchange {number}");
    assert_eq!(
      route(&head, &mut slates_http::Admission::new(ROOM)),
      Ok(expected),
      "exchange {number}"
    );
    let bytes = part(number, "request-head.bin");
    let mut reader = HeadReader::new();
    let mut admission = slates_http::Admission::new(ROOM);
    for cut in 0..bytes.len() {
      assert_eq!(
        reader.read(&bytes[..cut], &mut admission),
        Ok(Progress::Partial),
        "exchange {number} cut {cut}"
      );
    }
    let Ok(Progress::Complete { value, .. }) = reader.read(bytes, &mut admission) else {
      panic!("exchange {number} did not complete a byte at a time");
    };
    assert_eq!(value, head, "exchange {number}");
  }
}

/// A-595: do remove the framing of every request body held, whole and in cuts; gzip the inflated fetch again and inflate
/// it. Expect each body's data as sent (the chunked ones' SHA-256 equal to the one the recording host computed), the
/// gzip fetch's bytes back, and every body whole pkt-lines: the probes a flush alone, the large fetch 1401 wants of
/// 1400 commits (HEAD's wanted twice, as git asks for it).
#[test]
fn every_body_git_sent_reads_back_as_git_wrote_it() {
  for (number, length) in [(4, 182), (9, 4), (11, 4)] {
    let body = part(number, "request-body.bin");
    assert_eq!(body.len(), length);
    assert_eq!(
      unframe(Framing::Length(crate::to_u64(length)), &[body]),
      (body.to_vec(), length)
    );
    assert_eq!(
      packets(body).last(),
      Some(&Packet::Flush),
      "exchange {number}"
    );
  }
  assert_eq!(packets(part(9, "request-body.bin")), vec![Packet::Flush]);
  for number in [10, 12] {
    let raw = part(number, "request-chunked.bin");
    let recorded = String::from_utf8(part(number, "request-data.sha256").to_vec()).unwrap();
    let (data, taken) = unframe(Framing::Chunked, &[raw]);
    assert_eq!(taken, raw.len(), "exchange {number}");
    assert_eq!(sha256(&data), recorded.trim_end(), "exchange {number}");
    let bytes: Vec<&[u8]> = raw.chunks(1).collect();
    assert_eq!(
      unframe(Framing::Chunked, &bytes),
      (data.clone(), raw.len()),
      "exchange {number}"
    );
    let lines = packets(&data);
    assert_eq!(lines.last(), Some(&Packet::Flush), "exchange {number}");
    if number == 12 {
      // git wants each ref's tip, HEAD's among them, so the commit HEAD names is wanted twice.
      let wants: Vec<&[u8]> = lines
        .iter()
        .filter_map(|packet| match packet {
          Packet::Data(line) => line.strip_prefix(b"want "),
          _ => None,
        })
        .collect();
      let distinct: std::collections::BTreeSet<&[u8]> = wants.iter().copied().collect();
      assert_eq!((wants.len(), distinct.len()), (1401, 1400));
    }
  }
  let inflated = part(5, "request-inflated.bin");
  assert!(inflated.len() > 1024, "git gzips a request past 1024 bytes");
  let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(inflated).unwrap();
  let gzipped = encoder.finish().unwrap();
  let mut admission = slates_http::Admission::new(ROOM);
  let mut gunzip = Gunzip::new();
  for piece in gzipped.chunks(100) {
    gunzip.feed(piece, &mut admission).unwrap();
  }
  gunzip.finish(&mut admission).unwrap();
  assert_eq!(gunzip.take(), inflated);
  assert_eq!(packets(inflated).last(), Some(&Packet::Flush));
}

/// A-595: do write each reply head the routes answer with and compare it with `http-backend`'s CGI head for the same
/// request. Expect the same fields in the same order, byte for byte, after the status line and before the framing.
#[test]
fn every_reply_head_is_written_as_http_backend_wrote_it() {
  let cases: [(u32, Reply); 12] = [
    (1, Reply::Advertisement(Service::UploadPack)),
    (2, Reply::Advertisement(Service::UploadPack)),
    (3, Reply::Advertisement(Service::UploadPack)),
    (4, Reply::Result(Service::UploadPack)),
    (5, Reply::Result(Service::UploadPack)),
    (6, Reply::Advertisement(Service::ReceivePack)),
    (7, Reply::Result(Service::ReceivePack)),
    (8, Reply::Advertisement(Service::UploadPack)),
    (9, Reply::Result(Service::UploadPack)),
    (10, Reply::Result(Service::UploadPack)),
    (11, Reply::Result(Service::UploadPack)),
    (12, Reply::Result(Service::UploadPack)),
  ];
  for (number, reply) in cases {
    let cgi = part(number, "reply-head.bin");
    let fields = cgi.strip_suffix(b"\r\n").unwrap();
    let mut expected = b"HTTP/1.1 200 OK\r\n".to_vec();
    expected.extend_from_slice(fields);
    expected.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
    let mut ours = Vec::new();
    reply_head(reply, HttpVersion::Http11, true, &mut ours).unwrap();
    assert_eq!(
      String::from_utf8(ours).unwrap(),
      String::from_utf8(expected).unwrap(),
      "exchange {number}"
    );
  }
}

/// A-595: do write the prelude of each advertisement `http-backend` answered. Expect its body to start with it: the
/// `# service=` line and a flush for v0 and v1 (v1's `version 1` line after them), nothing before v2's `version 2`.
#[test]
fn every_advertisement_starts_as_http_backend_started_it() {
  let cases: [(u32, Service, Version, &[u8]); 5] = [
    (1, Service::UploadPack, Version::V0, b"0112"),
    (2, Service::UploadPack, Version::V1, b"000eversion 1\n"),
    (3, Service::UploadPack, Version::V2, b"000eversion 2\n"),
    (6, Service::ReceivePack, Version::V0, b"00b8"),
    (8, Service::UploadPack, Version::V2, b"000eversion 2\n"),
  ];
  for (number, service, version, next) in cases {
    let body = part(number, "reply-body.bin");
    let mut prelude = Vec::new();
    advertisement_prelude(service, version, &mut prelude).unwrap();
    assert_eq!(prelude.is_empty(), version == Version::V2);
    let rest = body
      .strip_prefix(prelude.as_slice())
      .unwrap_or_else(|| panic!("exchange {number}"));
    assert!(rest.starts_with(next), "exchange {number}");
    assert_eq!(
      packets(body).last(),
      Some(&Packet::Flush),
      "exchange {number}"
    );
  }
}
