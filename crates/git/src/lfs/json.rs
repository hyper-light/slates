//! The JSON that Git LFS's APIs speak, read within bounds and written compactly (A-610; RFC 8259 [B]).
//!
//! **Reading is two steps.** [`validate`] checks that the input is one JSON value and nothing else: the grammar of
//! RFC 8259 §2 to §7, strings in UTF-8 (§8.1) with every escape well formed and every UTF-16 surrogate paired (§7), and
//! numbers as §6 writes them. It runs as a loop, never by recursion, so no input can exhaust a thread's stack. The one
//! thing it keeps is a bit for each open object or array, eight bytes for every 64 levels, charged to the caller's
//! [`Admission`] as the nesting deepens. So nesting deeper than the request was admitted for is refused by kind
//! ([`Refusal::Nesting`]); no fixed depth is written down. A message's decoder then walks the validated input with a
//! [`Cursor`], which borrows strings and numbers in place as [`JsonStr`] and [`JsonNumber`] and skips any value it does
//! not use by counting brackets. Nothing in this module allocates but that bookkeeping.
//!
//! **Refusals** are typed and name the byte where reading stopped. An input cut short is [`Refusal::Truncated`] (a
//! reader holding part of a body is not yet whole; a whole body so cut is refused); a byte the grammar does not allow
//! is [`Refusal::Unexpected`]; a raw control character in a string is [`Refusal::Control`]; a bad escape or an unpaired
//! surrogate is [`Refusal::Escape`]; bytes that are not UTF-8 are [`Refusal::NotUtf8`]; a number outside the grammar
//! is [`Refusal::Number`]; and bytes after the value are [`Refusal::Trailing`]. A number is never converted here: a
//! decoder asks a [`JsonNumber`] for an integer, and one past 64 bits or with a fraction or an exponent is refused by
//! kind ([`NumberRefusal`]), so a huge number costs its digits' reading and nothing else.
//!
//! **Writing** ([`put_str`], [`put_u64`], [`put_i64`]) is compact: no whitespace, strings with `"`, `\` and every
//! control character escaped (§7), nothing else changed, so what is written reads back to the same text.

use crate::out::Out;
use crate::{Admission, PastAdmission, to_u64};

/// Format: the bytes RFC 8259 §2 allows between tokens: space, horizontal tab, line feed, carriage return.
const WHITESPACE: [u8; 4] = [b' ', b'\t', b'\n', b'\r'];

/// Format: the first code point that needs no escape in a string (RFC 8259 §7: "the control characters (U+0000
/// through U+001F)" must be escaped).
const FIRST_UNESCAPED: u8 = 0x20;

/// Format: the bits one hexadecimal digit carries.
const HEX_DIGIT_BITS: u32 = 4;

/// Format: the hexadecimal digits of a `\u` escape (RFC 8259 §7: "four hexadecimal digits").
const ESCAPE_DIGITS: usize = 4;

/// Format: the UTF-16 high (leading) surrogates, U+D800 to U+DBFF, each to be followed by a low one (RFC 8259 §7,
/// RFC 2781 §2.2).
const HIGH_SURROGATES: core::ops::RangeInclusive<u32> = 0xD800..=0xDBFF;

/// Format: the UTF-16 low (trailing) surrogates, U+DC00 to U+DFFF.
const LOW_SURROGATES: core::ops::RangeInclusive<u32> = 0xDC00..=0xDFFF;

/// Format: the bits a surrogate pair's halves each carry (RFC 2781 §2.2: "the 10 low-order bits").
const SURROGATE_BITS: u32 = 10;

/// Format: the first code point a surrogate pair names (RFC 2781 §2.1: U' = U - 0x10000).
const SUPPLEMENTARY: u32 = 0x1_0000;

/// Format: the open containers one bookkeeping word records, a bit each: a `u64`'s bits.
const LEVELS_PER_WORD: usize = 64;

/// Format: the digits of a number (RFC 8259 §6).
const DECIMAL: u64 = 10;

