//! `fetch` (A-582, part 2; gitprotocol-v2 "fetch" [B]; `upload-pack.c` `process_args`, `send_acks`,
//! `send_shallow_info`, `send_wanted_ref_info`, `create_pack_file`, and `fetch-pack.c` `send_fetch_request`,
//! `do_fetch_pack_v2`, `process_ack` [C]): its arguments, and its response's sections.
//!
//! **Arguments.** `want <id>`, `want-ref <ref>`, `have <id>`, `done`, `thin-pack`, `no-progress`, `include-tag`,
//! `ofs-delta`, `shallow <id>`, `deepen <depth>`, `deepen-since <time>`, `deepen-not <rev>`, `deepen-relative`, `filter
//! <spec>`, `packfile-uris <protocols>`, `sideband-all` and `wait-for-done`, each a whole line, in any order; anything
//! else is refused, as `process_args` dies on it ("unexpected line"). A depth is git's `int`, positive
//! (`process_deepen`); a time is git's `timestamp_t`, all but its largest, which revision.c reads as "no limit"
//! (`process_deepen_since`). `filter` and `packfile-uris` come at most once, as git dies on a second of each, and
//! `deepen` with `deepen-since` or `deepen-not` is refused, which the document says "Cannot be used with" it. A
//! filter's spec, a rev and a list of protocols are kept as sent, for the pieces that read them; each must name
//! something.
//!
//! **The response**, in the order the grammar gives:
//!
//! ```text
//! output = acknowledgements flush-pkt |
//!          [acknowledgments delim-pkt] [shallow-info delim-pkt]
//!          [wanted-refs delim-pkt] [packfile-uris delim-pkt]
//!          packfile flush-pkt
//! acknowledgments = PKT-LINE("acknowledgments" LF) (nak | *ack) (ready)
//! shallow-info = PKT-LINE("shallow-info" LF) *PKT-LINE((shallow | unshallow) LF)
//! wanted-refs = PKT-LINE("wanted-refs" LF) *PKT-LINE(wanted-ref LF)
//! packfile-uris = PKT-LINE("packfile-uris" LF) *packfile-uri
//! packfile = PKT-LINE("packfile" LF) *PKT-LINE(%x01-03 *%x00-ff)
//! ```
//!
//! An acknowledgments section holds `NAK` or `ACK`s, never both, then `ready` where the server is ready; after `ready`
//! a delimiter and the pack follow, and without it a flush ends the response (`fetch-pack.c` `process_ack`). The
//! packfile section is always multiplexed on side-band-64k's bands; under `sideband-all` every data packet of the
//! response is, and a progress packet (band 2) may come anywhere, so the server can keep the client waiting. An empty
//! band-1 packet is no keepalive outside the packfile: git's own client reads it as an empty line, where a section's
//! name must stand (git 2.50.1's `create_pack_file` sends `0005\1` while it waits; the document's keepalive is
//! `0005\2`). A band-3 packet is fatal, and nothing follows it.
//!
//! One machine, [`Response`], holds the grammar. A reader steps it with each packet ([`Response::read`]) and a writer
//! with each event before it writes it ([`Response::write`]), so a section out of order is refused both ways: git
//! 2.50.1's `upload_pack_v2` wrote `wanted-refs` before `shallow-info`, which its own client refused, until
//! `191740edaa4a` swapped them.

use super::line::{self, Ending, Line, Packets, Part, SP};
use super::{Refusal, refname};
use crate::Admission;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::out::{Measure, Out};
use crate::pktline::Packet;
use crate::sideband::{self, Band};

/// Format: the argument naming an object the client wants [gitprotocol-v2 "fetch"].
const WANT: &[u8] = b"want";

/// Format: the argument naming a ref the client wants (`ref-in-want`).
const WANT_REF: &[u8] = b"want-ref";

/// Format: the argument naming an object the client has.
const HAVE: &[u8] = b"have";

/// Format: the argument ending negotiation.
const DONE: &[u8] = b"done";

/// Format: the argument asking for a thin pack.
const THIN_PACK: &[u8] = b"thin-pack";

/// Format: the argument asking for no progress on band 2.
const NO_PROGRESS: &[u8] = b"no-progress";

/// Format: the argument asking for the annotated tags of objects sent.
const INCLUDE_TAG: &[u8] = b"include-tag";

/// Format: the argument saying the client reads offset deltas.
const OFS_DELTA: &[u8] = b"ofs-delta";

/// Format: the argument naming a commit the client holds shallow (`shallow`), and the shallow-info line naming one the
/// server cut.
const SHALLOW: &[u8] = b"shallow";

/// Format: the shallow-info line naming a commit no longer shallow.
const UNSHALLOW: &[u8] = b"unshallow";

