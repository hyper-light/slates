//! A push's update request, read as its bytes arrive (A-620 part 1; gitprotocol-pack "Reference Update Request and
//! Packfile Transfer"; git's `read_head_info`, `queue_command` and `read_push_options`, v2.50.1).
//!
//! The grammar: `*shallow ( command-list | push-cert )`, the command list ending in a flush, then the push options and
//! their flush when both sides chose `push-options`, then the pack. Each command is `old-id SP new-id SP refname`, the
//! first followed by a NUL and the client's capabilities. A stock client ends no line with LF; git strips one where it
//! is, and so does this reader.

use super::{Advertised, Refusal, reserve};
use crate::object_name::{ObjectFormat, ObjectId};
use crate::pktline::{self, Packet};
use crate::{Admission, to_u64};

/// Format: what a shallow line begins with (`read_head_info`: `starts_with(reader->line, "shallow ")`).
const SHALLOW: &[u8] = b"shallow ";

/// Format: the line a push certificate begins with, its capabilities after a NUL (gitprotocol-pack: `push-cert`).
const PUSH_CERT: &[u8] = b"push-cert";

/// Format: the byte that ends a command and begins the client's capabilities.
const NUL: u8 = 0;

/// Format: the byte between a command's fields, and between the client's capabilities.
const SPACE: u8 = b' ';

/// Format: the byte a line may end with, which git strips (`PACKET_READ_CHOMP_NEWLINE`).
const LF: u8 = b'\n';

/// Format: what every ref name receive-pack updates begins with (`update`: "only refs/... are allowed").
const REFS: &[u8] = b"refs/";

/// Format: the byte between a ref name's components.
const SLASH: u8 = b'/';

/// Format: no component of a ref name begins with it, and no name ends with it (`check_refname_component`).
const DOT: u8 = b'.';

/// Format: no component of a ref name ends with it (`LOCK_SUFFIX`).
const LOCK: &[u8] = b".lock";

/// Format: the pairs no component of a ref name holds: `..` and `@{` (`refname_disposition` 2 and 3).
const PAIRS: [&[u8]; 2] = [b"..", b"@{"];

/// Format: the length of a pair [`PAIRS`] holds.
const PAIR_LEN: usize = 2;

/// Format: the printable bytes no ref name holds (`refname_disposition` 4 and 5, `*` a pattern's, never a name's).
const FORBIDDEN: &[u8] = b" :?[\\^~*";

/// Format: the last ASCII control byte; every byte up to it is one.
const LAST_CONTROL: u8 = 0x1f;

/// Format: DEL, ASCII's other control byte.
const DEL: u8 = 0x7f;

/// Format: the capability asking for a report (gitprotocol-capabilities; `read_head_info`'s `parse_feature_request`).
const REPORT_STATUS: &[u8] = b"report-status";
/// Format: the capability asking for the report with its option lines.
const REPORT_STATUS_V2: &[u8] = b"report-status-v2";
/// Format: the capability asking for the answer multiplexed.
const SIDE_BAND_64K: &[u8] = b"side-band-64k";
/// Format: the capability asking for no progress.
const QUIET: &[u8] = b"quiet";
/// Format: the capability asking for every ref or none.
const ATOMIC: &[u8] = b"atomic";
/// Format: the capability announcing push options after the commands.
const PUSH_OPTIONS: &[u8] = b"push-options";
/// Format: the byte between a capability's name and its value.
const EQUALS: u8 = b'=';

/// Format: the capabilities a client gives a value, each before its `=`.
const OBJECT_FORMAT: &[u8] = b"object-format";
/// Format: the client's agent.
const AGENT: &[u8] = b"agent";
/// Format: the client's session id.
const SESSION_ID: &[u8] = b"session-id";

/// One command: a ref, the id the client saw it at, and the id it asks for; a zero id for a ref created or deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
  /// The id the client saw the ref at; zero to create it.
  pub old: ObjectId,
  /// The id the client asks for; zero to delete the ref.
  pub new: ObjectId,
  /// The ref's name, as the client wrote it.
  pub name: Vec<u8>,
}

impl Command {
  /// Whether the command creates its ref.
  pub fn is_create(&self) -> bool {
    is_zero(&self.old)
  }

  /// Whether the command deletes its ref.
  pub fn is_delete(&self) -> bool {
    is_zero(&self.new)
  }
}