/// Format: the most decimal digits a `u64` has, those of 18446744073709551615.
const U64_DIGITS: usize = 20;

/// Format: the hexadecimal digits a writer uses for a control character's `\u00XX` escape.
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// Why bytes are not one JSON value. Each names where reading stopped, from the input's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The input ended inside the value: not yet whole to a reader, and a whole body so cut is no message.
  Truncated {
    /// The input's length.
    at: u64,
  },
  /// A byte the grammar does not allow where it stands.
  Unexpected {
    /// Where it is.
    at: u64,
  },
  /// A raw control character in a string, which RFC 8259 §7 requires escaped.
  Control {
    /// Where it is.
    at: u64,
  },
  /// An escape other than RFC 8259's: a byte after `\` other than `"\/bfnrtu`, a `\u` without four hexadecimal
  /// digits, or a UTF-16 surrogate without its other half.
  Escape {
    /// Where the escape begins.
    at: u64,
  },
  /// Bytes in a string that are not UTF-8 (RFC 8259 §8.1).
  NotUtf8 {
    /// Where they begin.
    at: u64,
  },
  /// A number outside the grammar: a leading zero, a minus or a dot or an exponent without digits.
  Number {
    /// Where the number begins.
    at: u64,
  },
  /// Bytes after the one value the input is.
  Trailing {
    /// Where they begin.
    at: u64,
  },
  /// Nesting deeper than the admission holds the bookkeeping for: an eighth of a byte a level.
  Nesting {
    /// The depth that could not be opened.
    depth: u64,
    /// What the admission refused.
    past: PastAdmission,
  },
}

/// What [`validate`] expects next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
  /// A value.
  Value,
  /// A value or `]`, just inside an array.
  ValueOrEnd,
  /// A key or `}`, just inside an object.
  KeyOrEnd,
  /// A key, after a comma in an object.
  Key,
  /// The colon after a key.
  Colon,
  /// A comma or the end of the open container.
  CommaOrEnd,
  /// The input's end: the value is whole.
  Done,
}

/// The open containers of a value being validated: a bit each, set for an object and clear for an array.
struct Nesting {
  /// The bits, 64 levels a word.
  words: Vec<u64>,
  /// The open containers.
  depth: usize,
}

impl Nesting {
  /// Opens a container, charging a word to `admission` whenever the depth needs one more.
  fn open(&mut self, object: bool, admission: &mut Admission) -> Result<(), Refusal> {
    let word = self.depth.checked_div(LEVELS_PER_WORD).unwrap_or(0);
    let bit = self.depth.checked_rem(LEVELS_PER_WORD).unwrap_or(0);
    let deeper = self.depth.saturating_add(1);
    let refusal = |past| Refusal::Nesting {
      depth: to_u64(deeper),
      past,
    };
    if word >= self.words.len() {
      admission.charge_items::<u64>(1).map_err(refusal)?;
      self.words.try_reserve(1).map_err(|_| {
        refusal(PastAdmission {
          asked: to_u64(core::mem::size_of::<u64>()),
          left: admission.left(),
        })
      })?;
      self.words.push(0);
    }
    if let Some(slot) = self.words.get_mut(word) {
      let mask = 1u64
        .checked_shl(u32::try_from(bit).unwrap_or(0))
        .unwrap_or(0);
      *slot = if object { *slot | mask } else { *slot & !mask };
    }
    self.depth = deeper;
    Ok(())
  }

  /// Whether the innermost open container is an object.
  fn in_object(&self) -> bool {
    let Some(innermost) = self.depth.checked_sub(1) else {
      return false;
    };
    let word = innermost.checked_div(LEVELS_PER_WORD).unwrap_or(0);
    let bit = innermost.checked_rem(LEVELS_PER_WORD).unwrap_or(0);
    let mask = 1u64
      .checked_shl(u32::try_from(bit).unwrap_or(0))
      .unwrap_or(0);
    self.words.get(word).is_some_and(|slot| slot & mask != 0)
  }

