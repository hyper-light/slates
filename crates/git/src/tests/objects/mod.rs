//! A-590's tests (§4.17.2 "Pieces (git)", piece 4): the golden vectors stock git wrote and judged (`fixtures/fsck`),
//! the tree, commit and tag parsers and writers by use, git's UTF-8 and name rules against a transliteration of git's
//! own code, and hostile input.

mod commit;
mod golden;
mod hostile;
mod names;
mod tag;
mod tree;

use super::Report;
use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};

/// The objects of `fixtures/fsck`, compiled in by the table `make-fsck-fixtures.sh` wrote: each file's path under the
/// directory and its bytes.
mod table {
  include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/fsck/table.rs"
  ));
}

/// What `make-fsck-fixtures.sh` recorded of each object: a line each.
const MANIFEST: &str = include_str!("../../../fixtures/fsck/manifest.txt");
/// Each fixture file's SHA-256, as `shasum -a 256` wrote it.
const SHA256SUMS: &str = include_str!("../../../fixtures/fsck/SHA256SUMS");
/// The git that made and judged the objects.
const GIT_VERSION: &str = include_str!("../../../fixtures/fsck/git-version.txt");

/// One object of the manifest, and what stock git said of it.
#[derive(Clone, Copy, Debug)]
struct Golden {
  /// The repository's object format.
  format: ObjectFormat,
  /// The fixture's name.
  name: &'static str,
  /// The object's type.
  kind: Kind,
  /// The id git gave it.
  id: ObjectId,
  /// Its content.
  bytes: &'static [u8],
  /// `git fsck`'s findings: `-`, `unparsed`, or `<error|warning>:<id>` comma-separated.
  fsck: &'static str,
  /// `git fsck --strict`'s, written the same way.
  strict: &'static str,
  /// `git hash-object`'s verdict: `ok` or `refused`, then `:<the first id it printed>` when it printed one.
  hash_object: &'static str,
}

/// The bytes of fixture file `path` (`<format>/<name>`).
fn file(path: &str) -> &'static [u8] {
  table::FILES
    .iter()
    .find(|(known, _)| *known == path)
    .map(|(_, bytes)| *bytes)
    .unwrap_or_else(|| panic!("no fixture {path}"))
}

/// Every object of the manifest, in its order.
fn goldens() -> Vec<Golden> {
  MANIFEST
    .lines()
    .map(|line| {
      let fields: Vec<&str> = line.split(' ').collect();
      let [format, name, kind, id, fsck, strict, hash_object] = fields[..] else {
        panic!("a manifest line of seven fields: {line}");
      };
      let format = ObjectFormat::from_name(format.as_bytes()).unwrap();
      Golden {
        format,
        name,
        kind: Kind::from_name(kind.as_bytes()).unwrap(),
        id: ObjectId::from_hex(format, id.as_bytes()).unwrap(),
        bytes: file(&format!("{}/{name}", format.name())),
        fsck,
        strict,
        hash_object,
      }
    })
    .collect()
}

/// The object named `name` in `format`'s repository.
fn golden(format: ObjectFormat, name: &str) -> Golden {
  goldens()
    .into_iter()
    .find(|golden| golden.format == format && golden.name == name)
    .unwrap_or_else(|| panic!("no fixture {name} in {format:?}"))
}

/// Both formats, SHA-1 first.
const FORMATS: [ObjectFormat; 2] = [ObjectFormat::Sha1, ObjectFormat::Sha256];

/// An admission no fixture comes near.
fn ample() -> Admission {
  Admission::new(u64::MAX)
}

/// A report written as the manifest writes git's: each finding git reports, `error:<id>` or `warning:<id>`,
/// comma-separated, or `-` for none.
fn said(report: &Report) -> String {
  let findings: Vec<String> = report
    .reported()
    .map(|(id, level)| {
      let level = match level {
        super::Severity::Error => "error",
        super::Severity::Warn => "warning",
        other => panic!("a reported finding is an error or a warning, not {other:?}"),
      };
      format!("{level}:{}", id.name())
    })
    .collect();
  if findings.is_empty() {
    "-".to_owned()
  } else {
    findings.join(",")
  }
}

/// `golden` checked as its type is checked, under `strictness`.
fn check(golden: &Golden, strictness: super::Strictness) -> Report {
  match golden.kind {
    Kind::Tree => {
      super::tree::fsck(golden.bytes, golden.format, strictness, &mut ample())
        .unwrap()
        .report
    }
    Kind::Commit => super::commit::fsck(golden.bytes, golden.format, strictness),
    Kind::Tag => super::tag::fsck(golden.bytes, golden.format, strictness),
    Kind::Blob => panic!("no blob is a fixture of this piece"),
  }
}
