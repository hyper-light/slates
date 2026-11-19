//! The command request (A-582, part 2; gitprotocol-v2 "Command Request" [B]; `serve.c` `process_request`,
//! `parse_command` and `receive_client_capability` [C]).
//!
//! ```text
//! request = empty-request | command-request
//! empty-request = flush-pkt
//! command-request = command capability-list delim-pkt command-args flush-pkt
//! command = PKT-LINE("command=" key LF)
//! ```
//!
//! `serve.c` reads more than the grammar shows, and stock clients use it: the `command=` line may come after the
//! capabilities as well as before them (git's `bundle-uri` client sends its capabilities first, `connect.c`
//! `get_remote_bundle_uri`), and a flush may close the request right after them, a command with no arguments. A
//! request's head is therefore kept as its lines came ([`Head`]), with exactly one `command=` line among them. The
//! capabilities a client may send are the ones `serve.c` receives: `agent`, `server-option`, `object-format`,
//! `session-id` and `promisor-remote`; a command's name there, or any other key, is refused ("unknown capability"), as
//! is `object-format` without a value or naming a format git does not know. The commands are `ls-refs` and `fetch`;
//! any other is refused ("invalid command"): `object-info` and `bundle-uri` are a later part, and a server must not
//! advertise them before it reads them. The arguments' ids are in the format the last `object-format` names, SHA-1
//! when none does, as `serve.c` keeps the last.
//!
//! A server reads a request as it arrives: [`whole_len`] says when the bytes so far hold a whole one, since the first
//! flush always ends a request. Nothing caps a request's size but its bytes and the caller's admission.

use super::line::{self, Line, Packets, Part};
use super::{Capability, Refusal, advertisement, fetch, ls_refs};
use crate::object_name::ObjectFormat;
use crate::out::{Measure, Out};
use crate::pktline::{self, Packet};
use crate::{Admission, to_u64};

/// Format: what a request's command line begins with [gitprotocol-v2: `command = PKT-LINE("command=" key LF)`].
const COMMAND: &[u8] = b"command=";

/// Format: the capabilities a client may send, `serve.c`'s `capabilities[]` that are not commands.
const CLIENT_CAPABILITIES: [&[u8]; 5] = [
  b"agent",
  b"server-option",
  advertisement::OBJECT_FORMAT,
  b"session-id",
  b"promisor-remote",
];

/// Format: each command this crate reads, and its name [gitprotocol-v2 "ls-refs", "fetch"].
const COMMANDS: [(Command, &[u8]); 2] = [(Command::LsRefs, b"ls-refs"), (Command::Fetch, b"fetch")];

/// A request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request<'a> {
  /// A flush alone: the client will make no more requests.
  End,
  /// A command and its arguments.
  Command(CommandRequest<'a>),
}

/// A command's request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRequest<'a> {
  /// The lines before the arguments, as they came: one `command=` line and the client's capabilities.
  pub head: Vec<Line<Head<'a>>>,
  /// Whether a delimiter opened the arguments. A request whose flush follows its head has none, and no arguments.
  pub delimited: bool,
  /// The command, and its arguments as they came.
  pub arguments: Arguments<'a>,
}

/// A line of a request's head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head<'a> {
  /// The `command=` line, naming the command whose arguments [`CommandRequest::arguments`] holds.
  Command,
  /// A capability the client sends.
  Capability(Capability<'a>),
}

/// A command this crate reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
  /// `ls-refs`: list refs.
  LsRefs,
  /// `fetch`: negotiate, and send a pack.
  Fetch,
}

impl Command {
  /// The command's name.
  pub fn name(self) -> &'static [u8] {
    COMMANDS
      .iter()
      .find(|(command, _)| *command == self)
      .map_or(b"", |(_, name)| *name)
  }

  /// The command `name` names.
  pub fn from_name(name: &[u8]) -> Option<Command> {
    COMMANDS
      .iter()
      .find(|(_, known)| *known == name)
      .map(|(command, _)| *command)
  }
}

