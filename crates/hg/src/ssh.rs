//! SSH version 1, the framing of `hg serve --stdio` (A-569; `internals.wireprotocol` [B]; `wireprotoserver.py`
//! `_runsshserver`, `sshv1protocolhandler`, `_sshv1respondbytes`, `_sshv1respondstream`, `_sshv1respondooberror`,
//! and `sshpeer.py` `_sendrequest` [C]).
//!
//! The client writes a command as its name and a newline, then each argument its spec names as `<name>
//! <length>\n<value>`, and the dictionary (`*`) as `* <count>\n` and that many `<key> <length>\n<value>`. 7.2's client
//! writes them sorted by name, so the dictionary first; the server reads them in any order. An empty line ends the
//! session. `unbundle`'s input follows its arguments, once the server has answered with an empty string, as chunks
//! `<length>\n<data>` ending with `0\n`. The server answers a string as `<length>\n<value>`, a stream as raw bytes (a
//! bundle2 stream ends itself), an error with an empty line on stdout and the message followed by `\n-\n` on stderr,
//! and a push without bundle2 with two strings, an empty one and the result.
//!
//! **Stricter than 7.2, on purpose.** 7.2 reads a length with Python's `int()`, which takes a sign, spaces and `_`,
//! and then `read(n)`, which for a negative `n` reads to the end of the input; a length here is decimal digits
//! alone. 7.2 splits an argument line at any run of whitespace; here it is a name without whitespace, one space and the
//! length. An argument sent twice is refused (7.2 keeps the last, then fails on the one missing).
//!
//! **Bounds, none of them a literal.** A command line is bounded by the longest name among the commands the server
//! serves, an argument line by its spec's longest name and the digits of the caller's room, every length and count by
//! that room: the bytes its admission says the caller may still hold. A read takes what is buffered and says how
//! many bytes it needs when that is not yet all ([`Read::More`]); nothing is allocated but the arguments' vectors and a
//! mark per spec name, counted first and charged to the admission.

use crate::decimal::Decimal;
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::wire::{Arguments, STAR, Spec};
use crate::{Admission, PastAdmission};

/// Format: the byte that ends a command line, an argument line and a length.
const NEWLINE: u8 = b'\n';

/// Format: the byte between an argument's name and its length.
const SPACE: u8 = b' ';

/// Format: the bytes Python's `bytes.split()` splits at, none of which a name or a key holds.
const WHITESPACE: &[u8] = b" \t\n\r\x0b\x0c";

/// Format: what follows an error's message on stderr (`_sshv1respondooberror`).
const ERROR_END: &[u8] = b"\n-\n";

/// Format: decimal's base.
const RADIX: u64 = 10;

/// Format: the digit zero, from which each digit's value counts.
const ZERO: u8 = b'0';

/// Format: the fewest bytes a dictionary entry takes: a one-byte key, a space, one digit and a newline.
const SMALLEST_ENTRY: u64 = 4;

/// What reading the buffered bytes found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read<T> {
  /// The item, and how many bytes from the buffer's front it took.
  Done(T, usize),
  /// The buffer holds only part of the item: it must hold at least `needed` bytes from its front first.
  More {
    /// The bytes the buffer must hold.
    needed: usize,
  },
}

/// Why SSH input was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// No newline within the longest line the reader takes.
  LineTooLong {
    /// What the line was.
    what: &'static str,
    /// The longest it may be, its newline left out.
    limit: u64,
  },
  /// An argument line that is not a name without whitespace, one space and a length.
  ArgumentLine {
    /// Where the line begins.
    at: u64,
  },
  /// An argument the command's spec does not name (7.2: "unexpected parameter").
  UnexpectedArgument {
    /// Where its line begins.
    at: u64,
  },
  /// An argument sent twice.
  RepeatedArgument {
    /// Where its second line begins.
    at: u64,
  },
  /// A length or a count that is not decimal digits, or past `u64`.
  NotALength {
    /// What it was.
    what: &'static str,
    /// Where its line begins.
    at: u64,
  },
  /// A length or a count past what the caller may hold.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// A command line: `Done(name, consumed)`, the name empty for the empty line that ends the session. `longest` is the
