//! Hostile input: every cut of every vector, bit flips, lengths past the stream, a missing `PROPS-END`, and every
//! refusal the reader makes, each by kind and with Subversion's number.

use proptest::prelude::*;

use super::golden::{reading, vector};
use super::support::{
  Owned, ROOMY, V2, V3, digests_hex, dump, file_add, node, push, read, read_whole, refusal,
  revision, write,
};
use crate::dump::{
  Admission, Contradiction, Digest, DumpError, Header, NodeAction, PropertyFault, Read, Reader,
};
use crate::svndiff::SvndiffError;

/// The vectors whose every cut is tried.
const VECTORS: [&str; 4] = ["full.dump", "deltas.dump", "part1.dump", "part2.dump"];

/// What a reader makes of the stream ending where it stands, with `held` the bytes it has not consumed: the events it
/// still reads, or its refusal. It reads from `held` in place, so trying every cut stays linear in the stream.
fn end_here(reader: &Reader, held: &[u8]) -> Result<Vec<Owned>, DumpError> {
  let mut reader = reader.clone();
  let mut start = 0;
  let mut events = Vec::new();
  loop {
    match reader.read(&held[start..], true)? {
      Read::Event { event, consumed } => {
        push(&mut events, Owned::of(event));
        start += consumed;
      }
      Read::NeedMore { .. } => panic!("asked for more after the stream's end"),
      Read::End => return Ok(events),
    }
  }
}

/// Reads on with the stream open as far as `held` allows, dropping what the reader consumes.
fn read_on(reader: &mut Reader, held: &mut Vec<u8>, events: &mut Vec<Owned>) {
  loop {
    match reader.read(held, false).unwrap() {
      Read::Event { event, consumed } => {
        push(events, Owned::of(event));
        held.drain(..consumed);
      }
      Read::NeedMore { consumed, .. } => {
        held.drain(..consumed);
        return;
      }
      Read::End => panic!("ended with the stream open"),
    }
  }
}

/// Whether `cut`'s events, read so far and then at the stream's end, begin the whole stream's: the same events, but
/// that a deletion the cut parted from its addition is a deletion where the whole stream has the one replacement.
fn begins(read_so_far: &[Owned], at_end: &[Owned], whole: &[Owned]) -> bool {
  let all: Vec<&Owned> = read_so_far.iter().chain(at_end).collect();
  if all.len() > whole.len() {
    return false;
  }
  let split = |cut: &Owned, kept: &Owned| match (cut, kept) {
    (Owned::Node(deleted), Owned::Node(replaced)) => {
      deleted.action == NodeAction::Delete
        && replaced.action == NodeAction::Replace
        && deleted.path == replaced.path
    }
    _ => false,
  };
  all
    .iter()
    .zip(whole)
    .enumerate()
    .all(|(place, (&cut, kept))| cut == kept || (place + 1 == all.len() && split(cut, kept)))
}

/// A-593 (hostile, every cut): do hand each vector to a reader a byte at a time and, at every cut, end the stream
/// there. Expect each cut refused as truncated, or read cleanly to a record boundary with the whole stream's events up
/// to there (a deletion the cut parts from its replacement's addition being the deletion it then is), and no other
/// refusal: a cut never reads as a different history.
#[test]
fn every_cut_of_every_vector_is_refused_as_truncated_or_ends_between_records() {
  for name in VECTORS {
    let dump = vector(name);
    let whole = read_whole(dump).unwrap();
    let records = whole
      .iter()
      .filter(|event| matches!(event, Owned::Revision(_) | Owned::Node(_)))
      .count();
    let mut reader = Reader::new(ROOMY);
    let mut held = Vec::new();
    let mut events = Vec::new();
    let (mut truncated, mut clean) = (0usize, 0usize);
    for cut in 0..=dump.len() {
      read_on(&mut reader, &mut held, &mut events);
      match end_here(&reader, &held) {
        Err(DumpError::Truncated) => truncated += 1,
        Err(other) => panic!("{name} cut at {cut}: {other:?}"),
        Ok(at_end) => {
          clean += 1;
          assert!(
            begins(&events, &at_end, &whole),
            "{name} cut at {cut} reads as another history"
          );
          if cut == dump.len() {
            let mut all = events.clone();
            all.extend(at_end);
            assert_eq!(all, whole, "{name} whole");
          }
        }
      }
      if let Some(&byte) = dump.get(cut) {
        held.push(byte);
      }
    }
    assert!(
      truncated > 0 && clean >= records,
      "{name}: {truncated} truncated, {clean} clean, {records} records"
    );
  }
}

