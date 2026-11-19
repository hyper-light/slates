//! HTTP version 1 (A-569; `internals.wireprotocol` [B]; `wireprotoserver.py` `httpv1protocolhandler`,
//! `decodevaluefromheaders`, `_httpresponsetype`, `genversion2`, `_callhttp`, and `httppeer.py`
//! `makev1commandrequest`, `encodevalueinheaders` [C]).
//!
//! A command is the `cmd` field of the request's query. Its arguments are form fields (`urlencode`: `name=value`
//! joined by `&`, a space as `+`, other bytes `%XX`), read as 7.2's server reads them (`_args`): the query's fields,
//! overridden name by name either by the first `X-HgArgs-Post` bytes of the body, when that header is sent and not
//! zero (7.2's client sends it when the server advertises `httppostargs`), or else by the values of the headers
//! `X-HgArg-1`, `X-HgArg-2`, ... joined (7.2's client sends those when the server advertises `httpheader`, each header
//! line at most that long). A named argument is the first value of its field; the dictionary holds every other field
//! but `cmd`, each name once, in the order 7.2's dictionary keeps them: the query's names first, then the others. The
//! client says what it reads in `X-HgProto-1`, `X-HgProto-2`, ... joined: `0.1`, `0.2`, `comp=` and the engines it
//! decodes (`zlib,none` when it names none), `partial-pull`.
//!
//! A string answer is `application/mercurial-0.1`, as it is. A stream is `application/mercurial-0.2` to a client that
//! reads `0.2`: a byte giving the engine's name's length, the name, then the stream as that engine compresses it; and
//! `application/mercurial-0.1` compressed by zlib otherwise (the engines are D-17's; this module frames). An error is
//! `application/hg-error`, its message the body; a push without bundle2 answers `<result>\n<output>`, or
//! `0\n<message>\n` refused.
//!
//! **Stricter than 7.2, on purpose.** 7.2 reads `X-HgArgs-Post` with `int()`; here it is decimal digits alone, and a
//! length past the body is refused (7.2 reads what there is). A field's escapes are read as the bytes they name.
//!
//! **Bounds.** The request itself, and the caller's admission: a decode counts the fields and their decoded bytes
//! first, charges the exact total, then decodes once into the caller's buffer; what it joins or sorts on the way is
//! charged beside that, and freed.

use crate::decimal::Decimal;
use crate::out::Measure;
use crate::quote;
use crate::read::to_u64;
use crate::wire::{Arguments, STAR, Spec};
use crate::{Admission, PastAdmission};

/// Format: the query field that names the command.
pub const COMMAND: &[u8] = b"cmd";

/// Format: the prefix of the numbered headers that carry the arguments: `X-HgArg-1`, `X-HgArg-2`, ...
pub const ARGUMENT_HEADER: &[u8] = b"X-HgArg";

/// Format: the header giving how many bytes of the body's front are the arguments.
pub const POST_ARGUMENTS_HEADER: &[u8] = b"X-HgArgs-Post";

/// Format: the prefix of the numbered headers that carry what the client reads: `X-HgProto-1`, ...
pub const PROTOCOL_HEADER: &[u8] = b"X-HgProto";

/// Format: the media type of a raw answer, and of a zlib stream to a client that does not read `0.2`.
pub const MEDIA_TYPE_V1: &[u8] = b"application/mercurial-0.1";

/// Format: the media type of a stream that names its compression engine.
pub const MEDIA_TYPE_V2: &[u8] = b"application/mercurial-0.2";

/// Format: the media type of an error, its message the body.
pub const MEDIA_TYPE_ERROR: &[u8] = b"application/hg-error";

/// Format: the protocol capability that offers the engines a client decodes.
const COMPRESSION: &[u8] = b"comp=";

/// Format: the engines a client that names none decodes (`clientcompressionsupport`).
const DEFAULT_ENGINES: &[u8] = b"zlib,none";

/// Format: the byte between form fields.
const FIELD_SEPARATOR: u8 = b'&';

/// Format: the byte between a field's name and its value.
const ASSIGN: u8 = b'=';

