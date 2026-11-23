//! An obsolescence marker as 7.2 holds it in memory (goal 93, changeset evolution; `obsolete.py` \[C\], read
//! 2026-11-23 in the Mercurial 7.2 Homebrew installs): the tuple `(predecessor, successors, flags, metadata, date,
//! parents)` its readers make of either format and its writers encode from. 7.2 compares, sorts and exchanges markers
//! as these tuples, so a marker read in one format and written in the other is the one marker to it; [`crate::
//! obsmarkers`] reads and writes each format's bytes as they are, and this module is what 7.2 makes of them.
//!
//! **Reading.** Format 1's tuple is its fields: the metadata pairs in their stored order, the date as its float and
//! its timezone in minutes times 60, the parents as recorded (none for "no parent data"). Format 0 keeps the date and
//! the parents in its metadata (`_fm0readmarkers`): the entries are read into a map, a later key replacing an earlier
//! one's value in place; `date` is taken out (`0 0` where absent) and split at its one space into Python's `float()`
//! and `int()` of the two halves, a date neither reads being `(0.0, 0)`; then `p2` with `p1` (both taken out), or `p1`
//! alone (taken out), or `p0` (left in) give two, one or no recorded parents, each read from hexadecimal, and parents
//! any of which is not a 20-byte node are no parent data; the rest is sorted by key. Python's `float()` and `int()` are
//! read as CPython reads bytes: ASCII whitespace around, an underscore only between two digits, `inf`, `infinity` and
//! `nan` in any case with a sign; each held to CPython over 525,750 generated strings (`research/mercurial-server.md`).
//!
//! **Writing** (`_fm0encodeonemarker`, `_fm1encodeonemarker`). Format 0 refuses a SHA-256 marker, a flag past a byte
//! and more than 255 successors; it puts `date` (the float as Python's `repr` writes it, a space, the timezone in
//! decimal) and the parents (`p0` empty for none recorded, else `p1` and `p2` in lower-case hexadecimal) into the
//! metadata, a key of the tuple's own replaced, refuses a key holding `:` or NUL and a value holding NUL, and joins the
//! entries sorted by key. Format 1 writes the tuple's pairs in their order, refusing a key or value past 255 bytes, and
//! the timezone as minutes floored (`tz // 60`), refusing one past 16 bits. Python's `repr` of a float is its shortest
//! digits that read back to it, ties to the even digit, in fixed notation from 1e-4 up to below 1e16 and in exponent
//! notation (`e+16`, `e-05`) outside; held to CPython over 400,014 floats, ties included.
//!
//! **Comparing** is Python's tuple equality: bytes and integers as they are, the date's float by value (so `-0.0` is
//! `0.0`, and NaN is no date's equal, its own neither).

use std::borrow::Cow;

use crate::obsmarkers::{self, Date, EncodeRefusal, Format, Marker, Metadata};

/// Format: the flag of a marker whose nodes are SHA-256's (`usingsha256`).
const FLAG_SHA256: u16 = 2;
/// Format: the metadata key format 0 keeps the date under, and the default 7.2 reads where it is absent.
const DATE_KEY: &[u8] = b"date";
/// Format: see [`DATE_KEY`].
const NO_DATE: &[u8] = b"0 0";
/// Format: the metadata keys format 0 keeps the parents under (`p0` marks none recorded).
const PARENT_KEYS: [&[u8]; 3] = [b"p0", b"p1", b"p2"];
/// Format: format 0's separators: between entries, and between an entry's key and value.
const ENTRY_SEPARATOR: u8 = 0;
/// Format: see [`ENTRY_SEPARATOR`].
const KEY_SEPARATOR: u8 = b':';
/// Format: the bytes of a SHA-1 node, the only node format 0 parents are read as.
const SHA1_LEN: usize = 20;
/// Format: the bytes of a SHA-256 node, a marker's under its flag.
const SHA256_LEN: usize = 32;
/// Format: the seconds in a minute, format 1's timezone unit.
const MINUTE: i64 = 60;
/// Format: where Python's `repr` of a float leaves fixed notation: below a decimal point position of -3 or above 16.
const FIXED_LOW: i32 = -3;
/// Format: see [`FIXED_LOW`].
const FIXED_HIGH: i32 = 16;

