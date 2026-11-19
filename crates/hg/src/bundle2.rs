//! Bundle2 (`HG20`), decoded and encoded (A-514; `internals.bundle2` [B]; `mercurial/bundle2.py` [C]).
//!
//! A stream is the magic `HG20`; a signed 32-bit length and that many bytes of stream parameters, space-separated,
//! each `name` or `name=value`, URL-quoted, mandatory when its first letter is upper-case; then parts. A part is a
//! signed 32-bit header length (`0` ends the stream) and a header: a type of 1 to 255 bytes of `[a-zA-Z0-9_:-]`,
//! mandatory when any letter is upper-case; a 32-bit id; a byte each for the mandatory and advisory parameter counts;
//! a byte each for every parameter's key and value length, mandatory parameters first; the keys and values. Its
//! payload follows as signed 32-bit chunk lengths and chunks: `0` ends the part, and `-1` interrupts it, after which
//! one more part (or none, 7.2's "no part found during interruption") follows whole and the interrupted part resumes.
//!
//! The only stream parameter Mercurial defines is `Compression` (`UN`, `GZ`, `BZ`, `ZS`), matched without regard to
//! case as 7.2 matches it (`b2streamparamsmap[name.lower()]`). This module reads `UN` streams and refuses the
//! compressed ones by kind until the engines are wired (D-17). Any other mandatory stream parameter is refused, as
//! bundle2 requires, and an advisory one is kept. Parts are returned for their handlers: a mandatory part no handler
//! takes is the handler table's to refuse (piece 5), not the codec's.
//!
//! A decoded stream is its stream parameters, its parts in the order their headers appear (nested ones included, each
//! with its depth), and its payload as [`Event`]s in stream order, which is exactly what an encode writes back, so a
//! stream round-trips byte for byte, chunk boundaries and interruptions included.
//!
//! Stricter than 7.2, each on purpose: a part header with bytes left after its parameters is refused (7.2 ignores
//! them), and one whose parameters run past its end (7.2 slices them short); a part that repeats a parameter key is
//! refused (the document says keys "MUST be unique"; 7.2 keeps the last); bytes after the stream's end are refused.

use crate::out::{Measure, Out};
use crate::quote::{self, PATH_SAFE};
use crate::read::{Cursor, to_u64};
use crate::{Admission, PastAdmission, Truncated};

/// Format: bundle2's magic (`internals.bundle2` "Stream Format").
pub const MAGIC: [u8; 4] = *b"HG20";

/// Format: the payload chunk length that interrupts a part (`flaginterrupt = -1`, `bundle2.py`).
pub const INTERRUPT: i32 = -1;

/// Format: the length that ends a part's payload, and as a part header's length the stream (`internals.bundle2`).
pub const END: i32 = 0;

/// Format: the one stream parameter Mercurial defines (`b2streamparamsmap`), matched without regard to case.
pub const COMPRESSION: &[u8] = b"compression";

/// Format: the `Compression` value this module reads: `UN`, the bundle type of Mercurial's `none` engine.
pub const UNCOMPRESSED: &[u8] = b"UN";

/// Format: the `Compression` values Mercurial defines besides `UN`: zlib, bzip2 and zstd (`supportedbundletypes`).
pub const COMPRESSED: [&[u8]; 3] = [b"GZ", b"BZ", b"ZS"];

/// Format: the bytes a part type may hold besides letters and digits (`_parttypeforbidden`, `bundle2.py`).
const PART_TYPE_EXTRA: &[u8] = b"_:-";

/// A stream parameter, unquoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamParam {
  /// Its name.
  pub name: Vec<u8>,
  /// Its value, if it has one.
  pub value: Option<Vec<u8>>,
}

impl StreamParam {
  /// Whether the parameter is mandatory: its first letter is upper-case.
  pub fn is_mandatory(&self) -> bool {
    self.name.first().is_some_and(u8::is_ascii_uppercase)
  }
}

/// A part parameter: its key and its value, as sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param<'a> {
  /// The key.
  pub key: &'a [u8],
  /// The value.
  pub value: &'a [u8],
}

