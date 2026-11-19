//! `batch`, several commands in one request (A-569; `wireprotov1server.py` `batch`, `wireprototypes.py`
//! `escapebatcharg` and `unescapebatcharg`, `wireprotov1peer.py` `encodebatchcmds` \[C\]).
//!
//! The `cmds` argument lists the calls: `;` between calls, a space between a call's command and its arguments, `,`
//! between arguments, `=` between an argument's name and its value. Names and values escape the four separators, `:`
//! as `:c`, `,` as `:o`, `;` as `:s` and `=` as `:e`; a command is a plain name. The answer is each call's answer,
//! escaped the same way, joined by `;`.
//!
//! **Stricter than 7.2, on purpose.** 7.2 unescapes with a chain of `replace`, which leaves a `:` before any other
//! byte as it is; an escape other than the four is refused here. A call without its space and an argument without
//! exactly one `=` are refused, as 7.2's `split` fails on them; an empty argument is passed over, as 7.2 passes it.
//!
//! **Bounds.** The input, and the caller's admission: a decode checks the calls and counts their unescaped bytes and
//! their arguments, charges the exact total, then unescapes into the caller's buffer once.

use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{Admission, PastAdmission};

/// Format: the byte that begins an escape.
const ESCAPE: u8 = b':';

/// Format: each escaped byte and the letter that stands for it after the escape (`escapebatcharg`).
const ESCAPES: [(u8, u8); 4] = [(b':', b'c'), (b',', b'o'), (b';', b's'), (b'=', b'e')];

/// Format: the byte between calls.
const CALL_SEPARATOR: u8 = b';';

/// Format: the byte between a call's command and its arguments.
const COMMAND_SEPARATOR: u8 = b' ';

/// Format: the byte between a call's arguments.
const ARGUMENT_SEPARATOR: u8 = b',';

/// Format: the byte between an argument's name and its value.
const VALUE_SEPARATOR: u8 = b'=';

/// One argument of a call: its name and its value.
pub type Argument<'a> = (&'a [u8], &'a [u8]);

/// One call of a batch: its command and its arguments, unescaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call<'b> {
  /// The command.
  pub command: &'b [u8],
  /// The arguments, name and value, in order.
  pub arguments: Vec<Argument<'b>>,
}

/// Why a batch was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A call with no space after its command (7.2's `split(b' ', 1)` fails).
  Call {
    /// The call's index.
    call: u64,
  },
  /// An argument without exactly one `=` (7.2's `split(b'=')` fails).
  Argument {
    /// The call's index.
    call: u64,
  },
  /// A `:` not followed by one of `c`, `o`, `s`, `e`.
  Escape {
    /// The call's index.
    call: u64,
  },
  /// More than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Appends `plain` escaped (`escapebatcharg`).
pub fn escape(plain: &[u8], out: &mut Vec<u8>) {
  escape_into(plain, out);
}

/// Writes `plain` escaped.
fn escape_into(plain: &[u8], out: &mut impl Out) {
  for &byte in plain {
    match ESCAPES.iter().find(|(escaped, _)| *escaped == byte) {
      Some(&(_, letter)) => out.put(&[ESCAPE, letter]),
      None => out.put(&[byte]),
    }
  }
}

/// Writes `escaped` unescaped, or refuses an escape other than the four.
fn unescape_into(escaped: &[u8], call: u64, out: &mut impl Out) -> Result<(), Refusal> {
  let mut rest = escaped;
  while let Some((&byte, tail)) = rest.split_first() {
    if byte != ESCAPE {
      out.put(&[byte]);
      rest = tail;
      continue;
    }
    let (&letter, after) = tail.split_first().ok_or(Refusal::Escape { call })?;
    let &(plain, _) = ESCAPES
      .iter()
      .find(|(_, known)| *known == letter)
      .ok_or(Refusal::Escape { call })?;
    out.put(&[plain]);
    rest = after;
  }
  Ok(())
}

