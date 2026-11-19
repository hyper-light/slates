//! Lines (A-582, part 2): a text line is one data packet's data, its one trailing LF taken as how the line ended
//! [gitprotocol-common "pkt-line Format"]; the words of protocol v2's lines; and the walk over a message's packets that
//! every decode in [`super`] shares.

use super::Refusal;
use crate::object_name::{HexRefusal, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::pktline::{self, HEADER_LEN, MAX_DATA, Packet};
use crate::sideband::Band;
use crate::to_u64;

/// Format: the byte a text line ends with [gitprotocol-common: "A non-binary line SHOULD BE terminated by an LF"].
const LF: u8 = b'\n';

/// Format: the byte between a line's words [gitprotocol-v2: `SP`].
pub(super) const SP: u8 = b' ';

/// Format: the byte no text line holds. git reads each line into a string that a NUL ends (`pkt-line.c`
/// `packet_read_with_status`), so the bytes after one would be read as nothing; gitprotocol-v2 says it of server
/// options ("must not contain a NUL or LF character").
const NUL: u8 = 0;

/// Format: the digits git writes an object id in, lower case (`hex.c`'s `hash_to_hex`; gitprotocol-common's `HEXDIG`).
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Format: the bits one hexadecimal digit carries.
const HEX_DIGIT_BITS: u32 = 4;

/// Format: the bits of a byte its low hexadecimal digit carries.
const LOW_DIGIT: u8 = 0x0f;

/// Format: the digits of a decimal number.
const DECIMAL_DIGITS: core::ops::RangeInclusive<u8> = b'0'..=b'9';

/// Format: the base of a decimal number.
const DECIMAL: u64 = 10;

/// Format: the radix of a pkt-line's length digits [gitprotocol-common "pkt-line Format"].
const LENGTH_RADIX: u32 = 16;

/// Format: the most decimal digits a `u64` takes, those of 18446744073709551615.
const MAX_DECIMAL_DIGITS: usize = 20;

/// How a text line ended. "A non-binary line SHOULD BE terminated by an LF, which if present MUST be included in the
/// total length. Receivers MUST treat pkt-lines with non-binary data the same whether or not they contain the trailing
/// LF" [gitprotocol-common]; stock git writes both, so a line read here is written back as it came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
  /// With an LF, as the document writes every line.
  Lf,
  /// Without one, as git writes a fetch request's capabilities and flags and its server's `shallow` lines.
  NoLf,
}

impl Ending {
  /// The bytes the ending adds to its line.
  pub(super) fn bytes(self) -> &'static [u8] {
    match self {
      Ending::Lf => b"\n",
      Ending::NoLf => b"",
    }
  }
}

/// A value read from one text line, and how the line ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line<T> {
  /// What the line says.
  pub value: T,
  /// How it ended.
  pub ending: Ending,
}

impl<T> Line<T> {
  /// `value` on a line that ends with an LF, as the document writes every line.
  pub fn lf(value: T) -> Line<T> {
    Line {
      value,
      ending: Ending::Lf,
    }
  }

  /// `value` on a line without an LF.
  pub fn bare(value: T) -> Line<T> {
    Line {
      value,
      ending: Ending::NoLf,
    }
  }
}

/// The text of a data packet's `data` and how it ended: its one trailing LF taken off. Refused when the text holds a
/// NUL or another LF.
pub(super) fn text(data: &[u8], at: u64) -> Result<(&[u8], Ending), Refusal> {
  let (text, ending) = match data.strip_suffix(&[LF]) {
    Some(text) => (text, Ending::Lf),
    None => (data, Ending::NoLf),
  };
  check_text(text, at)?;
  Ok((text, ending))
}

/// Refused when `value`, a line's text or a part of it, holds a NUL or an LF.
pub(super) fn check_text(value: &[u8], at: u64) -> Result<(), Refusal> {
  if value.iter().any(|byte| *byte == NUL || *byte == LF) {
    return Err(Refusal::NotText { at });
  }
  Ok(())
}