/// A part's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part<'a> {
  /// The part's type as sent: its case says whether it is mandatory.
  pub kind: &'a [u8],
  /// The part's id.
  pub id: u32,
  /// Its parameters, the mandatory ones first.
  pub params: Vec<Param<'a>>,
  /// How many of `params` are mandatory.
  pub mandatory_params: usize,
  /// 0 for a part of the stream; n for a part that interrupted a part at depth n − 1.
  pub depth: u64,
}

impl<'a> Part<'a> {
  /// Whether the part is mandatory: its type holds an upper-case letter (7.2: `type != type.lower()`).
  pub fn is_mandatory(&self) -> bool {
    self.kind.iter().any(u8::is_ascii_uppercase)
  }

  /// Whether the part's type is `name`, without regard to case, as 7.2 routes it to a handler.
  pub fn is(&self, name: &[u8]) -> bool {
    self.kind.eq_ignore_ascii_case(name)
  }

  /// The value of the parameter `key`, mandatory or advisory.
  pub fn param(&self, key: &[u8]) -> Option<&'a [u8]> {
    self
      .params
      .iter()
      .find(|param| param.key == key)
      .map(|param| param.value)
  }

  /// The mandatory parameters.
  pub fn mandatory(&self) -> &[Param<'a>] {
    self.params.get(..self.mandatory_params).unwrap_or_default()
  }

  /// The advisory parameters.
  pub fn advisory(&self) -> &[Param<'a>] {
    self.params.get(self.mandatory_params..).unwrap_or_default()
  }
}

/// One step of a stream's payload, in stream order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
  /// A part's header: the part's index in [`Bundle2::parts`].
  Open(usize),
  /// One payload chunk of the innermost open part.
  Chunk(&'a [u8]),
  /// The innermost open part was interrupted; the next event opens the part that interrupted it.
  Interrupt,
  /// An interruption that carried no part (a header length of 0 after the `-1`).
  EmptyInterrupt,
  /// The innermost open part ended.
  Close,
}

/// A decoded bundle2 stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle2<'a> {
  /// The stream parameters, unquoted, in order.
  pub params: Vec<StreamParam>,
  /// Every part, in the order its header appears, nested ones included.
  pub parts: Vec<Part<'a>>,
  /// The payload, in stream order.
  pub events: Vec<Event<'a>>,
}

/// Why a bundle2 stream was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The first four bytes are not `HG20`.
  ForeignMagic {
    /// The bytes found.
    found: [u8; 4],
  },
  /// A length past the bytes left.
  Truncated(Truncated),
  /// A negative length where 7.2 refuses one ("negative bundle param size", "negative part header size", "negative
  /// chunk size"); `0xFFFF_FFFF` is −1 here.
  NegativeLength {
    /// The length's place.
    what: &'static str,
    /// Its value.
    value: i32,
  },
  /// A stream parameter with no name (7.2: "empty parameter name").
  EmptyParameterName,
  /// A stream parameter whose name does not begin with a letter (7.2: "non letter first character").
  ParameterNameNotLetter {
    /// The name.
    name: Vec<u8>,
  },
  /// A mandatory stream parameter Mercurial does not define.
  UnknownMandatoryParameter {
    /// The name.
    name: Vec<u8>,
  },
  /// A compressed stream, refused until the compression engines are wired (D-17).
  CompressedStream {
    /// The compression named.
    engine: Vec<u8>,
  },
  /// A `Compression` parameter with no value or one Mercurial does not define.
  UnknownCompression {
    /// The value, if any.
    engine: Option<Vec<u8>>,
  },
  /// A part type that is empty or holds a byte outside `[a-zA-Z0-9_:-]`.
  PartType {
    /// The type.
    kind: Vec<u8>,
  },
  /// A part header with bytes left after its parameters.
  HeaderLeftover {
    /// The part's type.
    kind: Vec<u8>,
    /// The bytes left.
    bytes: u64,
  },
  /// A part that names one parameter key twice.
  DuplicateParameter {
    /// The part's type.
    kind: Vec<u8>,
    /// The key.
    key: Vec<u8>,
  },
  /// Bytes after the stream's end.
  TrailingBytes {
    /// How many.
    bytes: u64,
  },
  /// More structure than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<Truncated> for Refusal {
  fn from(truncated: Truncated) -> Self {
    Self::Truncated(truncated)
  }
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Why a stream could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A part type that is empty, longer than a byte can count, or holds a byte outside `[a-zA-Z0-9_:-]`.
  PartType {
    /// The type.
    kind: Vec<u8>,
  },
  /// More parameters of one kind, or a longer key or value, than a byte can count.
  ParameterCount {
    /// The part's type.
    kind: Vec<u8>,
  },
  /// A block or a chunk longer than a signed 32-bit length holds.
  TooLong {
    /// What was too long.
    what: &'static str,
    /// Its length.
    len: u64,
  },
  /// An empty chunk: its length, 0, would read as the part's end.
  EmptyChunk {
    /// The event's index.
    at: usize,
  },
  /// Events that are not a well-formed stream.
  Malformed {
    /// The event's index.
    at: usize,
    /// What is wrong.
    reason: &'static str,
  },
}

