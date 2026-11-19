//! Git LFS on the server side (A-610; §4.17.2 "Large files", piece 12): the batch API, the basic transfer adapter's
//! actions, the locks API and the pointer file, read and written as pure codecs over bytes.
//!
//! **What a server does with them.** A client asks the batch API (`POST <repo>.git/info/lfs/objects/batch`) to
//! download or upload objects named by SHA-256 ([`batch`]); the answer gives each object the actions of the basic
//! adapter (an `href`, headers, an expiry) or an error with one of LFS's codes; the bytes then move by plain `PUT` and
//! `GET`, and an upload may be confirmed by a verify callback. Locks are per path: created, listed with a cursor,
//! listed for a push's verification as ours and theirs, and released, by force when another user holds them
//! ([`locks`]). A version's tree holds pointer files in place of the large files ([`pointer`]). [`route`] names the
//! endpoint a path under `<repo>.git/info/lfs` asks for. The HTTP server (§4.17.2 piece 8) carries these; the objects
//! themselves are content in the volume store (§4.17.2 "Large files").
//!
//! **Sources** (read 2026-11-19): git-lfs v3.8.1 (tag object 36e29152, commit a662a6dbf, released 2026-10-22):
//! `docs/api/batch.md`, `basic-transfers.md`, `locking.md`, `server-discovery.md`, `docs/spec.md` [B]; the JSON schemas
//! `tq/schemas/` and `locking/schemas/` [B]; and what the stock client runs [C]: its request structs (`tq/api.go`,
//! `tq/transfer.go`, `locking/api.go`), its pointer reader (`lfs/pointer.go`) and its test server
//! (`t/cmd/lfstest-gitserver.go`). Where the documents and the client differ, the client is followed, and the module
//! says so.
//!
//! **Bounds (§4.9, §4.2).** Every body is untrusted. JSON is read by [`json`]: validated first, as a loop, its
//! nesting's bookkeeping charged to the caller's [`Admission`](crate::Admission); then walked in place. A decoded
//! message borrows its strings from the body; the only things built are a batch request's lists, whose exact size is
//! charged before they are. No limit is a literal: the body's length is the request's, and the rest is charged.
//!
//! **Refusals by kind.** Malformed JSON, a field of the wrong type, a missing or repeated field, an unknown operation,
//! an oid that is not 64 lower-case hexadecimal digits, a negative size, a number past what git-lfs reads (Go's
//! `int64`), a hash other than SHA-256: each is a variant naming where it stood.

pub mod batch;
pub mod json;
pub mod locks;
pub mod objects;
pub mod pointer;
pub mod serve;

use crate::out::Out;

/// Format: an LFS object id's bytes, SHA-256's [B: spec.md: "Currently, only sha256 is supported"].
const OID_LEN: usize = 32;

/// Format: an oid's hexadecimal digits, two a byte, lower case (pointer.go's `oidRE`, `\A[0-9a-f]{64}\z`).
const OID_HEX_LEN: usize = 64;

/// Format: the digits an oid is written in, lower case.
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// Format: the bits one hexadecimal digit carries.
const HEX_DIGIT_BITS: u32 = 4;

/// Format: the bits of a byte its low hexadecimal digit carries.
const LOW_DIGIT: u8 = 0x0f;

/// An LFS object's id: the SHA-256 of its content, written as 64 lower-case hexadecimal digits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid([u8; OID_LEN]);

impl Oid {
  /// The id `hex` writes: exactly 64 lower-case hexadecimal digits, the only form git-lfs reads (pointer.go's `oidRE`)
  /// and writes. Upper case is refused, so one object has one name.
  pub fn from_hex(hex: &[u8]) -> Option<Oid> {
    if hex.len() != OID_HEX_LEN {
      return None;
    }
    let mut bytes = [0u8; OID_LEN];
    for (slot, pair) in bytes.iter_mut().zip(hex.chunks_exact(2)) {
      let high = lower_hex_value(*pair.first()?)?;
      let low = lower_hex_value(*pair.get(1)?)?;
      *slot = high.checked_shl(HEX_DIGIT_BITS)? | low;
    }
    Some(Oid(bytes))
  }

