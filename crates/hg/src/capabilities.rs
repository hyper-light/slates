//! Capabilities (A-569; `internals.wireprotocol` "Capabilities" [B]; `wireprotov1server.py` `_capabilities`,
//! `capabilities`, `hello`, `sshpeer.py` `_performhandshake`, `wireprotov1peer.py` `capable`, and `bundle2.py`
//! `encodecaps`, `decodecaps` [C]).
//!
//! A server lists what it supports as one line of capabilities separated by spaces, each a name or `name=value`: the
//! answer to `capabilities`, and to `hello` after `capabilities: ` and before a newline. One of them, `bundle2=`,
//! carries the bundle2 capability blob URL-quoted: one capability per line, sorted by name, each a name or
//! `name=value1,value2`, its name and values URL-quoted again (`encodecaps`). A client sends its own blob, quoted once,
//! as the payload of the `replycaps` part of every push (A-545's fixtures hold three).
//!
//! **Bounds.** A capabilities line is read in place. A blob is read in two passes, the first counting its entries,
//! values and unquoted bytes, which are charged to the caller's admission before the second builds them once.

use crate::Admission;
use crate::PastAdmission;
use crate::lines::lines;
use crate::out::Measure;
use crate::quote::{self, PATH_SAFE};

/// Format: the capability that carries the bundle2 capability blob, URL-quoted.
pub const BUNDLE2: &[u8] = b"bundle2";

/// Format: what a `hello` answer begins with (`hello`: `b'capabilities: %s\n'`).
pub const HELLO: &[u8] = b"capabilities: ";

/// Format: the byte between capabilities.
const SEPARATOR: u8 = b' ';

/// Format: the byte between a capability's name and its value, in the line and in the blob.
const ASSIGN: u8 = b'=';

/// Format: the byte between a blob capability's values.
const VALUE_SEPARATOR: u8 = b',';

/// Format: the byte that ends a `hello` answer, and between a blob's lines.
const NEWLINE: u8 = b'\n';

/// A capabilities line, read in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities<'a> {
  /// The line.
  line: &'a [u8],
}

/// The capabilities a `capabilities` answer lists.
pub fn decode(answer: &[u8]) -> Capabilities<'_> {
  Capabilities { line: answer }
}

/// The capabilities a `hello` answer lists after `capabilities: `, its newline left out; `None` for an answer that
/// does not begin so.
pub fn decode_hello(answer: &[u8]) -> Option<Capabilities<'_>> {
  let line = answer.strip_prefix(HELLO)?;
  Some(Capabilities {
    line: line.strip_suffix(&[NEWLINE]).unwrap_or(line),
  })
}

impl<'a> Capabilities<'a> {
  /// Each capability, in order: its name, and its value where it has one.
  pub fn iter(&self) -> impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)> + use<'a> {
    self
      .line
      .split(|byte| *byte == SEPARATOR)
      .filter(|token| !token.is_empty())
      .map(|token| match token.iter().position(|byte| *byte == ASSIGN) {
        Some(at) => (
          token.get(..at).unwrap_or_default(),
          token.get(at.saturating_add(1)..),
        ),
        None => (token, None),
      })
  }

  /// What 7.2's `capable` answers for `name`: `Some(None)` for a capability without a value, `Some(Some(value))` for
  /// one with, `None` for one not listed.
  pub fn get(&self, name: &[u8]) -> Option<Option<&'a [u8]>> {
    self
      .iter()
      .find(|(known, _)| *known == name)
      .map(|(_, value)| value)
  }
}

/// Why capabilities could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A name holding a space, an `=` or a newline, or a value holding a space or a newline, which would read back as
  /// other capabilities.
  Separator {
    /// The capability's index.
    capability: usize,
  },
}

