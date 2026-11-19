//! Golden tests of the part payloads (A-545): every payload of the bundles stock Mercurial 7.2 made — a getbundle's
//! (A-514's fixtures) and three pushes' captured on the wire — and two obsstores 7.2 wrote, each decoded to what 7.2's
//! listings, the logs and the server's state say, and each encoded back to its own bytes.

use crate::bundle2::{Bundle2, Part};
use crate::fixtures::{self, node, payloads};
use crate::obsmarkers::{Format, Marker};
use crate::phases::{Phase, PhaseHead};
use crate::{Admission, Node, bookmarks, heads, obsmarkers, phases, pushkey};

/// The base, public on the server before the pushes (client-log-2.txt).
fn base() -> Node {
  node("ad120869acf0c6c266f1293ca5da567d8b382f1d")
}

/// The draft the client amended, obsolete after the amend.
fn feature() -> Node {
  node("86d599a1904caf7869263a4d824bcb40473ed8fc")
}

/// The amended draft, pushed by the first push and made public by the second.
fn amended() -> Node {
  node("976eb9d7cd11eb8a63d94860fb7186069eef4a10")
}

/// The commit the third push brought.
fn strict() -> Node {
  node("3f9da5ae4bf7758fc91c8a56cf9def051a891e3b")
}

fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// The payload of every top-level part named `kind`, in order.
fn payloads_of(stream: &[u8], kind: &[u8]) -> Vec<Vec<u8>> {
  let decoded = Bundle2::decode(stream, &mut room()).unwrap();
  decoded
    .top_level()
    .filter(|(_, part)| part.is(kind))
    .map(|(index, _)| decoded.payload_bytes(index, &mut room()).unwrap())
    .collect()
}

/// The one payload of the part named `kind`.
fn payload_of(stream: &[u8], kind: &[u8]) -> Vec<u8> {
  let mut all = payloads_of(stream, kind);
  assert_eq!(all.len(), 1, "one {} part", String::from_utf8_lossy(kind));
  all.remove(0)
}

/// A bookmarks payload's entries as (name, node).
fn bookmark_entries(payload: &[u8]) -> Vec<(String, Option<Node>)> {
  bookmarks::decode(payload)
    .unwrap()
    .map(|entry| (String::from_utf8(entry.name.to_vec()).unwrap(), entry.node))
    .collect()
}

/// A bookmarks payload decoded and encoded again: its bytes back.
fn bookmarks_round_trip(payload: &[u8]) {
  let entries: Vec<_> = bookmarks::decode(payload).unwrap().collect();
  let mut out = Vec::new();
  bookmarks::encode(&entries, &mut out).unwrap();
  assert_eq!(out, payload);
}

/// A phases payload decoded and encoded again: its bytes back.
fn phases_round_trip(payload: &[u8]) -> Vec<PhaseHead> {
  let entries: Vec<_> = phases::decode(payload).unwrap().collect();
  let mut out = Vec::new();
  phases::encode(&entries, &mut out);
  assert_eq!(out, payload);
  entries
}

/// A heads payload decoded and encoded again: its bytes back.
fn heads_round_trip(payload: &[u8]) -> Vec<Node> {
  let nodes: Vec<_> = heads::decode(payload).unwrap().collect();
  let mut out = Vec::new();
  heads::encode(&nodes, &mut out);
  assert_eq!(out, payload);
  nodes
}

/// A marker payload decoded and encoded again in its format: its bytes back.
fn markers_round_trip(payload: &[u8]) -> (Format, Vec<Marker<'_>>) {
  let markers = obsmarkers::decode(payload).unwrap();
  let list: Vec<_> = markers.iter().collect();
  assert_eq!(list.len(), markers.len());
  let mut out = Vec::new();
  obsmarkers::encode(markers.format(), &list, &mut out).unwrap();
  assert_eq!(out, payload);
  (markers.format(), list)
}

/// A marker's metadata as text pairs.
fn metadata(marker: &Marker<'_>) -> Vec<(String, String)> {
  marker
    .entries()
    .map(|(key, value)| {
      (
        String::from_utf8(key.to_vec()).unwrap(),
        String::from_utf8(value.to_vec()).unwrap(),
      )
    })
    .collect()
}

/// The nodes of a marker field, as nodes.
fn nodes(chunks: core::slice::ChunksExact<'_, u8>) -> Vec<Node> {
  chunks.map(|chunk| Node::try_from(chunk).unwrap()).collect()
}