/// Format: the argument asking for a shallow history by depth.
const DEEPEN: &[u8] = b"deepen";

/// Format: the argument asking for a shallow history cut at a time.
const DEEPEN_SINCE: &[u8] = b"deepen-since";

/// Format: the argument asking for a shallow history cut at a rev.
const DEEPEN_NOT: &[u8] = b"deepen-not";

/// Format: the argument making a depth relative to the client's shallow boundary.
const DEEPEN_RELATIVE: &[u8] = b"deepen-relative";

/// Format: the argument naming a filter (`filter`).
const FILTER: &[u8] = b"filter";

/// Format: the argument naming the protocols of URIs the client takes (`packfile-uris`), and that section's name.
const PACKFILE_URIS: &[u8] = b"packfile-uris";

/// Format: the argument multiplexing the whole response (`sideband-all`).
const SIDEBAND_ALL: &[u8] = b"sideband-all";

/// Format: the argument asking the server never to send `ready` (`wait-for-done`).
const WAIT_FOR_DONE: &[u8] = b"wait-for-done";

/// Format: the acknowledgment that no have was common [gitprotocol-v2: `nak = PKT-LINE("NAK" LF)`].
const NAK: &[u8] = b"NAK";

/// Format: the acknowledgment of a common have, and the space after it [gitprotocol-v2: `ack = PKT-LINE("ACK" SP
/// obj-id LF)`].
const ACK: &[u8] = b"ACK";

/// Format: the line saying a pack follows [gitprotocol-v2: `ready = PKT-LINE("ready" LF)`].
const READY: &[u8] = b"ready";

/// Format: the deepest `deepen` git reads: its `int`'s largest, which is also its `INFINITE_DEPTH`, what `git fetch
/// --unshallow` sends (`upload-pack.c` `process_deepen`: `*depth = (int)strtol(...)`, refused unless positive).
const MAX_DEPTH: u32 = i32::MAX.unsigned_abs();

/// Format: the one `deepen-since` git refuses: `timestamp_t`'s largest, "-1", which revision.c reads as no limit
/// (`upload-pack.c` `process_deepen_since`).
const NO_LIMIT: u64 = u64::MAX;

/// Format: the lowest byte a packfile URI holds [gitprotocol-v2: `packfile-uri = PKT-LINE(40*(HEXDIGIT) SP *%x20-ff
/// LF)`].
const URI_LOWEST: u8 = 0x20;

/// Format: the most parts a response's text line is written in: an id line's word, space and id, then its ending.
const RESPONSE_LINE_PARTS: usize = 4;

