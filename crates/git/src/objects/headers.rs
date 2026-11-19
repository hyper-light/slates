//! What a commit and a tag share (A-590): header lines, each with the continuation lines after it, then an empty line
//! and the message.
//!
//! git checks a commit's or a tag's headers before it reads them (`verify_headers`, fsck.c): a NUL before the empty
//! line is `nulInHeader`, and headers with no newline after the last are `unterminatedHeader`, both fatal. Past that
//! check every header line ends in a newline, which is what lets git's own scans stop there. A line that begins with a
//! space continues the header before it, as `gpgsig` and `mergetag` use them (gitformat-signature). A header whose name
//! git gives no meaning of its own is kept as written, so writing a commit or a tag gives back the bytes it was read
//! from.

use super::{ObjectRefused, Refusal};
use crate::out::Out;
use crate::Admission;

/// One header of a commit or a tag, as written: its name, the bytes before the first space of its first line; and its
/// value, the bytes after that space up to the newline that ends its last continuation line, the newlines and the
/// leading spaces of continuation lines included. A first line with no space has no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
  /// The name.
  pub name: &'a [u8],
  /// The value, continuation lines included as written.
  pub value: Option<&'a [u8]>,
}

impl<'a> Header<'a> {
  /// The value's lines as git reads a header that runs over several (`read_commit_extra_header_lines`, commit.c): the
  /// first line, then each continuation line without the space that marks it. None for a header with no value.
  pub fn lines(&self) -> impl Iterator<Item = &'a [u8]> + 'a {
    self.value.into_iter().flat_map(|value| {
      value
        .split(|byte| *byte == b'\n')
        .enumerate()
        .map(|(index, line)| match index {
          0 => line,
          _ => line.strip_prefix(b" ").unwrap_or(line),
        })
    })
  }

  /// Writes the text the value carries, each line ended by a newline: a `gpgsig` header's signature as git hands it to
  /// the verifier, or the whole tag a `mergetag` header carries.
  pub fn unfold(&self, out: &mut impl Out) {
    for line in self.lines() {
      out.put(line);
      out.put(b"\n");
    }
  }

  /// Writes the header as it was read: its name, a space and its value when it has one, and the newline.
  pub fn write(&self, out: &mut impl Out) {
    out.put(self.name);
    if let Some(value) = self.value {
      out.put(b" ");
      out.put(value);
    }
    out.put(b"\n");
  }
}

/// git's `verify_headers`: a NUL before the first empty line, or headers that end with no newline, refused at its
/// place; Ok when the headers are whole.
pub(crate) fn verify(bytes: &[u8]) -> Result<(), ObjectRefused> {
  let mut previous = None;
  for (at, byte) in bytes.iter().enumerate() {
    match byte {
      b'\0' => {
        return Err(ObjectRefused {
          at,
          refusal: Refusal::NulInHeader,
        });
      }
      b'\n' if previous == Some(b'\n') => return Ok(()),
      _ => previous = Some(*byte),
    }
  }
  if bytes.last() == Some(&b'\n') {
    return Ok(());
  }
  Err(ObjectRefused {
    at: bytes.len(),
    refusal: Refusal::UnterminatedHeader,
  })
}

/// The headers' bytes, each line with its newline, and the message after the empty line, once [`verify`] passed. The
/// message is none when no empty line ends the headers.
pub(crate) fn split(bytes: &[u8]) -> (&[u8], Option<&[u8]>) {
  match bytes.windows(2).position(|pair| pair == b"\n\n") {
    Some(at) => {
      let (headers, rest) = bytes.split_at(at.saturating_add(1));
      (headers, Some(rest.get(1..).unwrap_or_default()))
    }
    None => (bytes, None),
  }
}

/// Parses a commit's or a tag's content into its headers and message: the headers checked as [`verify`] checks them,
/// then counted, charged to `admission`, and kept.
pub(crate) fn read<'a>(
  bytes: &'a [u8],
  admission: &mut Admission,
) -> Result<(Vec<Header<'a>>, Option<&'a [u8]>), ObjectRefused> {
  verify(bytes)?;
  let (head, message) = split(bytes);
  let count = lines(head).count();
  admission
    .charge_items::<Header<'a>>(count)
    .map_err(|past| ObjectRefused {
      at: 0,
      refusal: Refusal::PastAdmission(past),
    })?;
  let mut kept = Vec::with_capacity(count);
  kept.extend(lines(head));
  Ok((kept, message))
}

/// Each header of `headers` (whole lines, each ending in a newline) in order: a line, and every line after it that
/// begins with a space.
fn lines(headers: &[u8]) -> impl Iterator<Item = Header<'_>> {
  let mut rest = headers;
  core::iter::from_fn(move || {
    if rest.is_empty() {
      return None;
    }
    let mut end = newline_from(rest, 0);
    while rest.get(end.saturating_add(1)) == Some(&b' ') {
      end = newline_from(rest, end.saturating_add(1));
    }
    let line = rest.get(..end).unwrap_or_default();
    rest = rest.get(end.saturating_add(1)..).unwrap_or_default();
    Some(match line.iter().position(|byte| *byte == b' ') {
      Some(space) => Header {
        name: line.get(..space).unwrap_or_default(),
        value: line.get(space.saturating_add(1)..),
      },
      None => Header {
        name: line,
        value: None,
      },
    })
  })
}

/// The offset of the first newline in `bytes` at or after `from`, or the length when there is none.
fn newline_from(bytes: &[u8], from: usize) -> usize {
  bytes
    .get(from..)
    .and_then(|tail| tail.iter().position(|byte| *byte == b'\n'))
    .map_or(bytes.len(), |at| from.saturating_add(at))
}

/// Writes `headers` and the message after them as they were read.
pub(crate) fn write(headers: &[Header<'_>], message: Option<&[u8]>, out: &mut impl Out) {
  for header in headers {
    header.write(out);
  }
  if let Some(message) = message {
    out.put(b"\n");
    out.put(message);
  }
}
