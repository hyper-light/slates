//! receive-pack, the server's half of a push (A-620; §4.17.2 "Pieces (git)", piece 6, and "A push"). Part 1 is the
//! exchange git's `receive-pack` speaks, from its ref advertisement to its report, each ref's verdict asked of the
//! caller ([`Judge`]); part 2 makes each ref's update a work the engine judges, every ref recorded as one register entry
//! (A-618's works from a push, A-603's register).
//!
//! **Sources** (read 2026-11-19, git v2.50.1). The document [B]: `gitprotocol-pack.adoc`, "Pushing Data To a Server",
//! "Reference Update Request and Packfile Transfer" and "Report Status" (lines 494-720), and
//! `gitprotocol-capabilities.adoc`'s `atomic`, `push-options`, `report-status` and `report-status-v2`. The code [C],
//! which every stock push meets: `builtin/receive-pack.c`, its advertisement (`show_ref`, `write_head_info`), its
//! reading of the request (`read_head_info`, `queue_command`, `read_push_options`) and of the pack's header
//! (`parse_pack_header`, `unpack`), each command's outcome (`execute_commands`, `set_connectivity_errors`, `update`,
//! `execute_commands_atomic`, `report_message`), and its answer (`report`, `report_v2`); and `send-pack.c`, what a
//! stock client writes.
//!
//! **The exchange.** [`write_advertisement`] writes the refs with the capabilities after the first one's NUL, in git's
//! order. [`RequestReader`] reads the client's update request as its bytes arrive: the shallow lines, the commands (the
//! first carrying the client's capabilities), and the push options when both sides chose them, then stops where the
//! pack begins. The pack goes through A-608's reader, a thin pack's bases taken from the repository ([`Repository`]).
//! [`connectivity`] holds every new tip to the objects the pack brought and the repository holds. [`receive`] does all
//! of it for a push held whole and asks the [`Judge`] about each command that passed; [`write_report`] answers in
//! report-status or report-status-v2, multiplexed on side-band-64k when the client asked, git's `rp_error` lines on
//! band 2 first.
//!
//! **Each command's outcome**, in git's order and words. A pack the reader refused leaves every command
//! `unpacker error`, its own status git's words for it (`index-pack abnormal exit`, or the header's own). Then the
//! connectivity check: `missing necessary objects`, or `shallow update not allowed` where a commit the client called
//! shallow lacks a parent nothing holds. Then a ref name git would refuse (`funny refname`, with git's band-2 line),
//! then the judge. Under `atomic`, either every command applies or none does: the first one refused leaves every other
//! `atomic push failure`, as git words it.
//!
//! **Stricter than git, by the documents' own words.** `atomic` is "Either all refs are updated or none"
//! (gitprotocol-capabilities); git 2.50.1 still updates the other refs when one fails its connectivity check before
//! the atomic transaction begins, and this refuses them all. The capabilities ride on the first command alone, and
//! shallow lines come before the commands, as the grammar writes them; git reads either anywhere. A push certificate is
//! refused, since none is advertised.
//!
//! **Bounds.** The input bounds what is read and the caller's [`Admission`] what is built, each size charged before
//! it is allocated: every command, option and shallow line as it is read, the pack as A-608 charges it, and the
//! connectivity check's tables once their sizes are counted. A packet not yet whole is held up to git's 65520 bytes.

mod advertisement;
mod connect;
mod report;
mod request;

pub use advertisement::{Advertised, write_advertisement};
pub use connect::{Connected, connectivity};
pub use report::{message, write_report};
pub use request::{Command, Request, RequestReader, Requested};

use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::pack;
use crate::pktline;
use crate::unpack::{self, NoIndex, Object, Outside, Unpacked};
use crate::{Admission, PastAdmission, to_u64};

/// What the repository a push is made to holds (A-614's object store implements it).
pub trait Repository {
  /// The object named `id`, its type and content, if the repository holds it.
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])>;

  /// Whether the repository holds the object named `id`. Everything it holds is connected, since nothing enters it
  /// before its push passed this check, so a walk of a push's objects stops at any object it holds.
  fn holds(&self, id: &ObjectId) -> bool {
    self.object(id).is_some()
  }
}

