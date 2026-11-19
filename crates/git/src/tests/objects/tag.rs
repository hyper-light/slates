//! Tags by use: headers at the places git reads them, the signature's split, and the one place slates refuses what git
//! accepts.

use super::super::tag::{self, Tag};
use super::super::{FsckId, Ident, Severity, Strictness};
use super::{FORMATS, ample, golden, said};
use crate::object_name::Kind;

/// A-590 (tag.c's `parse_tag_buffer` reads `object`, `type`, `tag` and `tagger`; `fixtures/fsck` `ok-tag`, written by
/// `git tag -a`). Do: read the annotated tag stock git wrote, in both formats, and write it back. Expect: the root
/// commit it names, its type, its name, its tagger read into parts, its message, no signature, and the same bytes.
#[test]
fn a_tag_is_read_header_by_header_and_written_back_byte_for_byte() {
  for format in FORMATS {
    let golden = golden(format, "ok-tag");
    let tag = Tag::parse(golden.bytes, &mut ample()).unwrap();
    assert_eq!(tag.object(format), Some(super::golden(format, "ok-commit-root").id));
    assert_eq!(tag.kind(), Some(Kind::Commit));
    assert_eq!(tag.name(), Some(&b"v1"[..]));
    let tagger = Ident::parse(tag.tagger().unwrap()).unwrap();
    assert_eq!((tagger.name, tagger.seconds, tagger.zone), (&b"C O Mitter"[..], 1_112_911_993, &b"+0000"[..]));
    assert_eq!(tag.message(), Some(&b"version one\n"[..]));
    assert_eq!(tag.signature(), None);
    let mut written = Vec::new();
    tag.write(&mut written);
    assert_eq!(written, golden.bytes);
  }
}

/// A-590 (gpg-interface.c's `parse_signed_buffer`; gitformat-signature). Do: read the signed tag, in both formats, and
/// split it at its signature; then read tags whose last block is each format git recognizes, and one whose signature
/// line is not at a line's start. Expect: the part signed the unsigned tag stock git wrote, the block the rest; each
/// recognized block split at its last start; a marker inside a line no signature.
#[test]
fn a_signed_tag_splits_where_its_last_signature_block_begins() {
  for format in FORMATS {
    let golden = golden(format, "ok-tag-signed");
    let tag = Tag::parse(golden.bytes, &mut ample()).unwrap();
    let (signed, block) = tag.signature().unwrap();
    assert_eq!(signed, super::golden(format, "ok-tag").bytes);
    assert_eq!(block, b"-----BEGIN PGP SIGNATURE-----\n\nbm90IGEgc2lnbmF0dXJl\n-----END PGP SIGNATURE-----\n");
  }
  let head = "object 0000000000000000000000000000000000000001\ntype commit\ntag v\n\nm\n";
  for start in [
    "-----BEGIN PGP SIGNATURE-----",
    "-----BEGIN PGP MESSAGE-----",
    "-----BEGIN SIGNED MESSAGE-----",
    "-----BEGIN SSH SIGNATURE-----",
  ] {
    let text = format!("{head}-----BEGIN PGP SIGNATURE-----\nfirst\n{start}\nsecond\n");
    let tag = Tag::parse(text.as_bytes(), &mut ample()).unwrap();
    let (signed, block) = tag.signature().unwrap();
    assert_eq!(block, format!("{start}\nsecond\n").as_bytes(), "the last block begins the signature");
    assert!(signed.ends_with(b"first\n"));
  }
  let inline = format!("{head}see -----BEGIN PGP SIGNATURE-----\n");
  assert_eq!(Tag::parse(inline.as_bytes(), &mut ample()).unwrap().signature(), None);
}

/// A-590 (fsck.c's `fsck_tag_standalone`: the tagger's result is not looked at before the extra-header check, whose
/// ignored finding overwrites it; `fixtures/fsck` `bad-tag-tagger-extra-header`, which stock `git hash-object` printed
/// `missingEmail` of and wrote). Do: check the tag with a malformed tagger followed by an extra header, under both
/// strictnesses. Expect: `missingEmail` reported as git prints it, the extra header found and not reported, and the tag
/// refused for `missingEmail`, where git accepts it: the one place slates is stricter than git.
#[test]
fn a_bad_tagger_before_an_extra_header_is_refused_where_git_accepts_it() {
  for format in FORMATS {
    let golden = golden(format, "bad-tag-tagger-extra-header");
    assert_eq!(golden.hash_object, "ok:missingEmail", "git wrote it after printing the tagger's error");
    for strictness in [Strictness::Default, Strictness::Strict] {
      let report = tag::fsck(golden.bytes, format, strictness);
      assert_eq!(report.findings(), [FsckId::MissingEmail, FsckId::ExtraHeaderEntry]);
      assert_eq!(said(&report), "error:missingEmail");
      assert_eq!(report.refusal(), Some(FsckId::MissingEmail));
    }
    let clean = golden_report(format, "ignore-tag-extra-header");
    assert_eq!(clean.findings(), [FsckId::ExtraHeaderEntry]);
    assert_eq!(clean.refusal(), None, "an extra header alone is ignored, as in git");
  }
}

/// The default check of tag fixture `name`.
fn golden_report(format: crate::object_name::ObjectFormat, name: &str) -> super::super::Report {
  tag::fsck(golden(format, name).bytes, format, Strictness::Default)
}

/// A-590 (fsck.c: `badTagName` and `missingTaggerEntry` are infos; t1450 "tag with incorrect tag name & missing
/// tagger"). Do: check the tag with a name git refuses and no tagger, under both strictnesses. Expect: both reported
/// as warnings, in git's order, under either strictness, and the tag kept.
#[test]
fn a_bad_tag_name_and_a_missing_tagger_are_warnings_under_either_strictness() {
  for format in FORMATS {
    let golden = golden(format, "warn-tag-name-and-tagger");
    for strictness in [Strictness::Default, Strictness::Strict] {
      let report = tag::fsck(golden.bytes, format, strictness);
      let reported: Vec<_> = report.reported().collect();
      assert_eq!(
        reported,
        [(FsckId::BadTagName, Severity::Warn), (FsckId::MissingTaggerEntry, Severity::Warn)]
      );
      assert_eq!(report.refusal(), None);
    }
    let tag = Tag::parse(golden.bytes, &mut ample()).unwrap();
    assert_eq!(tag.name(), Some(&b"wrong name format"[..]));
    assert_eq!(tag.tagger(), None);
  }
}