  /// Closes the innermost container.
  fn close(&mut self) {
    self.depth = self.depth.saturating_sub(1);
  }
}

/// Validates `input` as exactly one JSON value, with whitespace around it allowed, charging the nesting's bookkeeping
/// to `admission` as it deepens. Reads each byte once.
pub fn validate(input: &[u8], admission: &mut Admission) -> Result<(), Refusal> {
  let mut nesting = Nesting {
    words: Vec::new(),
    depth: 0,
  };
  let mut at = 0usize;
  let mut expect = Expect::Value;
  loop {
    at = skip_whitespace(input, at);
    if expect == Expect::Done {
      return match input.get(at) {
        None => Ok(()),
        Some(_) => Err(Refusal::Trailing { at: to_u64(at) }),
      };
    }
    let Some(&byte) = input.get(at) else {
      return Err(Refusal::Truncated { at: to_u64(at) });
    };
    (at, expect) = step(input, at, byte, expect, &mut nesting, admission)?;
  }
}

/// One step of [`validate`]: reads the token at `at`, whose first byte is `byte`, and answers where reading goes on and
/// what it expects there.
fn step(
  input: &[u8],
  at: usize,
  byte: u8,
  expect: Expect,
  nesting: &mut Nesting,
  admission: &mut Admission,
) -> Result<(usize, Expect), Refusal> {
  let unexpected = Refusal::Unexpected { at: to_u64(at) };
  let after_value = |nesting: &Nesting| {
    if nesting.depth == 0 {
      Expect::Done
    } else {
      Expect::CommaOrEnd
    }
  };
  match (expect, byte) {
    (Expect::ValueOrEnd, b']') | (Expect::KeyOrEnd, b'}') => {
      nesting.close();
      Ok((at.saturating_add(1), after_value(nesting)))
    }
    (Expect::KeyOrEnd | Expect::Key, b'"') => Ok((string_end(input, at)?, Expect::Colon)),
    (Expect::Colon, b':') => Ok((at.saturating_add(1), Expect::Value)),
    (Expect::CommaOrEnd, b',') => {
      let next = if nesting.in_object() {
        Expect::Key
      } else {
        Expect::Value
      };
      Ok((at.saturating_add(1), next))
    }
    (Expect::CommaOrEnd, b']') if !nesting.in_object() => {
      nesting.close();
      Ok((at.saturating_add(1), after_value(nesting)))
    }
    (Expect::CommaOrEnd, b'}') if nesting.in_object() => {
      nesting.close();
      Ok((at.saturating_add(1), after_value(nesting)))
    }
    (Expect::Value | Expect::ValueOrEnd, b'{' | b'[') => {
      let object = byte == b'{';
      nesting.open(object, admission)?;
      let next = if object {
        Expect::KeyOrEnd
      } else {
        Expect::ValueOrEnd
      };
      Ok((at.saturating_add(1), next))
    }
    (Expect::Value | Expect::ValueOrEnd, _) => {
      Ok((scalar_end(input, at, byte)?, after_value(nesting)))
    }
    _ => Err(unexpected),
  }
}

/// The end of the string, number or literal that begins at `at` with `byte`.
fn scalar_end(input: &[u8], at: usize, byte: u8) -> Result<usize, Refusal> {
  match byte {
    b'"' => string_end(input, at),
    b'-' | b'0'..=b'9' => number_end(input, at),
    b't' => literal_end(input, at, b"true"),
    b'f' => literal_end(input, at, b"false"),
    b'n' => literal_end(input, at, b"null"),
    _ => Err(Refusal::Unexpected { at: to_u64(at) }),
  }
}

/// The first byte at or after `at` that is not whitespace.
fn skip_whitespace(input: &[u8], at: usize) -> usize {
  let mut at = at;
  while input.get(at).is_some_and(|byte| WHITESPACE.contains(byte)) {
    at = at.saturating_add(1);
  }
  at
}