  /// The id of `bytes`, SHA-256's 32.
  pub fn from_bytes(bytes: [u8; OID_LEN]) -> Oid {
    Oid(bytes)
  }

  /// The id's bytes.
  pub fn as_bytes(&self) -> &[u8; OID_LEN] {
    &self.0
  }

  /// Writes the id's 64 lower-case hexadecimal digits.
  pub fn put_hex(&self, out: &mut impl Out) {
    for byte in self.0 {
      let high = LOWER_HEX.get(usize::from(byte.checked_shr(HEX_DIGIT_BITS).unwrap_or(0)));
      let low = LOWER_HEX.get(usize::from(byte & LOW_DIGIT));
      for digit in [high, low].into_iter().flatten() {
        out.put(&[*digit]);
      }
    }
  }
}

impl AsRef<[u8]> for Oid {
  /// The id's 32 bytes: how `slates-vcs` keys an object (A-629).
  fn as_ref(&self) -> &[u8] {
    &self.0
  }
}

/// The value of one lower-case hexadecimal digit.
fn lower_hex_value(digit: u8) -> Option<u8> {
  LOWER_HEX
    .iter()
    .position(|known| *known == digit)
    .and_then(|value| u8::try_from(value).ok())
}

impl core::fmt::Display for Oid {
  /// The id's 64 lower-case hexadecimal digits.
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    for byte in self.0 {
      let high = LOWER_HEX.get(usize::from(byte.checked_shr(HEX_DIGIT_BITS).unwrap_or(0)));
      let low = LOWER_HEX.get(usize::from(byte & LOW_DIGIT));
      for digit in [high, low].into_iter().flatten() {
        core::fmt::Write::write_char(f, char::from(*digit))?;
      }
    }
    Ok(())
  }
}

impl core::fmt::Debug for Oid {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "Oid({self})")
  }
}

/// Format: the one hash git-lfs names objects with, and the batch API's `hash_algo` for it (batch.md: "Optional;
/// defaults to `sha256` if not specified").
pub const SHA256: &str = "sha256";

/// Format: the media type of every LFS API body, request and answer [B: batch.md, locking.md].
pub const MEDIA_TYPE: &str = "application/vnd.git-lfs+json";

/// An instant to the second, between the years 0000 and 9999: what RFC 3339 writes with four-digit years, and what Go,
/// which git-lfs's client is written in, will read and write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
  /// Seconds since 1970-01-01T00:00:00Z.
  seconds: i64,
}

/// Format: the seconds of a day.
const DAY_SECONDS: i64 = 86_400;

/// Format: the seconds of an hour.
const HOUR_SECONDS: i64 = 3_600;

/// Format: the seconds of a minute.
const MINUTE_SECONDS: i64 = 60;

/// Format: the days of a common year.
const YEAR_DAYS: i64 = 365;

/// Format: the years of the Julian leap cycle, and of a Gregorian century.
const LEAP_CYCLE_YEARS: i64 = 4;

/// Format: the years of a Gregorian century, whose last year leaps only if the era's does.
const CENTURY_YEARS: i64 = 100;

/// Format: the years of a Gregorian era, after which the calendar repeats.
const ERA_YEARS: i64 = 400;

/// Derived: the days of a 400-year era: 400 common years and 97 leap days.
const ERA_DAYS: i64 = ERA_YEARS.saturating_mul(YEAR_DAYS).saturating_add(
  ERA_YEARS
    .saturating_div(LEAP_CYCLE_YEARS)
    .saturating_sub(ERA_YEARS.saturating_div(CENTURY_YEARS))
    .saturating_add(1),
);

