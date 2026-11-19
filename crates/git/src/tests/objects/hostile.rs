//! Hostile input (§4.9): every cut and every bit flip of every object stock git wrote, over-long names, dates, modes
//! and header counts, admissions too small, and arbitrary bytes. Each is read or refused by kind, written back as read
//! when read, and judged by the check as the parse judged it; nothing panics.

use proptest::prelude::*;

use super::super::commit::{self, Commit};
use super::super::tag::{self, Tag};
use super::super::tree::{self, Mode, Tree};
use super::super::{FsckId, Ident, ObjectRefused, Refusal, Strictness};
use super::{FORMATS, Golden, ample, golden, goldens};
use crate::object_name::{Kind, ObjectFormat};
use crate::{Admission, PastAdmission};

/// `bytes` read as an object of `kind` in `format`, then written back: the bytes written, or the refusal.
fn read(kind: Kind, format: ObjectFormat, bytes: &[u8]) -> Result<Vec<u8>, ObjectRefused> {
  let mut out = Vec::new();
  match kind {
    Kind::Tree => Tree::parse(bytes, format, &mut ample())?.write(&mut out),
    Kind::Commit => Commit::parse(bytes, &mut ample())?.write(&mut out),
    Kind::Tag => Tag::parse(bytes, &mut ample())?.write(&mut out),
    Kind::Blob => panic!("blobs are not this piece's"),
  }
  Ok(out)
}

/// `bytes` checked as an object of `kind` in `format`: every finding, under each strictness.
fn findings(kind: Kind, format: ObjectFormat, bytes: &[u8], strictness: Strictness) -> Vec<FsckId> {
  let report = match kind {
    Kind::Tree => tree::fsck(bytes, format, strictness, &mut ample()).unwrap().report,
    Kind::Commit => commit::fsck(bytes, format, strictness),
    Kind::Tag => tag::fsck(bytes, format, strictness),
    Kind::Blob => panic!("blobs are not this piece's"),
  };
  report.findings().to_vec()
}

/// Reads and checks `bytes` as `kind`, and holds the two to each other: read, they are written back as they were and
/// the check finds nothing git could not read; refused, the refusal is typed and placed within the bytes, and the
/// check's first finding is the same reason, under either strictness.
fn judge(kind: Kind, format: ObjectFormat, bytes: &[u8]) {
  let parsed = read(kind, format, bytes);
  for strictness in [Strictness::Default, Strictness::Strict] {
    let found = findings(kind, format, bytes, strictness);
    match &parsed {
      Ok(written) => {
        assert_eq!(written, bytes);
        let unreadable = [FsckId::BadTree, FsckId::NulInHeader, FsckId::UnterminatedHeader];
        assert!(!found.iter().any(|id| unreadable.contains(id)), "{kind:?} {bytes:x?}: {found:?}");
      }
      Err(ObjectRefused { at, refusal }) => {
        assert!(*at <= bytes.len());
        let reason = refusal.fsck_id().unwrap_or_else(|| panic!("an ample admission never refuses: {refusal:?}"));
        let first = match kind {
          Kind::Tree => found.contains(&reason),
          _ => found.first() == Some(&reason),
        };
        assert!(first, "{kind:?} refused {refusal:?}, checked {found:?}: {bytes:x?}");
      }
    }
  }
}

/// `verify_headers`' rule, restated: a NUL before the first empty line's second newline is `nulInHeader` there; an
/// empty line or a last newline makes the headers whole; anything else is `unterminatedHeader` at the end.
fn headers_verdict(bytes: &[u8]) -> Result<(), ObjectRefused> {
  let blank = bytes.windows(2).position(|pair| pair == b"\n\n").map(|at| at + 1);
  let nul = bytes.iter().position(|byte| *byte == 0);
  match (nul, blank) {
    (Some(at), Some(end)) if at < end => Err(ObjectRefused { at, refusal: Refusal::NulInHeader }),
    (Some(at), None) => Err(ObjectRefused { at, refusal: Refusal::NulInHeader }),
    (_, Some(_)) => Ok(()),
    (None, None) if bytes.last() == Some(&b'\n') => Ok(()),
    (None, None) => Err(ObjectRefused { at: bytes.len(), refusal: Refusal::UnterminatedHeader }),
  }
}