/// Appends a capabilities line: each name, with `=value` where it has one, joined by spaces, in the order given.
pub fn encode(
  capabilities: &[(&[u8], Option<&[u8]>)],
  out: &mut Vec<u8>,
) -> Result<(), EncodeRefusal> {
  let breaks = |byte: &u8| *byte == SEPARATOR || *byte == NEWLINE;
  for (index, (name, value)) in capabilities.iter().enumerate() {
    let bad_name = name.is_empty() || name.iter().any(|byte| breaks(byte) || *byte == ASSIGN);
    if bad_name || value.is_some_and(|value| value.iter().any(breaks)) {
      return Err(EncodeRefusal::Separator { capability: index });
    }
  }
  for (index, (name, value)) in capabilities.iter().enumerate() {
    if index > 0 {
      out.push(SEPARATOR);
    }
    out.extend_from_slice(name);
    if let Some(value) = value {
      out.push(ASSIGN);
      out.extend_from_slice(value);
    }
  }
  Ok(())
}

/// Appends a `hello` answer's value for a capabilities line: `capabilities: `, the line, a newline.
pub fn encode_hello(line: &[u8], out: &mut Vec<u8>) {
  out.extend_from_slice(HELLO);
  out.extend_from_slice(line);
  out.push(NEWLINE);
}

/// A bundle2 capability blob, read: each capability's name and values, unquoted, in the blob's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle2Capabilities<'b> {
  /// Each capability's name, and the range of its values in `values`.
  entries: Vec<(&'b [u8], core::ops::Range<usize>)>,
  /// Every value, the capabilities' one after another.
  values: Vec<&'b [u8]>,
}

impl<'b> Bundle2Capabilities<'b> {
  /// Each capability, in the blob's order: its name and its values.
  pub fn iter(&self) -> impl Iterator<Item = (&'b [u8], &[&'b [u8]])> + '_ {
    self.entries.iter().map(|(name, range)| {
      (
        *name,
        self.values.get(range.clone()).unwrap_or_default(),
      )
    })
  }

  /// The values of `name`; the last if the blob names it twice, as 7.2 keeps it (`caps[key] = vals`).
  pub fn get(&self, name: &[u8]) -> Option<&[&'b [u8]]> {
    self
      .iter()
      .filter(|(known, _)| *known == name)
      .last()
      .map(|(_, values)| values)
  }
}

/// One blob line's name and its values, still quoted: `None` for a capability without `=`.
fn split_line(line: &[u8]) -> (&[u8], Option<&[u8]>) {
  match line.iter().position(|byte| *byte == ASSIGN) {
    Some(at) => (
      line.get(..at).unwrap_or_default(),
      line.get(at.saturating_add(1)..),
    ),
    None => (line, None),
  }
}

/// The values of a blob line, still quoted: none without `=`, and `=` alone one empty value, as 7.2 splits them.
fn split_values(values: Option<&[u8]>) -> impl Iterator<Item = &[u8]> {
  values
    .into_iter()
    .flat_map(|values| values.split(|byte| *byte == VALUE_SEPARATOR))
}

/// A bundle2 capability blob (`decodecaps`): its lines as `splitlines()` cuts them, empty ones passed over, each a
/// name or `name=value1,...`, name and values unquoted into `buffer`. The entries, values and unquoted bytes are
/// counted first and charged to `admission`.
pub fn decode_bundle2<'b>(
  blob: &[u8],
  admission: &mut Admission,
  buffer: &'b mut Vec<u8>,
) -> Result<Bundle2Capabilities<'b>, PastAdmission> {
  let filled_lines = || lines(blob).filter(|line| !line.is_empty());
  let (mut bytes, mut entries, mut values) = (Measure(0), 0_usize, 0_usize);
  for line in filled_lines() {
    let (name, quoted) = split_line(line);
    entries = entries.saturating_add(1);
    quote::unquote(name, &mut bytes);
    for value in split_values(quoted) {
      values = values.saturating_add(1);
      quote::unquote(value, &mut bytes);
    }
  }
  let mut kept = *admission;
  kept.charge(bytes.0)?;
  kept.charge_items::<(&[u8], core::ops::Range<usize>)>(entries)?;
  kept.charge_items::<&[u8]>(values)?;
  buffer.clear();
  buffer.reserve_exact(usize::try_from(bytes.0).unwrap_or(usize::MAX));
  for line in filled_lines() {
    let (name, quoted) = split_line(line);
    quote::unquote(name, buffer);
    for value in split_values(quoted) {
      quote::unquote(value, buffer);
    }
  }
  let filled: &'b Vec<u8> = buffer;
  let mut read = Bundle2Capabilities {
    entries: Vec::with_capacity(entries),
    values: Vec::with_capacity(values),
  };
  let mut at: usize = 0;
  let mut take = |quoted: &[u8]| {
    let mut length = Measure(0);
    quote::unquote(quoted, &mut length);
    let end = at.saturating_add(usize::try_from(length.0).unwrap_or(usize::MAX));
    let found = filled.get(at..end).unwrap_or_default();
    at = end;
    found
  };
  for line in filled_lines() {
    let (name, quoted) = split_line(line);
    let name = take(name);
    let first = read.values.len();
    for value in split_values(quoted) {
      read.values.push(take(value));
    }
    read.entries.push((name, first..read.values.len()));
  }
  *admission = kept;
  Ok(read)
}