/// Whether byte `at` of a vector read as `spans` lies in a whole text whose digests its header names.
fn in_checked_text(at: usize, spans: &[(usize, usize)]) -> bool {
  spans.iter().any(|&(start, end)| start <= at && at < end)
}

/// Reads `flipped`, the vector `name` with bit `bit` of byte `at` flipped: refused by kind, or read to events the
/// writer writes and the reader reads back as the same events; refused whenever the byte lies in a whole text, every
/// one of which the vectors check by MD5 and SHA-1.
fn judge_flip(name: &str, flipped: &[u8], at: usize, bit: u32, spans: &[(usize, usize)]) -> bool {
  match read_whole(flipped) {
    Err(refusal) => {
      assert!(refusal.subversion_code() > 0, "{refusal:?}");
      false
    }
    Ok(events) => {
      assert!(
        !in_checked_text(at, spans),
        "{name}: bit {bit} of byte {at}, in a checked text, read cleanly"
      );
      let written =
        write(&events).unwrap_or_else(|refusal| panic!("{name} {at}/{bit} written: {refusal:?}"));
      assert_eq!(
        read_whole(&written).unwrap(),
        events,
        "{name}: bit {bit} of byte {at} written and read again"
      );
      true
    }
  }
}

/// A-593 (hostile, bit flips): do flip every bit of every byte of the smallest vector, one at a time, and read each.
/// Expect each refused by kind or read to events the writer writes and the reader reads back unchanged, every flip
/// inside a whole text refused (its MD5 and SHA-1 are checked as it passes), and both outcomes met.
#[test]
fn every_bit_flip_of_the_smallest_vector_is_refused_or_read_consistently() {
  let name = "part1.dump";
  let dump = vector(name);
  let spans = reading(name).text_spans;
  assert!(!spans.is_empty());
  let (mut refused, mut accepted) = (0usize, 0usize);
  let mut flipped = dump.to_vec();
  for at in 0..dump.len() {
    for bit in 0..8 {
      flipped[at] ^= 1 << bit;
      if judge_flip(name, &flipped, at, bit, &spans) {
        accepted += 1;
      } else {
        refused += 1;
      }
      flipped[at] ^= 1 << bit;
    }
  }
  assert!(
    refused > 0 && accepted > 0,
    "{refused} refused, {accepted} accepted"
  );
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-593 (hostile, bit flips): do flip a random bit of a random byte of the larger vectors and read each. Expect the
  /// same as for the smallest vector's every flip.
  #[test]
  fn a_random_bit_flip_of_a_larger_vector_is_refused_or_read_consistently(
    which in 0usize..3, place in any::<prop::sample::Index>(), bit in 0u32..8
  ) {
    let name = LARGER[which];
    let dump = vector(name);
    let at = place.index(dump.len());
    let mut flipped = dump.to_vec();
    flipped[at] ^= 1 << bit;
    judge_flip(name, &flipped, at, bit, &larger_spans()[which]);
  }
}

/// The larger vectors, whose bits are flipped at random.
const LARGER: [&str; 3] = ["full.dump", "deltas.dump", "part2.dump"];

/// Where each larger vector's whole texts lie, read once.
fn larger_spans() -> &'static [Vec<(usize, usize)>; 3] {
  static SPANS: std::sync::OnceLock<[Vec<(usize, usize)>; 3]> = std::sync::OnceLock::new();
  SPANS.get_or_init(|| LARGER.map(|name| reading(name).text_spans))
}

/// The refusal `bytes` meets and its Subversion number.
fn refused(bytes: &[u8]) -> (DumpError, u32) {
  let refusal = refusal(bytes);
  (refusal, refusal.subversion_code())
}