/// Format: the byte between a numbered header's prefix and its number.
const NUMBER_SEPARATOR: u8 = b'-';

/// Format: the byte between protocol capabilities.
const TOKEN_SEPARATOR: u8 = b' ';

/// Format: the byte between engine names.
const ENGINE_SEPARATOR: u8 = b',';

/// Format: the byte after a push's result.
const NEWLINE: u8 = b'\n';

/// Format: the result a refused push answers (`pusherr`).
const REFUSED: &[u8] = b"0";

/// Format: decimal's base, for `X-HgArgs-Post`.
const RADIX: u64 = 10;

/// Format: the digit zero, from which each digit's value counts.
const ZERO: u8 = b'0';

/// Format: the room `encodevalueinheaders` keeps for a header's number, `-000`.
const NUMBER_ROOM: usize = 4;

/// Format: the bytes a header line holds besides its name and value, `: ` and the line end.
const HEADER_OVERHEAD: usize = 4;

/// A request as this module reads it.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
  /// The query string, after its `?`.
  pub query: &'a [u8],
  /// Every header, name and value, as received.
  pub headers: &'a [(&'a [u8], &'a [u8])],
  /// The body.
  pub body: &'a [u8],
}

impl<'a> Request<'a> {
  /// The value of the header `name`, matched without regard to case; the first if it came twice.
  pub fn header(&self, name: &[u8]) -> Option<&'a [u8]> {
    self
      .headers
      .iter()
      .find(|(known, _)| known.eq_ignore_ascii_case(name))
      .map(|(_, value)| *value)
  }

  /// The values of `<prefix>-1`, `<prefix>-2`, ... in order, up to the first missing (`decodevaluefromheaders`).
  fn numbered(&self, prefix: &'a [u8]) -> impl Iterator<Item = &'a [u8]> + '_ {
    (1_u64..).map_while(move |number| {
      self
        .headers
        .iter()
        .find(|(name, _)| is_numbered(name, prefix, number))
        .map(|(_, value)| *value)
    })
  }
}

/// Whether `name` is `<prefix>-<number>`, the prefix without regard to case.
fn is_numbered(name: &[u8], prefix: &[u8], number: u64) -> bool {
  let digits = Decimal::unsigned(number);
  let dash = prefix.len();
  match (
    name.get(..dash),
    name.get(dash),
    name.get(dash.saturating_add(1)..),
  ) {
    (Some(head), Some(&NUMBER_SEPARATOR), Some(tail)) => {
      head.eq_ignore_ascii_case(prefix) && tail == digits.as_bytes()
    }
    _ => false,
  }
}

/// Why an HTTP request or body was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// An `X-HgArgs-Post` that is not decimal digits, or longer than the body.
  PostArguments,
  /// A named argument the request does not carry (7.2: `KeyError`).
  MissingArgument {
    /// Its position among the spec's named arguments.
    position: u64,
  },
  /// A `0.2` body without a whole engine name.
  MediaHeader,
  /// More than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Why an answer or a request could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// An engine name empty or longer than a byte can count (7.2: `0 < len(name) < 256`).
  EngineName,
  /// A header limit that leaves no room for a value.
  HeaderLimit,
}

/// Where a request's arguments come from, beside its query.
enum Source<'a> {
  /// The body's front.
  Post(&'a [u8]),
  /// The numbered headers.
  Headers,
}

/// Where 7.2 reads `request`'s arguments from: the body's first `X-HgArgs-Post` bytes when it is sent and not zero,
/// else the numbered headers.
fn source<'a>(request: &Request<'a>) -> Result<Source<'a>, Refusal> {
  let declared = match request.header(POST_ARGUMENTS_HEADER) {
    Some(text) => decimal(text).ok_or(Refusal::PostArguments)?,
    None => 0,
  };
  if declared == 0 {
    return Ok(Source::Headers);
  }
  usize::try_from(declared)
    .ok()
    .and_then(|end| request.body.get(..end))
    .map(Source::Post)
    .ok_or(Refusal::PostArguments)
}

