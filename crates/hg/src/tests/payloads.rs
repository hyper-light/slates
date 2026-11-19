//! Golden tests of the part payloads (A-545): every payload of the bundles stock Mercurial 7.2 made — a getbundle's
//! (A-514's fixtures) and three pushes' captured on the wire — and two obsstores 7.2 wrote, each decoded to what 7.2's
//! listings, the logs and the server's state say, and each encoded back to its own bytes at the length the encoder
//! gave before it wrote.

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

/// The changeset the obsstores' prune marker records as its predecessor's parent.
fn pruned_parent() -> Node {
  node("42b391ca8218bd405e60ffbff1ba140f42dbd2e3")
}

fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// A length as the `u64` the encoders report.
fn len_of(bytes: &[u8]) -> u64 {
  u64::try_from(bytes.len()).unwrap()
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

/// A bookmarks payload decoded and encoded again: its bytes back, at the length `encoded_len` gave.
fn bookmarks_round_trip(payload: &[u8]) {
  let entries: Vec<_> = bookmarks::decode(payload).unwrap().collect();
  assert_eq!(bookmarks::encoded_len(&entries), Ok(len_of(payload)));
  let mut out = Vec::new();
  bookmarks::encode(&entries, &mut out).unwrap();
  assert_eq!(out, payload);
}

/// A phases payload decoded and encoded again: its bytes back, at the length `encoded_len` gave.
fn phases_round_trip(payload: &[u8]) -> Vec<PhaseHead> {
  let entries: Vec<_> = phases::decode(payload).unwrap().collect();
  assert_eq!(phases::encoded_len(&entries), len_of(payload));
  let mut out = Vec::new();
  phases::encode(&entries, &mut out);
  assert_eq!(out, payload);
  entries
}

/// A heads payload decoded and encoded again: its bytes back, at the length `encoded_len` gave.
fn heads_round_trip(payload: &[u8]) -> Vec<Node> {
  let nodes: Vec<_> = heads::decode(payload).unwrap().collect();
  assert_eq!(heads::encoded_len(&nodes), len_of(payload));
  let mut out = Vec::new();
  heads::encode(&nodes, &mut out);
  assert_eq!(out, payload);
  nodes
}

/// A marker payload decoded and encoded again in its format: its bytes back, at the length `encoded_len` gave.
fn markers_round_trip(payload: &[u8]) -> (Format, Vec<Marker<'_>>) {
  let markers = obsmarkers::decode(payload).unwrap();
  let list: Vec<_> = markers.iter().collect();
  assert_eq!(list.len(), markers.len());
  assert_eq!(
    obsmarkers::encoded_len(markers.format(), &list),
    Ok(len_of(payload))
  );
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

/// Whether a marker's metadata holds `key` with `value`.
fn has(marker: &Marker<'_>, key: &str, value: &str) -> bool {
  metadata(marker).contains(&(key.to_string(), value.to_string()))
}

/// The nodes of a marker field, as nodes.
fn nodes(chunks: core::slice::ChunksExact<'_, u8>) -> Vec<Node> {
  chunks.map(|chunk| Node::try_from(chunk).unwrap()).collect()
}

/// A marker's nodes as hexadecimal, as 7.2's `debugobsolete` begins its line: the predecessor, the successors.
fn hex_nodes(marker: &Marker<'_>) -> Vec<String> {
  let predecessor = Node::try_from(marker.predecessor).unwrap();
  let successors = nodes(marker.successor_nodes());
  core::iter::once(predecessor)
    .chain(successors)
    .map(|node| fixtures::hex(&node))
    .collect()
}

/// Holds each marker to its line of 7.2's `debugobsolete`: its predecessor, its successors, then its flags.
fn assert_listed(markers: &[Marker<'_>], listing: &str) {
  let lines: Vec<Vec<&str>> = listing
    .lines()
    .map(|line| line.split(' ').collect())
    .collect();
  assert_eq!(markers.len(), lines.len());
  for (marker, line) in markers.iter().zip(&lines) {
    let hex = hex_nodes(marker);
    assert_eq!(hex[..], line[..hex.len()]);
    assert_eq!(line[hex.len()], marker.flags.to_string());
  }
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

/// The pushkey parts of a stream, in order.
fn pushkey_parts<'a>(stream: &'a Bundle2<'a>) -> Vec<&'a Part<'a>> {
  stream
    .top_level()
    .map(|(_, part)| part)
    .filter(|part| part.is(pushkey::PART))
    .collect()
}

/// A-545 (golden): do decode the `phase-heads` payload of the getbundle 7.2 wrote (A-514's changegroup 02 fixture).
/// Expect the heads 7.2's listing gives each phase, in its order, and the payload encoded back to its bytes.
#[test]
fn a_getbundles_phase_heads_read_as_mercurial_lists_them() {
  let phase_heads = phases_round_trip(&payload_of(fixtures::CG02, b"phase-heads"));
  let listed: Vec<String> = phase_heads
    .iter()
    .map(|entry| format!("{} {}", fixtures::hex(&entry.node), entry.phase.name()))
    .collect();
  assert_eq!(listed, listed_under(fixtures::CG02_LISTING, "phase-heads"));
}

/// A-545 (golden): do decode the `obsmarkers` payload of the same getbundle. Expect format 1 and the one marker 7.2's
/// listing gives (the amend: predecessor, successor, flags 0, the epoch and its metadata), with no parent data, and
/// the payload encoded back to its bytes.
#[test]
fn a_getbundles_markers_read_as_mercurial_lists_them() {
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
      "{} 0 (Thu Jan 01 00:00:00 1970 +0000) {{'ef1': '9', 'operation': 'amend', 'user': 'test'}}",
      hex_nodes(marker).join(" ")
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

/// A-545 (golden): do decode the check parts and the bookmarks of the modern push 7.2's client sent. Expect
/// check:bookmarks to name the pushed bookmark as absent (the server had only `main`), check:phases and
/// check:updated-heads to name the server's one public head, and `bookmarks` to set `feature` to the amended draft;
/// each payload encoded back to its bytes.
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
}

/// A-545 (golden): do decode the modern push's `obsmarkers` payload. Expect the amend's one marker: the draft the
/// client amended, the amended draft as its successor, and 7.2's metadata, the payload encoded back to its bytes.
#[test]
fn the_modern_push_carries_the_amends_marker() {
  let payload = payload_of(payloads::PUSH_MODERN, b"obsmarkers");
  let (_, markers) = markers_round_trip(&payload);
  let [marker] = markers.as_slice() else {
    panic!("one marker")
  };
  assert_eq!(marker.predecessor, &feature()[..]);
  assert_eq!(nodes(marker.successor_nodes()), [amended()]);
  assert_eq!(
    metadata(marker),
    [("ef1", "9"), ("operation", "amend"), ("user", "test")]
      .map(|(key, value)| (key.to_string(), value.to_string()))
  );
}

/// A-545 (golden): do decode the legacy push's pushkey parts (`devel.legacy.exchange=bookmarks,phases`) and rebuild
/// each with `pushkey::part`. Expect the requests 7.2's listing shows (the amended draft's phase from 1 to 0; the new
/// bookmark `other` from nothing to it), and each part rebuilt equal to the one sent.
#[test]
fn the_legacy_push_moves_a_bookmark_and_a_phase_by_pushkey() {
  let stream = Bundle2::decode(payloads::PUSH_LEGACY, &mut room()).unwrap();
  let parts = pushkey_parts(&stream);
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
  assert_eq!(pushkey::hex_node(requests[1].new), Ok(Some(amended())));
}

/// A-545 (golden): do look the legacy push's pushkey parts up in 7.2's listing, and read its check parts. Expect both
/// parts listed as sent, check:bookmarks naming `other` as absent, and check:phases naming the server's public head
/// and the draft root it then held.
#[test]
fn the_legacy_push_is_listed_and_checks_what_the_client_saw() {
  let hex = fixtures::hex(&amended());
  assert!(payloads::PUSH_LEGACY_LISTING.contains(&format!(
    "pushkey -- {{key: other, namespace: bookmarks, new: {hex}, old: }} (mandatory: True)"
  )));
  assert!(payloads::PUSH_LEGACY_LISTING.contains(&format!(
    "pushkey -- {{key: {hex}, namespace: phases, new: 0, old: 1}} (mandatory: True)"
  )));
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
/// amended changeset, public by then) and no check:updated-heads, check:bookmarks the value `feature` had there, and
/// `bookmarks` moving it to the new commit.
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

/// A-545 (golden): do read the server's `reply:pushkey` parts and write each again with `pushkey::reply_part` from
/// the reply it carries. Expect each to answer its pushkey part's id with 1, as 7.2's listing shows, and each written
/// part equal to the one 7.2's server sent, type, id and parameters.
#[test]
fn the_servers_pushkey_replies_are_the_parts_slates_writes() {
  let legacy = Bundle2::decode(payloads::PUSH_LEGACY_REPLY, &mut room()).unwrap();
  let sent: Vec<&Part<'_>> = legacy
    .top_level()
    .map(|(_, part)| part)
    .filter(|part| part.is(pushkey::REPLY_PART))
    .collect();
  let replies: Vec<pushkey::Reply> = sent
    .iter()
    .map(|part| pushkey::reply(part).unwrap())
    .collect();
  let answered: Vec<(u32, i64)> = replies
    .iter()
    .map(|reply| (reply.in_reply_to, reply.result))
    .collect();
  assert_eq!(answered, [(3, 1), (5, 1)]);
  for (part, reply) in sent.iter().zip(&replies) {
    let text = pushkey::ReplyText::new(reply).unwrap();
    assert_eq!(&&pushkey::reply_part(part.id, &text), part);
  }
  assert!(
    payloads::PUSH_LEGACY_REPLY_LISTING.contains("reply:pushkey -- {in-reply-to: 3, return: 1}")
  );
}

/// A-545 (golden): do read the server's reply to the modern push. Expect its `reply:changegroup` advisory, answering
/// the changegroup part (id 4) with 1, as 7.2's listing shows.
#[test]
fn the_servers_changegroup_reply_carries_what_mercurial_lists() {
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

/// A-545 (golden): do decode every stream captured on the wire, the three pushes and the server's three replies, and
/// encode each again. Expect each stream's own bytes back.
#[test]
fn every_captured_stream_encodes_back_to_its_bytes() {
  for stream in [
    payloads::PUSH_MODERN,
    payloads::PUSH_MODERN_REPLY,
    payloads::PUSH_LEGACY,
    payloads::PUSH_LEGACY_REPLY,
    payloads::PUSH_STRICT,
    payloads::PUSH_STRICT_REPLY,
  ] {
    let mut out = Vec::new();
    Bundle2::decode(stream, &mut room())
      .unwrap()
      .encode(&mut out)
      .unwrap();
    assert_eq!(out, stream);
  }
}

/// A-545 (golden): do read the server's answers to the `listkeys phases` 7.2's client sent after each push, as the
/// server wrote them on the wire. Expect the amended draft as the one draft root after the modern push (its value 1,
/// draft's number), none after the legacy push made it public, the strict push's commit after that push, and each
/// payload encoded back to its bytes at the length `listkeys_encoded_len` gave.
#[test]
fn the_servers_listkeys_answers_name_its_draft_roots() {
  for (payload, root) in [
    (payloads::PUSH_MODERN_LISTKEYS_PHASES, Some(amended())),
    (payloads::PUSH_LEGACY_LISTKEYS_PHASES, None),
    (payloads::PUSH_STRICT_LISTKEYS_PHASES, Some(strict())),
  ] {
    let entries: Vec<_> = pushkey::listkeys(payload).unwrap().collect();
    let roots: Vec<Option<Node>> = entries
      .iter()
      .map(|(key, _)| pushkey::hex_node(key).unwrap())
      .collect();
    assert_eq!(roots, root.into_iter().map(Some).collect::<Vec<_>>());
    assert!(entries.iter().all(|(_, value)| *value == b"1"));
    assert_eq!(pushkey::listkeys_encoded_len(&entries), Ok(len_of(payload)));
    let mut out = Vec::new();
    pushkey::encode_listkeys(&entries, &mut out).unwrap();
    assert_eq!(out, payload);
  }
}

/// A-545 (golden): do read 7.2's `debugpushkey` of the server's namespaces and of its bookmarks before the pushes:
/// lines of the form a listkeys answer has, with one more newline at the end, which `splitlines()` drops. Expect the
/// four namespaces 7.2 serves, each with an empty value, and `main` at the base.
#[test]
fn mercurials_namespaces_and_bookmarks_read_as_listkeys_lines() {
  let namespaces: Vec<_> = pushkey::listkeys(payloads::SERVER_NAMESPACES.as_bytes())
    .unwrap()
    .collect();
  let expected: [(&[u8], &[u8]); 4] = [
    (b"bookmarks", b""),
    (b"namespaces", b""),
    (b"obsolete", b""),
    (b"phases", b""),
  ];
  assert_eq!(namespaces, expected);
  let bookmarks: Vec<_> = pushkey::listkeys(payloads::SERVER_BOOKMARKS_BEFORE.as_bytes())
    .unwrap()
    .collect();
  let [(name, value)] = bookmarks.as_slice() else {
    panic!("one bookmark")
  };
  assert_eq!(
    (*name, pushkey::hex_node(value)),
    (&b"main"[..], Ok(Some(base())))
  );
}

/// A-545 (golden): do decode the obsstore 7.2 wrote in format 1. Expect the three markers 7.2's `debugobsolete`
/// lists (the amend, the prune with its parent, the split with its own date and user), the dates and the prune's
/// parent in fields, and the obsstore encoded back to its bytes.
#[test]
fn the_format_1_obsstore_holds_the_markers_mercurial_lists() {
  let (format, markers) = markers_round_trip(payloads::OBSSTORE_V1);
  assert_eq!(format, Format::V1);
  assert_listed(&markers, payloads::OBSSTORE_LISTING);
  assert!(
    has(&markers[0], "operation", "amend")
      && has(&markers[1], "user", "test")
      && has(&markers[2], "user", "alice")
  );
  assert_eq!(
    markers[1].parent_nodes().map(nodes),
    Some(vec![pruned_parent()])
  );
  assert_eq!(markers[0].parent_nodes().map(nodes), None);
  assert_eq!(
    markers[2]
      .date
      .map(|date| (date.seconds(), date.offset_minutes)),
    Some((1_234_567_890.0, -60))
  );
  assert!(
    payloads::OBSSTORE_LISTING.contains("(Sat Feb 14 00:31:30 2009 +0100) {'user': 'alice'}")
  );
}

/// A-545 (golden): do decode the obsstore 7.2 wrote of the same markers in format 0. Expect 7.2's same listing, the
/// same markers, the dates and the prune's parent in their metadata (no field holds them), and the obsstore encoded
/// back to its bytes.
#[test]
fn the_format_0_obsstore_holds_the_same_markers_in_its_metadata() {
  assert_eq!(payloads::OBSSTORE_V0_LISTING, payloads::OBSSTORE_LISTING);
  let (format, markers) = markers_round_trip(payloads::OBSSTORE_V0);
  assert_eq!(format, Format::V0);
  assert_listed(&markers, payloads::OBSSTORE_V0_LISTING);
  assert!(has(&markers[0], "operation", "amend") && has(&markers[2], "user", "alice"));
  assert!(has(&markers[1], "p1", &fixtures::hex(&pruned_parent())));
  let date = metadata(&markers[2])
    .into_iter()
    .find_map(|(key, value)| (key == "date").then_some(value))
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

/// AC-HG.9 (hostile input), A-545: do cut each obsstore at every length short of its own. Expect a cut refused unless
/// it falls between markers, and then exactly the markers before it.
#[test]
fn a_cut_obsstore_is_refused_or_reads_the_markers_before_the_cut() {
  for bytes in [payloads::OBSSTORE_V0, payloads::OBSSTORE_V1] {
    let whole: Vec<_> = obsmarkers::decode(bytes).unwrap().iter().collect();
    for cut in 0..bytes.len() {
      if let Ok(prefix) = obsmarkers::decode(&bytes[..cut]) {
        let read: Vec<_> = prefix.iter().collect();
        assert_eq!(read[..], whole[..read.len()], "cut at {cut}");
      }
    }
  }
}

/// AC-HG.9 (hostile input), A-545: do flip every bit of the format 1 obsstore. Expect each flipped store refused, or
/// read to markers that encode back to exactly its bytes; never a panic, and some refused.
#[test]
fn a_flipped_obsstore_is_refused_or_reads_exactly() {
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
