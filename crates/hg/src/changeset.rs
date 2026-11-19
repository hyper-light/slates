//! A changeset's text (A-588; `changelog.py` `changelog.add`, `changelogrevision`, `readfiles`, `encodeextra`,
//! `decodeextra`, `_string_escape` and `_string_unescape` \[C\]): the manifest's node in hexadecimal, the user, the
//! date line (the time, the timezone in seconds west of UTC, and the extras), the files the changeset touched one per
//! line, a blank line, and the description:
//!
//! ```text
//! <manifest hex>\n<user>\n<time> <timezone>[ <extras>]\n[<file>\n...]\n<description>
//! ```
//!
//! The extras are `key:value` items, each escaped (`\` as `\\`, a newline as `\n`, a carriage return as `\r`, NUL as
//! `\0`) and joined by NUL bytes, sorted by key. They are read as 7.2 reads them: its escaped NULs first (so `\0`
//! followed by digits is a NUL and the digits, never an octal escape), then Python's `escape_decode`, which also reads
//! the other escapes an older Mercurial's `string_escape` wrote (`\t`, `\xhh`, octal, `\'`).
//!
//! A changeset is read in place, every field as it is stored, and [`encode`] writes exactly the fields [`decode`]
//! accepts: whatever one reads, the other writes back to its bytes, and whatever one writes, the other reads back as
//! the same fields. The extras alone are unescaped, into the caller's buffer, their bytes and items counted and charged
//! to its admission first.
//!
//! **Refused, as 7.2 cannot read them.** A text without its three lines and its blank line (`text.index` raises), a
//! date line without its timezone (`_rawdate` has one field), and an extra 7.2's `decodeextra` raises on: no `:` once
//! unescaped, a broken `\x`, a trailing `\`.
//!
//! **Refused, stricter than 7.2 on purpose.** An empty text (7.2's null changeset: no manifest and no user); an empty
//! user, which 7.2's own writer refuses ("an empty username ... will make the revision text contain two "\n\n"
//! sequences"), since its empty line is the first blank one `readfiles` finds, so 7.2's two readers then disagree on
//! the files; a manifest field that is not forty hexadecimal digits (`bin` reads any even count); and a time that is
//! not a decimal number, as every Mercurial wrote (`"%d %d"`, and an older one's float), where 7.2 reads anything
//! Python's `float()` takes, only when asked. The timezone is kept as stored: "Various tools did silly things with the
//! timezone", and 7.2 reads one it cannot parse as 0.

use crate::node::from_hex;
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{Admission, Node, PastAdmission};

/// Format: the byte that ends the first three lines and each file's line.
const NEWLINE: u8 = b'\n';

/// Format: the bytes between the files and the description (`b"\n".join` around an empty element).
const BLANK_LINE: &[u8] = b"\n\n";

/// Format: the byte between the date line's time, timezone and extras.
const SPACE: u8 = b' ';

/// Format: the byte between extras.
const NUL: u8 = 0;

/// Format: the byte between an extra's key and value.
const COLON: u8 = b':';

/// Format: the byte that begins an escape.
const BACKSLASH: u8 = b'\\';

/// Format: a time's decimal point (an older Mercurial wrote times as floats).
const POINT: u8 = b'.';

/// Format: a time's sign.
const MINUS: u8 = b'-';

/// Format: each byte `_string_escape` escapes, and the letter after the backslash that stands for it.
const ESCAPED: [(u8, u8); 4] = [(b'\\', b'\\'), (b'\n', b'n'), (b'\r', b'r'), (NUL, b'0')];

/// Format: the escapes Python's `escape_decode` reads besides octal and `\x`, and the byte each stands for.
const PYTHON_ESCAPES: [(u8, u8); 10] = [
  (b'\\', b'\\'),
  (b'\'', b'\''),
  (b'"', b'"'),
  // Format: the escapes continue (the doc above).
  (b'a', 0x07),
  (b'b', 0x08),
  (b'f', 0x0c),
  (b'n', b'\n'),
  // Format: the escapes continue (the doc above).
  (b'r', b'\r'),
  (b't', b'\t'),
  (b'v', 0x0b),
];