/// The object id `hex` writes in `format`: exactly the format's digits, each `0-9` or `a-f`.
pub(super) fn object_id(format: ObjectFormat, hex: &[u8], at: u64) -> Result<ObjectId, Refusal> {
  let id = ObjectId::from_hex(format, hex).map_err(|refusal| Refusal::ObjectId { at, refusal })?;
  match hex.iter().position(u8::is_ascii_uppercase) {
    Some(digit) => Err(Refusal::ObjectId {
      at,
      refusal: HexRefusal::NotHex { at: to_u64(digit) },
    }),
    None => Ok(id),
  }
}

/// Refused when `id` is not of `format`, so it would not be written in the format's length.
pub(super) fn check_format(id: &ObjectId, format: ObjectFormat, at: u64) -> Result<(), Refusal> {
  if id.format() == format {
    return Ok(());
  }
  Err(Refusal::ObjectId {
    at,
    refusal: HexRefusal::Length {
      len: to_u64(id.format().hex_len()),
    },
  })
}

/// The value of `digits`, canonical decimal: at least one digit, no zero before another, nothing else, within `u64`.
pub(super) fn decimal(digits: &[u8]) -> Option<u64> {
  let canonical = !digits.is_empty()
    && digits.iter().all(|byte| DECIMAL_DIGITS.contains(byte))
    && (digits.len() == 1 || digits.first() != Some(&b'0'));
  if !canonical {
    return None;
  }
  digits.iter().try_fold(0u64, |value, digit| {
    value
      .checked_mul(DECIMAL)
      .and_then(|value| value.checked_add(u64::from(digit.saturating_sub(b'0'))))
  })
}

/// One part of a line an encode writes: bytes as they are, or an object id in lower-case hexadecimal.
#[derive(Clone, Copy, Debug)]
pub(super) enum Part<'a> {
  /// Bytes as they are.
  Bytes(&'a [u8]),
  /// An object id, as git's `oid_to_hex` writes it.
  Id(&'a ObjectId),
  /// A number in canonical decimal, as git's `%d` and `%"PRItime` write it.
  Decimal(u64),
}

impl Part<'_> {
  /// The bytes the part writes.
  fn len(&self) -> usize {
    match self {
      Part::Bytes(bytes) => bytes.len(),
      Part::Id(id) => id.format().hex_len(),
      Part::Decimal(value) => decimal_digits(*value).1,
    }
  }

  /// Writes the part.
  fn put(&self, out: &mut impl Out) {
    match self {
      Part::Bytes(bytes) => out.put(bytes),
      Part::Id(id) => {
        for byte in id.as_bytes() {
          let high = HEX_DIGITS.get(usize::from(byte.checked_shr(HEX_DIGIT_BITS).unwrap_or(0)));
          let low = HEX_DIGITS.get(usize::from(byte & LOW_DIGIT));
          for digit in [high, low].into_iter().flatten() {
            out.put(&[*digit]);
          }
        }
      }
      Part::Decimal(value) => {
        let (digits, len) = decimal_digits(*value);
        let start = MAX_DECIMAL_DIGITS.saturating_sub(len);
        out.put(digits.get(start..).unwrap_or_default());
      }
    }
  }
}

/// `value`'s decimal digits, right-aligned in the array, and how many there are.
fn decimal_digits(value: u64) -> ([u8; MAX_DECIMAL_DIGITS], usize) {
  let mut digits = [b'0'; MAX_DECIMAL_DIGITS];
  let mut rest = value;
  let mut len = 0usize;
  for (written, slot) in digits.iter_mut().rev().enumerate() {
    let digit = u8::try_from(rest.checked_rem(DECIMAL).unwrap_or(0)).unwrap_or(0);
    *slot = b'0'.saturating_add(digit);
    rest = rest.checked_div(DECIMAL).unwrap_or(0);
    len = written.saturating_add(1);
    if rest == 0 {
      break;
    }
  }
  (digits, len)
}