/// A part header, read but not yet built into a [`Part`].
struct Header<'a> {
  /// The type.
  kind: &'a [u8],
  /// The id.
  id: u32,
  /// How many parameters are mandatory.
  mandatory: usize,
  /// Each parameter's key length and value length.
  sizes: &'a [u8],
  /// The keys and values, one after another.
  values: &'a [u8],
}

impl<'a> Header<'a> {
  /// How many parameters the header names.
  fn count(&self) -> usize {
    self.sizes.len() / 2
  }

  /// The parameters, in order.
  fn params(&self) -> impl Iterator<Item = Param<'a>> + use<'a> {
    let mut values = Cursor::new(self.values);
    let sizes: &'a [u8] = self.sizes;
    let (pairs, _) = sizes.as_chunks::<2>();
    pairs.iter().map_while(move |&[key_len, value_len]| {
      let key = values
        .take(usize::from(key_len), "part parameter key")
        .ok()?;
      let value = values
        .take(usize::from(value_len), "part parameter value")
        .ok()?;
      Some(Param { key, value })
    })
  }
}

/// Where a walk of the parts puts what it finds: the counting pass and the building pass.
trait Sink<'a> {
  /// A part's header, at `depth`.
  fn part(&mut self, header: &Header<'a>, depth: u64);
  /// A payload event.
  fn event(&mut self, event: Event<'a>);
}

/// The counting pass: what the building pass will hold.
#[derive(Default)]
struct Count {
  /// Parts.
  parts: usize,
  /// Part parameters, all parts together.
  params: usize,
  /// Events.
  events: usize,
}

impl<'a> Sink<'a> for Count {
  fn part(&mut self, header: &Header<'a>, _depth: u64) {
    self.parts = self.parts.saturating_add(1);
    self.params = self.params.saturating_add(header.count());
    self.events = self.events.saturating_add(1);
  }

  fn event(&mut self, _event: Event<'a>) {
    self.events = self.events.saturating_add(1);
  }
}

/// The building pass, into vectors the counting pass sized.
struct Build<'a> {
  /// The parts.
  parts: Vec<Part<'a>>,
  /// The events.
  events: Vec<Event<'a>>,
}

impl<'a> Sink<'a> for Build<'a> {
  fn part(&mut self, header: &Header<'a>, depth: u64) {
    let mut params = Vec::with_capacity(header.count());
    params.extend(header.params());
    self.events.push(Event::Open(self.parts.len()));
    self.parts.push(Part {
      kind: header.kind,
      id: header.id,
      params,
      mandatory_params: header.mandatory,
      depth,
    });
  }

  fn event(&mut self, event: Event<'a>) {
    self.events.push(event);
  }
}