/// Format: the escape `escape_decode` drops, a backslash before a newline (a line continuation).
const CONTINUATION: u8 = b'\n';

/// Format: the letter of a two-digit hexadecimal escape.
const HEX_ESCAPE: u8 = b'x';

/// Format: the digits of a hexadecimal escape.
const HEX_DIGITS: usize = 2;

/// Format: the most digits an octal escape reads.
const OCTAL_DIGITS: usize = 3;

/// Format: octal's base.
const OCTAL: u32 = 8;

/// A changeset's text, read in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Changeset<'a> {
  /// The manifest's node, its forty hexadecimal digits as stored.
  pub manifest: &'a [u8],
  /// The user.
  pub user: &'a [u8],
  /// The time in seconds since the epoch, as stored.
  pub time: &'a [u8],
  /// The timezone, seconds west of UTC, as stored.
  pub timezone: &'a [u8],
  /// The extras, escaped and separated by NUL bytes; `None` when the date line has none.
  pub extras: Option<&'a [u8]>,
  /// The files the changeset touched, one per line, without the last line's end; empty for none.
  pub files: &'a [u8],
  /// The description.
  pub description: &'a [u8],
}

/// Why a changeset was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A text without the lines and the blank line 7.2 reads.
  Shape {
    /// What was missing.
    what: &'static str,
  },
  /// An empty user.
  User,
  /// A manifest field that is not forty hexadecimal digits.
  Manifest,
  /// A time that is not a decimal number.
  Time,
  /// An extra 7.2 could not read: no `:` once unescaped, a broken `\x` or a trailing `\`.
  Extra {
    /// The extra's index among the date line's items.
    index: u64,
  },
  /// More than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Why a changeset or its extras could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A field that would not read back as itself: which.
  Field {
    /// The field.
    what: &'static str,
  },
}

/// The changeset a text holds, read in place, as 7.2's `changelogrevision` cuts it.
pub fn decode(text: &[u8]) -> Result<Changeset<'_>, Refusal> {
  let line = |from: usize, what: &'static str| {
    text
      .get(from..)
      .and_then(|rest| rest.iter().position(|byte| *byte == NEWLINE))
      .map(|end| from.saturating_add(end))
      .ok_or(Refusal::Shape { what })
  };
  let first = line(0, "the manifest line")?;
  let second = line(first.saturating_add(1), "the user line")?;
  let third = line(second.saturating_add(1), "the date line")?;
  let blank = if text.get(third.saturating_add(1)) == Some(&NEWLINE) {
    third
  } else {
    text
      .get(third.saturating_add(1)..)
      .and_then(|rest| {
        rest
          .windows(BLANK_LINE.len())
          .position(|window| window == BLANK_LINE)
      })
      .map(|at| third.saturating_add(1).saturating_add(at))
      .ok_or(Refusal::Shape {
        what: "the blank line",
      })?
  };
  let field = |from: usize, to: usize| text.get(from..to).unwrap_or_default();
  let (time, timezone, extras) = split_date(field(second.saturating_add(1), third))?;
  let changeset = Changeset {
    manifest: field(0, first),
    user: field(first.saturating_add(1), second),
    time,
    timezone,
    extras,
    files: if blank == third {
      &[]
    } else {
      field(third.saturating_add(1), blank)
    },
    description: text
      .get(blank.saturating_add(BLANK_LINE.len())..)
      .unwrap_or_default(),
  };
  changeset.manifest_node()?;
  if changeset.user.is_empty() {
    return Err(Refusal::User);
  }
  if !is_decimal(time) {
    return Err(Refusal::Time);
  }
  Ok(changeset)
}

/// A date line's time, its timezone and, if present, the extras after them.
type DateFields<'a> = (&'a [u8], &'a [u8], Option<&'a [u8]>);

