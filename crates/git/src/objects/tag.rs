//! A tag object (A-590): its headers (`object`, `type`, `tag`, `tagger`, and any other, each with its continuation
//! lines) and its message, parsed as written, written back byte for byte, and checked as git's `fsck_tag_standalone`
//! (fsck.c) checks them.
//!
//! A signed tag carries its signature at the end of its message, as a block whose first line names its format
//! (gitformat-signature); the signature covers the tag up to that line ([`Tag::signature`], as gpg-interface.c's
//! `parse_signed_buffer` splits it).

use super::fsck::{FsckId, Report, Strictness};
use super::headers::{self, Header};
use super::{ObjectRefused, hex, ident};
use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::refname;

/// Format: the first lines of the signature blocks git recognizes (gpg-interface.c's formats: OpenPGP's two, X.509's
/// and SSH's).
const SIGNATURE_STARTS: [&[u8]; 4] = [
  b"-----BEGIN PGP SIGNATURE-----",
  b"-----BEGIN PGP MESSAGE-----",
  b"-----BEGIN SIGNED MESSAGE-----",
  b"-----BEGIN SSH SIGNATURE-----",
];

/// The `object` header's place: first (fsck.c reads `object`, `type`, `tag` and `tagger` in this order).
const OBJECT: usize = 0;
/// The `type` header's place: second.
const TYPE: usize = 1;
/// The `tag` header's place: third.
const NAME: usize = 2;
/// Format: the `tagger` header's place, fourth, after `object`, `type` and `tag`.
const TAGGER: usize = 3;

/// A tag: its bytes, its headers in the order written, and its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag<'a> {
  /// The whole content, which a signature's split is found in.
  bytes: &'a [u8],
  /// The headers.
  headers: Vec<Header<'a>>,
  /// The message after the empty line; none when the headers end the tag.
  message: Option<&'a [u8]>,
}

impl<'a> Tag<'a> {
  /// Parses `bytes`, a tag's content, charging its headers to `admission` before any is kept. Only what git refuses to
  /// read at all is refused here (a NUL in the headers, or headers that end with no newline); the rest is [`fsck`]'s
  /// to judge.
  pub fn parse(bytes: &'a [u8], admission: &mut Admission) -> Result<Self, ObjectRefused> {
    let (headers, message) = headers::read(bytes, admission)?;
    Ok(Self {
      bytes,
      headers,
      message,
    })
  }

  /// Every header in the order written.
  pub fn headers(&self) -> &[Header<'a>] {
    &self.headers
  }

  /// The value of the header at `place` when it is named `name`.
  fn at(&self, place: usize, name: &[u8]) -> Option<&'a [u8]> {
    self
      .headers
      .get(place)
      .filter(|header| header.name == name)
      .and_then(|header| header.value)
  }

  /// The tagged object's id in `format`, from the first header, `object`.
  pub fn object(&self, format: ObjectFormat) -> Option<ObjectId> {
    ObjectId::from_hex(format, self.at(OBJECT, b"object")?).ok()
  }

  /// The tagged object's type, from the second header, `type`.
  pub fn kind(&self) -> Option<Kind> {
    Kind::from_name(self.at(TYPE, b"type")?).ok()
  }

  /// The tag's name, from the third header, `tag`.
  pub fn name(&self) -> Option<&'a [u8]> {
    self.at(NAME, b"tag")
  }

  /// The `tagger` header's value, the fourth header; early tags have none. [`super::Ident::parse`] reads its parts.
  pub fn tagger(&self) -> Option<&'a [u8]> {
    self.at(TAGGER, b"tagger")
  }

  /// The message, the signature block included.
  pub fn message(&self) -> Option<&'a [u8]> {
    self.message
  }

  /// The tag split where its signature begins (gpg-interface.c's `parse_signed_buffer`: the last line that starts a
  /// block it recognizes): what the signature covers, and the block. None when no line starts one.
  pub fn signature(&self) -> Option<(&'a [u8], &'a [u8])> {
    let mut start = None;
    let mut line = 0_usize;
    while let Some(rest) = self.bytes.get(line..).filter(|rest| !rest.is_empty()) {
      if SIGNATURE_STARTS.iter().any(|begin| rest.starts_with(begin)) {
        start = Some(line);
      }
      line = rest
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(self.bytes.len(), |end| {
          line.saturating_add(end).saturating_add(1)
        });
    }
    start.map(|at| self.bytes.split_at(at))
  }

  /// Writes the tag as it was read.
  pub fn write(&self, out: &mut impl Out) {
    headers::write(&self.headers, self.message, out);
  }
}

/// Checks `bytes`, a tag's content in a repository of `format`, as git's `fsck_tag_standalone` does under
/// `strictness`: the headers whole, then `object`, `type`, `tag` (its name as `refs/tags/<name>`), `tagger`, and no
/// header after it. It stops where git's does, with one difference (the module above): a malformed tagger refuses the
/// tag even when an extra header follows it.
pub fn fsck(bytes: &[u8], format: ObjectFormat, strictness: Strictness) -> Report {
  let mut report = Report::new(strictness);
  if let Err(refused) = headers::verify(bytes) {
    if let Some(id) = refused.refusal.fsck_id() {
      report.report(id);
    }
    return report;
  }
  if let Err(id) = check(bytes, format, &mut report) {
    report.report(id);
  }
  report
}

/// The line that begins `rest` without its newline, and what follows; none when no newline ends it.
fn line(rest: &[u8]) -> Option<(&[u8], &[u8])> {
  let end = rest.iter().position(|byte| *byte == b'\n')?;
  Some((rest.get(..end)?, rest.get(end.saturating_add(1)..)?))
}

/// `fsck_tag_standalone`'s walk, once the headers are whole: the finding that stops it, or Ok at its end, the findings
/// it went on past reported as it met them. `missingType` and `missingTag` (a `type` or `tag` line with no newline) are
/// kept as git keeps them, though whole headers never let them happen.
fn check(bytes: &[u8], format: ObjectFormat, report: &mut Report) -> Result<(), FsckId> {
  let after = bytes
    .strip_prefix(b"object ")
    .ok_or(FsckId::MissingObject)?;
  let object = hex::id_then(after, format, b'\n').ok_or(FsckId::BadObjectSha1)?;
  let rest = after
    .get(object.len().saturating_add(1)..)
    .unwrap_or_default();
  let after = rest
    .strip_prefix(b"type ")
    .ok_or(FsckId::MissingTypeEntry)?;
  let (kind, rest) = line(after).ok_or(FsckId::MissingType)?;
  Kind::from_name(kind).map_err(|_| FsckId::BadType)?;
  let after = rest.strip_prefix(b"tag ").ok_or(FsckId::MissingTagEntry)?;
  let (name, rest) = line(after).ok_or(FsckId::MissingTag)?;
  if !refname::tag_name_is_valid(name) && report.report(FsckId::BadTagName) {
    return Ok(());
  }
  let rest = match rest.strip_prefix(b"tagger ") {
    None => {
      // Early tags have no tagger: an info, reported as a warning, and the walk goes on.
      if report.report(FsckId::MissingTaggerEntry) {
        return Ok(());
      }
      rest
    }
    Some(after) => {
      let (advance, tagger) = ident::check(after);
      // git goes on to the extra-header check without stopping here; the finding is kept, and it refuses.
      if let Err(id) = tagger {
        report.report(id);
      }
      after.get(advance..).unwrap_or_default()
    }
  };
  if !rest.is_empty() && !rest.starts_with(b"\n") {
    report.report(FsckId::ExtraHeaderEntry);
  }
  Ok(())
}