/// The lines of a listing's part `kind` that follow its part line, to the next part line.
fn listed_under(listing: &str, kind: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut inside = false;
  for line in listing.lines() {
    if line.contains(" -- ") {
      inside = line.starts_with(&format!("{kind} -- "));
    } else if inside {
      out.push(line.trim().to_string());
    }
  }
  out
}

/// A-545 (golden): do decode the `phase-heads` and `obsmarkers` payloads of the getbundle 7.2 wrote (A-514's
/// changegroup 02 fixture). Expect the heads 7.2's listing gives each phase, in its order, and the one marker it lists
/// (the amend: predecessor, successor, flags 0, the epoch and its metadata); each payload encoded back to its bytes.
#[test]
fn a_getbundles_phases_and_markers_read_as_mercurial_lists_them() {
  let phase_heads = phases_round_trip(&payload_of(fixtures::CG02, b"phase-heads"));
  let listed: Vec<String> = phase_heads
    .iter()
    .map(|entry| format!("{} {}", fixtures::hex(&entry.node), entry.phase.name()))
    .collect();
  assert_eq!(listed, listed_under(fixtures::CG02_LISTING, "phase-heads"));
  let payload = payload_of(fixtures::CG02, b"obsmarkers");
  let (format, markers) = markers_round_trip(&payload);
  assert_eq!(format, Format::V1);
  let listing = listed_under(fixtures::CG02_LISTING, "obsmarkers");
  assert_eq!(listing[0], format!("version: 1 ({} bytes)", payload.len()));
  let [marker] = markers.as_slice() else {
    panic!("one marker")
  };
  assert_eq!(
    listing[1],
    format!(
      "{} {} 0 (Thu Jan 01 00:00:00 1970 +0000) {{'ef1': '9', 'operation': 'amend', 'user': 'test'}}",
      fixtures::hex(&marker.predecessor.try_into().unwrap()),
      fixtures::hex(&marker.successors.try_into().unwrap())
    )
  );
  assert_eq!(
    marker
      .date
      .map(|date| (date.seconds(), date.offset_minutes)),
    Some((0.0, 0))
  );
  assert_eq!(marker.parents, None);
}

/// A-545 (golden): do decode every payload of the modern push 7.2's client sent. Expect check:bookmarks to name the
/// pushed bookmark as absent (the server had only `main`), check:phases and check:updated-heads to name the server's
/// one public head, `bookmarks` to set `feature` to the amended draft, and the amend's marker; each payload encoded back
/// to its bytes.
#[test]
fn the_modern_push_names_what_the_client_saw_and_what_it_moves() {
  let stream = payloads::PUSH_MODERN;
  assert_eq!(
    payloads::SERVER_BOOKMARKS_BEFORE,
    format!("main\t{}\n", fixtures::hex(&base()))
  );
  let check_bookmarks = payload_of(stream, b"check:bookmarks");
  assert_eq!(
    bookmark_entries(&check_bookmarks),
    [("feature".to_string(), None)]
  );
  bookmarks_round_trip(&check_bookmarks);
  let moved = payload_of(stream, b"bookmarks");
  assert_eq!(
    bookmark_entries(&moved),
    [("feature".to_string(), Some(amended()))]
  );
  bookmarks_round_trip(&moved);
  let seen = phases_round_trip(&payload_of(stream, b"check:phases"));
  assert_eq!(
    seen,
    [PhaseHead {
      phase: Phase::Public,
      node: base()
    }]
  );
  assert_eq!(
    heads_round_trip(&payload_of(stream, b"check:updated-heads")),
    [base()]
  );
  let (_, markers) = markers_round_trip(&payload_of(stream, b"obsmarkers"));
  let [marker] = markers.as_slice() else {
    panic!("one marker")
  };
  assert_eq!(marker.predecessor, &feature()[..]);
  assert_eq!(nodes(marker.successor_nodes()), [amended()]);
  assert_eq!(
    metadata(marker),
    [("ef1", "9"), ("operation", "amend"), ("user", "test")]
      .map(|(k, v)| (k.to_string(), v.to_string()))
  );
}