/// A date line's time, timezone and extras (`split(b' ', 2)`).
fn split_date(date: &[u8]) -> Result<DateFields<'_>, Refusal> {
  // Format: `split(b' ', 2)` leaves at most three fields.
  let mut fields = date.splitn(3, |byte| *byte == SPACE);
  let time = fields.next().unwrap_or_default();
  let timezone = fields.next().ok_or(Refusal::Shape {
    what: "the timezone",
  })?;
  Ok((time, timezone, fields.next()))
}

/// Whether a time is a decimal number: an optional sign, digits, and digits after a point.
fn is_decimal(time: &[u8]) -> bool {
  let unsigned = time.strip_prefix(&[MINUS]).unwrap_or(time);
  let mut parts = unsigned.splitn(2, |byte| *byte == POINT);
  let whole = parts.next().unwrap_or_default();
  let fraction = parts.next().unwrap_or(b"0");
  let digits = |part: &[u8]| !part.is_empty() && part.iter().all(u8::is_ascii_digit);
  digits(whole) && digits(fraction)
}

/// A changeset's extras in their written order: each key with its value, borrowed from the decoding buffer.
type Extras<'b> = Vec<(&'b [u8], &'b [u8])>;

impl<'a> Changeset<'a> {
  /// The manifest's node.
  pub fn manifest_node(&self) -> Result<Node, Refusal> {
    from_hex(self.manifest).ok_or(Refusal::Manifest)
  }

  /// The files the changeset touched, in order.
  pub fn file_names(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
    let files = self.files;
    files
      .split(|byte| *byte == NEWLINE)
      .filter(move |_| !files.is_empty())
  }

  /// The extras, unescaped into `buffer` as 7.2 reads them (`decodeextra`), in their stored order, empty items passed
  /// over. A key stored twice is listed twice; 7.2's dictionary keeps the later value, and so must a reader of this
  /// list. Their bytes and items are counted and charged to `admission` first.
  pub fn extras<'b>(
    &self,
    admission: &mut Admission,
    buffer: &'b mut Vec<u8>,
  ) -> Result<Extras<'b>, Refusal> {
    let raw = self.extras.unwrap_or_default();
    let items = || {
      raw
        .split(|byte| *byte == NUL)
        .filter(|item| !item.is_empty())
        .enumerate()
    };
    let mut bytes = Measure(0);
    for (index, item) in items() {
      unescape(item, &mut bytes).ok_or(Refusal::Extra {
        index: to_u64(index),
      })?;
    }
    let count = items().count();
    let mut kept = *admission;
    kept.charge(bytes.0)?;
    kept.charge_items::<(&[u8], &[u8])>(count)?;
    buffer.clear();
    buffer.reserve_exact(usize::try_from(bytes.0).unwrap_or(usize::MAX));
    for (index, item) in items() {
      unescape(item, buffer).ok_or(Refusal::Extra {
        index: to_u64(index),
      })?;
    }
    let filled: &'b Vec<u8> = buffer;
    let mut read = Vec::with_capacity(count);
    let mut at: usize = 0;
    for (index, item) in items() {
      let mut length = Measure(0);
      unescape(item, &mut length).ok_or(Refusal::Extra {
        index: to_u64(index),
      })?;
      let end = at.saturating_add(usize::try_from(length.0).unwrap_or(usize::MAX));
      let unescaped = filled.get(at..end).unwrap_or_default();
      at = end;
      let colon = unescaped
        .iter()
        .position(|byte| *byte == COLON)
        .ok_or(Refusal::Extra {
          index: to_u64(index),
        })?;
      read.push((
        unescaped.get(..colon).unwrap_or_default(),
        unescaped.get(colon.saturating_add(1)..).unwrap_or_default(),
      ));
    }
    *admission = kept;
    Ok(read)
  }
}