/// Writes one data packet holding `parts` one after another, after `band`'s byte where it has one, at `at` in its
/// stream: the bytes it wrote. Refused, nothing written, when the data would be past a packet's [`MAX_DATA`].
pub(super) fn put_line(
  band: Option<Band>,
  parts: &[Part<'_>],
  at: u64,
  out: &mut impl Out,
) -> Result<u64, Refusal> {
  let band_len = usize::from(band.is_some());
  let len = parts
    .iter()
    .fold(band_len, |len, part| len.saturating_add(part.len()));
  if len > MAX_DATA {
    return Err(Refusal::TooLong {
      at,
      len: to_u64(len),
    });
  }
  let whole = len.saturating_add(HEADER_LEN);
  out.put(&pktline::header(whole));
  if let Some(band) = band {
    out.put(&[band.byte()]);
  }
  for part in parts {
    part.put(out);
  }
  Ok(to_u64(whole))
}

/// Writes one special packet (a flush, a delimiter or a response end): the bytes it wrote.
pub(super) fn put_special(packet: Packet<'_>, out: &mut impl Out) -> u64 {
  match pktline::write(packet, out) {
    Ok(()) => to_u64(HEADER_LEN),
    Err(_) => 0,
  }
}

/// The packets of a message, each with the byte where it begins.
pub(super) struct Packets<'a> {
  /// The message's bytes.
  bytes: &'a [u8],
  /// Where the next packet begins.
  at: usize,
}

impl<'a> Packets<'a> {
  /// The packets of `bytes`.
  pub(super) fn new(bytes: &'a [u8]) -> Packets<'a> {
    Packets { bytes, at: 0 }
  }

  /// Where the next packet begins: how many bytes the packets read so far took.
  pub(super) fn at(&self) -> u64 {
    to_u64(self.at)
  }

  /// The next packet and where it begins; `None` once every byte is read. Refused when the bytes left are not a whole
  /// packet.
  pub(super) fn next_packet(&mut self) -> Result<Option<(Packet<'a>, u64)>, Refusal> {
    let rest = self.bytes.get(self.at..).unwrap_or_default();
    if rest.is_empty() {
      return Ok(None);
    }
    let at = self.at();
    match pktline::read(rest, at).map_err(Refusal::Packet)? {
      Some((packet, took)) => {
        self.at = self.at.saturating_add(took);
        Ok(Some((packet, at)))
      }
      None => Err(Refusal::Packet(pktline::Refusal::Truncated {
        at,
        needed: to_u64(needed(rest)),
        left: to_u64(rest.len()),
      })),
    }
  }

  /// The next line of a list that a flush closes: its text, how it ended and where its packet begins; `None` at the
  /// flush. Refused when the bytes end first, at a delimiter or response end, or at a line that is not text.
  pub(super) fn next_line(&mut self) -> Result<Option<(&'a [u8], Ending, u64)>, Refusal> {
    let at = self.at();
    match self.next_packet()? {
      None => Err(Refusal::Unfinished { at }),
      Some((Packet::Flush, _)) => Ok(None),
      Some((Packet::Data(data), at)) => {
        let (text, ending) = text(data, at)?;
        Ok(Some((text, ending, at)))
      }
      Some((_, at)) => Err(Refusal::Misplaced { at }),
    }
  }

  /// Refused when bytes are left after the message's end.
  pub(super) fn finish(&self) -> Result<(), Refusal> {
    if self.at < self.bytes.len() {
      return Err(Refusal::Trailing { at: self.at() });
    }
    Ok(())
  }

  /// Whether every byte has been read.
  pub(super) fn is_done(&self) -> bool {
    self.at >= self.bytes.len()
  }
}

/// The bytes the packet `rest` begins with needs: the length its digits name once all four have arrived (each a
/// digit, or `pktline::read` would have refused it), its four digits before.
fn needed(rest: &[u8]) -> usize {
  rest
    .get(..HEADER_LEN)
    .and_then(|digits| core::str::from_utf8(digits).ok())
    .and_then(|digits| usize::from_str_radix(digits, LENGTH_RADIX).ok())
    .unwrap_or(HEADER_LEN)
}

#[cfg(test)]
mod tests {
  use super::{Ending, Packets, decimal, object_id, text};
  use crate::object_name::{HexRefusal, ObjectFormat};
  use crate::pktline::{self, Packet};
  use crate::v2::Refusal;