/// longest name among the commands the server serves; a line with no newline within it is refused, since no command
/// it serves is that long.
pub fn command(input: &[u8], longest: usize) -> Result<Read<&[u8]>, Refusal> {
  line(input, longest, "command line")
}

/// A command's arguments for `spec`: each name the spec holds, once, in any order, and the dictionary's entries. The
/// values are read in place. Every length and count is held to the room `admission` had when the read began; the
/// arguments' vectors are charged to it once the read is whole (a read that asks for more charges nothing), and the
/// marks of the names seen, which the read frees, must fit beside them.
pub fn arguments<'a>(
  input: &'a [u8],
  spec: Spec<'_>,
  admission: &mut Admission,
) -> Result<Read<Arguments<'a>>, Refusal> {
  let room = admission.left();
  let names = spec.names().count();
  let mut peak = *admission;
  peak.charge_items::<bool>(names)?;
  let mut seen = vec![false; names];
  let mut count = Count::default();
  let consumed = match walk(input, spec, room, &mut seen, &mut count)? {
    Read::Done((), consumed) => consumed,
    Read::More { needed } => return Ok(Read::More { needed }),
  };
  let named = spec.named().count();
  let mut kept = *admission;
  for charged in [&mut peak, &mut kept] {
    charged.charge_items::<&[u8]>(named)?;
    charged.charge_items::<(&[u8], &[u8])>(count.entries)?;
  }
  let mut build = Build {
    named: vec![&[][..]; named],
    dictionary: Vec::with_capacity(count.entries),
  };
  walk(input, spec, room, &mut seen, &mut build)?;
  *admission = kept;
  Ok(Read::Done(
    Arguments {
      named: build.named,
      dictionary: build.dictionary,
    },
    consumed,
  ))
}

/// One chunk of `unbundle`'s input, `<length>\n<data>`: `Done(Some(data), consumed)`, or `Done(None, consumed)` for
/// the `0\n` that ends the input. A length past what `admission` leaves the caller is refused.
pub fn chunk<'a>(input: &'a [u8], admission: &Admission) -> Result<Read<Option<&'a [u8]>>, Refusal> {
  let room = admission.left();
  let (size, used) = match length_line(input, room, "chunk length")? {
    Read::Done(size, used) => (size, used),
    Read::More { needed } => return Ok(Read::More { needed }),
  };
  if size == 0 {
    return Ok(Read::Done(None, used));
  }
  Ok(match value(input, used, size, room)? {
    Read::Done(data, end) => Read::Done(Some(data), end),
    Read::More { needed } => Read::More { needed },
  })
}

/// A string answer, `<length>\n<value>`, as a client reads one: `Done(value, consumed)`. A length past what
/// `admission` leaves the caller is refused.
pub fn string<'a>(input: &'a [u8], admission: &Admission) -> Result<Read<&'a [u8]>, Refusal> {
  let room = admission.left();
  let (size, used) = match length_line(input, room, "answer length")? {
    Read::Done(size, used) => (size, used),
    Read::More { needed } => return Ok(Read::More { needed }),
  };
  value(input, used, size, room)
}

/// Appends a string answer (`_sshv1respondbytes`): the value's length in decimal, a newline, the value.
pub fn respond(value: &[u8], out: &mut Vec<u8>) {
  put_string(value, out);
}

/// The length of [`respond`]'s answer to `value`, for a caller that charges it first.
pub fn respond_len(value: &[u8]) -> u64 {
  let mut measure = Measure(0);
  put_string(value, &mut measure);
  measure.0
}

/// Appends the error answer (`_sshv1respondooberror`): an empty line on stdout, and on stderr the message followed by
/// `\n-\n`.
pub fn respond_error(message: &[u8], stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) {
  stdout.push(NEWLINE);
  stderr.extend_from_slice(message);
  stderr.extend_from_slice(ERROR_END);
}

