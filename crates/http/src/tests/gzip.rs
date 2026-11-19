use std::io::Write as _;

use flate2::Compression;
use flate2::GzBuilder;
use flate2::write::GzEncoder;

use super::{Gunzip, GzipRefused};
use crate::reply::Status;
use crate::{Admission, PastAdmission, to_u64};

/// Room enough for every body these tests inflate but the bomb.
const ROOM: u64 = 1 << 24;

/// `data` gzipped by flate2's own encoder, one member.
fn gzipped(data: &[u8]) -> Vec<u8> {
  let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(data).unwrap();
  encoder.finish().unwrap()
}

/// A text long enough to span several deflate blocks' worth of matches.
fn text() -> Vec<u8> {
  (0..400)
    .flat_map(|line| format!("0032have {line:040x}\n").into_bytes())
    .collect()
}

/// What `pieces` inflate to under `admission`, fed in order and finished.
fn inflate(pieces: &[&[u8]], admission: &mut Admission) -> Result<Vec<u8>, GzipRefused> {
  let mut gunzip = Gunzip::new();
  let mut out = Vec::new();
  for piece in pieces {
    gunzip.feed(piece, admission)?;
    out.extend(gunzip.take());
  }
  gunzip.finish(admission)?;
  out.extend(gunzip.take());
  Ok(out)
}

/// A-595: do inflate a gzip body whole. Expect its bytes, every one charged to the admission and no more.
#[test]
fn a_gzip_body_inflates_to_its_bytes_charged_exactly() {
  let text = text();
  let mut admission = Admission::new(ROOM);
  assert_eq!(
    inflate(&[&gzipped(&text)], &mut admission),
    Ok(text.clone())
  );
  assert_eq!(admission.left(), ROOM - to_u64(text.len()));
}

/// A-595: do feed a gzip body in two pieces at every cut, and a byte at a time. Expect the same bytes each time.
#[test]
fn every_cut_of_a_gzip_body_inflates_the_same() {
  let text = text();
  let body = gzipped(&text);
  for cut in 0..=body.len() {
    let pieces: [&[u8]; 2] = [&body[..cut], &body[cut..]];
    assert_eq!(
      inflate(&pieces, &mut Admission::new(ROOM)),
      Ok(text.clone()),
      "cut at {cut}"
    );
  }
  let bytes: Vec<&[u8]> = body.chunks(1).collect();
  assert_eq!(inflate(&bytes, &mut Admission::new(ROOM)), Ok(text));
}

/// A-595: do end a gzip body at every cut short of its end, and send an empty one. Expect each refused as malformed when
/// it is finished.
#[test]
fn every_body_cut_short_is_refused_at_its_finish() {
  let body = gzipped(b"a short request body, cut everywhere");
  for cut in 0..body.len() {
    let mut gunzip = Gunzip::new();
    let mut admission = Admission::new(ROOM);
    gunzip.feed(&body[..cut], &mut admission).unwrap();
    assert_eq!(
      gunzip.finish(&mut admission),
      Err(GzipRefused::Malformed),
      "cut at {cut}"
    );
  }
}

/// A-595: do inflate sixteen mebibytes of zeros (sixteen kibibytes gzipped) under a room of one mebibyte. Expect it
/// refused past the room, no more than the room made, charged or held.
#[test]
fn a_gzip_bomb_stops_at_the_room() {
  let room = 1 << 20;
  let bomb = gzipped(&vec![0; 16 << 20]);
  assert!(bomb.len() < 64 << 10, "the bomb is {} bytes", bomb.len());
  let mut gunzip = Gunzip::new();
  let mut admission = Admission::new(room);
  let mut made = 0usize;
  let mut refusal = None;
  for piece in bomb.chunks(4096) {
    match gunzip.feed(piece, &mut admission) {
      Ok(()) => made += gunzip.take().len(),
      Err(refused) => {
        refusal = Some(refused);
        break;
      }
    }
  }
  made += gunzip.take().len();
  let Some(GzipRefused::TooLarge(PastAdmission { asked, left })) = refusal else {
    panic!("not refused past the room: {refusal:?}");
  };
  assert!(asked > left, "asked {asked}, left {left}");
  assert!(to_u64(made) <= room, "made {made}");
  assert!(admission.left() <= room);
  assert_eq!(admission.left() + to_u64(made), room);
  assert_eq!(gunzip.feed(b"", &mut admission), Err(GzipRefused::Ended));
}