/// A command and its arguments, as they came.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arguments<'a> {
  /// `ls-refs` and its arguments.
  LsRefs(Vec<Line<ls_refs::Argument<'a>>>),
  /// `fetch` and its arguments.
  Fetch(Vec<Line<fetch::Argument<'a>>>),
}

impl Arguments<'_> {
  /// The command.
  pub fn command(&self) -> Command {
    match self {
      Arguments::LsRefs(_) => Command::LsRefs,
      Arguments::Fetch(_) => Command::Fetch,
    }
  }

  /// How many arguments there are.
  pub fn len(&self) -> usize {
    match self {
      Arguments::LsRefs(arguments) => arguments.len(),
      Arguments::Fetch(arguments) => arguments.len(),
    }
  }

  /// Whether there are none.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

impl<'a> CommandRequest<'a> {
  /// The command.
  pub fn command(&self) -> Command {
    self.arguments.command()
  }

  /// The client's capabilities, as they came.
  pub fn capabilities(&self) -> impl Iterator<Item = Capability<'a>> + use<'_, 'a> {
    self.head.iter().filter_map(|line| match line.value {
      Head::Capability(capability) => Some(capability),
      Head::Command => None,
    })
  }

  /// The format the request's ids are in: the last `object-format`'s, SHA-1 when none names one; `None` when the last
  /// names none git knows.
  pub fn object_format(&self) -> Option<ObjectFormat> {
    match self
      .capabilities()
      .filter(|capability| capability.key == advertisement::OBJECT_FORMAT)
      .last()
    {
      None => Some(ObjectFormat::Sha1),
      Some(capability) => ObjectFormat::from_name(capability.value?).ok(),
    }
  }
}

/// The capability a head line's text writes, refused when it is none a client sends; and the object format it names,
/// when it is `object-format`.
fn client_capability(
  text: &[u8],
  at: u64,
) -> Result<(Capability<'_>, Option<ObjectFormat>), Refusal> {
  let capability = Capability::parse(text, at)?;
  check_client_capability(&capability, at)
}

/// Refused when `capability` is none a client sends, or an `object-format` naming no format: the format it names.
fn check_client_capability<'a>(
  capability: &Capability<'a>,
  at: u64,
) -> Result<(Capability<'a>, Option<ObjectFormat>), Refusal> {
  capability.check(at)?;
  if !CLIENT_CAPABILITIES.contains(&capability.key) {
    return Err(Refusal::UnknownCapability { at });
  }
  if capability.key != advertisement::OBJECT_FORMAT {
    return Ok((*capability, None));
  }
  let format = capability
    .value
    .and_then(|name| ObjectFormat::from_name(name).ok())
    .ok_or(Refusal::ObjectFormat { at })?;
  Ok((*capability, Some(format)))
}

/// The command a `command=` line names.
fn command(name: &[u8], at: u64) -> Result<Command, Refusal> {
  Command::from_name(name).ok_or(Refusal::UnknownCommand { at })
}

/// What a walk over a request finds, handed over as it finds it.
trait Sink<'a> {
  /// A line of the head.
  fn head(&mut self, line: Line<Head<'a>>);
  /// An `ls-refs` argument.
  fn ls_refs(&mut self, line: Line<ls_refs::Argument<'a>>);
  /// A `fetch` argument.
  fn fetch(&mut self, line: Line<fetch::Argument<'a>>);
}

/// A sink that counts what it is handed.
#[derive(Default)]
struct Count {
  /// Head lines.
  head: usize,
  /// Arguments.
  arguments: usize,
}

impl<'a> Sink<'a> for Count {
  fn head(&mut self, _: Line<Head<'a>>) {
    self.head = self.head.saturating_add(1);
  }

  fn ls_refs(&mut self, _: Line<ls_refs::Argument<'a>>) {
    self.arguments = self.arguments.saturating_add(1);
  }

  fn fetch(&mut self, _: Line<fetch::Argument<'a>>) {
    self.arguments = self.arguments.saturating_add(1);
  }
}

/// A sink that keeps what it is handed.
struct Build<'a> {
  /// The head's lines.
  head: Vec<Line<Head<'a>>>,
  /// The `ls-refs` arguments.
  ls_refs: Vec<Line<ls_refs::Argument<'a>>>,
  /// The `fetch` arguments.
  fetch: Vec<Line<fetch::Argument<'a>>>,
}

impl<'a> Sink<'a> for Build<'a> {
  fn head(&mut self, line: Line<Head<'a>>) {
    self.head.push(line);
  }

  fn ls_refs(&mut self, line: Line<ls_refs::Argument<'a>>) {
    self.ls_refs.push(line);
  }

  fn fetch(&mut self, line: Line<fetch::Argument<'a>>) {
    self.fetch.push(line);
  }
}

/// What a request's head held, once it is read.
#[derive(Clone, Copy, Debug)]
struct HeadEnd {
  /// The command, if a `command=` line came.
  command: Option<Command>,
  /// Whether any line came.
  seen: bool,
  /// The format the last `object-format` named, SHA-1 when none did.
  format: ObjectFormat,
  /// Whether a delimiter, not a flush, closed the head.
  delimited: bool,
  /// Where the packet closing the head begins.
  at: u64,
}

