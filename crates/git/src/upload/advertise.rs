//! The capability advertisement upload-pack answers, and a request held to it (A-640 part 2; §4.17.2 "The wire";
//! `serve.c` `protocol_v2_advertise_capabilities`, `process_request`, `parse_command` and `receive_client_capability`;
//! `upload-pack.c` `upload_pack_advertise`; `ls-refs.c` `ls_refs_advertise`).
//!
//! **What is advertised**, in `serve.c`'s order: `agent`, `ls-refs=unborn` (the one feature git lists for it; `symrefs`,
//! `peel` and `ref-prefix` are its base arguments), `fetch` with every feature §4.17.2 names, in the order git lists
//! them (`shallow wait-for-done filter ref-in-want sideband-all packfile-uris`), `server-option`, `object-format` (the
//! repository's), `session-id` and `object-info`. `bundle-uri` is advertised once the repository offers a bundle list
//! (A-624); none does yet, so it is not.
//!
//! **What a request is held to**, as `serve.c` holds it: its command is one advertised (`invalid command`), each
//! capability it sends is one advertised that is no command (`unknown capability`: `promisor-remote` is not advertised),
//! and its object format, SHA-1 where it names none, is the repository's (`mismatched object format`). The features a
//! `fetch` or `ls-refs` uses need no check of their own: every one is advertised.

use super::{AGENT, Refusal, charged};
use crate::Admission;
use crate::object_name::ObjectFormat;
use crate::v2::advertisement::{self, Advertisement};
use crate::v2::request::{Command, CommandRequest};
use crate::v2::{Capability, Ending, Line};

/// Format: the capability naming the repository's object format, whose value is filled in when advertised.
const OBJECT_FORMAT: &[u8] = b"object-format";

/// Format: each capability advertised and its value, in `serve.c`'s order (`object-format`'s value is the
/// repository's, filled in when advertised).
const CAPABILITIES: [(&[u8], Option<&[u8]>); 7] = [
  (b"agent", Some(AGENT.as_bytes())),
  (b"ls-refs", Some(b"unborn")),
  (
    b"fetch",
    Some(b"shallow wait-for-done filter ref-in-want sideband-all packfile-uris"),
  ),
  (b"server-option", None),
  (OBJECT_FORMAT, None),
  (b"session-id", None),
  (b"object-info", None),
];

/// Format: the commands advertised: those of [`CAPABILITIES`] a client may name in `command=`.
const COMMANDS: [Command; 3] = [Command::LsRefs, Command::Fetch, Command::ObjectInfo];

/// Format: the capabilities a client may send, those of [`CAPABILITIES`] that are no command.
const CLIENT_CAPABILITIES: [&[u8]; 4] = [b"agent", b"server-option", OBJECT_FORMAT, b"session-id"];

/// The advertisement's capabilities for a repository whose objects are named in `format`.
fn capabilities(format: ObjectFormat) -> [Line<Capability<'static>>; CAPABILITIES.len()] {
  CAPABILITIES.map(|(key, value)| {
    let value = if key == OBJECT_FORMAT {
      Some(format.name().as_bytes())
    } else {
      value
    };
    Line::lf(Capability { key, value })
  })
}

/// The advertisement for a repository whose objects are named in `format`, the list and the bytes charged to
/// `admission` before they are made.
pub(super) fn answer(format: ObjectFormat, admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
  admission
    .charge_items::<Line<Capability<'static>>>(CAPABILITIES.len())
    .map_err(Refusal::PastAdmission)?;
  let advertisement = Advertisement {
    version: Ending::Lf,
    capabilities: capabilities(format).to_vec(),
  };
  charged(admission, |out| {
    advertisement::encode(&advertisement, out).map_err(Refusal::Wire)
  })
  .map(|(bytes, ())| bytes)
}

/// Refused when `request` is not one the advertisement allows (`process_request`): a command not advertised, a
/// capability not advertised or one that is a command, or an object format other than the repository's `format`.
pub(super) fn check(request: &CommandRequest<'_>, format: ObjectFormat) -> Result<(), Refusal> {
  let command = request.command();
  if !COMMANDS.contains(&command) {
    return Err(Refusal::Command(command));
  }
  if request
    .capabilities()
    .any(|capability| !CLIENT_CAPABILITIES.contains(&capability.key))
  {
    return Err(Refusal::Capability);
  }
  match request.object_format() {
    Some(client) if client == format => Ok(()),
    client => Err(Refusal::ObjectFormat {
      server: format,
      client,
    }),
  }
}