/// A stream of format `version` holding revision 1 and then `records`.
fn in_revision_one(version: &[u8], records: &[&[u8]]) -> Vec<u8> {
  let mut parts: Vec<&[u8]> = vec![version];
  let first = revision(1);
  parts.push(&first);
  parts.extend_from_slice(records);
  dump(&parts)
}

/// A node record with `headers` and no content block.
fn bare(headers: &str) -> Vec<u8> {
  format!("{headers}\n\n").into_bytes()
}

/// `SVN_ERR_STREAM_MALFORMED_DATA`.
const MALFORMED: u32 = 140_001;
/// `SVN_ERR_STREAM_UNRECOGNIZED_DATA`.
const UNRECOGNIZED: u32 = 140_002;
/// `SVN_ERR_INCOMPLETE_DATA`.
const INCOMPLETE: u32 = 200_003;
/// `SVN_ERR_INCORRECT_PARAMS`.
const INCORRECT_PARAMS: u32 = 200_004;
/// `SVN_ERR_CHECKSUM_MISMATCH`.
const CHECKSUM_MISMATCH: u32 = 200_014;
/// `SVN_ERR_BAD_CHECKSUM_PARSE`.
const BAD_CHECKSUM_PARSE: u32 = 125_012;
/// `SVN_ERR_BAD_UUID`.
const BAD_UUID: u32 = 125_008;

/// A-593 (hostile): do read streams whose version record is missing, foreign, malformed, of format 1 or 4, or cut.
/// Expect each refused by kind with Subversion's number; a foreign stream at its first byte.
#[test]
fn a_stream_without_a_version_record_of_format_two_or_three_is_refused() {
  let cases: [(&[u8], DumpError, u32); 7] = [
    (b"", DumpError::Truncated, INCOMPLETE),
    (b"PK\x03\x04", DumpError::MalformedVersion, MALFORMED),
    (
      b"SVN-fs-dump-format-version: 1\n\n",
      DumpError::UnsupportedVersion { version: 1 },
      MALFORMED,
    ),
    (
      b"SVN-fs-dump-format-version: 4\n\n",
      DumpError::UnsupportedVersion { version: 4 },
      MALFORMED,
    ),
    (
      b"SVN-fs-dump-format-version:2\n\n",
      DumpError::MalformedVersion,
      MALFORMED,
    ),
    (
      b"SVN-fs-dump-format-version: 2x\n\n",
      DumpError::MalformedVersion,
      MALFORMED,
    ),
    (
      b"SVN-fs-dump-format-version: 2",
      DumpError::Truncated,
      INCOMPLETE,
    ),
  ];
  for (bytes, expected, code) in cases {
    assert_eq!(
      refused(bytes),
      (expected, code),
      "{:?}",
      String::from_utf8_lossy(bytes)
    );
  }
  let mut reader = Reader::new(ROOMY);
  assert_eq!(
    reader.read(b"P", false),
    Err(DumpError::MalformedVersion),
    "refused at its first byte"
  );
}