/// Format: the arguments that are a word alone, each with its word.
const FLAGS: [(Argument<'static>, &[u8]); 8] = [
  (Argument::Done, DONE),
  (Argument::ThinPack, THIN_PACK),
  (Argument::NoProgress, NO_PROGRESS),
  (Argument::IncludeTag, INCLUDE_TAG),
  (Argument::OfsDelta, OFS_DELTA),
  (Argument::DeepenRelative, DEEPEN_RELATIVE),
  (Argument::SidebandAll, SIDEBAND_ALL),
  (Argument::WaitForDone, WAIT_FOR_DONE),
];

/// One `fetch` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Argument<'a> {
  /// `want <id>`: an object the client wants.
  Want(ObjectId),
  /// `want-ref <ref>`: a ref the client wants, by its full name.
  WantRef(&'a [u8]),
  /// `have <id>`: an object the client has.
  Have(ObjectId),
  /// `done`: negotiation is over; send the pack.
  Done,
  /// `thin-pack`: deltas may name bases the client holds.
  ThinPack,
  /// `no-progress`: send no progress on band 2.
  NoProgress,
  /// `include-tag`: send the annotated tags of objects sent.
  IncludeTag,
  /// `ofs-delta`: the client reads deltas whose base is named by offset.
  OfsDelta,
  /// `shallow <id>`: a commit the client holds without its parents.
  Shallow(ObjectId),
  /// `deepen <depth>`: a history this many commits deep.
  Deepen(u32),
  /// `deepen-since <time>`: a history cut at this time, in seconds since the epoch.
  DeepenSince(u64),
  /// `deepen-not <rev>`: a history cut at this rev, a ref's name, short or full.
  DeepenNot(&'a [u8]),
  /// `deepen-relative`: the depth counts from the client's shallow boundary.
  DeepenRelative,
  /// `filter <spec>`: leave objects out by this filter, as `rev-list --filter` reads it.
  Filter(&'a [u8]),
  /// `packfile-uris <protocols>`: the client takes URIs of these protocols, separated by commas, in place of objects.
  PackfileUris(&'a [u8]),
  /// `sideband-all`: multiplex the whole response.
  SidebandAll,
  /// `wait-for-done`: never send `ready`.
  WaitForDone,
}

impl<'a> Argument<'a> {
  /// The argument a line's text writes, its ids in `format`. Refused when it writes none, or a value that is none.
  pub(crate) fn parse(
    text: &'a [u8],
    format: ObjectFormat,
    at: u64,
  ) -> Result<Argument<'a>, Refusal> {
    if let Some((flag, _)) = FLAGS.iter().find(|(_, word)| *word == text) {
      return Ok(*flag);
    }
    let split = text
      .iter()
      .position(|byte| *byte == SP)
      .ok_or(Refusal::UnknownArgument { at })?;
    let word = text.get(..split).unwrap_or_default();
    let value = text.get(split.saturating_add(1)..).unwrap_or_default();
    valued(word, value, format, at)
  }

  /// Refused when the argument's value is not one it may take in `format`.
  pub(crate) fn check(&self, format: ObjectFormat, at: u64) -> Result<(), Refusal> {
    match self {
      Argument::Want(id) | Argument::Have(id) | Argument::Shallow(id) => {
        line::check_format(id, format, at)
      }
      Argument::WantRef(name) => ref_name(name, at).map(|_| ()),
      Argument::Deepen(depth) => self::depth(u64::from(*depth), at).map(|_| ()),
      Argument::DeepenSince(time) => since(*time, at).map(|_| ()),
      Argument::DeepenNot(value) | Argument::Filter(value) | Argument::PackfileUris(value) => {
        line::check_text(value, at)?;
        named(value, at).map(|_| ())
      }
      _ => Ok(()),
    }
  }

  /// The line's text in parts: the word, a space where there is a value, and the value.
  pub(super) fn parts(&self) -> [Part<'_>; 3] {
    let none = Part::Bytes(b"");
    let space = Part::Bytes(&[SP]);
    match self {
      Argument::Want(id) => [Part::Bytes(WANT), space, Part::Id(id)],
      Argument::Have(id) => [Part::Bytes(HAVE), space, Part::Id(id)],
      Argument::Shallow(id) => [Part::Bytes(SHALLOW), space, Part::Id(id)],
      Argument::WantRef(name) => [Part::Bytes(WANT_REF), space, Part::Bytes(name)],
      Argument::Deepen(depth) => [Part::Bytes(DEEPEN), space, Part::Decimal(u64::from(*depth))],
      Argument::DeepenSince(time) => [Part::Bytes(DEEPEN_SINCE), space, Part::Decimal(*time)],
      Argument::DeepenNot(rev) => [Part::Bytes(DEEPEN_NOT), space, Part::Bytes(rev)],
      Argument::Filter(spec) => [Part::Bytes(FILTER), space, Part::Bytes(spec)],
      Argument::PackfileUris(list) => [Part::Bytes(PACKFILE_URIS), space, Part::Bytes(list)],
      flag => [Part::Bytes(flag_word(*flag)), none, none],
    }
  }
}

/// The word of a flag, an argument that is a word alone.
fn flag_word(flag: Argument<'_>) -> &'static [u8] {
  FLAGS
    .iter()
    .find(|(known, _)| *known == flag)
    .map_or(b"", |(_, word)| *word)
}

/// The argument `word value` writes.
fn valued<'a>(
  word: &[u8],
  value: &'a [u8],
  format: ObjectFormat,
  at: u64,
) -> Result<Argument<'a>, Refusal> {
  match word {
    WANT => line::object_id(format, value, at).map(Argument::Want),
    HAVE => line::object_id(format, value, at).map(Argument::Have),
    SHALLOW => line::object_id(format, value, at).map(Argument::Shallow),
    WANT_REF => ref_name(value, at).map(Argument::WantRef),
    DEEPEN => number(value, at).and_then(|depth| self::depth(depth, at).map(Argument::Deepen)),
    DEEPEN_SINCE => number(value, at).and_then(|time| since(time, at).map(Argument::DeepenSince)),
    DEEPEN_NOT => named(value, at).map(Argument::DeepenNot),
    FILTER => named(value, at).map(Argument::Filter),
    PACKFILE_URIS => named(value, at).map(Argument::PackfileUris),
    _ => Err(Refusal::UnknownArgument { at }),
  }
}

/// The ref `name` names, refused when it is not a ref name.
fn ref_name(name: &[u8], at: u64) -> Result<&[u8], Refusal> {
  if refname::is_valid(name) {
    return Ok(name);
  }
  Err(Refusal::Refname { at })
}

/// The value of `digits`, canonical decimal.
fn number(digits: &[u8], at: u64) -> Result<u64, Refusal> {
  line::decimal(digits).ok_or(Refusal::Number { at })
}

/// `value` as a depth: positive, and no deeper than git's `int`.
fn depth(value: u64, at: u64) -> Result<u32, Refusal> {
  u32::try_from(value)
    .ok()
    .filter(|depth| (1..=MAX_DEPTH).contains(depth))
    .ok_or(Refusal::Number { at })
}

/// `value` as a time: any but the one git reads as no limit.
fn since(value: u64, at: u64) -> Result<u64, Refusal> {
  if value == NO_LIMIT {
    return Err(Refusal::Number { at });
  }
  Ok(value)
}

/// `value`, refused when it is empty.
fn named(value: &[u8], at: u64) -> Result<&[u8], Refusal> {
  if value.is_empty() {
    return Err(Refusal::EmptyValue { at });
  }
  Ok(value)
}

/// The rules between a request's arguments: `filter` and `packfile-uris` at most once each, and `deepen` never with
/// `deepen-since` or `deepen-not`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Rules {
  /// A `filter` came.
  filter: bool,
  /// A `packfile-uris` came.
  packfile_uris: bool,
  /// A `deepen` came.
  depth: bool,
  /// A `deepen-since` or `deepen-not` came.
  cut: bool,
}

impl Rules {
  /// Admits `argument` as the request's next: refused when it breaks a rule with one before it.
  pub(crate) fn admit(&mut self, argument: &Argument<'_>, at: u64) -> Result<(), Refusal> {
    match argument {
      Argument::Filter(_) => once(&mut self.filter, at),
      Argument::PackfileUris(_) => once(&mut self.packfile_uris, at),
      Argument::Deepen(_) => {
        self.depth = true;
        self.conflict(at)
      }
      Argument::DeepenSince(_) | Argument::DeepenNot(_) => {
        self.cut = true;
        self.conflict(at)
      }
      _ => Ok(()),
    }
  }

  /// Refused when both kinds of deepening came.
  fn conflict(&self, at: u64) -> Result<(), Refusal> {
    if self.depth && self.cut {
      return Err(Refusal::DeepenConflict { at });
    }
    Ok(())
  }
}

/// Marks what may come once as come: refused when it had.
fn once(seen: &mut bool, at: u64) -> Result<(), Refusal> {
  if *seen {
    return Err(Refusal::Repeated { at });
  }
  *seen = true;
  Ok(())
}

/// A section of a `fetch` response. The order of the variants is the order a response carries them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
  /// `acknowledgments`: which haves are common, and whether a pack follows.
  Acknowledgments,
  /// `shallow-info`: the shallow boundary the pack cuts at.
  ShallowInfo,
  /// `wanted-refs`: the ids of the refs wanted by name.
  WantedRefs,
  /// `packfile-uris`: packs to download in place of objects.
  PackfileUris,
  /// `packfile`: the pack, multiplexed.
  Packfile,
}