/// Appends the answer to a push without bundle2 (`pushres`): an empty string, then the result as a string.
pub fn respond_push(result: i64, out: &mut Vec<u8>) {
  put_string(&[], out);
  put_string(Decimal::signed(result).as_bytes(), out);
}

/// Appends a request as 7.2's client writes it (`_sendrequest`): the command line, then the named arguments and the
/// dictionary as one list sorted by name (`*` among them), each named value with its length, the dictionary with its
/// count and then its entries in their order.
pub fn encode_request(
  command: &[u8],
  named: &[(&[u8], &[u8])],
  dictionary: Option<&[(&[u8], &[u8])]>,
  out: &mut Vec<u8>,
) {
  out.extend_from_slice(command);
  out.push(NEWLINE);
  let mut items: Vec<(&[u8], Option<&[u8]>)> = named
    .iter()
    .map(|(name, value)| (*name, Some(*value)))
    .collect();
  if dictionary.is_some() {
    items.push((STAR, None));
  }
  items.sort_by(|left, right| left.0.cmp(right.0));
  for (name, value) in items {
    match value {
      Some(value) => put_argument(name, value, out),
      None => {
        let entries = dictionary.unwrap_or_default();
        put_header(name, to_u64(entries.len()), out);
        for (key, entry) in entries {
          put_argument(key, entry, out);
        }
      }
    }
  }
}

/// Writes one argument: its name, a space, its length, a newline, its value.
fn put_argument(name: &[u8], value: &[u8], out: &mut Vec<u8>) {
  put_header(name, to_u64(value.len()), out);
  out.extend_from_slice(value);
}

/// Writes an argument line: the name, a space, the number, a newline.
fn put_header(name: &[u8], number: u64, out: &mut Vec<u8>) {
  out.extend_from_slice(name);
  out.push(SPACE);
  out.extend_from_slice(Decimal::unsigned(number).as_bytes());
  out.push(NEWLINE);
}

/// Writes a string: its length, a newline, its bytes.
fn put_string(value: &[u8], out: &mut impl Out) {
  out.put(Decimal::unsigned(to_u64(value.len())).as_bytes());
  out.put(&[NEWLINE]);
  out.put(value);
}

/// The line at the front of `input`, its newline left out, if a newline comes within `limit` bytes of it.
fn line<'a>(input: &'a [u8], limit: usize, what: &'static str) -> Result<Read<&'a [u8]>, Refusal> {
  let window = input.get(..limit.saturating_add(1)).unwrap_or(input);
  match window.iter().position(|byte| *byte == NEWLINE) {
    Some(end) => Ok(Read::Done(
      input.get(..end).unwrap_or_default(),
      end.saturating_add(1),
    )),
    None if input.len() > limit => Err(Refusal::LineTooLong {
      what,
      limit: to_u64(limit),
    }),
    None => Ok(Read::More {
      needed: input.len().saturating_add(1),
    }),
  }
}

/// How many decimal digits `value` has: the longest length an admission of `value` bytes leaves room for.
fn digits(value: u64) -> usize {
  Decimal::unsigned(value).as_bytes().len()
}

/// A line holding a length alone, within the digits `room` has.
fn length_line(input: &[u8], room: u64, what: &'static str) -> Result<Read<u64>, Refusal> {
  Ok(match line(input, digits(room), what)? {
    Read::Done(text, used) => Read::Done(length(text, what, 0)?, used),
    Read::More { needed } => Read::More { needed },
  })
}

/// The decimal value of `text`: digits alone, within `u64`.
fn length(text: &[u8], what: &'static str, at: usize) -> Result<u64, Refusal> {
  let refusal = Refusal::NotALength {
    what,
    at: to_u64(at),
  };
  if text.is_empty() {
    return Err(refusal);
  }
  text.iter().try_fold(0_u64, |total, byte| {
    if !byte.is_ascii_digit() {
      return Err(refusal);
    }
    let digit = u64::from(byte.saturating_sub(ZERO));
    total
      .checked_mul(RADIX)
      .and_then(|total| total.checked_add(digit))
      .ok_or(refusal)
  })
}