/// The end of the literal `word` that begins at `at`.
fn literal_end(input: &[u8], at: usize, word: &[u8]) -> Result<usize, Refusal> {
  for (offset, expected) in word.iter().enumerate() {
    let here = at.saturating_add(offset);
    match input.get(here) {
      None => return Err(Refusal::Truncated { at: to_u64(here) }),
      Some(byte) if byte != expected => return Err(Refusal::Unexpected { at: to_u64(here) }),
      Some(_) => {}
    }
  }
  Ok(at.saturating_add(word.len()))
}

/// The end of the digits that begin at `at`: at least one, or the number that began at `start` is refused (cut short
/// when the input ends there).
fn digits_end(input: &[u8], at: usize, start: usize) -> Result<usize, Refusal> {
  let mut end = at;
  while input.get(end).is_some_and(u8::is_ascii_digit) {
    end = end.saturating_add(1);
  }
  if end > at {
    return Ok(end);
  }
  if end >= input.len() {
    return Err(Refusal::Truncated { at: to_u64(end) });
  }
  Err(Refusal::Number { at: to_u64(start) })
}

/// The end of the number that begins at `at` (RFC 8259 §6: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`).
fn number_end(input: &[u8], at: usize) -> Result<usize, Refusal> {
  let mut end = at;
  if input.get(end) == Some(&b'-') {
    end = end.saturating_add(1);
  }
  match input.get(end) {
    None => return Err(Refusal::Truncated { at: to_u64(end) }),
    Some(b'0') => {
      end = end.saturating_add(1);
      if input.get(end).is_some_and(u8::is_ascii_digit) {
        return Err(Refusal::Number { at: to_u64(at) });
      }
    }
    Some(b'1'..=b'9') => end = digits_end(input, end, at)?,
    Some(_) => return Err(Refusal::Number { at: to_u64(at) }),
  }
  if input.get(end) == Some(&b'.') {
    end = digits_end(input, end.saturating_add(1), at)?;
  }
  if matches!(input.get(end), Some(b'e' | b'E')) {
    end = end.saturating_add(1);
    if matches!(input.get(end), Some(b'+' | b'-')) {
      end = end.saturating_add(1);
    }
    end = digits_end(input, end, at)?;
  }
  Ok(end)
}

/// The value of one hexadecimal digit of either case.
fn hex_value(byte: u8) -> Option<u32> {
  char::from(byte).to_digit(16)
}

/// The UTF-16 code unit of the `\uXXXX` escape whose `u` is at `at`, or a refusal naming the escape at `escape`.
fn code_unit(input: &[u8], at: usize, escape: usize) -> Result<u32, Refusal> {
  let mut unit: u32 = 0;
  for offset in 1..=ESCAPE_DIGITS {
    let here = at.saturating_add(offset);
    let Some(&byte) = input.get(here) else {
      return Err(Refusal::Truncated { at: to_u64(here) });
    };
    let digit = hex_value(byte).ok_or(Refusal::Escape { at: to_u64(escape) })?;
    unit = unit
      .checked_shl(HEX_DIGIT_BITS)
      .map_or(digit, |shifted| shifted | digit);
  }
  Ok(unit)
}