/// Format: each section and its header's text, in the order a response carries them [gitprotocol-v2 "fetch":
/// `output`].
const SECTIONS: [(Section, &[u8]); 5] = [
  (Section::Acknowledgments, b"acknowledgments"),
  (Section::ShallowInfo, b"shallow-info"),
  (Section::WantedRefs, b"wanted-refs"),
  (Section::PackfileUris, PACKFILE_URIS),
  (Section::Packfile, b"packfile"),
];

impl Section {
  /// The section's header.
  pub fn name(self) -> &'static [u8] {
    SECTIONS
      .iter()
      .find(|(section, _)| *section == self)
      .map_or(b"", |(_, name)| *name)
  }

  /// The section `name` heads.
  pub fn from_name(name: &[u8]) -> Option<Section> {
    SECTIONS
      .iter()
      .find(|(_, known)| *known == name)
      .map(|(section, _)| *section)
  }
}

/// A line of the wanted-refs section: a ref the client wanted by name, and its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WantedRef<'a> {
  /// What the ref names.
  pub id: ObjectId,
  /// The ref's full name, as the client wanted it.
  pub name: &'a [u8],
}

/// A line of the packfile-uris section: a pack the client downloads in place of its objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackfileUri<'a> {
  /// The pack's own hash, as `git index-pack` prints it, in the repository's format.
  pub hash: ObjectId,
  /// Where to download it: bytes from 0x20 up.
  pub uri: &'a [u8],
}