/// Writes `escaped` read as `_string_unescape` reads it, or `None` where `escape_decode` would raise.
fn unescape(escaped: &[u8], out: &mut impl Out) -> Option<()> {
  let mut rest = escaped;
  while let Some((&byte, tail)) = rest.split_first() {
    if byte != BACKSLASH {
      out.put(&[byte]);
      rest = tail;
      continue;
    }
    rest = unescape_one(tail, out)?;
  }
  Some(())
}

/// Writes the escape after a backslash and answers what follows it: `\0` a NUL (7.2's fix-up before
/// `escape_decode`, which pairs backslashes from the left as this walk does), then `escape_decode`'s own rules.
fn unescape_one<'a>(tail: &'a [u8], out: &mut impl Out) -> Option<&'a [u8]> {
  let (&letter, after) = tail.split_first()?;
  if letter == b'0' {
    out.put(&[NUL]);
    return Some(after);
  }
  if letter == CONTINUATION {
    return Some(after);
  }
  if let Some(&(_, plain)) = PYTHON_ESCAPES.iter().find(|(known, _)| *known == letter) {
    out.put(&[plain]);
    return Some(after);
  }
  if letter == HEX_ESCAPE {
    let digits = after.get(..HEX_DIGITS)?;
    out.put(&[crate::quote::hex_pair(digits)?]);
    return after.get(HEX_DIGITS..);
  }
  // Format: an octal escape's digits are 0 to 7.
  if letter.is_ascii_digit() && letter < b'8' {
    return Some(octal(tail, out));
  }
  out.put(&[BACKSLASH, letter]);
  Some(after)
}

/// Writes the octal escape at the front of `digits` (one to three digits, its value taken a byte wide as CPython
/// takes it) and answers what follows it.
fn octal<'a>(digits: &'a [u8], out: &mut impl Out) -> &'a [u8] {
  let count = digits
    .iter()
    .take(OCTAL_DIGITS)
    // Format: an octal escape's digits are 0 to 7.
    .take_while(|digit| (b'0'..b'8').contains(*digit))
    .count();
  let value = digits.iter().take(count).fold(0_u32, |total, digit| {
    total
      .saturating_mul(OCTAL)
      .saturating_add(u32::from(digit.saturating_sub(b'0')))
  });
  out.put(&[value.to_le_bytes().first().copied().unwrap_or_default()]);
  digits.get(count..).unwrap_or_default()
}

/// Appends the extras' field as 7.2 writes it (`encodeextra`): each `key:value` escaped, sorted by key, joined by NUL
/// bytes. A key holding `:` is refused, since it would read back split at its own `:`, and so is a key given twice,
/// which 7.2's dictionary cannot hold and would read back as the later value alone.
pub fn encode_extras(extras: &[(&[u8], &[u8])], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  if extras.iter().any(|(key, _)| key.contains(&COLON)) {
    return Err(EncodeRefusal::Field {
      what: "an extra's key",
    });
  }
  let mut sorted: Vec<&(&[u8], &[u8])> = extras.iter().collect();
  sorted.sort_by(|left, right| left.0.cmp(right.0));
  if sorted.windows(2).any(|pair| match pair {
    [left, right] => left.0 == right.0,
    _ => false,
  }) {
    return Err(EncodeRefusal::Field {
      what: "an extra's key given twice",
    });
  }
  for (index, (key, value)) in sorted.into_iter().enumerate() {
    if index > 0 {
      out.push(NUL);
    }
    for byte in key.iter().chain(&[COLON]).chain(value.iter()) {
      match ESCAPED.iter().find(|(plain, _)| plain == byte) {
        Some(&(_, letter)) => out.extend_from_slice(&[BACKSLASH, letter]),
        None => out.push(*byte),
      }
    }
  }
  Ok(())
}

/// Appends a changeset's text from its fields as stored, after checking [`decode`] would read each back as itself.
pub fn encode(changeset: &Changeset<'_>, out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  check(changeset)?;
  write(changeset, out);
  Ok(())
}

