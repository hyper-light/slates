//! The authz file's lines, read as `svn_config__parse_stream` reads them (`libsvn_subr/config_file.c`, Subversion's
//! `1.14.x` branch at `d434dc14`; A-606): what a line is, where a section header and an option start, how a value
//! continues onto the next line, and which carriage returns are dropped. Each section header and each option goes to a
//! [`Sink`], as Subversion's reader calls its constructor's callbacks, in file order.
//!
//! Subversion reads through a 16 KiB buffer and drops the carriage returns that open a refill (`parser_getc_plain`
//! falls back to `parser_getc`), so a carriage return inside a name at such a boundary is lost there and kept anywhere
//! else; this reader keeps it everywhere but at a line's start and after `[`, which is what Subversion does for every
//! byte off the boundary.

use std::borrow::Cow;

use super::AuthzRefusal;

/// The NUL byte, refused anywhere in a file and in a path (`authz` module documentation, item 6).
/// Format: ASCII 0.
pub(super) const NUL: u8 = 0;

/// The line feed, which ends a line.
/// Format: `parser_get_line` reads to `'\n'` (`config_file.c` 280-328).
const LINE_FEED: u8 = b'\n';

/// The carriage return, dropped wherever `parser_getc` reads a line's first byte.
/// Format: `while (*c == '\r')` (`config_file.c` 244).
const CARRIAGE_RETURN: u8 = b'\r';

/// The horizontal tab, whitespace.
/// Format: `svn_ctype_table` marks it `SVN_CTYPE_SPACE` (`ctype.c` 58).
const TAB: u8 = b'\t';

/// The vertical tab, whitespace.
/// Format: `ctype.c` 60.
const VERTICAL_TAB: u8 = 0x0b;

/// The form feed, whitespace.
/// Format: `ctype.c` 61.
const FORM_FEED: u8 = 0x0c;

/// The space.
/// Format: `ctype.c` 81.
const SPACE: u8 = b' ';

/// The byte that opens a section header.
/// Format: `case '['` (`config_file.c` 786).
const SECTION_OPEN: u8 = b'[';

/// The byte that ends a section header's name; the rest of its line is ignored.
/// Format: `strchr(ctx->section->data, ']')` (`config_file.c` 570-583).
const SECTION_CLOSE: u8 = b']';

/// The byte that starts a comment.
/// Format: `case '#'` (`config_file.c` 805).
const COMMENT: u8 = b'#';

/// One separator between an option's name and its value.
/// Format: `strchr(ctx->line_read->data, '=')` (`config_file.c` 497).
const EQUALS: u8 = b'=';

/// The other separator, which wins when it comes before the first `=`.
/// Format: `strchr(ctx->line_read->data, ':')` before the `=` (`config_file.c` 498-511).
const COLON: u8 = b':';

/// What joins a continuation line to the value above it.
/// Format: `svn_stringbuf_appendbyte(ctx->value, ' ')` (`config_file.c` 474).
const CONTINUATION_JOIN: u8 = b' ';

/// The UTF-8 byte order mark, skipped at the start of a file.
/// Format: `skip_bom` (`config_file.c` 383-411): 0xEF, 0xBB, 0xBF.
const BYTE_ORDER_MARK: &[u8] = b"\xEF\xBB\xBF";

/// Whether `byte` is whitespace as `svn_ctype_isspace` decides.
pub(super) fn is_space(byte: u8) -> bool {
  matches!(
    byte,
    TAB | LINE_FEED | VERTICAL_TAB | FORM_FEED | CARRIAGE_RETURN | SPACE
  )
}

/// `bytes` without its leading and trailing whitespace (`svn_stringbuf_strip_whitespace`).
pub(super) fn strip(bytes: &[u8]) -> &[u8] {
  trim_end(trim_start(bytes))
}

/// `bytes` without its leading whitespace.
fn trim_start(bytes: &[u8]) -> &[u8] {
  let start = bytes
    .iter()
    .position(|byte| !is_space(*byte))
    .unwrap_or(bytes.len());
  bytes.get(start..).unwrap_or_default()
}

