//! The ra_svn item codec (§4.17.4 "5. The wire", its ra_svn item; A-531): the four items every `svn://` and
//! `svn+ssh://` message is made of, read from bytes as they arrive and written as Subversion writes them.
//!
//! The grammar (`subversion/libsvn_ra_svn/protocol` §1, Subversion's `1.14.x` branch at `d434dc14`;
//! `docs/wip/research/vcs-servers-subversion.md` §2): an item is a word, a number, a string or a list, and every item
//! ends with whitespace, a space or a line feed. A word is a letter followed by letters, digits and `-`; a number is
//! decimal digits; a string is its length in decimal, a `:`, and that many bytes of anything; a list is `(`, items, and
//! `)`. Where the document's grammar and the deployed parser (`libsvn_ra_svn/marshal.c` `read_item`) differ, the parser
//! here accepts what the deployed one accepts, since that is what every stock peer was tested against: whitespace after
//! `(` may be absent, any amount of whitespace may separate items, and whitespace before an item is skipped. It writes
//! what the deployed writer writes (`svn_ra_svn__start_list` and its siblings): `( ` to open a list, `) ` to close it,
//! and one space after each word, number and string, so a value written and read again is the same value.
//!
//! What it holds to:
//! - **No number of its own.** Every bound an item is read within is given by the caller in [`Bounds`]: the bytes an
//!   item may take, the longest word, the deepest list and the most items. The server derives them from the data they
//!   govern (the command's prototype, the protocol's words) and from the deployment (the connection's share of its
//!   shard's memory); [`Bounds::of`] gives the tightest bounds a value needs, which is how a bound is derived from data.
//! - **Every length checked before anything is held.** A string's declared length is checked against the item's bound
//!   when its `:` is read, before one of its bytes is waited for; a word's against the word bound as it grows; a list's
//!   depth when it opens; the item count as each item starts. So the caller's buffer never grows past `item_bytes`, and
//!   [`Parsed::NeedMore`] never asks for more than that.
//! - **No bytes kept, nothing read twice.** The parser keeps its place and the lists it has opened, never the bytes:
//!   the caller holds them, hands the same buffer back with more at its end, and drops the first `consumed` bytes once
//!   an item is read. A string's bytes are skipped in one step once they are all there. So a peer that sends one byte at
//!   a time costs one step a byte, never a re-read of what came before.
//! - **Every refusal typed**, with the number of the Subversion error a client is answered with
//!   ([`ItemError::subversion_code`]), and its offset. Strictly more than the deployed parser refuses: a number past
//!   64 bits is refused exactly, where `read_item`'s check misses some values.
//!
//! No I/O, no clock, no randomness, no panic. Reading, writing and measuring a value recurse once per list level, so the
//! caller's depth bound is also the bound on that recursion; the server's is the deepest prototype it reads.

use std::str;

/// The space, one of the grammar's two whitespace bytes.
/// Format: `space = 1*(SP / LF)` (`protocol` §1).
const SPACE: u8 = b' ';

/// The line feed, the grammar's other whitespace byte.
/// Format: `space = 1*(SP / LF)` (`protocol` §1).
const LINE_FEED: u8 = b'\n';

/// The byte that opens a list.
/// Format: `list = "(" space *item ")" space` (`protocol` §1).
const LIST_OPEN: u8 = b'(';

/// The byte that closes a list.
/// Format: `list = "(" space *item ")" space` (`protocol` §1).
const LIST_CLOSE: u8 = b')';

/// The byte between a string's length and its bytes.
/// Format: `string = 1*DIGIT ":" *OCTET space` (`protocol` §1).
const STRING_MARK: u8 = b':';

/// The one byte other than letters and digits a word may hold after its first letter.
/// Format: `word = ALPHA *(ALPHA / DIGIT / "-") space` (`protocol` §1).
const WORD_HYPHEN: u8 = b'-';

/// The base numbers and string lengths are written in.
/// Format: `number = 1*DIGIT space`, decimal (`protocol` §1).
const DECIMAL: u64 = 10;

/// The low four bits of an ASCII digit, which are its value.
/// Format: ASCII `0` to `9` are 0x30 to 0x39.
const DIGIT_VALUE_MASK: u8 = 0x0f;

/// The decimal digits of the largest number, 2^64 - 1, the most any number written here takes.
/// Format: 18,446,744,073,709,551,615 has twenty digits.
const NUMBER_DIGITS: usize = 20;

/// The bytes a list adds around its items: `( ` before them and `) ` after.
/// Format: `svn_ra_svn__start_list` and `svn_ra_svn__end_list` (`marshal.c`) write two bytes each.
const LIST_FRAME_BYTES: usize = 4;