/// The length of a changeset's text, for a caller that charges it first; refused as [`encode`] refuses.
pub fn encoded_len(changeset: &Changeset<'_>) -> Result<u64, EncodeRefusal> {
  check(changeset)?;
  let mut measure = Measure(0);
  write(changeset, &mut measure);
  Ok(measure.0)
}

/// Refuses a field [`decode`] would refuse or read back as another.
fn check(changeset: &Changeset<'_>) -> Result<(), EncodeRefusal> {
  let line = |field: &[u8]| !field.contains(&NEWLINE);
  let word = |field: &[u8]| line(field) && !field.contains(&SPACE);
  let files = changeset.files;
  let fields = [
    (from_hex(changeset.manifest).is_some(), "the manifest"),
    (
      !changeset.user.is_empty() && line(changeset.user),
      "the user",
    ),
    (is_decimal(changeset.time), "the time"),
    (word(changeset.timezone), "the timezone"),
    (changeset.extras.is_none_or(line), "the extras"),
    (
      files.is_empty()
        || (!files.starts_with(&[NEWLINE])
          && !files.ends_with(&[NEWLINE])
          && !files
            .windows(BLANK_LINE.len())
            .any(|window| window == BLANK_LINE)),
      "the files",
    ),
  ];
  match fields.iter().find(|(holds, _)| !holds) {
    Some(&(_, what)) => Err(EncodeRefusal::Field { what }),
    None => Ok(()),
  }
}

/// Writes a changeset that [`check`] passed.
fn write(changeset: &Changeset<'_>, out: &mut impl Out) {
  for field in [changeset.manifest, changeset.user] {
    out.put(field);
    out.put(&[NEWLINE]);
  }
  out.put(changeset.time);
  out.put(&[SPACE]);
  out.put(changeset.timezone);
  if let Some(extras) = changeset.extras {
    out.put(&[SPACE]);
    out.put(extras);
  }
  out.put(&[NEWLINE]);
  if !changeset.files.is_empty() {
    out.put(changeset.files);
    out.put(&[NEWLINE]);
  }
  out.put(&[NEWLINE]);
  out.put(changeset.description);
}

#[cfg(test)]
mod tests {
  use super::{Changeset, EncodeRefusal, Refusal, decode, encode, encode_extras, encoded_len};
  use crate::Admission;
  use proptest::prelude::*;

  fn room() -> Admission {
    Admission::new(1 << 20)
  }

  const MANIFEST: &[u8] = b"38a51fca0cbc207c4af656034604724b2e089ff6";

  /// The text `encode` writes for `changeset`, checked at the length `encoded_len` gave.
  fn written(changeset: &Changeset<'_>) -> Result<Vec<u8>, EncodeRefusal> {
    let mut out = Vec::new();
    encode(changeset, &mut out)?;
    assert_eq!(
      encoded_len(changeset),
      Ok(u64::try_from(out.len()).unwrap())
    );
    Ok(out)
  }

  /// A-588: do read a changeset with files and one without, as 7.2 writes them, and one whose timezone is empty, as a
  /// tool's "silly" timezone may be. Expect each field, the empty file list where the date line is followed by the
  /// blank line, the empty timezone kept, and each written back to its bytes at the length `encoded_len` gave.
  #[test]
  fn changesets_read_and_write_back() {
    let with_files = [
      MANIFEST,
      b"\nTest <t@e>\n0 0\na\nb/c\n\nsome\n\ndescription",
    ]
    .concat();
    let read = decode(&with_files).unwrap();
    assert_eq!(
      (read.user, read.time, read.timezone, read.extras),
      (&b"Test <t@e>"[..], &b"0"[..], &b"0"[..], None)
    );
    assert_eq!(read.file_names().collect::<Vec<_>>(), [&b"a"[..], b"b/c"]);
    assert_eq!(read.description, b"some\n\ndescription");
    let without = [MANIFEST, b"\nTest\n12 -3600 branch:stable\n\nopen"].concat();
    let empty = decode(&without).unwrap();
    assert_eq!((empty.files, empty.file_names().count()), (&b""[..], 0));
    let silly = [MANIFEST, b"\nTest\n12 \n\nx"].concat();
    let no_zone = decode(&silly).unwrap();
    assert_eq!((no_zone.timezone, no_zone.extras), (&b""[..], None));
    for (text, changeset) in [(&with_files, read), (&without, empty), (&silly, no_zone)] {
      assert_eq!(written(&changeset).as_ref(), Ok(text));
    }
  }