impl<'a> Bundle2<'a> {
  /// Decodes a bundle2 stream. Its stream parameters are checked first; then its parts are walked once to check and
  /// count them, the exact total charged to `admission`, and walked again to build.
  pub fn decode(input: &'a [u8], admission: &mut Admission) -> Result<Self, Refusal> {
    let mut cursor = Cursor::new(input);
    let found: [u8; 4] = cursor.array("bundle2 magic")?;
    if found != MAGIC {
      return Err(Refusal::ForeignMagic { found });
    }
    let block = sized(&mut cursor, "stream parameters length", "stream parameters")?;
    let params = stream_params(block, admission)?;
    let mut count = Count::default();
    walk(&mut cursor.clone(), &mut count)?;
    admission.charge_items::<Part<'_>>(count.parts)?;
    admission.charge_items::<Param<'_>>(count.params)?;
    admission.charge_items::<Event<'_>>(count.events)?;
    let mut build = Build {
      parts: Vec::with_capacity(count.parts),
      events: Vec::with_capacity(count.events),
    };
    walk(&mut cursor, &mut build)?;
    Ok(Self {
      params,
      parts: build.parts,
      events: build.events,
    })
  }

  /// The parts of the stream itself, not interrupting ones, with their indices.
  pub fn top_level(&self) -> impl Iterator<Item = (usize, &Part<'a>)> {
    self
      .parts
      .iter()
      .enumerate()
      .filter(|(_, part)| part.depth == 0)
  }

  /// The payload of part `index`: its own chunks in order, the parts that interrupted it left out. An index no part
  /// has yields nothing.
  pub fn payload(&self, index: usize) -> Payload<'_, 'a> {
    let start = self
      .events
      .iter()
      .position(|event| *event == Event::Open(index));
    Payload {
      events: start
        .and_then(|open| self.events.get(open.saturating_add(1)..))
        .unwrap_or_default(),
      nested: 0,
      done: start.is_none(),
    }
  }

  /// The payload of part `index` in one vector, its length charged to `admission` first.
  pub fn payload_bytes(
    &self,
    index: usize,
    admission: &mut Admission,
  ) -> Result<Vec<u8>, PastAdmission> {
    let len = self.payload(index).fold(0, |total: u64, chunk| {
      total.saturating_add(to_u64(chunk.len()))
    });
    admission.charge(len)?;
    let capacity = usize::try_from(len).map_err(|_| PastAdmission {
      asked: len,
      left: admission.left(),
    })?;
    let mut out = Vec::with_capacity(capacity);
    for chunk in self.payload(index) {
      out.extend_from_slice(chunk);
    }
    Ok(out)
  }

  /// The encoded stream's length, for a caller that charges it before encoding.
  pub fn encoded_len(&self) -> Result<u64, EncodeRefusal> {
    let mut measure = Measure(0);
    self.encode_into(&mut measure)?;
    Ok(measure.0)
  }

  /// Appends the encoded stream to `out`.
  pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
    self.encode_into(out)
  }

  /// Writes the stream: magic, parameters, every event, the end.
  fn encode_into(&self, out: &mut impl Out) -> Result<(), EncodeRefusal> {
    out.put(&MAGIC);
    let mut block = Vec::new();
    for (position, param) in self.params.iter().enumerate() {
      if position > 0 {
        block.push(b' ');
      }
      quote::quote(&param.name, PATH_SAFE, &mut block);
      if let Some(value) = &param.value {
        block.push(b'=');
        quote::quote(value, PATH_SAFE, &mut block);
      }
    }
    put_length(out, block.len(), "stream parameters")?;
    out.put(&block);
    let mut state = Replay::default();
    for (at, event) in self.events.iter().enumerate() {
      state.event(self, at, *event, out)?;
    }
    state.finish(self.events.len())?;
    out.put(&END.to_be_bytes());
    Ok(())
  }
}

/// The payload of one part; see [`Bundle2::payload`].
pub struct Payload<'b, 'a> {
  /// The events after the part's header.
  events: &'b [Event<'a>],
  /// How many parts that interrupted it are open.
  nested: u64,
  /// Whether the part has ended.
  done: bool,
}

impl<'a> Iterator for Payload<'_, 'a> {
  type Item = &'a [u8];

  fn next(&mut self) -> Option<&'a [u8]> {
    while !self.done {
      let (event, rest) = self.events.split_first()?;
      self.events = rest;
      match *event {
        Event::Chunk(chunk) if self.nested == 0 => return Some(chunk),
        Event::Open(_) => self.nested = self.nested.saturating_add(1),
        Event::Close if self.nested == 0 => self.done = true,
        Event::Close => self.nested = self.nested.saturating_sub(1),
        Event::Chunk(_) | Event::Interrupt | Event::EmptyInterrupt => {}
      }
    }
    None
  }
}

