//! Protocol v0 and v1: the ref advertisement and the lines of a fetch, for clients that do not speak protocol v2
//! (A-604; §4.17.2 "Pieces (git)", piece 14's first part).
//!
//! A fetch over protocol v0 is three steps, each a run of pkt-lines ([`crate::pktline`]):
//! 1. **The ref advertisement** ([`Advertisement`]). The server lists its refs, a line each: an object name, a space
//!    and the ref's name. The first line also carries the server's capabilities, after a NUL. An annotated tag's line
//!    is followed at once by its *peeled* line, the tag's name with `^{}` after it, naming the object the tag points
//!    at. A repository with no refs sends one line in their place, the null object name and the name
//!    `capabilities^{}`, to carry the capabilities. A shallow repository then names its shallow commits,
//!    `shallow <id>`, and a flush ends the advertisement. Protocol v1 is v0 with one line before all of it,
//!    `version 1`, which a server sends when the client asked for version 1 (`GIT_PROTOCOL=version=1`).
//! 2. **The request** ([`ClientLine`]): `want <id>` for each object the client wants, the first followed by the
//!    capabilities the client chose from the server's; `shallow <id>` for each commit the client holds without its
//!    parents; the depth it asks for, `deepen <depth>`, `deepen-since <time>` or `deepen-not <ref>`; `filter <spec>`
//!    for a partial clone; then a flush. When the client asked for depth, the server answers first with the
//!    **shallow-update** ([`ShallowUpdate`]): the commits that become shallow, then those that stop being shallow,
//!    then a flush.
//! 3. **The negotiation**: the client's `have <id>` lines in batches, each ended by a flush or by `done`, and the
//!    server's answers ([`ServerLine`]): `NAK`, or `ACK <id>`, plain or with a status, `continue` under `multi_ack`,
//!    `common` or `ready` under `multi_ack_detailed`. The pack follows, on side-band when the client asked for it
//!    ([`crate::sideband`]).
//!
//! Either side may send `ERR <text>` where a line is due, which ends the exchange ("Throughout the protocol, where
//! `PKT-LINE(...)` is expected, an error packet MAY be sent" [gitprotocol-pack]).
//!
//! **Byte for byte.** Each line is written as stock git 2.50.1 writes it: object names in lower-case hexadecimal, and
//! the line ended by LF where git's writer ends it so. git ends the advertisement's lines, `want`, `have`, `done`,
//! `ACK` and `NAK` with LF, and writes the request's `shallow`, `deepen`, `deepen-since`, `deepen-not` and `filter`,
//! the shallow-update's lines and `ERR` without one (`upload-pack.c`, `fetch-pack.c`, `shallow.c`, `pkt-line.c`). A
//! reader takes a line with or without its LF, as git's readers take one LF off a line's end
//! (`PACKET_READ_CHOMP_NEWLINE`; "the sender SHOULD include a LF, but the receiver MUST NOT complain if it is not
//! present"), and an object name's digits in either case ("both MUST treat obj-id as case-insensitive")
//! [gitprotocol-pack]. So a line as git writes it reads, and writes back to its own bytes; the golden vectors prove
//! that for every line stock git wrote.
//!
//! **Capabilities are kept by name.** A capability list ([`Capabilities`]) is read as names and values, each checked
//! against the document's grammar, and none is refused for being unknown: a capability git does not define is kept by
//! its name, in its place, and written back. Which ones a server honours, and whether it refuses a client's capability
//! that it never advertised ("Server MUST diagnose and abort if capabilities it does not understand were sent"
//! [gitprotocol-capabilities], which git 2.50.1's `upload-pack` does not do), is the serving piece's decision.
//!
//! **Refused by kind (§4.9).** A malformed line is refused with a typed reason and the byte where it stopped
//! ([`LineRefusal`]): an object name of the wrong length or with a byte that is no hexadecimal digit, a ref name with
//! a control byte or a space in it, a capability outside its grammar, a number not written as git writes it. Where
//! git's readers take more than any git writes, these readers are stricter: bytes after an object name (git's
//! `get_oid_hex` reads the digits it needs and ignores the rest), a depth in octal or hexadecimal or with a sign
//! (`strtol` with base 0), an `ACK` with a word after it that is none of the three (`get_ack` searches the rest with
//! `strstr`). No git client or server writes those, so refusing them costs no client, and every line these readers
//! take means one thing. A whole message that breaks its grammar is refused as a [`Refusal`]: a peeled line that does
//! not follow its ref ("The peeled value of a ref (that is "ref^{}") MUST be immediately after the ref itself"
//! [gitprotocol-pack]), a ref after the shallow lines, a version line naming any version but 1.
//!
//! **Bounds.** Reading a line allocates nothing: a line borrows its words, and its capability list, from the bytes it
//! was read from. The advertisement and the shallow-update, which hold lists, are walked twice, once to check them and
//! count their entries, charging the exact total to the caller's [`Admission`], and once to build, as
//! [`crate::pktline::decode`] is. A reader given part of a message answers `None` until the rest has arrived. Every
//! writer measures a line before it writes it and refuses one past a pkt-line's data, [`MAX_DATA`], writing nothing;
//! a message is checked whole before its first byte is written.
//!
//! **Sources** (read 2026-11-19, git 2.50.1). The documents [B]: `gitprotocol-pack` ("pkt-line Format", "Reference
//! Discovery", "Packfile Negotiation", their grammars) and `gitprotocol-capabilities`. The code [C]: `upload-pack.c`
//! (`write_v0_ref`, `get_common_commits`, `process_shallow`, `process_deepen`, `process_deepen_since`,
//! `send_shallow`, `send_unshallow`), `fetch-pack.c` (`find_common`, `get_ack`), `connect.c` (`get_remote_heads`,
//! `process_capabilities`, `process_dummy_ref`, `parse_feature_value`), `protocol.c`
//! (`determine_protocol_version_client`), `shallow.c` (`write_shallow_commits`), `pkt-line.c` and `refs.c`
//! (`refname_disposition`).
//!
//! **Golden vectors.** `fixtures/v0/` holds what stock git 2.50.1 (Apple Git-155) wrote on its own wire: the
//! advertisement of a repository with branches and an annotated tag under each version, of an empty repository, a
//! shallow one, a SHA-256 one, and `receive-pack`'s; whole exchanges between `git fetch-pack` and `git upload-pack`
//! under protocol versions 0 and 1, each side copied by a tee (a clone, a fetch with haves, clones by `--depth`,
//! `--shallow-since`, `--shallow-exclude` and `--filter=blob:none`, a shallow clone deepened); and requests the script
//! wrote with upload-pack's stateless answers, under `multi_ack`, `multi_ack_detailed` and neither, reaching `ready`
//! and `unshallow`, and refusing a want with `ERR`. `fixtures/v0/make-v0-fixtures.sh` made them on 2026-11-19 and
//! wrote `SHA256SUMS`, which the tests check every file against before reading it; a second run wrote the same bytes.

use crate::object_name::{
  HexRefusal, ObjectFormat, ObjectId, UnknownFormat, decimal, write_decimal,
};
use crate::out::{Measure, Out};
use crate::pktline::{self, HEADER_LEN, MAX_DATA, Packet};
use crate::{Admission, PastAdmission, to_u64};

/// Format: the byte git's writers end most lines with, and which its readers take off a line's end when it is there
/// (`pkt-line.c`'s `PACKET_READ_CHOMP_NEWLINE`).
const LF: u8 = b'\n';

/// Format: the separator between a line's words, and between capabilities [gitprotocol-pack].
const SP: u8 = b' ';

/// Format: the byte between the first ref line's name and the server's capabilities [gitprotocol-pack "first-ref"].
const NUL: u8 = 0;