/// The shape of a request a walk found.
#[derive(Clone, Copy, Debug)]
enum Shape {
  /// A flush alone.
  End,
  /// A command; whether its arguments were delimited.
  Command(Command, bool),
}

/// Reads a request's head, handing each line to `sink`.
fn read_head<'a>(packets: &mut Packets<'a>, sink: &mut impl Sink<'a>) -> Result<HeadEnd, Refusal> {
  let mut end = HeadEnd {
    command: None,
    seen: false,
    format: ObjectFormat::Sha1,
    delimited: false,
    at: 0,
  };
  loop {
    let at = packets.at();
    match packets.next_packet()? {
      None => return Err(Refusal::Unfinished { at }),
      Some((Packet::Data(data), at)) => head_line(data, at, &mut end, sink)?,
      Some((Packet::Delim, at)) => {
        return Ok(HeadEnd {
          delimited: true,
          at,
          ..end
        });
      }
      Some((Packet::Flush, at)) => return Ok(HeadEnd { at, ..end }),
      Some((Packet::ResponseEnd, at)) => return Err(Refusal::Misplaced { at }),
    }
  }
}

/// Reads one head line into `end` and `sink`.
fn head_line<'a>(
  data: &'a [u8],
  at: u64,
  end: &mut HeadEnd,
  sink: &mut impl Sink<'a>,
) -> Result<(), Refusal> {
  let (text, ending) = line::text(data, at)?;
  end.seen = true;
  let value = match text.strip_prefix(COMMAND) {
    Some(name) => {
      if end.command.is_some() {
        return Err(Refusal::SecondCommand { at });
      }
      end.command = Some(command(name, at)?);
      Head::Command
    }
    None => {
      let (capability, format) = client_capability(text, at)?;
      end.format = format.unwrap_or(end.format);
      Head::Capability(capability)
    }
  };
  sink.head(Line { value, ending });
  Ok(())
}

/// Reads a command's arguments, up to the flush closing them, handing each to `sink`.
fn read_arguments<'a>(
  packets: &mut Packets<'a>,
  command: Command,
  format: ObjectFormat,
  sink: &mut impl Sink<'a>,
) -> Result<(), Refusal> {
  let mut rules = fetch::Rules::default();
  while let Some((text, ending, at)) = packets.next_line()? {
    match command {
      Command::LsRefs => sink.ls_refs(Line {
        value: ls_refs::Argument::parse(text, at)?,
        ending,
      }),
      Command::Fetch => {
        let value = fetch::Argument::parse(text, format, at)?;
        rules.admit(&value, at)?;
        sink.fetch(Line { value, ending });
      }
    }
  }
  Ok(())
}

/// Walks the request in `bytes`, handing what it finds to `sink`: the request's shape.
fn walk<'a>(bytes: &'a [u8], sink: &mut impl Sink<'a>) -> Result<Shape, Refusal> {
  let mut packets = Packets::new(bytes);
  let head = read_head(&mut packets, sink)?;
  let shape = match (head.command, head.seen, head.delimited) {
    (None, false, false) => Shape::End,
    (None, _, _) => return Err(Refusal::NoCommand { at: head.at }),
    (Some(command), _, false) => Shape::Command(command, false),
    (Some(command), _, true) => {
      read_arguments(&mut packets, command, head.format, sink)?;
      Shape::Command(command, true)
    }
  };
  packets.finish()?;
  Ok(shape)
}

/// Decodes the request `bytes` hold, whole: refused when a line is none its place takes, a packet comes where the
/// request has none, the bytes end before its flush or go on after it; or when its lists would take more than
/// `admission` has left, charged before they are made.
pub fn decode<'a>(bytes: &'a [u8], admission: &mut Admission) -> Result<Request<'a>, Refusal> {
  let mut count = Count::default();
  let shape = walk(bytes, &mut count)?;
  let argument = match shape {
    Shape::Command(Command::LsRefs, _) => core::mem::size_of::<Line<ls_refs::Argument<'a>>>(),
    Shape::Command(Command::Fetch, _) => core::mem::size_of::<Line<fetch::Argument<'a>>>(),
    Shape::End => 0,
  };
  let head = core::mem::size_of::<Line<Head<'a>>>().saturating_mul(count.head);
  admission
    .charge(to_u64(
      head.saturating_add(argument.saturating_mul(count.arguments)),
    ))
    .map_err(Refusal::PastAdmission)?;
  let (ls_refs, fetch) = match shape {
    Shape::Command(Command::LsRefs, _) => (count.arguments, 0),
    Shape::Command(Command::Fetch, _) => (0, count.arguments),
    Shape::End => (0, 0),
  };
  let mut build = Build {
    head: Vec::with_capacity(count.head),
    ls_refs: Vec::with_capacity(ls_refs),
    fetch: Vec::with_capacity(fetch),
  };
  walk(bytes, &mut build)?;
  Ok(match shape {
    Shape::End => Request::End,
    Shape::Command(command, delimited) => Request::Command(CommandRequest {
      head: build.head,
      delimited,
      arguments: match command {
        Command::LsRefs => Arguments::LsRefs(build.ls_refs),
        Command::Fetch => Arguments::Fetch(build.fetch),
      },
    }),
  })
}