/// The `size` bytes at `at`, refused past `room`.
fn value(input: &[u8], at: usize, size: u64, room: u64) -> Result<Read<&[u8]>, Refusal> {
  let past = PastAdmission {
    asked: size,
    left: room,
  };
  if size > room {
    return Err(past.into());
  }
  let end = usize::try_from(size)
    .ok()
    .and_then(|size| at.checked_add(size))
    .ok_or(past)?;
  Ok(match input.get(at..end) {
    Some(found) => Read::Done(found, end),
    None => Read::More { needed: end },
  })
}

/// An argument or entry line at `at`: a name without whitespace, one space, a length, a newline. Answers the name, the
/// number and where the line ends.
fn header<'a>(
  input: &'a [u8],
  at: usize,
  limit: usize,
  what: &'static str,
) -> Result<Read<(&'a [u8], u64)>, Refusal> {
  let rest = input.get(at..).unwrap_or_default();
  let (text, used) = match line(rest, limit, what)? {
    Read::Done(text, used) => (text, used),
    Read::More { needed } => {
      return Ok(Read::More {
        needed: at.saturating_add(needed),
      });
    }
  };
  let refusal = Refusal::ArgumentLine { at: to_u64(at) };
  let space = text
    .iter()
    .position(|byte| *byte == SPACE)
    .ok_or(refusal)?;
  let name = text.get(..space).unwrap_or_default();
  if name.is_empty() || name.iter().any(|byte| WHITESPACE.contains(byte)) {
    return Err(refusal);
  }
  let number = length(text.get(space.saturating_add(1)..).unwrap_or_default(), what, at)?;
  Ok(Read::Done((name, number), at.saturating_add(used)))
}

/// Where a walk of a command's arguments puts what it reads: the counting pass and the building pass.
trait Sink<'a> {
  /// A named argument's value, at its position among the spec's named arguments.
  fn named(&mut self, position: usize, value: &'a [u8]);
  /// A dictionary entry.
  fn entry(&mut self, key: &'a [u8], value: &'a [u8]);
}

/// The counting pass: how many dictionary entries came.
#[derive(Default)]
struct Count {
  /// The entries.
  entries: usize,
}

impl<'a> Sink<'a> for Count {
  fn named(&mut self, _position: usize, _value: &'a [u8]) {}

  fn entry(&mut self, _key: &'a [u8], _value: &'a [u8]) {
    self.entries = self.entries.saturating_add(1);
  }
}

/// The building pass, into vectors the counting pass sized.
struct Build<'a> {
  /// The named arguments, in the spec's order.
  named: Vec<&'a [u8]>,
  /// The dictionary's entries.
  dictionary: Vec<(&'a [u8], &'a [u8])>,
}

impl<'a> Sink<'a> for Build<'a> {
  fn named(&mut self, position: usize, value: &'a [u8]) {
    if let Some(slot) = self.named.get_mut(position) {
      *slot = value;
    }
  }

  fn entry(&mut self, key: &'a [u8], value: &'a [u8]) {
    self.dictionary.push((key, value));
  }
}

/// Walks one argument line per spec name, each with its value or its entries, into `sink`.
fn walk<'a>(
  input: &'a [u8],
  spec: Spec<'_>,
  room: u64,
  seen: &mut [bool],
  sink: &mut impl Sink<'a>,
) -> Result<Read<()>, Refusal> {
  seen.fill(false);
  let limit = spec
    .longest_name()
    .saturating_add(1)
    .saturating_add(digits(room));
  let mut at: usize = 0;
  for _ in 0..seen.len() {
    match argument(input, at, spec, Bounds { room, limit }, seen, sink)? {
      Read::Done((), next) => at = next,
      Read::More { needed } => return Ok(Read::More { needed }),
    }
  }
  Ok(Read::Done((), at))
}

/// What an argument is held to: the caller's room, and the longest argument line.
#[derive(Clone, Copy)]
struct Bounds {
  /// The bytes the caller may still hold.
  room: u64,
  /// The longest argument line, its newline left out.
  limit: usize,
}