/// One item of the protocol (`protocol` §1), borrowing its words and strings from the bytes it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value<'a> {
  /// A word: a letter, then letters, digits and `-`.
  Word(&'a str),
  /// A number, at most 2^64 - 1.
  Number(u64),
  /// A string: any bytes.
  String(&'a [u8]),
  /// A list of items.
  List(Vec<Value<'a>>),
}

/// The bounds one item is read within, all given by the caller (module documentation).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
  /// The most bytes an item may take, counted from the start of the buffer the parser is given to the item's
  /// terminating whitespace: what the caller's buffer holds for it.
  pub item_bytes: usize,
  /// The longest word.
  pub word_bytes: usize,
  /// The deepest list: an item that is no list has depth 0, `( )` depth 1, `( ( ) )` depth 2.
  pub depth: usize,
  /// The most items one item may be, itself and every item within it counted.
  pub nodes: usize,
}

impl Bounds {
  /// The tightest bounds that admit `value` as [`write`] writes it: its written length, its longest word, its depth and
  /// its item count. `None` when a measure overflows.
  pub fn of(value: &Value<'_>) -> Option<Self> {
    let mut bounds = Self::default();
    bounds.measure(value, 0)?;
    Some(bounds)
  }

  /// Adds `value`, met at list depth `depth`, to the measures.
  fn measure(&mut self, value: &Value<'_>, depth: usize) -> Option<()> {
    self.nodes = self.nodes.checked_add(1)?;
    let written = match value {
      Value::Word(word) => {
        self.word_bytes = self.word_bytes.max(word.len());
        word.len().checked_add(1)?
      }
      Value::Number(number) => decimal_len(*number).checked_add(1)?,
      Value::String(bytes) => {
        let length = u64::try_from(bytes.len()).ok()?;
        decimal_len(length)
          .checked_add(bytes.len())?
          .checked_add(2)?
      }
      Value::List(items) => {
        let inner = depth.checked_add(1)?;
        self.depth = self.depth.max(inner);
        for item in items {
          self.measure(item, inner)?;
        }
        LIST_FRAME_BYTES
      }
    };
    self.item_bytes = self.item_bytes.checked_add(written)?;
    Some(())
  }
}

/// How many decimal digits `number` is written in.
fn decimal_len(number: u64) -> usize {
  let mut digits = 1usize;
  let mut rest = number.wrapping_div(DECIMAL);
  while rest > 0 {
    digits = digits.saturating_add(1);
    rest = rest.wrapping_div(DECIMAL);
  }
  digits
}

/// Why an item was refused, with the offset in the parser's buffer where it was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemError {
  /// A byte that cannot start or continue an item where it stands: foreign bytes, a `)` with no list open, an item not
  /// followed by whitespace (`SVN_ERR_RA_SVN_MALFORMED_DATA`, "Malformed network data").
  Malformed {
    /// Where the byte is.
    at: usize,
  },
  /// A word longer than the word bound (`SVN_ERR_RA_SVN_MALFORMED_DATA`, "Word is too long").
  WordTooLong {
    /// Where the word passed its bound.
    at: usize,
  },
  /// A number past 2^64 - 1 (`SVN_ERR_RA_SVN_MALFORMED_DATA`, "Number is larger than maximum").
  NumberTooLarge {
    /// Where the digit that overflowed is.
    at: usize,
  },
  /// A list opened past the depth bound (`SVN_ERR_RA_SVN_MALFORMED_DATA`, "Items are nested too deeply").
  NestedTooDeep {
    /// Where the `(` is.
    at: usize,
  },
  /// An item that would take more bytes than its bound, a string's declared length counted at its `:`
  /// (`SVN_ERR_RA_SVN_REQUEST_SIZE`).
  ItemTooLarge {
    /// Where the item passed its bound, or the `:` of the string that would.
    at: usize,
  },
  /// An item that would be more items than its bound (`SVN_ERR_RA_SVN_REQUEST_SIZE`).
  TooManyItems {
    /// Where the item past the bound starts.
    at: usize,
  },
  /// The bytes ended inside an item (`SVN_ERR_RA_SVN_CONNECTION_CLOSED`).
  Truncated,
  /// A word to be written that is not a letter followed by letters, digits and `-`.
  InvalidWord,
}

/// `SVN_ERR_RA_SVN_CONNECTION_CLOSED`.
/// Format: the ra_svn category's start, 120,000 + 18 × 5,000 = 210,000, plus 2 (`svn_error_codes.h`; research notes
/// §6).
const SVN_ERR_RA_SVN_CONNECTION_CLOSED: u32 = 210_002;

