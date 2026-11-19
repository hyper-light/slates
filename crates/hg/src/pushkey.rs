//! Pushkey: the `pushkey` part and its `reply:pushkey`, and the encoding of a namespace's keys that `listkeys` answers
//! with (A-545; `internals.bundle2` "pushkey", "reply:pushkey", "listkeys" [B]; `pushkey.py`, `bundle2.py`,
//! `bundle2_part_handlers.py`, `exchange.py` [C]).
//!
//! - A `pushkey` part asks for one key of one namespace to move from an old value to a new one, in four mandatory
//!   parameters and no payload; 7.2's client writes them in the order `namespace`, `key`, `old`, `new`
//!   (`_pushb2bookmarkspushkey`, `_pushb2phasespushkey`). Its answer is a mandatory `reply:pushkey` part with two
//!   advisory parameters, `in-reply-to` and `return`, decimal (`'%d'`, `'%i'`), which 7.2 reads with `int()`; a
//!   number is read here as Rust reads a decimal, a subset of what `int()` takes (no spaces, no `_`), which is every
//!   number 7.2 writes.
//! - `listkeys` answers, and a `listkeys` part holds, `key\tvalue` lines joined by newlines (`encodekeys`); 7.2 splits
//!   them with `splitlines()` (a line ends at `\n`, `\r` or `\r\n`) and each at its tab, one tab exactly or `split`
//!   fails (`decodekeys`). So a line here must hold exactly one tab, and a key or value to be written must hold no tab
//!   and no line end.
//! - The bookmarks namespace's values are a node's 40 hexadecimal digits, or empty for a bookmark absent or deleted; the
//!   phases namespace's keys are nodes the same way (its draft roots, `listphases`, `pushphase`), beside `publishing`,
//!   which a publishing repository lists with the value `True`.
//!
//! Nothing is allocated in decoding: a payload is checked whole, then read in place. A listkeys encode is checked
//! whole before a byte is written, and its exact length is known before it is made ([`listkeys_encoded_len`]); a
//! reply's numbers are written into a fixed buffer its part borrows ([`ReplyText`]).

use core::fmt::Write as _;

use crate::bundle2::{Param, Part};
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{NODE_LEN, Node};

/// Format: the pushkey part's type, mandatory as 7.2's client sends it (`bundler.newpart(b'pushkey')`).
pub const PART: &[u8] = b"PUSHKEY";

/// Format: the reply's type, mandatory as 7.2's server sends it (`op.reply.newpart(b'reply:pushkey')`); a reader
/// routes it without regard to case.
pub const REPLY_PART: &[u8] = b"REPLY:PUSHKEY";

/// Format: the pushkey part's parameters, in the order 7.2's client adds them.
pub const PARAMETERS: [&[u8]; 4] = [b"namespace", b"key", b"old", b"new"];

/// Format: the reply's parameter naming the part it answers (`in-reply-to`).
pub const IN_REPLY_TO: &[u8] = b"in-reply-to";

/// Format: the reply's parameter carrying the result (`return`).
pub const RETURN: &[u8] = b"return";

/// Format: the byte between a listkeys line's key and value (`encodekeys`).
const TAB: u8 = b'\t';

/// Format: the byte between listkeys lines (`b'\n'.join`).
const NEWLINE: u8 = b'\n';

/// Format: the line ends Python's `bytes.splitlines()` splits at: `\n`, `\r`, and `\r\n` as one.
const LINE_ENDS: [u8; 2] = [NEWLINE, b'\r'];

/// Format: hexadecimal, the base of a node's digits (`node.hex`).
const HEX_RADIX: u32 = 16;

/// Format: the hexadecimal digits that write one byte.
const HEX_DIGITS_PER_BYTE: usize = 2;

/// Format: the most bytes a decimal `i64` takes as text: the 19 digits of `i64::MIN`'s magnitude and its sign.
const DECIMAL_MAX_LEN: usize = 20;

/// A pushkey request: one key of one namespace, from an old value to a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
  /// The namespace (`bookmarks`, `phases`, `obsolete`, ...).
  pub namespace: &'a [u8],
  /// The key.
  pub key: &'a [u8],
  /// The value the client saw (empty for none).
  pub old: &'a [u8],
  /// The value it asks for (empty to delete).
  pub new: &'a [u8],
}

/// A `reply:pushkey`: the part it answers and the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
  /// The id of the `pushkey` part it answers.
  pub in_reply_to: u32,
  /// The result (1 moved; 0 refused).
  pub result: i64,
}