/// Where each of a tree's entries ends, from 0: a cut there leaves that many whole entries.
fn entry_ends(golden: &Golden) -> Option<Vec<usize>> {
  let tree = Tree::parse(golden.bytes, golden.format, &mut ample()).ok()?;
  let mut ends = vec![0];
  for entry in tree.entries() {
    let mut written = Vec::new();
    entry.write(&mut written);
    ends.push(ends.last().unwrap() + written.len());
  }
  Some(ends)
}

/// A-590 (§4.9; `decode_tree_entry`, `verify_headers`). Do: cut every object stock git wrote at every length, from
/// nothing to one byte short. Expect: a tree cut where an entry ends read with the entries before it, and cut anywhere
/// else refused as git refuses it (`badTree`); a commit or a tag read exactly when its cut headers are whole by git's
/// rule and refused otherwise with git's reason at git's place; each cut judged the same by the check; no panic.
#[test]
fn every_cut_of_every_object_is_read_or_refused_by_kind() {
  let mut cuts = 0_usize;
  for golden in goldens() {
    let ends = entry_ends(&golden);
    for cut in 0..golden.bytes.len() {
      let bytes = &golden.bytes[..cut];
      judge(golden.kind, golden.format, bytes);
      let parsed = read(golden.kind, golden.format, bytes);
      match (golden.kind, &ends) {
        (Kind::Tree, Some(ends)) => match ends.iter().position(|end| *end == cut) {
          Some(count) => {
            assert_eq!(Tree::parse(bytes, golden.format, &mut ample()).unwrap().entries().len(), count);
          }
          None => assert_eq!(parsed.unwrap_err().refusal.fsck_id(), Some(FsckId::BadTree), "{} at {cut}", golden.name),
        },
        (Kind::Tree, None) => {}
        _ => assert_eq!(parsed.map(|_| ()), headers_verdict(bytes), "{} at {cut}", golden.name),
      }
      cuts += 1;
    }
  }
  // Every object of the corpus, cut at each of its 39617 bytes in all.
  assert_eq!(cuts, 39_617);
}

/// Format: the largest object whose every bit is flipped; only the 4 KiB-name tree is past it.
const FLIP_LIMIT: usize = 1024;

/// A-590 (§4.9). Do: flip every bit of every object stock git wrote, up to a kibibyte, one at a time. Expect: each
/// flipped object read and written back as it is, or refused by kind and place; the check judging it as the parse did;
/// no panic.
#[test]
fn every_bit_flip_of_every_object_is_read_or_refused_by_kind() {
  let mut flips = 0_usize;
  for golden in goldens().iter().filter(|golden| golden.bytes.len() <= FLIP_LIMIT) {
    let mut bytes = golden.bytes.to_vec();
    for at in 0..bytes.len() {
      for bit in 0..8 {
        bytes[at] ^= 1 << bit;
        judge(golden.kind, golden.format, &bytes);
        bytes[at] ^= 1 << bit;
        flips += 1;
      }
    }
  }
  // The 358 objects up to a kibibyte, 31355 bytes of them, eight bits each.
  assert_eq!(flips, 250_840);
}

/// A mebibyte of `byte`.
fn mebibyte(byte: u8) -> Vec<u8> {
  vec![byte; 1 << 20]
}