/// A-545 (golden): do decode the legacy push's pushkey parts (`devel.legacy.exchange=bookmarks,phases`) and rebuild
/// each with `pushkey::part`. Expect the requests 7.2's listing shows (the amended draft's phase from 1 to 0; the new
/// bookmark `other` from nothing to it), each part rebuilt equal to the one sent, its stream encoded back to its bytes,
/// and check:phases naming the server's public head and the draft root it then held.
#[test]
fn the_legacy_push_moves_a_bookmark_and_a_phase_by_pushkey() {
  let stream = Bundle2::decode(payloads::PUSH_LEGACY, &mut room()).unwrap();
  let parts: Vec<&Part<'_>> = stream
    .top_level()
    .map(|(_, part)| part)
    .filter(|part| part.is(pushkey::PART))
    .collect();
  let requests: Vec<pushkey::Request<'_>> = parts
    .iter()
    .map(|part| pushkey::request(part).unwrap())
    .collect();
  let hex = fixtures::hex(&amended());
  let expected = [
    pushkey::Request {
      namespace: b"phases",
      key: hex.as_bytes(),
      old: b"1",
      new: b"0",
    },
    pushkey::Request {
      namespace: b"bookmarks",
      key: b"other",
      old: b"",
      new: hex.as_bytes(),
    },
  ];
  assert_eq!(requests, expected);
  for (part, request) in parts.iter().zip(&requests) {
    assert_eq!(&&pushkey::part(part.id, request), part);
  }
  assert!(payloads::PUSH_LEGACY_LISTING.contains(&format!(
    "pushkey -- {{key: other, namespace: bookmarks, new: {hex}, old: }} (mandatory: True)"
  )));
  assert!(payloads::PUSH_LEGACY_LISTING.contains(&format!(
    "pushkey -- {{key: {hex}, namespace: phases, new: 0, old: 1}} (mandatory: True)"
  )));
  assert_eq!(pushkey::hex_node(requests[1].new), Ok(Some(amended())));
  let mut out = Vec::new();
  stream.encode(&mut out).unwrap();
  assert_eq!(out, payloads::PUSH_LEGACY);
  assert_eq!(
    bookmark_entries(&payload_of(payloads::PUSH_LEGACY, b"check:bookmarks")),
    [("other".to_string(), None)]
  );
  let seen = phases_round_trip(&payload_of(payloads::PUSH_LEGACY, b"check:phases"));
  assert_eq!(
    seen,
    [
      PhaseHead {
        phase: Phase::Public,
        node: base()
      },
      PhaseHead {
        phase: Phase::Draft,
        node: amended()
      }
    ]
  );
}

/// A-545 (golden): do decode the push to the strict server. Expect check:heads naming every head the server had (the
/// amended changeset, public by then), check:bookmarks the value `feature` had there, and `bookmarks` moving it to the
/// new commit.
#[test]
fn the_strict_push_names_every_head() {
  let stream = payloads::PUSH_STRICT;
  assert!(payloads_of(stream, b"check:updated-heads").is_empty());
  assert_eq!(
    heads_round_trip(&payload_of(stream, b"check:heads")),
    [amended()]
  );
  assert_eq!(
    bookmark_entries(&payload_of(stream, b"check:bookmarks")),
    [("feature".to_string(), Some(amended()))]
  );
  assert_eq!(
    bookmark_entries(&payload_of(stream, b"bookmarks")),
    [("feature".to_string(), Some(strict()))]
  );
  assert!(payloads::CLIENT_LOG.contains(&format!("{} draft strict", fixtures::hex(&strict()))));
}

/// A-545 (golden): do read the server's replies. Expect each `reply:pushkey` to answer its part's id with 1, and the
/// changegroup's and markers' replies to carry the parameters 7.2's listing shows.
#[test]
fn the_servers_replies_answer_the_parts_they_reply_to() {
  let legacy = Bundle2::decode(payloads::PUSH_LEGACY_REPLY, &mut room()).unwrap();
  let replies: Vec<(u32, i64)> = legacy
    .top_level()
    .filter(|(_, part)| part.is(pushkey::REPLY_PART))
    .map(|(_, part)| {
      pushkey::reply(part)
        .map(|reply| (reply.in_reply_to, reply.result))
        .unwrap()
    })
    .collect();
  assert_eq!(replies, [(3, 1), (5, 1)]);
  assert!(
    payloads::PUSH_LEGACY_REPLY_LISTING.contains("reply:pushkey -- {in-reply-to: 3, return: 1}")
  );
  let modern = Bundle2::decode(payloads::PUSH_MODERN_REPLY, &mut room()).unwrap();
  let (_, changegroup) = modern
    .top_level()
    .find(|(_, part)| part.is(b"reply:changegroup"))
    .unwrap();
  assert_eq!(
    (
      changegroup.param(b"return"),
      changegroup.param(b"in-reply-to")
    ),
    (Some(&b"1"[..]), Some(&b"4"[..]))
  );
  assert!(!changegroup.is_mandatory());
  assert!(
    payloads::PUSH_MODERN_REPLY_LISTING
      .contains("reply:changegroup -- {in-reply-to: 4, return: 1} (mandatory: False)")
  );
}