/// The length of the first whole request `bytes` begin with, or `None` while they hold less than one: a server
/// reading a request as it arrives asks again as more comes. Refused when the bytes are not pkt-lines.
pub fn whole_len(bytes: &[u8]) -> Result<Option<usize>, Refusal> {
  let mut at = 0usize;
  loop {
    let rest = bytes.get(at..).unwrap_or_default();
    match pktline::read(rest, to_u64(at)).map_err(Refusal::Packet)? {
      None => return Ok(None),
      Some((Packet::Flush, took)) => return Ok(Some(at.saturating_add(took))),
      Some((_, took)) => at = at.saturating_add(took),
    }
  }
}

/// Writes `request` as it came, each line as it ended. Refused, nothing written, when its head holds no `command=`
/// line or two, a capability no client sends or an `object-format` naming no format, an argument with no delimiter
/// before it, an argument value it may not take, or a line longer than a packet carries.
pub fn encode(request: &Request<'_>, out: &mut impl Out) -> Result<(), Refusal> {
  write(request, &mut Measure::default())?;
  write(request, out)
}

/// Writes `request` to `out`, checking each line before it writes it.
fn write(request: &Request<'_>, out: &mut impl Out) -> Result<(), Refusal> {
  match request {
    Request::End => {
      line::put_special(Packet::Flush, out);
      Ok(())
    }
    Request::Command(request) => {
      let (at, format) = write_head(request, out)?;
      write_arguments(request, at, format, out)
    }
  }
}

/// Writes a request's head, then its delimiter where it has one: where the next packet begins, and the format of the
/// arguments' ids.
fn write_head(
  request: &CommandRequest<'_>,
  out: &mut impl Out,
) -> Result<(u64, ObjectFormat), Refusal> {
  let mut at = 0u64;
  let mut format = ObjectFormat::Sha1;
  let mut commands = 0usize;
  for line in &request.head {
    let parts: [&[u8]; 4] = match line.value {
      Head::Command => {
        commands = commands.saturating_add(1);
        if commands > 1 {
          return Err(Refusal::SecondCommand { at });
        }
        [COMMAND, request.command().name(), b"", line.ending.bytes()]
      }
      Head::Capability(capability) => {
        let (_, named) = check_client_capability(&capability, at)?;
        format = named.unwrap_or(format);
        let [key, assign, value] = capability.parts();
        [key, assign, value, line.ending.bytes()]
      }
    };
    at = at.saturating_add(line::put_line(None, &parts.map(Part::Bytes), at, out)?);
  }
  if commands == 0 {
    return Err(Refusal::NoCommand { at });
  }
  if request.delimited {
    at = at.saturating_add(line::put_special(Packet::Delim, out));
  } else if !request.arguments.is_empty() {
    return Err(Refusal::UnknownArgument { at });
  }
  Ok((at, format))
}

/// Writes a request's arguments from `at` on, then its flush.
fn write_arguments(
  request: &CommandRequest<'_>,
  mut at: u64,
  format: ObjectFormat,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  match &request.arguments {
    Arguments::LsRefs(arguments) => {
      for line in arguments {
        line.value.check(at)?;
        let [word, prefix] = line.value.parts();
        let parts = [
          Part::Bytes(word),
          Part::Bytes(prefix),
          Part::Bytes(line.ending.bytes()),
        ];
        at = at.saturating_add(line::put_line(None, &parts, at, out)?);
      }
    }
    Arguments::Fetch(arguments) => {
      let mut rules = fetch::Rules::default();
      for line in arguments {
        line.value.check(format, at)?;
        rules.admit(&line.value, at)?;
        let [word, space, value] = line.value.parts();
        let parts = [word, space, value, Part::Bytes(line.ending.bytes())];
        at = at.saturating_add(line::put_line(None, &parts, at, out)?);
      }
    }
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

#[cfg(test)]
#[path = "tests/request.rs"]
mod tests;