/// Format: ASCII's delete, a control no ref name holds (`refs.c`'s `refname_disposition`).
const DEL: u8 = 0x7f;

/// Format: the separator between a capability's name and its value, as in `agent=git/2.50.1`
/// [gitprotocol-capabilities].
const EQUALS: u8 = b'=';

/// Format: the bytes a capability's name holds besides lower-case letters and digits:
/// `1*(LC_ALPHA / DIGIT / "-" / "_")` [gitprotocol-pack "capability"].
const NAME_PUNCTUATION: &[u8] = b"-_";

/// Format: the end of a peeled line's name, after the tag's name [gitprotocol-pack "other-peeled"].
const PEELED: &[u8] = b"^{}";

/// Format: the name on the line an advertisement with no refs sends, with the null object name [gitprotocol-pack
/// "no-refs"].
const NO_REFS: &[u8] = b"capabilities^{}";

/// Format: the word a version line begins with (`protocol.c`'s `determine_protocol_version_client`).
const VERSION: &[u8] = b"version";

/// Format: the version a protocol v1 advertisement's first line names [gitprotocol-pack "advertised-refs"].
const VERSION_ONE: &[u8] = b"1";

/// Format: the capability naming the advertisement's object format [gitprotocol-capabilities "object-format"].
const OBJECT_FORMAT: &[u8] = b"object-format";

/// Format: the request's `want` [gitprotocol-pack "first-want", "additional-want"].
const WANT: &[u8] = b"want";

/// Format: the negotiation's `have` [gitprotocol-pack "have-line"].
const HAVE: &[u8] = b"have";

/// Format: the line that ends the negotiation [gitprotocol-pack "compute-end"].
const DONE: &[u8] = b"done";

/// Format: a shallow commit's line, in the advertisement, the request and the shallow-update [gitprotocol-pack
/// "shallow", "shallow-line"].
const SHALLOW: &[u8] = b"shallow";

/// Format: the shallow-update's line for a commit that stops being shallow [gitprotocol-pack "unshallow-line"].
const UNSHALLOW: &[u8] = b"unshallow";

/// Format: a depth request by commits [gitprotocol-pack "depth-request"].
const DEEPEN: &[u8] = b"deepen";

/// Format: a depth request by time.
const DEEPEN_SINCE: &[u8] = b"deepen-since";

/// Format: a depth request by a ref to exclude.
const DEEPEN_NOT: &[u8] = b"deepen-not";

/// Format: a partial clone's filter [gitprotocol-pack "filter-request"].
const FILTER: &[u8] = b"filter";

/// Format: an acknowledgment [gitprotocol-pack "server-response"].
const ACK: &[u8] = b"ACK";

/// Format: the negative answer.
const NAK: &[u8] = b"NAK";

/// Format: the error line either side may send where a line is due [gitprotocol-pack "error-line"].
const ERR: &[u8] = b"ERR";

/// Format: each status an `ACK` may carry after its object name, and its word (`upload-pack.c`'s
/// `get_common_commits`; [gitprotocol-pack "ack_status"]).
const ACK_STATUSES: [(AckStatus, &[u8]); 3] = [
  (AckStatus::Continue, b"continue"),
  (AckStatus::Common, b"common"),
  (AckStatus::Ready, b"ready"),
];

/// Format: the least depth git takes: `upload-pack.c`'s `process_deepen` refuses one not above zero ("Invalid
/// deepen").
const MIN_DEPTH: u32 = 1;

/// Format: the greatest depth git takes, `INT_MAX`: `process_deepen` reads an `int`, and `--unshallow` asks for
/// `INFINITE_DEPTH`, which is `0x7fffffff` (`shallow.h`).
const MAX_DEPTH: u32 = i32::MAX.unsigned_abs();

/// Format: the one time `deepen-since` may not name, git's `-1` as its unsigned `timestamp_t`, which revision walks
/// take as no limit (`upload-pack.c`'s `process_deepen_since`).
const NO_TIME: u64 = u64::MAX;

/// Why one line is not a line of protocol v0. Each `at` counts bytes from the line's first, after its pkt-line length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineRefusal {
  /// The line's first word names no line this reader takes.
  Unknown,
  /// A byte, or the line's end, where the line's form wants another: a space or a NUL that is missing, or bytes after
  /// the line's last word.
  Form {
    /// Where.
    at: u64,
  },
  /// An object name that is not one in the exchange's format.
  Id {
    /// Where the name begins.
    at: u64,
    /// What is wrong with it.
    refusal: HexRefusal,
  },
  /// A ref's name, or `deepen-not`'s, that is empty or holds a byte no ref name holds: a control, a space or DEL.
  Name {
    /// Where.
    at: u64,
  },
  /// A capability that is empty (two spaces together, a space at the list's end, an empty list) or outside its
  /// grammar: a name byte other than `a-z`, `0-9`, `-` and `_`, or a value byte that no ref name holds.
  Capability {
    /// Where.
    at: u64,
  },
  /// An `object-format` capability naming a format git does not know.
  Format {
    /// Where the capability begins.
    at: u64,
  },
  /// A depth or a time not written as git writes a number (decimal, no sign, no zero first) or outside git's range:
  /// a depth from 1 to 2^31 − 1, a time below 2^64 − 1.
  Number {
    /// Where the number begins.
    at: u64,
  },
  /// A filter's spec that is empty, or a filter's spec or an error's text that holds a NUL or an LF.
  Text {
    /// Where.
    at: u64,
  },
}

/// Why bytes are not an advertisement or a shallow-update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The bytes are not pkt-lines.
  Packet(pktline::Refusal),
  /// A packet the message cannot hold where it stands: a special packet other than the closing flush, a peeled line
  /// that does not follow its ref, `capabilities^{}` other than alone in the first line or with an object name other
  /// than the null one, a ref after the shallow lines or after `capabilities^{}`, a `shallow` after an `unshallow`.
  Unexpected {
    /// Where its packet begins.
    at: u64,
  },
  /// A line refused.
  Line {
    /// Where its packet begins.
    at: u64,
    /// Why.
    refusal: LineRefusal,
  },
  /// A version line naming a version other than 1: `version 2` opens protocol v2's capability advertisement, not
  /// this one, and git refuses `version 0` ("protocol error: server explicitly said version 0").
  Version {
    /// Where its packet begins.
    at: u64,
  },
  /// The server sent `ERR` where the message's line was due: its text is the packet's at `at`
  /// ([`ServerLine::read`] reads it).
  ServerError {
    /// Where its packet begins.
    at: u64,
  },
  /// The message's lists would take more than the admission had left; nothing was built.
  PastAdmission(PastAdmission),
}

/// Why a line or a message was not written. Nothing was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
  /// A line whose data would be longer than a pkt-line carries, [`MAX_DATA`].
  TooLong {
    /// The data's length.
    len: u64,
  },
  /// A part a reader would refuse; its `at` counts from the line's first byte as it would be written.
  Line(LineRefusal),
  /// An object name in another format than its message's (an advertisement's `object-format`, a shallow-update's
  /// first name), or an `object-format` naming a format git does not know.
  Format,
}

/// One capability: its name and, after `=`, its value when it has one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Capability<'a> {
  /// The name: `a-z`, `0-9`, `-` and `_`.
  pub name: &'a [u8],
  /// The value after the first `=`, when there is one.
  pub value: Option<&'a [u8]>,
}

impl core::fmt::Debug for Capability<'_> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self.name.escape_ascii())?;
    if let Some(value) = self.value {
      write!(f, "={}", value.escape_ascii())?;
    }
    Ok(())
  }
}

/// A capability list: the server's, after the first ref line's NUL, or the client's, after its first `want`'s object
/// name. Read from a line, it borrows the line's bytes; made to be written, it borrows the caller's list.
#[derive(Clone, Copy)]
pub struct Capabilities<'a> {
  source: Source<'a>,
}

