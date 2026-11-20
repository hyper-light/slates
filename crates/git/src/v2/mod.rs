//! Protocol v2's messages (A-582, part 2; §4.17.2 "The wire" and "A fetch"): the capability advertisement, the command
//! request, `ls-refs`'s arguments and answer, and `fetch`'s arguments and response, decoded and encoded as pure
//! functions over bytes, every one of them a run of pkt-lines ([`crate::pktline`]).
//!
//! A server answers a client's first contact with its capabilities ([`advertisement`]): `version 2`, then one line a
//! capability, `key[=value]`, then a flush. The client then sends one command a request ([`request`]): `command=<name>`
//! and its own capabilities, a delimiter, the command's arguments, a flush. `ls-refs` ([`ls_refs`]) answers one line a
//! ref; `fetch` ([`fetch`]) answers in sections, each opened by its name and closed by a delimiter, in a fixed order:
//! acknowledgments, shallow-info, wanted-refs, packfile-uris, then the packfile, multiplexed on side-band-64k's bands,
//! and a flush.
//!
//! **Sources** (read 2026-11-19, the revision stock git 2.50.1 shipped, the golden vectors' git). The documents \[B\]:
//! `Documentation/gitprotocol-v2.adoc` at v2.50.1 (SHA-256 `2e739965…`; master at `e1ec09b2`, which
//! `research/vcs-servers.md` cites as `64ef6a81…`, differs only in `object-info` and `promisor-remote`, neither read
//! here) and `gitprotocol-common.adoc` (`bcb00e39…`). The code \[C\], v2.50.1: `serve.c` (the request and its
//! capabilities), `ls-refs.c`, `upload-pack.c` (`process_args`, the sections), `fetch-pack.c` and `connect.c` (what
//! git's own client writes and how it reads each answer), `pkt-line.c` and `sideband.c` (lines and bands), `refs.c`
//! (`check_refname_format`).
//!
//! **Lines.** A text line's one trailing LF is optional: "Receivers MUST treat pkt-lines with non-binary data the same
//! whether or not they contain the trailing LF" [gitprotocol-common]. Stock git writes both forms in one request: its
//! client writes `command=fetch`, its capabilities and its flags without an LF and `want`, `have` and `done` with one
//! (`fetch-pack.c`), and its server writes `shallow <id>` without one (`upload-pack.c` `send_shallow`). So every line
//! read here records how it ended ([`Line`], [`Ending`]), and a decoded message is written back to its exact bytes. A
//! text line holds no NUL, where git's C readers would end the line's string, and no LF but its last byte.
//!
//! **Read as git's server reads, written as git writes (A-624).** An object id is the object format's hexadecimal
//! digits, exactly its length and nothing after it, its digits of either case: git's server reads an id with
//! `get_oid_hex`, which takes both (`hex.c`'s `hexval`), and gitprotocol-pack asks it of every receiver ("both MUST
//! treat obj-id as case-insensitive"); an encode writes the id in lower case, as `oid_to_hex` does. A line's LF is
//! optional, as above, and recorded. So every stream stock git wrote encodes back byte for byte, and one with an
//! upper-case id encodes back in git's form. Where git's reader takes what no git writes, the reader here refuses
//! it: bytes after an id (`get_oid_hex` ignores what follows) and a number not in canonical decimal (git's `strtol`
//! with base 0 also takes octal and `0x`). The pkt-line lengths' digits are read in either case, as part 1 reads them
//! (`hexval`), and written in lower case. A ref name is `HEAD` or under `refs/` and passes `check_refname_format`
//! ([`crate::refname`]).
//!
//! **Order is the grammar's.** `fetch`'s sections come in the document's order. git 2.50.1's `upload_pack_v2` wrote
//! `wanted-refs` before `shallow-info`, which its own client refused ("expected 'packfile', received 'shallow-info'"),
//! and git fixed it in `191740edaa4a` ("upload-pack: swap wanted-ref/shallow-info responses", 2026-09-16, with a test
//! in t5703): one machine ([`fetch::Response`]) holds the order, and both the reader and the writer step it, so slates
//! neither writes a section out of order nor reads one.
//!
//! **Bounds (§4.9), none of them a literal.** The input bounds every decode: each line is one pkt-line, at most
//! [`crate::pktline::MAX_DATA`] bytes, and a value is a slice of it. What a decode builds beside the input it borrows,
//! the lists of capabilities, arguments, refs and events, is counted in a first walk and charged exactly to the
//! caller's [`crate::Admission`] before a second walk builds it. The format bounds the rest: a depth is git's `int`, a
//! timestamp git's `timestamp_t`, an id the object format's length. Every refusal is a typed [`Refusal`] naming the
//! byte where the packet at fault begins; an encode checks everything before it writes, so a refused encode writes
//! nothing.