/// The end of the escape whose `\` is at `at`, and the code point it names.
fn escape_end(input: &[u8], at: usize) -> Result<(usize, char), Refusal> {
  let refused = Refusal::Escape { at: to_u64(at) };
  let letter_at = at.saturating_add(1);
  let Some(&letter) = input.get(letter_at) else {
    return Err(Refusal::Truncated {
      at: to_u64(letter_at),
    });
  };
  let simple = match letter {
    b'"' => Some('"'),
    b'\\' => Some('\\'),
    b'/' => Some('/'),
    b'b' => Some('\u{8}'),
    b'f' => Some('\u{c}'),
    b'n' => Some('\n'),
    b'r' => Some('\r'),
    b't' => Some('\t'),
    b'u' => None,
    _ => return Err(refused),
  };
  if let Some(named) = simple {
    return Ok((letter_at.saturating_add(1), named));
  }
  let unit = code_unit(input, letter_at, at)?;
  let after = letter_at.saturating_add(ESCAPE_DIGITS).saturating_add(1);
  if LOW_SURROGATES.contains(&unit) {
    return Err(refused);
  }
  if !HIGH_SURROGATES.contains(&unit) {
    return char::from_u32(unit)
      .map(|named| (after, named))
      .ok_or(refused);
  }
  for (offset, expected) in [b'\\', b'u'].iter().enumerate() {
    let here = after.saturating_add(offset);
    match input.get(here) {
      None => return Err(Refusal::Truncated { at: to_u64(here) }),
      Some(byte) if byte != expected => return Err(refused),
      Some(_) => {}
    }
  }
  let low = code_unit(input, after.saturating_add(1), at)?;
  if !LOW_SURROGATES.contains(&low) {
    return Err(refused);
  }
  let high_bits = unit.saturating_sub(*HIGH_SURROGATES.start());
  let low_bits = low.saturating_sub(*LOW_SURROGATES.start());
  let point = high_bits
    .checked_shl(SURROGATE_BITS)
    .map(|high| high | low_bits)
    .and_then(|bits| bits.checked_add(SUPPLEMENTARY))
    .and_then(char::from_u32)
    .ok_or(refused)?;
  Ok((after.saturating_add(2).saturating_add(ESCAPE_DIGITS), point))
}

/// The end of the UTF-8 sequence that begins at `at` with a byte past ASCII.
fn utf8_end(input: &[u8], at: usize) -> Result<usize, Refusal> {
  let mut end = at;
  while input.get(end).is_some_and(|byte| !byte.is_ascii()) {
    end = end.saturating_add(1);
  }
  let run = input.get(at..end).unwrap_or_default();
  match core::str::from_utf8(run) {
    Ok(_) => Ok(end),
    Err(error) => {
      let bad = at.saturating_add(error.valid_up_to());
      if error.error_len().is_none() && end >= input.len() {
        Err(Refusal::Truncated {
          at: to_u64(input.len()),
        })
      } else {
        Err(Refusal::NotUtf8 { at: to_u64(bad) })
      }
    }
  }
}

/// The end of the string whose opening quote is at `at`: one past its closing quote.
fn string_end(input: &[u8], at: usize) -> Result<usize, Refusal> {
  let mut here = at.saturating_add(1);
  loop {
    let Some(&byte) = input.get(here) else {
      return Err(Refusal::Truncated { at: to_u64(here) });
    };
    here = match byte {
      b'"' => return Ok(here.saturating_add(1)),
      b'\\' => escape_end(input, here)?.0,
      _ if byte < FIRST_UNESCAPED => return Err(Refusal::Control { at: to_u64(here) }),
      _ if byte.is_ascii() => here.saturating_add(1),
      _ => utf8_end(input, here)?,
    };
  }
}

/// A string read from validated JSON, borrowed in place: the bytes between its quotes, escapes not yet resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JsonStr<'a> {
  /// The bytes between the quotes, UTF-8 with well-formed escapes.
  raw: &'a [u8],
}