/// One packet of a `fetch` response, as the grammar reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
  /// A section's header.
  Header(Line<Section>),
  /// `NAK`: no have was common.
  Nak(Ending),
  /// `ACK <id>`: a common have.
  Ack(Line<ObjectId>),
  /// `ready`: a pack follows in this response.
  Ready(Ending),
  /// `shallow <id>`: a commit whose parents the pack leaves out.
  Shallow(Line<ObjectId>),
  /// `unshallow <id>`: a commit the client held shallow whose parents the pack brings.
  Unshallow(Line<ObjectId>),
  /// `<id> <ref>`: a ref the client wanted by name.
  WantedRef(Line<WantedRef<'a>>),
  /// `<hash> <uri>`: a pack to download.
  PackfileUri(Line<PackfileUri<'a>>),
  /// The delimiter closing a section before the packfile.
  Delim,
  /// Band 1 in the packfile section: bytes of the pack.
  Pack(&'a [u8]),
  /// Band 2: progress, text for the client to show; under `sideband-all` anywhere.
  Progress(&'a [u8]),
  /// Band 3: a fatal error; nothing follows it.
  Error(&'a [u8]),
  /// The flush ending the response.
  Flush,
}

/// Where a response stands in its grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
  /// Before its first section.
  Start,
  /// In the acknowledgments section, after the lines it has had.
  Acknowledgments(Acked),
  /// In the shallow-info section.
  ShallowInfo,
  /// In the wanted-refs section.
  WantedRefs,
  /// In the packfile-uris section.
  PackfileUris,
  /// After a section and the delimiter closing it: the next section comes later in the order.
  Between(Section),
  /// In the packfile section.
  Packfile,
  /// After the flush ending the response.
  Done,
  /// After a fatal error.
  Errored,
}

/// The acknowledgments a section has had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acked {
  /// None yet.
  Nothing,
  /// `NAK`.
  Nak,
  /// One `ACK` or more.
  Acks,
  /// `ready`, the section's last line.
  Ready,
}

/// The grammar of one `fetch` response, stepped one packet at a time by a reader ([`Response::read`]) or a writer
/// ([`Response::write`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
  /// Where the response stands.
  state: State,
  /// The format its ids are written in.
  format: ObjectFormat,
  /// Whether the client asked for `sideband-all`, so every data packet carries a band.
  multiplexed: bool,
}

impl Response {
  /// A response whose ids are in `format`, multiplexed throughout when the client asked for `sideband-all`.
  pub fn new(format: ObjectFormat, multiplexed: bool) -> Response {
    Response {
      state: State::Start,
      format,
      multiplexed,
    }
  }

  /// Whether the response has ended: after its last flush, or a fatal error.
  pub fn finished(&self) -> bool {
    matches!(self.state, State::Done | State::Errored)
  }

  /// Reads `packet`, the response's next, beginning `at` bytes into it: the event it is. Refused when the grammar has
  /// no place for it there.
  pub fn read<'a>(&mut self, packet: Packet<'a>, at: u64) -> Result<Event<'a>, Refusal> {
    if self.finished() {
      return Err(self.after_end(at));
    }
    let event = self.event(packet, at)?;
    self.state = self.next_state(&event, at)?;
    Ok(event)
  }

  /// Writes `event` as the response's next, beginning `at` bytes into it: the bytes it wrote. Refused, nothing written,
  /// when the grammar has no place for it there, a value is not one it may hold, or its line is longer than a packet
  /// carries.
  pub fn write(&mut self, event: &Event<'_>, at: u64, out: &mut impl Out) -> Result<u64, Refusal> {
    self.check(event, at)?;
    let next = self.next_state(event, at)?;
    let written = self.put(event, at, out)?;
    self.state = next;
    Ok(written)
  }

  /// What refuses a packet after the response's end.
  fn after_end(&self, at: u64) -> Refusal {
    match self.state {
      State::Errored => Refusal::AfterError { at },
      _ => Refusal::Trailing { at },
    }
  }

  /// Whether the response's data packets carry a band here: always in the packfile section, and everywhere under
  /// `sideband-all`.
  fn banded(&self) -> bool {
    self.multiplexed || self.state == State::Packfile
  }

  /// The event `packet` is, here.
  fn event<'a>(&self, packet: Packet<'a>, at: u64) -> Result<Event<'a>, Refusal> {
    match packet {
      Packet::Flush => Ok(Event::Flush),
      Packet::Delim => Ok(Event::Delim),
      Packet::ResponseEnd => Err(Refusal::Misplaced { at }),
      Packet::Data(data) if self.banded() => self.banded_event(data, at),
      Packet::Data(data) => self.line_event(data, at),
    }
  }

  /// The event a multiplexed data packet is.
  fn banded_event<'a>(&self, data: &'a [u8], at: u64) -> Result<Event<'a>, Refusal> {
    let (band, bytes) = sideband::demux(data).map_err(|refusal| Refusal::Band { at, refusal })?;
    match band {
      Band::Progress => Ok(Event::Progress(bytes)),
      Band::Error => Ok(Event::Error(bytes)),
      Band::Data if self.state == State::Packfile => Ok(Event::Pack(bytes)),
      Band::Data => self.line_event(bytes, at),
    }
  }

  /// The event a text line is, by the section it stands in.
  fn line_event<'a>(&self, data: &'a [u8], at: u64) -> Result<Event<'a>, Refusal> {
    let (text, ending) = line::text(data, at)?;
    match self.state {
      State::Start | State::Between(_) => header(text, ending, at),
      State::Acknowledgments(_) => acknowledgment(text, ending, self.format, at),
      State::ShallowInfo => shallow(text, ending, self.format, at),
      State::WantedRefs => wanted_ref(text, ending, self.format, at),
      State::PackfileUris => packfile_uri(text, ending, self.format, at),
      State::Packfile | State::Done | State::Errored => Err(Refusal::Misplaced { at }),
    }
  }

  /// Where the response stands after `event`, or why the grammar has no place for it here.
  fn next_state(&self, event: &Event<'_>, at: u64) -> Result<State, Refusal> {
    match (self.state, event) {
      (State::Done | State::Errored, _) => Err(self.after_end(at)),
      (_, Event::Progress(_)) if self.banded() => Ok(self.state),
      (_, Event::Error(_)) if self.banded() => Ok(State::Errored),
      (_, Event::Progress(_) | Event::Error(_)) => Err(Refusal::Misplaced { at }),
      (state, Event::Header(line)) => opened(state, line.value, at),
      (State::Acknowledgments(acked), Event::Nak(_) | Event::Ack(_) | Event::Ready(_)) => {
        acknowledged(acked, event, at)
      }
      (state, Event::Delim) => delimited(state, at),
      (state, Event::Flush) => flushed(state, at),
      (State::Packfile, Event::Pack(_)) => Ok(State::Packfile),
      (State::ShallowInfo, Event::Shallow(_) | Event::Unshallow(_)) => Ok(State::ShallowInfo),
      (State::WantedRefs, Event::WantedRef(_)) => Ok(State::WantedRefs),
      (State::PackfileUris, Event::PackfileUri(_)) => Ok(State::PackfileUris),
      (_, Event::Pack(_)) => Err(Refusal::Misplaced { at }),
      _ => Err(Refusal::Line { at }),
    }
  }

  /// Refused when `event`'s values are not ones it may hold in this response's format.
  fn check(&self, event: &Event<'_>, at: u64) -> Result<(), Refusal> {
    match event {
      Event::Ack(line) | Event::Shallow(line) | Event::Unshallow(line) => {
        line::check_format(&line.value, self.format, at)
      }
      Event::WantedRef(line) => {
        line::check_format(&line.value.id, self.format, at)?;
        ref_name(line.value.name, at).map(|_| ())
      }
      Event::PackfileUri(line) => {
        line::check_format(&line.value.hash, self.format, at)?;
        check_uri(line.value.uri, at)
      }
      _ => Ok(()),
    }
  }

  /// Writes `event`'s packet: the bytes written.
  fn put(&self, event: &Event<'_>, at: u64, out: &mut impl Out) -> Result<u64, Refusal> {
    let band = self.multiplexed.then_some(Band::Data);
    let space = Part::Bytes(&[SP]);
    match event {
      Event::Header(line) => text_line(
        band,
        &[Part::Bytes(line.value.name())],
        line.ending,
        at,
        out,
      ),
      Event::Nak(ending) => text_line(band, &[Part::Bytes(NAK)], *ending, at, out),
      Event::Ready(ending) => text_line(band, &[Part::Bytes(READY)], *ending, at, out),
      Event::Ack(line) => text_line(
        band,
        &[Part::Bytes(ACK), space, Part::Id(&line.value)],
        line.ending,
        at,
        out,
      ),
      Event::Shallow(line) => text_line(
        band,
        &[Part::Bytes(SHALLOW), space, Part::Id(&line.value)],
        line.ending,
        at,
        out,
      ),
      Event::Unshallow(line) => text_line(
        band,
        &[Part::Bytes(UNSHALLOW), space, Part::Id(&line.value)],
        line.ending,
        at,
        out,
      ),
      Event::WantedRef(line) => {
        let parts = [
          Part::Id(&line.value.id),
          space,
          Part::Bytes(line.value.name),
        ];
        text_line(band, &parts, line.ending, at, out)
      }
      Event::PackfileUri(line) => {
        let parts = [
          Part::Id(&line.value.hash),
          space,
          Part::Bytes(line.value.uri),
        ];
        text_line(band, &parts, line.ending, at, out)
      }
      Event::Pack(bytes) => line::put_line(Some(Band::Data), &[Part::Bytes(bytes)], at, out),
      Event::Progress(bytes) => {
        line::put_line(Some(Band::Progress), &[Part::Bytes(bytes)], at, out)
      }
      Event::Error(bytes) => line::put_line(Some(Band::Error), &[Part::Bytes(bytes)], at, out),
      Event::Delim => Ok(line::put_special(Packet::Delim, out)),
      Event::Flush => Ok(line::put_special(Packet::Flush, out)),
    }
  }
}