/// Where a capability list's entries are.
#[derive(Clone, Copy)]
enum Source<'a> {
  /// The list as it arrived, checked when read: entries separated by single spaces.
  Wire(&'a [u8]),
  /// A list the caller made, checked when written.
  Listed(&'a [Capability<'a>]),
}

impl<'a> Capabilities<'a> {
  /// Reads a capability list, `list` being all of it: every entry checked, none refused for its name.
  pub fn read(list: &'a [u8]) -> Result<Capabilities<'a>, LineRefusal> {
    Capabilities::read_at(list, 0)
  }

  /// The list `list` makes, to be written: checked when it is.
  pub fn listed(list: &'a [Capability<'a>]) -> Capabilities<'a> {
    Capabilities {
      source: Source::Listed(list),
    }
  }

  /// Reads a list that begins `at` bytes into its line.
  fn read_at(list: &'a [u8], at: usize) -> Result<Capabilities<'a>, LineRefusal> {
    for (offset, entry) in entries(list) {
      check_capability(split_capability(entry), at.saturating_add(offset))?;
    }
    Ok(Capabilities {
      source: Source::Wire(list),
    })
  }

  /// The capabilities in their order.
  pub fn iter(&self) -> CapabilityIter<'a> {
    CapabilityIter {
      source: self.source,
    }
  }

  /// The first capability named `name`.
  pub fn get(&self, name: &[u8]) -> Option<Capability<'a>> {
    self.iter().find(|capability| capability.name == name)
  }

  /// The object format the list names: the first `object-format`'s value ("if so, the first one given is the one used
  /// in the ref advertisement"), and SHA-1 when there is none ("If this capability is not provided, it is assumed that
  /// the only supported algorithm is SHA-1") [gitprotocol-capabilities].
  pub fn object_format(&self) -> Result<ObjectFormat, UnknownFormat> {
    self.format_at(0).map_err(|_| UnknownFormat)
  }

  /// The object format the list names, the list beginning `at` bytes into its line.
  fn format_at(&self, at: usize) -> Result<ObjectFormat, LineRefusal> {
    let Some((offset, capability)) = self
      .positioned()
      .find(|(_, capability)| capability.name == OBJECT_FORMAT)
    else {
      return Ok(ObjectFormat::Sha1);
    };
    ObjectFormat::from_name(capability.value.unwrap_or_default()).map_err(|UnknownFormat| {
      LineRefusal::Format {
        at: to_u64(at.saturating_add(offset)),
      }
    })
  }

  /// Each capability with where it begins in the list as written.
  fn positioned(&self) -> impl Iterator<Item = (usize, Capability<'a>)> + use<'a> {
    self.iter().scan(0usize, |offset, capability| {
      let at = *offset;
      *offset = offset
        .saturating_add(written_len(&capability))
        .saturating_add(1);
      Some((at, capability))
    })
  }

  /// Checks a list to be written, which begins `at` bytes into its line: at least one entry, each in its grammar. A
  /// list that was read was checked then.
  fn check(&self, at: usize) -> Result<(), LineRefusal> {
    if let Source::Wire(_) = self.source {
      return Ok(());
    }
    let mut entries = 0usize;
    for (offset, capability) in self.positioned() {
      check_capability(capability, at.saturating_add(offset))?;
      entries = entries.saturating_add(1);
    }
    if entries == 0 {
      return Err(LineRefusal::Capability { at: to_u64(at) });
    }
    Ok(())
  }

  /// Writes the list: its entries, single spaces between them.
  fn put(&self, out: &mut dyn Out) {
    match self.source {
      Source::Wire(list) => out.put(list),
      Source::Listed(list) => {
        for (index, capability) in list.iter().enumerate() {
          if index > 0 {
            out.put(&[SP]);
          }
          out.put(capability.name);
          if let Some(value) = capability.value {
            out.put(&[EQUALS]);
            out.put(value);
          }
        }
      }
    }
  }
}

impl PartialEq for Capabilities<'_> {
  fn eq(&self, other: &Self) -> bool {
    self.iter().eq(other.iter())
  }
}

impl Eq for Capabilities<'_> {}

impl core::fmt::Debug for Capabilities<'_> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.debug_list().entries(self.iter()).finish()
  }
}

/// The capabilities of a list in their order ([`Capabilities::iter`]).
#[derive(Clone)]
pub struct CapabilityIter<'a> {
  source: Source<'a>,
}

impl<'a> Iterator for CapabilityIter<'a> {
  type Item = Capability<'a>;

  fn next(&mut self) -> Option<Capability<'a>> {
    match &mut self.source {
      Source::Wire(rest) => next_entry(rest).map(split_capability),
      Source::Listed(rest) => {
        let (first, others) = rest.split_first()?;
        *rest = others;
        Some(*first)
      }
    }
  }
}

/// Each entry of a list as it arrived, with where it begins: the bytes between single spaces.
fn entries(list: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
  list
    .split(|byte| *byte == SP)
    .scan(0usize, |offset, entry| {
      let at = *offset;
      *offset = offset.saturating_add(entry.len()).saturating_add(1);
      Some((at, entry))
    })
}

/// Takes the first entry off what is left of a checked list; `None` once nothing is left.
fn next_entry<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
  let list = *rest;
  if list.is_empty() {
    return None;
  }
  let (entry, after) = split_word(list);
  *rest = after.unwrap_or_default();
  Some(entry)
}

/// An entry's name and value: the bytes before its first `=`, and those after it.
fn split_capability(entry: &[u8]) -> Capability<'_> {
  match entry.iter().position(|byte| *byte == EQUALS) {
    Some(equals) => Capability {
      name: entry.get(..equals).unwrap_or_default(),
      value: entry.get(equals.saturating_add(1)..),
    },
    None => Capability {
      name: entry,
      value: None,
    },
  }
}

/// The bytes `capability` takes written: its name, and `=` and its value when it has one.
fn written_len(capability: &Capability<'_>) -> usize {
  capability
    .value
    .map_or(0, |value| value.len().saturating_add(1))
    .saturating_add(capability.name.len())
}

/// Checks one capability, which begins `at` bytes into its line: a name of at least one byte, each a lower-case letter,
/// a digit, `-` or `_`; a value, when there is one, of bytes a ref name may hold.
fn check_capability(capability: Capability<'_>, at: usize) -> Result<(), LineRefusal> {
  let refused = |offset: usize| LineRefusal::Capability {
    at: to_u64(at.saturating_add(offset)),
  };
  if capability.name.is_empty() {
    return Err(refused(0));
  }
  if let Some(bad) = capability
    .name
    .iter()
    .position(|byte| !is_capability_name_byte(*byte))
  {
    return Err(refused(bad));
  }
  let value_at = capability.name.len().saturating_add(1);
  match capability
    .value
    .and_then(|value| value.iter().position(|byte| !is_word_byte(*byte)))
  {
    Some(bad) => Err(refused(value_at.saturating_add(bad))),
    None => Ok(()),
  }
}

/// Whether `byte` may be in a capability's name: `1*(LC_ALPHA / DIGIT / "-" / "_")` [gitprotocol-pack].
fn is_capability_name_byte(byte: u8) -> bool {
  byte.is_ascii_lowercase() || byte.is_ascii_digit() || NAME_PUNCTUATION.contains(&byte)
}

/// Whether `byte` may be in a word: a ref's name, a capability's value, `deepen-not`'s argument. Any byte above space
/// but DEL: no ref name holds a control or a space (`refs.c`'s `refname_disposition`), a space or a tab ends a
/// capability's value (`connect.c`'s `parse_feature_value`), and bytes past ASCII are kept, as a ref name may hold
/// UTF-8.
fn is_word_byte(byte: u8) -> bool {
  byte > SP && byte != DEL
}