/// A repository's objects, as the pack reader takes a thin pack's bases from.
struct Bases<'r, R: ?Sized>(&'r R);

impl<R: Repository + ?Sized> Outside for Bases<'_, R> {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    self.0.object(id)
  }
}

/// Each command's verdict, asked once the protocol's own checks have passed it: part 2's engine; a test's git policy.
pub trait Judge {
  /// Whether `command` applies: what report-status-v2 reports of it (nothing for a plain update), or why not.
  fn judge(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Vec<Reported>, Declined>;
}

/// A command the judge refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declined {
  /// Why, in git's words: the report's `ng <ref> <reason>`.
  pub reason: Vec<u8>,
  /// The line git's `rp_error` sends on band 2 for it, made by [`message`], if any.
  pub message: Option<Vec<u8>>,
}

/// What report-status-v2 reports of a command a hook rewrote (part 2's review requests): the ref it wrote and the ids
/// it wrote it from and to, each line only when given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reported {
  /// `option refname`: the ref written, when another than the command's.
  pub refname: Option<Vec<u8>>,
  /// `option old-oid`.
  pub old: Option<ObjectId>,
  /// `option new-oid`.
  pub new: Option<ObjectId>,
  /// `option forced-update`.
  pub forced: bool,
}

/// One command's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// Applied, with what report-status-v2 reports of it.
  Ok(Vec<Reported>),
  /// Refused.
  Ng(Declined),
}

/// A push received: its request, its pack, and each command's outcome in the order the client sent them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
  /// The request.
  pub request: Request,
  /// The pack's objects, or why the reader refused it; `None` when no pack was due (only deletes, or no command).
  pub unpacked: Option<Result<Unpacked, unpack::Refusal>>,
  /// Each command's outcome.
  pub outcomes: Vec<Outcome>,
}

/// The objects a push's pack brought, by name.
pub struct Pushed<'a> {
  /// The objects, in pack order.
  objects: &'a [Object],
  /// Each object's name and position, sorted by name.
  by_id: Vec<(ObjectId, usize)>,
}

impl<'a> Pushed<'a> {
  /// The objects a pack brought, indexed by name in a table charged to `admission` before it is made.
  pub fn new(objects: &'a [Object], admission: &mut Admission) -> Result<Self, Refusal> {
    admission
      .charge_items::<(ObjectId, usize)>(objects.len())
      .map_err(Refusal::PastAdmission)?;
    let mut by_id = Vec::new();
    reserve(&mut by_id, objects.len())?;
    by_id.extend(
      objects
        .iter()
        .enumerate()
        .map(|(at, object)| (object.id, at)),
    );
    by_id.sort_unstable();
    Ok(Self { objects, by_id })
  }

  /// The pushed object named `id`, if the pack brought it.
  pub fn object(&self, id: &ObjectId) -> Option<&'a Object> {
    self.position(id).and_then(|at| self.objects.get(at))
  }

  /// The position in pack order of the object named `id`, if the pack brought it.
  pub fn position(&self, id: &ObjectId) -> Option<usize> {
    self
      .by_id
      .binary_search_by(|(known, _)| known.cmp(id))
      .ok()
      .and_then(|at| self.by_id.get(at))
      .map(|(_, at)| *at)
  }

  /// The objects, in pack order.
  pub fn objects(&self) -> &'a [Object] {
    self.objects
  }
}

