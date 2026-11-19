//! A commit object (A-590): its headers (`tree`, each `parent`, `author`, `committer`, then `encoding`, `gpgsig`,
//! `gpgsig-sha256`, `mergetag` and any other, each with its continuation lines) and its message, parsed as written,
//! written back byte for byte, and checked as git's `fsck_commit` (fsck.c) checks them.
//!
//! A signed commit carries its signature in a header of continuation lines: `gpgsig` in a SHA-1 repository and
//! `gpgsig-sha256` in a SHA-256 one, and a commit may carry both (gitformat-signature). What a verifier reads is
//! commit.c's `parse_buffer_signed_by_header` \[C, v2.50.1\]: the signature is every header of the repository's own
//! name, unfolded in order ([`Commit::write_signature`]), and it covers the commit without any header whose line begins
//! `gpgsig` ([`Commit::write_signed_payload`]; A-644). A merge of a signed tag carries the whole tag in a `mergetag`
//! header ([`Header::unfold`] writes it back out).

use super::fsck::{FsckId, Report, Strictness};
use super::headers::{self, Header};
use super::{ObjectRefused, hex, ident};
use crate::Admission;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::out::Out;

/// Format: the header a SHA-1 repository's commit signature is in (commit.c's `gpg_sig_headers`).
const SHA1_SIGNATURE: &[u8] = b"gpgsig";
/// Format: the header a SHA-256 repository's commit signature is in.
const SHA256_SIGNATURE: &[u8] = b"gpgsig-sha256";
/// Format: the headers git gives a meaning of its own to; any other is an extra header (gitformat-signature).
const KNOWN: [&[u8]; 8] = [
  b"tree",
  b"parent",
  b"author",
  b"committer",
  b"encoding",
  SHA1_SIGNATURE,
  SHA256_SIGNATURE,
  b"mergetag",
];

/// A commit: its headers in the order written, and its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit<'a> {
  /// The headers.
  headers: Vec<Header<'a>>,
  /// The message after the empty line; none when the headers end the commit.
  message: Option<&'a [u8]>,
}

impl<'a> Commit<'a> {
  /// Parses `bytes`, a commit's content, charging its headers to `admission` before any is kept. Only what git refuses
  /// to read at all is refused here (a NUL in the headers, or headers that end with no newline); the rest is
  /// [`fsck`]'s to judge, so a commit git once wrote is still read and written back as it was.
  pub fn parse(bytes: &'a [u8], admission: &mut Admission) -> Result<Self, ObjectRefused> {
    let (headers, message) = headers::read(bytes, admission)?;
    Ok(Self { headers, message })
  }

  /// Every header in the order written.
  pub fn headers(&self) -> &[Header<'a>] {
    &self.headers
  }

