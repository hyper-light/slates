//! An SSH version 1 session of `slates hg serve --stdio` (A-619 part 3; §4.17.3 "Pull and clone"; 7.2's
//! `wireprotoserver._runsshserver`, `wireprotov1server.dispatch`, `batch`, `hello`, `protocaps` and `getbundle` \[C\]):
//! one step serves the request at the front of the buffered input, as 7.2's loop serves one per line.
//!
//! **A step.** A command line, its arguments by 7.2's spec for it (`wireprotov1server.commands`), then its answer: a
//! string (`<length>\n<value>`), a raw bundle2 stream for `getbundle`, or an out-of-band error (an empty line on
//! stdout and the message then `\n-\n` on stderr). An empty line ends the session. A name 7.2's table lacks is
//! answered with an empty string and no argument read, as 7.2 answers it. A push's `unbundle` is taken by the session's
//! carrier before this step, since its bundle follows its request (over SSH [`super::stdio`], over HTTP
//! `super::http::unbundle_request`); so is a `pushkey`, which lands through the same driver (A-758, [`super::keys`]). A
//! command 7.2 serves and this server does not (clone bundles, `debugwireargs`) has its arguments read, so the session stays in step, and is answered with an
//! out-of-band error. `batch` runs each call of a command that answers a string and joins
//! the escaped answers with `;`. A `getbundle` refused where the client asked for bundle2 is answered as 7.2 answers an
//! abort there: a bundle2 stream of one `ERROR:ABORT` part with the message; a bundle1 request is answered out of band.
//!
//! **What is advertised.** `hello` and `capabilities` list what is served, sorted as 7.2 sorts them: `batch`,
//! `branchmap`, `bundle2`, `getbundle`, `known`, `lookup` (`super::lookup`, as 7.2 always lists it), `protocaps`,
//! `pushkey` and `unbundle`. A 7.2 client asks the same `getbundle` of
//! it as of a stock server (`exchange._pullbundle2` reads `phases`, `bookmarks`, `listkeys` and `obsmarkers` from it).
//! `pushkey` is listed since its command is served (A-758), and a 7.2 client asks `listkeys` of no server that does not
//! list it (`wireprotov1peer.listkeys` answers `{}` without asking): `hg incoming -B`, `hg outgoing -B` and `hg
//! debugpushkey` saw no bookmark here before it was. Pushes are served over both transports (over HTTP A-680 part 2b, `super::http::pushed`; over SSH
//! A-696, `super::stdio`), so `unbundle` is listed with 7.2's value and `bundle2` holds, beside the parts a `getbundle`
//! answer carries, what a push needs, as 7.2 lists them: `changegroup=01,02,03` (the versions a push is taken in),
//! `checkheads=related` (decided: the verdict is per line, so a client checks only the heads it moves),
//! `error=abort,unsupportedcontent,pushraced,pushkey` and `pushkey`. A 7.2 client reads these to build its bundle, so a
//! stock client sends this server the bundle it sends a stock server (A-696's golden); one that reads `bookmarks` and
//! `phases=heads` moves both in its bundle, and calls the `pushkey` command only for `hg debugpushkey`.
//!
//! **Shared with HTTP** (A-650 part 1). [`dispatch`] answers a command whatever carries it, giving 7.2's three kinds
//! of answer (a string, a stream, an error out of band) for its transport to frame. The transport decides what
//! `capabilities` and `hello` list (over HTTP, `compression`, `httpheader` and `httpmediatype` beside the rest, and no
//! `protocaps`) and the words a bundle1 `getbundle` is refused with (over HTTP, 7.2's own).
//!
//! **Stricter than 7.2, on purpose.** A request that cannot be read (a line too long for any command, an argument past
//! the admission) is answered out of band and ends the session, where 7.2 fails mid-stream. A command's own refusal is
//! answered out of band and the session goes on, where 7.2's exceptions end the process. A batch call of a command that
//! does not answer a string is refused (7.2 asserts).