/// Writes a text line of `parts` and its ending, on `band` where the response is multiplexed: the bytes written.
fn text_line(
  band: Option<Band>,
  parts: &[Part<'_>],
  ending: Ending,
  at: u64,
  out: &mut impl Out,
) -> Result<u64, Refusal> {
  let mut all = [Part::Bytes(b""); RESPONSE_LINE_PARTS];
  for (slot, part) in all
    .iter_mut()
    .zip(parts.iter().copied().chain([Part::Bytes(ending.bytes())]))
  {
    *slot = part;
  }
  line::put_line(band, &all, at, out)
}

/// Where the response stands after a header for `section` in `state`: refused unless the response stands before its
/// first section or after the delimiter of one that comes earlier.
fn opened(state: State, section: Section, at: u64) -> Result<State, Refusal> {
  let later = match state {
    State::Start => true,
    State::Between(previous) => section > previous,
    _ => false,
  };
  if !later {
    return Err(Refusal::SectionOutOfOrder { at });
  }
  Ok(match section {
    Section::Acknowledgments => State::Acknowledgments(Acked::Nothing),
    Section::ShallowInfo => State::ShallowInfo,
    Section::WantedRefs => State::WantedRefs,
    Section::PackfileUris => State::PackfileUris,
    Section::Packfile => State::Packfile,
  })
}

/// Where an acknowledgments section stands after `event`, having had `acked`: `NAK` first and alone, `ACK`s with no
/// `NAK`, `ready` last.
fn acknowledged(acked: Acked, event: &Event<'_>, at: u64) -> Result<State, Refusal> {
  let next = match (acked, event) {
    (Acked::Nothing, Event::Nak(_)) => Acked::Nak,
    (Acked::Nothing | Acked::Acks, Event::Ack(_)) => Acked::Acks,
    (Acked::Nothing | Acked::Nak | Acked::Acks, Event::Ready(_)) => Acked::Ready,
    _ => return Err(Refusal::Acknowledgment { at }),
  };
  Ok(State::Acknowledgments(next))
}

/// Where the response stands after a delimiter in `state`: it closes a section other than the packfile, and an
/// acknowledgments section only after `ready`.
fn delimited(state: State, at: u64) -> Result<State, Refusal> {
  match state {
    State::Acknowledgments(Acked::Ready) => Ok(State::Between(Section::Acknowledgments)),
    State::Acknowledgments(_) => Err(Refusal::Acknowledgment { at }),
    State::ShallowInfo => Ok(State::Between(Section::ShallowInfo)),
    State::WantedRefs => Ok(State::Between(Section::WantedRefs)),
    State::PackfileUris => Ok(State::Between(Section::PackfileUris)),
    _ => Err(Refusal::Misplaced { at }),
  }
}

/// Where the response stands after a flush in `state`: it ends an acknowledgments section without `ready`, or the
/// packfile section; anywhere else the response would end without its pack.
fn flushed(state: State, at: u64) -> Result<State, Refusal> {
  match state {
    State::Acknowledgments(Acked::Ready) => Err(Refusal::Acknowledgment { at }),
    State::Acknowledgments(_) | State::Packfile => Ok(State::Done),
    _ => Err(Refusal::NoPackfile { at }),
  }
}

/// The header `text` names.
fn header(text: &[u8], ending: Ending, at: u64) -> Result<Event<'static>, Refusal> {
  Section::from_name(text)
    .map(|section| {
      Event::Header(Line {
        value: section,
        ending,
      })
    })
    .ok_or(Refusal::UnknownSection { at })
}

