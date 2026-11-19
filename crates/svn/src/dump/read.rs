//! The dump reader (the module documentation's reading rules): a parser that resumes where it stopped over a buffer its
//! caller holds, keeping its place and the record it is in, never the stream's bytes.
//!
//! Reading in the order `load.c` reads: the version line; then, between records, blank lines and lines that begin with
//! whitespace passed over (`svn_repos_parse_dumpstream3` 625-636); a record's header block up to its empty line; its
//! property section entry by entry; its text in pieces or its delta window by window; and the rest of its content block
//! passed over. A deletion is held until the next record shows whether it is the first half of a replacement.

use super::digest::{Digests, check_text, checks_anything};
use super::header::{Block, Record, parse_number};
use super::{
  Admission, DumpError, Event, Format, LINE_FEED, Md5, NUMBER_DIGITS, NodeAction, NodeHeader,
  PROPS_END_LINE, Property, PropertyFault, Read, Sha1, TextSection, to_u64,
};
use crate::index::Revision;
use crate::svndiff::{Decoded, Decoder, SvndiffError};

/// The version line's text before its number.
/// Format: `svn_repos__dump_magic_header_record` writes `SVN_REPOS_DUMPFILE_MAGIC_HEADER ": %d\n\n"` (`dump.c` 516-524),
/// and `parse_format_version` refuses a first line that does not begin so (`load.c` 361-427).
const VERSION_PREFIX: &[u8] = b"SVN-fs-dump-format-version: ";

/// The bytes Subversion counts as whitespace, any of which begins a line `load.c` passes over between records.
/// Format: `svn_ctype_isspace` (`svn_ctype.h` 84; `ctype.c` 58-62 and 81): tab, line feed, vertical tab, form feed,
/// carriage return and space.
const WHITESPACE: [u8; 6] = [b'\t', b'\n', 0x0b, 0x0c, b'\r', b' '];

/// The prefix of a property entry's line naming a property to set.
/// Format: `hash.c` 289 writes `K <length>\n`; `load.c` 213 reads it.
const SET_PREFIX: &[u8] = b"K ";

/// The prefix of a property entry's line giving the value's length.
/// Format: `hash.c` 290 writes `V <length>\n`; `load.c` 230 reads it.
const VALUE_PREFIX: &[u8] = b"V ";

/// The prefix of a property entry's line naming a property to delete.
/// Format: `hash.c` 313 writes `D <length>\n`; `load.c` 259 reads it.
const DELETE_PREFIX: &[u8] = b"D ";

/// Reads a dump a part at a time from a buffer its caller holds (the module documentation).
#[derive(Clone, Debug)]
pub struct Reader {
  admission: Admission,
  format: Option<Format>,
  revision: Option<Revision>,
  state: State,
  /// How far into the current line its line feed has been searched for, so no byte is searched twice.
  scanned: usize,
  /// A deletion held until the next record shows whether it begins a replacement.
  held_delete: Option<NodeHeader>,
  /// A record read past a held deletion, begun on the next call.
  queued: Option<Record>,
  /// The refusal that ended the stream; every later call answers it again.
  refused: Option<DumpError>,
}

/// Where the reader stands.
#[derive(Clone, Debug)]
enum State {
  /// Before the version line.
  Start,
  /// Between records.
  Gap,
  /// In a record's header block, the headers so far.
  Headers(Box<Block>),
  /// In a property section.
  Properties(Section),
  /// In a whole text.
  Text(Box<WholeText>),
  /// In a text delta.
  Delta(Box<DeltaText>),
  /// In the rest of a content block past its sections: the bytes left to pass over.
  Rest(u64),
  /// The stream ended between records.
  Ended,
}

/// What follows a record's property section, or its header when it has none.
#[derive(Clone, Copy, Debug)]
struct After {
  text: Option<TextSection>,
  rest: u64,
}