use super::getbundle::{Answering, Asked, Store, getbundle};
use super::keys;
use super::{Refusal, discovery, lookup, pull, stream};
use crate::bundle2::{Bundle2, Event, Param, Part};
use crate::decimal::Decimal;
use crate::push::Outcome;
use crate::ssh::{self, Read};
use crate::wire::{Arguments, Spec};
use crate::{Admission, PastAdmission, batch, capabilities, compression, http, pushkey};

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

/// Format: the capabilities advertised over SSH, sorted as `capabilities` sorts them; `bundle2`'s value is made from
/// [`BUNDLE2_SERVED`], and `pushkey` and `unbundle` are listed since pushes are served over SSH (A-696, A-758).
const SERVED: [&[u8]; 9] = [
  b"batch",
  b"branchmap",
  b"bundle2",
  b"getbundle",
  b"known",
  b"lookup",
  b"protocaps",
  b"pushkey",
  b"unbundle",
];

/// Format: the bundle2 capabilities advertised over either transport, by name (module docs): what a `getbundle` answer
/// carries (A-619 part 2), and what a push needs beside, as 7.2's `getrepocaps` names them for a server
/// (`bundle2.capabilities`, `concurrent-push-mode`'s `check-related`; A-680 part 2b, A-696).
const BUNDLE2_SERVED: [(&[u8], &[&[u8]]); 9] = [
  (b"HG20", &[]),
  (b"bookmarks", &[]),
  (b"changegroup", &[b"01", b"02", b"03"]),
  (b"checkheads", &[b"related"]),
  (
    b"error",
    &[b"abort", b"unsupportedcontent", b"pushraced", b"pushkey"],
  ),
  (b"listkeys", &[]),
  (b"obsmarkers", &[b"V0", b"V1"]),
  (b"phases", &[b"heads"]),
  (b"pushkey", &[]),
];

/// Format: the capabilities advertised over HTTP, sorted as `capabilities` sorts them: SSH's but `protocaps` (an HTTP
/// client says what it reads in `X-HgProto`), what 7.2's `httpv1protocolhandler.addcapabilities` adds beside `batch`
/// (`compression`, `httpheader` and `httpmediatype`), and `pushkey` and `unbundle`, since pushes are served over HTTP
/// (A-680 part 2b, A-758).
const SERVED_HTTP: [&[u8]; 11] = [
  b"batch",
  b"branchmap",
  b"bundle2",
  b"compression",
  b"getbundle",
  b"httpheader",
  b"httpmediatype",
  b"known",
  b"lookup",
  b"pushkey",
  b"unbundle",
];

/// Format: `unbundle`'s value, the bundle1 formats 7.2 lists for a push (`bundle2.bundlepriority`); a bundle1 push is
/// refused with 7.2's own `bundle2required` words (A-680 part 2b), as a 7.2 server whose `server.bundle1` is off
/// refuses one while listing the same value.
const UNBUNDLE_FORMATS: &[u8] = b"HG10GZ,HG10BZ,HG10UN";

/// Format: `compression`'s value over HTTP: the engines a `0.2` answer may be written in, each of A-598's by its wire
/// name; which one an answer takes is its caller's choice ([`super::http::Compressor`]).
const HTTP_ENGINES: &[u8] = b"zstd,zlib,bzip2,none";

/// Format: `httpmediatype`'s value, as 7.2 advertises it: `0.1` requests read (`rx`), `0.1` and `0.2` answers written
/// (`tx`).
const HTTP_MEDIA_TYPES: &[u8] = b"0.1rx,0.1tx,0.2tx";

/// Format: 7.2's answer over HTTP to a bundle1 `getbundle` or push where bundle2 is required (`wireprotov1server.py`'s
/// `bundle2required`: the message, then the hint in parentheses, each line ended).
pub(super) const BUNDLE2_REQUIRED: &[u8] = b"incompatible Mercurial client; bundle2 required\n\
  (see https://www.mercurial-scm.org/wiki/IncompatibleClient)\n";