/// A-593 (hostile): do read header blocks that are malformed, repeat a header, name no record or two, put a node's
/// header on a revision record, carry a malformed UUID or number, or are cut by the stream's end. Expect each refused
/// by kind with Subversion's number, the cut block where `svnadmin load` takes the end for its blank line.
#[test]
fn a_malformed_header_block_is_refused_by_kind() {
  let cases: [(&[u8], DumpError, u32); 12] = [
    (
      b"Revision-number 1\n\n",
      DumpError::MalformedHeader,
      MALFORMED,
    ),
    (
      b"Revision-number:1\n\n",
      DumpError::MalformedHeader,
      MALFORMED,
    ),
    (b": 1\n\n", DumpError::MalformedHeader, MALFORMED),
    (
      b"Revision-number: 1\x00\n\n",
      DumpError::MalformedHeader,
      MALFORMED,
    ),
    (
      b"Revision-number: 1\nRevision-number: 2\n\n",
      DumpError::DuplicateHeader {
        header: Header::RevisionNumber,
      },
      MALFORMED,
    ),
    (
      b"Revision-number: x\n\n",
      DumpError::MalformedValue {
        header: Header::RevisionNumber,
      },
      MALFORMED,
    ),
    (
      b"Revision-number: 9223372036854775808\n\n",
      DumpError::MalformedValue {
        header: Header::RevisionNumber,
      },
      MALFORMED,
    ),
    (
      b"X-Unknown: y\n\n",
      DumpError::UnrecognizedRecord,
      MALFORMED,
    ),
    (
      b"Revision-number: 1\nNode-path: a\n\n",
      DumpError::Contradiction {
        fault: Contradiction::MixedRecord,
      },
      MALFORMED,
    ),
    (
      b"Revision-number: 1\nNode-kind: file\n\n",
      DumpError::Contradiction {
        fault: Contradiction::HeaderOutOfPlace,
      },
      MALFORMED,
    ),
    (
      b"UUID: not-a-uuid\n\n",
      DumpError::MalformedValue {
        header: Header::Uuid,
      },
      BAD_UUID,
    ),
    (b"Revision-number: 1\n", DumpError::Truncated, INCOMPLETE),
  ];
  for (block, expected, code) in cases {
    assert_eq!(
      refused(&dump(&[V2, block])),
      (expected, code),
      "{:?}",
      String::from_utf8_lossy(block)
    );
  }
}