/// A property section being read.
#[derive(Clone, Copy, Debug)]
struct Section {
  /// Its bytes left, `PROPS-END` included.
  left: u64,
  /// Whether `D` entries may stand in it: a node's section that is a delta.
  deletions: bool,
  after: After,
}

/// A whole text being read.
#[derive(Clone, Debug)]
struct WholeText {
  left: u64,
  expected_md5: Option<Md5>,
  expected_sha1: Option<Sha1>,
  /// The text's digests so far, when its headers name one to check.
  digests: Option<Digests>,
  rest: u64,
}

/// A text delta being read.
#[derive(Clone, Debug)]
struct DeltaText {
  left: u64,
  decoder: Decoder,
  rest: u64,
}

/// The bytes one call reads from, and how many of them it is done with.
struct Cursor<'a> {
  buffer: &'a [u8],
  start: usize,
  ended: bool,
}

impl<'a> Cursor<'a> {
  /// The bytes not yet consumed.
  fn rest(&self) -> &'a [u8] {
    self.buffer.get(self.start..).unwrap_or_default()
  }

  /// Marks `count` more bytes done with.
  fn consume(&mut self, count: usize) {
    self.start = self.start.saturating_add(count).min(self.buffer.len());
  }
}

/// What one step of reading made.
enum Step<'a> {
  /// Read on.
  Continue,
  /// An event to answer.
  Event(Event<'a>),
  /// At least this many bytes, from the cursor's start, are needed.
  NeedMore(usize),
  /// The stream ended between records.
  End,
}

/// One property entry, or the section's end, read from the start of the bytes given.
enum Entry<'a> {
  NeedMore(usize),
  End,
  Read {
    property: Property<'a>,
    length: usize,
  },
}

/// A held deletion and the record after it: one replacement, or two records.
enum Fold {
  Folded(Record),
  Apart(NodeHeader, Record),
}

impl Reader {
  /// A reader that has read nothing, within `admission`.
  pub fn new(admission: Admission) -> Self {
    Self {
      admission,
      format: None,
      revision: None,
      state: State::Start,
      scanned: 0,
      held_delete: None,
      queued: None,
      refused: None,
    }
  }