/// Why a push's exchange cannot go on: its request is not one git's protocol allows, or what it asks for is past the
/// admission. A pack's own refusals are not among them; git answers those in its report (`unpack <reason>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A packet git's pkt-line reader refuses.
  PktLine(pktline::Refusal),
  /// The bytes ended before the request's last flush.
  Truncated {
    /// The bytes read.
    at: u64,
  },
  /// A delimiter or response-end packet, which a push's request never holds.
  UnexpectedPacket {
    /// Where it begins.
    at: u64,
  },
  /// A command that is not `old-id SP new-id SP refname` in the repository's format (git's "protocol error: expected
  /// old/new/ref").
  Command {
    /// Where its packet begins.
    at: u64,
  },
  /// A shallow line that is not `shallow` and an id, or one after the commands began.
  Shallow {
    /// Where its packet begins.
    at: u64,
  },
  /// Capabilities on a command line but the first.
  Capabilities {
    /// Where its packet begins.
    at: u64,
  },
  /// The client's `object-format`, or SHA-1 when it names none, is not the repository's (git's "unsupported object
  /// format").
  ObjectFormat {
    /// Where the first command's packet begins.
    at: u64,
  },
  /// A push certificate, which this server never advertises.
  PushCert {
    /// Where its packet begins.
    at: u64,
  },
  /// A line of the advertisement or the report past a pkt-line's room (git's "impossibly long line").
  LineTooLong {
    /// The line's length.
    len: u64,
  },
  /// What the request, or the connectivity check's tables, would take is more than the admission had left.
  PastAdmission(PastAdmission),
  /// The system refused memory the admission held.
  AllocationRefused {
    /// The bytes asked for.
    bytes: u64,
  },
}

/// Format: the reason every command of a push whose pack was refused is given (`execute_commands`).
pub const UNPACKER_ERROR: &[u8] = b"unpacker error";

/// Format: the reason a command whose new tip reaches an object nothing holds is given (`set_connectivity_errors`).
pub const MISSING_OBJECTS: &[u8] = b"missing necessary objects";

/// Format: the reason a command that would make the repository shallow is given (`update_shallow_info`).
pub const SHALLOW_UPDATE: &[u8] = b"shallow update not allowed";

/// Format: the reason a ref name git refuses is given (`update`).
pub const FUNNY_REFNAME: &[u8] = b"funny refname";

/// Format: the reason every other command of a refused atomic push is given (`execute_commands_atomic`).
pub const ATOMIC_FAILURE: &[u8] = b"atomic push failure";

/// Receives a push held whole: reads its request from `bytes`, reads the pack that follows when one is due (its bases
/// outside it from `repository`), checks every new tip's connectivity, and asks `judge` about each command the
/// protocol's own checks passed. Refused only for a request the protocol does not allow or past the admission; a pack
/// the reader refuses is answered in the report, as git answers it.
pub fn receive<R: Repository + ?Sized, J: Judge + ?Sized>(
  bytes: &[u8],
  advertised: &Advertised<'_>,
  repository: &R,
  judge: &mut J,
  admission: &mut Admission,
) -> Result<Received, Refusal> {
  let mut reader = RequestReader::new(advertised);
  let took = reader.read(bytes, admission)?;
  let request = reader.request()?;
  let unpacked = request.wants_pack().then(|| {
    let pack = bytes.get(took..).unwrap_or_default();
    read_pack(pack, advertised.format, repository, admission)
  });
  let outcomes = outcomes(&request, unpacked.as_ref(), repository, judge, admission)?;
  Ok(Received {
    request,
    unpacked,
    outcomes,
  })
}

/// The pack that follows a request, read by A-608's reader, a thin pack's bases taken from `repository`.
fn read_pack<R: Repository + ?Sized>(
  pack: &[u8],
  format: ObjectFormat,
  repository: &R,
  admission: &mut Admission,
) -> Result<Unpacked, unpack::Refusal> {
  let mut reader = unpack::Reader::new(format, &NoIndex);
  reader.read(pack, admission)?;
  reader.finish_with(&Bases(repository), admission)
}