/// A length-prefixed block: a signed 32-bit length, refused if negative, then that many bytes.
fn sized<'a>(
  cursor: &mut Cursor<'a>,
  what: &'static str,
  block: &'static str,
) -> Result<&'a [u8], Refusal> {
  let value = cursor.i32(what)?;
  let len = usize::try_from(value).map_err(|_| Refusal::NegativeLength { what, value })?;
  Ok(cursor.take(len, block)?)
}

/// The stream parameters of `block`: counted, charged, then unquoted and checked one by one.
fn stream_params(block: &[u8], admission: &mut Admission) -> Result<Vec<StreamParam>, Refusal> {
  if block.is_empty() {
    return Ok(Vec::new());
  }
  let items = block.split(|byte| *byte == b' ');
  admission.charge_items::<StreamParam>(items.clone().count())?;
  admission.charge(to_u64(block.len()))?;
  let mut params = Vec::with_capacity(items.clone().count());
  for item in items {
    let mut halves = item.splitn(2, |byte| *byte == b'=');
    let param = StreamParam {
      name: unquote(halves.next().unwrap_or_default()),
      value: halves.next().map(unquote),
    };
    check_stream_param(&param)?;
    params.push(param);
  }
  Ok(params)
}

/// 7.2's `_processparam`: a name that begins with a letter; `Compression` read; any other mandatory name refused.
fn check_stream_param(param: &StreamParam) -> Result<(), Refusal> {
  let first = param.name.first().ok_or(Refusal::EmptyParameterName)?;
  if !first.is_ascii_alphabetic() {
    return Err(Refusal::ParameterNameNotLetter {
      name: param.name.clone(),
    });
  }
  if param.name.eq_ignore_ascii_case(COMPRESSION) {
    return check_compression(param.value.as_deref());
  }
  if first.is_ascii_uppercase() {
    return Err(Refusal::UnknownMandatoryParameter {
      name: param.name.clone(),
    });
  }
  Ok(())
}

/// 7.2's `processcompression`: `UN` read; the other engines refused by kind; anything else unknown.
fn check_compression(value: Option<&[u8]>) -> Result<(), Refusal> {
  match value {
    Some(engine) if engine == UNCOMPRESSED => Ok(()),
    Some(engine) if COMPRESSED.contains(&engine) => Err(Refusal::CompressedStream {
      engine: engine.to_vec(),
    }),
    other => Err(Refusal::UnknownCompression {
      engine: other.map(<[u8]>::to_vec),
    }),
  }
}

/// Python's `unquote_to_bytes` of one stream parameter's name or value ([`quote::unquote`]).
fn unquote(quoted: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(quoted.len());
  quote::unquote(quoted, &mut out);
  out
}

/// Whether `kind` is a part type 7.2 accepts: 1 to 255 bytes of `[a-zA-Z0-9_:-]` (`validateparttype`).
fn valid_kind(kind: &[u8]) -> bool {
  !kind.is_empty()
    && kind
      .iter()
      .all(|byte| byte.is_ascii_alphanumeric() || PART_TYPE_EXTRA.contains(byte))
}

/// Reads one part header after its length, or `None` for a length of 0.
fn read_header<'a>(
  cursor: &mut Cursor<'a>,
  what: &'static str,
) -> Result<Option<Header<'a>>, Refusal> {
  let value = cursor.i32(what)?;
  if value == END {
    return Ok(None);
  }
  let len = usize::try_from(value).map_err(|_| Refusal::NegativeLength { what, value })?;
  parse_header(cursor.take(len, "part header")?).map(Some)
}