/// Format: the capability naming the requirements a stream clone's client needs (7.2's `streamreqs`), listed while a
/// stream is served ([`super::getbundle::Store::streams`]; stream clones part 2), its value
/// [`crate::render::STREAM_REQUIREMENTS`]; never `stream`, which 7.2 lists only for a store of `revlogv1` alone.
const STREAMREQS: &[u8] = b"streamreqs";

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
  /// An HTTP request whose arguments could not be read (A-650 part 1, [`super::http`]).
  Http(http::Refusal),
  /// An HTTP request naming no command 7.2's table holds (7.2: "requested wire protocol command is not available over
  /// HTTP").
  NotAvailable,
  /// A stream answer its chosen engine could not compress.
  Compression(compression::EncodeRefusal),
  /// A stream answer's compression chosen in an engine the client does not read.
  EngineNotRead {
    /// The engine.
    engine: compression::Engine,
  },
  /// A request that needs more input than its channel lets the client send unconsumed, so it can never be whole
  /// (A-650 part 2, [`super::stdio`]).
  PastWindow {
    /// The input bytes the request needs.
    needed: u64,
    /// The channel's window.
    capacity: u64,
  },
  /// A push refused (A-680 part 2b, [`super::http::pushed`]): its words travel in its own answer, the error part of its
  /// reply or 7.2's `pusherr`.
  Push,
  /// A `pushkey` refused before anything was landed (A-758, [`super::keys`]): its words travel in its answer.
  Pushkey(super::keys::Refusal),
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
/// it. Answers go to `stdout` and, for an error, `stderr`; everything held is charged to `admission`. A `getbundle`'s
/// answer is made whole here ([`begin_serving`]'s, its steps at once); a refusal past its stream's start ends the
/// session, as 7.2 fails mid-stream.
pub fn serve(
  store: &impl Store,
  input: &[u8],
  admission: &mut Admission,
  stdout: &mut Vec<u8>,
  stderr: &mut Vec<u8>,
) -> Read<Step> {
  let mut answering = None;
  let read = begin_serving(store, input, admission, (stdout, stderr), &mut answering);
  let Some(mut answering) = answering else {
    return read;
  };
  loop {
    match answering.step(store, admission, stdout) {
      Ok(true) => {}
      Ok(false) => return read,
      Err(refusal) => {
        let consumed = match read {
          Read::Done(_, consumed) => consumed,
          Read::More { .. } => 0,
        };
        return Read::Done(
          Step::Ended {
            refused: Some(Refused::Serve(refusal)),
          },
          consumed,
        );
      }
    }
  }
}

/// An answer made a slice at a time (A-745; §4.17.1 "A fetch's answer"): a `getbundle`'s, or a `stream_out`'s.
#[derive(Debug)]
pub enum Producing {
  /// A `getbundle`'s bundle2 stream.
  Bundle(Box<Answering>),
  /// A `stream_out`'s revlogs.
  Revlogs(Box<crate::render::Streaming>),
}

impl Producing {
  /// Appends the answer's next slice to `out`, what it makes charged to `room`: whether more follow. A refused step
  /// leaves the answer where it was.
  pub fn step(
    &mut self,
    store: &impl Store,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    match self {
      Producing::Bundle(answering) => answering.step(store, room, out),
      Producing::Revlogs(streaming) => {
        let (index, content) = store.revlogs().ok_or(Refusal::Store { what: "a revlog" })?;
        streaming
          .step(index, content, room, out)
          .map_err(stream::revlog_refused)
      }
    }
  }
}