/// The value of decimal digits alone, within `u64`.
fn decimal(text: &[u8]) -> Option<u64> {
  if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
    return None;
  }
  text.iter().try_fold(0_u64, |total, byte| {
    total
      .checked_mul(RADIX)?
      .checked_add(u64::from(byte.saturating_sub(ZERO)))
  })
}

/// The body after the arguments `X-HgArgs-Post` puts at its front: `unbundle`'s input.
pub fn payload<'a>(request: &Request<'a>) -> Result<&'a [u8], Refusal> {
  Err(Refusal::PostArguments)
}

/// A request's form fields, decoded: the query's, and the arguments' (the body's front or the numbered headers').
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fields<'b> {
  /// The query's fields, name and value, in order.
  pub query: Vec<(&'b [u8], &'b [u8])>,
  /// The arguments' fields, in order.
  pub arguments: Vec<(&'b [u8], &'b [u8])>,
}

/// A form's fields still quoted, as `parse_qsl` splits them: at `&`, empty ones passed over, each at its first `=`, a
/// field without one an empty value (`keep_blank_values`).
fn raw_fields(text: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
  text
    .split(|byte| *byte == FIELD_SEPARATOR)
    .filter(|field| !field.is_empty())
    .map(
      |field| match field.iter().position(|byte| *byte == ASSIGN) {
        Some(at) => (
          field.get(..at).unwrap_or_default(),
          field.get(at.saturating_add(1)..).unwrap_or_default(),
        ),
        None => (field, &[][..]),
      },
    )
}

/// A form's decoded bytes and its fields.
fn measure_form(text: &[u8]) -> (u64, usize) {
  let mut bytes = Measure(0);
  let mut count: usize = 0;
  for (name, value) in raw_fields(text) {
    quote::unquote_plus(name, &mut bytes);
    quote::unquote_plus(value, &mut bytes);
    count = count.saturating_add(1);
  }
  (bytes.0, count)
}

/// A form's fields over `filled`, from `at`, each name and value as long as its decoding.
fn slice_fields<'b>(
  text: &[u8],
  filled: &'b [u8],
  at: &mut usize,
  count: usize,
) -> Vec<(&'b [u8], &'b [u8])> {
  let mut take = |quoted: &[u8]| {
    let mut length = Measure(0);
    quote::unquote_plus(quoted, &mut length);
    let end = at.saturating_add(usize::try_from(length.0).unwrap_or(usize::MAX));
    let found = filled.get(*at..end).unwrap_or_default();
    *at = end;
    found
  };
  let mut fields = Vec::with_capacity(count);
  for (name, value) in raw_fields(text) {
    let name = take(name);
    fields.push((name, take(value)));
  }
  fields
}

/// A request's form fields, decoded into `buffer`. The arguments' headers are joined first (charged beside the
/// rest, and freed); the fields and their decoded bytes are counted and charged to `admission`, then decoded once.
pub fn fields<'b>(
  request: &Request<'_>,
  admission: &mut Admission,
  buffer: &'b mut Vec<u8>,
) -> Result<Fields<'b>, Refusal> {
  Err(Refusal::PostArguments)
}

/// The value of the first field named `name`.
fn first<'b>(fields: &[(&'b [u8], &'b [u8])], name: &[u8]) -> Option<&'b [u8]> {
  fields
    .iter()
    .find(|(known, _)| *known == name)
    .map(|(_, value)| *value)
}