impl<'a> JsonStr<'a> {
  /// The string's text when it holds no escape: then its bytes are its text.
  pub fn as_str(&self) -> Option<&'a str> {
    if self.raw.contains(&b'\\') {
      return None;
    }
    core::str::from_utf8(self.raw).ok()
  }

  /// The string's characters, its escapes resolved.
  pub fn chars(&self) -> impl Iterator<Item = char> + use<'a> {
    let raw = self.raw;
    let mut at = 0usize;
    core::iter::from_fn(move || {
      let byte = *raw.get(at)?;
      if byte == b'\\' {
        let (end, named) = escape_end(raw, at).ok()?;
        at = end;
        return Some(named);
      }
      let rest = raw.get(at..)?;
      let width = utf8_width(byte);
      let text = core::str::from_utf8(rest.get(..width)?).ok()?;
      at = at.saturating_add(width);
      text.chars().next()
    })
  }

  /// The length of the string's text in UTF-8, its escapes resolved.
  pub fn decoded_len(&self) -> usize {
    self
      .chars()
      .fold(0usize, |len, named| len.saturating_add(named.len_utf8()))
  }

  /// Whether the string's text is `text`, its escapes resolved.
  pub fn is(&self, text: &str) -> bool {
    match self.as_str() {
      Some(plain) => plain == text,
      None => self.chars().eq(text.chars()),
    }
  }

  /// The string's text, owned, its exact length charged to `admission` before it is made.
  pub fn owned(&self, admission: &mut Admission) -> Result<String, PastAdmission> {
    let len = self.decoded_len();
    admission.charge(to_u64(len))?;
    let mut text = String::new();
    text.try_reserve_exact(len).map_err(|_| PastAdmission {
      asked: to_u64(len),
      left: admission.left(),
    })?;
    text.extend(self.chars());
    Ok(text)
  }
}

/// The bytes of the UTF-8 sequence a lead byte begins (1 for ASCII and for a byte no sequence begins with).
fn utf8_width(lead: u8) -> usize {
  match lead.leading_ones() {
    2 => 2,
    3 => 3,
    4 => 4,
    _ => 1,
  }
}

/// A number read from validated JSON, borrowed in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonNumber<'a> {
  /// Its text, as RFC 8259 §6 writes it.
  raw: &'a [u8],
}

/// An integer a [`JsonNumber`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integer {
  /// Zero or more (`-0` is zero).
  NonNegative(u64),
  /// Less than zero: its magnitude.
  Negative(u64),
}

/// Why a [`JsonNumber`] is not an integer of 64 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberRefusal {
  /// It has a fraction or an exponent: git-lfs reads these fields into Go integers, which refuse both.
  NotInteger,
  /// Its magnitude is past 64 bits.
  TooLarge,
}

impl JsonNumber<'_> {
  /// The integer the number writes.
  pub fn integer(&self) -> Result<Integer, NumberRefusal> {
    let (negative, digits) = match self.raw.split_first() {
      Some((b'-', rest)) => (true, rest),
      _ => (false, self.raw),
    };
    if !digits.iter().all(u8::is_ascii_digit) {
      return Err(NumberRefusal::NotInteger);
    }
    let mut magnitude: u64 = 0;
    for digit in digits {
      magnitude = magnitude
        .checked_mul(DECIMAL)
        .and_then(|tens| tens.checked_add(u64::from(digit.saturating_sub(b'0'))))
        .ok_or(NumberRefusal::TooLarge)?;
    }
    Ok(if negative && magnitude != 0 {
      Integer::Negative(magnitude)
    } else {
      Integer::NonNegative(magnitude)
    })
  }
}

/// The kind of a JSON value, by its first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
  /// `{`.
  Object,
  /// `[`.
  Array,
  /// `"`.
  String,
  /// `-` or a digit.
  Number,
  /// `true` or `false`.
  Bool,
  /// `null`.
  Null,
}

/// A value of another kind than the message's field holds, at `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
  /// Where the value begins.
  pub at: u64,
}

/// Walks input [`validate`] passed: each step reads one token or skips one value. Any input it is handed anyway that
/// is not valid answers [`Shape`], never a panic.
#[derive(Clone, Copy, Debug)]
pub struct Cursor<'a> {
  /// The validated input.
  input: &'a [u8],
  /// Where the next token begins, whitespace before it included.
  at: usize,
}

impl<'a> Cursor<'a> {
  /// A cursor at the start of `input`, which [`validate`] passed.
  pub fn new(input: &'a [u8]) -> Self {
    Self { input, at: 0 }
  }

  /// Where the next value begins.
  pub fn position(&self) -> u64 {
    to_u64(skip_whitespace(self.input, self.at))
  }