/// The calls of `cmds` and each one's raw arguments, split as 7.2 splits them.
fn split_calls(cmds: &[u8]) -> impl Iterator<Item = Result<(&[u8], &[u8]), Refusal>> + '_ {
  cmds
    .split(|byte| *byte == CALL_SEPARATOR)
    .enumerate()
    .map(|(index, call)| {
      let refusal = Refusal::Call {
        call: to_u64(index),
      };
      let space = call
        .iter()
        .position(|byte| *byte == COMMAND_SEPARATOR)
        .ok_or(refusal)?;
      Ok((
        call.get(..space).unwrap_or_default(),
        call.get(space.saturating_add(1)..).unwrap_or_default(),
      ))
    })
}

/// A call's arguments, each name and value still escaped; empty ones passed over.
fn split_arguments(
  arguments: &[u8],
  call: u64,
) -> impl Iterator<Item = Result<(&[u8], &[u8]), Refusal>> + '_ {
  arguments
    .split(|byte| *byte == ARGUMENT_SEPARATOR)
    .filter(|argument| !argument.is_empty())
    .map(move |argument| {
      let mut halves = argument.split(|byte| *byte == VALUE_SEPARATOR);
      match (halves.next(), halves.next(), halves.next()) {
        (Some(name), Some(value), None) => Ok((name, value)),
        _ => Err(Refusal::Argument { call }),
      }
    })
}

/// What a decode will hold: unescaped bytes, calls, arguments.
#[derive(Default)]
struct Sizes {
  /// The unescaped bytes.
  bytes: u64,
  /// The calls.
  calls: usize,
  /// The arguments, all calls together.
  arguments: usize,
}

/// The calls of a `cmds` argument, their commands and arguments unescaped into `buffer`. The calls are checked and
/// counted first, the exact total charged to `admission`, then built once.
pub fn calls<'b>(
  cmds: &[u8],
  admission: &mut Admission,
  buffer: &'b mut Vec<u8>,
) -> Result<Vec<Call<'b>>, Refusal> {
  let sizes = measure(cmds)?;
  let mut kept = *admission;
  kept.charge(sizes.bytes)?;
  kept.charge_items::<Call<'_>>(sizes.calls)?;
  kept.charge_items::<(&[u8], &[u8])>(sizes.arguments)?;
  buffer.clear();
  buffer.reserve_exact(usize::try_from(sizes.bytes).unwrap_or(usize::MAX));
  for (index, call) in split_calls(cmds).enumerate() {
    let (command, arguments) = call?;
    buffer.extend_from_slice(command);
    for argument in split_arguments(arguments, to_u64(index)) {
      let (name, value) = argument?;
      unescape_into(name, to_u64(index), buffer)?;
      unescape_into(value, to_u64(index), buffer)?;
    }
  }
  let filled: &'b Vec<u8> = buffer;
  let built = slice_calls(cmds, filled, sizes.calls)?;
  *admission = kept;
  Ok(built)
}

/// Checks every call and counts what [`calls`] will hold.
fn measure(cmds: &[u8]) -> Result<Sizes, Refusal> {
  let mut sizes = Sizes::default();
  for (index, call) in split_calls(cmds).enumerate() {
    let (command, arguments) = call?;
    sizes.calls = sizes.calls.saturating_add(1);
    sizes.bytes = sizes.bytes.saturating_add(to_u64(command.len()));
    for argument in split_arguments(arguments, to_u64(index)) {
      let (name, value) = argument?;
      sizes.arguments = sizes.arguments.saturating_add(1);
      let mut length = Measure(0);
      unescape_into(name, to_u64(index), &mut length)?;
      unescape_into(value, to_u64(index), &mut length)?;
      sizes.bytes = sizes.bytes.saturating_add(length.0);
    }
  }
  Ok(sizes)
}