/// Whether `byte` may be in a text: a filter's spec or an error's message. Any but NUL, which ends git's C strings, and
/// LF, which ends a line.
fn is_text_byte(byte: u8) -> bool {
  byte != NUL && byte != LF
}

/// The line a data packet carries without the one LF git's readers take off its end.
fn chomp(data: &[u8]) -> &[u8] {
  data.strip_suffix(&[LF]).unwrap_or(data)
}

/// A line's first word, up to its first space, and the rest after that space; `None` when it has no space.
fn split_word(line: &[u8]) -> (&[u8], Option<&[u8]>) {
  match line.iter().position(|byte| *byte == SP) {
    Some(space) => (
      line.get(..space).unwrap_or_default(),
      line.get(space.saturating_add(1)..),
    ),
    None => (line, None),
  }
}

/// Where the argument after `word` and its space begins.
fn argument_at(word: &[u8]) -> usize {
  word.len().saturating_add(1)
}

/// The object name `rest` begins with, in `format`, up to the next space or its end, and what follows it; `rest`
/// begins `at` bytes into its line.
fn read_id(rest: &[u8], at: usize, format: ObjectFormat) -> Result<(ObjectId, &[u8]), LineRefusal> {
  let end = rest
    .iter()
    .position(|byte| *byte == SP)
    .unwrap_or(rest.len());
  let (digits, after) = rest.split_at_checked(end).unwrap_or((rest, &[]));
  let id = ObjectId::from_hex(format, digits).map_err(|refusal| LineRefusal::Id {
    at: to_u64(at),
    refusal,
  })?;
  Ok((id, after))
}

/// The object name that is all of `rest`, which begins `at` bytes into its line.
fn only_id(rest: &[u8], at: usize, format: ObjectFormat) -> Result<ObjectId, LineRefusal> {
  let (id, after) = read_id(rest, at, format)?;
  if !after.is_empty() {
    return Err(LineRefusal::Form {
      at: to_u64(at.saturating_add(format.hex_len())),
    });
  }
  Ok(id)
}

/// The depth `digits` write, which begin `at` bytes into their line: decimal as git writes it, from 1 to `INT_MAX`.
fn read_depth(digits: &[u8], at: usize) -> Result<u32, LineRefusal> {
  decimal(digits)
    .ok()
    .and_then(|depth| u32::try_from(depth).ok())
    .filter(|depth| (MIN_DEPTH..=MAX_DEPTH).contains(depth))
    .ok_or(LineRefusal::Number { at: to_u64(at) })
}

/// The time `digits` write, which begin `at` bytes into their line: decimal as git writes it, any but git's `-1`.
fn read_time(digits: &[u8], at: usize) -> Result<u64, LineRefusal> {
  decimal(digits)
    .ok()
    .filter(|time| *time != NO_TIME)
    .ok_or(LineRefusal::Number { at: to_u64(at) })
}

/// Checks a ref's name, which begins `at` bytes into its line: at least one byte, each one a ref name may hold.
fn check_name(name: &[u8], at: usize) -> Result<(), LineRefusal> {
  let bad = if name.is_empty() {
    Some(0)
  } else {
    name.iter().position(|byte| !is_word_byte(*byte))
  };
  match bad {
    Some(offset) => Err(LineRefusal::Name {
      at: to_u64(at.saturating_add(offset)),
    }),
    None => Ok(()),
  }
}

/// Checks a text, which begins `at` bytes into its line: no NUL and no LF.
fn check_text(text: &[u8], at: usize) -> Result<(), LineRefusal> {
  match text.iter().position(|byte| !is_text_byte(*byte)) {
    Some(offset) => Err(LineRefusal::Text {
      at: to_u64(at.saturating_add(offset)),
    }),
    None => Ok(()),
  }
}

/// Checks a filter's spec, which begins `at` bytes into its line: a text of at least one byte.
fn check_spec(spec: &[u8], at: usize) -> Result<(), LineRefusal> {
  if spec.is_empty() {
    return Err(LineRefusal::Text { at: to_u64(at) });
  }
  check_text(spec, at)
}

/// Writes one data packet whose data `body` writes: measured first, so its length goes before it, then written.
/// Refused, nothing written, when the data is past [`MAX_DATA`].
fn put_line(out: &mut impl Out, body: impl Fn(&mut dyn Out)) -> Result<(), WriteRefusal> {
  let mut measure = Measure::default();
  body(&mut measure);
  let len = usize::try_from(measure.0)
    .ok()
    .filter(|len| *len <= MAX_DATA)
    .ok_or(WriteRefusal::TooLong { len: measure.0 })?;
  out.put(&pktline::header(len.saturating_add(HEADER_LEN)));
  body(out);
  Ok(())
}

/// Writes `word`, a space and `id` in lower-case hexadecimal.
fn put_word_id(line: &mut dyn Out, word: &[u8], id: &ObjectId) {
  line.put(word);
  line.put(&[SP]);
  id.write_hex(line);
}

/// Writes `word`, a space and `text`.
fn put_word_text(line: &mut dyn Out, word: &[u8], text: &[u8]) {
  line.put(word);
  line.put(&[SP]);
  line.put(text);
}

/// Writes `word`, a space and `value` in decimal.
fn put_word_number(line: &mut dyn Out, word: &[u8], value: u64) {
  line.put(word);
  line.put(&[SP]);
  write_decimal(value, line);
}

/// Refuses `id` when it is not in `format`.
fn same_format(id: ObjectId, format: ObjectFormat) -> Result<(), WriteRefusal> {
  if id.format() == format {
    Ok(())
  } else {
    Err(WriteRefusal::Format)
  }
}

/// Bytes shown as escaped ASCII in quotes, for a value's `Debug`.
struct Shown<'a>(&'a [u8]);

impl core::fmt::Debug for Shown<'_> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "\"{}\"", self.0.escape_ascii())
  }
}

