//! The golden vectors: every object stock git 2.50.1 wrote and judged in `fixtures/fsck`, read, written back and checked
//! here as git read and judged it.

use super::super::commit::Commit;
use super::super::tag::Tag;
use super::super::tree::Tree;
use super::super::{FsckId, ObjectRefused, Refusal, Severity, Strictness};
use super::{GIT_VERSION, Golden, MANIFEST, SHA256SUMS, ample, check, file, goldens, said};
use crate::object_name::{Hasher, Kind, ObjectFormat, name_of};
use crate::out::Out;

/// `bytes`' SHA-256 in lower-case hexadecimal, as `shasum -a 256` writes it.
fn sha256(bytes: &[u8]) -> String {
  let mut hasher = Hasher::raw(ObjectFormat::Sha256);
  hasher.put(bytes);
  hasher.finish().unwrap().to_string()
}

/// A-590 (golden vectors, `fixtures/fsck/make-fsck-fixtures.sh`). Do: hash every fixture file with SHA-256, the
/// manifest and git's version line included, and name every object of the manifest from its type and bytes. Expect:
/// each file's SHA-256 the one `SHA256SUMS` holds, every file listed there and nothing else; each object's name the one
/// stock git gave it; the objects made by stock git 2.50.1 (Apple Git-155), 90 in each format and the 180 name objects
/// in SHA-1.
#[test]
fn every_fixture_is_the_bytes_its_sum_names_and_the_object_its_id_names() {
  let sums: Vec<(&str, &str)> = SHA256SUMS
    .lines()
    .map(|line| line.split_once("  ").unwrap())
    .map(|(sum, path)| (path, sum))
    .collect();
  let goldens = goldens();
  assert_eq!(sums.len(), goldens.len() + 2, "every object, the manifest and the version line");
  for (path, sum) in &sums {
    let bytes = match *path {
      "manifest.txt" => MANIFEST.as_bytes(),
      "git-version.txt" => GIT_VERSION.as_bytes(),
      object => file(object),
    };
    assert_eq!(sha256(bytes), *sum, "{path}");
  }
  for golden in &goldens {
    let path = format!("{}/{}", golden.format.name(), golden.name);
    assert!(sums.iter().any(|(listed, _)| *listed == path), "{path} has its sum");
    assert_eq!(name_of(golden.format, golden.kind, golden.bytes), Ok(golden.id), "{path}");
  }
  assert_eq!(GIT_VERSION, "git version 2.50.1 (Apple Git-155)\n");
  let in_format = |format| goldens.iter().filter(|golden| golden.format == format).count();
  assert_eq!(in_format(ObjectFormat::Sha256), 90);
  assert_eq!(in_format(ObjectFormat::Sha1), 90 + 180);
}

/// The fsck message the manifest's verdict `column` (`git fsck`'s or `--strict`'s) names first, when it names one.
fn first_named(column: &str) -> Option<FsckId> {
  let first = column.split(',').next()?;
  let (_, id) = first.split_once(':')?;
  Some(FsckId::from_name(id).unwrap_or_else(|| panic!("git's id {id} is one of fsck.h's")))
}

/// `golden` parsed by its type, and written back.
fn parse_and_write(golden: &Golden) -> Result<Vec<u8>, ObjectRefused> {
  let mut out = Vec::new();
  match golden.kind {
    Kind::Tree => Tree::parse(golden.bytes, golden.format, &mut ample())?.write(&mut out),
    Kind::Commit => Commit::parse(golden.bytes, &mut ample())?.write(&mut out),
    Kind::Tag => Tag::parse(golden.bytes, &mut ample())?.write(&mut out),
    Kind::Blob => panic!("no blob is a fixture of this piece"),
  }
  Ok(out)
}

/// A-590 (git's tree walk, `decode_tree_entry`; `verify_headers`). Do: parse every object by its type and write it
/// back. Expect: an object git cannot read refused with git's reason (a tree whose walk git reports `badTree` of; a
/// commit or tag whose headers git reports `nulInHeader` or `unterminatedHeader` of); every other written back byte
/// for byte, the ones fsck finds fault with included.
#[test]
fn every_object_git_reads_is_read_and_written_back_byte_for_byte() {
  let mut refused = 0;
  for golden in goldens() {
    let unreadable = match golden.kind {
      Kind::Tree => golden.fsck.contains("error:badTree"),
      _ => matches!(first_named(golden.fsck), Some(FsckId::NulInHeader | FsckId::UnterminatedHeader)),
    };
    match parse_and_write(&golden) {
      Ok(written) => {
        assert!(!unreadable, "{} {}: git cannot read it", golden.format.name(), golden.name);
        assert_eq!(written, golden.bytes, "{} {}", golden.format.name(), golden.name);
      }
      Err(ObjectRefused { refusal, at }) => {
        refused += 1;
        assert!(unreadable, "{} {}: refused {refusal:?}", golden.format.name(), golden.name);
        let expected = match golden.kind {
          Kind::Tree => FsckId::BadTree,
          _ => first_named(golden.fsck).unwrap(),
        };
        assert_eq!(refusal.fsck_id(), Some(expected), "{}", golden.name);
        assert!(at <= golden.bytes.len());
        assert!(!matches!(refusal, Refusal::PastAdmission(_)));
      }
    }
  }
  // Four trees and three commits or tags in each format.
  assert_eq!(refused, 14);
}