/// One argument at `at`: its line, marked seen, then its value or the dictionary's entries.
fn argument<'a>(
  input: &'a [u8],
  at: usize,
  spec: Spec<'_>,
  bounds: Bounds,
  seen: &mut [bool],
  sink: &mut impl Sink<'a>,
) -> Result<Read<()>, Refusal> {
  let ((name, number), after) = match header(input, at, bounds.limit, "argument line")? {
    Read::Done(found, after) => (found, after),
    Read::More { needed } => return Ok(Read::More { needed }),
  };
  let unexpected = Refusal::UnexpectedArgument { at: to_u64(at) };
  let mark = spec
    .names()
    .position(|known| known == name)
    .and_then(|position| seen.get_mut(position))
    .ok_or(unexpected)?;
  if *mark {
    return Err(Refusal::RepeatedArgument { at: to_u64(at) });
  }
  *mark = true;
  if name == STAR {
    return dictionary(input, after, number, bounds.room, sink);
  }
  let position = spec.position(name).ok_or(unexpected)?;
  Ok(match value(input, after, number, bounds.room)? {
    Read::Done(found, end) => {
      sink.named(position, found);
      Read::Done((), end)
    }
    Read::More { needed } => Read::More { needed },
  })
}

/// The dictionary's `count` entries at `at`, each `<key> <length>\n<value>`; a count more entries than `room` could
/// hold is refused before any is read.
fn dictionary<'a>(
  input: &'a [u8],
  at: usize,
  count: u64,
  room: u64,
  sink: &mut impl Sink<'a>,
) -> Result<Read<()>, Refusal> {
  let most = room / SMALLEST_ENTRY;
  if count > most {
    return Err(
      PastAdmission {
        asked: count,
        left: most,
      }
      .into(),
    );
  }
  let limit = usize::try_from(room).unwrap_or(usize::MAX);
  let mut next = at;
  for _ in 0..count {
    let ((key, size), after) = match header(input, next, limit, "dictionary entry")? {
      Read::Done(found, after) => (found, after),
      Read::More { needed } => return Ok(Read::More { needed }),
    };
    match value(input, after, size, room)? {
      Read::Done(found, end) => {
        sink.entry(key, found);
        next = end;
      }
      Read::More { needed } => return Ok(Read::More { needed }),
    }
  }
  Ok(Read::Done((), next))
}

#[cfg(test)]
mod tests {
  use super::{
    Read, Refusal, arguments, chunk, command, encode_request, respond, respond_error, respond_len,
    respond_push, string,
  };
  use crate::wire::{Arguments, Spec};
  use crate::{Admission, PastAdmission};
  use proptest::prelude::*;

  fn room() -> Admission {
    Admission::new(1 << 20)
  }

  /// The request 7.2's client writes for `debugwireargs un deux --three trois --four ''`.
  fn wireargs_request() -> Vec<u8> {
    let mut out = Vec::new();
    let dictionary: [(&[u8], &[u8]); 2] = [(b"three", b"trois"), (b"four", b"")];
    encode_request(
      b"debugwireargs",
      &[(b"one", b"un"), (b"two", b"deux")],
      Some(&dictionary),
      &mut out,
    );
    out
  }

  /// A-569: do write the request 7.2's client writes for `debugwireargs` and read its command and arguments back.
  /// Expect the dictionary first on the wire (7.2 sorts by name, and `*` sorts first), the named values in the spec's
  /// order, the entries in theirs, and every byte read.
  #[test]
  fn a_request_reads_back_as_its_client_wrote_it() {
    let request = wireargs_request();
    assert!(request.starts_with(b"debugwireargs\n* 2\nthree 5\ntrois"));
    let Ok(Read::Done(name, used)) = command(&request, 64) else {
      panic!("a command line")
    };
    assert_eq!(name, b"debugwireargs");
    let rest = &request[used..];
    let expected = Arguments {
      named: vec![&b"un"[..], &b"deux"[..]],
      dictionary: vec![(&b"three"[..], &b"trois"[..]), (&b"four"[..], &b""[..])],
    };
    assert_eq!(
      arguments(rest, Spec(b"one two *"), &mut room()),
      Ok(Read::Done(expected, rest.len()))
    );
  }