/// A-593 (hostile): do read node records without an action or with one or a kind the format does not name, before any
/// revision or in revision 0, a deletion with content, a text on a directory, a copy source on a change or half of
/// one, copy and delta checksums without what they check, deltas in format 2, a short `Content-length` or property
/// section, malformed or wrong checksums. Expect each refused by kind with Subversion's number.
#[test]
fn a_node_record_whose_headers_contradict_each_other_or_the_stream_is_refused_by_kind() {
  let contradiction = |fault| (DumpError::Contradiction { fault }, MALFORMED);
  let hex32 = "0123456789abcdef0123456789abcdef";
  let cases: Vec<(Vec<u8>, (DumpError, u32))> = vec![
    (
      in_revision_one(V2, &[&bare("Node-path: a\nNode-kind: file")]),
      (
        DumpError::MissingHeader {
          header: Header::NodeAction,
        },
        UNRECOGNIZED,
      ),
    ),
    (
      in_revision_one(
        V2,
        &[&bare("Node-path: a\nNode-kind: file\nNode-action: move")],
      ),
      (
        DumpError::MalformedValue {
          header: Header::NodeAction,
        },
        UNRECOGNIZED,
      ),
    ),
    (
      in_revision_one(
        V2,
        &[&bare("Node-path: a\nNode-kind: link\nNode-action: add")],
      ),
      (
        DumpError::MalformedValue {
          header: Header::NodeKind,
        },
        UNRECOGNIZED,
      ),
    ),
    (
      in_revision_one(V2, &[&bare("Node-path: a\nNode-action: add")]),
      (
        DumpError::MissingHeader {
          header: Header::NodeKind,
        },
        MALFORMED,
      ),
    ),
    (
      dump(&[V2, &bare("Node-path: a\nNode-action: delete")]),
      contradiction(Contradiction::NodeOutsideRevision),
    ),
    (
      dump(&[V2, &revision(0), &bare("Node-path: a\nNode-action: delete")]),
      contradiction(Contradiction::NodeInRevisionZero),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          "Node-path: a\nNode-action: delete\nProp-content-length: 10\nContent-length: 10\n",
          b"PROPS-END\n",
        )],
      ),
      contradiction(Contradiction::ContentOnDelete),
    ),
    (
      in_revision_one(
        V2,
        &[&bare(
          "Node-path: a\nNode-action: delete\nContent-length: 0",
        )],
      ),
      contradiction(Contradiction::ContentOnDelete),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          "Node-path: a\nNode-kind: dir\nNode-action: add\nText-content-length: 1\nContent-length: 1\n",
          b"x",
        )],
      ),
      contradiction(Contradiction::TextOnDirectory),
    ),
    (
      in_revision_one(
        V2,
        &[&bare(
          "Node-path: a\nNode-kind: dir\nNode-action: change\nNode-copyfrom-rev: 0\nNode-copyfrom-path: b",
        )],
      ),
      contradiction(Contradiction::CopySourceOutOfPlace),
    ),
    (
      in_revision_one(
        V2,
        &[&bare(
          "Node-path: a\nNode-kind: dir\nNode-action: add\nNode-copyfrom-rev: 0",
        )],
      ),
      contradiction(Contradiction::CopySourceIncomplete),
    ),
    (
      in_revision_one(
        V2,
        &[&bare(&format!(
          "Node-path: a\nNode-kind: file\nNode-action: add\nText-copy-source-md5: {hex32}"
        ))],
      ),
      contradiction(Contradiction::CopyChecksumWithoutCopy),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          "Node-path: a\nNode-kind: file\nNode-action: change\nText-delta: true\nText-content-length: 4\nContent-length: 4\n",
          b"SVN\x00",
        )],
      ),
      contradiction(Contradiction::DeltaInFormatTwo),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          "Node-path: a\nNode-kind: file\nNode-action: change\nProp-delta: true\nProp-content-length: 10\nContent-length: 10\n",
          b"PROPS-END\n",
        )],
      ),
      contradiction(Contradiction::DeltaInFormatTwo),
    ),
    (
      in_revision_one(
        V3,
        &[&bare(
          "Node-path: a\nNode-kind: file\nNode-action: change\nText-delta: maybe",
        )],
      ),
      (
        DumpError::MalformedValue {
          header: Header::TextDelta,
        },
        MALFORMED,
      ),
    ),
    (
      in_revision_one(
        V3,
        &[&bare(
          "Node-path: a\nNode-kind: file\nNode-action: change\nProp-delta: true",
        )],
      ),
      contradiction(Contradiction::DeltaWithoutSection),
    ),
    (
      in_revision_one(
        V3,
        &[&node(
          &format!(
            "Node-path: a\nNode-kind: file\nNode-action: change\nText-delta-base-md5: {hex32}\nText-content-length: 1\nContent-length: 1\n"
          ),
          b"x",
        )],
      ),
      contradiction(Contradiction::DeltaBaseWithoutDelta),
    ),
    (
      in_revision_one(
        V2,
        &[&bare(&format!(
          "Node-path: a\nNode-kind: file\nNode-action: change\nText-content-md5: {hex32}"
        ))],
      ),
      contradiction(Contradiction::TextChecksumWithoutText),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          "Node-path: a\nNode-kind: dir\nNode-action: change\nProp-content-length: 10\nContent-length: 5\n",
          b"PROPS-END\n",
        )],
      ),
      contradiction(Contradiction::ContentLengthShort),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          "Node-path: a\nNode-kind: dir\nNode-action: change\nProp-content-length: 5\nContent-length: 5\n",
          b"PROPS",
        )],
      ),
      contradiction(Contradiction::PropertySectionTooShort),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          "Node-path: a\nNode-kind: file\nNode-action: change\nText-content-md5: xyz\nText-content-length: 1\nContent-length: 1\n",
          b"x",
        )],
      ),
      (
        DumpError::MalformedValue {
          header: Header::TextContentMd5,
        },
        BAD_CHECKSUM_PARSE,
      ),
    ),
    (
      in_revision_one(
        V2,
        &[&node(
          &format!(
            "Node-path: a\nNode-kind: file\nNode-action: change\nText-content-md5: {}\nText-content-length: 1\nContent-length: 1\n",
            &hex32[1..]
          ),
          b"x",
        )],
      ),
      (
        DumpError::MalformedValue {
          header: Header::TextContentMd5,
        },
        BAD_CHECKSUM_PARSE,
      ),
    ),
  ];
  for (bytes, expected) in cases {
    assert_eq!(
      refused(&bytes),
      expected,
      "{:?}",
      String::from_utf8_lossy(&bytes)
    );
  }
}