/// Appends a bundle2 capability blob (`encodecaps`): the capabilities sorted by name, one per line, each name and
/// value URL-quoted, `name=value1,value2` where it has values.
pub fn encode_bundle2(capabilities: &[(&[u8], &[&[u8]])], out: &mut Vec<u8>) {
  let mut sorted: Vec<&(&[u8], &[&[u8]])> = capabilities.iter().collect();
  sorted.sort_by(|left, right| left.0.cmp(right.0));
  for (index, (name, values)) in sorted.into_iter().enumerate() {
    if index > 0 {
      out.push(NEWLINE);
    }
    quote::quote(name, PATH_SAFE, out);
    for (position, value) in values.iter().enumerate() {
      out.push(if position == 0 { ASSIGN } else { VALUE_SEPARATOR });
      quote::quote(value, PATH_SAFE, out);
    }
  }
}

/// Appends the value of the `bundle2` capability for `blob`: the blob URL-quoted (`urlreq.quote`).
pub fn encode_bundle2_value(blob: &[u8], out: &mut Vec<u8>) {
  quote::quote(blob, PATH_SAFE, out);
}

/// The blob a `bundle2` capability's value carries, unquoted into `buffer`, its length charged to `admission` first.
pub fn decode_bundle2_value<'b>(
  value: &[u8],
  admission: &mut Admission,
  buffer: &'b mut Vec<u8>,
) -> Result<&'b [u8], PastAdmission> {
  let mut length = Measure(0);
  quote::unquote(value, &mut length);
  admission.charge(length.0)?;
  buffer.clear();
  buffer.reserve_exact(usize::try_from(length.0).unwrap_or(usize::MAX));
  quote::unquote(value, buffer);
  Ok(buffer.as_slice())
}

#[cfg(test)]
mod tests {
  use super::{
    Capabilities, EncodeRefusal, decode, decode_bundle2, decode_bundle2_value, decode_hello, encode,
    encode_bundle2, encode_bundle2_value, encode_hello,
  };
  use crate::Admission;
  use crate::out::{Measure, Out};
  use proptest::prelude::*;

  fn room() -> Admission {
    Admission::new(1 << 20)
  }

  /// The bytes `write` puts.
  fn measured(write: impl FnOnce(&mut Measure)) -> u64 {
    let mut measure = Measure(0);
    write(&mut measure);
    measure.0
  }

  /// A-569: do read a capabilities line with plain names, values and a value holding `=`. Expect each name with its
  /// value (split at the first `=`), and 7.2's `capable` answers for a plain name, a valued one and a missing one.
  #[test]
  fn a_capabilities_line_reads_as_capable_answers() {
    let caps: Capabilities<'_> = decode(b"lookup  httpheader=1024 bundle2=HG20%0Ab=c");
    let read: Vec<_> = caps.iter().collect();
    assert_eq!(read.len(), 3);
    assert_eq!(caps.get(b"lookup"), Some(None));
    assert_eq!(caps.get(b"bundle2"), Some(Some(&b"HG20%0Ab=c"[..])));
    assert_eq!(caps.get(b"known"), None);
  }