/// One part header's fields, every length held to the header's own bytes, nothing left over, no key twice.
fn parse_header(block: &[u8]) -> Result<Header<'_>, Refusal> {
  let mut cursor = Cursor::new(block);
  let kind_len = usize::from(cursor.u8("part type length")?);
  let kind = cursor.take(kind_len, "part type")?;
  if !valid_kind(kind) {
    return Err(Refusal::PartType {
      kind: kind.to_vec(),
    });
  }
  let id = cursor.u32("part id")?;
  let mandatory = usize::from(cursor.u8("mandatory parameter count")?);
  let advisory = usize::from(cursor.u8("advisory parameter count")?);
  let pairs = mandatory.saturating_add(advisory);
  let sizes = cursor.take(pairs.saturating_mul(2), "part parameter sizes")?;
  let values_len = sizes.iter().fold(0, |total: usize, size| {
    total.saturating_add(usize::from(*size))
  });
  let values = cursor.take(values_len, "part parameter keys and values")?;
  if !cursor.is_done() {
    return Err(Refusal::HeaderLeftover {
      kind: kind.to_vec(),
      bytes: to_u64(cursor.left()),
    });
  }
  let header = Header {
    kind,
    id,
    mandatory,
    sizes,
    values,
  };
  check_unique(&header)?;
  Ok(header)
}

/// Refuses a header that names one key twice, comparing each key with those before it, with nothing allocated.
fn check_unique(header: &Header<'_>) -> Result<(), Refusal> {
  for (position, param) in header.params().enumerate() {
    if header
      .params()
      .take(position)
      .any(|earlier| earlier.key == param.key)
    {
      return Err(Refusal::DuplicateParameter {
        kind: header.kind.to_vec(),
        key: param.key.to_vec(),
      });
    }
  }
  Ok(())
}

/// Walks every part of the stream and its payload into `sink`, then requires the input to end where the stream does.
fn walk<'a>(cursor: &mut Cursor<'a>, sink: &mut impl Sink<'a>) -> Result<(), Refusal> {
  while let Some(header) = read_header(cursor, "part header length")? {
    sink.part(&header, 0);
    walk_payload(cursor, sink)?;
  }
  if cursor.is_done() {
    Ok(())
  } else {
    Err(Refusal::TrailingBytes {
      bytes: to_u64(cursor.left()),
    })
  }
}

/// Walks one part's payload, and every part that interrupts it, with a count of the parts open and no recursion.
fn walk_payload<'a>(cursor: &mut Cursor<'a>, sink: &mut impl Sink<'a>) -> Result<(), Refusal> {
  let mut open: u64 = 1;
  while open > 0 {
    let value = cursor.i32("payload chunk length")?;
    open = step(cursor, sink, value, open)?;
  }
  Ok(())
}

/// One payload step: a chunk, the end of the innermost open part, or an interruption.
fn step<'a>(
  cursor: &mut Cursor<'a>,
  sink: &mut impl Sink<'a>,
  value: i32,
  open: u64,
) -> Result<u64, Refusal> {
  match value {
    END => {
      sink.event(Event::Close);
      Ok(open.saturating_sub(1))
    }
    INTERRUPT => match read_header(cursor, "interrupting part header length")? {
      None => {
        sink.event(Event::EmptyInterrupt);
        Ok(open)
      }
      Some(header) => {
        sink.event(Event::Interrupt);
        sink.part(&header, open);
        Ok(open.saturating_add(1))
      }
    },
    _ => {
      let what = "payload chunk length";
      let len = usize::try_from(value).map_err(|_| Refusal::NegativeLength { what, value })?;
      sink.event(Event::Chunk(cursor.take(len, "payload chunk")?));
      Ok(open)
    }
  }
}

/// Writes a signed 32-bit length, refusing one it cannot hold.
fn put_length(out: &mut impl Out, len: usize, what: &'static str) -> Result<(), EncodeRefusal> {
  let value = i32::try_from(len).map_err(|_| EncodeRefusal::TooLong {
    what,
    len: to_u64(len),
  })?;
  out.put(&value.to_be_bytes());
  Ok(())
}

/// The state of an encode's replay of the events: parts open, and whether an interruption awaits its part.
#[derive(Default)]
struct Replay {
  /// Parts open.
  open: u64,
  /// Whether the previous event was an interruption awaiting its part.
  interrupted: bool,
}

impl Replay {
  /// Writes one event, refusing one that does not fit where it stands.
  fn event(
    &mut self,
    stream: &Bundle2<'_>,
    at: usize,
    event: Event<'_>,
    out: &mut impl Out,
  ) -> Result<(), EncodeRefusal> {
    if let Event::Open(index) = event {
      return self.open_part(stream, at, index, out);
    }
    if self.open == 0 || self.interrupted {
      return Err(EncodeRefusal::Malformed {
        at,
        reason: "a payload event outside an open part, or between an interruption and its part",
      });
    }
    self.payload_event(at, event, out)
  }

