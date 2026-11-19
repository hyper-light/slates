//! An SSH version 1 session of `slates hg serve --stdio` (A-619 part 3; §4.17.3 "Pull and clone"; 7.2's
//! `wireprotoserver._runsshserver`, `wireprotov1server.dispatch`, `batch`, `hello`, `protocaps` and `getbundle` \[C\]):
//! one step serves the request at the front of the buffered input, as 7.2's loop serves one per line.
//!
//! **A step.** A command line, its arguments by 7.2's spec for it (`wireprotov1server.commands`), then its answer: a
//! string (`<length>\n<value>`), a raw bundle2 stream for `getbundle`, or an out-of-band error (an empty line on
//! stdout and the message then `\n-\n` on stderr). An empty line ends the session. A name 7.2's table lacks is
//! answered with an empty string and no argument read, as 7.2 answers it. A command 7.2 serves and this server does not
//! (pushes, `lookup`, stream clones, clone bundles, `debugwireargs`) has its arguments read, so the session stays in
//! step, and is answered with an out-of-band error. `batch` runs each call of a command that answers a string and joins
//! the escaped answers with `;`. A `getbundle` refused where the client asked for bundle2 is answered as 7.2 answers an
//! abort there: a bundle2 stream of one `ERROR:ABORT` part with the message; a bundle1 request is answered out of band.
//!
//! **What is advertised.** `hello` and `capabilities` list what is served, sorted as 7.2 sorts them: `batch`,
//! `branchmap`, `bundle2` (the parts a `getbundle` answer carries, and `error=abort`), `getbundle`, `known` and
//! `protocaps`. A 7.2 client asks the same `getbundle` of it as of a stock server (`exchange._pullbundle2` reads
//! `phases`, `bookmarks`, `listkeys` and `obsmarkers` from it); it lacks `lookup` (a pull of a named revision is refused
//! by the client), `pushkey`, `unbundle` and stream clones, which later pieces serve.
//!
//! **Stricter than 7.2, on purpose.** A request that cannot be read (a line too long for any command, an argument past
//! the admission) is answered out of band and ends the session, where 7.2 fails mid-stream. A command's own refusal is
//! answered out of band and the session goes on, where 7.2's exceptions end the process. A batch call of a command that
//! does not answer a string is refused (7.2 asserts).

use super::getbundle::{Asked, Store, getbundle};
use super::{Refusal, discovery, pull};
use crate::bundle2::{Bundle2, Event, Param, Part};
use crate::ssh::{self, Read};
use crate::wire::{Arguments, Spec};
use crate::{Admission, PastAdmission, batch, capabilities, pushkey};

/// Format: 7.2's version 1 commands and their argument specs (`wireprotov1server.commands`).
const COMMANDS: [(&[u8], &[u8]); 21] = [
  (b"batch", b"cmds *"),
  (b"between", b"pairs"),
  (b"branches", b"nodes"),
  (b"branchmap", b""),
  (b"capabilities", b""),
  (b"changegroup", b"roots"),
  (b"changegroupsubset", b"bases heads"),
  (b"clonebundles", b""),
  (b"clonebundles_manifest", b"*"),
  (b"debugwireargs", b"one two *"),
  (b"get_cached_bundle_inline", b"path"),
  (b"getbundle", b"*"),
  (b"heads", b""),
  (b"hello", b""),
  (b"known", b"nodes *"),
  (b"listkeys", b"namespace"),
  (b"lookup", b"key"),
  (b"protocaps", b"caps"),
  (b"pushkey", b"namespace key old new"),
  (b"stream_out", b""),
  (b"unbundle", b"heads"),
];

/// Format: the capabilities advertised, sorted as `capabilities` sorts them; `bundle2`'s value is made from
/// [`BUNDLE2_SERVED`].
const SERVED: [&[u8]; 6] = [
  b"batch",
  b"branchmap",
  b"bundle2",
  b"getbundle",
  b"known",
  b"protocaps",
];