  /// A-588: do write extras holding every byte 7.2 escapes, a NUL before digits among them, and read them back. Expect
  /// 7.2's bytes (sorted keys, `\0`, `\\`, `\n`, `\r`, NUL between items) and the same extras, the NUL before `12` a
  /// NUL and the digits rather than an octal escape.
  #[test]
  fn extras_escape_and_read_back_as_mercurial_does() {
    let value: &[u8] = b"a\nb\x00c\\d\re\x0012:tail";
    let mut field = Vec::new();
    encode_extras(&[(b"source", b"svn:1"), (b"note", value)], &mut field).unwrap();
    assert_eq!(field, b"note:a\\nb\\0c\\\\d\\re\\012:tail\x00source:svn:1");
    let text = [MANIFEST, b"\nT\n0 0 ", &field, b"\n\nx"].concat();
    let mut buffer = Vec::new();
    let extras = decode(&text)
      .unwrap()
      .extras(&mut room(), &mut buffer)
      .unwrap();
    let expected: [(&[u8], &[u8]); 2] = [(b"note", value), (b"source", b"svn:1")];
    assert_eq!(extras, expected);
  }

  /// A-588: do read extras an older Mercurial's `string_escape` wrote (`\t`, `\x41`, octal `\101`, `\'`), an escape
  /// Python keeps as it is (`\q`), an octal escape past a byte (`\777`, which CPython takes a byte wide), and
  /// backslashes before a `0` in odd and even runs (7.2 pairs them from the left before its `\0` fix-up). Expect the
  /// bytes 7.2 reads: `\t`, `A`, `A`, `'`, `\q`, 0xFF, then a backslash and a NUL, then a backslash and `0`.
  #[test]
  fn older_escapes_read_as_python_reads_them() {
    let text = [
      MANIFEST,
      b"\nT\n0 0 k:\\t\\x41\\101\\'\\q\\777\\\\\\0\\\\0\n\nx",
    ]
    .concat();
    let mut buffer = Vec::new();
    let extras = decode(&text)
      .unwrap()
      .extras(&mut room(), &mut buffer)
      .unwrap();
    assert_eq!(extras, [(&b"k"[..], &b"\tAA'\\q\xff\\\x00\\0"[..])]);
  }

  /// A-588: do read extras naming one key twice and one admission too small for them. Expect both items in their stored
  /// order (7.2's dictionary keeps the later), and the small admission refused with nothing charged.
  #[test]
  fn extras_are_listed_as_stored_and_charged_first() {
    let text = [MANIFEST, b"\nT\n0 0 branch:a\x00branch:b\n\nx"].concat();
    let read = decode(&text).unwrap();
    let mut buffer = Vec::new();
    let both = read.extras(&mut room(), &mut buffer).unwrap();
    let expected: [(&[u8], &[u8]); 2] = [(b"branch", b"a"), (b"branch", b"b")];
    assert_eq!(both, expected);
    let mut small = Admission::new(4);
    assert!(matches!(
      read.extras(&mut small, &mut Vec::new()),
      Err(Refusal::PastAdmission(_))
    ));
    assert_eq!(small.left(), 4);
  }