/// The acknowledgment `text` writes.
fn acknowledgment(
  text: &[u8],
  ending: Ending,
  format: ObjectFormat,
  at: u64,
) -> Result<Event<'static>, Refusal> {
  if text == NAK {
    return Ok(Event::Nak(ending));
  }
  if text == READY {
    return Ok(Event::Ready(ending));
  }
  match after_word(text, ACK) {
    Some(hex) => line::object_id(format, hex, at).map(|id| Event::Ack(Line { value: id, ending })),
    None => Err(Refusal::Acknowledgment { at }),
  }
}

/// The shallow-info line `text` writes.
fn shallow(
  text: &[u8],
  ending: Ending,
  format: ObjectFormat,
  at: u64,
) -> Result<Event<'static>, Refusal> {
  if let Some(hex) = after_word(text, SHALLOW) {
    return line::object_id(format, hex, at).map(|id| Event::Shallow(Line { value: id, ending }));
  }
  match after_word(text, UNSHALLOW) {
    Some(hex) => {
      line::object_id(format, hex, at).map(|id| Event::Unshallow(Line { value: id, ending }))
    }
    None => Err(Refusal::Line { at }),
  }
}

/// The wanted-refs line `text` writes.
fn wanted_ref(
  text: &[u8],
  ending: Ending,
  format: ObjectFormat,
  at: u64,
) -> Result<Event<'_>, Refusal> {
  let (hex, name) = split_word(text).ok_or(Refusal::Line { at })?;
  let id = line::object_id(format, hex, at)?;
  let name = ref_name(name, at)?;
  Ok(Event::WantedRef(Line {
    value: WantedRef { id, name },
    ending,
  }))
}