/// A-593 (hostile, checksums): do read a whole text whose MD5 is another text's, and one whose MD5 is right and SHA-1
/// another's. Expect each refused as a checksum mismatch naming both digests; then an all-zero MD5, which names none,
/// and an uppercase one, both read.
#[test]
fn a_whole_text_whose_digests_are_another_s_is_refused() {
  let (right_md5, right_sha1) = digests_hex(b"text");
  let (wrong_md5, wrong_sha1) = digests_hex(b"other");
  let record = |md5: &str, sha1: &str| {
    node(
      &format!(
        "Node-path: a\nNode-kind: file\nNode-action: change\nText-content-md5: {md5}\nText-content-sha1: {sha1}\n\
         Text-content-length: 4\nContent-length: 4\n"
      ),
      b"text",
    )
  };
  let (refusal, code) = refused(&in_revision_one(V2, &[&record(&wrong_md5, &right_sha1)]));
  assert_eq!(code, CHECKSUM_MISMATCH);
  assert!(matches!(
    refusal,
    DumpError::ChecksumMismatch {
      expected: Digest::Md5(_),
      actual: Digest::Md5(_)
    }
  ));
  let (refusal, _) = refused(&in_revision_one(V2, &[&record(&right_md5, &wrong_sha1)]));
  assert!(matches!(
    refusal,
    DumpError::ChecksumMismatch {
      expected: Digest::Sha1(_),
      actual: Digest::Sha1(_)
    }
  ));
  let zeros = "0".repeat(32);
  assert!(
    read_whole(&in_revision_one(V2, &[&record(&zeros, &right_sha1)])).is_ok(),
    "all zeros names no digest"
  );
  let upper = right_md5.to_uppercase();
  assert!(
    read_whole(&in_revision_one(V2, &[&record(&upper, &right_sha1)])).is_ok(),
    "uppercase hex"
  );
}

/// A revision record whose property section is `section`, its length stated as `length`.
fn revision_with(section: &[u8], length: usize) -> Vec<u8> {
  let mut bytes =
    format!("Revision-number: 1\nProp-content-length: {length}\nContent-length: {length}\n\n")
      .into_bytes();
  bytes.extend_from_slice(section);
  bytes.extend_from_slice(b"\n");
  bytes
}

/// A-593 (hostile, property sections): do read sections without `PROPS-END` whose entries fill the stated length
/// exactly (which `svnadmin load` reads), with `PROPS-END` before the length runs out, with an entry past the length,
/// with lines other than entries, lengths that are not digits, a name not followed by a line feed, a deletion in a
/// revision's properties or in a section that is no delta, and a name holding a NUL. Expect each refused by kind.
#[test]
fn a_property_section_that_does_not_hold_together_is_refused_by_kind() {
  let fault = |fault| DumpError::Property { fault };
  let entries = b"K 1\na\nV 1\nb\n";
  let cases: Vec<(Vec<u8>, DumpError)> = vec![
    (
      revision_with(entries, entries.len()),
      fault(PropertyFault::Unterminated),
    ),
    (
      revision_with(b"PROPS-END\nK 1\na\nV 1\nb\n", 22),
      fault(PropertyFault::EndsEarly),
    ),
    (
      revision_with(b"K 100\na\nV 1\nb\nPROPS-END\n", 23),
      fault(PropertyFault::PastSection),
    ),
    (
      revision_with(b"X 1\na\nPROPS-END\n", 16),
      fault(PropertyFault::MalformedLine),
    ),
    (
      revision_with(b"K x\na\nPROPS-END\n", 16),
      fault(PropertyFault::MalformedLength),
    ),
    (
      revision_with(b"K 1\na\nX 1\nb\nPROPS-END\n", 22),
      fault(PropertyFault::MalformedLine),
    ),
    (
      revision_with(b"K 1\naXV 1\nb\nPROPS-END\n", 22),
      fault(PropertyFault::MalformedData),
    ),
    (
      revision_with(b"D 1\na\nPROPS-END\n", 16),
      fault(PropertyFault::DeletionOutOfPlace),
    ),
    (
      revision_with(b"K 3\na\x00b\nV 1\nb\nPROPS-END\n", 24),
      fault(PropertyFault::NameNul),
    ),
  ];
  for (record, expected) in cases {
    let bytes = dump(&[V2, &record]);
    assert_eq!(
      refusal(&bytes),
      expected,
      "{:?}",
      String::from_utf8_lossy(&record)
    );
  }
  assert_eq!(
    refusal(&dump(&[V2, &revision_with(b"K x\na\nPROPS-END\n", 16)])).subversion_code(),
    INCORRECT_PARAMS
  );
  let deletion = node(
    "Node-path: a\nNode-kind: dir\nNode-action: change\nProp-content-length: 16\nContent-length: 16\n",
    b"D 1\na\nPROPS-END\n",
  );
  assert_eq!(
    refusal(&in_revision_one(V3, &[&deletion])),
    fault(PropertyFault::DeletionOutOfPlace)
  );
}

