//! receive-pack's ref advertisement (A-620 part 1; gitprotocol-pack "Reference Discovery"; git's `show_ref` and
//! `write_head_info`, v2.50.1): each ref as `id SP name`, the first followed by a NUL and the capabilities in git's
//! order, an empty repository's one line naming `capabilities^{}` at the zero id, then a flush.

use super::Refusal;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::out::Out;
use crate::pktline::{self, Packet};

/// Format: the capabilities receive-pack always advertises, in `show_ref`'s order.
const ALWAYS: &[u8] = b"report-status report-status-v2 delete-refs side-band-64k quiet";
/// Format: `atomic`, when advertised.
const ATOMIC: &[u8] = b" atomic";
/// Format: `ofs-delta`, when advertised.
const OFS_DELTA: &[u8] = b" ofs-delta";
/// Format: `push-options`, when advertised.
const PUSH_OPTIONS: &[u8] = b" push-options";
/// Format: `session-id=`, before the session's id.
const SESSION_ID: &[u8] = b" session-id=";
/// Format: `object-format=`, before the repository's format.
const OBJECT_FORMAT: &[u8] = b" object-format=";
/// Format: `agent=`, before the server's agent.
const AGENT: &[u8] = b" agent=";
/// Format: the name an empty repository's one line gives (`write_head_info`).
const NO_REFS: &[u8] = b"capabilities^{}";
/// Format: the byte between the first line's ref and the capabilities.
const NUL: u8 = 0;
/// Format: the byte between a line's id and its name.
const SPACE: u8 = b' ';
/// Format: the byte every advertised line ends with.
const LF: u8 = b'\n';

/// What receive-pack advertises: its repository's format and the capabilities a deployment chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advertised<'a> {
  /// The repository's object format.
  pub format: ObjectFormat,
  /// `atomic` (git: `receive.advertiseAtomic`, on by default).
  pub atomic: bool,
  /// `ofs-delta` (git: `repack.useDeltaBaseOffset`, on by default).
  pub ofs_delta: bool,
  /// `push-options` (git: `receive.advertisePushOptions`).
  pub push_options: bool,
  /// `session-id=`, when the deployment tells clients its session (git: `transfer.advertiseSID`).
  pub session_id: Option<&'a [u8]>,
  /// `agent=`.
  pub agent: &'a [u8],
}

/// Writes receive-pack's advertisement of `refs` (each an id and a name, in the order to advertise them: git's is by
/// name) as git writes it. Refused, the lines before it written, for a line past a pkt-line's room.
pub fn write_advertisement(
  refs: &[(ObjectId, &[u8])],
  advertised: &Advertised<'_>,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let zeros = vec![0u8; advertised.format.raw_len()];
  let zero = ObjectId::from_bytes(advertised.format, &zeros);
  let mut lines = refs.iter().map(|(id, name)| (*id, *name));
  let first = lines.next().or_else(|| zero.map(|zero| (zero, NO_REFS)));
  if let Some((id, name)) = first {
    let mut line = ref_line(id, name);
    line.push(NUL);
    line.extend_from_slice(&capabilities(advertised));
    line.push(LF);
    write_line(Packet::Data(&line), out)?;
  }
  for (id, name) in lines {
    let mut line = ref_line(id, name);
    line.push(LF);
    write_line(Packet::Data(&line), out)?;
  }
  write_line(Packet::Flush, out)
}

/// `id SP name`, a ref's line before its end.
fn ref_line(id: ObjectId, name: &[u8]) -> Vec<u8> {
  let mut line = id.to_string().into_bytes();
  line.push(SPACE);
  line.extend_from_slice(name);
  line
}

/// The capabilities `advertised` names, in `show_ref`'s order.
fn capabilities(advertised: &Advertised<'_>) -> Vec<u8> {
  let mut list = ALWAYS.to_vec();
  for (on, word) in [
    (advertised.atomic, ATOMIC),
    (advertised.ofs_delta, OFS_DELTA),
    (advertised.push_options, PUSH_OPTIONS),
  ] {
    if on {
      list.extend_from_slice(word);
    }
  }
  if let Some(session) = advertised.session_id {
    list.extend_from_slice(SESSION_ID);
    list.extend_from_slice(session);
  }
  list.extend_from_slice(OBJECT_FORMAT);
  list.extend_from_slice(advertised.format.name().as_bytes());
  list.extend_from_slice(AGENT);
  list.extend_from_slice(advertised.agent);
  list
}

/// Writes `packet`, or refuses a data packet too long for one.
pub(super) fn write_line(packet: Packet<'_>, out: &mut impl Out) -> Result<(), Refusal> {
  pktline::write(packet, out).map_err(|too_long| Refusal::LineTooLong { len: too_long.len })
}