/// A-590 (§4.9; fsck.c's `largePathname`, `date_overflows`, `parse_mode`). Do: read and check a commit whose author's
/// name is a mebibyte, a header line of a mebibyte with no newline, a tree entry named with a mebibyte, a mode of
/// 100000 octal digits, a tag named with a mebibyte, and a date of 100000 digits. Expect: the long name read whole and
/// accepted; the unended line `unterminatedHeader` at its end; the long entry `largePathname`; the long mode read,
/// wrapped to 16 bits as git wraps it, and `badFilemode`; the long tag name accepted; the long date
/// `badDateOverflow`; no panic and no overflow.
#[test]
fn over_long_names_lines_modes_and_dates_are_read_without_overflow() {
  let tree_line = format!("tree {}\n", "4".repeat(40));
  let ident = b" <a@b> 1 +0000\n";
  let mut commit = tree_line.clone().into_bytes();
  commit.extend_from_slice(b"author ");
  commit.extend(mebibyte(b'n'));
  commit.extend_from_slice(ident);
  commit.extend_from_slice(b"committer C");
  commit.extend_from_slice(ident);
  commit.extend_from_slice(b"\nm\n");
  let parsed = Commit::parse(&commit, &mut ample()).unwrap();
  assert_eq!(Ident::parse(parsed.author().unwrap()).unwrap().name.len(), 1 << 20);
  assert_eq!(commit::fsck(&commit, ObjectFormat::Sha1, Strictness::Strict).reported().count(), 0);

  let mut unended = tree_line.clone().into_bytes();
  unended.extend_from_slice(b"x-extra ");
  unended.extend(mebibyte(b'x'));
  let at = unended.len();
  assert_eq!(
    Commit::parse(&unended, &mut ample()),
    Err(ObjectRefused { at, refusal: Refusal::UnterminatedHeader })
  );

  let id = [0x55; 20];
  let long_name = mebibyte(b'n');
  let mut long_entry = Vec::new();
  tree::Entry::new(Mode::File, &long_name, &id).write(&mut long_entry);
  let checked = tree::fsck(&long_entry, ObjectFormat::Sha1, Strictness::Default, &mut ample()).unwrap();
  assert_eq!(checked.report.findings(), [FsckId::LargePathname]);

  let mut long_mode = vec![b'7'; 100_000];
  long_mode.extend_from_slice(b" n\0");
  long_mode.extend_from_slice(&id);
  let tree = Tree::parse(&long_mode, ObjectFormat::Sha1, &mut ample()).unwrap();
  assert_eq!(tree.entries()[0].mode, u16::MAX);
  let checked = tree::fsck(&long_mode, ObjectFormat::Sha1, Strictness::Default, &mut ample()).unwrap();
  assert_eq!(checked.report.findings(), [FsckId::BadFilemode]);

  let mut long_tag = format!("object {}\ntype commit\ntag ", "4".repeat(40)).into_bytes();
  long_tag.extend(mebibyte(b'a'));
  long_tag.extend_from_slice(b"\ntagger T <t@e> 1 +0000\n\nm\n");
  assert_eq!(tag::fsck(&long_tag, ObjectFormat::Sha1, Strictness::Strict).reported().count(), 0);
  assert_eq!(Tag::parse(&long_tag, &mut ample()).unwrap().name().unwrap().len(), 1 << 20);

  let mut long_date = tree_line.into_bytes();
  long_date.extend_from_slice(b"author A <a@b> ");
  long_date.extend(vec![b'9'; 100_000]);
  long_date.extend_from_slice(b" +0000\ncommitter C <c@d> 1 +0000\n");
  assert_eq!(commit::fsck(&long_date, ObjectFormat::Sha1, Strictness::Default).refusal(), Some(FsckId::BadDateOverflow));
}