/// The packfile-uris line `text` writes.
fn packfile_uri(
  text: &[u8],
  ending: Ending,
  format: ObjectFormat,
  at: u64,
) -> Result<Event<'_>, Refusal> {
  let (hex, uri) = split_word(text).ok_or(Refusal::Line { at })?;
  let hash = line::object_id(format, hex, at)?;
  check_uri(uri, at)?;
  Ok(Event::PackfileUri(Line {
    value: PackfileUri { hash, uri },
    ending,
  }))
}

/// Refused when `uri` holds a byte below 0x20.
fn check_uri(uri: &[u8], at: u64) -> Result<(), Refusal> {
  if uri.iter().any(|byte| *byte < URI_LOWEST) {
    return Err(Refusal::Line { at });
  }
  Ok(())
}

/// What follows `word` and one space in `text`, if `text` begins so.
fn after_word<'a>(text: &'a [u8], word: &[u8]) -> Option<&'a [u8]> {
  text.strip_prefix(word)?.strip_prefix(&[SP])
}

/// `text` cut at its first space: the word before, and all after.
fn split_word(text: &[u8]) -> Option<(&[u8], &[u8])> {
  let split = text.iter().position(|byte| *byte == SP)?;
  Some((text.get(..split)?, text.get(split.saturating_add(1)..)?))
}

/// Decodes a `fetch` response, whole: its ids in `format`, multiplexed throughout when the client asked for
/// `sideband-all`. Refused when a packet has no place in the grammar where it stands, the bytes end before the
/// response does or go on after it; or when the list of its events would take more than `admission` has left, charged
/// before the list is made.
pub fn decode_response<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  multiplexed: bool,
  admission: &mut Admission,
) -> Result<Vec<Event<'a>>, Refusal> {
  let count = walk(bytes, format, multiplexed, |_| {})?;
  admission
    .charge_items::<Event<'a>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut events = Vec::with_capacity(count);
  walk(bytes, format, multiplexed, |event| events.push(event))?;
  Ok(events)
}

/// Walks the response in `bytes`, handing each event to `each`: how many there were.
fn walk<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  multiplexed: bool,
  mut each: impl FnMut(Event<'a>),
) -> Result<usize, Refusal> {
  let mut response = Response::new(format, multiplexed);
  let mut packets = Packets::new(bytes);
  let mut count = 0usize;
  while !response.finished() {
    let (packet, at) = packets
      .next_packet()?
      .ok_or(Refusal::Unfinished { at: packets.at() })?;
    each(response.read(packet, at)?);
    count = count.saturating_add(1);
  }
  if !packets.is_done() {
    return Err(response.after_end(packets.at()));
  }
  Ok(count)
}

/// Writes a `fetch` response of `events`, in `format`, multiplexed throughout where `multiplexed`. Refused, nothing
/// written, when an event has no place in the grammar where it stands, a value is not one it may hold, a packet is
/// longer than a packet carries, or the events end before the response does.
pub fn encode_response(
  events: &[Event<'_>],
  format: ObjectFormat,
  multiplexed: bool,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  write_response(events, format, multiplexed, &mut Measure::default())?;
  write_response(events, format, multiplexed, out)
}

/// Writes the response to `out`, checking each event before it writes it.
fn write_response(
  events: &[Event<'_>],
  format: ObjectFormat,
  multiplexed: bool,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let mut response = Response::new(format, multiplexed);
  let mut at = 0u64;
  for event in events {
    at = at.saturating_add(response.write(event, at, out)?);
  }
  if !response.finished() {
    return Err(Refusal::Unfinished { at });
  }
  Ok(())
}

#[cfg(test)]
#[path = "tests/fetch.rs"]
mod tests;