/// Format: the bundle2 capabilities advertised, by name: what a `getbundle` answer carries (A-619 part 2).
const BUNDLE2_SERVED: [(&[u8], &[&[u8]]); 7] = [
  (b"HG20", &[]),
  (b"bookmarks", &[]),
  (b"changegroup", &[b"02", b"03"]),
  (b"error", &[b"abort"]),
  (b"listkeys", &[]),
  (b"obsmarkers", &[b"V0", b"V1"]),
  (b"phases", &[b"heads"]),
];

/// Format: `protocaps`' answer.
const OK: &[u8] = b"OK";

/// Format: the part 7.2 answers a bundle2 request's abort with, mandatory, and its message's key.
const ABORT_PART: &[u8] = b"ERROR:ABORT";

/// Format: the key of an abort part's message.
const MESSAGE: &[u8] = b"message";

/// Derived: the most bytes escaping writes for one byte (`:` and the letter for each separator).
const ESCAPE_GROWTH: u64 = 2;

/// What one step of a session did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
  /// A request was answered; `refused` says why where its answer is an error.
  Answered {
    /// The refusal its error answer reports, if any.
    refused: Option<Refused>,
  },
  /// The session is over: the empty line that ends it, or a request that could not be read (answered as an error).
  Ended {
    /// Why the request could not be read, if that ended it.
    refused: Option<Refused>,
  },
}

/// Why a request was answered with an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The request's line or arguments could not be read.
  Request(ssh::Refusal),
  /// A batch's calls could not be read.
  Batch(batch::Refusal),
  /// A command 7.2 serves that this server does not, or one a batch cannot carry.
  NotServed,
  /// A call without an argument its command needs.
  MissingArgument {
    /// The argument's name.
    name: &'static [u8],
  },
  /// The command's own refusal.
  Serve(Refusal),
}

impl From<Refusal> for Refused {
  fn from(refusal: Refusal) -> Self {
    Self::Serve(refusal)
  }
}

impl From<batch::Refusal> for Refused {
  fn from(refusal: batch::Refusal) -> Self {
    Self::Batch(refusal)
  }
}

impl From<PastAdmission> for Refused {
  fn from(past: PastAdmission) -> Self {
    Self::Serve(Refusal::PastAdmission(past))
  }
}

/// Serves the request at the front of `input`: `Done(step, consumed)`, or `More` where the buffer holds only part of
/// it. Answers go to `stdout` and, for an error, `stderr`; everything held is charged to `admission`.
pub fn serve(
  store: &impl Store,
  input: &[u8],
  admission: &mut Admission,
  stdout: &mut Vec<u8>,
  stderr: &mut Vec<u8>,
) -> Read<Step> {
  let longest = COMMANDS
    .iter()
    .map(|(name, _)| name.len())
    .max()
    .unwrap_or(0);
  let (command, used) = match ssh::command(input, longest) {
    Ok(Read::Done(command, used)) => (command, used),
    Ok(Read::More { needed }) => return Read::More { needed },
    Err(refusal) => return ended(Refused::Request(refusal), input.len(), stdout, stderr),
  };
  if command.is_empty() {
    return Read::Done(Step::Ended { refused: None }, used);
  }
  let Some(spec) = COMMANDS
    .iter()
    .find(|(name, _)| *name == command)
    .map(|(_, spec)| Spec(*spec))
  else {
    ssh::respond(&[], stdout);
    return Read::Done(Step::Answered { refused: None }, used);
  };
  let rest = input.get(used..).unwrap_or_default();
  let (arguments, taken) = match ssh::arguments(rest, spec, admission) {
    Ok(Read::Done(arguments, taken)) => (arguments, taken),
    Ok(Read::More { needed }) => {
      return Read::More {
        needed: used.saturating_add(needed),
      };
    }
    Err(refusal) => return ended(Refused::Request(refusal), input.len(), stdout, stderr),
  };
  let refused = answer(store, command, spec, &arguments, admission, stdout, stderr);
  Read::Done(Step::Answered { refused }, used.saturating_add(taken))
}

/// Answers an unreadable request out of band and ends the session, the rest of the input taken.
fn ended(
  refused: Refused,
  consumed: usize,
  stdout: &mut Vec<u8>,
  stderr: &mut Vec<u8>,
) -> Read<Step> {
  ssh::respond_error(&message(&refused), stdout, stderr);
  Read::Done(
    Step::Ended {
      refused: Some(refused),
    },
    consumed,
  )
}

