//! The `pushkey` command (A-758; §4.17.3 "The wire surface"; 7.2's `wireprotov1server.pushkey`, `pushkey.py`'s `push`
//! and `wireprotov1peer.pushkey` \[C\]): one key of one namespace moved from the value the client saw to another,
//! answered `1` where it moved and `0` where it did not, then any words for the client, each of whose lines it prints
//! after `remote: `. A stock 7.2 client sends it for `hg debugpushkey` and for a bookmark or phase it moves on a server
//! whose bundle2 capabilities lack the parts that move them; it calls `listkeys` (served by [`super::pull::listkeys`])
//! only on a server that lists `pushkey` among its capabilities, which is why both are advertised together.
//!
//! **One write path** (CLAUDE.md §2 item 7). The command is landed as the bundle2 part of the same name is, never by a
//! second path: [`bundle`] writes the request as a push of one advisory `pushkey` part after a `replycaps` part, which
//! the caller hands to the push driver with the `force` heads argument (a pushkey names its own old value, the check
//! that holds it, and 7.2 judges no heads for it), and [`answer`] reads the driver's `reply:pushkey` back. So a key
//! moved by the command is judged by the repository's policy, its hooks and its race checks exactly as a key moved by a
//! push's part, and is recorded in the same entry kind. The part is advisory, so a key that does not move is answered
//! `0`, as 7.2's `pushkey` answers `False`, never an abort.
//!
//! **Namespaces.** `bookmarks` and `phases` are moved ([`crate::push::classify`]). Any other (7.2's `obsolete`, whose
//! markers this server does not take, or one an extension adds) is refused before anything is landed
//! ([`Refusal::UnknownNamespace`]) and answered `0` with slates' words naming it, where 7.2 answers a namespace it lacks
//! with a bare `0`: the client learns why, and its exit status is the one 7.2's gives.

use super::session::Refused;
use crate::bundle2::{Bundle2, Event, Part};
use crate::push::Outcome;
use crate::pushkey::{self, Request};
use crate::read::to_u64;
use crate::wire::{Arguments, Spec};
use crate::{Admission, PastAdmission};

/// Format: the command's name (`wireprotov1server.commands`).
pub const COMMAND: &[u8] = b"pushkey";

/// Format: the namespaces a key is moved in, the two a push's parts move (`pushkey.py`'s table less `obsolete`).
const MOVED_NAMESPACES: [&[u8]; 2] = [b"bookmarks", b"phases"];

/// Format: the advisory part a request is landed as: the pushkey part's type in lower case (bundle2's advisory bit).
const ADVISORY_PART: &[u8] = b"pushkey";

/// Format: the part that asks the driver for a reply (`replycaps`), and the capability blob it carries: bundle2's
/// version alone, all a reply of `reply:pushkey` needs.
const REPLY_CAPS_PART: &[u8] = b"replycaps";
/// Format: see [`REPLY_CAPS_PART`].
const REPLY_CAPS_BLOB: &[u8] = b"HG20";

/// Format: the ids of the two parts, in stream order; the `reply:pushkey` answering the second carries its id.
const REPLY_CAPS_ID: u32 = 0;
/// Format: see [`REPLY_CAPS_ID`].
const PUSHKEY_ID: u32 = 1;

/// Format: the answer's first line, the result as 7.2's `b'%d\n'` writes it.
const MOVED: &[u8] = b"1\n";
/// Format: see [`MOVED`].
const NOT_MOVED: &[u8] = b"0\n";

/// Format: slates' words for a namespace no key is moved in, before its name.
const UNKNOWN_NAMESPACE: &[u8] = b"pushkey: no key of namespace '";
/// Format: see [`UNKNOWN_NAMESPACE`].
const UNKNOWN_NAMESPACE_AFTER: &[u8] = b"' is moved by this server\n";

/// Format: slates' words for a request its push's bundle cannot carry.
const UNCARRIED: &[u8] = b"pushkey: the request cannot be carried in a push\n";
/// Format: slates' words for a request past what its push may hold.
const PAST_ROOM: &[u8] = b"pushkey: the request is past what its push may hold\n";

/// Format: the line end that ends each line of words the client prints.
const LINE_END: u8 = b'\n';