  /// Reads on from the start of `buffer`, the bytes the last call did not consume followed by any the caller has
  /// received since, until an event is read, more bytes are needed, or the stream ends; `ended` says `buffer` holds the
  /// stream's last bytes. Events borrow from `buffer`; the caller drops the bytes [`Read`] says were consumed once it is
  /// done with the event.
  ///
  /// # Errors
  /// Every refusal of [`DumpError`] the stream meets; after one, the stream is refused whole and every later call
  /// answers the same refusal.
  pub fn read<'a>(&mut self, buffer: &'a [u8], ended: bool) -> Result<Read<'a>, DumpError> {
    if let Some(refusal) = self.refused {
      return Err(refusal);
    }
    let mut cursor = Cursor {
      buffer,
      start: 0,
      ended,
    };
    let read = self.run(&mut cursor);
    if let Err(refusal) = &read {
      self.refused = Some(*refusal);
    }
    read
  }

  /// Steps until something is to be answered.
  fn run<'a>(&mut self, cursor: &mut Cursor<'a>) -> Result<Read<'a>, DumpError> {
    loop {
      match self.step(cursor)? {
        Step::Continue => {}
        Step::Event(event) => {
          return Ok(Read::Event {
            event,
            consumed: cursor.start,
          });
        }
        Step::NeedMore(needed) => return self.need_more(cursor, needed),
        Step::End => return Ok(Read::End),
      }
    }
  }

  /// The answer when `needed` bytes are needed: refused past the admission or once the stream has ended.
  fn need_more<'a>(&self, cursor: &Cursor<'a>, needed: usize) -> Result<Read<'a>, DumpError> {
    self.admit(needed)?;
    if cursor.ended {
      return Err(DumpError::Truncated);
    }
    Ok(Read::NeedMore {
      consumed: cursor.start,
      needed,
    })
  }

  /// Refuses a part that needs more than the admission held at once.
  fn admit(&self, needed: usize) -> Result<(), DumpError> {
    if needed > self.admission.buffer_bytes {
      return Err(DumpError::PastAdmission {
        needed,
        admitted: self.admission.buffer_bytes,
      });
    }
    Ok(())
  }

  /// One step from where the reader stands.
  fn step<'a>(&mut self, cursor: &mut Cursor<'a>) -> Result<Step<'a>, DumpError> {
    if let Some(record) = self.queued.take() {
      return self.begin(record);
    }
    match std::mem::replace(&mut self.state, State::Ended) {
      State::Start => self.version_line(cursor),
      State::Gap => self.gap(cursor),
      State::Headers(block) => self.header_line(block, cursor),
      State::Properties(section) => self.property(section, cursor),
      State::Text(text) => self.whole_text(text, cursor),
      State::Delta(delta) => self.delta(delta, cursor),
      State::Rest(left) => Ok(self.skip(left, cursor)),
      State::Ended => Ok(Step::End),
    }
  }

  /// The length of the line at the start of `rest`, its line feed excluded, once the line feed is there; searched on
  /// from where the last search stopped, and the line admitted once its length is known.
  fn line(&mut self, rest: &[u8]) -> Result<Option<usize>, DumpError> {
    let from = self.scanned.min(rest.len());
    let found = rest
      .get(from..)
      .unwrap_or_default()
      .iter()
      .position(|&byte| byte == LINE_FEED);
    let Some(offset) = found else {
      self.scanned = rest.len();
      return Ok(None);
    };
    let length = from.saturating_add(offset);
    self.admit(length.saturating_add(1))?;
    self.scanned = 0;
    Ok(Some(length))
  }

  /// The version line, refused at its first byte that cannot begin one.
  fn version_line<'a>(&mut self, cursor: &mut Cursor<'a>) -> Result<Step<'a>, DumpError> {
    self.state = State::Start;
    let rest = cursor.rest();
    let seen = rest.len().min(VERSION_PREFIX.len());
    if rest.get(..seen) != VERSION_PREFIX.get(..seen) {
      return Err(DumpError::MalformedVersion);
    }
    let Some(length) = self.line(rest)? else {
      return Ok(Step::NeedMore(rest.len().saturating_add(1)));
    };
    let number = rest
      .get(..length)
      .and_then(|line| line.get(VERSION_PREFIX.len()..))
      .and_then(parse_number)
      .ok_or(DumpError::MalformedVersion)?;
    let format =
      Format::from_number(number).ok_or(DumpError::UnsupportedVersion { version: number })?;
    cursor.consume(length.saturating_add(1));
    self.format = Some(format);
    self.state = State::Gap;
    Ok(Step::Event(Event::Format(format)))
  }

  /// Between records: a blank line or one that begins with whitespace passed over, a record's first line begun, or the
  /// stream's end.
  fn gap<'a>(&mut self, cursor: &mut Cursor<'a>) -> Result<Step<'a>, DumpError> {
    self.state = State::Gap;
    let rest = cursor.rest();
    let Some(first) = rest.first() else {
      if cursor.ended {
        return Ok(self.end_of_stream());
      }
      return Ok(Step::NeedMore(1));
    };
    if !WHITESPACE.contains(first) {
      self.state = State::Headers(Box::default());
      return Ok(Step::Continue);
    }
    let Some(length) = self.line(rest)? else {
      return Ok(Step::NeedMore(rest.len().saturating_add(1)));
    };
    cursor.consume(length.saturating_add(1));
    Ok(Step::Continue)
  }

  /// The stream's end between records: a held deletion answered, then the end.
  fn end_of_stream<'a>(&mut self) -> Step<'a> {
    match self.held_delete.take() {
      Some(deletion) => Step::Event(Event::Node(deletion)),
      None => {
        self.state = State::Ended;
        Step::End
      }
    }
  }

  /// One line of a header block: a header read, or the block's end and the record it names.
  fn header_line<'a>(
    &mut self,
    mut block: Box<Block>,
    cursor: &mut Cursor<'a>,
  ) -> Result<Step<'a>, DumpError> {
    let rest = cursor.rest();
    let Some(length) = self.line(rest)? else {
      self.state = State::Headers(block);
      return Ok(Step::NeedMore(rest.len().saturating_add(1)));
    };
    let line = rest.get(..length).unwrap_or_default();
    cursor.consume(length.saturating_add(1));
    if !line.is_empty() {
      block.add(line)?;
      self.state = State::Headers(block);
      return Ok(Step::Continue);
    }
    let format = self.format.ok_or(DumpError::MalformedVersion)?;
    let record = block.finish(format, self.revision)?;
    self.dispatch(record)
  }

  /// A record read: begun, or first folded into a held deletion's replacement, or queued behind the deletion.
  fn dispatch<'a>(&mut self, record: Record) -> Result<Step<'a>, DumpError> {
    let Some(deletion) = self.held_delete.take() else {
      return self.begin(record);
    };
    match fold(deletion, record) {
      Fold::Folded(record) => self.begin(record),
      Fold::Apart(deletion, record) => {
        self.queued = Some(record);
        self.state = State::Gap;
        Ok(Step::Event(Event::Node(deletion)))
      }
    }
  }

  /// Begins a record: its event answered and its sections set to be read; a deletion held instead.
  fn begin<'a>(&mut self, record: Record) -> Result<Step<'a>, DumpError> {
    match record {
      Record::Format(format) => {
        self.format = Some(format);
        self.state = State::Gap;
        Ok(Step::Event(Event::Format(format)))
      }
      Record::Uuid(uuid) => {
        self.state = State::Gap;
        Ok(Step::Event(Event::Uuid(uuid)))
      }
      Record::Revision { header, rest } => {
        self.revision = Some(header.number);
        let after = After { text: None, rest };
        self.state = match header.properties {
          Some(left) => State::Properties(Section {
            left,
            deletions: false,
            after,
          }),
          None => rest_or_gap(rest),
        };
        Ok(Step::Event(Event::Revision(header)))
      }
      Record::Node { header, rest } => self.begin_node(header, rest),
    }
  }

  /// Begins a node record; a deletion is held.
  fn begin_node<'a>(&mut self, header: NodeHeader, rest: u64) -> Result<Step<'a>, DumpError> {
    if header.action == NodeAction::Delete {
      self.held_delete = Some(header);
      self.state = State::Gap;
      return Ok(Step::Continue);
    }
    let after = After {
      text: header.text,
      rest,
    };
    self.state = match header.properties {
      Some(section) => State::Properties(Section {
        left: section.length,
        deletions: section.delta,
        after,
      }),
      None => text_or_rest(after)?,
    };
    Ok(Step::Event(Event::Node(header)))
  }

  /// One entry of a property section, or its end.
  fn property<'a>(
    &mut self,
    section: Section,
    cursor: &mut Cursor<'a>,
  ) -> Result<Step<'a>, DumpError> {
    let reading = EntryReading {
      rest: cursor.rest(),
      left: section.left,
      deletions: section.deletions,
      admission: self.admission,
    };
    match reading.entry()? {
      Entry::NeedMore(needed) => {
        self.state = State::Properties(section);
        Ok(Step::NeedMore(needed))
      }
      Entry::End => {
        cursor.consume(PROPS_END_LINE.len());
        self.state = text_or_rest(section.after)?;
        Ok(Step::Event(Event::PropertiesEnd))
      }
      Entry::Read { property, length } => {
        cursor.consume(length);
        self.state = State::Properties(Section {
          left: section.left.saturating_sub(to_u64(length)),
          ..section
        });
        Ok(Step::Event(Event::Property(property)))
      }
    }
  }

  /// A piece of a whole text, or its end with its digests checked.
  fn whole_text<'a>(
    &mut self,
    mut text: Box<WholeText>,
    cursor: &mut Cursor<'a>,
  ) -> Result<Step<'a>, DumpError> {
    if text.left == 0 {
      text.check()?;
      self.state = rest_or_gap(text.rest);
      return Ok(Step::Event(Event::TextEnd));
    }
    let piece = within(cursor.rest(), text.left);
    if piece.is_empty() {
      self.state = State::Text(text);
      return Ok(Step::NeedMore(1));
    }
    if let Some(digests) = text.digests.as_mut() {
      digests.update(piece)?;
    }
    text.left = text.left.saturating_sub(to_u64(piece.len()));
    cursor.consume(piece.len());
    self.state = State::Text(text);
    Ok(Step::Event(Event::Text(piece)))
  }

  /// A part of a text delta, read by svndiff's decoder within the text's section, or the delta's end.
  fn delta<'a>(
    &mut self,
    mut delta: Box<DeltaText>,
    cursor: &mut Cursor<'a>,
  ) -> Result<Step<'a>, DumpError> {
    if delta.left == 0 {
      delta.decoder.finish(&[]).map_err(DumpError::Svndiff)?;
      self.state = rest_or_gap(delta.rest);
      return Ok(Step::Event(Event::TextEnd));
    }
    let input = within(cursor.rest(), delta.left);
    let (event, consumed) = match delta.decoder.decode(input).map_err(DumpError::Svndiff)? {
      Decoded::Header { version, consumed } => (Event::Delta(version), consumed),
      Decoded::Window { window, consumed } => (Event::Window(window), consumed),
      Decoded::NeedMore { needed } => {
        if to_u64(needed) > delta.left {
          return Err(DumpError::Svndiff(SvndiffError::UnexpectedEnd));
        }
        self.state = State::Delta(delta);
        return Ok(Step::NeedMore(needed));
      }
    };
    self.admit(consumed)?;
    delta.left = delta.left.saturating_sub(to_u64(consumed));
    cursor.consume(consumed);
    self.state = State::Delta(delta);
    Ok(Step::Event(event))
  }

  /// Passes over the rest of a content block past its sections.
  fn skip<'a>(&mut self, left: u64, cursor: &mut Cursor<'a>) -> Step<'a> {
    let piece = within(cursor.rest(), left);
    let left = left.saturating_sub(to_u64(piece.len()));
    cursor.consume(piece.len());
    if left == 0 {
      self.state = State::Gap;
      return Step::Continue;
    }
    self.state = State::Rest(left);
    Step::NeedMore(1)
  }
}