/// Derived: the days of a leap cycle less its last, which is the leap day when there is one: 4 x 365.
const LEAP_CYCLE_DAYS_LESS_ONE: i64 = LEAP_CYCLE_YEARS.saturating_mul(YEAR_DAYS);

/// Derived: the days of a century less its last: 100 x 365 and 24 leap days.
const CENTURY_DAYS_LESS_ONE: i64 = CENTURY_YEARS.saturating_mul(YEAR_DAYS).saturating_add(
  CENTURY_YEARS
    .saturating_div(LEAP_CYCLE_YEARS)
    .saturating_sub(1),
);

/// Format: the days the civil-calendar algorithm counts from, 0000-03-01 to 1970-01-01 (Howard Hinnant,
/// "chrono-Compatible Low-Level Date Algorithms", `civil_from_days`, which counts years from March so the leap day is a
/// year's last).
const EPOCH_SHIFT: i64 = 719_468;

/// Format: the days of January and February of the year 0000, a leap year: from 0000-01-01 to 0000-03-01.
const LEAP_JANUARY_FEBRUARY_DAYS: i64 = 60;

/// Format: the line of Hinnant's month from a March-based day of the year: month `(5 x day + 2) / 153`, and back.
const MONTH_LINE_SLOPE: i64 = 5;

/// Format: the offset of that line.
const MONTH_LINE_OFFSET: i64 = 2;

/// Format: the days the line counts five months in (March to July).
const MONTH_LINE_DAYS: i64 = 153;

/// Format: the March-based index of January, the year's first month that belongs to the next calendar year.
const JANUARY_FROM_MARCH: i64 = 10;

/// Format: March's number, the first month of a March-based year.
const MARCH: i64 = 3;

/// Format: February's number, the last month of a March-based year.
const FEBRUARY: i64 = 2;

/// Derived: 0000-01-01T00:00:00Z in seconds since the epoch, the first instant four-digit years write: the days from
/// 0000-01-01 to 1970-01-01, the epoch shift and 0000's January and February, before the epoch.
const FIRST_SECOND: i64 = EPOCH_SHIFT
  .saturating_add(LEAP_JANUARY_FEBRUARY_DAYS)
  .saturating_mul(DAY_SECONDS)
  .saturating_neg();

/// Format: 9999-12-31T23:59:59Z in seconds since the epoch, the last instant four-digit years write (a test reads it
/// back through the calendar).
const LAST_SECOND: i64 = 253_402_300_799;

/// Format: the digits of an RFC 3339 year, and of every other field.
const YEAR_DIGITS: u32 = 4;

/// Format: the digits of a month, a day, an hour, a minute and a second.
const FIELD_DIGITS: u32 = 2;

impl Timestamp {
  /// The instant `seconds` after 1970-01-01T00:00:00Z, or `None` outside the years 0000 to 9999.
  pub fn from_unix(seconds: i64) -> Option<Timestamp> {
    (FIRST_SECOND..=LAST_SECOND)
      .contains(&seconds)
      .then_some(Timestamp { seconds })
  }

  /// Seconds since 1970-01-01T00:00:00Z.
  pub fn unix(&self) -> i64 {
    self.seconds
  }

  /// Writes the instant as RFC 3339 does, upper case, to the second, in UTC: `YYYY-MM-DDTHH:MM:SSZ` (batch.md: "String
  /// uppercase RFC 3339-formatted timestamp with second precision"; Go's `time.Time` writes UTC with `Z`).
  pub fn put_rfc3339(&self, out: &mut impl Out) {
    let days = self.seconds.div_euclid(DAY_SECONDS);
    let of_day = self.seconds.rem_euclid(DAY_SECONDS);
    let (year, month, day) = civil_from_days(days);
    let hour = of_day.checked_div(HOUR_SECONDS).unwrap_or(0);
    let minute = of_day
      .checked_rem(HOUR_SECONDS)
      .and_then(|rest| rest.checked_div(MINUTE_SECONDS))
      .unwrap_or(0);
    let second = of_day.checked_rem(MINUTE_SECONDS).unwrap_or(0);
    put_padded(out, year, YEAR_DIGITS);
    out.put(b"-");
    put_padded(out, month, FIELD_DIGITS);
    out.put(b"-");
    put_padded(out, day, FIELD_DIGITS);
    out.put(b"T");
    put_padded(out, hour, FIELD_DIGITS);
    out.put(b":");
    put_padded(out, minute, FIELD_DIGITS);
    out.put(b":");
    put_padded(out, second, FIELD_DIGITS);
    out.put(b"Z");
  }
}