/// Answers one read request; the refusal its error answer reports, if any.
fn answer(
  store: &impl Store,
  command: &[u8],
  spec: Spec<'_>,
  arguments: &Arguments<'_>,
  admission: &mut Admission,
  stdout: &mut Vec<u8>,
  stderr: &mut Vec<u8>,
) -> Option<Refused> {
  if command == b"getbundle" {
    return getbundle_answer(store, arguments, admission, stdout, stderr);
  }
  let mut value = Vec::new();
  let answered = if command == b"batch" {
    let cmds = arguments.get(spec, b"cmds").unwrap_or_default();
    batch_answer(store, cmds, admission, &mut value)
  } else {
    let named = |name: &[u8]| arguments.get(spec, name);
    string_answer(store, command, named, admission, &mut value)
  };
  match answered.and_then(|()| {
    admission
      .charge(ssh::respond_len(&value))
      .map_err(Refused::from)
  }) {
    Ok(()) => {
      ssh::respond(&value, stdout);
      None
    }
    Err(refused) => {
      ssh::respond_error(&message(&refused), stdout, stderr);
      Some(refused)
    }
  }
}

/// Writes the answer of a command that answers a string, its arguments found by name.
fn string_answer<'a>(
  store: &impl Store,
  command: &[u8],
  argument: impl Fn(&[u8]) -> Option<&'a [u8]>,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let need = |name: &'static [u8]| argument(name).ok_or(Refused::MissingArgument { name });
  match command {
    b"heads" => discovery::heads(store, admission, out)?,
    b"known" => discovery::known(store, need(b"nodes")?, admission, out)?,
    b"between" => discovery::between(store, need(b"pairs")?, admission, out)?,
    b"branches" => discovery::branches(store, need(b"nodes")?, admission, out)?,
    b"branchmap" => discovery::branchmap(store, admission, out)?,
    b"listkeys" => pull::listkeys(store, need(b"namespace")?, admission, out)?,
    b"capabilities" => advertised(admission, out)?,
    b"hello" => {
      let mut line = Vec::new();
      advertised(admission, &mut line)?;
      capabilities::encode_hello(&line, out);
    }
    b"protocaps" => out.extend_from_slice(OK),
    _ => return Err(Refused::NotServed),
  }
  Ok(())
}

/// Writes the capabilities line: [`SERVED`], `bundle2`'s value the quoted blob of [`BUNDLE2_SERVED`].
fn advertised(admission: &mut Admission, out: &mut Vec<u8>) -> Result<(), Refused> {
  let mut blob = Vec::new();
  capabilities::encode_bundle2(&BUNDLE2_SERVED, &mut blob);
  let mut quoted = Vec::new();
  capabilities::encode_bundle2_value(&blob, &mut quoted);
  let line: Vec<(&[u8], Option<&[u8]>)> = SERVED
    .iter()
    .map(|name| {
      (
        *name,
        (*name == capabilities::BUNDLE2).then_some(quoted.as_slice()),
      )
    })
    .collect();
  let len = line.iter().fold(0_usize, |total, (name, value)| {
    total
      .saturating_add(name.len())
      .saturating_add(value.map_or(0, |value| value.len().saturating_add(1)))
      .saturating_add(1)
  });
  admission.charge(crate::read::to_u64(len))?;
  capabilities::encode(&line, out).map_err(|_| {
    Refused::Serve(Refusal::Unencodable {
      what: "a capability",
    })
  })
}

/// Writes a batch's answer: each call's string answer, escaped, joined by `;`. A call of a command that does not answer
/// a string is refused, and with it the batch.
fn batch_answer(
  store: &impl Store,
  cmds: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let mut buffer = Vec::new();
  let calls = batch::calls(cmds, admission, &mut buffer)?;
  admission.charge_items::<Vec<u8>>(calls.len())?;
  let mut results = Vec::with_capacity(calls.len());
  for call in &calls {
    if call.command == b"batch" || call.command == b"getbundle" {
      return Err(Refused::NotServed);
    }
    let argument = |name: &[u8]| {
      call
        .arguments
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
    };
    let mut result = Vec::new();
    string_answer(store, call.command, argument, admission, &mut result)?;
    results.push(result);
  }
  let total = results.iter().fold(0_u64, |total, result| {
    total.saturating_add(
      crate::read::to_u64(result.len())
        .saturating_mul(ESCAPE_GROWTH)
        .saturating_add(1),
    )
  });
  admission.charge(total)?;
  let views: Vec<&[u8]> = results.iter().map(Vec::as_slice).collect();
  batch::encode_results(&views, out);
  Ok(())
}