impl WholeText {
  /// A whole text of `text`'s section, with `rest` bytes of its content block after it.
  fn new(text: TextSection, rest: u64) -> Result<Self, DumpError> {
    let digests = if checks_anything(text.md5, text.sha1) {
      Some(Digests::new()?)
    } else {
      None
    };
    Ok(Self {
      left: text.length,
      expected_md5: text.md5,
      expected_sha1: text.sha1,
      digests,
      rest,
    })
  }

  /// Refuses a text whose digests are not the ones its headers name.
  fn check(&mut self) -> Result<(), DumpError> {
    match self.digests.take() {
      Some(digests) => check_text(self.expected_md5, self.expected_sha1, digests.finish()?),
      None => Ok(()),
    }
  }
}

/// The state that reads what follows a record's property section, or its header when it has none.
fn text_or_rest(after: After) -> Result<State, DumpError> {
  Ok(match after.text {
    Some(text) if text.delta.is_some() => State::Delta(Box::new(DeltaText {
      left: text.length,
      decoder: Decoder::new(),
      rest: after.rest,
    })),
    Some(text) => State::Text(Box::new(WholeText::new(text, after.rest)?)),
    None => rest_or_gap(after.rest),
  })
}

/// The state that passes over `rest` bytes of a content block, or none.
fn rest_or_gap(rest: u64) -> State {
  if rest == 0 {
    State::Gap
  } else {
    State::Rest(rest)
  }
}