/// One line a client sends in a fetch over protocol v0 or v1: its request, and its negotiation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClientLine<'a> {
  /// `want <id>`: an object the client wants. The first carries the capabilities the client chose,
  /// `want <id> <capabilities>`; the others carry none [gitprotocol-pack "first-want", "additional-want"].
  Want {
    /// The object.
    id: ObjectId,
    /// The client's capabilities, on its first `want`.
    capabilities: Option<Capabilities<'a>>,
  },
  /// `have <id>`: an object the client holds.
  Have(ObjectId),
  /// `done`: the client asks for the pack now.
  Done,
  /// `shallow <id>`: a commit the client holds without its parents.
  Shallow(ObjectId),
  /// `deepen <depth>`: how many commits from each tip the client wants, 1 to 2^31 − 1 (`--unshallow` asks for the
  /// most).
  Deepen(u32),
  /// `deepen-since <time>`: commits no older than this, in seconds since the epoch.
  DeepenSince(u64),
  /// `deepen-not <ref>`: commits not reachable from this ref, by the name the user gave it.
  DeepenNot(&'a [u8]),
  /// `filter <spec>`: the objects a partial clone leaves out, as `rev-list --filter` names them.
  Filter(&'a [u8]),
  /// `ERR <text>`: the client ends the exchange, saying why.
  Error(&'a [u8]),
}

impl<'a> ClientLine<'a> {
  /// Reads the line a data packet's `data` carry, its object names in `format`: refused by kind when it is no client
  /// line.
  pub fn read(data: &'a [u8], format: ObjectFormat) -> Result<ClientLine<'a>, LineRefusal> {
    let line = chomp(data);
    let (word, rest) = split_word(line);
    let Some(rest) = rest else {
      return match word {
        DONE => Ok(ClientLine::Done),
        WANT | HAVE | SHALLOW | DEEPEN | DEEPEN_SINCE | DEEPEN_NOT | FILTER | ERR => {
          Err(LineRefusal::Form {
            at: to_u64(line.len()),
          })
        }
        _ => Err(LineRefusal::Unknown),
      };
    };
    let at = argument_at(word);
    match word {
      WANT => read_want(rest, at, format),
      HAVE => only_id(rest, at, format).map(ClientLine::Have),
      SHALLOW => only_id(rest, at, format).map(ClientLine::Shallow),
      DEEPEN => read_depth(rest, at).map(ClientLine::Deepen),
      DEEPEN_SINCE => read_time(rest, at).map(ClientLine::DeepenSince),
      DEEPEN_NOT => check_name(rest, at).map(|()| ClientLine::DeepenNot(rest)),
      FILTER => check_spec(rest, at).map(|()| ClientLine::Filter(rest)),
      ERR => check_text(rest, at).map(|()| ClientLine::Error(rest)),
      DONE => Err(LineRefusal::Form {
        at: to_u64(DONE.len()),
      }),
      _ => Err(LineRefusal::Unknown),
    }
  }

  /// Writes the line in one data packet as git's `fetch-pack` writes it. Refused, nothing written, when a part is one
  /// [`ClientLine::read`] would refuse, or the line is past a packet's data.
  pub fn write(&self, out: &mut impl Out) -> Result<(), WriteRefusal> {
    self.check().map_err(WriteRefusal::Line)?;
    put_line(out, |line| self.put(line))
  }

  /// Checks the parts a reader would refuse.
  fn check(&self) -> Result<(), LineRefusal> {
    match self {
      ClientLine::Want {
        id,
        capabilities: Some(capabilities),
      } => capabilities.check(
        argument_at(WANT)
          .saturating_add(id.format().hex_len())
          .saturating_add(1),
      ),
      ClientLine::Deepen(depth) if !(MIN_DEPTH..=MAX_DEPTH).contains(depth) => {
        Err(LineRefusal::Number {
          at: to_u64(argument_at(DEEPEN)),
        })
      }
      ClientLine::DeepenSince(NO_TIME) => Err(LineRefusal::Number {
        at: to_u64(argument_at(DEEPEN_SINCE)),
      }),
      ClientLine::DeepenNot(name) => check_name(name, argument_at(DEEPEN_NOT)),
      ClientLine::Filter(spec) => check_spec(spec, argument_at(FILTER)),
      ClientLine::Error(text) => check_text(text, argument_at(ERR)),
      _ => Ok(()),
    }
  }

  /// Writes the line's bytes, ended as git ends it: `want`, `have` and `done` with LF (`fetch-pack.c`), the rest
  /// without (`shallow.c`'s `write_shallow_commits`, `fetch-pack.c`'s depth and filter lines, `pkt-line.c`'s
  /// `packet_writer_error`).
  fn put(&self, line: &mut dyn Out) {
    match self {
      ClientLine::Want { id, capabilities } => {
        put_word_id(line, WANT, id);
        if let Some(capabilities) = capabilities {
          line.put(&[SP]);
          capabilities.put(line);
        }
        line.put(&[LF]);
      }
      ClientLine::Have(id) => {
        put_word_id(line, HAVE, id);
        line.put(&[LF]);
      }
      ClientLine::Done => {
        line.put(DONE);
        line.put(&[LF]);
      }
      ClientLine::Shallow(id) => put_word_id(line, SHALLOW, id),
      ClientLine::Deepen(depth) => put_word_number(line, DEEPEN, u64::from(*depth)),
      ClientLine::DeepenSince(time) => put_word_number(line, DEEPEN_SINCE, *time),
      ClientLine::DeepenNot(name) => put_word_text(line, DEEPEN_NOT, name),
      ClientLine::Filter(spec) => put_word_text(line, FILTER, spec),
      ClientLine::Error(text) => put_word_text(line, ERR, text),
    }
  }
}

/// Reads `want`'s object name and, after a space, the client's capabilities; `rest` begins `at` bytes into its line.
fn read_want(rest: &[u8], at: usize, format: ObjectFormat) -> Result<ClientLine<'_>, LineRefusal> {
  let (id, after) = read_id(rest, at, format)?;
  let capabilities = match after.split_first() {
    None => None,
    Some((_, list)) => Some(Capabilities::read_at(
      list,
      at.saturating_add(format.hex_len()).saturating_add(1),
    )?),
  };
  Ok(ClientLine::Want { id, capabilities })
}

impl core::fmt::Debug for ClientLine<'_> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      ClientLine::Want { id, capabilities } => f
        .debug_struct("Want")
        .field("id", id)
        .field("capabilities", capabilities)
        .finish(),
      ClientLine::Have(id) => f.debug_tuple("Have").field(id).finish(),
      ClientLine::Done => f.write_str("Done"),
      ClientLine::Shallow(id) => f.debug_tuple("Shallow").field(id).finish(),
      ClientLine::Deepen(depth) => f.debug_tuple("Deepen").field(depth).finish(),
      ClientLine::DeepenSince(time) => f.debug_tuple("DeepenSince").field(time).finish(),
      ClientLine::DeepenNot(name) => f.debug_tuple("DeepenNot").field(&Shown(name)).finish(),
      ClientLine::Filter(spec) => f.debug_tuple("Filter").field(&Shown(spec)).finish(),
      ClientLine::Error(text) => f.debug_tuple("Error").field(&Shown(text)).finish(),
    }
  }
}

/// Which acknowledgment an `ACK` is [gitprotocol-pack "Packfile Negotiation"].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckStatus {
  /// `ACK <id>`: without `multi_ack` or `multi_ack_detailed`, the first common object; with either, after `done`, the
  /// last common object, before the pack.
  Plain,
  /// `ACK <id> continue`: under `multi_ack`, a common object, the server not yet ready.
  Continue,
  /// `ACK <id> common`: under `multi_ack_detailed`, a common object.
  Common,
  /// `ACK <id> ready`: under `multi_ack_detailed`, the server has what it needs to make the pack.
  Ready,
}

impl AckStatus {
  /// The word after the object name; none for a plain `ACK`.
  pub fn word(self) -> Option<&'static [u8]> {
    ACK_STATUSES
      .iter()
      .find(|(status, _)| *status == self)
      .map(|(_, word)| *word)
  }

  /// The status `word` names.
  fn from_word(word: &[u8]) -> Option<AckStatus> {
    ACK_STATUSES
      .iter()
      .find(|(_, known)| *known == word)
      .map(|(status, _)| *status)
  }
}

/// One line a server answers a fetch's negotiation with, over protocol v0 or v1.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServerLine<'a> {
  /// `NAK`: no common object yet, or, after `done`, none at all.
  Nak,
  /// `ACK <id>`, plain or with a status.
  Ack {
    /// The object acknowledged as common.
    id: ObjectId,
    /// Which acknowledgment.
    status: AckStatus,
  },
  /// `ERR <text>`: the server ends the exchange, saying why.
  Error(&'a [u8]),
}