/// The proleptic Gregorian date of the day `days` after 1970-01-01, by Howard Hinnant's `civil_from_days`: its year,
/// month and day. Every step is checked; within [`Timestamp`]'s range none saturates.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
  let shifted = days.saturating_add(EPOCH_SHIFT);
  let era = shifted.div_euclid(ERA_DAYS);
  let day_of_era = shifted.rem_euclid(ERA_DAYS);
  let per = |value: i64, span: i64| value.checked_div(span).unwrap_or(0);
  let year_of_era = per(
    day_of_era
      .saturating_sub(per(day_of_era, LEAP_CYCLE_DAYS_LESS_ONE))
      .saturating_add(per(day_of_era, CENTURY_DAYS_LESS_ONE))
      .saturating_sub(per(day_of_era, ERA_DAYS.saturating_sub(1))),
    YEAR_DAYS,
  );
  let day_of_year = day_of_era.saturating_sub(
    year_of_era
      .saturating_mul(YEAR_DAYS)
      .saturating_add(per(year_of_era, LEAP_CYCLE_YEARS))
      .saturating_sub(per(year_of_era, CENTURY_YEARS)),
  );
  let month_index = per(
    day_of_year
      .saturating_mul(MONTH_LINE_SLOPE)
      .saturating_add(MONTH_LINE_OFFSET),
    MONTH_LINE_DAYS,
  );
  let day = day_of_year
    .saturating_sub(per(
      month_index
        .saturating_mul(MONTH_LINE_DAYS)
        .saturating_add(MONTH_LINE_OFFSET),
      MONTH_LINE_SLOPE,
    ))
    .saturating_add(1);
  let month = if month_index < JANUARY_FROM_MARCH {
    month_index.saturating_add(MARCH)
  } else {
    month_index.saturating_sub(JANUARY_FROM_MARCH.saturating_sub(1))
  };
  let year = year_of_era
    .saturating_add(era.saturating_mul(ERA_YEARS))
    .saturating_add(i64::from(month <= FEBRUARY));
  (year, month, day)
}

/// Writes `value`, at least zero, in decimal with at least `width` digits.
fn put_padded(out: &mut impl Out, value: i64, width: u32) {
  let magnitude = value.unsigned_abs();
  let digits = magnitude
    .checked_ilog10()
    .map_or(1, |log| log.saturating_add(1));
  for _ in digits..width {
    out.put(b"0");
  }
  json::put_u64(out, magnitude);
}

/// The body of an answer other than 200 (and of a lock conflict, beside its lock): what went wrong in words, and
/// optionally the request's id and where to read about it [B: batch.md "Response Errors", locking.md].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorBody<'a> {
  /// What went wrong.
  pub message: &'a str,
  /// The request's id, for whoever reads the server's records.
  pub request_id: Option<&'a str>,
  /// Where the error is explained.
  pub documentation_url: Option<&'a str>,
}

impl ErrorBody<'_> {
  /// Writes the body's fields, each after a comma but the first when `first`.
  fn put_fields(&self, out: &mut impl Out, first: bool) {
    if !first {
      out.put(b",");
    }
    out.put(b"\"message\":");
    json::put_str(out, self.message);
    if let Some(request_id) = self.request_id {
      out.put(b",\"request_id\":");
      json::put_str(out, request_id);
    }
    if let Some(url) = self.documentation_url {
      out.put(b",\"documentation_url\":");
      json::put_str(out, url);
    }
  }
}