/// Why a pushkey part, a reply, a listkeys payload or a namespace value was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A part that is not the one asked for.
  NotThisPart {
    /// Its type.
    kind: Vec<u8>,
  },
  /// A part without a parameter it needs.
  MissingParameter {
    /// The parameter.
    key: &'static [u8],
  },
  /// A parameter that is not a decimal number its field holds.
  NotANumber {
    /// The parameter.
    key: &'static [u8],
  },
  /// A listkeys line without exactly one tab.
  Line {
    /// The line's number, from 0.
    line: u64,
  },
  /// A namespace value that is neither empty nor a node's 40 hexadecimal digits.
  NotANode {
    /// Its length.
    len: u64,
  },
}

/// Why listkeys entries or a reply could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A key or value holding a tab or a line end, which would read back as another entry.
  Separator {
    /// The entry's index.
    entry: usize,
  },
  /// A number whose decimal text did not fit its buffer, which is sized for every `i64`'s.
  NumberText,
}

/// The request a `pushkey` part carries.
pub fn request<'a>(part: &Part<'a>) -> Result<Request<'a>, Refusal> {
  let [namespace, key, old, new] = PARAMETERS;
  if !part.is(PART) {
    return Err(Refusal::NotThisPart {
      kind: part.kind.to_vec(),
    });
  }
  Ok(Request {
    namespace: parameter(part, namespace)?,
    key: parameter(part, key)?,
    old: parameter(part, old)?,
    new: parameter(part, new)?,
  })
}

/// A part's parameter, or the refusal naming it.
fn parameter<'a>(part: &Part<'a>, key: &'static [u8]) -> Result<&'a [u8], Refusal> {
  part.param(key).ok_or(Refusal::MissingParameter { key })
}

/// The `pushkey` part 7.2's client writes for `request`: its type in capitals, the four parameters mandatory, in order.
pub fn part<'a>(id: u32, request: &Request<'a>) -> Part<'a> {
  let values = [request.namespace, request.key, request.old, request.new];
  let params: Vec<Param<'a>> = PARAMETERS
    .iter()
    .zip(values)
    .map(|(key, value)| Param { key: *key, value })
    .collect();
  Part {
    kind: PART,
    id,
    mandatory_params: params.len(),
    params,
    depth: 0,
  }
}

/// The answer a `reply:pushkey` part carries.
pub fn reply(part: &Part<'_>) -> Result<Reply, Refusal> {
  if !part.is(REPLY_PART) {
    return Err(Refusal::NotThisPart {
      kind: part.kind.to_vec(),
    });
  }
  Ok(Reply {
    in_reply_to: number(part, IN_REPLY_TO)?,
    result: number(part, RETURN)?,
  })
}

/// A decimal parameter, held to its field's type.
fn number<T: core::str::FromStr>(part: &Part<'_>, key: &'static [u8]) -> Result<T, Refusal> {
  let value = parameter(part, key)?;
  core::str::from_utf8(value)
    .ok()
    .and_then(|text| text.parse().ok())
    .ok_or(Refusal::NotANumber { key })
}

/// A number's decimal text, as Python's `'%d'` writes it, held in place so a part can borrow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decimal {
  /// The text's bytes, from the first.
  bytes: [u8; DECIMAL_MAX_LEN],
  /// How many of them hold the text.
  len: usize,
}

impl Decimal {
  /// The text of `value`.
  fn new(value: i64) -> Result<Self, EncodeRefusal> {
    let mut decimal = Self {
      bytes: [0; DECIMAL_MAX_LEN],
      len: 0,
    };
    write!(decimal, "{value}").map_err(|_| EncodeRefusal::NumberText)?;
    Ok(decimal)
  }

  /// The text.
  fn as_bytes(&self) -> &[u8] {
    self.bytes.get(..self.len).unwrap_or_default()
  }
}

impl core::fmt::Write for Decimal {
  fn write_str(&mut self, text: &str) -> core::fmt::Result {
    let end = self
      .len
      .checked_add(text.len())
      .ok_or(core::fmt::Error)?;
    let room = self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?;
    for (slot, byte) in room.iter_mut().zip(text.as_bytes()) {
      *slot = *byte;
    }
    self.len = end;
    Ok(())
  }
}

/// The text of a `reply:pushkey` part's two numbers, held for [`reply_part`] to borrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyText {
  /// `in-reply-to`'s text.
  in_reply_to: Decimal,
  /// `return`'s text.
  result: Decimal,
}