  /// AC-HG.9 (hostile input), A-588: do read an empty text, texts cut before each of their lines and before the blank
  /// line, a manifest that is not hexadecimal, an empty user, a date without a timezone, a time that is not a number,
  /// and extras 7.2 cannot read. Expect each refused by its kind.
  #[test]
  fn changesets_mercurial_would_misread_are_refused() {
    assert_eq!(
      decode(b""),
      Err(Refusal::Shape {
        what: "the manifest line"
      })
    );
    let whole = [MANIFEST, b"\nT\n0 0\na\n\nd"].concat();
    for (cut, what) in [
      (41, "the user line"),
      (43, "the date line"),
      (47, "the blank line"),
    ] {
      assert_eq!(decode(&whole[..cut]), Err(Refusal::Shape { what }));
    }
    let not_hex = [&b"zz"[..], &MANIFEST[2..], b"\nT\n0 0\n\nd"].concat();
    assert_eq!(decode(&not_hex), Err(Refusal::Manifest));
    let no_user = [MANIFEST, b"\n\n0 0\n\nd"].concat();
    assert_eq!(decode(&no_user), Err(Refusal::User));
    let no_zone = [MANIFEST, b"\nT\n0\n\nd"].concat();
    assert_eq!(
      decode(&no_zone),
      Err(Refusal::Shape {
        what: "the timezone"
      })
    );
    let bad_time = [MANIFEST, b"\nT\n1e9 0\n\nd"].concat();
    assert_eq!(decode(&bad_time), Err(Refusal::Time));
    for extras in [&b"nocolon"[..], b"k:\\x4", b"k:v\\"] {
      let text = [MANIFEST, b"\nT\n0 0 ", extras, b"\n\nd"].concat();
      let read = decode(&text).unwrap();
      assert_eq!(
        read.extras(&mut room(), &mut Vec::new()).map(|_| ()),
        Err(Refusal::Extra { index: 0 })
      );
    }
  }

  /// A-588: do write a manifest that is not hexadecimal, a user holding a newline, an empty user, a time that is not a
  /// number, a timezone holding a space, files beginning with a line end, an extra's key holding `:`, and a key given
  /// twice. Expect each refused naming its field, nothing written.
  #[test]
  fn fields_that_would_not_read_back_are_not_written() {
    let base = Changeset {
      manifest: MANIFEST,
      user: b"T",
      time: b"0",
      timezone: b"0",
      extras: None,
      files: b"",
      description: b"",
    };
    let not_hex = [&b"zz"[..], &MANIFEST[2..]].concat();
    let cases = [
      (
        Changeset {
          manifest: &not_hex,
          ..base
        },
        "the manifest",
      ),
      (
        Changeset {
          user: b"a\nb",
          ..base
        },
        "the user",
      ),
      (Changeset { user: b"", ..base }, "the user"),
      (
        Changeset {
          time: b"1e9",
          ..base
        },
        "the time",
      ),
      (
        Changeset {
          timezone: b"0 1",
          ..base
        },
        "the timezone",
      ),
      (
        Changeset {
          files: b"\na",
          ..base
        },
        "the files",
      ),
    ];
    for (changeset, what) in cases {
      let mut out = Vec::new();
      assert_eq!(
        encode(&changeset, &mut out),
        Err(EncodeRefusal::Field { what })
      );
      assert_eq!(encoded_len(&changeset), Err(EncodeRefusal::Field { what }));
      assert!(out.is_empty());
    }
    assert_eq!(
      encode_extras(&[(b"a:b", b"c")], &mut Vec::new()),
      Err(EncodeRefusal::Field {
        what: "an extra's key"
      })
    );
    assert_eq!(
      encode_extras(&[(b"k", b"1"), (b"j", b"2"), (b"k", b"3")], &mut Vec::new()),
      Err(EncodeRefusal::Field {
        what: "an extra's key given twice"
      })
    );
  }