/// The calls over the filled buffer: each command and each unescaped name and value, at the offsets their measured
/// lengths give.
fn slice_calls<'b>(cmds: &[u8], filled: &'b [u8], count: usize) -> Result<Vec<Call<'b>>, Refusal> {
  let mut built = Vec::with_capacity(count);
  let mut at: usize = 0;
  let mut take = |length: u64| {
    let end = at.saturating_add(usize::try_from(length).unwrap_or(usize::MAX));
    let found = filled.get(at..end).unwrap_or_default();
    at = end;
    found
  };
  for (index, call) in split_calls(cmds).enumerate() {
    let (command, arguments) = call?;
    let command = take(to_u64(command.len()));
    let mut pairs = Vec::new();
    for argument in split_arguments(arguments, to_u64(index)) {
      let (name, value) = argument?;
      let (mut name_length, mut value_length) = (Measure(0), Measure(0));
      unescape_into(name, to_u64(index), &mut name_length)?;
      unescape_into(value, to_u64(index), &mut value_length)?;
      pairs.push((take(name_length.0), take(value_length.0)));
    }
    built.push(Call {
      command,
      arguments: pairs,
    });
  }
  Ok(built)
}

/// Appends a `cmds` argument as 7.2's client writes it (`encodebatchcmds`): each command, a space, its arguments as
/// `name=value` escaped and joined by `,`; the calls joined by `;`.
pub fn encode_calls(calls: &[(&[u8], &[Argument<'_>])], out: &mut Vec<u8>) {
  for (index, (command, arguments)) in calls.iter().enumerate() {
    if index > 0 {
      out.push(CALL_SEPARATOR);
    }
    out.extend_from_slice(command);
    out.push(COMMAND_SEPARATOR);
    for (position, (name, value)) in arguments.iter().enumerate() {
      if position > 0 {
        out.push(ARGUMENT_SEPARATOR);
      }
      escape_into(name, out);
      out.push(VALUE_SEPARATOR);
      escape_into(value, out);
    }
  }
}

/// Appends a batch's answer (`batch`): each call's answer escaped, joined by `;`.
pub fn encode_results(results: &[&[u8]], out: &mut Vec<u8>) {
  for (index, result) in results.iter().enumerate() {
    if index > 0 {
      out.push(CALL_SEPARATOR);
    }
    escape_into(result, out);
  }
}

/// The answers of a batch's answer, unescaped into `buffer`, as 7.2's client reads them (`_submitbatch`). The exact
/// total is charged to `admission` first.
pub fn results<'b>(
  answer: &[u8],
  admission: &mut Admission,
  buffer: &'b mut Vec<u8>,
) -> Result<Vec<&'b [u8]>, Refusal> {
  let parts = || answer.split(|byte| *byte == CALL_SEPARATOR).enumerate();
  let mut bytes = Measure(0);
  for (index, part) in parts() {
    unescape_into(part, to_u64(index), &mut bytes)?;
  }
  let count = parts().count();
  let mut kept = *admission;
  kept.charge(bytes.0)?;
  kept.charge_items::<&[u8]>(count)?;
  buffer.clear();
  buffer.reserve_exact(usize::try_from(bytes.0).unwrap_or(usize::MAX));
  for (index, part) in parts() {
    unescape_into(part, to_u64(index), buffer)?;
  }
  let filled: &'b Vec<u8> = buffer;
  let mut built = Vec::with_capacity(count);
  let mut at: usize = 0;
  for (index, part) in parts() {
    let mut length = Measure(0);
    unescape_into(part, to_u64(index), &mut length)?;
    let end = at.saturating_add(usize::try_from(length.0).unwrap_or(usize::MAX));
    built.push(filled.get(at..end).unwrap_or_default());
    at = end;
  }
  *admission = kept;
  Ok(built)
}

#[cfg(test)]
mod tests {
  use super::{Argument, Call, Refusal, calls, encode_calls, encode_results, escape, results};
  use crate::Admission;
  use proptest::prelude::*;

  fn room() -> Admission {
    Admission::new(1 << 20)
  }

  /// A-569: do escape a value holding each separator and an existing escape. Expect 7.2's escapes, `:` first so an
  /// escape is never escaped twice.
  #[test]
  fn values_escape_as_mercurial_escapes_them() {
    let mut out = Vec::new();
    escape(b"a:b,c;d=e:c", &mut out);
    assert_eq!(out, b"a:cb:oc:sd:ee:cc");
  }

  /// A-569: do write the batch 7.2's discovery sends (`heads` without arguments, `known` with nodes) and read it back.
  /// Expect `heads ` with its space, the arguments unescaped, and the same calls.
  #[test]
  fn a_batch_reads_back_as_its_client_wrote_it() {
    let known: [(&[u8], &[u8]); 1] = [(b"nodes", b"ab cd")];
    let mut cmds = Vec::new();
    encode_calls(&[(b"heads", &[]), (b"known", &known)], &mut cmds);
    assert_eq!(cmds, b"heads ;known nodes=ab cd");
    let mut buffer = Vec::new();
    let read = calls(&cmds, &mut room(), &mut buffer).unwrap();
    assert_eq!(
      read,
      [
        Call {
          command: b"heads",
          arguments: vec![]
        },
        Call {
          command: b"known",
          arguments: vec![(&b"nodes"[..], &b"ab cd"[..])]
        }
      ]
    );
  }

  /// AC-HG.9 (hostile input), A-569: do read a call without its space, an argument with two `=`, an escape other than
  /// the four, a `:` at the end, and calls past the admission. Expect each refused by its kind and call, and the
  /// admission left as it was.
  #[test]
  fn batches_mercurial_would_misread_are_refused() {
    let read = |cmds: &[u8], admission: &mut Admission| {
      calls(cmds, admission, &mut Vec::new()).map(|read| read.len())
    };
    assert_eq!(read(b"heads", &mut room()), Err(Refusal::Call { call: 0 }));
    assert_eq!(
      read(b"heads ;known a=b=c", &mut room()),
      Err(Refusal::Argument { call: 1 })
    );
    assert_eq!(
      read(b"lookup key=a:x", &mut room()),
      Err(Refusal::Escape { call: 0 })
    );
    assert_eq!(
      read(b"lookup key=a:", &mut room()),
      Err(Refusal::Escape { call: 0 })
    );
    let mut small = Admission::new(4);
    assert!(matches!(
      read(b"heads ", &mut small),
      Err(Refusal::PastAdmission(_))
    ));
    assert_eq!(small.left(), 4);
  }

  /// A-569: do write a batch's answers, one holding every separator, and read them back. Expect each answer back.
  #[test]
  fn answers_read_back() {
    let answers: [&[u8]; 3] = [b"1;2", b"", b"a:b,c=d"];
    let mut out = Vec::new();
    encode_results(&answers, &mut out);
    assert_eq!(out, b"1:s2;;a:cb:oc:ed");
    assert_eq!(
      results(&out, &mut room(), &mut Vec::new()).unwrap(),
      answers
    );
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-569: do write batches of generated calls whose names and values hold any bytes, and read them back. Expect
    /// the same calls.
    #[test]
    fn generated_batches_read_back(
      generated in proptest::collection::vec(
        ("[a-z]{1,8}", proptest::collection::vec((proptest::collection::vec(any::<u8>(), 1..6), proptest::collection::vec(any::<u8>(), 0..6)), 0..3)),
        1..4,
      ),
    ) {
      let arguments: Vec<Vec<Argument<'_>>> = generated.iter().map(|(_, pairs)| pairs.iter().map(|(name, value)| (name.as_slice(), value.as_slice())).collect()).collect();
      let encoded_calls: Vec<(&[u8], &[Argument<'_>])> = generated.iter().zip(&arguments).map(|((command, _), pairs)| (command.as_bytes(), pairs.as_slice())).collect();
      let mut cmds = Vec::new();
      encode_calls(&encoded_calls, &mut cmds);
      let mut buffer = Vec::new();
      let read = calls(&cmds, &mut room(), &mut buffer).unwrap();
      let expected: Vec<Call<'_>> = encoded_calls.iter().map(|(command, pairs)| Call { command, arguments: pairs.to_vec() }).collect();
      prop_assert_eq!(read, expected);
    }

    /// AC-HG.9, A-569: do read arbitrary bytes as a batch and as its answer. Expect calls, answers or a refusal; never
    /// a panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
      let mut buffer = Vec::new();
      if let Ok(read) = calls(&bytes, &mut room(), &mut buffer) {
        prop_assert!(!read.is_empty());
      }
      let mut answers = Vec::new();
      if let Ok(read) = results(&bytes, &mut room(), &mut answers) {
        prop_assert!(!read.is_empty());
      }
    }
  }
}