impl<'a> ServerLine<'a> {
  /// Reads the line a data packet's `data` carry, its object names in `format`: refused by kind when it is no server
  /// line.
  pub fn read(data: &'a [u8], format: ObjectFormat) -> Result<ServerLine<'a>, LineRefusal> {
    let line = chomp(data);
    let (word, rest) = split_word(line);
    let Some(rest) = rest else {
      return match word {
        NAK => Ok(ServerLine::Nak),
        ACK | ERR => Err(LineRefusal::Form {
          at: to_u64(line.len()),
        }),
        _ => Err(LineRefusal::Unknown),
      };
    };
    let at = argument_at(word);
    match word {
      ACK => read_ack(rest, at, format),
      ERR => check_text(rest, at).map(|()| ServerLine::Error(rest)),
      NAK => Err(LineRefusal::Form {
        at: to_u64(NAK.len()),
      }),
      _ => Err(LineRefusal::Unknown),
    }
  }

  /// Writes the line in one data packet as git's `upload-pack` writes it: `ACK` and `NAK` ended by LF
  /// (`get_common_commits`), `ERR` without (`packet_writer_error`). Refused, nothing written, for an error's text a
  /// reader would refuse or one past a packet's data.
  pub fn write(&self, out: &mut impl Out) -> Result<(), WriteRefusal> {
    if let ServerLine::Error(text) = self {
      check_text(text, argument_at(ERR)).map_err(WriteRefusal::Line)?;
    }
    put_line(out, |line| self.put(line))
  }

  /// Writes the line's bytes.
  fn put(&self, line: &mut dyn Out) {
    match self {
      ServerLine::Nak => {
        line.put(NAK);
        line.put(&[LF]);
      }
      ServerLine::Ack { id, status } => {
        put_word_id(line, ACK, id);
        if let Some(word) = status.word() {
          line.put(&[SP]);
          line.put(word);
        }
        line.put(&[LF]);
      }
      ServerLine::Error(text) => put_word_text(line, ERR, text),
    }
  }
}

/// Reads `ACK`'s object name and its status, if any, after a space; `rest` begins `at` bytes into its line.
fn read_ack(rest: &[u8], at: usize, format: ObjectFormat) -> Result<ServerLine<'_>, LineRefusal> {
  let (id, after) = read_id(rest, at, format)?;
  let status = match after.split_first() {
    None => AckStatus::Plain,
    Some((_, word)) => AckStatus::from_word(word).ok_or(LineRefusal::Form {
      at: to_u64(at.saturating_add(format.hex_len()).saturating_add(1)),
    })?,
  };
  Ok(ServerLine::Ack { id, status })
}

impl core::fmt::Debug for ServerLine<'_> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      ServerLine::Nak => f.write_str("Nak"),
      ServerLine::Ack { id, status } => f
        .debug_struct("Ack")
        .field("id", id)
        .field("status", status)
        .finish(),
      ServerLine::Error(text) => f.debug_tuple("Error").field(&Shown(text)).finish(),
    }
  }
}

/// A message's packets, read one at a time from where the last ended.
struct Packets<'a> {
  bytes: &'a [u8],
  /// Where the next packet begins.
  at: usize,
}

impl<'a> Packets<'a> {
  /// The packets of `bytes`, from their start.
  fn new(bytes: &'a [u8]) -> Packets<'a> {
    Packets { bytes, at: 0 }
  }

  /// The next packet and where it begins; `None` while the bytes do not yet hold it.
  fn take(&mut self) -> Result<Option<(usize, Packet<'a>)>, Refusal> {
    let rest = self.bytes.get(self.at..).unwrap_or_default();
    let Some((packet, took)) = pktline::read(rest, to_u64(self.at)).map_err(Refusal::Packet)?
    else {
      return Ok(None);
    };
    let at = self.at;
    self.at = at.saturating_add(took);
    Ok(Some((at, packet)))
  }
}

/// The line a message's data packet carries, without the LF git's readers take off; refused when it is `ERR`, the
/// server ending the exchange where the message's line was due.
fn message_line(at: usize, data: &[u8]) -> Result<&[u8], Refusal> {
  let line = chomp(data);
  match split_word(line) {
    (ERR, Some(_)) => Err(Refusal::ServerError { at: to_u64(at) }),
    _ => Ok(line),
  }
}

/// A refusal of the line whose packet begins `at`.
fn refused_line(at: usize) -> impl Fn(LineRefusal) -> Refusal {
  move |refusal| Refusal::Line {
    at: to_u64(at),
    refusal,
  }
}

/// Which protocol an advertisement speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
  /// Protocol v0: the advertisement opens with its first ref.
  V0,
  /// Protocol v1: the advertisement opens with `version 1`, and is v0's after it.
  V1,
}

/// A ref as an advertisement names it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AdvertisedRef<'a> {
  /// The object the ref names.
  pub id: ObjectId,
  /// The ref's name as the server sent it: `HEAD`, a full name such as `refs/heads/main`, or `.have` for an object
  /// `receive-pack` offers from an alternate.
  pub name: &'a [u8],
  /// For an annotated tag, the object it peels to, sent on the line after the tag's as `<id> <name>^{}`.
  pub peeled: Option<ObjectId>,
}

impl core::fmt::Debug for AdvertisedRef<'_> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.debug_struct("AdvertisedRef")
      .field("id", &self.id)
      .field("name", &Shown(self.name))
      .field("peeled", &self.peeled)
      .finish()
  }
}

/// A ref advertisement, as `upload-pack` and `receive-pack` send it over protocol v0 or v1 [gitprotocol-pack
/// "Reference Discovery"].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertisement<'a> {
  /// Whether the server opened with `version 1`.
  pub version: Version,
  /// The server's capabilities, from the first line, in their order, each kept by its name.
  pub capabilities: Capabilities<'a>,
  /// The refs in their order. Empty for a repository with none, for which the server sends the line
  /// `capabilities^{}` instead.
  pub refs: Vec<AdvertisedRef<'a>>,
  /// The commits a shallow repository holds without their parents.
  pub shallows: Vec<ObjectId>,
}