/// A-545 (golden): do decode the two obsstores 7.2 wrote of the same three markers, in format 0 and format 1. Expect
/// both to hold what 7.2's `debugobsolete` lists (the listings are the same bytes): the amend, the prune with its
/// parent, and the split with its own date and user; format 1 carrying the dates and the prune's parent in fields,
/// format 0 in its metadata; each obsstore encoded back to its bytes.
#[test]
fn both_obsstore_formats_hold_the_markers_mercurial_lists() {
  assert_eq!(payloads::OBSSTORE_LISTING, payloads::OBSSTORE_V0_LISTING);
  let listed: Vec<Vec<&str>> = payloads::OBSSTORE_LISTING
    .lines()
    .map(|line| line.split(' ').collect())
    .collect();
  let parent = node("42b391ca8218bd405e60ffbff1ba140f42dbd2e3");
  for (bytes, format) in [
    (payloads::OBSSTORE_V0, Format::V0),
    (payloads::OBSSTORE_V1, Format::V1),
  ] {
    let (decoded, markers) = markers_round_trip(bytes);
    assert_eq!(decoded, format);
    assert_eq!(markers.len(), listed.len());
    for (marker, line) in markers.iter().zip(&listed) {
      assert_eq!(
        fixtures::hex(&marker.predecessor.try_into().unwrap()),
        line[0]
      );
      let successors: Vec<String> = marker
        .successor_nodes()
        .map(|n| fixtures::hex(&n.try_into().unwrap()))
        .collect();
      assert_eq!(successors, line[1..=successors.len()]);
      assert_eq!(line[successors.len() + 1], marker.flags.to_string());
    }
    let pairs: Vec<Vec<(String, String)>> = markers.iter().map(metadata).collect();
    let has = |marker: usize, key: &str, value: &str| {
      pairs[marker].contains(&(key.to_string(), value.to_string()))
    };
    assert!(has(0, "operation", "amend") && has(1, "user", "test") && has(2, "user", "alice"));
    match format {
      Format::V1 => {
        assert_eq!(markers[1].parent_nodes().map(nodes), Some(vec![parent]));
        assert_eq!(markers[0].parent_nodes().map(nodes), None);
        assert_eq!(
          markers[2]
            .date
            .map(|date| (date.seconds(), date.offset_minutes)),
          Some((1_234_567_890.0, -60))
        );
      }
      Format::V0 => {
        assert!(has(1, "p1", &fixtures::hex(&parent)));
        let date = pairs[2]
          .iter()
          .find(|(key, _)| key == "date")
          .map(|(_, value)| value.clone())
          .unwrap();
        assert!(
          date.starts_with("1234567890") && date.ends_with(" -3600"),
          "{date}"
        );
        assert!(
          markers
            .iter()
            .all(|marker| marker.date.is_none() && marker.parents.is_none())
        );
      }
    }
  }
  assert!(
    payloads::OBSSTORE_LISTING.contains("(Sat Feb 14 00:31:30 2009 +0100) {'user': 'alice'}")
  );
}

/// AC-HG.9 (hostile input), A-545: do cut each obsstore at every length short of its own, and flip every bit of the
/// format 1 one. Expect a cut refused unless it falls between markers (then the markers before it), and a flipped
/// stream refused or read to markers that encode back to exactly its bytes; never a panic.
#[test]
fn cut_and_flipped_obsstores_are_refused_or_read_exactly() {
  for bytes in [payloads::OBSSTORE_V0, payloads::OBSSTORE_V1] {
    let whole: Vec<_> = obsmarkers::decode(bytes).unwrap().iter().collect();
    for cut in 0..bytes.len() {
      if let Ok(prefix) = obsmarkers::decode(&bytes[..cut]) {
        let read: Vec<_> = prefix.iter().collect();
        assert_eq!(read[..], whole[..read.len()], "cut at {cut}");
      }
    }
  }
  let mut refused = 0;
  for at in 0..payloads::OBSSTORE_V1.len() {
    for bit in 0..8 {
      let mut flipped = payloads::OBSSTORE_V1.to_vec();
      flipped[at] ^= 1 << bit;
      match obsmarkers::decode(&flipped) {
        Ok(markers) => {
          let list: Vec<_> = markers.iter().collect();
          let mut out = Vec::new();
          obsmarkers::encode(markers.format(), &list, &mut out).unwrap();
          assert_eq!(out, flipped, "flip of bit {bit} at {at}");
        }
        Err(_) => refused += 1,
      }
    }
  }
  assert!(refused > 0);
}