/// `SVN_ERR_RA_SVN_MALFORMED_DATA`.
/// Format: the ra_svn category's start plus 4 (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_MALFORMED_DATA: u32 = 210_004;

/// `SVN_ERR_RA_SVN_REQUEST_SIZE`.
/// Format: the ra_svn category's start plus 9 (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_REQUEST_SIZE: u32 = 210_009;

impl ItemError {
  /// The number of the Subversion error this refusal is answered with, which `svn` prints after an `E`.
  pub fn subversion_code(self) -> u32 {
    match self {
      Self::Malformed { .. }
      | Self::WordTooLong { .. }
      | Self::NumberTooLarge { .. }
      | Self::NestedTooDeep { .. }
      | Self::InvalidWord => SVN_ERR_RA_SVN_MALFORMED_DATA,
      Self::ItemTooLarge { .. } | Self::TooManyItems { .. } => SVN_ERR_RA_SVN_REQUEST_SIZE,
      Self::Truncated => SVN_ERR_RA_SVN_CONNECTION_CLOSED,
    }
  }
}

/// What one call of [`Parser::parse`] made of the buffer it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
  /// One item was read; the first `consumed` bytes, its terminating whitespace included, are done with.
  Item {
    /// The item.
    value: Value<'a>,
    /// How many bytes from the start of the buffer it took.
    consumed: usize,
  },
  /// The buffer ends inside the item: at least `needed` bytes from its start are needed, never more than the item's
  /// bound.
  NeedMore {
    /// How many bytes from the start of the buffer the item needs at least.
    needed: usize,
  },
}

/// A word or string by its place in the buffer.
#[derive(Clone, Copy, Debug)]
struct Span {
  start: usize,
  end: usize,
}

impl Span {
  /// The bytes the span names.
  fn bytes(self, buffer: &[u8]) -> Result<&[u8], ItemError> {
    buffer
      .get(self.start..self.end)
      .ok_or(ItemError::Malformed { at: self.start })
  }
}

/// An item read so far, by places in the buffer.
#[derive(Clone, Debug)]
enum Node {
  Word(Span),
  Number(u64),
  String(Span),
  List(Vec<Node>),
}

impl Node {
  /// The value the node names in `buffer`.
  fn value(self, buffer: &[u8]) -> Result<Value<'_>, ItemError> {
    Ok(match self {
      Self::Word(span) => {
        let at = span.start;
        Value::Word(str::from_utf8(span.bytes(buffer)?).map_err(|_| ItemError::Malformed { at })?)
      }
      Self::Number(number) => Value::Number(number),
      Self::String(span) => Value::String(span.bytes(buffer)?),
      Self::List(items) => Value::List(
        items
          .into_iter()
          .map(|item| item.value(buffer))
          .collect::<Result<_, _>>()?,
      ),
    })
  }
}

/// Where the parser stands within the current item.
#[derive(Clone, Debug)]
enum State {
  /// Between items: whitespace is skipped, then an item starts or, inside a list, the list ends.
  Between,
  /// The digits of a number, or of a string's length, read so far.
  Digits(u64),
  /// A string's bytes, `[start, end)`; its whitespace comes at `end`.
  Bytes { start: usize, end: usize },
  /// A word that started at `start`.
  Word { start: usize },
  /// A string or a list that ended and waits for its whitespace.
  Ended(Node),
}

/// Reads one item at a time from a buffer the caller holds (module documentation).
#[derive(Clone, Debug)]
pub struct Parser {
  bounds: Bounds,
  scanned: usize,
  state: State,
  open: Vec<Vec<Node>>,
  items: usize,
}

impl Parser {
  /// A parser that reads items within `bounds`.
  pub fn new(bounds: Bounds) -> Self {
    Self {
      bounds,
      scanned: 0,
      state: State::Between,
      open: Vec::new(),
      items: 0,
    }
  }