  /// A-569: do read a request cut at every length short of its own. Expect each cut to ask for more, never to read
  /// short, and the bytes it asks for never more than the request's own length.
  #[test]
  fn a_request_cut_anywhere_asks_for_more() {
    let request = wireargs_request();
    let rest = &request[b"debugwireargs\n".len()..];
    for cut in 0..rest.len() {
      match arguments(&rest[..cut], Spec(b"one two *"), &mut room()) {
        Ok(Read::More { needed }) => assert!(needed > cut && needed <= rest.len(), "cut at {cut}"),
        other => panic!("cut at {cut}: {other:?}"),
      }
    }
  }

  /// AC-HG.9 (hostile input), A-569: do read a command line with no newline within the longest name, an empty line,
  /// and one still arriving. Expect `LineTooLong`, the empty name that ends the session, and a request for one byte
  /// more than is held.
  #[test]
  fn command_lines_are_held_to_the_longest_name() {
    assert_eq!(
      command(b"getbundlexx", 9),
      Err(Refusal::LineTooLong {
        what: "command line",
        limit: 9
      })
    );
    assert_eq!(command(b"\nheads\n", 9), Ok(Read::Done(&b""[..], 1)));
    assert_eq!(command(b"head", 9), Ok(Read::More { needed: 5 }));
  }

  /// AC-HG.9 (hostile input), A-569: do send an argument the spec does not name, one twice, a line without a space, a
  /// name with a tab, and a length that is not digits. Expect each refused by its kind at its line.
  #[test]
  fn arguments_mercurial_would_misread_are_refused() {
    let spec = Spec(b"namespace key");
    let read = |bytes: &[u8]| arguments(bytes, spec, &mut room()).map(|_| ());
    assert_eq!(
      read(b"other 1\nx"),
      Err(Refusal::UnexpectedArgument { at: 0 })
    );
    assert_eq!(
      read(b"key 1\nxkey 1\ny"),
      Err(Refusal::RepeatedArgument { at: 7 })
    );
    assert_eq!(read(b"key1\nx"), Err(Refusal::ArgumentLine { at: 0 }));
    assert_eq!(read(b"k\tey 1\nx"), Err(Refusal::ArgumentLine { at: 0 }));
    assert_eq!(
      read(b"key +1\nx"),
      Err(Refusal::NotALength {
        what: "argument line",
        at: 0
      })
    );
  }

  /// AC-HG.9 (hostile input), A-569: do declare a value past the caller's room, a length of `u64::MAX`, a dictionary
  /// of more entries than the room holds, and a length with more digits than the room's. Expect `PastAdmission` before
  /// a byte of the value is asked for, and `LineTooLong` for the lines longer than a length within the room can make.
  #[test]
  fn lengths_and_counts_past_the_room_are_refused() {
    let small = || Admission::new(100);
    assert_eq!(
      arguments(b"key 101\n", Spec(b"key"), &mut small()),
      Err(Refusal::PastAdmission(PastAdmission {
        asked: 101,
        left: 100
      }))
    );
    assert_eq!(
      arguments(b"key 18446744073709551615\n", Spec(b"key"), &mut room()),
      Err(Refusal::LineTooLong {
        what: "argument line",
        limit: 11
      })
    );
    assert_eq!(
      arguments(b"* 26\n", Spec(b"*"), &mut small()),
      Err(Refusal::PastAdmission(PastAdmission {
        asked: 26,
        left: 25
      }))
    );
    assert!(matches!(
      arguments(b"key 00000000001\n", Spec(b"key"), &mut small()),
      Err(Refusal::LineTooLong { .. })
    ));
  }