/// `bytes` without its trailing whitespace.
fn trim_end(bytes: &[u8]) -> &[u8] {
  let end = bytes
    .iter()
    .rposition(|byte| !is_space(*byte))
    .map_or(0, |last| last.saturating_add(1));
  bytes.get(..end).unwrap_or_default()
}

/// The line, counted from one, that the byte at `at` stands on.
pub(super) fn line_of(bytes: &[u8], at: usize) -> usize {
  let before = bytes.get(..at).unwrap_or(bytes);
  before
    .iter()
    .filter(|byte| **byte == LINE_FEED)
    .count()
    .saturating_add(1)
}

/// What the reader hands its sections and options to, as `svn_config__parse_stream` calls its constructor.
pub(super) trait Sink<'a> {
  /// A section header whose name is `name` (the bytes between `[` and the first `]`), on `line`.
  ///
  /// # Errors
  /// The refusal that ends the reading.
  fn section(&mut self, name: &'a [u8], line: usize) -> Result<(), AuthzRefusal>;

  /// An option `name` whose value is `value`, stripped and with its continuation lines joined, starting on `line`.
  ///
  /// # Errors
  /// The refusal that ends the reading.
  fn option(
    &mut self,
    name: &'a [u8],
    value: Cow<'a, [u8]>,
    line: usize,
  ) -> Result<(), AuthzRefusal>;
}

/// A refusal and the line it was found on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Refused {
  /// The line the refused header, comment or option starts on.
  pub(super) line: usize,
  /// Why.
  pub(super) kind: AuthzRefusal,
}

/// Reads `bytes` to their end, handing every section and option to `sink` in order.
///
/// # Errors
/// [`Refused`] at the first line that cannot be read, or the first refusal of `sink`.
pub(super) fn read<'a>(bytes: &'a [u8], sink: &mut impl Sink<'a>) -> Result<(), Refused> {
  let mut reader = Reader::new(bytes);
  loop {
    let indent = reader.indent();
    let line = reader.line;
    let outcome = match reader.peek() {
      None => return Ok(()),
      Some(LINE_FEED) => {
        reader.take_line();
        Ok(())
      }
      Some(SECTION_OPEN) => reader.section(indent, sink),
      Some(COMMENT) => reader.comment(indent),
      Some(_) => reader.option(indent, sink),
    };
    outcome.map_err(|kind| Refused { line, kind })?;
  }
}

/// The reader's place in a file.
struct Reader<'a> {
  bytes: &'a [u8],
  at: usize,
  line: usize,
  in_section: bool,
}

impl<'a> Reader<'a> {
  /// A reader at the start of `bytes`, past the carriage returns and the byte order mark that may open them.
  fn new(bytes: &'a [u8]) -> Self {
    let mut reader = Self {
      bytes,
      at: 0,
      line: 1,
      in_section: false,
    };
    reader.skip_carriage_returns();
    if reader.rest().starts_with(BYTE_ORDER_MARK) {
      reader.at = reader.at.saturating_add(BYTE_ORDER_MARK.len());
    }
    reader
  }