impl<'b> Fields<'b> {
  /// The command: the query's first `cmd` field.
  pub fn command(&self) -> Option<&'b [u8]> {
  None
  }

  /// The value 7.2 reads for `name` (`knownargs[k][0]`): the arguments' first field of that name, else the query's.
  pub fn value(&self, name: &[u8]) -> Option<&'b [u8]> {
  None
  }

  /// The command's arguments for `spec` (`getargs`): each named one's value ([`Fields::value`]), refused where the
  /// request lacks it; and, where the spec takes one, the dictionary: every other name but `cmd`, once each, at its
  /// first place among the query's fields and then the arguments'. The arguments' vectors are charged to
  /// `admission`, and the sort that finds each name's first place beside them.
  pub fn arguments(
    &self,
    spec: Spec<'_>,
    admission: &mut Admission,
  ) -> Result<Arguments<'b>, Refusal> {
  Err(Refusal::MissingArgument { position: 0 })
  }

  /// Every name but `cmd` and the spec's, once each at its first place, with its value.
  fn dictionary(
    &self,
    spec: Spec<'_>,
    peak: &mut Admission,
    kept: &mut Admission,
  ) -> Result<Vec<(&'b [u8], &'b [u8])>, Refusal> {
    let all = || {
      self
        .query
        .iter()
        .chain(&self.arguments)
        .map(|(name, _)| *name)
    };
    let total = all().count();
    peak.charge_items::<(&[u8], usize)>(total)?;
    peak.charge_items::<bool>(total)?;
    let mut sorted: Vec<(&[u8], usize)> = all().enumerate().map(|(at, name)| (name, at)).collect();
    sorted.sort();
    let mut earliest = vec![false; total];
    let mut previous: Option<&[u8]> = None;
    for (name, at) in sorted {
      if previous != Some(name) {
        if let Some(mark) = earliest.get_mut(at) {
          *mark = true;
        }
      }
      previous = Some(name);
    }
    let wanted = |name: &[u8]| name != COMMAND && name != STAR && !spec.has(name);
    let count = all()
      .zip(&earliest)
      .filter(|(name, first_place)| **first_place && wanted(name))
      .count();
    peak.charge_items::<(&[u8], &[u8])>(count)?;
    kept.charge_items::<(&[u8], &[u8])>(count)?;
    Ok(
      all()
        .zip(&earliest)
        .filter(|(name, first_place)| **first_place && wanted(name))
        .map(|(name, _)| (name, self.value(name).unwrap_or_default()))
        .collect(),
    )
  }
}

/// What a client reads: its `X-HgProto` headers joined (`getprotocaps`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolCaps {
  /// The joined value.
  text: Vec<u8>,
}

/// What `request`'s client reads, its headers joined, their length charged to `admission` first.
pub fn protocol_caps(
  request: &Request<'_>,
  admission: &mut Admission,
) -> Result<ProtocolCaps, PastAdmission> {
  Ok(ProtocolCaps { text: Vec::new() })
}

impl ProtocolCaps {
  /// Each capability, in order.
  pub fn tokens(&self) -> impl Iterator<Item = &[u8]> {
    self
      .text
      .split(|byte| *byte == TOKEN_SEPARATOR)
      .filter(|token| !token.is_empty())
  }

  /// Whether the client sent `token` (`0.1`, `0.2`, `partial-pull`).
  pub fn has(&self, token: &[u8]) -> bool {
    self.tokens().any(|known| known == token)
  }

  /// The engines the client decodes: its first `comp=` list, or `zlib` and `none` when it names none
  /// (`clientcompressionsupport`).
  pub fn engines(&self) -> impl Iterator<Item = &[u8]> {
    self
      .tokens()
      .find_map(|token| token.strip_prefix(COMPRESSION))
      .unwrap_or(DEFAULT_ENGINES)
      .split(|byte| *byte == ENGINE_SEPARATOR)
  }
}

/// Appends the header of an `application/mercurial-0.2` body (`genversion2`): the engine name's length in one byte,
/// then the name.
pub fn encode_v2_header(engine: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  Err(EncodeRefusal::EngineName)
}

/// The engine an `application/mercurial-0.2` body names, and the payload after it.
pub fn decode_v2(body: &[u8]) -> Result<(&[u8], &[u8]), Refusal> {
  Err(Refusal::MediaHeader)
}

/// Appends the body of a push answered without bundle2 (`pushres`): the result, a newline, the output.
pub fn encode_push_result(result: i64, output: &[u8], out: &mut Vec<u8>) {
  
}

/// Appends the body of a push refused without bundle2 (`pusherr`): `0`, a newline, the message, a newline.
pub fn encode_push_error(message: &[u8], out: &mut Vec<u8>) {
  
}