impl ReplyText {
  /// The text of `reply`'s numbers.
  pub fn new(reply: &Reply) -> Result<Self, EncodeRefusal> {
    Ok(Self {
      in_reply_to: Decimal::new(i64::from(reply.in_reply_to))?,
      result: Decimal::new(reply.result)?,
    })
  }
}

/// The `reply:pushkey` part 7.2's server writes, numbered `id` in its reply stream: its type in capitals, then
/// `in-reply-to` and `return`, both advisory (`handlepushkey`).
pub fn reply_part(id: u32, text: &ReplyText) -> Part<'_> {
  Part {
    kind: REPLY_PART,
    id,
    params: vec![
      Param {
        key: IN_REPLY_TO,
        value: text.in_reply_to.as_bytes(),
      },
      Param {
        key: RETURN,
        value: text.result.as_bytes(),
      },
    ],
    mandatory_params: 0,
    depth: 0,
  }
}

/// The entries of a listkeys payload, in order.
#[derive(Clone)]
pub struct Keys<'a> {
  /// The payload left.
  rest: &'a [u8],
}

/// The entries of a listkeys payload, the payload checked whole first: every line holds exactly one tab.
pub fn listkeys(payload: &[u8]) -> Result<Keys<'_>, Refusal> {
  let mut lines = Keys { rest: payload };
  let mut index: u64 = 0;
  while let Some(line) = lines.next_line() {
    if line.iter().filter(|byte| **byte == TAB).count() != 1 {
      return Err(Refusal::Line { line: index });
    }
    index = index.saturating_add(1);
  }
  Ok(Keys { rest: payload })
}

impl<'a> Keys<'a> {
  /// The next line, without its end, as `splitlines()` cuts them.
  fn next_line(&mut self) -> Option<&'a [u8]> {
    if self.rest.is_empty() {
      return None;
    }
    let end = self
      .rest
      .iter()
      .position(|byte| LINE_ENDS.contains(byte))
      .unwrap_or(self.rest.len());
    let line = self.rest.get(..end)?;
    let skip = match (self.rest.get(end), self.rest.get(end.saturating_add(1))) {
      (Some(&b'\r'), Some(&NEWLINE)) => 2,
      (Some(_), _) => 1,
      (None, _) => 0,
    };
    self.rest = self
      .rest
      .get(end.saturating_add(skip)..)
      .unwrap_or_default();
    Some(line)
  }
}

impl<'a> Iterator for Keys<'a> {
  type Item = (&'a [u8], &'a [u8]);

  fn next(&mut self) -> Option<Self::Item> {
    let line = self.next_line()?;
    let tab = line.iter().position(|byte| *byte == TAB)?;
    Some((line.get(..tab)?, line.get(tab.saturating_add(1)..)?))
  }
}

/// The length of the listkeys payload of `entries`, for a caller that charges it before encoding; refused as
/// [`encode_listkeys`] refuses.
pub fn listkeys_encoded_len(entries: &[(&[u8], &[u8])]) -> Result<u64, EncodeRefusal> {
  check_listkeys(entries)?;
  let mut measure = Measure(0);
  write_listkeys(entries, &mut measure);
  Ok(measure.0)
}

/// Appends a listkeys payload of `entries`: `key\tvalue` lines joined by newlines, after checking no key or value
/// holds a tab or a line end; nothing is written on a refusal.
pub fn encode_listkeys(entries: &[(&[u8], &[u8])], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  check_listkeys(entries)?;
  write_listkeys(entries, out);
  Ok(())
}

/// Refuses the first entry whose key or value holds a tab or a line end.
fn check_listkeys(entries: &[(&[u8], &[u8])]) -> Result<(), EncodeRefusal> {
  let separator = |byte: &u8| *byte == TAB || LINE_ENDS.contains(byte);
  for (entry, (key, value)) in entries.iter().enumerate() {
    if key.iter().any(separator) || value.iter().any(separator) {
      return Err(EncodeRefusal::Separator { entry });
    }
  }
  Ok(())
}

/// Writes entries [`check_listkeys`] passed, a newline between each two and none after the last (`b'\n'.join`).
fn write_listkeys(entries: &[(&[u8], &[u8])], out: &mut impl Out) {
  for (position, (key, value)) in entries.iter().enumerate() {
    if position > 0 {
      out.put(&[NEWLINE]);
    }
    out.put(key);
    out.put(&[TAB]);
    out.put(value);
  }
}

