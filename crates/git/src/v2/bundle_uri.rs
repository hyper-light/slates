//! `bundle-uri` (A-624; gitprotocol-v2 "bundle-uri", `technical/bundle-uri.adoc` and `config/bundle.adoc` \[B\];
//! `bundle-uri.c` `bundle_uri_command`, `bundle_uri_parse_line` and `bundle_list_update`, `connect.c`
//! `get_remote_bundle_uri` \[C\]): its answer, the bundles a client may seed a clone or a fetch from.
//!
//! The request takes no argument: `bundle_uri_command` dies on one, and [`super::request`] refuses it. git's client
//! sends its capabilities, then `command=bundle-uri`, a delimiter and the flush. The answer is a line for each key of
//! the list, `<key>=<value>`, then a flush; git's server writes every `bundle.*` key of its configuration that way,
//! without an LF. A key names the list's own settings (`bundle.version`, `bundle.mode`, `bundle.heuristic`) or one
//! bundle's (`bundle.<id>.uri`, `bundle.<id>.creationtoken`).
//!
//! **Read as git's client reads a line** (`bundle_uri_parse_line`, `bundle_list_update`): a line splits at its first
//! `=`; a line with none, with an empty key or an empty value, or with a key outside `bundle.` is refused, and so are a
//! version other than 1 and a mode other than `all` and `any`, as git's client errs on each. Keys git does not know,
//! and a heuristic it does not know, are kept by name, as it ignores them. git's client also errs on a bundle's second
//! `uri`, a rule about the list rather than a line; [`answer`] never writes one, and the line reader leaves it to the
//! list's reader.
//!
//! **Written from a list.** [`BundleList`] is what a repository offers ([`Bundles`]); [`answer`] writes it as git's
//! server writes the same configuration: `bundle.version=1`, `bundle.mode`, `bundle.heuristic` where there is one, then
//! each bundle's `uri` and its `creationtoken` where it has one, keys in git's configuration case (section and name in
//! lower case, a bundle's id as given), no LF. A repository offering no list is answered by the flush alone.

use super::Refusal;
use super::line::{self, Line, Packets, Part};
use crate::Admission;
use crate::out::{Measure, Out};
use crate::pktline::Packet;

/// Format: the byte between a line's key and its value [gitprotocol-v2 "bundle-uri": `<key>=<value>`].
const ASSIGN: u8 = b'=';

/// Format: the namespace every key is in [gitprotocol-v2 "bundle-uri": "a config key from the `bundle.*` namespace"].
const NAMESPACE: &[u8] = b"bundle.";

/// Format: the list's version key, and the one version git reads (`bundle_list_update`: "if (version != 1)").
const VERSION: &[u8] = b"bundle.version";

/// Format: the one version a list may name.
const VERSION_ONE: &[u8] = b"1";

/// Format: the list's mode key.
const MODE: &[u8] = b"bundle.mode";

/// Format: the list's heuristic key.
const HEURISTIC: &[u8] = b"bundle.heuristic";

/// Format: the separator before a bundle's key, after its id.
const DOT: &[u8] = b".";

/// Format: the key naming a bundle's URI, after its id (`bundle_list_update`'s "uri").
const URI: &[u8] = b"uri";

/// Format: the key naming a bundle's creation token, in git's configuration case (`bundle_list_update`'s
/// "creationtoken"; `config/bundle.adoc` writes it `creationToken`, and git's configuration reads a name in lower case).
const CREATION_TOKEN: &[u8] = b"creationtoken";

/// Format: each mode a list may have, and its value (`bundle_list_update`: "all", "any").
const MODES: [(Mode, &[u8]); 2] = [(Mode::All, b"all"), (Mode::Any, b"any")];

/// Format: the heuristic git knows, and its value (`bundle-uri.c`'s `heuristics`: "creationToken").
const CREATION_TOKEN_HEURISTIC: &[u8] = b"creationToken";

/// Format: the most parts a line [`answer`] writes takes: `bundle.`, an id, `.`, a key, `=` and a value.
const ANSWER_LINE_PARTS: usize = 6;

/// One line of a `bundle-uri` answer: a key of the `bundle.*` namespace and its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair<'a> {
  /// The key, `bundle.` and the rest: bytes of a text line, no `=`.
  pub key: &'a [u8],
  /// The value: bytes of a text line, at least one.
  pub value: &'a [u8],
}