/// Serves the request at the front of `input` as [`serve`] does, but a `getbundle` whose answer begins, or a
/// `stream_out` whose revlogs are served, is handed back in `answering` to be made a slice at a time (A-745), nothing of
/// it written yet.
pub fn begin_serving(
  store: &impl Store,
  input: &[u8],
  admission: &mut Admission,
  (stdout, stderr): (&mut Vec<u8>, &mut Vec<u8>),
  answering: &mut Option<Producing>,
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
  let Some(spec) = spec_of(command) else {
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
  let consumed = used.saturating_add(taken);
  if command == b"getbundle" {
    let asked = getbundle_asked(&arguments);
    let refused = match Answering::begin(store, &asked, admission) {
      Ok(begun) => {
        *answering = Some(Producing::Bundle(Box::new(begun)));
        None
      }
      Err(refusal) => {
        let answered = getbundle_refused(refusal, Transport::Ssh);
        framed(answered, admission, stdout, stderr)
      }
    };
    return Read::Done(Step::Answered { refused }, consumed);
  }
  if command == b"stream_out" && store.revlogs().is_some() {
    *answering = Some(Producing::Revlogs(Box::default()));
    return Read::Done(Step::Answered { refused: None }, consumed);
  }
  let refused = answer(store, command, spec, &arguments, admission, stdout, stderr);
  Read::Done(Step::Answered { refused }, consumed)
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

/// Answers one read request over SSH, framed as 7.2 frames it there (`_runsshserver`): a string as `<length>\n<value>`,
/// a stream as it is, an error out of band; the refusal its error answer reports, if any.
fn answer(
  store: &impl Store,
  command: &[u8],
  spec: Spec<'_>,
  arguments: &Arguments<'_>,
  admission: &mut Admission,
  stdout: &mut Vec<u8>,
  stderr: &mut Vec<u8>,
) -> Option<Refused> {
  let answered = dispatch(store, Transport::Ssh, command, spec, arguments, admission);
  framed(answered, admission, stdout, stderr)
}

/// An answer and its refusal framed over SSH (`_runsshserver`): a string as `<length>\n<value>`, a stream as it is, an
/// error out of band; the refusal its error answer reports, if any.
fn framed(
  (answered, refused): (Answer, Option<Refused>),
  admission: &mut Admission,
  stdout: &mut Vec<u8>,
  stderr: &mut Vec<u8>,
) -> Option<Refused> {
  match answered {
    Answer::Bytes(value) => match admission.charge(ssh::respond_len(&value)) {
      Ok(()) => {
        ssh::respond(&value, stdout);
        refused
      }
      Err(past) => {
        let refused = Refused::from(past);
        ssh::respond_error(&message(&refused), stdout, stderr);
        Some(refused)
      }
    },
    Answer::Stream { stream, .. } | Answer::Legacy(stream) => {
      stdout.extend_from_slice(&stream);
      refused
    }
    Answer::OutOfBand(words) => {
      ssh::respond_error(&words, stdout, stderr);
      refused
    }
  }
}

/// Which transport a command is answered over (§4.17.3 "The wire surface"): what `capabilities` and `hello` list, and
/// the words a bundle1 `getbundle` is refused with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
  /// SSH version 1 (this module's session).
  Ssh,
  /// HTTP version 1 ([`super::http`]).
  Http {
    /// `httpheader`: the longest header line the server's edge reads, which a client splits its arguments to.
    header_line: u64,
  },
}

/// A command's answer before its transport frames it: 7.2's three kinds of answer (`wireprototypes`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
  /// A string (`bytesresponse`).
  Bytes(Vec<u8>),
  /// A bundle2 stream (`streamres`): `getbundle`'s, or the abort part 7.2 answers a refused bundle2 request with, which
  /// 7.2 sends uncompressed (`prefer_uncompressed`).
  Stream {
    /// The stream.
    stream: Vec<u8>,
    /// Whether 7.2 sends it uncompressed.
    prefer_uncompressed: bool,
  },
  /// An error out of band (`ooberror`): its words.
  OutOfBand(Vec<u8>),
  /// A raw stream 7.2 sends as it is (`streamreslegacy`): `stream_out`'s, over HTTP as `application/mercurial-0.1`.
  Legacy(Vec<u8>),
}

/// The argument spec 7.2 reads `command`'s arguments by (`wireprotov1server.commands`), `None` for a name its table
/// lacks.
pub(crate) fn spec_of(command: &[u8]) -> Option<Spec<'static>> {
  COMMANDS
    .iter()
    .find(|(name, _)| *name == command)
    .map(|(_, spec)| Spec(spec))
}

