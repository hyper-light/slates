//! pkt-lines (A-582; git's `gitprotocol-common.txt` "pkt-line Format" and `gitprotocol-v2.txt` "Packet-Line Framing"
//! [B]; `pkt-line.c` [C]): every message of git's wire is a run of packets, each four hexadecimal digits giving its
//! whole length, the four digits included, then that many bytes less four of data. A length of zero is the flush
//! packet, which ends a message; protocol v2 adds one, the delimiter between a message's sections, and two, the response
//! end a stateless server sends after a response. No other length shorter than the four digits exists, and no packet is
//! longer than [`MAX_LEN`].
//!
//! [`read`] reads one packet from the bytes a stream has delivered so far, saying when they do not yet hold all of it,
//! so a server reads a request as it arrives; [`decode`] decodes a whole stream at once under an [`Admission`];
//! [`write`] encodes one packet as git writes it, its length in lower-case digits.

use crate::out::Out;
use crate::{Admission, PastAdmission, to_u64};

/// Format: the bytes of a pkt-line's length, four hexadecimal digits that count themselves [gitprotocol-common
/// "pkt-line Format"].
pub const HEADER_LEN: usize = 4;

/// Format: the longest pkt-line, its length included, git's `LARGE_PACKET_MAX`: "Implementations MUST NOT send
/// pkt-line whose length exceeds 65520 (65516 bytes of payload + 4 bytes of length data)" [gitprotocol-common].
pub const MAX_LEN: usize = 65520;

/// Derived: the most data one pkt-line carries, [`MAX_LEN`] less its length: "The maximum length of a pkt-line's data
/// component is 65516 bytes" [gitprotocol-common].
pub const MAX_DATA: usize = MAX_LEN.saturating_sub(HEADER_LEN);

/// Format: the flush packet's length, `0000`, which ends a message [gitprotocol-common].
const FLUSH_LEN: usize = 0;

/// Format: the delimiter's length, `0001`, between a protocol v2 message's sections [gitprotocol-v2 "Packet-Line
/// Framing"].
const DELIM_LEN: usize = 1;

/// Format: the response end's length, `0002`, after a stateless protocol v2 response [gitprotocol-v2 "Packet-Line
/// Framing"].
const RESPONSE_END_LEN: usize = 2;

/// Format: the digits git writes a length in, lower case (`pkt-line.c`'s `set_packet_header`).
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Format: the bits one hexadecimal digit carries.
const DIGIT_BITS: u32 = 4;

/// Format: the base of a length's digits, sixteen [gitprotocol-common "pkt-line Format"].
const RADIX: u32 = 16;

/// One packet of a pkt-line stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
  /// A data packet's data: the bytes after its length (none for `0004`, which git reads and should not send).
  Data(&'a [u8]),
  /// `0000`: the flush packet, the end of a message.
  Flush,
  /// `0001`: protocol v2's delimiter, between a message's sections.
  Delim,
  /// `0002`: protocol v2's response end, after a stateless response.
  ResponseEnd,
}

/// Why bytes are not a pkt-line stream. Each names where in its stream it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A length digit outside `0-9`, `a-f` and `A-F`.
  NotHex {
    /// Where the digit is.
    at: u64,
  },
  /// The length `0003`: shorter than its own four digits, and no special packet (git's "bad line length").
  ShorterThanHeader {
    /// Where the length begins.
    at: u64,
  },
  /// A length past [`MAX_LEN`].
  TooLong {
    /// Where the length begins.
    at: u64,
    /// The length it names.
    len: u64,
  },
  /// The stream ended inside a packet.
  Truncated {
    /// Where the packet begins.
    at: u64,
    /// The bytes the packet needs.
    needed: u64,
    /// The bytes that were left.
    left: u64,
  },
  /// The list of the stream's packets would take more than the admission had left.
  PastAdmission(PastAdmission),
}

/// A data packet whose data is past [`MAX_DATA`]: refused, nothing written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataTooLong {
  /// The data's length.
  pub len: u64,
}

/// The value of one length digit, of either case as git's `hexval` reads it.
fn digit(byte: u8) -> Option<u32> {
  char::from(byte).to_digit(RADIX)
}

/// The length the digits at the start of `bytes` name, or the first that is not one (as an index into `bytes`); `None`
/// when fewer than four digits have arrived and each that has is a digit.
fn length(bytes: &[u8]) -> Result<Option<usize>, usize> {
  let mut len: u32 = 0;
  for (index, byte) in bytes.iter().take(HEADER_LEN).enumerate() {
    let Some(value) = digit(*byte) else {
      return Err(index);
    };
    len = len.saturating_mul(RADIX).saturating_add(value);
  }
  if bytes.len() < HEADER_LEN {
    return Ok(None);
  }
  Ok(Some(usize::try_from(len).unwrap_or(usize::MAX)))
}