impl<'a> Advertisement<'a> {
  /// Decodes the advertisement `bytes` begin with, charging its lists to `admission` first: the advertisement and the
  /// bytes it took, its flush included. `None` while `bytes` do not yet hold its flush. Its object names are in the
  /// format its first line's `object-format` names, SHA-1 when it names none.
  pub fn decode(
    bytes: &'a [u8],
    admission: &mut Admission,
  ) -> Result<Option<(Advertisement<'a>, usize)>, Refusal> {
    let Some(walked) = walk_advertisement(bytes, &mut |_| {})? else {
      return Ok(None);
    };
    let room = to_u64(core::mem::size_of::<AdvertisedRef<'a>>())
      .saturating_mul(to_u64(walked.refs))
      .saturating_add(
        to_u64(core::mem::size_of::<ObjectId>()).saturating_mul(to_u64(walked.shallows)),
      );
    admission.charge(room).map_err(Refusal::PastAdmission)?;
    let mut refs: Vec<AdvertisedRef<'a>> = Vec::with_capacity(walked.refs);
    let mut shallows = Vec::with_capacity(walked.shallows);
    walk_advertisement(bytes, &mut |entry| match entry {
      Entry::Ref(reference) => refs.push(reference),
      Entry::Peeled(id) => {
        if let Some(tag) = refs.last_mut() {
          tag.peeled = Some(id);
        }
      }
      Entry::Shallow(id) => shallows.push(id),
    })?;
    let advertisement = Advertisement {
      version: walked.version,
      capabilities: walked.capabilities,
      refs,
      shallows,
    };
    Ok(Some((advertisement, walked.took)))
  }

  /// Writes the advertisement as git's `upload-pack` and `receive-pack` write it, its flush included: the refs in the
  /// order given (the serving piece sorts them, as the document asks). Refused, nothing written, when a part is one
  /// [`Advertisement::decode`] would refuse, or a line is past a packet's data.
  pub fn write(&self, out: &mut impl Out) -> Result<(), WriteRefusal> {
    self.encode(&mut Measure::default())?;
    self.encode(out)
  }

  /// Writes every line, checking each part first.
  fn encode(&self, out: &mut impl Out) -> Result<(), WriteRefusal> {
    let format = self
      .capabilities
      .object_format()
      .map_err(|UnknownFormat| WriteRefusal::Format)?;
    if self.version == Version::V1 {
      put_line(out, |line| {
        line.put(VERSION);
        line.put(&[SP]);
        line.put(VERSION_ONE);
        line.put(&[LF]);
      })?;
    }
    self.encode_first(format, out)?;
    for reference in self.refs.iter().skip(1) {
      check_ref(reference, format)?;
      put_line(out, |line| {
        reference.id.write_hex(line);
        line.put(&[SP]);
        line.put(reference.name);
        line.put(&[LF]);
      })?;
      put_peeled(reference, out)?;
    }
    for id in &self.shallows {
      same_format(*id, format)?;
      put_line(out, |line| {
        put_word_id(line, SHALLOW, id);
        line.put(&[LF]);
      })?;
    }
    pktline::write(Packet::Flush, out).map_err(|refused| WriteRefusal::TooLong { len: refused.len })
  }

  /// Writes the first line, the first ref's or `capabilities^{}` with the null name, the capabilities after a NUL;
  /// then the first ref's peeled line.
  fn encode_first(&self, format: ObjectFormat, out: &mut impl Out) -> Result<(), WriteRefusal> {
    let first = match self.refs.first() {
      Some(first) => {
        check_ref(first, format)?;
        *first
      }
      None => AdvertisedRef {
        id: ObjectId::null(format),
        name: NO_REFS,
        peeled: None,
      },
    };
    let list_at = format
      .hex_len()
      .saturating_add(1)
      .saturating_add(first.name.len())
      .saturating_add(1);
    self
      .capabilities
      .check(list_at)
      .map_err(WriteRefusal::Line)?;
    put_line(out, |line| {
      first.id.write_hex(line);
      line.put(&[SP]);
      line.put(first.name);
      line.put(&[NUL]);
      self.capabilities.put(line);
      line.put(&[LF]);
    })?;
    put_peeled(&first, out)
  }
}

/// Checks a ref to be written: its object names in `format`, and a name that reads back as itself, so not a peeled
/// line's.
fn check_ref(reference: &AdvertisedRef<'_>, format: ObjectFormat) -> Result<(), WriteRefusal> {
  same_format(reference.id, format)?;
  if let Some(peeled) = reference.peeled {
    same_format(peeled, format)?;
  }
  let name_at = format.hex_len().saturating_add(1);
  check_name(reference.name, name_at).map_err(WriteRefusal::Line)?;
  if reference.name.ends_with(PEELED) {
    return Err(WriteRefusal::Line(LineRefusal::Name {
      at: to_u64(name_at),
    }));
  }
  Ok(())
}

/// Writes a ref's peeled line, `<id> <name>^{}`, when it has one.
fn put_peeled(reference: &AdvertisedRef<'_>, out: &mut impl Out) -> Result<(), WriteRefusal> {
  let Some(peeled) = reference.peeled else {
    return Ok(());
  };
  put_line(out, |line| {
    peeled.write_hex(line);
    line.put(&[SP]);
    line.put(reference.name);
    line.put(PEELED);
    line.put(&[LF]);
  })
}

/// One entry a walk of an advertisement hands its builder.
enum Entry<'a> {
  /// A ref.
  Ref(AdvertisedRef<'a>),
  /// The object the ref before it peels to.
  Peeled(ObjectId),
  /// A shallow commit.
  Shallow(ObjectId),
}

/// What a walk of a whole advertisement found besides its entries.
struct Walked<'a> {
  version: Version,
  capabilities: Capabilities<'a>,
  /// The refs, peeled lines not counted.
  refs: usize,
  shallows: usize,
  /// The bytes the advertisement took, its flush included.
  took: usize,
}

/// What may come next in an advertisement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
  /// A ref, a peeled line right after its ref, a shallow line or the flush.
  Refs,
  /// A shallow line or the flush: after `capabilities^{}`, or after a shallow line.
  Shallows,
}

/// Where a walk of an advertisement stands.
struct RefWalk<'a> {
  packets: Packets<'a>,
  /// The format the first line's capabilities name.
  format: ObjectFormat,
  place: Place,
  /// The name of the last ref, while its peeled line may still follow.
  peelable: Option<&'a [u8]>,
  refs: usize,
  shallows: usize,
}

/// Walks the advertisement `bytes` begin with, handing each entry to `each`: what it found, or `None` while the bytes
/// do not yet hold its flush.
fn walk_advertisement<'a>(
  bytes: &'a [u8],
  each: &mut dyn FnMut(Entry<'a>),
) -> Result<Option<Walked<'a>>, Refusal> {
  let mut walk = RefWalk {
    packets: Packets::new(bytes),
    format: ObjectFormat::Sha1,
    place: Place::Refs,
    peelable: None,
    refs: 0,
    shallows: 0,
  };
  let Some((version, capabilities)) = walk.opening(each)? else {
    return Ok(None);
  };
  loop {
    let Some((at, packet)) = walk.packets.take()? else {
      return Ok(None);
    };
    match packet {
      Packet::Flush => {
        return Ok(Some(Walked {
          version,
          capabilities,
          refs: walk.refs,
          shallows: walk.shallows,
          took: walk.packets.at,
        }));
      }
      Packet::Data(data) => walk.after_first(at, message_line(at, data)?, each)?,
      Packet::Delim | Packet::ResponseEnd => return Err(Refusal::Unexpected { at: to_u64(at) }),
    }
  }
}

impl<'a> RefWalk<'a> {
  /// The next packet's line, without its LF: refused for a special packet, and for `ERR`; `None` while the bytes do
  /// not yet hold it.
  fn line(&mut self) -> Result<Option<(usize, &'a [u8])>, Refusal> {
    let Some((at, packet)) = self.packets.take()? else {
      return Ok(None);
    };
    match packet {
      Packet::Data(data) => message_line(at, data).map(|line| Some((at, line))),
      Packet::Flush | Packet::Delim | Packet::ResponseEnd => {
        Err(Refusal::Unexpected { at: to_u64(at) })
      }
    }
  }