/// Answers `command`, its arguments read by `spec`, over `transport`: its answer, and the refusal an error answer
/// reports. Everything held is charged to `admission`.
pub(crate) fn dispatch(
  store: &impl Store,
  transport: Transport,
  command: &[u8],
  spec: Spec<'_>,
  arguments: &Arguments<'_>,
  admission: &mut Admission,
) -> (Answer, Option<Refused>) {
  if command == b"getbundle" {
    return getbundle_answer(store, transport, arguments, admission);
  }
  if command == b"stream_out" {
    let mut stream = Vec::new();
    return match stream::stream_out(store, admission, &mut stream) {
      Ok(()) => (Answer::Legacy(stream), None),
      Err(refusal) => {
        let refused = Refused::Serve(refusal);
        (Answer::OutOfBand(message(&refused)), Some(refused))
      }
    };
  }
  let mut value = Vec::new();
  let answered = if command == b"batch" {
    let cmds = arguments.get(spec, b"cmds").unwrap_or_default();
    batch_answer(store, transport, cmds, admission, &mut value)
  } else {
    let named = |name: &[u8]| arguments.get(spec, name);
    string_answer(store, transport, command, named, admission, &mut value)
  };
  match answered {
    Ok(()) => (Answer::Bytes(value), None),
    Err(refused) => (Answer::OutOfBand(message(&refused)), Some(refused)),
  }
}

/// Writes the answer of a command that answers a string, its arguments found by name.
fn string_answer<'a>(
  store: &impl Store,
  transport: Transport,
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
    b"lookup" => lookup::lookup(store, need(b"key")?, admission, out)?,
    b"capabilities" => advertised((transport, store.streams()), admission, out)?,
    b"hello" => {
      let mut line = Vec::new();
      advertised((transport, store.streams()), admission, &mut line)?;
      capabilities::encode_hello(&line, out);
    }
    b"protocaps" => out.extend_from_slice(OK),
    _ => return Err(Refused::NotServed),
  }
  Ok(())
}

/// Writes the capabilities line of what is served over `transport`: over SSH [`SERVED`]; over HTTP [`SERVED_HTTP`],
/// `httpheader`'s value the transport's; over either, `bundle2`'s value the quoted blob of [`BUNDLE2_SERVED`].
fn advertised(
  (transport, streams): (Transport, bool),
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let (served, header_line): (&[&[u8]], u64) = match transport {
    Transport::Ssh => (SERVED.as_slice(), 0),
    Transport::Http { header_line } => (SERVED_HTTP.as_slice(), header_line),
  };
  let mut names = served.to_vec();
  if streams {
    names.push(STREAMREQS);
    names.sort_unstable();
  }
  let mut blob = Vec::new();
  capabilities::encode_bundle2(&BUNDLE2_SERVED, &mut blob);
  let mut quoted = Vec::new();
  capabilities::encode_bundle2_value(&blob, &mut quoted);
  let header_line = Decimal::unsigned(header_line);
  let value = |name: &[u8]| -> Option<&[u8]> {
    match name {
      b"bundle2" => Some(quoted.as_slice()),
      b"compression" => Some(HTTP_ENGINES),
      b"httpheader" => Some(header_line.as_bytes()),
      b"httpmediatype" => Some(HTTP_MEDIA_TYPES),
      b"unbundle" => Some(UNBUNDLE_FORMATS),
      STREAMREQS => Some(crate::render::STREAM_REQUIREMENTS),
      _ => None,
    }
  };
  let line: Vec<(&[u8], Option<&[u8]>)> = names.iter().map(|name| (*name, value(name))).collect();
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

/// Writes a batch's answer where no call needs a push landed: each call's string answer, escaped, joined by `;`
/// ([`Batch`]). A call of a command that does not answer a string is refused, and with it the batch; so is a `pushkey`
/// call here, whose push only a caller holding the push driver lands (the SSH session and the HTTP edge, A-758).
fn batch_answer(
  store: &impl Store,
  transport: Transport,
  cmds: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let mut batch = Batch::begin(cmds, admission)?;
  match batch.step(store, transport, admission)? {
    BatchStep::Answered(answer) => {
      out.extend_from_slice(&answer);
      Ok(())
    }
    BatchStep::Push(_) => Err(Refused::NotServed),
  }
}

/// One call of a [`Batch`], owned: its command and its arguments, unescaped.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedCall {
  /// The command.
  command: Vec<u8>,
  /// The arguments, name and value, in order.
  arguments: Vec<(Vec<u8>, Vec<u8>)>,
}