  /// A-569: do write a capabilities line and a `hello` answer of it, and read the answer back. Expect the line, its
  /// `capabilities: ` prefix and newline, and the same capabilities; a name with a space refused.
  #[test]
  fn a_hello_answer_reads_back() {
    let mut line = Vec::new();
    encode(&[(b"lookup", None), (b"httpheader", Some(b"1024"))], &mut line).unwrap();
    assert_eq!(line, b"lookup httpheader=1024");
    let mut hello = Vec::new();
    encode_hello(&line, &mut hello);
    assert_eq!(hello, b"capabilities: lookup httpheader=1024\n");
    assert_eq!(decode_hello(&hello), Some(decode(&line)));
    assert_eq!(
      encode(&[(b"a b", None)], &mut Vec::new()),
      Err(EncodeRefusal::Separator { capability: 0 })
    );
  }

  /// A-569: do write a blob of capabilities given out of order, with values needing quotes, and read it back, through
  /// the `bundle2=` capability's own quoting too. Expect the names sorted, each name and value quoted, the line's
  /// value quoted again, and the same capabilities read back.
  #[test]
  fn a_bundle2_blob_reads_back_through_both_quotings() {
    let values: [&[u8]; 2] = [b"01", b"02"];
    let none: [&[u8]; 0] = [];
    let caps: [(&[u8], &[&[u8]]); 2] = [(b"changegroup", &values), (b"HG20", &none)];
    let mut blob = Vec::new();
    encode_bundle2(&caps, &mut blob);
    assert_eq!(blob, b"HG20\nchangegroup=01,02");
    let mut value = Vec::new();
    encode_bundle2_value(&blob, &mut value);
    assert_eq!(value, b"HG20%0Achangegroup%3D01%2C02");
    let mut unquoted = Vec::new();
    let back = decode_bundle2_value(&value, &mut room(), &mut unquoted).unwrap();
    assert_eq!(back, blob.as_slice());
    let mut buffer = Vec::new();
    let read = decode_bundle2(&blob, &mut room(), &mut buffer).unwrap();
    assert_eq!(read.get(b"changegroup"), Some(&values[..]));
    assert_eq!(read.get(b"HG20"), Some(&none[..]));
  }

  /// AC-HG.9 (hostile input), A-569: do read a blob with empty lines, `\r\n` ends, a name repeated, `=` alone and a
  /// broken escape, and one past its admission. Expect the empty lines passed over, the last of the repeated name's
  /// values kept, `=` one empty value, the broken escape kept as sent, and `PastAdmission` with nothing charged.
  #[test]
  fn blobs_read_as_mercurial_reads_them() {
    let mut buffer = Vec::new();
    let read = decode_bundle2(b"\r\na=1\r\n\na=2,%zz\nb=", &mut room(), &mut buffer).unwrap();
    let expected_a: [&[u8]; 2] = [b"2", b"%zz"];
    let expected_b: [&[u8]; 1] = [b""];
    assert_eq!(read.get(b"a"), Some(&expected_a[..]));
    assert_eq!(read.get(b"b"), Some(&expected_b[..]));
    let mut small = Admission::new(2);
    assert!(decode_bundle2(b"abc", &mut small, &mut Vec::new()).is_err());
    assert_eq!(small.left(), 2);
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-569: do write blobs of generated capabilities whose names and values hold any bytes, and read them back.
    /// Expect each capability's values back, and the bytes charged equal to the unquoted bytes.
    #[test]
    fn generated_blobs_read_back(
      generated in proptest::collection::btree_map(
        proptest::collection::vec(any::<u8>(), 1..8),
        proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..6), 0..3),
        0..5,
      ),
    ) {
      let values: Vec<Vec<&[u8]>> = generated.values().map(|list| list.iter().map(Vec::as_slice).collect()).collect();
      let caps: Vec<(&[u8], &[&[u8]])> = generated.keys().zip(&values).map(|(name, list)| (name.as_slice(), list.as_slice())).collect();
      let mut blob = Vec::new();
      encode_bundle2(&caps, &mut blob);
      let mut buffer = Vec::new();
      let read = decode_bundle2(&blob, &mut room(), &mut buffer).unwrap();
      for (name, list) in &caps {
        prop_assert_eq!(read.get(name), Some(*list));
      }
      let unquoted = measured(|out| for (name, list) in &caps { out.put(name); for value in *list { out.put(value); } });
      prop_assert_eq!(u64::try_from(buffer.len()).unwrap(), unquoted);
    }
  }
}
