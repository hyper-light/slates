//! An identity line, `author`, `committer` or `tagger` (A-590): a name, a space, an email in angle brackets, a space, a
//! date in seconds since the epoch, a space and a time zone, checked as git's `fsck_ident` (fsck.c) checks it, in its
//! order, and read into its parts when it passes.
//!
//! git scans the line in place, stopping at its newline (or at a C string's NUL), and checks each byte past a stop the
//! way it checks the newline. Here a line is read up to its first newline, and a byte past it reads as that newline,
//! which answers every check as git's scan does.

use super::FsckId;

/// Format: the bytes `strcspn(p, "<>\n")` stops at in `fsck_ident`, with the NUL that ends a C string.
const STOPS: &[u8] = b"<>\n\0";

/// Format: the digits of a time zone after its sign, `+hhmm` (`fsck_ident` reads `p[1]` to `p[4]`).
const ZONE_DIGITS: usize = 4;

/// Format: the base of a date's digits (`parse_timestamp(p, &end, 10)`).
const DECIMAL: u64 = 10;

/// An identity's parts, from a line [`Ident::parse`] accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ident<'a> {
  /// The name: the bytes before the space before `<`, which may be none.
  pub name: &'a [u8],
  /// The email: the bytes between `<` and `>`.
  pub email: &'a [u8],
  /// The date in seconds since the epoch: at most `i64::MAX`, the most git's 64-bit `time_t` holds
  /// (`date_overflows`, date.c).
  pub seconds: u64,
  /// The time zone as written: a sign and four digits.
  pub zone: &'a [u8],
}

impl<'a> Ident<'a> {
  /// Reads `line`, an identity's value (a header's value, without the newline that ends it), as `fsck_ident` checks
  /// it: its parts, or the first finding git reports of it.
  pub fn parse(line: &'a [u8]) -> Result<Ident<'a>, FsckId> {
    if at(line, 0) == b'<' {
      return Err(FsckId::MissingNameBeforeEmail);
    }
    let open = until_stop(line, 0);
    match at(line, open) {
      b'<' => {}
      b'>' => return Err(FsckId::BadName),
      _ => return Err(FsckId::MissingEmail),
    }
    // `open` is past the first byte: a `<` there was refused above.
    if at(line, open.saturating_sub(1)) != b' ' {
      return Err(FsckId::MissingSpaceBeforeEmail);
    }
    let email_from = open.saturating_add(1);
    let close = until_stop(line, email_from);
    if at(line, close) != b'>' {
      return Err(FsckId::BadEmail);
    }
    if at(line, close.saturating_add(1)) != b' ' {
      return Err(FsckId::MissingSpaceBeforeDate);
    }
    let (seconds, zone_from) = date(line, skip_blanks(line, close.saturating_add(2)))?;
    Ok(Ident {
      name: line.get(..open.saturating_sub(1)).unwrap_or_default(),
      email: line.get(email_from..close).unwrap_or_default(),
      seconds,
      zone: zone(line, zone_from)?,
    })
  }
}

/// Checks the identity that begins `rest` (the bytes after `author `, `committer ` or `tagger `, to the object's end)
/// as `fsck_ident` does: how far its line runs, its newline included (git moves past it before it checks), and the
/// line's parts or the finding git reports of it.
pub(crate) fn check(rest: &[u8]) -> (usize, Result<Ident<'_>, FsckId>) {
  let newline = rest.iter().position(|byte| *byte == b'\n');
  let advance = newline.map_or(rest.len(), |at| at.saturating_add(1));
  let line = rest.get(..newline.unwrap_or(rest.len())).unwrap_or_default();
  (advance, Ident::parse(line))
}

/// The byte at `at`, or the newline that ends the line past its end.
fn at(line: &[u8], at: usize) -> u8 {
  line.get(at).copied().unwrap_or(b'\n')
}

/// The offset of the first of [`STOPS`] at or after `from`, or the line's end, where its newline stands.
fn until_stop(line: &[u8], from: usize) -> usize {
  line
    .get(from..)
    .and_then(|tail| tail.iter().position(|byte| STOPS.contains(byte)))
    .map_or(line.len(), |at| from.saturating_add(at))
}

/// Past the spaces and tabs from `from`: git allows more than the one space before a date ("we have traditionally
/// allowed extra whitespace").
fn skip_blanks(line: &[u8], from: usize) -> usize {
  let blanks = line
    .get(from..)
    .unwrap_or_default()
    .iter()
    .take_while(|byte| matches!(byte, b' ' | b'\t'))
    .count();
  from.saturating_add(blanks)
}

/// The date at `from`, read as git's `parse_timestamp` (`strtoumax`, base ten) reads it and judged as `fsck_ident`
/// judges it: its value and where the time zone begins, or the finding.
fn date(line: &[u8], from: usize) -> Result<(u64, usize), FsckId> {
  let first = at(line, from);
  if !first.is_ascii_digit() {
    return Err(FsckId::BadDate);
  }
  if first == b'0' && at(line, from.saturating_add(1)) != b' ' {
    return Err(FsckId::ZeroPaddedDate);
  }
  let digits = line
    .get(from..)
    .unwrap_or_default()
    .iter()
    .take_while(|byte| byte.is_ascii_digit())
    .count();
  let end = from.saturating_add(digits);
  // `strtoumax` answers its maximum past it, which `date_overflows` refuses as it refuses anything past `i64::MAX`.
  let seconds = line
    .get(from..end)
    .unwrap_or_default()
    .iter()
    .try_fold(0_u64, |value, digit| {
      value
        .checked_mul(DECIMAL)?
        .checked_add(u64::from(digit.wrapping_sub(b'0')))
    })
    .unwrap_or(u64::MAX);
  if i64::try_from(seconds).is_err() {
    return Err(FsckId::BadDateOverflow);
  }
  if at(line, end) != b' ' {
    return Err(FsckId::BadDate);
  }
  Ok((seconds, end.saturating_add(1)))
}

/// The time zone at `from`: a sign, four digits and the line's end; or `badTimezone`.
fn zone(line: &[u8], from: usize) -> Result<&[u8], FsckId> {
  let digits = (1..=ZONE_DIGITS).all(|offset| at(line, from.saturating_add(offset)).is_ascii_digit());
  let end = from.saturating_add(ZONE_DIGITS).saturating_add(1);
  if !matches!(at(line, from), b'+' | b'-') || !digits || at(line, end) != b'\n' {
    return Err(FsckId::BadTimezone);
  }
  Ok(line.get(from..end).unwrap_or_default())
}