/// A `batch` answered a call at a time, in order, as 7.2's `wireprotov1server.batch` runs its calls (A-569, A-758): a
/// call that answers a string is answered from the store it is stepped over, and a `pushkey` call is handed to the
/// caller as the push it is landed as ([`super::keys`]), whose outcome [`Batch::pushed`] takes as that call's answer
/// before the next call is answered, so a call after it reads what it moved. Its calls and answers are charged to the
/// admission each step is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
  /// The calls.
  calls: Vec<OwnedCall>,
  /// The next call to answer.
  next: usize,
  /// The answers so far, in order, unescaped.
  results: Vec<Vec<u8>>,
}

/// What a [`Batch`]'s step came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchStep {
  /// A `pushkey` call's push, for the caller to land and hand back to [`Batch::pushed`] (the bundle [`super::keys::bundle`]
  /// writes).
  Push(Vec<u8>),
  /// Every call answered: the batch's answer, each answer escaped, joined by `;`.
  Answered(Vec<u8>),
}

impl Batch {
  /// The batch `cmds` lists, its calls read and copied within `admission`.
  pub fn begin(cmds: &[u8], admission: &mut Admission) -> Result<Batch, Refused> {
    // The owned copies take at most the escaped bytes again, charged before the calls are read.
    admission.charge(crate::read::to_u64(cmds.len()))?;
    let mut buffer = Vec::new();
    let calls = batch::calls(cmds, admission, &mut buffer)?;
    admission.charge_items::<OwnedCall>(calls.len())?;
    admission.charge_items::<Vec<u8>>(calls.len())?;
    let calls = calls
      .iter()
      .map(|call| OwnedCall {
        command: call.command.to_vec(),
        arguments: call
          .arguments
          .iter()
          .map(|(name, value)| (name.to_vec(), value.to_vec()))
          .collect(),
      })
      .collect::<Vec<_>>();
    Ok(Batch {
      results: Vec::with_capacity(calls.len()),
      calls,
      next: 0,
    })
  }

  /// Answers the calls from the next one on over `store`, within `admission`, until a `pushkey` call needs its push
  /// landed or every call is answered. A call that cannot be answered refuses the batch, as 7.2's does.
  pub fn step(
    &mut self,
    store: &impl Store,
    transport: Transport,
    admission: &mut Admission,
  ) -> Result<BatchStep, Refused> {
    while let Some(call) = self.calls.get(self.next) {
      if call.command == b"batch" || call.command == b"getbundle" {
        return Err(Refused::NotServed);
      }
      let argument = |name: &[u8]| {
        call
          .arguments
          .iter()
          .find(|(key, _)| key.as_slice() == name)
          .map(|(_, value)| value.as_slice())
      };
      let mut result = Vec::new();
      if call.command == keys::COMMAND {
        let request = keys::request_by(argument)?;
        match keys::bundle(&request, admission) {
          Ok(bundle) => return Ok(BatchStep::Push(bundle)),
          Err(refusal) => {
            result = keys::refused(refusal, &request, admission)?;
          }
        }
      } else {
        string_answer(
          store,
          transport,
          &call.command,
          argument,
          admission,
          &mut result,
        )?;
      }
      self.results.push(result);
      self.next = self.next.saturating_add(1);
    }
    let total = self.results.iter().fold(0_u64, |total, result| {
      total.saturating_add(
        crate::read::to_u64(result.len())
          .saturating_mul(ESCAPE_GROWTH)
          .saturating_add(1),
      )
    });
    admission.charge(total)?;
    let views: Vec<&[u8]> = self.results.iter().map(Vec::as_slice).collect();
    let mut out = Vec::new();
    batch::encode_results(&views, &mut out);
    Ok(BatchStep::Answered(out))
  }