/// Each command's outcome: every command `unpacker error` after a refused pack; otherwise the connectivity check's
/// refusals, then each remaining command's ref name and the judge's verdict, all or none under `atomic`.
pub fn outcomes<R: Repository + ?Sized, J: Judge + ?Sized>(
  request: &Request,
  unpacked: Option<&Result<Unpacked, unpack::Refusal>>,
  repository: &R,
  judge: &mut J,
  admission: &mut Admission,
) -> Result<Vec<Outcome>, Refusal> {
  let count = request.commands.len();
  if let Some(Err(_)) = unpacked {
    return Ok(refused_all(count, UNPACKER_ERROR));
  }
  let objects = match unpacked {
    Some(Ok(unpacked)) => unpacked.objects.as_slice(),
    _ => &[],
  };
  let pushed = Pushed::new(objects, admission)?;
  let connected = connectivity(request, &pushed, repository, admission)?;
  let mut outcomes: Vec<Option<Outcome>> = connected
    .iter()
    .map(|connected| match connected {
      Connected::Complete => None,
      Connected::Missing => Some(refused(MISSING_OBJECTS, None)),
      Connected::Shallow => Some(refused(SHALLOW_UPDATE, None)),
    })
    .collect();
  let atomic = request.requested.atomic;
  if !(atomic && outcomes.iter().any(Option::is_some)) {
    judge_each(request, &pushed, judge, atomic, &mut outcomes);
  }
  let failed = outcomes
    .iter()
    .any(|outcome| matches!(outcome, Some(Outcome::Ng(_))));
  let all_or_none = atomic && failed;
  Ok(
    outcomes
      .into_iter()
      .map(|outcome| match outcome {
        Some(Outcome::Ng(declined)) => Outcome::Ng(declined),
        _ if all_or_none => refused(ATOMIC_FAILURE, None),
        Some(applied) => applied,
        None => refused(ATOMIC_FAILURE, None),
      })
      .collect(),
  )
}

/// Asks about each command no check has refused yet, in order: its ref name as git checks it, then the judge. Under
/// `atomic` the first refusal ends the asking, as git's atomic transaction stops at its first failure.
fn judge_each<J: Judge + ?Sized>(
  request: &Request,
  pushed: &Pushed<'_>,
  judge: &mut J,
  atomic: bool,
  outcomes: &mut [Option<Outcome>],
) {
  for (command, outcome) in request.commands.iter().zip(outcomes.iter_mut()) {
    if outcome.is_some() {
      continue;
    }
    let verdict = if request::refname_ok(&command.name, command.is_delete()) {
      judge
        .judge(command, pushed)
        .map_or_else(Outcome::Ng, Outcome::Ok)
    } else {
      let line = [
        b"refusing to update funny ref '",
        command.name.as_slice(),
        b"' remotely",
      ]
      .concat();
      refused(FUNNY_REFNAME, Some(message(&line)))
    };
    let declined = matches!(verdict, Outcome::Ng(_));
    *outcome = Some(verdict);
    if atomic && declined {
      return;
    }
  }
}

/// A refusal with `reason`, and the band-2 line given.
fn refused(reason: &[u8], message: Option<Vec<u8>>) -> Outcome {
  Outcome::Ng(Declined {
    reason: reason.to_vec(),
    message,
  })
}

/// `count` refusals, each with `reason`.
fn refused_all(count: usize, reason: &[u8]) -> Vec<Outcome> {
  (0..count).map(|_| refused(reason, None)).collect()
}

/// Reserves room for exactly `count` more values in `vec`, or refuses by kind.
fn reserve<T>(vec: &mut Vec<T>, count: usize) -> Result<(), Refusal> {
  vec
    .try_reserve_exact(count)
    .map_err(|_| Refusal::AllocationRefused {
      bytes: to_u64(core::mem::size_of::<T>()).saturating_mul(to_u64(count)),
    })
}

/// git's words for why a pack was refused, as its report's `unpack` line gives them: the header's own refusals
/// (`parse_pack_header`), and `index-pack abnormal exit` for every refusal of the pack's body.
pub fn unpack_words(refusal: &unpack::Refusal) -> &'static [u8] {
  match refusal {
    unpack::Refusal::Pack(pack::Refusal::Signature { .. }) => {
      b"protocol error (pack signature mismatch detected)"
    }
    unpack::Refusal::Pack(pack::Refusal::Version { .. }) => {
      b"protocol error (pack version unsupported)"
    }
    unpack::Refusal::Truncated { at } if *at < crate::entry::PACK_HEADER_LEN => {
      b"eof before pack header was fully read"
    }
    _ => b"index-pack abnormal exit",
  }
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