  /// The refusal for the value at the cursor.
  fn shape(&self) -> Shape {
    Shape {
      at: self.position(),
    }
  }

  /// The kind of the next value.
  pub fn kind(&self) -> Option<Kind> {
    match self.input.get(skip_whitespace(self.input, self.at))? {
      b'{' => Some(Kind::Object),
      b'[' => Some(Kind::Array),
      b'"' => Some(Kind::String),
      b'-' | b'0'..=b'9' => Some(Kind::Number),
      b't' | b'f' => Some(Kind::Bool),
      b'n' => Some(Kind::Null),
      _ => None,
    }
  }

  /// Consumes `byte` as the next token.
  fn expect(&mut self, byte: u8) -> Result<(), Shape> {
    let at = skip_whitespace(self.input, self.at);
    if self.input.get(at) != Some(&byte) {
      return Err(self.shape());
    }
    self.at = at.saturating_add(1);
    Ok(())
  }

  /// Consumes the next token if it is `byte`.
  fn take(&mut self, byte: u8) -> bool {
    let at = skip_whitespace(self.input, self.at);
    if self.input.get(at) == Some(&byte) {
      self.at = at.saturating_add(1);
      return true;
    }
    false
  }

  /// Enters the object that is the next value.
  pub fn begin_object(&mut self) -> Result<(), Shape> {
    self.expect(b'{')
  }

  /// The next key of the object being walked, or `None` at its end, which is consumed. Call it right after
  /// [`Cursor::begin_object`] and after each key's value.
  pub fn next_key(&mut self) -> Result<Option<JsonStr<'a>>, Shape> {
    if self.take(b'}') {
      return Ok(None);
    }
    self.take(b',');
    let key = self.string()?;
    self.expect(b':')?;
    Ok(Some(key))
  }

  /// Enters the array that is the next value.
  pub fn begin_array(&mut self) -> Result<(), Shape> {
    self.expect(b'[')
  }

  /// Whether another element of the array being walked follows (then the cursor is at it), or the array's end, which
  /// is consumed. Call it right after [`Cursor::begin_array`] and after each element.
  pub fn next_element(&mut self) -> Result<bool, Shape> {
    if self.take(b']') {
      return Ok(false);
    }
    self.take(b',');
    Ok(true)
  }

  /// The elements of the array that is the next value, counted without moving the cursor.
  pub fn count_elements(&self) -> Result<usize, Shape> {
    let mut ahead = *self;
    ahead.begin_array()?;
    let mut count = 0usize;
    while ahead.next_element()? {
      ahead.skip_value()?;
      count = count.saturating_add(1);
    }
    Ok(count)
  }

  /// The string that is the next value.
  pub fn string(&mut self) -> Result<JsonStr<'a>, Shape> {
    let start = skip_whitespace(self.input, self.at);
    if self.input.get(start) != Some(&b'"') {
      return Err(self.shape());
    }
    let end = string_end(self.input, start).map_err(|_| self.shape())?;
    let raw = self
      .input
      .get(start.saturating_add(1)..end.saturating_sub(1))
      .ok_or_else(|| self.shape())?;
    self.at = end;
    Ok(JsonStr { raw })
  }

  /// The number that is the next value.
  pub fn number(&mut self) -> Result<JsonNumber<'a>, Shape> {
    let start = skip_whitespace(self.input, self.at);
    if !matches!(self.input.get(start), Some(b'-' | b'0'..=b'9')) {
      return Err(self.shape());
    }
    let end = number_end(self.input, start).map_err(|_| self.shape())?;
    let raw = self.input.get(start..end).ok_or_else(|| self.shape())?;
    self.at = end;
    Ok(JsonNumber { raw })
  }

  /// The `true` or `false` that is the next value.
  pub fn boolean(&mut self) -> Result<bool, Shape> {
    let start = skip_whitespace(self.input, self.at);
    let (word, value): (&[u8], bool) = match self.input.get(start) {
      Some(b't') => (b"true", true),
      Some(b'f') => (b"false", false),
      _ => return Err(self.shape()),
    };
    self.at = literal_end(self.input, start, word).map_err(|_| self.shape())?;
    Ok(value)
  }

  /// Consumes the next value if it is `null`, and says whether it was.
  pub fn null(&mut self) -> bool {
    let start = skip_whitespace(self.input, self.at);
    match literal_end(self.input, start, b"null") {
      Ok(end) => {
        self.at = end;
        true
      }
      Err(_) => false,
    }
  }

  /// Skips the next value whatever it holds, counting brackets: validated input needs no other bookkeeping.
  pub fn skip_value(&mut self) -> Result<(), Shape> {
    let mut depth = 0usize;
    loop {
      let start = skip_whitespace(self.input, self.at);
      let Some(&byte) = self.input.get(start) else {
        return Err(self.shape());
      };
      let end = match byte {
        b'{' | b'[' => {
          depth = depth.saturating_add(1);
          start.saturating_add(1)
        }
        b'}' | b']' => {
          depth = depth.checked_sub(1).ok_or_else(|| self.shape())?;
          start.saturating_add(1)
        }
        b',' | b':' if depth > 0 => start.saturating_add(1),
        _ => scalar_end(self.input, start, byte).map_err(|_| self.shape())?,
      };
      self.at = end;
      if depth == 0 {
        return Ok(());
      }
    }
  }
}