/// Whether `id` is the zero id of its format.
pub(crate) fn is_zero(id: &ObjectId) -> bool {
  id.as_bytes().iter().all(|byte| *byte == 0)
}

/// What the client asked for on its first command line: the capabilities that change the exchange, those the server
/// advertised alone counted, and the values it gave.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requested {
  /// `report-status`: a report after the pack.
  pub report_status: bool,
  /// `report-status-v2`: the report with its option lines.
  pub report_status_v2: bool,
  /// `side-band-64k`: the answer multiplexed, messages on band 2.
  pub side_band_64k: bool,
  /// `quiet`: no progress.
  pub quiet: bool,
  /// `atomic`, when advertised: every ref or none.
  pub atomic: bool,
  /// `push-options`, when advertised: options follow the commands.
  pub push_options: bool,
  /// `agent=`.
  pub agent: Option<Vec<u8>>,
  /// `session-id=`.
  pub session_id: Option<Vec<u8>>,
}

/// A push's update request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
  /// The hash the repository names its objects with.
  pub format: ObjectFormat,
  /// The commits the client holds only shallow copies of.
  pub shallow: Vec<ObjectId>,
  /// The commands, in the order sent.
  pub commands: Vec<Command>,
  /// What the client asked for.
  pub requested: Requested,
  /// The push options, in the order sent.
  pub options: Vec<Vec<u8>>,
}

impl Request {
  /// Whether a pack follows the request: one does unless every command deletes (gitprotocol-pack: "The packfile MUST
  /// NOT be sent if the only command used is 'delete'"), and none follows an empty request.
  pub fn wants_pack(&self) -> bool {
    self.commands.iter().any(|command| !command.is_delete())
  }
}

/// What the reader reads next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// Shallow lines and commands, up to their flush.
  Commands,
  /// Push options, up to their flush.
  Options,
  /// Nothing: the request is whole.
  Done,
}

/// A push's update request, read as its bytes arrive.
pub struct RequestReader<'a> {
  /// What the server advertised: the repository's format, and which capabilities a client may turn on.
  advertised: &'a Advertised<'a>,
  /// What comes next.
  stage: Stage,
  /// The bytes read so far.
  at: u64,
  /// The first bytes of a packet whose bytes have not all arrived, at most git's 65520.
  partial: Vec<u8>,
  /// The request so far.
  request: Request,
}

impl<'a> RequestReader<'a> {
  /// A reader of a request made to a server that advertised `advertised`.
  pub fn new(advertised: &'a Advertised<'a>) -> Self {
    Self {
      advertised,
      stage: Stage::Commands,
      at: 0,
      partial: Vec::new(),
      request: Request {
        format: advertised.format,
        shallow: Vec::new(),
        commands: Vec::new(),
        requested: Requested::default(),
        options: Vec::new(),
      },
    }
  }

  /// Reads `bytes`, the next bytes of the push, as far as the request goes, charging what it keeps to `admission`: how
  /// many it took. It takes none past the request's last flush, where the pack begins.
  pub fn read(&mut self, bytes: &[u8], admission: &mut Admission) -> Result<usize, Refusal> {
    let mut used = 0usize;
    while self.stage != Stage::Done {
      let rest = bytes.get(used..).unwrap_or_default();
      if rest.is_empty() {
        break;
      }
      used = used.saturating_add(self.step(rest, admission)?);
    }
    Ok(used)
  }

  /// Whether the request is whole.
  pub fn is_whole(&self) -> bool {
    self.stage == Stage::Done
  }

  /// The request, once whole; refused when its bytes ended before its last flush.
  pub fn request(self) -> Result<Request, Refusal> {
    if self.stage != Stage::Done {
      return Err(Refusal::Truncated { at: self.at });
    }
    Ok(self.request)
  }