/// A-590 (§4.9; fsck.c's `multipleAuthors`). Do: read a commit of 100000 `parent` lines, under an ample admission and
/// under one a byte short of their headers' room; check one of 100000 `author` lines. Expect: every parent read, in
/// order; the short admission refused with what it asked and had, nothing charged; the authors `multipleAuthors`.
#[test]
fn a_hundred_thousand_headers_are_charged_first_and_judged() {
  let parent = format!("parent {}\n", "6".repeat(40));
  let mut parents = format!("tree {}\n", "4".repeat(40)).into_bytes();
  for _ in 0..100_000 {
    parents.extend_from_slice(parent.as_bytes());
  }
  parents.extend_from_slice(b"author A <a@b> 1 +0000\ncommitter C <c@d> 1 +0000\n");
  let commit = Commit::parse(&parents, &mut ample()).unwrap();
  assert_eq!(commit.parents(ObjectFormat::Sha1).count(), 100_000);
  assert_eq!(commit::fsck(&parents, ObjectFormat::Sha1, Strictness::Strict).reported().count(), 0);
  let room = u64::try_from(core::mem::size_of::<super::super::Header<'_>>() * 100_003).unwrap();
  let mut short = Admission::new(room - 1);
  assert_eq!(
    Commit::parse(&parents, &mut short),
    Err(ObjectRefused {
      at: 0,
      refusal: Refusal::PastAdmission(PastAdmission { asked: room, left: room - 1 })
    })
  );
  assert_eq!(short.left(), room - 1);
  let mut authors = format!("tree {}\n", "4".repeat(40)).into_bytes();
  for _ in 0..100_000 {
    authors.extend_from_slice(b"author A <a@b> 1 +0000\n");
  }
  authors.extend_from_slice(b"committer C <c@d> 1 +0000\n");
  assert_eq!(commit::fsck(&authors, ObjectFormat::Sha1, Strictness::Default).refusal(), Some(FsckId::MultipleAuthors));
}

/// A-590 (fsck.c: `missingType` and `missingTag` cannot be met once the headers are whole). Do: check every cut of
/// the well-formed tag stock git wrote, in both formats, each `type` and `tag` line cut before and after its newline
/// among them. Expect: never either finding: whole headers always end such a line with a newline, so
/// `verify_headers` or an earlier check meets the bytes first.
#[test]
fn a_type_or_tag_line_with_no_newline_is_met_first_by_another_check() {
  for format in FORMATS {
    let golden = golden(format, "ok-tag");
    for cut in 0..=golden.bytes.len() {
      let found = findings(Kind::Tag, format, &golden.bytes[..cut], Strictness::Strict);
      assert!(!found.contains(&FsckId::MissingType) && !found.contains(&FsckId::MissingTag), "{cut}: {found:?}");
    }
  }
}

/// Format: the pieces generated commits and tags are made of: every header name the checks read, the bytes their
/// scans stop at, an id of each format's length and a broken one, dates and zones.
const TOKENS: [&[u8]; 24] = [
  b"tree ",
  b"parent ",
  b"author ",
  b"committer ",
  b"object ",
  b"type ",
  b"tag ",
  b"tagger ",
  b"commit",
  b"\n",
  b" ",
  b"<",
  b">",
  b"a@b",
  b"A U Thor",
  b"1112911993",
  b"0",
  b"+0000",
  b"-01",
  b"\0",
  b"\t",
  b"4444444444444444444444444444444444444444",
  b"4444444444444444444444444444444444444444444444444444444444444444",
  b"zz",
];

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-590 (§4.9). Do: read and check arbitrary bytes as a tree of each format, a commit and a tag. Expect: each read
  /// and written back as it was, or refused by kind with the check agreeing; no panic.
  #[test]
  fn any_bytes_are_read_or_refused_by_kind(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
    for format in FORMATS {
      judge(Kind::Tree, format, &bytes);
      judge(Kind::Commit, format, &bytes);
      judge(Kind::Tag, format, &bytes);
    }
  }

  /// A-590 (§4.9). Do: read and check commits and tags put together from header names, ids, identities' pieces and
  /// the bytes git's scans stop at. Expect: as for any bytes; these reach every check of the commit and tag walks.
  #[test]
  fn header_shaped_bytes_are_read_or_refused_by_kind(picks in proptest::collection::vec(0..TOKENS.len(), 0..48)) {
    let bytes: Vec<u8> = picks.iter().flat_map(|pick| TOKENS[*pick].iter().copied()).collect();
    for format in FORMATS {
      judge(Kind::Commit, format, &bytes);
      judge(Kind::Tag, format, &bytes);
    }
  }
}