/// A marker as 7.2 holds it (module docs).
#[derive(Clone, Debug)]
pub struct Tuple<'a> {
  /// The predecessor's node.
  pub predecessor: &'a [u8],
  /// The successors' nodes, one after another.
  pub successors: &'a [u8],
  /// The flags.
  pub flags: u16,
  /// The metadata pairs, in the tuple's order.
  pub metadata: Vec<(&'a [u8], &'a [u8])>,
  /// The date: its float's bits, and the timezone in seconds.
  pub date: (u64, i64),
  /// The recorded parents' nodes, one after another, or none for no parent data.
  pub parents: Option<Cow<'a, [u8]>>,
}

/// Why a marker has no tuple here: a format 0 timezone past 64 bits (7.2 reads any integer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unread;

impl<'a> Tuple<'a> {
  /// The tuple 7.2's reader makes of `marker` (module docs, "Reading").
  pub fn of(marker: &Marker<'a>) -> Result<Tuple<'a>, Unread> {
    match marker.metadata {
      Metadata::Sized { sizes, bytes } => {
        let mut metadata = Vec::with_capacity(sizes.len() / 2);
        let mut at = 0_usize;
        for pair in sizes.chunks_exact(2) {
          let &[key, value] = pair else {
            continue;
          };
          let (key, value) = (usize::from(key), usize::from(value));
          let key_end = at.saturating_add(key);
          let value_end = key_end.saturating_add(value);
          metadata.push((
            bytes.get(at..key_end).unwrap_or_default(),
            bytes.get(key_end..value_end).unwrap_or_default(),
          ));
          at = value_end;
        }
        let date = marker.date.unwrap_or(Date {
          seconds_bits: 0,
          offset_minutes: 0,
        });
        Ok(Tuple {
          predecessor: marker.predecessor,
          successors: marker.successors,
          flags: marker.flags,
          metadata,
          date: (date.seconds_bits, i64::from(date.offset_minutes) * MINUTE),
          parents: marker.parents.map(Cow::Borrowed),
        })
      }
      Metadata::Joined(joined) => {
        let mut metadata: Vec<(&'a [u8], &'a [u8])> = Vec::new();
        for entry in joined.split(|byte| *byte == ENTRY_SEPARATOR) {
          if entry.is_empty() {
            continue;
          }
          let Some(at) = entry.iter().position(|byte| *byte == KEY_SEPARATOR) else {
            continue;
          };
          let (key, value) = (&entry[..at], &entry[at.saturating_add(1)..]);
          match metadata.iter_mut().find(|(held, _)| *held == key) {
            Some(slot) => slot.1 = value,
            None => metadata.push((key, value)),
          }
        }
        let date = take(&mut metadata, DATE_KEY).unwrap_or(NO_DATE);
        let date = read_date(date)?;
        let [p0, p1, p2] = PARENT_KEYS;
        let parents = if has(&metadata, p2) {
          Some(vec![take(&mut metadata, p1), take(&mut metadata, p2)])
        } else if has(&metadata, p1) {
          Some(vec![take(&mut metadata, p1)])
        } else if has(&metadata, p0) {
          Some(Vec::new())
        } else {
          None
        };
        let parents = parents.and_then(|hexes| {
          let mut nodes = Vec::with_capacity(hexes.len().saturating_mul(SHA1_LEN));
          for hex in hexes {
            let node = unhex(hex?)?;
            if node.len() != SHA1_LEN {
              return None;
            }
            nodes.extend_from_slice(&node);
          }
          Some(Cow::Owned(nodes))
        });
        metadata.sort_unstable_by(|left, right| left.0.cmp(right.0));
        Ok(Tuple {
          predecessor: marker.predecessor,
          successors: marker.successors,
          flags: marker.flags,
          metadata,
          date,
          parents,
        })
      }
    }
  }

  /// The length of each of its nodes: SHA-256's under its flag, else SHA-1's.
  pub fn node_len(&self) -> usize {
    if self.flags & FLAG_SHA256 == 0 {
      SHA1_LEN
    } else {
      SHA256_LEN
    }
  }

  /// Its successors' nodes.
  pub fn successor_nodes(&self) -> core::slice::ChunksExact<'a, u8> {
    self.successors.chunks_exact(self.node_len())
  }