/// A namespace value as a node: its 40 hexadecimal digits, in either case as 7.2's `bin()` reads them, or `None` for
/// the empty value.
pub fn hex_node(value: &[u8]) -> Result<Option<Node>, Refusal> {
  if value.is_empty() {
    return Ok(None);
  }
  let not_a_node = Refusal::NotANode {
    len: to_u64(value.len()),
  };
  let (pairs, odd) = value.as_chunks::<HEX_DIGITS_PER_BYTE>();
  if !odd.is_empty() || pairs.len() != NODE_LEN {
    return Err(not_a_node);
  }
  let mut node = [0; NODE_LEN];
  for (byte, pair) in node.iter_mut().zip(pairs) {
    *byte = hex_pair(pair).ok_or_else(|| not_a_node.clone())?;
  }
  Ok(Some(node))
}

/// Two hexadecimal digits as the byte they name.
fn hex_pair(pair: &[u8; HEX_DIGITS_PER_BYTE]) -> Option<u8> {
  let mut value: u32 = 0;
  for digit in pair {
    value = value
      .checked_mul(HEX_RADIX)?
      .checked_add(char::from(*digit).to_digit(HEX_RADIX)?)?;
  }
  u8::try_from(value).ok()
}

/// Appends a node's 40 lower-case hexadecimal digits, as 7.2's `hex()` writes them.
pub fn encode_hex(node: &Node, out: &mut Vec<u8>) {
  for byte in node {
    for digit in [u32::from(*byte) / HEX_RADIX, u32::from(*byte) % HEX_RADIX] {
      let character = char::from_digit(digit, HEX_RADIX).unwrap_or('0');
      out.push(u8::try_from(character).unwrap_or(b'0'));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{
    EncodeRefusal, Refusal, Reply, ReplyText, Request, encode_hex, encode_listkeys, hex_node,
    listkeys, listkeys_encoded_len, part, reply, reply_part, request,
  };
  use crate::bundle2::{Param, Part};
  use proptest::prelude::*;

  /// A-545: do build the pushkey part 7.2's client writes and read its request back. Expect the type in capitals,
  /// the four parameters mandatory and in 7.2's order, and the same request.
  #[test]
  fn a_pushkey_part_carries_its_request() {
    let asked = Request {
      namespace: b"bookmarks",
      key: b"main",
      old: b"",
      new: b"ab",
    };
    let built = part(7, &asked);
    assert_eq!(built.kind, b"PUSHKEY");
    assert!(built.is_mandatory());
    assert_eq!(built.mandatory().len(), 4);
    assert_eq!(request(&built), Ok(asked));
  }

  /// AC-HG.9 (hostile input), A-545: do read a request from another part, from a pushkey part missing `new`, and a
  /// reply whose numbers are not numbers or do not fit. Expect each refused by kind.
  #[test]
  fn parts_missing_what_they_need_are_refused() {
    let other = Part {
      kind: b"bookmarks",
      id: 0,
      params: Vec::new(),
      mandatory_params: 0,
      depth: 0,
    };
    assert!(matches!(request(&other), Err(Refusal::NotThisPart { .. })));
    let mut short = part(
      1,
      &Request {
        namespace: b"n",
        key: b"k",
        old: b"o",
        new: b"x",
      },
    );
    short.params.pop();
    assert_eq!(
      request(&short),
      Err(Refusal::MissingParameter { key: b"new" })
    );
    let answer = |reply_to: &'static [u8], result: &'static [u8]| Part {
      kind: b"REPLY:PUSHKEY",
      id: 0,
      params: vec![
        Param {
          key: b"in-reply-to",
          value: reply_to,
        },
        Param {
          key: b"return",
          value: result,
        },
      ],
      mandatory_params: 0,
      depth: 0,
    };
    assert_eq!(
      reply(&answer(b"3", b"1")).map(|r| (r.in_reply_to, r.result)),
      Ok((3, 1))
    );
    assert_eq!(
      reply(&answer(b"x", b"1")),
      Err(Refusal::NotANumber {
        key: b"in-reply-to"
      })
    );
    assert_eq!(
      reply(&answer(b"4294967296", b"1")),
      Err(Refusal::NotANumber {
        key: b"in-reply-to"
      })
    );
  }

  /// A-545: do encode listkeys entries and read them back, and read lines ended by `\r\n` and `\r` as
  /// `splitlines()` cuts them. Expect the entries back in order, no trailing newline written, and the length
  /// `listkeys_encoded_len` gave.
  #[test]
  fn listkeys_entries_round_trip_and_lines_end_as_python_ends_them() {
    let entries: [(&[u8], &[u8]); 2] = [(b"main", b"ab"), (b"publishing", b"True")];
    let mut payload = Vec::new();
    encode_listkeys(&entries, &mut payload).unwrap();
    assert_eq!(payload, b"main\tab\npublishing\tTrue");
    assert_eq!(listkeys_encoded_len(&entries), Ok(23));
    assert_eq!(listkeys(&payload).unwrap().collect::<Vec<_>>(), entries);
    let crlf: Vec<_> = listkeys(b"a\t1\r\nb\t2\rc\t3\n").unwrap().collect();
    let expected: [(&[u8], &[u8]); 3] = [(b"a", b"1"), (b"b", b"2"), (b"c", b"3")];
    assert_eq!(crlf, expected);
    assert_eq!(listkeys(b"").unwrap().count(), 0);
  }

  /// AC-HG.9, A-545: do read lines with no tab, two tabs and an empty line between two, and encode a key holding a
  /// newline. Expect each line refused by its number (7.2's `split(b'\t')` fails on all three) and the key refused.
  #[test]
  fn listkeys_lines_without_one_tab_are_refused() {
    assert_eq!(listkeys(b"a\t1\nb").err(), Some(Refusal::Line { line: 1 }));
    assert_eq!(listkeys(b"a\t1\t2").err(), Some(Refusal::Line { line: 0 }));
    assert_eq!(
      listkeys(b"a\t1\n\nb\t2").err(),
      Some(Refusal::Line { line: 1 })
    );
    let newline: [(&[u8], &[u8]); 1] = [(b"a\nb", b"1")];
    assert_eq!(
      encode_listkeys(&newline, &mut Vec::new()),
      Err(EncodeRefusal::Separator { entry: 0 })
    );
  }

  /// A-545: do read namespace values: empty, a node in lower case and in upper case, 39 digits, and a digit that is
  /// not hexadecimal. Expect none for the empty one, the node for both cases (written back lower-case), and the last
  /// two refused.
  #[test]
  fn namespace_values_are_nodes_or_empty() {
    assert_eq!(hex_node(b""), Ok(None));
    let lower = b"976eb9d7cd11eb8a63d94860fb7186069eef4a10";
    let node = hex_node(lower).unwrap().unwrap();
    assert_eq!(hex_node(&lower.to_ascii_uppercase()), Ok(Some(node)));
    let mut out = Vec::new();
    encode_hex(&node, &mut out);
    assert_eq!(out, lower);
    assert_eq!(hex_node(&lower[..39]), Err(Refusal::NotANode { len: 39 }));
    let mut bad = lower.to_vec();
    bad[5] = b'g';
    assert_eq!(hex_node(&bad), Err(Refusal::NotANode { len: 40 }));
  }

  /// A-545: do write the replies 7.2's server writes, and one at the widest numbers each field holds. Expect the type
  /// in capitals, `in-reply-to` then `return`, both advisory, as decimal text, and each reply read back as written.
  #[test]
  fn a_reply_is_written_as_mercurials_server_writes_it() {
    let text = ReplyText::new(&Reply {
      in_reply_to: 3,
      result: 1,
    })
    .unwrap();
    let built = reply_part(0, &text);
    assert_eq!(built.kind, b"REPLY:PUSHKEY");
    assert!(built.is_mandatory() && built.mandatory().is_empty());
    let params: Vec<(&[u8], &[u8])> = built.params.iter().map(|p| (p.key, p.value)).collect();
    let expected: [(&[u8], &[u8]); 2] = [(b"in-reply-to", b"3"), (b"return", b"1")];
    assert_eq!(params, expected);
    let widest = Reply {
      in_reply_to: u32::MAX,
      result: i64::MIN,
    };
    let text = ReplyText::new(&widest).unwrap();
    assert_eq!(reply(&reply_part(1, &text)), Ok(widest));
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// AC-HG.9, A-545: do read arbitrary bytes as a listkeys payload and as a namespace value. Expect entries, a
    /// node, or a refusal; never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
      if let Ok(keys) = listkeys(&bytes) {
        prop_assert!(keys.count() <= bytes.len());
      }
      if let Ok(Some(node)) = hex_node(&bytes) {
        let mut out = Vec::new();
        encode_hex(&node, &mut out);
        prop_assert!(out.eq_ignore_ascii_case(&bytes));
      }
    }

    /// A-545: do write a reply of any id and any result, and read it back. Expect the same reply.
    #[test]
    fn any_reply_reads_back_as_written(in_reply_to in any::<u32>(), result in any::<i64>()) {
      let written = Reply { in_reply_to, result };
      let text = ReplyText::new(&written).unwrap();
      prop_assert_eq!(reply(&reply_part(0, &text)), Ok(written));
    }
  }
}