/// A-593 (hostile, lengths past the stream): do read records whose property section, text, content block, property
/// entry or delta window states a length past the stream's end. Expect each refused as truncated, never read short.
#[test]
fn a_length_past_the_stream_is_refused_as_truncated() {
  let cases: Vec<Vec<u8>> = vec![
    dump(&[V2, b"Revision-number: 1\nProp-content-length: 1000\nContent-length: 1000\n\nK 1\na\nV 1\nb\n"]),
    in_revision_one(V2, &[b"Node-path: a\nNode-kind: file\nNode-action: change\nText-content-length: 4611686018427387904\nContent-length: 4611686018427387904\n\nx"]),
    dump(&[V2, b"Revision-number: 1\nContent-length: 99\n\nshort"]),
    dump(&[V2, b"Revision-number: 1\nProp-content-length: 30\nContent-length: 30\n\nK 1\na\nV 15\nshort"]),
    in_revision_one(V3, &[b"Node-path: a\nNode-kind: file\nNode-action: add\nText-delta: true\nText-content-length: 10\nContent-length: 10\n\nSVN\x00\x00\x00\x05"]),
  ];
  for bytes in cases {
    assert_eq!(
      refusal(&bytes),
      DumpError::Truncated,
      "{:?}",
      String::from_utf8_lossy(&bytes)
    );
  }
}

/// A-593 (hostile, deltas): do read format-3 texts that are a zlib delta (svndiff version 1), shorter than their
/// delta, empty, or not svndiff at all. Expect each refused with svndiff's own kind and number.
#[test]
fn a_text_delta_svndiff_refuses_is_refused_with_its_kind() {
  let delta = |content: &[u8]| {
    let headers = format!(
      "Node-path: a\nNode-kind: file\nNode-action: add\nText-delta: true\nText-content-length: {0}\nContent-length: {0}\n",
      content.len()
    );
    in_revision_one(V3, &[&node(&headers, content)])
  };
  let window = b"SVN\x00\x00\x00\x01\x01\x01\x81x";
  let cases: [(Vec<u8>, SvndiffError); 4] = [
    (delta(b"SVN\x01"), SvndiffError::VersionNotOffered),
    (
      delta(&window[..window.len() - 1]),
      SvndiffError::UnexpectedEnd,
    ),
    (delta(b""), SvndiffError::UnexpectedEnd),
    (delta(b"PK\x03\x04"), SvndiffError::InvalidHeader),
  ];
  for (bytes, expected) in cases {
    let refusal = refusal(&bytes);
    assert_eq!(refusal, DumpError::Svndiff(expected));
    assert_eq!(refusal.subversion_code(), expected.subversion_code());
  }
  assert!(
    read_whole(&delta(window)).is_ok(),
    "the window itself reads"
  );
}