  /// The fields a writer may give a changeset.
  #[derive(Debug)]
  struct Fields {
    /// The manifest's node.
    manifest: [u8; 20],
    /// A user: no newline, not empty.
    user: Vec<u8>,
    /// A decimal time.
    time: String,
    /// A timezone: no space, no newline.
    timezone: String,
    /// Extras, or none at all.
    extras: Option<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>,
    /// File names: no newline, not empty.
    files: Vec<Vec<u8>>,
    /// A description.
    description: Vec<u8>,
  }

  /// Generated [`Fields`].
  fn fields() -> impl Strategy<Value = Fields> {
    let no_newline = |most: usize| {
      proptest::collection::vec(
        any::<u8>().prop_filter("no newline", |byte| *byte != b'\n'),
        1..most,
      )
    };
    let key = proptest::collection::vec(
      any::<u8>().prop_filter("no colon", |byte| *byte != b':'),
      0..6,
    );
    let extras = proptest::option::of(proptest::collection::btree_map(
      key,
      proptest::collection::vec(any::<u8>(), 0..8),
      0..4,
    ));
    (
      any::<[u8; 20]>(),
      no_newline(12),
      "-?[0-9]{1,10}(\\.[0-9]{1,3})?",
      "[^ \n]{0,6}",
      extras,
      proptest::collection::vec(no_newline(8), 0..4),
      proptest::collection::vec(any::<u8>(), 0..24),
    )
      .prop_map(
        |(manifest, user, time, timezone, extras, files, description)| Fields {
          manifest,
          user,
          time,
          timezone,
          extras,
          files,
          description,
        },
      )
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-588: do write extras of any bytes and read them back. Expect the same extras, sorted by key.
    #[test]
    fn any_extras_read_back(
      generated in proptest::collection::btree_map("[a-z]{1,6}", proptest::collection::vec(any::<u8>(), 0..12), 0..4),
    ) {
      let pairs: Vec<(&[u8], &[u8])> =
        generated.iter().map(|(key, value)| (key.as_bytes(), value.as_slice())).collect();
      let mut field = Vec::new();
      encode_extras(&pairs, &mut field).unwrap();
      let text = [MANIFEST, b"\nT\n0 0 ", &field, b"\n\nx"].concat();
      let mut buffer = Vec::new();
      let read = decode(&text).unwrap().extras(&mut room(), &mut buffer).unwrap();
      prop_assert_eq!(read, pairs);
    }

    /// A-588: do write changesets of generated fields (any manifest node, user, decimal time, timezone, extras, files
    /// and description) and read them. Expect each read back as the same fields.
    #[test]
    fn written_changesets_read_back_as_written(generated in fields()) {
      let manifest = crate::fixtures::hex(&generated.manifest);
      let mut extras_field = Vec::new();
      if let Some(extras) = &generated.extras {
        let pairs: Vec<(&[u8], &[u8])> = extras.iter().map(|(key, value)| (key.as_slice(), value.as_slice())).collect();
        encode_extras(&pairs, &mut extras_field).unwrap();
      }
      let joined = generated.files.join(&b'\n');
      let changeset = Changeset {
        manifest: manifest.as_bytes(),
        user: &generated.user,
        time: generated.time.as_bytes(),
        timezone: generated.timezone.as_bytes(),
        extras: generated.extras.as_ref().map(|_| extras_field.as_slice()),
        files: &joined,
        description: &generated.description,
      };
      let text = written(&changeset).unwrap();
      prop_assert_eq!(decode(&text), Ok(changeset));
    }

    /// AC-HG.9, A-588: do read arbitrary bytes as a changeset and its extras. Expect a refusal, or a changeset written
    /// back to exactly the same bytes; never a panic.
    #[test]
    fn arbitrary_bytes_read_back_or_are_refused(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
      let text = [MANIFEST, b"\n", &bytes].concat();
      if let Ok(read) = decode(&text) {
        prop_assert_eq!(written(&read), Ok(text.clone()));
        let mut buffer = Vec::new();
        let _extras = read.extras(&mut room(), &mut buffer).map(|extras| extras.len());
      }
    }
  }
}