/// Appends `fields` as `urlencode` writes them, in the order given: `name=value` joined by `&`, each quoted with a
/// space as `+`.
pub fn encode_form(fields: &[(&[u8], &[u8])], out: &mut Vec<u8>) {
  
}

/// The values `encodevalueinheaders` splits `value` into for headers named `<prefix>-<N>`, each header line (name,
/// `: `, value, line end, its number given room for three digits) at most `limit` bytes.
pub fn header_values<'v>(
  value: &'v [u8],
  prefix: &[u8],
  limit: usize,
) -> Result<core::slice::Chunks<'v, u8>, EncodeRefusal> {
  Err(EncodeRefusal::HeaderLimit)
}

#[cfg(test)]
mod tests {
  use super::{
    ARGUMENT_HEADER, EncodeRefusal, Fields, Refusal, Request, decode_v2, encode_form,
    encode_push_error, encode_push_result, encode_v2_header, fields, header_values, payload,
    protocol_caps,
  };
  use crate::wire::{Arguments, Spec};
  use crate::{Admission, PastAdmission};
  use proptest::prelude::*;

  fn room() -> Admission {
    Admission::new(1 << 20)
  }

  /// A request with `query`, `headers` and `body`, its fields decoded.
  fn decode<'b>(
    query: &[u8],
    headers: &[(&[u8], &[u8])],
    body: &[u8],
    buffer: &'b mut Vec<u8>,
  ) -> Result<Fields<'b>, Refusal> {
    fields(
      &Request {
        query,
        headers,
        body,
      },
      &mut room(),
      buffer,
    )
  }

  /// A-569: do read a request as 7.2's client sends `debugwireargs` to a server advertising `httpheader`: `cmd` in
  /// the query, the arguments form-encoded across two `X-hgarg-<N>` headers (urllib capitalizes the names), split
  /// mid-field. Expect the command, the named arguments, and the dictionary's entries decoded.
  #[test]
  fn arguments_read_from_numbered_headers_split_anywhere() {
    let headers: [(&[u8], &[u8]); 2] = [
      (b"X-hgarg-2", b"wo=deux"),
      (b"X-hgarg-1", b"four=caf%C3%A9&one=un&t"),
    ];
    let mut buffer = Vec::new();
    let read = decode(b"cmd=debugwireargs", &headers, b"", &mut buffer).unwrap();
    assert_eq!(read.command(), Some(&b"debugwireargs"[..]));
    let spec = Spec(b"one two *");
    let expected = Arguments {
      named: vec![&b"un"[..], &b"deux"[..]],
      dictionary: vec![(&b"four"[..], &b"caf\xc3\xa9"[..])],
    };
    assert_eq!(read.arguments(spec, &mut room()), Ok(expected));
  }

  /// A-569: do read arguments 7.2's client puts at the body's front (`X-HgArgs-Post`), with `unbundle`'s input after
  /// them, and arguments in the query alone. Expect the post arguments read over the headers', the input after them,
  /// and the query's fields read where nothing else carries arguments.
  #[test]
  fn arguments_read_from_the_body_or_the_query() {
    let headers: [(&[u8], &[u8]); 2] = [(b"x-hgargs-post", b"10"), (b"X-HgArg-1", b"heads=no")];
    let body = b"heads=abcdHG20...";
    let request = Request {
      query: b"cmd=unbundle",
      headers: &headers,
      body,
    };
    let mut buffer = Vec::new();
    let read = fields(&request, &mut room(), &mut buffer).unwrap();
    assert_eq!(read.value(b"heads"), Some(&b"abcd"[..]));
    assert_eq!(payload(&request), Ok(&b"HG20..."[..]));
    let mut query_buffer = Vec::new();
    let query = decode(b"cmd=lookup&key=a+b%2B", &[], b"", &mut query_buffer).unwrap();
    assert_eq!(query.value(b"key"), Some(&b"a b+"[..]));
  }

  /// A-569: do give a name in the query and in the headers, twice in each, and a spec that names neither. Expect the
  /// headers' first value read, and the dictionary holding each name once at its first place, `cmd` left out, as
  /// 7.2's dictionary keeps them.
  #[test]
  fn names_in_the_query_and_the_headers_read_as_mercurials_dictionary() {
    let headers: [(&[u8], &[u8]); 1] = [(b"X-HgArg-1", b"b=2&a=3&b=4&c=5")];
    let mut buffer = Vec::new();
    let read = decode(b"a=1&cmd=x&a=9", &headers, b"", &mut buffer).unwrap();
    let expected: [(&[u8], &[u8]); 3] = [(b"a", b"3"), (b"b", b"2"), (b"c", b"5")];
    let arguments = read.arguments(Spec(b"*"), &mut room()).unwrap();
    assert_eq!(arguments.dictionary, expected);
  }

  /// AC-HG.9 (hostile input), A-569: do declare post arguments longer than the body, a post length that is not
  /// digits, a named argument the request lacks, and fields past the admission. Expect `PostArguments`,
  /// `MissingArgument` at its position, and `PastAdmission` with nothing charged.
  #[test]
  fn requests_mercurial_would_misread_are_refused() {
    let long: [(&[u8], &[u8]); 1] = [(b"X-HgArgs-Post", b"11")];
    let signed: [(&[u8], &[u8]); 1] = [(b"X-HgArgs-Post", b"+1")];
    let read =
      |headers: &[(&[u8], &[u8])]| decode(b"", headers, b"0123456789", &mut Vec::new()).map(|_| ());
    assert_eq!(read(&long), Err(Refusal::PostArguments));
    assert_eq!(read(&signed), Err(Refusal::PostArguments));
    let mut buffer = Vec::new();
    let only_one = decode(b"cmd=x&one=1", &[], b"", &mut buffer).unwrap();
    assert_eq!(
      only_one.arguments(Spec(b"one two"), &mut room()),
      Err(Refusal::MissingArgument { position: 1 })
    );
    let mut small = Admission::new(3);
    let request = Request {
      query: b"cmd=heads",
      headers: &[],
      body: b"",
    };
    assert_eq!(
      fields(&request, &mut small, &mut Vec::new()).map(|_| ()),
      Err(Refusal::PastAdmission(PastAdmission { asked: 8, left: 3 }))
    );
    assert_eq!(small.left(), 3);
  }

  /// A-569: do read what a client says it reads across two `X-HgProto` headers, and a client that names no engine.
  /// Expect its capabilities, its engines in its order, and `zlib` and `none` for the other.
  #[test]
  fn protocol_capabilities_name_media_types_and_engines() {
    let headers: [(&[u8], &[u8]); 2] = [
      (b"X-HgProto-1", b"0.1 0.2 comp=zstd,"),
      (b"X-HgProto-2", b"zlib,none partial-pull"),
    ];
    let request = Request {
      query: b"",
      headers: &headers,
      body: b"",
    };
    let caps = protocol_caps(&request, &mut room()).unwrap();
    assert!(caps.has(b"0.2") && caps.has(b"partial-pull") && !caps.has(b"0.3"));
    assert_eq!(
      caps.engines().collect::<Vec<_>>(),
      [&b"zstd"[..], b"zlib", b"none"]
    );
    let bare = protocol_caps(
      &Request {
        query: b"",
        headers: &[],
        body: b"",
      },
      &mut room(),
    )
    .unwrap();
    assert_eq!(bare.engines().collect::<Vec<_>>(), [&b"zlib"[..], b"none"]);
  }

  /// AC-HG.9 (hostile input), A-569: do frame a `0.2` body and read it back, and read bodies with no header, an empty
  /// name and a name longer than the body. Expect the engine and payload back, each broken body refused, and an empty
  /// or overlong engine name not encoded.
  #[test]
  fn version_2_bodies_name_their_engine() {
    let mut body = Vec::new();
    encode_v2_header(b"zstd", &mut body).unwrap();
    body.extend_from_slice(b"payload");
    assert_eq!(body[..5], *b"\x04zstd");
    assert_eq!(decode_v2(&body), Ok((&b"zstd"[..], &b"payload"[..])));
    for broken in [&b""[..], b"\x00none", b"\x09zstd"] {
      assert_eq!(decode_v2(broken), Err(Refusal::MediaHeader));
    }
    assert_eq!(
      encode_v2_header(b"", &mut Vec::new()),
      Err(EncodeRefusal::EngineName)
    );
    assert_eq!(
      encode_v2_header(&[b'z'; 256], &mut Vec::new()),
      Err(EncodeRefusal::EngineName)
    );
  }

  /// A-569: do write a push's answers without bundle2 and split arguments into headers at 7.2's default limit. Expect
  /// `<result>\n<output>` and `0\n<message>\n`, and values of 1009 bytes, `X-HgArg-NNN: ` and the line end taking the
  /// rest of 1024.
  #[test]
  fn push_answers_and_header_splits_are_mercurials() {
    let mut pushed = Vec::new();
    encode_push_result(1, b"added 1 changesets", &mut pushed);
    assert_eq!(pushed, b"1\nadded 1 changesets");
    let mut refused = Vec::new();
    encode_push_error(b"push failed", &mut refused);
    assert_eq!(refused, b"0\npush failed\n");
    let value = vec![b'a'; 2020];
    let sizes: Vec<usize> = header_values(&value, ARGUMENT_HEADER, 1024)
      .unwrap()
      .map(<[u8]>::len)
      .collect();
    assert_eq!(sizes, [1009, 1009, 2]);
    assert_eq!(
      header_values(&value, ARGUMENT_HEADER, 15).err(),
      Some(EncodeRefusal::HeaderLimit)
    );
  }

  /// Generated fields: lower-case names, any values.
  fn arbitrary_fields() -> impl Strategy<Value = Vec<(String, Vec<u8>)>> {
    proptest::collection::btree_map(
      "[a-z]{1,6}",
      proptest::collection::vec(any::<u8>(), 0..12),
      0..6,
    )
    .prop_map(|map| map.into_iter().collect())
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-569: do form-encode generated fields as 7.2's client does, split them across headers at a generated limit,
    /// and read them back. Expect every field back, decoded, in order.
    #[test]
    fn generated_fields_read_back_from_headers(generated in arbitrary_fields(), limit in 16_usize..64) {
      let pairs: Vec<(&[u8], &[u8])> = generated.iter().map(|(name, value)| (name.as_bytes(), value.as_slice())).collect();
      let mut encoded = Vec::new();
      encode_form(&pairs, &mut encoded);
      let values: Vec<&[u8]> = header_values(&encoded, ARGUMENT_HEADER, limit).unwrap().collect();
      let names: Vec<String> = (1..=values.len()).map(|number| format!("X-HgArg-{number}")).collect();
      let headers: Vec<(&[u8], &[u8])> = names.iter().map(String::as_bytes).zip(values).collect();
      let mut buffer = Vec::new();
      let read = decode(b"cmd=x", &headers, b"", &mut buffer).unwrap();
      prop_assert_eq!(read.arguments, pairs);
    }

    /// AC-HG.9, A-569: do read arbitrary bytes as a query, a header's value and a body behind a post length. Expect
    /// fields or a refusal; never a panic.
    #[test]
    fn arbitrary_requests_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..96), post in 0_u8..100) {
      let length = post.to_string();
      let headers: [(&[u8], &[u8]); 2] = [(b"X-HgArgs-Post", length.as_bytes()), (b"X-HgArg-1", &bytes)];
      let mut buffer = Vec::new();
      let request = Request { query: &bytes, headers: &headers, body: &bytes };
      if let Ok(read) = fields(&request, &mut room(), &mut buffer) {
        prop_assert!(read.arguments(Spec(b"one *"), &mut room()).is_ok() || read.value(b"one").is_none());
      }
      if let Ok(rest) = payload(&request) {
        prop_assert!(rest.len() <= bytes.len());
      }
      prop_assert!(decode_v2(&bytes).is_ok() || bytes.first().is_none_or(|length| usize::from(*length) == 0 || usize::from(*length) >= bytes.len()));
    }
  }
}