  /// A-569: do read `unbundle`'s chunked input: two chunks and the end, a chunk still arriving, and a chunk past the
  /// room. Expect each chunk's bytes, `None` at `0\n`, a request for the whole chunk, and `PastAdmission`.
  #[test]
  fn chunked_input_reads_to_its_end() {
    let input = b"3\nabc2\nde0\n";
    assert_eq!(chunk(input, &room()), Ok(Read::Done(Some(&b"abc"[..]), 5)));
    assert_eq!(
      chunk(&input[5..], &room()),
      Ok(Read::Done(Some(&b"de"[..]), 4))
    );
    assert_eq!(chunk(&input[9..], &room()), Ok(Read::Done(None, 2)));
    assert_eq!(chunk(b"5\nab", &room()), Ok(Read::More { needed: 7 }));
    assert!(matches!(
      chunk(b"9\n", &Admission::new(8)),
      Err(Refusal::PastAdmission(_))
    ));
  }

  /// A-569: do write a string answer, an error, and a push's answer without bundle2, and read the string back.
  /// Expect `<length>\n<value>` at the length `respond_len` gave, an empty line on stdout with the message and `\n-\n`
  /// on stderr, and an empty string followed by the result.
  #[test]
  fn answers_are_framed_as_mercurials_server_frames_them() {
    let mut out = Vec::new();
    respond(b"capabilities: lookup", &mut out);
    assert_eq!(out, b"20\ncapabilities: lookup");
    assert_eq!(respond_len(b"capabilities: lookup"), 23);
    assert_eq!(string(&out, &room()), Ok(Read::Done(&b"capabilities: lookup"[..], 23)));
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    respond_error(b"bundle2 required", &mut stdout, &mut stderr);
    assert_eq!((stdout, stderr), (b"\n".to_vec(), b"bundle2 required\n-\n".to_vec()));
    let mut push = Vec::new();
    respond_push(-1, &mut push);
    assert_eq!(push, b"0\n2\n-1");
  }

  /// A generated argument: a lower-case name and any bytes.
  fn arbitrary_arguments() -> impl Strategy<Value = Vec<(String, Vec<u8>)>> {
    proptest::collection::btree_map("[a-z]{1,8}", proptest::collection::vec(any::<u8>(), 0..16), 0..5)
      .prop_map(|map| map.into_iter().collect())
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-569: do write requests of generated named arguments and dictionaries and read them back. Expect the same
    /// values in the spec's order and the same entries in theirs.
    #[test]
    fn generated_requests_read_back(named in arbitrary_arguments(), entries in arbitrary_arguments()) {
      let names: Vec<&[u8]> = named.iter().map(|(name, _)| name.as_bytes()).collect();
      let spec_text = [names.join(&b' '), b"*".to_vec()].join(&b' ');
      let pairs: Vec<(&[u8], &[u8])> = named.iter().map(|(name, value)| (name.as_bytes(), value.as_slice())).collect();
      let dictionary: Vec<(&[u8], &[u8])> = entries.iter().map(|(key, value)| (key.as_bytes(), value.as_slice())).collect();
      let mut request = Vec::new();
      encode_request(b"cmd", &pairs, Some(&dictionary), &mut request);
      let rest = &request[4..];
      let read = arguments(rest, Spec(&spec_text), &mut room());
      let expected = Arguments { named: pairs.iter().map(|(_, value)| *value).collect(), dictionary };
      prop_assert_eq!(read, Ok(Read::Done(expected, rest.len())));
    }

    /// AC-HG.9, A-569: do read arbitrary bytes as arguments, a chunk and a string. Expect a read, a request for more or
    /// a refusal; never a panic, and never a read past the bytes given.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
      if let Ok(Read::Done(_, used)) = arguments(&bytes, Spec(b"one two *"), &mut room()) {
        prop_assert!(used <= bytes.len());
      }
      if let Ok(Read::Done(_, used)) = chunk(&bytes, &room()) {
        prop_assert!(used <= bytes.len());
      }
      if let Ok(Read::Done(_, used)) = string(&bytes, &room()) {
        prop_assert!(used <= bytes.len());
      }
    }
  }
}