  /// Reads one packet from `rest`, or as much of one as has arrived: how many bytes it took. A packet whole in `rest` is
  /// read in place; only one whose bytes have not all arrived is kept, and topped up by later calls.
  fn step(&mut self, rest: &[u8], admission: &mut Admission) -> Result<usize, Refusal> {
    if self.partial.is_empty() {
      let at = self.at;
      let Some((packet, len)) = pktline::read(rest, at).map_err(Refusal::PktLine)? else {
        reserve(&mut self.partial, rest.len())?;
        self.partial.extend_from_slice(rest);
        self.at = self.at.saturating_add(to_u64(rest.len()));
        return Ok(rest.len());
      };
      self.at = self.at.saturating_add(to_u64(len));
      self.packet(packet, at, admission)?;
      return Ok(len);
    }
    let begun = self.partial.len();
    let room = pktline::MAX_LEN.saturating_sub(begun).max(1);
    let take = room.min(rest.len());
    reserve(&mut self.partial, take)?;
    self
      .partial
      .extend_from_slice(rest.get(..take).unwrap_or_default());
    let began = self.at.saturating_sub(to_u64(begun));
    let partial = core::mem::take(&mut self.partial);
    let read = pktline::read(&partial, began).map_err(Refusal::PktLine)?;
    let Some((packet, len)) = read else {
      self.partial = partial;
      self.at = self.at.saturating_add(to_u64(take));
      return Ok(take);
    };
    let mine = len.saturating_sub(begun);
    self.at = self.at.saturating_add(to_u64(mine));
    self.packet(packet, began, admission)?;
    Ok(mine)
  }

  /// One whole packet, which began `at` bytes into the push.
  fn packet(
    &mut self,
    packet: Packet<'_>,
    at: u64,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    match (self.stage, packet) {
      (Stage::Commands, Packet::Flush) => {
        self.stage = if self.request.requested.push_options {
          Stage::Options
        } else {
          Stage::Done
        };
        Ok(())
      }
      (Stage::Options, Packet::Flush) => {
        self.stage = Stage::Done;
        Ok(())
      }
      (Stage::Commands, Packet::Data(line)) => self.line(chomp(line), at, admission),
      (Stage::Options, Packet::Data(line)) => {
        let option = owned(chomp(line), admission)?;
        push_charged(&mut self.request.options, option, admission)
      }
      _ => Err(Refusal::UnexpectedPacket { at }),
    }
  }

  /// One line of the command list: a shallow line, or a command, the first with the client's capabilities.
  fn line(&mut self, line: &[u8], at: u64, admission: &mut Admission) -> Result<(), Refusal> {
    if let Some(hex) = line.strip_prefix(SHALLOW) {
      if !self.request.commands.is_empty() {
        return Err(Refusal::Shallow { at });
      }
      let id = ObjectId::from_hex(self.request.format, hex).map_err(|_| Refusal::Shallow { at })?;
      return push_charged(&mut self.request.shallow, id, admission);
    }
    if line == PUSH_CERT
      || line
        .strip_prefix(PUSH_CERT)
        .is_some_and(|rest| rest.first() == Some(&NUL))
    {
      return Err(Refusal::PushCert { at });
    }
    let (text, capabilities) = match line.iter().position(|byte| *byte == NUL) {
      Some(nul) => (
        line.get(..nul).unwrap_or_default(),
        line.get(nul.saturating_add(1)..),
      ),
      None => (line, None),
    };
    let first = self.request.commands.is_empty();
    match (first, capabilities) {
      (true, capabilities) => self.capabilities(capabilities.unwrap_or_default(), at)?,
      (false, Some(_)) => return Err(Refusal::Capabilities { at }),
      (false, None) => {}
    }
    let command = command(text, self.request.format, at, admission)?;
    push_charged(&mut self.request.commands, command, admission)
  }

  /// The client's capabilities, from its first command line: those the server advertised turn on; an `object-format`
  /// other than the repository's (SHA-1 when it names none) is refused, as git refuses it.
  fn capabilities(&mut self, list: &[u8], at: u64) -> Result<(), Refusal> {
    let mut format = None;
    for word in list
      .split(|byte| *byte == SPACE)
      .filter(|word| !word.is_empty())
    {
      let (name, value) = match word.iter().position(|byte| *byte == EQUALS) {
        Some(equals) => (
          word.get(..equals).unwrap_or_default(),
          word.get(equals.saturating_add(1)..),
        ),
        None => (word, None),
      };
      match (name, value) {
        (name, Some(value)) if name == OBJECT_FORMAT => format = Some(value),
        (name, Some(value)) if name == AGENT => self.request.requested.agent = Some(value.to_vec()),
        (name, Some(value)) if name == SESSION_ID => {
          self.request.requested.session_id = Some(value.to_vec())
        }
        (name, None) => self.flag(name),
        _ => {}
      }
    }
    let named = format.unwrap_or(ObjectFormat::Sha1.name().as_bytes());
    if ObjectFormat::from_name(named) != Ok(self.request.format) {
      return Err(Refusal::ObjectFormat { at });
    }
    Ok(())
  }