/// A-593 (hostile, admission): do hand a reader admitted 64 bytes a revision whose property entry states a name of
/// 1,000 bytes, a byte at a time. Expect it refused as past the admission once the entry's length line has arrived,
/// never having asked for more than 64 bytes, and never having waited for the name's bytes.
#[test]
fn a_length_past_the_admission_is_refused_before_its_bytes_are_waited_for() {
  let admission = Admission { buffer_bytes: 64 };
  let name = vec![b'n'; 1000];
  let mut section = b"K 1000\n".to_vec();
  section.extend_from_slice(&name);
  section.extend_from_slice(b"\nV 0\n\nPROPS-END\n");
  let bytes = dump(&[V2, &revision_with(&section, section.len())]);
  let entry_line_end = bytes
    .windows(7)
    .position(|window| window == b"K 1000\n")
    .unwrap()
    + 7;
  let mut reader = Reader::new(admission);
  let mut held = Vec::new();
  for (handed, &byte) in bytes.iter().enumerate() {
    match reader.read(&held, false) {
      Ok(Read::Event { consumed, .. }) => {
        held.drain(..consumed);
      }
      Ok(Read::NeedMore { consumed, needed }) => {
        assert!(needed <= 64, "asked for {needed}");
        held.drain(..consumed);
      }
      Ok(Read::End) => panic!("ended"),
      Err(refusal) => {
        assert!(
          matches!(refusal, DumpError::PastAdmission { admitted: 64, .. }),
          "{refusal:?}"
        );
        assert!(
          handed <= entry_line_end + 1,
          "waited for the name's bytes: {handed} handed"
        );
        return;
      }
    }
    held.push(byte);
  }
  panic!("never refused");
}

/// A-593 (format, as `load.c` reads): do read a dump with blank lines and lines that begin with whitespace between its
/// records, headers in another order, headers the format does not name, a content block longer than its sections, and
/// a deletion followed, after blank lines, by an addition of the same path. Expect the events of the plain dump, the
/// content block's rest passed over, the deletion and addition one replacement.
#[test]
fn what_load_passes_over_between_and_inside_records_is_passed_over() {
  let plain = in_revision_one(V2, &[&file_add("a", b"text")]);
  let (md5, sha1) = digests_hex(b"text");
  let noisy = dump(&[
    V2,
    b"\n\n  a line that begins with a space\n\t\n",
    &revision(1),
    b"\n\n",
    &node(
      &format!(
        "X-Unknown: ignored\nContent-length: 14\nText-content-length: 4\nProp-content-length: 10\n\
         Text-content-sha1: {sha1}\nText-content-md5: {md5}\nNode-action: add\nNode-kind: file\nNode-path: a\n"
      ),
      b"PROPS-END\ntext",
    ),
  ]);
  assert_eq!(read_whole(&noisy).unwrap(), read_whole(&plain).unwrap());
  let longer = dump(&[
    V2,
    b"Revision-number: 1\nProp-content-length: 10\nContent-length: 15\n\nPROPS-END\nextra\n",
  ]);
  assert_eq!(
    read_whole(&longer).unwrap(),
    read_whole(&dump(&[V2, &revision(1)])).unwrap()
  );
  let parted = in_revision_one(
    V2,
    &[
      b"Node-path: a\nNode-action: delete\n\n\n\n\n",
      &file_add("a", b"text"),
    ],
  );
  let events = read_whole(&parted).unwrap();
  let nodes: Vec<NodeAction> = events
    .iter()
    .filter_map(|event| {
      if let Owned::Node(header) = event {
        Some(header.action)
      } else {
        None
      }
    })
    .collect();
  assert_eq!(nodes, [NodeAction::Replace]);
}

/// A-593 (format): do read a format-2 stream whose second version record switches to format 3, then a delta. Expect
/// the delta read after the switch; and refused after a switch back to format 2.
#[test]
fn a_version_record_in_the_middle_of_a_stream_switches_its_format() {
  let delta = node(
    "Node-path: a\nNode-kind: file\nNode-action: add\nText-delta: true\nText-content-length: 4\nContent-length: 4\n",
    b"SVN\x00",
  );
  assert!(read_whole(&dump(&[V2, V3, &revision(1), &delta])).is_ok());
  assert_eq!(
    refusal(&dump(&[V3, V2, &revision(1), &delta])),
    DumpError::Contradiction {
      fault: Contradiction::DeltaInFormatTwo
    }
  );
  let pieces = read(&dump(&[V2, V3, &revision(1), &delta]), 1, ROOMY).unwrap();
  assert_eq!(pieces.events.len(), 7, "{:?}", pieces.events);
}