/// Why a `pushkey` request was refused before anything was landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A namespace no key is moved in here (the module doc's namespaces).
  UnknownNamespace,
  /// A request the push's bundle could not carry.
  Unencodable,
  /// More than the request's admission had left.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// The request `arguments` name, read by the command's `spec`; refused naming the first argument it lacks.
pub(crate) fn request<'a>(
  arguments: &Arguments<'a>,
  spec: Spec<'_>,
) -> Result<Request<'a>, Refused> {
  request_by(|name| arguments.get(spec, name))
}

/// The request whose arguments `argument` finds by name (a batch's call, A-569); refused naming the first it lacks.
pub(crate) fn request_by<'a>(
  argument: impl Fn(&[u8]) -> Option<&'a [u8]>,
) -> Result<Request<'a>, Refused> {
  let [namespace, key, old, new] =
    pushkey::PARAMETERS.map(|name| argument(name).ok_or(Refused::MissingArgument { name }));
  Ok(Request {
    namespace: namespace?,
    key: key?,
    old: old?,
    new: new?,
  })
}

/// The push `request` is landed as (the module doc's one write path): a bundle2 stream of a `replycaps` part and one
/// advisory `pushkey` part, charged to `admission` before it is written; refused for a namespace no key is moved in.
pub fn bundle(request: &Request<'_>, admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
  if !MOVED_NAMESPACES.contains(&request.namespace) {
    return Err(Refusal::UnknownNamespace);
  }
  let reply_caps = Part {
    kind: REPLY_CAPS_PART,
    id: REPLY_CAPS_ID,
    params: Vec::new(),
    mandatory_params: 0,
    depth: 0,
  };
  let moved = Part {
    kind: ADVISORY_PART,
    ..pushkey::part(PUSHKEY_ID, request)
  };
  let stream = Bundle2 {
    params: Vec::new(),
    parts: vec![reply_caps, moved],
    events: vec![
      Event::Open(0),
      Event::Chunk(REPLY_CAPS_BLOB),
      Event::Close,
      Event::Open(1),
      Event::Close,
    ],
  };
  let length = stream.encoded_len().map_err(|_| Refusal::Unencodable)?;
  admission.charge(length)?;
  let mut out = Vec::new();
  stream.encode(&mut out).map_err(|_| Refusal::Unencodable)?;
  Ok(out)
}

/// The answer to a request the push driver came to `outcome` on (the module doc): `1` where its `reply:pushkey` says the
/// key moved, else `0` and the words of what refused it, each line printed by the client after `remote: `; charged to
/// `admission` before it is made.
pub fn answer(outcome: &Outcome, admission: &mut Admission) -> Result<Vec<u8>, PastAdmission> {
  let words = match outcome {
    Outcome::Accepted(reply) if reply.pushkey_moved(PUSHKEY_ID) == Some(true) => {
      return charged(MOVED.to_vec(), admission);
    }
    Outcome::Accepted(_) => Vec::new(),
    Outcome::Refused(reply) => reply.error_lines(),
    Outcome::Unprocessed(words) => ended(words),
    Outcome::Bundle1 => ended(super::session::BUNDLE2_REQUIRED),
  };
  charged([NOT_MOVED, &words].concat(), admission)
}

/// The answer to a request refused before anything was landed: `0` and slates' words for `refusal` (the module doc's
/// namespaces), charged to `admission` first.
pub fn refused(
  refusal: Refusal,
  request: &Request<'_>,
  admission: &mut Admission,
) -> Result<Vec<u8>, PastAdmission> {
  let words = match refusal {
    Refusal::UnknownNamespace => [
      UNKNOWN_NAMESPACE,
      request.namespace,
      UNKNOWN_NAMESPACE_AFTER,
    ]
    .concat(),
    Refusal::Unencodable => UNCARRIED.to_vec(),
    Refusal::PastAdmission(_) => PAST_ROOM.to_vec(),
  };
  charged([NOT_MOVED, &words].concat(), admission)
}

/// `answer` charged to `admission`, refused past it.
fn charged(answer: Vec<u8>, admission: &mut Admission) -> Result<Vec<u8>, PastAdmission> {
  admission.charge(to_u64(answer.len()))?;
  Ok(answer)
}

/// `words` ended by a line end, unless they end with one or are empty.
fn ended(words: &[u8]) -> Vec<u8> {
  let mut ended = words.to_vec();
  if !ended.is_empty() && ended.last() != Some(&LINE_END) {
    ended.push(LINE_END);
  }
  ended
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