pub mod advertisement;
pub mod bundle_uri;
pub mod fetch;
mod line;
pub mod ls_refs;
pub mod object_info;
pub mod packfile_uris;
pub mod request;

pub use line::{Ending, Line};

use crate::object_name::HexRefusal;
use crate::{PastAdmission, pktline, sideband};

/// Format: the byte between a capability's key and its value [gitprotocol-v2 "Capability Advertisement":
/// `capability = PKT-LINE(key[=value] LF)`].
const ASSIGN: u8 = b'=';

/// Format: the bytes a capability's key may hold besides letters and digits [gitprotocol-v2: `key = 1*(ALPHA | DIGIT |
/// "-_")`].
const KEY_PUNCTUATION: &[u8] = b"-_";

/// A capability: a key, and the value after `=` where it has one. The same shape advertises a server's capabilities
/// and carries a client's in its request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability<'a> {
  /// The key: letters, digits, `-` and `_`, at least one.
  pub key: &'a [u8],
  /// The value after the first `=`, which may itself hold `=`; `None` for a key alone. Any bytes of a text line.
  pub value: Option<&'a [u8]>,
}

impl<'a> Capability<'a> {
  /// The capability a line's text writes, `key[=value]`, refused when its key is not of the grammar.
  pub(crate) fn parse(text: &'a [u8], at: u64) -> Result<Capability<'a>, Refusal> {
    let (key, value) = match text.iter().position(|byte| *byte == ASSIGN) {
      Some(split) => (
        text.get(..split).unwrap_or_default(),
        text.get(split.saturating_add(1)..),
      ),
      None => (text, None),
    };
    let capability = Capability { key, value };
    capability.check(at)?;
    Ok(capability)
  }

  /// Refused when the key is empty or holds a byte the grammar's `key` does not, or the value holds a NUL or an LF.
  pub(crate) fn check(&self, at: u64) -> Result<(), Refusal> {
    let key_ok = !self.key.is_empty()
      && self
        .key
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || KEY_PUNCTUATION.contains(byte));
    if !key_ok {
      return Err(Refusal::Capability { at });
    }
    line::check_text(self.value.unwrap_or_default(), at)
  }

  /// The line's text in its three parts: the key, `=` where there is a value, and the value.
  pub(crate) fn parts(&self) -> [&'a [u8]; 3] {
    match self.value {
      Some(value) => [self.key, b"=", value],
      None => [self.key, b"", b""],
    }
  }
}