  /// The bytes from here to the end.
  fn rest(&self) -> &'a [u8] {
    self.bytes.get(self.at..).unwrap_or_default()
  }

  /// The byte here.
  fn peek(&self) -> Option<u8> {
    self.bytes.get(self.at).copied()
  }

  /// Moves past the byte here.
  fn advance(&mut self) {
    self.at = self.at.saturating_add(1);
  }

  /// Drops the carriage returns here, as `parser_getc` does.
  fn skip_carriage_returns(&mut self) {
    while self.peek() == Some(CARRIAGE_RETURN) {
      self.advance();
    }
  }

  /// At a line's start: drops its carriage returns, then counts the whitespace short of a line feed, as
  /// `skip_whitespace` does (`config_file.c` 331-348).
  fn indent(&mut self) -> usize {
    self.skip_carriage_returns();
    let mut count = 0usize;
    while let Some(byte) = self.peek() {
      if byte == LINE_FEED || !is_space(byte) {
        break;
      }
      count = count.saturating_add(1);
      self.advance();
    }
    count
  }

  /// The rest of this line without its line feed, and whether a line feed ended it; moves past both.
  fn take_line(&mut self) -> (&'a [u8], bool) {
    let rest = self.rest();
    match rest.iter().position(|byte| *byte == LINE_FEED) {
      Some(length) => {
        self.at = self.at.saturating_add(length).saturating_add(1);
        self.line = self.line.saturating_add(1);
        (rest.get(..length).unwrap_or_default(), true)
      }
      None => {
        self.at = self.bytes.len();
        (rest, false)
      }
    }
  }

  /// A section header at `[` (`parse_section_name`).
  fn section(&mut self, indent: usize, sink: &mut impl Sink<'a>) -> Result<(), AuthzRefusal> {
    if indent > 0 {
      return Err(AuthzRefusal::SectionHeaderIndented);
    }
    let line = self.line;
    self.advance();
    self.skip_carriage_returns();
    let (header, _) = self.take_line();
    let end = header
      .iter()
      .position(|byte| *byte == SECTION_CLOSE)
      .ok_or(AuthzRefusal::SectionHeaderUnterminated)?;
    sink.section(header.get(..end).unwrap_or_default(), line)?;
    self.in_section = true;
    Ok(())
  }

  /// A comment at `#`.
  fn comment(&mut self, indent: usize) -> Result<(), AuthzRefusal> {
    if indent > 0 {
      return Err(AuthzRefusal::CommentIndented);
    }
    self.take_line();
    Ok(())
  }

  /// An option, its value continued by the indented lines after it (`parse_option`).
  fn option(&mut self, indent: usize, sink: &mut impl Sink<'a>) -> Result<(), AuthzRefusal> {
    if !self.in_section {
      return Err(AuthzRefusal::SectionHeaderExpected);
    }
    if indent > 0 {
      return Err(AuthzRefusal::OptionExpected);
    }
    let line = self.line;
    let (text, ended) = self.take_line();
    let (name, first) = split_option(text)?;
    let value = if ended {
      self.continued(strip(first))
    } else {
      Cow::Borrowed(strip(first))
    };
    sink.option(name, value, line)
  }

  /// `first` with every continuation line after it, each stripped and joined with one space, up to a blank line, a
  /// line that starts in the first column, or the end (`parse_value_continuation_lines`).
  fn continued(&mut self, first: &'a [u8]) -> Cow<'a, [u8]> {
    let mut value = Cow::Borrowed(first);
    loop {
      let indent = self.indent();
      match self.peek() {
        None => return value,
        Some(LINE_FEED) => {
          self.take_line();
          return value;
        }
        Some(_) if indent == 0 => return value,
        Some(_) => {
          let (text, ended) = self.take_line();
          let joined = value.to_mut();
          joined.push(CONTINUATION_JOIN);
          joined.extend_from_slice(strip(text));
          if !ended {
            return value;
          }
        }
      }
    }
  }
}

/// An option line's name, without the whitespace before its separator, and the first line of its value, without the
/// whitespace after it.
fn split_option(text: &[u8]) -> Result<(&[u8], &[u8]), AuthzRefusal> {
  let separator = separator_of(text).ok_or(AuthzRefusal::OptionSeparatorMissing)?;
  let name = trim_end(text.get(..separator).unwrap_or_default());
  if name.is_empty() {
    return Err(AuthzRefusal::OptionNameEmpty);
  }
  let value = text.get(separator.saturating_add(1)..).unwrap_or_default();
  Ok((name, trim_start(value)))
}

/// Where an option line's separator is: a `:` before the first `=`, else the first `=`, else the first `:`.
fn separator_of(text: &[u8]) -> Option<usize> {
  let colon = text.iter().position(|byte| *byte == COLON);
  match text.iter().position(|byte| *byte == EQUALS) {
    Some(equals) => Some(colon.filter(|colon| *colon < equals).unwrap_or(equals)),
    None => colon,
  }
}