  /// Its recorded parents' nodes, none for no parent data.
  pub fn parent_nodes(&self) -> Option<core::slice::ChunksExact<'_, u8>> {
    self
      .parents
      .as_deref()
      .map(|parents| parents.chunks_exact(self.node_len()))
  }

  /// 7.2's order of two markers (`obsutil.sortedmarkers`, `sorted(repo.obsstore)`): Python's order of their tuples,
  /// field after field, a node list compared node by node, a date by its seconds (a NaN, which Python never orders, as
  /// equal) and then its timezone, and a marker without parent data before one with any.
  pub fn order(&self, other: &Tuple<'_>) -> core::cmp::Ordering {
    self
      .predecessor
      .cmp(other.predecessor)
      .then_with(|| self.successor_nodes().cmp(other.successor_nodes()))
      .then_with(|| self.flags.cmp(&other.flags))
      .then_with(|| self.metadata.cmp(&other.metadata))
      .then_with(|| {
        f64::from_bits(self.date.0)
          .partial_cmp(&f64::from_bits(other.date.0))
          .unwrap_or(core::cmp::Ordering::Equal)
      })
      .then_with(|| self.date.1.cmp(&other.date.1))
      .then_with(|| match (self.parent_nodes(), other.parent_nodes()) {
        (Some(left), Some(right)) => left.cmp(right),
        (left, right) => left.is_some().cmp(&right.is_some()),
      })
  }

  /// Whether 7.2 holds `self` and `other` for the one marker (module docs, "Comparing").
  pub fn same(&self, other: &Tuple<'_>) -> bool {
    self.predecessor == other.predecessor
      && self.successors == other.successors
      && self.flags == other.flags
      && self.metadata == other.metadata
      && f64::from_bits(self.date.0) == f64::from_bits(other.date.0)
      && self.date.1 == other.date.1
      && self.parents == other.parents
  }

  /// Appends a payload of this marker alone in `format`, as 7.2's writer of that format writes it (module docs,
  /// "Writing"); nothing is written on a refusal.
  pub fn encode(&self, format: Format, out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
    self.as_marker(format, |marker| obsmarkers::encode(format, &[marker], out))
  }

  /// Appends this marker in `format` with no version byte (one of a payload's markers, after its
  /// [`obsmarkers::header`]), as [`Tuple::encode`] writes it.
  pub fn encode_marker(&self, format: Format, out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
    self.as_marker(format, |marker| {
      obsmarkers::encode_markers(format, &[marker], out)
    })
  }

  /// The length of [`Tuple::encode_marker`]'s bytes, refused as it refuses.
  pub fn marker_len(&self, format: Format) -> Result<u64, EncodeRefusal> {
    self.as_marker(format, |marker| obsmarkers::markers_len(format, &[marker]))
  }

  /// `write` handed this marker as `format` reads it (module docs, "Writing"), refused where that format cannot carry
  /// it.
  fn as_marker<R>(
    &self,
    format: Format,
    write: impl FnOnce(Marker<'_>) -> Result<R, EncodeRefusal>,
  ) -> Result<R, EncodeRefusal> {
    let unencodable = |reason: &'static str| EncodeRefusal::Unencodable { marker: 0, reason };
    match format {
      Format::V0 => {
        if self.flags & FLAG_SHA256 != 0 {
          return Err(unencodable("format 0 cannot carry a SHA-256 marker"));
        }
        let mut entries: Vec<(&[u8], Cow<'_, [u8]>)> = Vec::with_capacity(self.metadata.len());
        for (key, value) in &self.metadata {
          put(&mut entries, key, Cow::Borrowed(value));
        }
        let mut date = python_repr(f64::from_bits(self.date.0)).into_bytes();
        date.push(b' ');
        date.extend_from_slice(self.date.1.to_string().as_bytes());
        put(&mut entries, DATE_KEY, Cow::Owned(date));
        if let Some(parents) = &self.parents {
          let [p0, p1, p2] = PARENT_KEYS;
          if parents.is_empty() {
            put(&mut entries, p0, Cow::Borrowed(&[]));
          }
          for (key, node) in [p1, p2].into_iter().zip(parents.chunks(SHA1_LEN)) {
            let mut hex = Vec::with_capacity(node.len().saturating_mul(2));
            lower_hex(node, &mut hex);
            put(&mut entries, key, Cow::Owned(hex));
          }
        }
        if entries.iter().any(|(key, value)| {
          key.contains(&KEY_SEPARATOR)
            || key.contains(&ENTRY_SEPARATOR)
            || value.contains(&ENTRY_SEPARATOR)
        }) {
          return Err(unencodable(
            "format 0 metadata keys hold no ':' or NUL, values no NUL",
          ));
        }
        entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
        let mut joined = Vec::new();
        for (index, (key, value)) in entries.iter().enumerate() {
          if index > 0 {
            joined.push(ENTRY_SEPARATOR);
          }
          joined.extend_from_slice(key);
          joined.push(KEY_SEPARATOR);
          joined.extend_from_slice(value);
        }
        let marker = Marker {
          flags: self.flags,
          predecessor: self.predecessor,
          successors: self.successors,
          parents: None,
          date: None,
          metadata: Metadata::Joined(&joined),
        };
        write(marker)
      }
      Format::V1 => {
        let offset_minutes = i16::try_from(self.date.1.div_euclid(MINUTE))
          .map_err(|_| unencodable("format 1 timezones fit 16 bits of minutes"))?;
        let mut sizes = Vec::with_capacity(self.metadata.len().saturating_mul(2));
        let mut bytes = Vec::new();
        for (key, value) in &self.metadata {
          let (Ok(key_len), Ok(value_len)) = (u8::try_from(key.len()), u8::try_from(value.len()))
          else {
            return Err(unencodable(
              "format 1 metadata keys and values are at most 255 bytes",
            ));
          };
          sizes.extend_from_slice(&[key_len, value_len]);
          bytes.extend_from_slice(key);
          bytes.extend_from_slice(value);
        }
        let marker = Marker {
          flags: self.flags,
          predecessor: self.predecessor,
          successors: self.successors,
          parents: self.parents.as_deref(),
          date: Some(Date {
            seconds_bits: self.date.0,
            offset_minutes,
          }),
          metadata: Metadata::Sized {
            sizes: &sizes,
            bytes: &bytes,
          },
        };
        write(marker)
      }
    }
  }
}

/// Takes `key`'s value out of `metadata`, if it holds one.
fn take<'a>(metadata: &mut Vec<(&'a [u8], &'a [u8])>, key: &[u8]) -> Option<&'a [u8]> {
  let at = metadata.iter().position(|(held, _)| *held == key)?;
  Some(metadata.remove(at).1)
}

/// Whether `metadata` holds `key`.
fn has(metadata: &[(&[u8], &[u8])], key: &[u8]) -> bool {
  metadata.iter().any(|(held, _)| *held == key)
}

/// Sets `key` to `value` in `entries`, a key held already keeping its place (a Python dict's assignment).
fn put<'e>(entries: &mut Vec<(&'e [u8], Cow<'e, [u8]>)>, key: &'e [u8], value: Cow<'e, [u8]>) {
  match entries.iter_mut().find(|(held, _)| *held == key) {
    Some(slot) => slot.1 = value,
    None => entries.push((key, value)),
  }
}

