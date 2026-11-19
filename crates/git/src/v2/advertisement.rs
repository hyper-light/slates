//! The capability advertisement (A-582, part 2; gitprotocol-v2 "Capability Advertisement" [B]; `serve.c`
//! `protocol_v2_advertise_capabilities`, `connect.c` `process_capabilities_v2` and `server_supports_feature` [C]).
//!
//! A server's first answer: `version 2`, then one line a capability, `key[=value]`, then a flush:
//!
//! ```text
//! capability-advertisement = protocol-version capability-list flush-pkt
//! protocol-version = PKT-LINE("version 2" LF)
//! capability = PKT-LINE(key[=value] LF)
//! ```
//!
//! A command's value lists the features it supports beyond its base, separated by spaces (`fetch=shallow
//! wait-for-done`). Clients ignore keys they do not know, so the list is read whatever its keys; each key is held to
//! the grammar's `key`, and each value is any bytes of a text line. The document's `value` names fewer bytes than its
//! `agent` and `session-id` sections allow (`~`, `'`, `"`, `` ` `` and `|`), and its `server-option` section allows any
//! byte but NUL and LF, which is the one rule every value here is held to.

use super::line::{self, Ending, Line, Packets, Part};
use super::{Capability, Refusal};
use crate::Admission;
use crate::object_name::ObjectFormat;
use crate::out::{Measure, Out};
use crate::pktline::Packet;

/// Format: the advertisement's first line's text [gitprotocol-v2: `protocol-version = PKT-LINE("version 2" LF)`].
const VERSION_2: &[u8] = b"version 2";

/// Format: the byte between the features a command's value lists [gitprotocol-v2 "ls-refs", "fetch": "a space
/// separated list of features"].
const FEATURE_SEPARATOR: u8 = b' ';

/// Format: the capability naming the hash the server's objects are named by [gitprotocol-v2 "object-format"].
pub(crate) const OBJECT_FORMAT: &[u8] = b"object-format";

/// A capability advertisement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertisement<'a> {
  /// How the `version 2` line ended.
  pub version: Ending,
  /// The capabilities, in the order advertised.
  pub capabilities: Vec<Line<Capability<'a>>>,
}

impl<'a> Advertisement<'a> {
  /// The first capability named `key`, as git's client finds one (`server_supports_v2`): `None` when none is
  /// advertised, `Some(None)` when one is without a value.
  pub fn get(&self, key: &[u8]) -> Option<Option<&'a [u8]>> {
    self
      .capabilities
      .iter()
      .find(|line| line.value.key == key)
      .map(|line| line.value.value)
  }

  /// Whether `key` is advertised with `feature` among the features its value lists (`server_supports_feature`).
  pub fn supports(&self, key: &[u8], feature: &[u8]) -> bool {
    self.get(key).flatten().is_some_and(|value| {
      value
        .split(|byte| *byte == FEATURE_SEPARATOR)
        .any(|word| word == feature)
    })
  }

  /// The object format the server names; SHA-1 when it names none ("If not specified, the server is assumed to only
  /// handle SHA-1" [gitprotocol-v2 "object-format"]); `None` for a value git does not know, or none.
  pub fn object_format(&self) -> Option<ObjectFormat> {
    match self.get(OBJECT_FORMAT) {
      None => Some(ObjectFormat::Sha1),
      Some(value) => ObjectFormat::from_name(value?).ok(),
    }
  }
}

/// Decodes the advertisement `bytes` hold, whole: refused when it does not open with `version 2`, holds a line that
/// is not a capability or a packet other than its closing flush, ends before that flush, or goes on after it; or when
/// its list of capabilities would take more than `admission` has left, charged before the list is made.
pub fn decode<'a>(
  bytes: &'a [u8],
  admission: &mut Admission,
) -> Result<Advertisement<'a>, Refusal> {
  let (_, count) = walk(bytes, |_| {})?;
  admission
    .charge_items::<Line<Capability<'a>>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut capabilities = Vec::with_capacity(count);
  let (version, _) = walk(bytes, |capability| capabilities.push(capability))?;
  Ok(Advertisement {
    version,
    capabilities,
  })
}

/// Writes `advertisement` as git writes one, each line as it ended. Refused, nothing written, when a capability is
/// not of the grammar or a line is longer than a packet carries.
pub fn encode(advertisement: &Advertisement<'_>, out: &mut impl Out) -> Result<(), Refusal> {
  write(advertisement, &mut Measure::default())?;
  write(advertisement, out)
}

/// Walks the advertisement in `bytes`, handing each capability to `each`: how the version line ended, and how many
/// capabilities there were.
fn walk<'a>(
  bytes: &'a [u8],
  mut each: impl FnMut(Line<Capability<'a>>),
) -> Result<(Ending, usize), Refusal> {
  let mut packets = Packets::new(bytes);
  let version = read_version(&mut packets)?;
  let mut count = 0usize;
  while let Some((text, ending, at)) = packets.next_line()? {
    each(Line {
      value: Capability::parse(text, at)?,
      ending,
    });
    count = count.saturating_add(1);
  }
  packets.finish()?;
  Ok((version, count))
}

/// Reads the version line: how it ended.
fn read_version(packets: &mut Packets<'_>) -> Result<Ending, Refusal> {
  let at = packets.at();
  match packets.next_packet()? {
    None => Err(Refusal::Unfinished { at }),
    Some((Packet::Data(data), at)) => match line::text(data, at)? {
      (text, ending) if text == VERSION_2 => Ok(ending),
      _ => Err(Refusal::Version { at }),
    },
    Some((_, at)) => Err(Refusal::Version { at }),
  }
}

/// Writes `advertisement` to `out`, checking each line before it writes it.
fn write(advertisement: &Advertisement<'_>, out: &mut impl Out) -> Result<(), Refusal> {
  let version = [
    Part::Bytes(VERSION_2),
    Part::Bytes(advertisement.version.bytes()),
  ];
  let mut at = line::put_line(None, &version, 0, out)?;
  for capability in &advertisement.capabilities {
    capability.value.check(at)?;
    let [key, assign, value] = capability.value.parts();
    let parts = [
      Part::Bytes(key),
      Part::Bytes(assign),
      Part::Bytes(value),
      Part::Bytes(capability.ending.bytes()),
    ];
    at = at.saturating_add(line::put_line(None, &parts, at, out)?);
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

#[cfg(test)]
#[path = "tests/advertisement.rs"]
mod tests;