/// Why bytes are not a protocol v2 message, or why one cannot be written. Each names where it stopped: the byte where
/// the packet at fault begins, in the bytes read or in the bytes an encode would have written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The bytes are not pkt-lines (part 1's refusal: a length digit that is none, `0003`, a length past 65520, or the
  /// bytes ending inside a packet).
  Packet(pktline::Refusal),
  /// A multiplexed packet with no band byte, or a band other than 1, 2 and 3.
  Band {
    /// Where the packet begins.
    at: u64,
    /// Why.
    refusal: sideband::Refusal,
  },
  /// What the decode would build is past the caller's admission; nothing was built.
  PastAdmission(PastAdmission),
  /// The bytes ended before the message did (an advertisement, request or answer is closed by a flush).
  Unfinished {
    /// Where the bytes ended.
    at: u64,
  },
  /// Bytes after the message's end.
  Trailing {
    /// Where they begin.
    at: u64,
  },
  /// A flush, delimiter, response end or band where the message has none.
  Misplaced {
    /// Where the packet begins.
    at: u64,
  },
  /// A text line holding a NUL, or an LF before its last byte.
  NotText {
    /// Where the line's packet begins.
    at: u64,
  },
  /// A line longer than one packet carries: refused, nothing written.
  TooLong {
    /// Where its packet would have begun.
    at: u64,
    /// The line's data, its band byte included.
    len: u64,
  },
  /// An advertisement that does not begin with `version 2`.
  Version {
    /// Where its first packet begins.
    at: u64,
  },
  /// A capability whose key is empty or holds a byte other than letters, digits, `-` and `_`.
  Capability {
    /// Where its line begins.
    at: u64,
  },
  /// A request's capability that is no capability a client sends (serve.c's "unknown capability").
  UnknownCapability {
    /// Where its line begins.
    at: u64,
  },
  /// An `object-format` with no value, or naming a format git does not know.
  ObjectFormat {
    /// Where its line begins.
    at: u64,
  },
  /// `command=` naming no command this crate reads (serve.c's "invalid command").
  UnknownCommand {
    /// Where its line begins.
    at: u64,
  },
  /// A second `command=` line in one request.
  SecondCommand {
    /// Where its line begins.
    at: u64,
  },
  /// A request with capabilities but no `command=` line ("no command requested").
  NoCommand {
    /// Where the packet that ended its head begins.
    at: u64,
  },
  /// An argument the command does not take, or written as it does not take it (`ls-refs.c` and `upload-pack.c`'s
  /// "unexpected line").
  UnknownArgument {
    /// Where its line begins.
    at: u64,
  },
  /// An object id that is not the object format's hexadecimal digits: of another length, or with a byte that is no
  /// digit.
  ObjectId {
    /// Where its line begins.
    at: u64,
    /// Why, at which digit.
    refusal: HexRefusal,
  },
  /// A ref name that is neither `HEAD` nor a name under `refs/` that `check_refname_format` passes.
  Refname {
    /// Where its line begins.
    at: u64,
  },
  /// A number not written in canonical decimal, or outside the range git reads it in.
  Number {
    /// Where its line begins.
    at: u64,
  },
  /// An argument whose value must name something, and is empty.
  EmptyValue {
    /// Where its line begins.
    at: u64,
  },
  /// What may come once came again: a second `filter` or `packfile-uris` argument (git dies on each), or a ref's
  /// second attribute of one kind.
  Repeated {
    /// Where its line begins.
    at: u64,
  },
  /// `deepen` with `deepen-since` or `deepen-not`, which the document says "Cannot be used with" it.
  DeepenConflict {
    /// Where the second of the pair begins.
    at: u64,
  },
  /// A line where a section's name must stand that names no section.
  UnknownSection {
    /// Where its line begins.
    at: u64,
  },
  /// A section after one it must precede, or a second of one kind.
  SectionOutOfOrder {
    /// Where its header begins.
    at: u64,
  },
  /// An acknowledgments section out of its grammar: `NAK` with `ACK`, a second `NAK`, a line after `ready`, a
  /// delimiter without `ready` or a flush after it.
  Acknowledgment {
    /// Where the line or packet at fault begins.
    at: u64,
  },
  /// A line its section's grammar has no place for.
  Line {
    /// Where its line begins.
    at: u64,
  },
  /// A response ending before its packfile section, which every response but acknowledgments alone carries.
  NoPackfile {
    /// Where the flush begins.
    at: u64,
  },
  /// A packet after a fatal error on band 3, after which the stream ends.
  AfterError {
    /// Where the packet begins.
    at: u64,
  },
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