  /// A capability asked for by name alone: on when it is one git's receive-pack honours and, for `atomic` and
  /// `push-options`, one the server advertised; any other is passed over, as git passes it over.
  fn flag(&mut self, name: &[u8]) {
    let requested = &mut self.request.requested;
    let advertised = self.advertised;
    let flag = match name {
      name if name == REPORT_STATUS => &mut requested.report_status,
      name if name == REPORT_STATUS_V2 => &mut requested.report_status_v2,
      name if name == SIDE_BAND_64K => &mut requested.side_band_64k,
      name if name == QUIET => &mut requested.quiet,
      name if name == ATOMIC && advertised.atomic => &mut requested.atomic,
      name if name == PUSH_OPTIONS && advertised.push_options => &mut requested.push_options,
      _ => return,
    };
    *flag = true;
  }
}

/// `line` without the LF it may end with.
fn chomp(line: &[u8]) -> &[u8] {
  line.strip_suffix(&[LF]).unwrap_or(line)
}

/// A command's text, `old-id SP new-id SP refname`, in `format`.
fn command(
  text: &[u8],
  format: ObjectFormat,
  at: u64,
  admission: &mut Admission,
) -> Result<Command, Refusal> {
  let refused = Refusal::Command { at };
  let hex = format.hex_len();
  let old = text.get(..hex).ok_or(refused)?;
  let new_from = hex.saturating_add(1);
  let new_to = new_from.saturating_add(hex);
  let spaces = [text.get(hex), text.get(new_to)];
  if spaces != [Some(&SPACE), Some(&SPACE)] {
    return Err(refused);
  }
  let new = text.get(new_from..new_to).ok_or(refused)?;
  let name = text.get(new_to.saturating_add(1)..).unwrap_or_default();
  Ok(Command {
    old: ObjectId::from_hex(format, old).map_err(|_| refused)?,
    new: ObjectId::from_hex(format, new).map_err(|_| refused)?,
    name: owned(name, admission)?,
  })
}

/// `bytes` kept, charged to `admission` first.
fn owned(bytes: &[u8], admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
  admission
    .charge(to_u64(bytes.len()))
    .map_err(Refusal::PastAdmission)?;
  let mut kept = Vec::new();
  reserve(&mut kept, bytes.len())?;
  kept.extend_from_slice(bytes);
  Ok(kept)
}

/// Pushes `value` onto `list`, growing it, when full, to twice its room (at least one): the room added charged to
/// `admission` before it is made, so what the list holds is what was charged.
pub(super) fn push_charged<T>(
  list: &mut Vec<T>,
  value: T,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  if list.len() == list.capacity() {
    let more = list.capacity().max(1);
    admission
      .charge_items::<T>(more)
      .map_err(super::Refusal::PastAdmission)?;
    reserve(list, more)?;
  }
  list.push(value);
  Ok(())
}

/// Whether receive-pack updates a ref of this name (`update`): it begins `refs/`, passes git's `check_refname_format`
/// (the rule A-615 makes one for the crate: no empty component, none beginning with `.` or ending with `.lock`, no `..`,
/// `@{`, control byte, DEL, space, `:`, `?`, `[`, `\`, `^`, `~` or `*`, and not ending with `.`), and names two
/// components after `refs/` unless the command deletes it (`REFNAME_ALLOW_ONELEVEL` for a delete alone).
pub(crate) fn refname_ok(name: &[u8], delete: bool) -> bool {
  let Some(rest) = name.strip_prefix(REFS) else {
    return false;
  };
  let components_ok = rest.split(|byte| *byte == SLASH).all(component_ok);
  let levels_ok = delete || rest.contains(&SLASH);
  components_ok && levels_ok && name.last() != Some(&DOT)
}

/// Whether one component of a ref name passes git's rule.
fn component_ok(component: &[u8]) -> bool {
  !component.is_empty()
    && component.first() != Some(&DOT)
    && !component.ends_with(LOCK)
    && !component
      .windows(PAIR_LEN)
      .any(|pair| PAIRS.contains(&pair))
    && !component.iter().any(|byte| forbidden(*byte))
}

/// Whether `byte` is one no ref name holds: a control byte, DEL, or `refname_disposition`'s forbidden printable ones.
fn forbidden(byte: u8) -> bool {
  byte <= LAST_CONTROL || byte == DEL || FORBIDDEN.contains(&byte)
}