  /// Writes a payload event inside an open part: a chunk; an interruption (`-1`); an empty one (`-1`, then a header
  /// length of 0); the part's end (`0`).
  fn payload_event(
    &mut self,
    at: usize,
    event: Event<'_>,
    out: &mut impl Out,
  ) -> Result<(), EncodeRefusal> {
    match event {
      Event::Chunk(chunk) => return put_chunk(out, at, chunk),
      Event::Open(_) => return Ok(()),
      Event::Interrupt => self.interrupted = true,
      Event::EmptyInterrupt => out.put(&INTERRUPT.to_be_bytes()),
      Event::Close => self.open = self.open.saturating_sub(1),
    }
    let length = if matches!(event, Event::Interrupt) {
      INTERRUPT
    } else {
      END
    };
    out.put(&length.to_be_bytes());
    Ok(())
  }

  /// Writes a part's header where one may open: at the stream's top level, or after an interruption.
  fn open_part(
    &mut self,
    stream: &Bundle2<'_>,
    at: usize,
    index: usize,
    out: &mut impl Out,
  ) -> Result<(), EncodeRefusal> {
    if self.open > 0 && !self.interrupted {
      return Err(EncodeRefusal::Malformed {
        at,
        reason: "a part opened inside another without an interruption",
      });
    }
    let part = stream.parts.get(index).ok_or(EncodeRefusal::Malformed {
      at,
      reason: "an event opens a part the stream does not hold",
    })?;
    let header = encode_header(part)?;
    put_length(out, header.len(), "part header")?;
    out.put(&header);
    self.open = self.open.saturating_add(1);
    self.interrupted = false;
    Ok(())
  }

  /// Refuses a stream that ends with a part open or an interruption awaiting its part.
  fn finish(&self, at: usize) -> Result<(), EncodeRefusal> {
    if self.open > 0 || self.interrupted {
      return Err(EncodeRefusal::Malformed {
        at,
        reason: "the stream ends with a part still open",
      });
    }
    Ok(())
  }
}

/// Writes one payload chunk: its length, never 0, then its bytes.
fn put_chunk(out: &mut impl Out, at: usize, chunk: &[u8]) -> Result<(), EncodeRefusal> {
  if chunk.is_empty() {
    return Err(EncodeRefusal::EmptyChunk { at });
  }
  put_length(out, chunk.len(), "payload chunk")?;
  out.put(chunk);
  Ok(())
}

/// A part's header bytes: type, id, the two counts, the sizes, the keys and values.
fn encode_header(part: &Part<'_>) -> Result<Vec<u8>, EncodeRefusal> {
  let kind_len = u8::try_from(part.kind.len())
    .ok()
    .filter(|_| valid_kind(part.kind));
  let kind_len = kind_len.ok_or_else(|| EncodeRefusal::PartType {
    kind: part.kind.to_vec(),
  })?;
  let count = |params: &[Param<'_>]| {
    u8::try_from(params.len()).map_err(|_| EncodeRefusal::ParameterCount {
      kind: part.kind.to_vec(),
    })
  };
  let mandatory =
    part
      .params
      .get(..part.mandatory_params)
      .ok_or_else(|| EncodeRefusal::ParameterCount {
        kind: part.kind.to_vec(),
      })?;
  let mut header = vec![kind_len];
  header.extend_from_slice(part.kind);
  header.extend_from_slice(&part.id.to_be_bytes());
  header.push(count(mandatory)?);
  header.push(count(part.advisory())?);
  for param in &part.params {
    for field in [param.key, param.value] {
      header.push(
        u8::try_from(field.len()).map_err(|_| EncodeRefusal::ParameterCount {
          kind: part.kind.to_vec(),
        })?,
      );
    }
  }
  for param in &part.params {
    header.extend_from_slice(param.key);
    header.extend_from_slice(param.value);
  }
  Ok(header)
}

#[cfg(test)]
#[path = "tests/bundle2.rs"]
mod tests;