impl<'a> Pair<'a> {
  /// The pair a line's text writes: refused as git's client refuses the line.
  fn parse(text: &'a [u8], at: u64) -> Result<Pair<'a>, Refusal> {
    let split = text
      .iter()
      .position(|byte| *byte == ASSIGN)
      .ok_or(Refusal::Line { at })?;
    let pair = Pair {
      key: text.get(..split).unwrap_or_default(),
      value: text.get(split.saturating_add(1)..).unwrap_or_default(),
    };
    pair.check(at)?;
    Ok(pair)
  }

  /// Refused when git's client would refuse the line: no key or no value, a key outside `bundle.` or holding `=`,
  /// a version other than 1, a mode other than `all` and `any`; or bytes no text line holds.
  fn check(&self, at: u64) -> Result<(), Refusal> {
    line::check_text(self.key, at)?;
    line::check_text(self.value, at)?;
    let shaped =
      self.key.starts_with(NAMESPACE) && !self.key.contains(&ASSIGN) && !self.value.is_empty();
    if !shaped {
      return Err(Refusal::Line { at });
    }
    match self.key {
      VERSION if self.value != VERSION_ONE => Err(Refusal::Number { at }),
      MODE if !MODES.iter().any(|(_, word)| *word == self.value) => Err(Refusal::Line { at }),
      _ => Ok(()),
    }
  }
}

/// How a client uses a list's bundles [`config/bundle.adoc`: `bundle.mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
  /// `all`: the bundles together are what the list offers.
  All,
  /// `any`: any one bundle is enough.
  Any,
}

/// How a client chooses among a list's bundles [`config/bundle.adoc`: `bundle.heuristic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Heuristic {
  /// `creationToken`: by each bundle's creation token, newest first, as far back as the client needs.
  CreationToken,
}

/// A bundle a list offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bundle<'a> {
  /// Its id, the configuration's subsection: at least one byte, no `=`.
  pub id: &'a [u8],
  /// Where a client downloads it, absolute or relative to the server's URL.
  pub uri: &'a [u8],
  /// Its creation token, for the `creationToken` heuristic.
  pub creation_token: Option<u64>,
}

/// The bundle list a repository offers, version 1, the only one git reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleList<'a> {
  /// How a client uses the bundles.
  pub mode: Mode,
  /// How a client chooses among them, where the list says.
  pub heuristic: Option<Heuristic>,
  /// The bundles, in the order written.
  pub bundles: &'a [Bundle<'a>],
}

/// What a repository answers `bundle-uri` from.
pub trait Bundles {
  /// The list of bundles the repository offers, or `None` when it offers none.
  fn bundle_list(&self) -> Option<BundleList<'_>>;
}

/// Walks the answer in `bytes`, handing each line to `each`: how many there were.
fn walk<'a>(bytes: &'a [u8], mut each: impl FnMut(Line<Pair<'a>>)) -> Result<usize, Refusal> {
  let mut packets = Packets::new(bytes);
  let mut count = 0usize;
  while let Some((text, ending, at)) = packets.next_line()? {
    each(Line {
      value: Pair::parse(text, at)?,
      ending,
    });
    count = count.saturating_add(1);
  }
  packets.finish()?;
  Ok(count)
}

/// Decodes a `bundle-uri` answer, whole. Refused when a line is one git's client refuses, a packet comes where the
/// answer has none, the bytes end before its flush or go on after it; or when its lines would take more than
/// `admission` has left, charged before the list is made.
pub fn decode_answer<'a>(
  bytes: &'a [u8],
  admission: &mut Admission,
) -> Result<Vec<Line<Pair<'a>>>, Refusal> {
  let _ = (bytes, admission);
  Err(Refusal::Unfinished { at: 0 })
}