  /// A-582 part 2 (gitprotocol-common: a line's LF is optional and a receiver treats both alike): do read lines with
  /// and without their LF, with an LF inside, and with a NUL. Expect: the text without its one LF and how it ended; a
  /// NUL or an inner LF refused `NotText` at the packet's place.
  #[test]
  fn a_line_ends_with_or_without_its_lf_and_holds_no_nul_or_inner_lf() {
    assert_eq!(text(b"done\n", 4), Ok((&b"done"[..], Ending::Lf)));
    assert_eq!(text(b"done", 4), Ok((&b"done"[..], Ending::NoLf)));
    assert_eq!(text(b"\n", 4), Ok((&b""[..], Ending::Lf)));
    assert_eq!(text(b"do\nne\n", 4), Err(Refusal::NotText { at: 4 }));
    assert_eq!(text(b"done\0\n", 9), Err(Refusal::NotText { at: 9 }));
    assert_eq!(text(b"done\n\n", 9), Err(Refusal::NotText { at: 9 }));
  }

  /// A-582 part 2 (gitprotocol-common's `HEXDIG` is lower case): do read ids of each length, case and digit. Expect:
  /// lower case read; upper case refused at its digit, though git's `get_oid_hex` reads it; a short, long or
  /// non-hexadecimal id refused by kind.
  #[test]
  fn an_object_id_is_the_formats_lower_case_digits_and_nothing_else() {
    let hex = b"d0f175d3390380c935e9b56e0cebb715995d838d";
    let id = object_id(ObjectFormat::Sha1, hex, 0).unwrap();
    assert_eq!(id.to_string().as_bytes(), hex);
    let upper = b"d0f175d3390380c935e9b56e0CEBB715995d838d";
    assert_eq!(
      object_id(ObjectFormat::Sha1, upper, 7),
      Err(Refusal::ObjectId {
        at: 7,
        refusal: HexRefusal::NotHex { at: 25 }
      })
    );
    assert_eq!(
      object_id(ObjectFormat::Sha256, hex, 7),
      Err(Refusal::ObjectId {
        at: 7,
        refusal: HexRefusal::Length { len: 40 }
      })
    );
    let mut long = hex.to_vec();
    long.push(b'0');
    assert!(matches!(
      object_id(ObjectFormat::Sha1, &long, 0),
      Err(Refusal::ObjectId { .. })
    ));
    assert!(matches!(
      object_id(ObjectFormat::Sha1, &hex[..39], 0),
      Err(Refusal::ObjectId { .. })
    ));
  }

  /// A-582 part 2: do read decimals canonical and not. Expect: canonical ones read to `u64::MAX`; leading zeros, signs,
  /// spaces, `0x`, an empty string and a value past `u64` refused.
  #[test]
  fn a_number_is_canonical_decimal_within_u64() {
    assert_eq!(decimal(b"0"), Some(0));
    assert_eq!(decimal(b"1"), Some(1));
    assert_eq!(decimal(b"18446744073709551615"), Some(u64::MAX));
    for refused in [
      &b""[..],
      b"01",
      b"+1",
      b"-1",
      b" 1",
      b"1 ",
      b"0x10",
      b"18446744073709551616",
    ] {
      assert_eq!(decimal(refused), None, "{refused:?}");
    }
  }

  /// A-582 part 2: do walk a message's packets and cut it inside a packet. Expect: each packet with the byte it begins
  /// at, then `None`; the cut refused `Truncated` as part 1's whole decode refuses it.
  #[test]
  fn the_walk_names_each_packets_place_and_a_cut_as_part_one_does() {
    let mut packets = Packets::new(b"0009done\n00010000");
    assert_eq!(
      packets.next_packet(),
      Ok(Some((Packet::Data(b"done\n"), 0)))
    );
    assert_eq!(packets.next_packet(), Ok(Some((Packet::Delim, 9))));
    assert_eq!(packets.next_packet(), Ok(Some((Packet::Flush, 13))));
    assert_eq!(packets.next_packet(), Ok(None));
    let cut = b"0009done\n0009do";
    let mut packets = Packets::new(cut);
    packets.next_packet().unwrap();
    let whole = pktline::decode(cut, &mut crate::Admission::new(u64::MAX)).unwrap_err();
    assert_eq!(packets.next_packet(), Err(Refusal::Packet(whole)));
  }
}