/// Reads the packet at the start of `bytes`, which begin `at` bytes into their stream: the packet and the bytes it took.
/// `None` while `bytes` holds less than the whole packet, so a reader takes more and asks again; refused when no more
/// bytes could make them a packet.
pub fn read(bytes: &[u8], at: u64) -> Result<Option<(Packet<'_>, usize)>, Refusal> {
  let len = match length(bytes) {
    Ok(Some(len)) => len,
    Ok(None) => return Ok(None),
    Err(index) => {
      return Err(Refusal::NotHex {
        at: at.saturating_add(to_u64(index)),
      });
    }
  };
  let packet = match len {
    FLUSH_LEN => Packet::Flush,
    DELIM_LEN => Packet::Delim,
    RESPONSE_END_LEN => Packet::ResponseEnd,
    short if short < HEADER_LEN => return Err(Refusal::ShorterThanHeader { at }),
    long if long > MAX_LEN => {
      return Err(Refusal::TooLong {
        at,
        len: to_u64(long),
      });
    }
    whole => {
      return Ok(
        bytes
          .get(HEADER_LEN..whole)
          .map(|data| (Packet::Data(data), whole)),
      );
    }
  };
  Ok(Some((packet, HEADER_LEN)))
}

/// Decodes every packet of `bytes`, a whole stream: refused when it ends inside a packet or holds bytes that are none,
/// or when the list of its packets would take more than `admission` has left, charged before the list is made.
pub fn decode<'a>(bytes: &'a [u8], admission: &mut Admission) -> Result<Vec<Packet<'a>>, Refusal> {
  let count = walk(bytes, |_| {})?;
  admission
    .charge_items::<Packet<'a>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut packets = Vec::with_capacity(count);
  walk(bytes, |packet| packets.push(packet))?;
  Ok(packets)
}

/// Walks the packets of `bytes`, handing each to `each`: how many there were.
fn walk<'a>(bytes: &'a [u8], mut each: impl FnMut(Packet<'a>)) -> Result<usize, Refusal> {
  let mut at = 0usize;
  let mut count = 0usize;
  while let Some(rest) = bytes.get(at..).filter(|rest| !rest.is_empty()) {
    match read(rest, to_u64(at))? {
      Some((packet, took)) => {
        each(packet);
        at = at.saturating_add(took);
        count = count.saturating_add(1);
      }
      None => {
        let needed = length(rest).ok().flatten().unwrap_or(HEADER_LEN);
        return Err(Refusal::Truncated {
          at: to_u64(at),
          needed: to_u64(needed),
          left: to_u64(rest.len()),
        });
      }
    }
  }
  Ok(count)
}

/// Writes `packet` as git does: a data packet as its whole length in four lower-case hexadecimal digits and then its
/// data, a special packet as its four digits. Refused, nothing written, for data past [`MAX_DATA`].
pub fn write(packet: Packet<'_>, out: &mut impl Out) -> Result<(), DataTooLong> {
  match packet {
    Packet::Data(data) => {
      if data.len() > MAX_DATA {
        return Err(DataTooLong {
          len: to_u64(data.len()),
        });
      }
      out.put(&header(data.len().saturating_add(HEADER_LEN)));
      out.put(data);
    }
    Packet::Flush => out.put(&header(FLUSH_LEN)),
    Packet::Delim => out.put(&header(DELIM_LEN)),
    Packet::ResponseEnd => out.put(&header(RESPONSE_END_LEN)),
  }
  Ok(())
}

/// The four lower-case hexadecimal digits of `len`, a packet's whole length, at most [`MAX_LEN`] (its callers'
/// bound; a larger one keeps its low digits).
pub(crate) fn header(len: usize) -> [u8; HEADER_LEN] {
  let mut header = [0u8; HEADER_LEN];
  let mut rest = len;
  for slot in header.iter_mut().rev() {
    let value = rest.checked_rem(HEX_DIGITS.len()).unwrap_or(0);
    *slot = HEX_DIGITS.get(value).copied().unwrap_or(b'0');
    rest = rest.checked_shr(DIGIT_BITS).unwrap_or(0);
  }
  header
}

#[cfg(test)]
#[path = "tests/pktline.rs"]
mod tests;