  /// Reads on from where the last call stopped in `buffer`, the same buffer with more at its end, until an item ends or
  /// the buffer does. After [`Parsed::Item`] the caller drops the first `consumed` bytes and the next call starts at
  /// the new start.
  ///
  /// # Errors
  /// Every refusal of [`ItemError`] but `Truncated` and `InvalidWord`; after one, the parser's place is lost and the
  /// stream is refused whole.
  pub fn parse<'a>(&mut self, buffer: &'a [u8]) -> Result<Parsed<'a>, ItemError> {
    loop {
      let at = self.scanned;
      if let State::Bytes { start, end } = self.state {
        if buffer.len() < end {
          return Ok(Parsed::NeedMore {
            needed: end.saturating_add(1),
          });
        }
        self.state = State::Ended(Node::String(Span { start, end }));
        self.scanned = end;
        continue;
      }
      let Some(&byte) = buffer.get(at) else {
        return self.need_more(at);
      };
      if at >= self.bounds.item_bytes {
        return Err(ItemError::ItemTooLarge { at });
      }
      self.scanned = at.saturating_add(1);
      if let Some(node) = self.step(byte, at)? {
        let consumed = self.scanned;
        self.reset();
        let value = node.value(buffer)?;
        return Ok(Parsed::Item { value, consumed });
      }
    }
  }

  /// Ends the stream: refused when it ends inside an item.
  ///
  /// # Errors
  /// [`ItemError::Truncated`] when an item was begun and not ended.
  pub fn finish(&self) -> Result<(), ItemError> {
    if matches!(self.state, State::Between) && self.open.is_empty() {
      Ok(())
    } else {
      Err(ItemError::Truncated)
    }
  }

  /// The answer for a buffer that ends at `at`, inside an item or before one.
  fn need_more<'a>(&self, at: usize) -> Result<Parsed<'a>, ItemError> {
    if at >= self.bounds.item_bytes {
      return Err(ItemError::ItemTooLarge { at });
    }
    Ok(Parsed::NeedMore {
      needed: at.saturating_add(1),
    })
  }

  /// Starts over, for the next item.
  fn reset(&mut self) {
    self.scanned = 0;
    self.state = State::Between;
    self.open.clear();
    self.items = 0;
  }

  /// Reads one byte at `at`: the item it ends, when it ends the outermost one.
  fn step(&mut self, byte: u8, at: usize) -> Result<Option<Node>, ItemError> {
    match std::mem::replace(&mut self.state, State::Between) {
      State::Between => self.between(byte, at).map(|()| None),
      State::Digits(value) => self.digits(value, byte, at),
      State::Word { start } => self.word(start, byte, at),
      State::Ended(node) => self.ended(node, byte, at),
      State::Bytes { start, end } => {
        self.state = State::Bytes { start, end };
        Err(ItemError::Malformed { at })
      }
    }
  }

  /// A byte between items: whitespace, the end of the list it is in, or the start of an item.
  fn between(&mut self, byte: u8, at: usize) -> Result<(), ItemError> {
    match Start::of(byte) {
      Start::Whitespace => Ok(()),
      Start::Close => {
        let items = self.open.pop().ok_or(ItemError::Malformed { at })?;
        self.state = State::Ended(Node::List(items));
        Ok(())
      }
      Start::Foreign => Err(ItemError::Malformed { at }),
      start => {
        self.count_item(at)?;
        self.begin(start, byte, at)
      }
    }
  }

  /// Starts the item whose first byte, `byte` at `at`, is of kind `start`.
  fn begin(&mut self, start: Start, byte: u8, at: usize) -> Result<(), ItemError> {
    match start {
      Start::Open => {
        if self.open.len() >= self.bounds.depth {
          return Err(ItemError::NestedTooDeep { at });
        }
        self.open.push(Vec::new());
      }
      Start::Digit => self.state = State::Digits(u64::from(byte & DIGIT_VALUE_MASK)),
      Start::Letter => {
        if self.bounds.word_bytes == 0 {
          return Err(ItemError::WordTooLong { at });
        }
        self.state = State::Word { start: at };
      }
      Start::Whitespace | Start::Close | Start::Foreign => return Err(ItemError::Malformed { at }),
    }
    Ok(())
  }

  /// Counts the item that starts at `at` against the item bound.
  fn count_item(&mut self, at: usize) -> Result<(), ItemError> {
    self.items = self.items.saturating_add(1);
    if self.items > self.bounds.nodes {
      return Err(ItemError::TooManyItems { at });
    }
    Ok(())
  }

  /// A byte after a number's digits so far: another digit, the `:` that makes it a string's length, or its whitespace.
  fn digits(&mut self, value: u64, byte: u8, at: usize) -> Result<Option<Node>, ItemError> {
    if byte.is_ascii_digit() {
      let next = value
        .checked_mul(DECIMAL)
        .and_then(|shifted| shifted.checked_add(u64::from(byte & DIGIT_VALUE_MASK)))
        .ok_or(ItemError::NumberTooLarge { at })?;
      self.state = State::Digits(next);
      return Ok(None);
    }
    if byte == STRING_MARK {
      let start = at.saturating_add(1);
      let end = usize::try_from(value)
        .ok()
        .and_then(|length| start.checked_add(length))
        .filter(|end| *end < self.bounds.item_bytes)
        .ok_or(ItemError::ItemTooLarge { at })?;
      self.state = State::Bytes { start, end };
      return Ok(None);
    }
    if is_whitespace(byte) {
      return Ok(self.complete(Node::Number(value)));
    }
    Err(ItemError::Malformed { at })
  }

  /// A byte after a word's letters so far: another letter, digit or `-`, or its whitespace.
  fn word(&mut self, start: usize, byte: u8, at: usize) -> Result<Option<Node>, ItemError> {
    if byte.is_ascii_alphanumeric() || byte == WORD_HYPHEN {
      let length = at.saturating_sub(start).saturating_add(1);
      if length > self.bounds.word_bytes {
        return Err(ItemError::WordTooLong { at });
      }
      self.state = State::Word { start };
      return Ok(None);
    }
    if is_whitespace(byte) {
      return Ok(self.complete(Node::Word(Span { start, end: at })));
    }
    Err(ItemError::Malformed { at })
  }

  /// The byte after a string or a list, which must be whitespace.
  fn ended(&mut self, node: Node, byte: u8, at: usize) -> Result<Option<Node>, ItemError> {
    if is_whitespace(byte) {
      return Ok(self.complete(node));
    }
    Err(ItemError::Malformed { at })
  }

  /// Places an ended item in the list it is in, or gives it back when it is the outermost one.
  fn complete(&mut self, node: Node) -> Option<Node> {
    match self.open.last_mut() {
      Some(list) => {
        list.push(node);
        None
      }
      None => Some(node),
    }
  }
}