/// Writes a `bundle-uri` answer of `lines`, each as it ended, then its flush. Refused, nothing written, when a line is
/// one git's client refuses or longer than a packet carries.
pub fn encode_answer(lines: &[Line<Pair<'_>>], out: &mut impl Out) -> Result<(), Refusal> {
  let _ = (lines, out);
  Err(Refusal::Line { at: 0 })
}

/// Writes the lines to `out`, checking each before it writes it.
fn write_lines(lines: &[Line<Pair<'_>>], out: &mut impl Out) -> Result<(), Refusal> {
  let mut at = 0u64;
  for line in lines {
    line.value.check(at)?;
    let parts = [
      Part::Bytes(line.value.key),
      Part::Bytes(&[ASSIGN]),
      Part::Bytes(line.value.value),
      Part::Bytes(line.ending.bytes()),
    ];
    at = at.saturating_add(line::put_line(None, &parts, at, out)?);
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

/// Answers a `bundle-uri` request from `bundles`, as git's server writes the same configuration: the flush alone when
/// it offers no list. Refused, nothing written, when a bundle's id is empty or holds `=`, its URI is empty, or a part
/// holds bytes no text line holds or a line is longer than a packet carries.
pub fn answer(bundles: &impl Bundles, out: &mut impl Out) -> Result<(), Refusal> {
  let _ = (bundles, out);
  Err(Refusal::Line { at: 0 })
}

/// Writes `list` to `out`, checking each line before it writes it.
fn write_list(list: Option<&BundleList<'_>>, out: &mut impl Out) -> Result<(), Refusal> {
  let mut at = 0u64;
  if let Some(list) = list {
    let mode = MODES
      .iter()
      .find(|(mode, _)| *mode == list.mode)
      .map_or(b"".as_slice(), |(_, word)| *word);
    at = put_pair(&[VERSION], VERSION_ONE, at, out)?;
    at = at.saturating_add(put_pair(&[MODE], mode, at, out)?);
    if list.heuristic == Some(Heuristic::CreationToken) {
      at = at.saturating_add(put_pair(&[HEURISTIC], CREATION_TOKEN_HEURISTIC, at, out)?);
    }
    for bundle in list.bundles {
      at = at.saturating_add(put_bundle(bundle, at, out)?);
    }
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

/// Writes one bundle's lines: its `uri`, then its `creationtoken` where it has one. The bytes written.
fn put_bundle(bundle: &Bundle<'_>, at: u64, out: &mut impl Out) -> Result<u64, Refusal> {
  if bundle.id.is_empty() || bundle.id.contains(&ASSIGN) {
    return Err(Refusal::Line { at });
  }
  let mut written = put_pair(&[NAMESPACE, bundle.id, DOT, URI], bundle.uri, at, out)?;
  if let Some(token) = bundle.creation_token {
    let key = [NAMESPACE, bundle.id, DOT, CREATION_TOKEN];
    written = written.saturating_add(put_decimal_pair(
      &key,
      token,
      at.saturating_add(written),
      out,
    )?);
  }
  Ok(written)
}

/// Writes `key=value`, the key in its parts (at most five): the bytes written. Refused when the line is one git's client
/// refuses.
fn put_pair(key: &[&[u8]], value: &[u8], at: u64, out: &mut impl Out) -> Result<u64, Refusal> {
  check_key(key, at)?;
  line::check_text(value, at)?;
  if value.is_empty() {
    return Err(Refusal::Line { at });
  }
  let mut parts = [Part::Bytes(b""); ANSWER_LINE_PARTS];
  let pieces = key.iter().copied().chain([&[ASSIGN][..], value]);
  for (slot, piece) in parts.iter_mut().zip(pieces) {
    *slot = Part::Bytes(piece);
  }
  line::put_line(None, &parts, at, out)
}

/// Writes `key=<value in decimal>`, the key in its four parts: the bytes written.
fn put_decimal_pair(
  key: &[&[u8]; 4],
  value: u64,
  at: u64,
  out: &mut impl Out,
) -> Result<u64, Refusal> {
  check_key(key, at)?;
  let [namespace, id, dot, name] = *key;
  let parts = [
    Part::Bytes(namespace),
    Part::Bytes(id),
    Part::Bytes(dot),
    Part::Bytes(name),
    Part::Bytes(&[ASSIGN]),
    Part::Decimal(value),
  ];
  line::put_line(None, &parts, at, out)
}

/// Refused when a part of a key holds bytes no text line holds.
fn check_key(key: &[&[u8]], at: u64) -> Result<(), Refusal> {
  key.iter().try_for_each(|part| line::check_text(part, at))
}

#[cfg(test)]
#[path = "tests/bundle_uri.rs"]
mod tests;