/// Writes `body` as an error answer's JSON: `{"message": ..., "request_id": ..., "documentation_url": ...}`.
pub fn write_error(body: &ErrorBody<'_>, out: &mut impl Out) {
  out.put(b"{");
  body.put_fields(out, true);
  out.put(b"}");
}

/// An endpoint of the LFS API, named by the path after `<repo>.git/info/lfs` [B: batch.md, locking.md,
/// server-discovery.md].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route<'a> {
  /// `/objects/batch`: the batch API, by `POST`.
  Batch,
  /// `/locks`: create a lock by `POST`, list them by `GET`.
  Locks,
  /// `/locks/verify`: list locks for a push's verification, by `POST`.
  LocksVerify,
  /// `/locks/{id}/unlock`: release a lock, by `POST`. git-lfs writes the id into the path unescaped
  /// (`locking/api.go`: `fmt.Sprintf("locks/%s/unlock", id)`), so a server issues ids of RFC 3986's unreserved
  /// characters alone and the segment is the id as issued.
  Unlock {
    /// The lock's id: one path segment, not empty.
    id: &'a [u8],
  },
}

/// The endpoint `path`, the part of a request's path after `<repo>.git/info/lfs`, asks for, or `None`.
pub fn route(path: &[u8]) -> Option<Route<'_>> {
  match path {
    b"/objects/batch" => Some(Route::Batch),
    b"/locks" => Some(Route::Locks),
    b"/locks/verify" => Some(Route::LocksVerify),
    _ => {
      let id = path.strip_prefix(b"/locks/")?.strip_suffix(b"/unlock")?;
      (!id.is_empty() && !id.contains(&b'/')).then_some(Route::Unlock { id })
    }
  }
}

/// What is wrong with a `ref` property's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefProblem {
  /// Neither an object nor `null`.
  Type {
    /// Where the value begins.
    at: u64,
  },
  /// Its `name` is not a string.
  NameType {
    /// Where the name's value begins.
    at: u64,
  },
  /// Its `name` given twice.
  NameRepeated {
    /// Where the second begins.
    at: u64,
  },
}

/// Reads a `ref` property's value: `{"name": ...}`, or `null` or an object without a name for none (batch.md: servers
/// "should be able to operate with a missing or null `ref` property"). Its other keys are skipped.
pub(crate) fn read_ref<'a>(
  cursor: &mut json::Cursor<'a>,
) -> Result<Option<json::JsonStr<'a>>, RefProblem> {
  if cursor.null() {
    return Ok(None);
  }
  let shape = |shape: json::Shape| RefProblem::Type { at: shape.at };
  cursor.begin_object().map_err(shape)?;
  let mut name = None;
  while let Some(key) = cursor.next_key().map_err(shape)? {
    let at = cursor.position();
    if !key.is("name") {
      cursor.skip_value().map_err(shape)?;
      continue;
    }
    if name.is_some() {
      return Err(RefProblem::NameRepeated { at });
    }
    name = Some(
      cursor
        .string()
        .map_err(|shape| RefProblem::NameType { at: shape.at })?,
    );
  }
  Ok(name)
}

/// Whether `name` is an HTTP field name: one or more of RFC 9110 §5.6.2's token characters.
fn is_header_name(name: &str) -> bool {
  !name.is_empty()
    && name
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// Whether `value` is an HTTP field value a client can send as it is: no control character but a tab, so no line
/// ending can split it into a second field (RFC 9110 §5.5).
fn is_header_value(value: &str) -> bool {
  value
    .bytes()
    .all(|byte| byte == b'\t' || (byte >= b' ' && byte != 0x7f))
}

#[cfg(test)]
#[path = "tests/lfs.rs"]
pub(crate) mod tests;