/// What a byte read between items is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
  /// Whitespace, skipped.
  Whitespace,
  /// `(`: a list starts.
  Open,
  /// `)`: the list it is in ends.
  Close,
  /// A digit: a number, or a string's length, starts.
  Digit,
  /// A letter: a word starts.
  Letter,
  /// Anything else, which no item starts with.
  Foreign,
}

impl Start {
  /// The kind of `byte`.
  fn of(byte: u8) -> Self {
    if is_whitespace(byte) {
      Self::Whitespace
    } else if byte == LIST_OPEN {
      Self::Open
    } else if byte == LIST_CLOSE {
      Self::Close
    } else if byte.is_ascii_digit() {
      Self::Digit
    } else if byte.is_ascii_alphabetic() {
      Self::Letter
    } else {
      Self::Foreign
    }
  }
}

/// Whether `byte` is the grammar's whitespace.
fn is_whitespace(byte: u8) -> bool {
  byte == SPACE || byte == LINE_FEED
}

/// Whether `word` is one the grammar allows: a letter, then letters, digits and `-`.
fn is_word(word: &str) -> bool {
  let mut bytes = word.bytes();
  bytes
    .next()
    .is_some_and(|first| first.is_ascii_alphabetic())
    && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == WORD_HYPHEN)
}

/// Appends `value` as Subversion's writer writes it: a word, a number or a string followed by one space, a list as `( `,
/// its items and `) `.
///
/// # Errors
/// [`ItemError::InvalidWord`] for a word outside the grammar; what was appended before it stays.
pub fn write(value: &Value<'_>, out: &mut Vec<u8>) -> Result<(), ItemError> {
  match value {
    Value::Word(word) => {
      if !is_word(word) {
        return Err(ItemError::InvalidWord);
      }
      out.extend_from_slice(word.as_bytes());
    }
    Value::Number(number) => write_decimal(*number, out),
    Value::String(bytes) => {
      let length =
        u64::try_from(bytes.len()).map_err(|_| ItemError::ItemTooLarge { at: out.len() })?;
      write_decimal(length, out);
      out.push(STRING_MARK);
      out.extend_from_slice(bytes);
    }
    Value::List(items) => {
      out.push(LIST_OPEN);
      out.push(SPACE);
      for item in items {
        write(item, out)?;
      }
      out.push(LIST_CLOSE);
    }
  }
  out.push(SPACE);
  Ok(())
}

/// Appends `number` in decimal, its shortest form.
fn write_decimal(number: u64, out: &mut Vec<u8>) {
  let mut digits = [0u8; NUMBER_DIGITS];
  let mut rest = number;
  let mut used = 0usize;
  for slot in digits.iter_mut().rev() {
    let [low, ..] = rest.wrapping_rem(DECIMAL).to_le_bytes();
    *slot = b'0' | low;
    used = used.saturating_add(1);
    rest = rest.wrapping_div(DECIMAL);
    if rest == 0 {
      break;
    }
  }
  out.extend_from_slice(
    digits
      .get(NUMBER_DIGITS.saturating_sub(used)..)
      .unwrap_or(&[]),
  );
}

#[cfg(test)]
mod tests;