/// The first `left` bytes of `rest`, or all of it when it is shorter.
fn within(rest: &[u8], left: u64) -> &[u8] {
  let end = usize::try_from(left).unwrap_or(usize::MAX).min(rest.len());
  rest.get(..end).unwrap_or_default()
}

/// The one replacement a held deletion and the record after it make, when that record adds the deleted path:
/// `load-fs-vtable.c` applies the two as it applies a replacement (`new_node_record`: a deletion, then
/// `maybe_add_with_history`), and FSFS records one replacement.
fn fold(deletion: NodeHeader, record: Record) -> Fold {
  match record {
    Record::Node { mut header, rest }
      if header.action == NodeAction::Add && header.path == deletion.path =>
    {
      header.action = NodeAction::Replace;
      Fold::Folded(Record::Node { header, rest })
    }
    record => Fold::Apart(deletion, record),
  }
}

/// One property entry's reading: the bytes from its start, the section's bytes left, whether deletions may stand, and
/// the admission each length is checked against.
struct EntryReading<'a> {
  rest: &'a [u8],
  left: u64,
  deletions: bool,
  admission: Admission,
}

impl<'a> EntryReading<'a> {
  /// The entry at the start of the bytes, or the section's end.
  fn entry(&self) -> Result<Entry<'a>, DumpError> {
    if self.left == 0 {
      return Err(property_fault(PropertyFault::Unterminated));
    }
    let Some(line) = self.length_line(0, PropertyFault::Unterminated)? else {
      return Ok(Entry::NeedMore(self.rest.len().saturating_add(1)));
    };
    let text = self.rest.get(..line).unwrap_or_default();
    if text.len().saturating_add(1) == PROPS_END_LINE.len() && PROPS_END_LINE.starts_with(text) {
      return self.end(line);
    }
    if let Some(digits) = text.strip_prefix(SET_PREFIX) {
      return self.set(line, digits);
    }
    if let Some(digits) = text.strip_prefix(DELETE_PREFIX) {
      return self.delete(line, digits);
    }
    Err(property_fault(PropertyFault::MalformedLine))
  }

  /// The length of the length line starting at `at`, its line feed excluded, once it is there, the bytes through it
  /// admitted; refused when it is longer than any length line or `PROPS-END`, and as `cut` when the section ends
  /// inside it.
  fn length_line(&self, at: usize, cut: PropertyFault) -> Result<Option<usize>, DumpError> {
    let longest = SET_PREFIX.len().saturating_add(NUMBER_DIGITS);
    let section = within(self.rest, self.left);
    let tail = section.get(at..).unwrap_or_default();
    if let Some(length) = tail
      .iter()
      .take(longest.saturating_add(1))
      .position(|&byte| byte == LINE_FEED)
    {
      self.admit(at.saturating_add(length).saturating_add(1))?;
      return Ok(Some(length));
    }
    if tail.len() > longest {
      let counted = [SET_PREFIX, VALUE_PREFIX, DELETE_PREFIX]
        .iter()
        .any(|prefix| tail.starts_with(prefix));
      let fault = if counted {
        PropertyFault::MalformedLength
      } else {
        PropertyFault::MalformedLine
      };
      return Err(property_fault(fault));
    }
    if to_u64(section.len()) == self.left {
      return Err(property_fault(cut));
    }
    Ok(None)
  }

  /// Refuses a part of an entry that needs more than the admission held at once.
  fn admit(&self, needed: usize) -> Result<(), DumpError> {
    if needed > self.admission.buffer_bytes {
      return Err(DumpError::PastAdmission {
        needed,
        admitted: self.admission.buffer_bytes,
      });
    }
    Ok(())
  }

  /// `PROPS-END`, which must end the section exactly.
  fn end(&self, line: usize) -> Result<Entry<'a>, DumpError> {
    if to_u64(line.saturating_add(1)) != self.left {
      return Err(property_fault(PropertyFault::EndsEarly));
    }
    Ok(Entry::End)
  }

  /// A `D` entry: a name to delete.
  fn delete(&self, line: usize, digits: &[u8]) -> Result<Entry<'a>, DumpError> {
    if !self.deletions {
      return Err(property_fault(PropertyFault::DeletionOutOfPlace));
    }
    let name_start = line.saturating_add(1);
    let Some(name) = self.counted(name_start, digits)? else {
      return Ok(Entry::NeedMore(self.needed(name_start, digits)?));
    };
    check_name(name.bytes)?;
    Ok(Entry::Read {
      property: Property::Delete { name: name.bytes },
      length: name.end,
    })
  }

  /// A `K` entry and its `V`: a name and the value to set it to.
  fn set(&self, line: usize, digits: &[u8]) -> Result<Entry<'a>, DumpError> {
    let name_start = line.saturating_add(1);
    let Some(name) = self.counted(name_start, digits)? else {
      return Ok(Entry::NeedMore(self.needed(name_start, digits)?));
    };
    check_name(name.bytes)?;
    let Some(value_line) = self.length_line(name.end, PropertyFault::PastSection)? else {
      return Ok(Entry::NeedMore(self.rest.len().saturating_add(1)));
    };
    let value_text = self
      .rest
      .get(name.end..name.end.saturating_add(value_line))
      .unwrap_or_default();
    let value_digits = value_text
      .strip_prefix(VALUE_PREFIX)
      .ok_or(property_fault(PropertyFault::MalformedLine))?;
    let value_start = name.end.saturating_add(value_line).saturating_add(1);
    let Some(value) = self.counted(value_start, value_digits)? else {
      return Ok(Entry::NeedMore(self.needed(value_start, value_digits)?));
    };
    Ok(Entry::Read {
      property: Property::Set {
        name: name.bytes,
        value: value.bytes,
      },
      length: value.end,
    })
  }

  /// The bytes a length line's `digits` count from `start`, and where the line feed after them ends, once they are all
  /// here; refused when the count is not digits, when it runs past the section or the admission, when no line feed
  /// follows, and for a name, when it holds a NUL.
  fn counted(&self, start: usize, digits: &[u8]) -> Result<Option<Counted<'a>>, DumpError> {
    let end = self.needed(start, digits)?;
    if self.rest.len() < end {
      return Ok(None);
    }
    let line_feed = end.saturating_sub(1);
    if self.rest.get(line_feed) != Some(&LINE_FEED) {
      return Err(property_fault(PropertyFault::MalformedData));
    }
    let bytes = self.rest.get(start..line_feed).unwrap_or_default();
    Ok(Some(Counted { bytes, end }))
  }

  /// How many bytes from the entry's start reach past the line feed after the bytes `digits` count from `start`,
  /// refused when the count is not digits or the bytes run past the section or the admission.
  fn needed(&self, start: usize, digits: &[u8]) -> Result<usize, DumpError> {
    let count = parse_number(digits).ok_or(property_fault(PropertyFault::MalformedLength))?;
    let end = to_u64(start)
      .checked_add(count)
      .and_then(|end| end.checked_add(1))
      .filter(|&end| end <= self.left)
      .ok_or(property_fault(PropertyFault::PastSection))?;
    let end = usize::try_from(end).unwrap_or(usize::MAX);
    self.admit(end)?;
    Ok(end)
  }
}

/// The bytes a length line counted, and where the line feed after them ends, from the entry's start.
struct Counted<'a> {
  bytes: &'a [u8],
  end: usize,
}

/// Refuses a property name holding a NUL, which ends Subversion's C string of it.
fn check_name(name: &[u8]) -> Result<(), DumpError> {
  if name.contains(&0) {
    return Err(property_fault(PropertyFault::NameNul));
  }
  Ok(())
}

/// A property section's refusal.
fn property_fault(fault: PropertyFault) -> DumpError {
  DumpError::Property { fault }
}