/// A-595: do inflate a foreign magic, a body whose CRC-32 or length disagrees with its data, a corrupt deflate stream,
/// and a member followed by bytes that start no member. Expect each refused as malformed, at the feed or at the finish.
#[test]
fn malformed_gzip_is_refused() {
  let body = gzipped(b"the request's data");
  let crc = body.len() - 8;
  let length = body.len() - 4;
  let mut cases: Vec<Vec<u8>> = vec![b"PK\x03\x04 not gzip at all".to_vec()];
  for at in [crc, length, 10] {
    let mut flipped = body.clone();
    flipped[at] ^= 0xff;
    cases.push(flipped);
  }
  let mut trailing = body.clone();
  trailing.extend_from_slice(b"garbage after the member");
  cases.push(trailing);
  for case in cases {
    assert_eq!(
      inflate(&[&case], &mut Admission::new(ROOM)),
      Err(GzipRefused::Malformed),
      "{case:?}"
    );
  }
}

/// A-595: do inflate two members one after the other (RFC 1952 §2.2: "a series of members"). Expect both members'
/// bytes in order.
#[test]
fn every_member_inflates_in_turn() {
  let mut body = gzipped(b"first member, ");
  body.extend_from_slice(&gzipped(b"second member"));
  assert_eq!(
    inflate(&[&body], &mut Admission::new(ROOM)),
    Ok(b"first member, second member".to_vec())
  );
}

/// A-595: do inflate a member whose header carries a name, a comment, an extra field and a time. Expect its bytes.
#[test]
fn a_header_with_its_optional_fields_inflates() {
  let mut encoder = GzBuilder::new()
    .filename("request.bin")
    .comment("written by a test")
    .extra(vec![1, 2, 3, 4])
    .mtime(1_112_911_993)
    .write(Vec::new(), Compression::best());
  encoder.write_all(b"with a full header").unwrap();
  let body = encoder.finish().unwrap();
  assert_eq!(
    inflate(&[&body], &mut Admission::new(ROOM)),
    Ok(b"with a full header".to_vec())
  );
}

/// A-595: do feed and finish an inflater after it finished, and after a malformed feed. Expect each refused as ended.
#[test]
fn an_inflater_fed_after_its_end_or_a_refusal_is_refused() {
  let mut admission = Admission::new(ROOM);
  let mut gunzip = Gunzip::new();
  gunzip.feed(&gzipped(b"x"), &mut admission).unwrap();
  gunzip.finish(&mut admission).unwrap();
  assert_eq!(gunzip.take(), b"x");
  assert_eq!(
    gunzip.feed(b"more", &mut admission),
    Err(GzipRefused::Ended)
  );
  assert_eq!(gunzip.finish(&mut admission), Err(GzipRefused::Ended));
  let mut gunzip = Gunzip::new();
  assert_eq!(
    gunzip.feed(b"not gzip, not at all", &mut admission),
    Err(GzipRefused::Malformed)
  );
  assert_eq!(
    gunzip.feed(&gzipped(b"x"), &mut admission),
    Err(GzipRefused::Ended)
  );
}

/// A-595: do ask each refusal's status. Expect 413 past the room and 400 for the rest.
#[test]
fn each_refusal_has_its_status() {
  assert_eq!(
    GzipRefused::TooLarge(PastAdmission { asked: 2, left: 1 }).status(),
    Status::CONTENT_TOO_LARGE
  );
  assert_eq!(GzipRefused::Malformed.status(), Status::BAD_REQUEST);
  assert_eq!(GzipRefused::Ended.status(), Status::BAD_REQUEST);
}