/// Writes `text` as a JSON string: quoted, with `"`, `\` and every control character escaped (RFC 8259 §7).
pub fn put_str(out: &mut impl Out, text: &str) {
  out.put(b"\"");
  let bytes = text.as_bytes();
  let mut plain_from = 0usize;
  for (at, byte) in bytes.iter().enumerate() {
    let escape: Option<&[u8]> = match byte {
      b'"' => Some(b"\\\""),
      b'\\' => Some(b"\\\\"),
      b'\n' => Some(b"\\n"),
      b'\r' => Some(b"\\r"),
      b'\t' => Some(b"\\t"),
      0x08 => Some(b"\\b"),
      0x0c => Some(b"\\f"),
      _ if *byte < FIRST_UNESCAPED => None,
      _ => continue,
    };
    out.put(bytes.get(plain_from..at).unwrap_or_default());
    match escape {
      Some(named) => out.put(named),
      None => {
        let high = LOWER_HEX.get(usize::from(byte.checked_shr(HEX_DIGIT_BITS).unwrap_or(0)));
        let low = LOWER_HEX.get(usize::from(byte & 0x0f));
        out.put(b"\\u00");
        for digit in [high, low].into_iter().flatten() {
          out.put(&[*digit]);
        }
      }
    }
    plain_from = at.saturating_add(1);
  }
  out.put(bytes.get(plain_from..).unwrap_or_default());
  out.put(b"\"");
}

/// Writes `value` as a JSON number, in decimal.
pub fn put_u64(out: &mut impl Out, value: u64) {
  let mut digits = [0u8; U64_DIGITS];
  let mut start = digits.len();
  let mut left = value;
  loop {
    let Some(next) = start.checked_sub(1) else {
      break;
    };
    start = next;
    if let Some(slot) = digits.get_mut(start) {
      let digit = left.checked_rem(DECIMAL).unwrap_or(0);
      *slot = b'0'.saturating_add(u8::try_from(digit).unwrap_or(0));
    }
    left = left.checked_div(DECIMAL).unwrap_or(0);
    if left == 0 {
      break;
    }
  }
  out.put(digits.get(start..).unwrap_or_default());
}

/// Writes `value` as a JSON number, in decimal, with its sign.
pub fn put_i64(out: &mut impl Out, value: i64) {
  if value < 0 {
    out.put(b"-");
  }
  put_u64(out, value.unsigned_abs());
}

#[cfg(test)]
#[path = "../tests/lfs_json.rs"]
mod tests;