/// A format 0 date's float and timezone, as `_fm0readmarkers` reads them: `(0.0, 0)` where either half is no
/// Python number or the value holds no single space; refused for a timezone past 64 bits.
fn read_date(date: &[u8]) -> Result<(u64, i64), Unread> {
  let mut halves = date.split(|byte| *byte == b' ');
  let (Some(when), Some(offset), None) = (halves.next(), halves.next(), halves.next()) else {
    return Ok((0.0_f64.to_bits(), 0));
  };
  match (python_float(when), python_int(offset)) {
    (Some(seconds), Some(Ok(offset))) => Ok((seconds.to_bits(), offset)),
    (Some(_), Some(Err(Unread))) => Err(Unread),
    _ => Ok((0.0_f64.to_bits(), 0)),
  }
}

/// Whether `byte` is whitespace to CPython's `float()` and `int()` of bytes (`Py_ISSPACE`).
fn python_space(byte: u8) -> bool {
  matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `text` without the whitespace around it.
fn trimmed(text: &[u8]) -> &[u8] {
  let start = text
    .iter()
    .position(|byte| !python_space(*byte))
    .unwrap_or(text.len());
  let end = text
    .iter()
    .rposition(|byte| !python_space(*byte))
    .map_or(start, |end| end.saturating_add(1));
  text.get(start..end.max(start)).unwrap_or_default()
}

/// `text` without its underscores, each of which stands between two digits; none where one does not.
fn without_underscores(text: &[u8]) -> Option<Vec<u8>> {
  for (at, byte) in text.iter().enumerate() {
    if *byte == b'_' {
      let before = at
        .checked_sub(1)
        .and_then(|before| text.get(before))
        .is_some_and(u8::is_ascii_digit);
      let after = text
        .get(at.saturating_add(1))
        .is_some_and(u8::is_ascii_digit);
      if !(before && after) {
        return None;
      }
    }
  }
  Some(text.iter().copied().filter(|byte| *byte != b'_').collect())
}

/// CPython's `float()` of the bytes `text` (module docs, "Reading"); none where it raises.
fn python_float(text: &[u8]) -> Option<f64> {
  let plain = without_underscores(trimmed(text))?;
  if plain
    .iter()
    .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')))
  {
    return None;
  }
  core::str::from_utf8(&plain).ok()?.parse::<f64>().ok()
}

/// CPython's `int()` of the bytes `text` in base 10 (module docs, "Reading"); none where it raises, refused past 64
/// bits.
fn python_int(text: &[u8]) -> Option<Result<i64, Unread>> {
  let text = trimmed(text);
  let (negative, digits) = match text.split_first() {
    Some((b'-', rest)) => (true, rest),
    Some((b'+', rest)) => (false, rest),
    _ => (false, text),
  };
  if !digits.first().is_some_and(u8::is_ascii_digit) {
    return None;
  }
  let plain = without_underscores(digits)?;
  if plain.is_empty() || !plain.iter().all(u8::is_ascii_digit) {
    return None;
  }
  let mut value: i64 = 0;
  for digit in plain {
    let next = value
      .checked_mul(10)
      .and_then(|value| value.checked_add(i64::from(digit - b'0')));
    match next {
      Some(next) => value = next,
      None => return Some(Err(Unread)),
    }
  }
  Some(Ok(if negative { -value } else { value }))
}

/// Python's `repr` of `value` (module docs, "Writing"): the shortest digits that read back to it, rounded to the even
/// digit on a tie, laid out as CPython lays them out.
pub fn python_repr(value: f64) -> String {
  if value.is_nan() {
    return "nan".to_owned();
  }
  let sign = if value.is_sign_negative() { "-" } else { "" };
  if value.is_infinite() {
    return format!("{sign}inf");
  }
  let magnitude = value.abs();
  if magnitude == 0.0 {
    return format!("{sign}0.0");
  }
  // The shortest's digit count; then exactly that many, correctly rounded (ties to even), which is the shortest
  // CPython picks where two shortest strings read back.
  let shortest = format!("{magnitude:e}");
  let count = shortest
    .split_once('e')
    .map_or(shortest.as_str(), |(mantissa, _)| mantissa)
    .bytes()
    .filter(u8::is_ascii_digit)
    .count();
  let exact = format!("{magnitude:.*e}", count.saturating_sub(1));
  let (mantissa, exponent) = exact.split_once('e').unwrap_or((exact.as_str(), "0"));
  let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
  let digits = digits.trim_end_matches('0');
  let digits = if digits.is_empty() { "0" } else { digits };
  let point = exponent.parse::<i32>().unwrap_or(0).saturating_add(1);
  let length = i32::try_from(digits.len()).unwrap_or(i32::MAX);
  let (first, rest) = digits.split_at(1);
  let body = if !(FIXED_LOW..=FIXED_HIGH).contains(&point) {
    let exponent = point.saturating_sub(1);
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    let fraction = if rest.is_empty() {
      String::new()
    } else {
      format!(".{rest}")
    };
    format!(
      "{first}{fraction}e{exponent_sign}{:02}",
      exponent.unsigned_abs()
    )
  } else if point <= 0 {
    let zeros = "0".repeat(usize::try_from(point.unsigned_abs()).unwrap_or(0));
    format!("0.{zeros}{digits}")
  } else if point >= length {
    let zeros = "0".repeat(usize::try_from(point.saturating_sub(length)).unwrap_or(0));
    format!("{digits}{zeros}.0")
  } else {
    let (whole, fraction) = digits.split_at(usize::try_from(point).unwrap_or(0));
    format!("{whole}.{fraction}")
  };
  format!("{sign}{body}")
}

/// Appends `bytes` in lower-case hexadecimal (`node.hex`).
fn lower_hex(bytes: &[u8], out: &mut Vec<u8>) {
  const DIGITS: &[u8; 16] = b"0123456789abcdef";
  for byte in bytes {
    out.extend_from_slice(&[
      DIGITS[usize::from(byte >> 4)],
      DIGITS[usize::from(byte & 0xf)],
    ]);
  }
}

/// The bytes the hexadecimal `hex` names (`binascii.unhexlify`: either case, an even count); none otherwise.
fn unhex(hex: &[u8]) -> Option<Vec<u8>> {
  if !hex.len().is_multiple_of(2) {
    return None;
  }
  hex
    .chunks_exact(2)
    .map(|pair| {
      let &[high, low] = pair else {
        return None;
      };
      let digit = |byte: u8| u8::try_from(char::from(byte).to_digit(16)?).ok();
      Some(digit(high)? << 4 | digit(low)?)
    })
    .collect()
}

#[cfg(test)]
#[path = "marker_tuple_tests.rs"]
mod tests;