  /// Reads the opening: `version 1` when the server sent it, then the first line. The version and the capabilities;
  /// `None` while the bytes do not yet hold them.
  fn opening(
    &mut self,
    each: &mut dyn FnMut(Entry<'a>),
  ) -> Result<Option<(Version, Capabilities<'a>)>, Refusal> {
    let Some((at, line)) = self.line()? else {
      return Ok(None);
    };
    let (version, at, line) = match split_word(line) {
      (VERSION, Some(number)) => {
        if number != VERSION_ONE {
          return Err(Refusal::Version { at: to_u64(at) });
        }
        let Some((at, line)) = self.line()? else {
          return Ok(None);
        };
        (Version::V1, at, line)
      }
      _ => (Version::V0, at, line),
    };
    let capabilities = self.first(at, line, each)?;
    Ok(Some((version, capabilities)))
  }

  /// Reads the first line: a ref, or `capabilities^{}` with the null name, then a NUL and the server's capabilities,
  /// whose `object-format` sets the format of every object name in the advertisement, this line's included.
  fn first(
    &mut self,
    at: usize,
    line: &'a [u8],
    each: &mut dyn FnMut(Entry<'a>),
  ) -> Result<Capabilities<'a>, Refusal> {
    let refused = refused_line(at);
    let Some(nul) = line.iter().position(|byte| *byte == NUL) else {
      return Err(refused(LineRefusal::Form {
        at: to_u64(line.len()),
      }));
    };
    let list_at = nul.saturating_add(1);
    let list = line.get(list_at..).unwrap_or_default();
    let capabilities = Capabilities::read_at(list, list_at).map_err(&refused)?;
    self.format = capabilities.format_at(list_at).map_err(&refused)?;
    let head = line.get(..nul).unwrap_or_default();
    let (id, name) = read_ref(head, self.format).map_err(&refused)?;
    if name == NO_REFS {
      if !id.is_null() {
        return Err(Refusal::Unexpected { at: to_u64(at) });
      }
      self.place = Place::Shallows;
      return Ok(capabilities);
    }
    self.reference(at, id, name, each)?;
    Ok(capabilities)
  }

  /// Takes a line after the first: a ref or a peeled line while refs may come, or a shallow line.
  fn after_first(
    &mut self,
    at: usize,
    line: &'a [u8],
    each: &mut dyn FnMut(Entry<'a>),
  ) -> Result<(), Refusal> {
    if let (SHALLOW, Some(rest)) = split_word(line) {
      let id = only_id(rest, argument_at(SHALLOW), self.format).map_err(refused_line(at))?;
      self.place = Place::Shallows;
      self.peelable = None;
      self.shallows = self.shallows.saturating_add(1);
      each(Entry::Shallow(id));
      return Ok(());
    }
    if self.place == Place::Shallows {
      return Err(Refusal::Unexpected { at: to_u64(at) });
    }
    let (id, name) = read_ref(line, self.format).map_err(refused_line(at))?;
    self.reference(at, id, name, each)
  }

  /// Takes a ref's line, or the peeled line of the ref right before it.
  fn reference(
    &mut self,
    at: usize,
    id: ObjectId,
    name: &'a [u8],
    each: &mut dyn FnMut(Entry<'a>),
  ) -> Result<(), Refusal> {
    match name.strip_suffix(PEELED) {
      Some(tag) if name != NO_REFS && self.peelable == Some(tag) => {
        self.peelable = None;
        each(Entry::Peeled(id));
        Ok(())
      }
      Some(_) => Err(Refusal::Unexpected { at: to_u64(at) }),
      None => {
        self.peelable = Some(name);
        self.refs = self.refs.saturating_add(1);
        each(Entry::Ref(AdvertisedRef {
          id,
          name,
          peeled: None,
        }));
        Ok(())
      }
    }
  }
}

/// Reads a ref's line, `<id> <name>`, its object name in `format`.
fn read_ref(line: &[u8], format: ObjectFormat) -> Result<(ObjectId, &[u8]), LineRefusal> {
  let (id, after) = read_id(line, 0, format)?;
  let Some((_, name)) = after.split_first() else {
    return Err(LineRefusal::Form {
      at: to_u64(line.len()),
    });
  };
  check_name(name, format.hex_len().saturating_add(1))?;
  Ok((id, name))
}

/// The shallow-update a server sends after a request for depth, before the negotiation [gitprotocol-pack
/// "shallow-update"].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShallowUpdate {
  /// `shallow <id>`: commits whose parents the pack will not carry.
  pub shallow: Vec<ObjectId>,
  /// `unshallow <id>`: commits the client said were shallow whose parents the pack now carries.
  pub unshallow: Vec<ObjectId>,
}

/// One line of a shallow-update.
#[derive(Clone, Copy)]
enum ShallowLine {
  /// `shallow <id>`.
  Shallow(ObjectId),
  /// `unshallow <id>`.
  Unshallow(ObjectId),
}

/// What a walk of a whole shallow-update counted.
#[derive(Default)]
struct ShallowCount {
  shallow: usize,
  unshallow: usize,
  /// The bytes it took, its flush included.
  took: usize,
}

impl ShallowCount {
  /// Counts `line`, which begins at `at`: refused for a `shallow` after an `unshallow`, against the grammar's order.
  fn count(&mut self, at: usize, line: ShallowLine) -> Result<(), Refusal> {
    match line {
      ShallowLine::Shallow(_) if self.unshallow > 0 => Err(Refusal::Unexpected { at: to_u64(at) }),
      ShallowLine::Shallow(_) => {
        self.shallow = self.shallow.saturating_add(1);
        Ok(())
      }
      ShallowLine::Unshallow(_) => {
        self.unshallow = self.unshallow.saturating_add(1);
        Ok(())
      }
    }
  }
}

impl ShallowUpdate {
  /// Decodes the shallow-update `bytes` begin with, its object names in `format`, charging its lists to `admission`
  /// first: the shallow-update and the bytes it took, its flush included. `None` while `bytes` do not yet hold its
  /// flush.
  pub fn decode(
    bytes: &[u8],
    format: ObjectFormat,
    admission: &mut Admission,
  ) -> Result<Option<(ShallowUpdate, usize)>, Refusal> {
    let Some(counted) = walk_shallow_update(bytes, format, &mut |_| {})? else {
      return Ok(None);
    };
    admission
      .charge_items::<ObjectId>(counted.shallow.saturating_add(counted.unshallow))
      .map_err(Refusal::PastAdmission)?;
    let mut update = ShallowUpdate {
      shallow: Vec::with_capacity(counted.shallow),
      unshallow: Vec::with_capacity(counted.unshallow),
    };
    walk_shallow_update(bytes, format, &mut |line| match line {
      ShallowLine::Shallow(id) => update.shallow.push(id),
      ShallowLine::Unshallow(id) => update.unshallow.push(id),
    })?;
    Ok(Some((update, counted.took)))
  }

  /// Writes the shallow-update as git's `upload-pack` writes it: each `shallow` line, then each `unshallow` line, no
  /// LF on either (`send_shallow`, `send_unshallow`), then a flush. Refused, nothing written, when its object names
  /// are not all in one format.
  pub fn write(&self, out: &mut impl Out) -> Result<(), WriteRefusal> {
    self.encode(&mut Measure::default())?;
    self.encode(out)
  }

  /// Writes every line, checking each name's format first.
  fn encode(&self, out: &mut impl Out) -> Result<(), WriteRefusal> {
    let format = self
      .shallow
      .iter()
      .chain(&self.unshallow)
      .next()
      .map_or(ObjectFormat::Sha1, ObjectId::format);
    for (word, ids) in [(SHALLOW, &self.shallow), (UNSHALLOW, &self.unshallow)] {
      for id in ids {
        same_format(*id, format)?;
        put_line(out, |line| put_word_id(line, word, id))?;
      }
    }
    pktline::write(Packet::Flush, out).map_err(|refused| WriteRefusal::TooLong { len: refused.len })
  }
}

/// Walks the shallow-update `bytes` begin with, handing each line to `each`: what it counted, or `None` while the bytes
/// do not yet hold its flush.
fn walk_shallow_update(
  bytes: &[u8],
  format: ObjectFormat,
  each: &mut dyn FnMut(ShallowLine),
) -> Result<Option<ShallowCount>, Refusal> {
  let mut packets = Packets::new(bytes);
  let mut counted = ShallowCount::default();
  loop {
    let Some((at, packet)) = packets.take()? else {
      return Ok(None);
    };
    let data = match packet {
      Packet::Data(data) => data,
      Packet::Flush => {
        counted.took = packets.at;
        return Ok(Some(counted));
      }
      Packet::Delim | Packet::ResponseEnd => return Err(Refusal::Unexpected { at: to_u64(at) }),
    };
    let line = read_shallow_line(message_line(at, data)?, format).map_err(refused_line(at))?;
    counted.count(at, line)?;
    each(line);
  }
}

/// Reads one shallow-update line.
fn read_shallow_line(line: &[u8], format: ObjectFormat) -> Result<ShallowLine, LineRefusal> {
  let (word, rest) = split_word(line);
  match (word, rest) {
    (SHALLOW, Some(rest)) => only_id(rest, argument_at(SHALLOW), format).map(ShallowLine::Shallow),
    (UNSHALLOW, Some(rest)) => {
      only_id(rest, argument_at(UNSHALLOW), format).map(ShallowLine::Unshallow)
    }
    (SHALLOW | UNSHALLOW, None) => Err(LineRefusal::Form {
      at: to_u64(line.len()),
    }),
    _ => Err(LineRefusal::Unknown),
  }
}

#[cfg(test)]
#[path = "tests/v0.rs"]
mod tests;