  /// Takes what the push of the `pushkey` call [`Batch::step`] handed out came to as that call's answer
  /// ([`super::keys::answer`]), charged to `admission`; the next step answers the call after it.
  pub fn pushed(&mut self, outcome: &Outcome, admission: &mut Admission) -> Result<(), Refused> {
    let answer = keys::answer(outcome, admission)?;
    self.results.push(answer);
    self.next = self.next.saturating_add(1);
    Ok(())
  }
}

/// Answers a `getbundle`: its stream; a refusal of a bundle2 request as an `ERROR:ABORT` part, sent uncompressed; of a
/// bundle1 request out of band, over HTTP in 7.2's own words ([`BUNDLE2_REQUIRED`]).
fn getbundle_answer(
  store: &impl Store,
  transport: Transport,
  arguments: &Arguments<'_>,
  admission: &mut Admission,
) -> (Answer, Option<Refused>) {
  let asked = getbundle_asked(arguments);
  let mut stream = Vec::new();
  match getbundle(store, &asked, admission, &mut stream) {
    Ok(()) => {
      let answer = Answer::Stream {
        stream,
        prefer_uncompressed: false,
      };
      (answer, None)
    }
    Err(refusal) => getbundle_refused(refusal, transport),
  }
}

/// What a `getbundle`'s arguments ask.
pub(crate) fn getbundle_asked<'a>(arguments: &Arguments<'a>) -> Asked<'a> {
  Asked {
    heads: arguments.entry(b"heads").unwrap_or_default(),
    common: arguments.entry(b"common").unwrap_or_default(),
    bundlecaps: arguments.entry(b"bundlecaps").unwrap_or_default(),
    cg: arguments.entry(b"cg"),
    phases: arguments.entry(b"phases"),
    bookmarks: arguments.entry(b"bookmarks"),
    obsmarkers: arguments.entry(b"obsmarkers"),
    listkeys: arguments.entry(b"listkeys"),
  }
}

/// The answer to a `getbundle` refused before its stream began: a bundle2 request's as an `ERROR:ABORT` part, sent
/// uncompressed; a bundle1 request's out of band, over HTTP in 7.2's own words ([`BUNDLE2_REQUIRED`]).
pub(crate) fn getbundle_refused(
  refusal: Refusal,
  transport: Transport,
) -> (Answer, Option<Refused>) {
  let refused = Refused::Serve(refusal);
  let words = match (refusal, transport) {
    (Refusal::NoBundle2, Transport::Http { .. }) => BUNDLE2_REQUIRED.to_vec(),
    _ => message(&refused),
  };
  if refusal == Refusal::NoBundle2 {
    return (Answer::OutOfBand(words), Some(refused));
  }
  let mut abort = Vec::new();
  match abort_stream(&words, &mut abort) {
    Ok(()) => {
      let answer = Answer::Stream {
        stream: abort,
        prefer_uncompressed: true,
      };
      (answer, Some(refused))
    }
    Err(_) => (Answer::OutOfBand(words), Some(refused)),
  }
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
pub(super) fn message(refused: &Refused) -> Vec<u8> {
  let words = match refused {
    Refused::Request(refusal) => format!("unreadable request: {refusal:?}"),
    Refused::Batch(refusal) => format!("unreadable batch: {refusal:?}"),
    Refused::NotServed => "command not served here".to_owned(),
    Refused::MissingArgument { name } => {
      format!("missing argument {}", String::from_utf8_lossy(name))
    }
    Refused::Serve(refusal) => serve_message(refusal),
    Refused::Http(refusal) => format!("unreadable request: {refusal:?}"),
    Refused::NotAvailable => {
      "requested wire protocol command is not available over HTTP".to_owned()
    }
    Refused::Compression(refusal) => format!("the answer could not be compressed: {refusal:?}"),
    Refused::EngineNotRead { engine } => {
      format!("the answer's engine {engine:?} is not one the client reads")
    }
    Refused::PastWindow { needed, capacity } => {
      format!("a request of {needed} bytes is past the channel's window of {capacity}")
    }
    Refused::Push => "push refused".to_owned(),
    Refused::Pushkey(refusal) => format!("pushkey refused: {refusal:?}"),
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