/// Answers a `getbundle`: its stream; a refusal of a bundle2 request as an `ERROR:ABORT` part, of a bundle1 request
/// out of band.
fn getbundle_answer(
  store: &impl Store,
  arguments: &Arguments<'_>,
  admission: &mut Admission,
  stdout: &mut Vec<u8>,
  stderr: &mut Vec<u8>,
) -> Option<Refused> {
  let asked = Asked {
    heads: arguments.entry(b"heads").unwrap_or_default(),
    common: arguments.entry(b"common").unwrap_or_default(),
    bundlecaps: arguments.entry(b"bundlecaps").unwrap_or_default(),
    cg: arguments.entry(b"cg"),
    phases: arguments.entry(b"phases"),
    bookmarks: arguments.entry(b"bookmarks"),
    obsmarkers: arguments.entry(b"obsmarkers"),
    listkeys: arguments.entry(b"listkeys"),
  };
  let mut stream = Vec::new();
  let refusal = match getbundle(store, &asked, admission, &mut stream) {
    Ok(()) => {
      stdout.extend_from_slice(&stream);
      return None;
    }
    Err(refusal) => refusal,
  };
  let bundle1 = refusal == Refusal::NoBundle2;
  let refused = Refused::Serve(refusal);
  let words = message(&refused);
  if bundle1 || abort_stream(&words, stdout).is_err() {
    ssh::respond_error(&words, stdout, stderr);
  }
  Some(refused)
}

/// Appends a bundle2 stream of one `ERROR:ABORT` part carrying `words` (`wireprotov1server.getbundle`'s answer to an
/// abort of a bundle2 request).
fn abort_stream(words: &[u8], out: &mut Vec<u8>) -> Result<(), crate::bundle2::EncodeRefusal> {
  let stream = Bundle2 {
    params: Vec::new(),
    parts: vec![Part {
      kind: ABORT_PART,
      id: 0,
      params: vec![Param {
        key: MESSAGE,
        value: words,
      }],
      mandatory_params: 1,
      depth: 0,
    }],
    events: vec![Event::Open(0), Event::Close],
  };
  stream.encode(out)
}

/// The words an error answer carries for a refusal: 7.2's own where it aborts alike.
fn message(refused: &Refused) -> Vec<u8> {
  let words = match refused {
    Refused::Request(refusal) => format!("unreadable request: {refusal:?}"),
    Refused::Batch(refusal) => format!("unreadable batch: {refusal:?}"),
    Refused::NotServed => "command not served here".to_owned(),
    Refused::MissingArgument { name } => {
      format!("missing argument {}", String::from_utf8_lossy(name))
    }
    Refused::Serve(refusal) => serve_message(refusal),
  };
  words.into_bytes()
}

/// A serving refusal's words, 7.2's where it aborts alike.
fn serve_message(refusal: &Refusal) -> String {
  match refusal {
    Refusal::NoCommonExchange {
      what: "changegroup version",
    } => "no common changegroup version".to_owned(),
    Refusal::NoCommonExchange {
      what: "obsolescence marker format",
    } => "bundler does not support common obsmarker format".to_owned(),
    Refusal::NoCommonExchange { what } => format!("no common {what} exchange method"),
    Refusal::NoBundle2 => "bundle1 is not served here; this server answers bundle2".to_owned(),
    Refusal::UnknownNode { node } => {
      let mut hex = Vec::new();
      pushkey::encode_hex(node, &mut hex);
      format!("unknown revision {}", String::from_utf8_lossy(&hex))
    }
    other => format!("{other:?}"),
  }
}