  /// The message, after the empty line that ends the headers.
  pub fn message(&self) -> Option<&'a [u8]> {
    self.message
  }

  /// The value of the first header named `name`.
  fn first(&self, name: &[u8]) -> Option<&'a [u8]> {
    self
      .headers
      .iter()
      .find(|header| header.name == name)
      .and_then(|header| header.value)
  }

  /// The root tree's id in `format`, from the first header, which must be `tree` (commit.c's `parse_commit_buffer`
  /// reads it there); none when it is not one.
  pub fn tree(&self, format: ObjectFormat) -> Option<ObjectId> {
    let first = self
      .headers
      .first()
      .filter(|header| header.name == b"tree")?;
    ObjectId::from_hex(format, first.value?).ok()
  }

  /// Each parent's id in `format`: the `parent` headers right after `tree`, in order. For a commit fsck passed that is
  /// every parent; the walk stops at a parent that is not an id.
  pub fn parents(&self, format: ObjectFormat) -> impl Iterator<Item = ObjectId> + '_ {
    self
      .headers
      .iter()
      .skip(1)
      .take_while(|header| header.name == b"parent")
      .map_while(move |header| ObjectId::from_hex(format, header.value?).ok())
  }

  /// The `author` header's value; [`super::Ident::parse`] reads its parts.
  pub fn author(&self) -> Option<&'a [u8]> {
    self.first(b"author")
  }

  /// The `committer` header's value.
  pub fn committer(&self) -> Option<&'a [u8]> {
    self.first(b"committer")
  }

  /// The `encoding` header's value: the message's encoding, when it is not UTF-8.
  pub fn encoding(&self) -> Option<&'a [u8]> {
    self.first(b"encoding")
  }

  /// The signature header of `format`'s repository (`gpgsig`, or `gpgsig-sha256`), whose lines
  /// ([`Header::unfold`]) are the signature.
  pub fn signature(&self, format: ObjectFormat) -> Option<&Header<'a>> {
    let name = signature_name(format);
    self.headers.iter().find(|header| header.name == name)
  }

  /// Writes the signature a verifier reads (A-644): every header of `format`'s repository's own signature name, each
  /// unfolded, in order, as commit.c's `parse_buffer_signed_by_header` gathers them; whether there was one. Two such
  /// headers are one signature of both, which no armour reads as one, so a verifier refuses it.
  pub fn write_signature(&self, _format: ObjectFormat, _out: &mut impl Out) -> bool {
    false
  }

  /// The `mergetag` headers: each a whole tag object, which [`Header::unfold`] writes out.
  pub fn mergetags(&self) -> impl Iterator<Item = &Header<'a>> + '_ {
    self
      .headers
      .iter()
      .filter(|header| header.name == b"mergetag")
  }

  /// The headers git gives no meaning of its own, kept as written (gitformat-signature, "extra headers").
  pub fn extra_headers(&self) -> impl Iterator<Item = &Header<'a>> + '_ {
    self
      .headers
      .iter()
      .filter(|header| !KNOWN.contains(&header.name))
  }

  /// Writes the commit as it was read.
  pub fn write(&self, out: &mut impl Out) {
    headers::write(&self.headers, self.message, out);
  }

  /// Writes what a signature covers: the commit without any header whose line begins `gpgsig`, as commit.c's
  /// `parse_buffer_signed_by_header` leaves out the repository's own signature header, the other format's, and any
  /// other header so named (A-644: a payload that kept one would verify what git does not).
  pub fn write_signed_payload(&self, out: &mut impl Out) {
    for header in &self.headers {
      if header.name != SHA1_SIGNATURE && header.name != SHA256_SIGNATURE {
        header.write(out);
      }
    }
    if let Some(message) = self.message {
      out.put(b"\n");
      out.put(message);
    }
  }
}

/// The header `format`'s repository keeps a commit's signature in (commit.c's `gpg_sig_headers`).
fn signature_name(format: ObjectFormat) -> &'static [u8] {
  match format {
    ObjectFormat::Sha1 => SHA1_SIGNATURE,
    ObjectFormat::Sha256 => SHA256_SIGNATURE,
  }
}

/// Checks `bytes`, a commit's content in a repository of `format`, as git's `fsck_commit` does under `strictness`:
/// the headers whole, then `tree`, each `parent`, one `author`, `committer`, each identity, and a NUL anywhere. The
/// check stops at its first error, as git's does.
pub fn fsck(bytes: &[u8], format: ObjectFormat, strictness: Strictness) -> Report {
  let mut report = Report::new(strictness);
  if let Err(refused) = headers::verify(bytes) {
    if let Some(id) = refused.refusal.fsck_id() {
      report.report(id);
    }
    return report;
  }
  if let Err(id) = check(bytes, format) {
    report.report(id);
    return report;
  }
  if bytes.contains(&b'\0') {
    report.report(FsckId::NulInCommit);
  }
  report
}

/// `fsck_commit`'s header walk, once the headers are whole: the first finding, or Ok once `committer` passes. Every
/// finding before `nulInCommit` is an error under either strictness, so the first ends the walk.
fn check(bytes: &[u8], format: ObjectFormat) -> Result<(), FsckId> {
  let mut rest = bytes.strip_prefix(b"tree ").ok_or(FsckId::MissingTree)?;
  rest = after_id(rest, format).ok_or(FsckId::BadTreeSha1)?;
  while let Some(after) = rest.strip_prefix(b"parent ") {
    rest = after_id(after, format).ok_or(FsckId::BadParentSha1)?;
  }
  let mut authors = 0_usize;
  while let Some(after) = rest.strip_prefix(b"author ") {
    authors = authors.saturating_add(1);
    let (advance, line) = ident::check(after);
    line?;
    rest = after.get(advance..).unwrap_or_default();
  }
  match authors {
    0 => return Err(FsckId::MissingAuthor),
    1 => {}
    _ => return Err(FsckId::MultipleAuthors),
  }
  let after = rest
    .strip_prefix(b"committer ")
    .ok_or(FsckId::MissingCommitter)?;
  ident::check(after).1.map(|_| ())
}

/// The bytes after an id of `format` and its newline at the start of `text`; none when `text` does not begin so.
fn after_id(text: &[u8], format: ObjectFormat) -> Option<&[u8]> {
  let id = hex::id_then(text, format, b'\n')?;
  text.get(id.len().saturating_add(1)..)
}