/// A-590 (fsck.c, every check at its default severity; git 2.50.1's `git fsck`, `git fsck --strict` and
/// `git hash-object`). Do: check every object under both strictnesses. Expect: what slates reports, each finding at its
/// level and in its order, is what `git fsck` and `git fsck --strict` printed; an object `git fsck` could not parse at
/// all is refused for the reason `git hash-object`, which runs fsck's own checks on the bytes, gave; whatever
/// `git hash-object` refused is reported under the strict check, its first finding first, and whatever it wrote is
/// reported clean, but for the one place slates refuses what git accepts (a bad tagger before an extra header).
#[test]
fn every_object_is_judged_as_stock_git_judges_it_under_both_strictnesses() {
  let mut divergences = Vec::new();
  for golden in goldens() {
    let named = format!("{} {}", golden.format.name(), golden.name);
    for (strictness, column) in [(Strictness::Default, golden.fsck), (Strictness::Strict, golden.strict)] {
      let report = check(&golden, strictness);
      if column == "unparsed" {
        let (_, reason) = golden.hash_object.split_once(':').unwrap();
        assert_eq!(report.refusal(), FsckId::from_name(reason), "{named} under {strictness:?}");
      } else {
        assert_eq!(said(&report), column, "{named} under {strictness:?}");
      }
    }
    let strict = check(&golden, Strictness::Strict);
    let first = strict.reported().next().map(|(id, _)| id.name());
    match golden.hash_object.split_once(':') {
      Some(("refused", printed)) => assert_eq!(first, Some(printed), "{named}"),
      None => assert_eq!((golden.hash_object, first), ("ok", None), "{named}"),
      Some(("ok", printed)) => {
        assert_eq!(strict.refusal().map(FsckId::name), Some(printed), "{named}: slates refuses it");
        divergences.push(golden.name);
      }
      Some(other) => panic!("{named}: hash-object said {other:?}"),
    }
  }
  assert_eq!(divergences, ["bad-tag-tagger-extra-header"; 2], "the one divergence, once in each format");
}

/// A-590. Do: read what each fixture's name says it is (`ok-` clean, `warn-` and `info-` kept with warnings, `bad-`
/// refused, `ignore-` clean with an ignored finding) against slates' own check under `git fsck`'s strictness. Expect:
/// each is what its name says, so the corpus is what its script says it made.
#[test]
fn each_fixture_is_what_its_name_says_under_gits_default_check() {
  for golden in goldens().iter().filter(|golden| !golden.name.starts_with("name-")) {
    let report = check(golden, Strictness::Default);
    let (prefix, _) = golden.name.split_once('-').unwrap();
    let warned = report.warnings().count() > 0;
    match prefix {
      "ok" => assert_eq!(report.reported().count(), 0, "{}", golden.name),
      "warn" | "info" => assert!(warned && report.refusal().is_none(), "{}", golden.name),
      "bad" => assert!(report.refusal().is_some(), "{}", golden.name),
      "ignore" => {
        assert_eq!(report.reported().count(), 0, "{}", golden.name);
        assert_eq!(report.findings(), [FsckId::ExtraHeaderEntry]);
      }
      other => panic!("{}: an unknown prefix {other}", golden.name),
    }
  }
}

/// A-590 (fsck.h). Do: name every message id and read each name back; take each id's level under both strictnesses.
/// Expect: 71 ids, each with one name that reads back to it alone; a fatal or an error reported as an error under
/// either, a warning as a warning or, under the strict check, an error, an info as a warning, and an ignored id never.
#[test]
fn every_fsck_id_has_one_name_and_its_level_follows_the_strictness() {
  let mut names: Vec<&str> = FsckId::ALL.iter().map(|id| id.name()).collect();
  for id in FsckId::ALL {
    assert_eq!(FsckId::from_name(id.name()), Some(id));
  }
  names.sort_unstable();
  names.dedup();
  assert_eq!(names.len(), FsckId::ALL.len());
  assert_eq!(FsckId::from_name("BadTree"), None, "names compare exactly");
  for id in FsckId::ALL {
    let levels = (id.severity().reported(Strictness::Default), id.severity().reported(Strictness::Strict));
    let expected = match id.severity() {
      Severity::Fatal | Severity::Error => (Severity::Error, Severity::Error),
      Severity::Warn => (Severity::Warn, Severity::Error),
      Severity::Info => (Severity::Warn, Severity::Warn),
      Severity::Ignore => (Severity::Ignore, Severity::Ignore),
    };
    assert_eq!(levels, expected, "{id:?}");
  }
}
