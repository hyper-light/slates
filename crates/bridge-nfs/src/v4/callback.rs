//! The NFSv4.1 callback program's messages (RFC 8881 §20): the `CB_COMPOUND` arguments this server sends a client
//! over a session's back channel, and the status of the client's reply. Pure encoding and decoding; the daemon owns
//! the transport (`slates_server::callback`).
//!
//! Every callback compound opens with `CB_SEQUENCE` (§20.9), which names the session and the back channel's slot
//! and sequence id, as a fore-channel compound opens with `SEQUENCE`. A probe is `CB_SEQUENCE` alone (§10.2: "a
//! CB_COMPOUND procedure with a single operation, CB_SEQUENCE, can be used to check the continuity of the
//! backchannel"); a recall is `CB_SEQUENCE` then `CB_RECALL` (§20.2).

use super::types::{SessionId, Stateid};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: `callback_ident`, unused in NFSv4.1 (§20.2: "the server MUST set it to zero").
const CALLBACK_IDENT: u32 = 0;
/// Format: `OP_CB_RECALL` (§20.2).
const OP_CB_RECALL: u32 = 4;
/// Format: `OP_CB_SEQUENCE` (§20.9).
const OP_CB_SEQUENCE: u32 = 11;
/// Format: the back channel's one slot this server calls on.
const SLOT: u32 = 0;

/// Writes the compound header (naming the session's `minor` version) and `CB_SEQUENCE` for `sessionid` at
/// `sequenceid` on the one slot.
fn open_compound(
  writer: &mut XdrWriter,
  (sessionid, minor, sequenceid): (&SessionId, u32, u32),
  operations: u32,
) {
  writer.opaque(b""); // tag
  writer.u32(minor);
  writer.u32(CALLBACK_IDENT);
  writer.u32(operations);
  writer.u32(OP_CB_SEQUENCE);
  writer.fixed(sessionid);
  writer.u32(sequenceid);
  writer.u32(SLOT);
  writer.u32(SLOT); // highest slot
  writer.bool(false); // cache this
  writer.u32(0); // no referring call lists
}

/// A probe of `sessionid`'s back channel: `CB_SEQUENCE` alone.
pub fn probe(sessionid: &SessionId, minor: u32, sequenceid: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  open_compound(&mut writer, (sessionid, minor, sequenceid), 1);
  writer.into_bytes()
}

/// A recall of the delegation `stateid` on the file `fh` (§20.2): `CB_SEQUENCE`, then `CB_RECALL`. `truncate` tells
/// the client the file is being truncated to zero, so it need not flush data past that.
pub fn recall(
  (sessionid, minor, sequenceid): (&SessionId, u32, u32),
  stateid: &Stateid,
  truncate: bool,
  fh: &[u8],
) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  open_compound(&mut writer, (sessionid, minor, sequenceid), 2);
  writer.u32(OP_CB_RECALL);
  stateid.encode(&mut writer);
  writer.bool(truncate);
  writer.opaque(fh);
  writer.into_bytes()
}

/// The status of a `CB_COMPOUND4res` (its first word), or `None` for results too short to carry one.
pub fn status(results: &[u8]) -> Option<u32> {
  XdrReader::new(results).u32().ok()
}
