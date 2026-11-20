//! receive-pack, the server's half of a push (A-620; §4.17.2 "Pieces (git)", piece 6, and "A
//! push"). Part 1 is the exchange git's `receive-pack` speaks, from its ref advertisement to its
//! report, every pushed object checked as git's strict fsck checks it, the refs a deployment hides
//! kept out of sight and out of reach, each command's verdict and then its move asked of the caller
//! ([`Judge`]), and the whole run over one connection as a sans-io [`Session`]. Part 2 implements
//! [`Judge`]: each ref's update a work the engine judges (A-618's works from a push, A-635's
//! verdict over several), appended to that ref's own log (A-642: each green keeps its own log, and
//! a repository has no position every change passes through).
//!
//! **Sources** (read 2026-11-19, git v2.50.1). The document \[B\]: `gitprotocol-pack.adoc`,
//! "Pushing Data To a Server", "Reference Update Request and Packfile Transfer" and "Report
//! Status" (lines 494-720), and `gitprotocol-capabilities.adoc`'s `atomic`, `push-options`,
//! `report-status` and `report-status-v2`. The code \[C\], which every stock push meets:
//! `builtin/receive-pack.c`, its advertisement (`show_ref`, `write_head_info`), its reading of the
//! request (`read_head_info`, `queue_command`, `read_push_options`) and of the pack (`unpack`,
//! `parse_pack_header`, `unpack_limit`), each command's outcome (`execute_commands`,
//! `set_connectivity_errors`, `reject_updates_to_hidden`, `update`, `execute_commands_atomic`,
//! `report_message`), and its answer (`report`, `report_v2`); `connected.c` (the check's
//! `--exclude-hidden=receive`); and `send-pack.c`, what a stock client writes.
//!
//! **The exchange.** [`write_advertisement`] writes the refs with the capabilities after the first
//! one's NUL, in git's order. [`RequestReader`] reads the client's update request as its bytes
//! arrive: the shallow lines, the commands (the first carrying the client's capabilities), and the
//! push options when both sides chose them, then stops where the pack begins. The pack goes
//! through A-608's reader, a thin pack's bases taken from the repository ([`Repository`]), then
//! every object through A-590's checks under a push's strictness ([`fsck::first_refusal`]).
//! [`connectivity`] holds every new tip to the objects the pack brought and the repository holds.
//! [`check`] and [`decide`] are the push's two halves: read and checked, then its verdicts and
//! appends, the caller free between them to admit the pushed objects to its store. [`receive`] does
//! both for a push held whole; [`Session`] does them over one connection as the bytes arrive,
//! waiting checked between them; [`write_report`] answers in report-status or report-status-v2,
//! multiplexed on side-band-64k when the client asked, the lines git sends on band 2 first.
//!
//! **Each command's outcome**, in git's order and words. A pack the reader or the checks refused
//! leaves every command `unpacker error`, the status git's own words for it: the header's, or
//! `unpack-objects abnormal exit` below git's `unpack_limit` and `index-pack abnormal exit` at or
//! above it. Then the connectivity check: `missing necessary objects`, or `shallow update not
//! allowed` where a commit the client called shallow lacks a parent nothing holds. Then a hidden
//! ref ([`HiddenRefs`]): `deny updating a hidden ref`, `deny deleting a hidden ref`. Then a ref
//! name git would refuse (`funny refname`, with git's band-2 line), then the judge's verdict, which
//! moves nothing, then the append that moves the ref. Without `atomic` each command is judged and
//! appended on its own, in order, so one ref's refusal holds no other (git's
//! `execute_commands_non_atomic`; A-642). Under `atomic`, either every command applies or none
//! does: the first one refused leaves every other `atomic push failure`, as git words it, and once
//! every verdict has accepted, one append moves every ref or none ([`ATOMIC_TRANSACTION_FAILED`]
//! when one moved since its verdict read it).
//!
//! **A protected branch's refusals** ([`protected`], A-644) are answered in git's own words:
//! `non-fast-forward` and `deletion prohibited` as git refuses them itself, and `hook declined`
//! after the words of what git leaves to a hook (a linear history, signed commits), relayed as git
//! relays a hook's.
//!
//! **Stricter than git, by the documents' own words.** `atomic` is "Either all refs are updated or
//! none" (gitprotocol-capabilities); git 2.50.1 still updates the other refs when one fails a check
//! before the atomic transaction begins, and this refuses them all (main approved, 2026-11-19). The
//! capabilities ride on the first command alone, and shallow lines come before the commands, as
//! the grammar writes them; git reads either anywhere. A push certificate is refused, since none is
//! advertised. Every pushed object is checked, where git checks only when `receive.fsckObjects` or
//! `transfer.fsckObjects` asks (§4.17.2 "A push", item 4: always).
//!
//! **Bounds.** The input bounds what is read and the caller's [`Admission`] what is built, each
//! size charged before it is allocated: every command, option and shallow line as it is read, the
//! pack as A-608 charges it, and the connectivity check's tables once their sizes are counted. A
//! packet not yet whole is held up to git's 65520 bytes.

mod advertisement;
mod checking;
mod connect;
pub mod fsck;
pub mod protected;
mod report;
mod request;
mod session;
pub mod works;

pub use advertisement::{Advertised, write_advertisement};
pub use checking::Checking;
pub use connect::{Connected, connectivity};
pub use report::{message, write_hook_words, write_report, write_report_end, write_report_held};
pub use request::{Command, Request, RequestReader, Requested};
pub use session::Session;

use crate::object_name::{Kind, ObjectId};
use crate::objects::FsckId;
use crate::pack;
use crate::pktline;
use std::borrow::Cow;

use crate::unpack::{self, Contents, Held, Keep, Kept, NoIndex, Object, Outside, Placed};
use crate::{Admission, PastAdmission, to_u64};

/// What the repository a push is made to holds (A-614's object store implements it, A-638).
pub trait Repository {
  /// The object named `id`, its type and content, if the repository holds it.
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])>;

  /// Whether the repository holds the object named `id`: everything it holds is connected, since
  /// nothing enters it before its push passed this check, so a walk of a push's objects stops at
  /// any object it holds. An object only hidden refs reach counts too. git's check runs
  /// `--exclude-hidden=receive` (git 2.39, `connected.c`) so that hidden refs do not bound its
  /// walk, for its cost alone: the walk goes on through such an object and finds it held.
  fn holds(&self, id: &ObjectId) -> bool {
    self.object(id).is_some()
  }
}

/// The refs a deployment hides from a pusher, as git's `transfer.hideRefs` and `receive.hideRefs`
/// hide them (A-597's `vcs_policy` answers it in the server): left out of the advertisement, and
/// refused when a command names one.
pub trait HiddenRefs {
  /// Whether the ref named `name` is hidden.
  fn is_hidden(&self, name: &[u8]) -> bool;
}

/// No ref hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoneHidden;

impl HiddenRefs for NoneHidden {
  fn is_hidden(&self, _name: &[u8]) -> bool {
    false
  }
}

/// What a borrowed predicate hides: a session owns its hidden refs ([`Session`]), and a caller that
/// keeps the predicate itself hands the session a reference.
impl<T: HiddenRefs + ?Sized> HiddenRefs for &T {
  fn is_hidden(&self, name: &[u8]) -> bool {
    (**self).is_hidden(name)
  }
}

/// Each command's verdict, asked once the protocol's own checks have passed it, then its move:
/// part 2's engine (A-618's work from the push judged, A-642's append); a test's git policy. A
/// verdict moves nothing, since an atomic push may still refuse every command after it. Only an
/// append moves a ref: one entry on that ref's own log, appended only while the ref is still at
/// the position its verdict read. No append waits on another ref's (A-642: a repository has no
/// position every change passes through).
pub trait Judge {
  /// What an accepted command carries from its verdict to its append: part 2's work, and the
  /// position its ref was read at.
  type Accepted;

  /// Whether `command` may apply, moving nothing: what its append takes, or why not.
  fn judge(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Self::Accepted, Declined>;

  /// A turn of what `command` needs before its verdict (A-722 part 4c: a move's admission walks every object it brings,
  /// which at git/git's size outlasts a step): whether it is ready, so this step judges it; not ready, the step ends
  /// with the command undecided and the next step asks again. A judge that needs nothing first is ready at once.
  fn ready(&mut self, _command: &Command, _pushed: &Pushed<'_>) -> bool {
    true
  }

  /// Appends one accepted command's entry to its ref's own log, on its own (a push without
  /// `atomic`): what report-status-v2 reports of it (nothing for a plain update), or why not
  /// ([`FAILED_TO_UPDATE`] when the ref moved since its verdict read it).
  fn append(
    &mut self,
    command: &Command,
    accepted: Self::Accepted,
  ) -> Result<Vec<Reported>, Declined>;

  /// Appends an atomic push's commands, every one accepted, one entry on each ref, each only while
  /// its ref is still at the position its verdict read: all or none. What report-status-v2 reports
  /// of each, in the order given (nothing for one given no reports), or why none applied
  /// ([`ATOMIC_TRANSACTION_FAILED`] when a ref moved).
  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, Self::Accepted)>,
  ) -> Result<Vec<Vec<Reported>>, Declined>;
}

/// A command the judge refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declined {
  /// Why, in git's words: the report's `ng <ref> <reason>`.
  pub reason: Vec<u8>,
  /// What a hook wrote of it, relayed on band 2 before `message` as git relays a hook's words: in pieces of at most
  /// [`protected::HOOK_PIECE`] bytes, as `copy_to_sideband` reads them (A-644). Empty when no hook spoke.
  pub hook: Vec<u8>,
  /// The line git's `rp_error` sends on band 2 for it, made by [`message`], if any.
  pub message: Option<Vec<u8>>,
}

/// What report-status-v2 reports of a command a hook rewrote (part 2's review requests): the ref
/// it wrote and the ids it wrote it from and to, each line only when given.
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

/// Why a push's pack was refused, with the objects its header counted when its twelve bytes came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackRefused {
  /// The objects the pack's header counts, if its header was whole: what decides git's words.
  pub objects: Option<u32>,
  /// Why.
  pub why: PackRefusal,
}

/// Why a push's pack was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackRefusal {
  /// A-608's reader refused it.
  Unpack(unpack::Refusal),
  /// An object failed A-590's checks under a push's strictness: the first in pack order.
  Fsck {
    /// The object's name.
    id: ObjectId,
    /// The finding that refused it.
    finding: FsckId,
  },
}

/// A push received: its request, its pack, and each command's outcome in the order the client
/// sent them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
  /// The request.
  pub request: Request,
  /// The pack's objects' places, or why it was refused; `None` when no pack was due (only deletes,
  /// or no command).
  pub unpacked: Option<Result<Kept, PackRefused>>,
  /// Each command's outcome.
  pub outcomes: Vec<Outcome>,
}

/// The objects a push's pack brought, by name: each one's place, and its bytes where the caller
/// lends them (A-719 part 3: a server's judge reads them from its own store instead).
pub struct Pushed<'a> {
  /// The objects' places, in pack order.
  objects: &'a [Placed],
  /// Each object's name and position, sorted by name.
  by_id: Vec<(ObjectId, usize)>,
  /// Where their bytes are read, where the caller lends them.
  contents: Option<&'a dyn Contents>,
}

impl<'a> Pushed<'a> {
  /// The objects a pack brought, their bytes read from `contents` where it is lent, indexed by name
  /// in a table charged to `admission` before it is made.
  pub fn new(
    objects: &'a [Placed],
    contents: Option<&'a dyn Contents>,
    admission: &mut Admission,
  ) -> Result<Self, Refusal> {
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
    Ok(Self {
      objects,
      by_id,
      contents,
    })
  }

  /// The pushed object named `id`, its type and bytes, if the pack brought it and its bytes are
  /// lent.
  pub fn object(&self, id: &ObjectId) -> Option<(Kind, Cow<'a, [u8]>)> {
    let at = self.position(id)?;
    let placed = self.objects.get(at)?;
    Some((placed.kind, self.contents?.content(at, id)?))
  }

  /// The bytes of the object at position `at`, if they are lent.
  pub fn content(&self, at: usize) -> Option<Cow<'a, [u8]>> {
    let placed = self.objects.get(at)?;
    self.contents?.content(at, &placed.id)
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

  /// The objects' places, in pack order.
  pub fn objects(&self) -> &'a [Placed] {
    self.objects
  }
}

/// A pack read into memory over `repository`: the keeper a session keeps of its own, and
/// [`receive`]'s.
pub(crate) struct InMemory<'a, R: ?Sized> {
  /// The repository.
  pub(crate) repository: &'a R,
  /// The objects kept.
  pub(crate) held: &'a mut Held,
}

impl<R: Repository + ?Sized> Repository for InMemory<'_, R> {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    self.repository.object(id)
  }

  fn holds(&self, id: &ObjectId) -> bool {
    self.repository.holds(id)
  }
}

impl<R: ?Sized> Contents for InMemory<'_, R> {
  fn content(&self, at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    self.held.content(at, id)
  }
}

impl<R: ?Sized> Keep for InMemory<'_, R> {
  fn keep(
    &mut self,
    at: usize,
    object: Object,
    hold: crate::Hold,
    admission: &mut Admission,
  ) -> Result<(), unpack::Refusal> {
    self.held.keep(at, object, hold, admission)
  }

  fn release(&mut self, hold: crate::Hold, admission: &mut Admission) {
    self.held.release(hold, admission);
  }
}

/// A push's site as the pack reader finishes a thin pack: its keeper, and its repository as the
/// outside bases.
struct Thin<'s, S: ?Sized>(&'s mut S);

impl<S: Repository + ?Sized> Outside for Thin<'_, S> {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    Repository::object(&*self.0, id)
  }
}

impl<S: Contents + ?Sized> Contents for Thin<'_, S> {
  fn content(&self, at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    self.0.content(at, id)
  }
}

impl<S: Keep + ?Sized> Keep for Thin<'_, S> {
  fn keep(
    &mut self,
    at: usize,
    object: Object,
    hold: crate::Hold,
    admission: &mut Admission,
  ) -> Result<(), unpack::Refusal> {
    self.0.keep(at, object, hold, admission)
  }

  fn release(&mut self, hold: crate::Hold, admission: &mut Admission) {
    self.0.release(hold, admission);
  }
}

/// Why a push's exchange cannot go on: its request is not one git's protocol allows, or what it
/// asks for is past the admission. A pack's own refusals are not among them; git answers those in
/// its report (`unpack <reason>`).
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
  /// A command that is not `old-id SP new-id SP refname` in the repository's format (git's
  /// "protocol error: expected old/new/ref").
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
  /// The client's `object-format`, or SHA-1 when it names none, is not the repository's (git's
  /// "unsupported object format").
  ObjectFormat {
    /// Where the first command's packet begins.
    at: u64,
  },
  /// A push certificate, which this server never advertises.
  PushCert {
    /// Where its packet begins.
    at: u64,
  },
  /// A part of the advertisement A-604's writer refuses: a capability or a ref name a reader would
  /// refuse, an id of another format, or a line past a pkt-line's room.
  Advertisement(crate::v0::WriteRefusal),
  /// A line of the report past a pkt-line's room (git's "impossibly long line").
  LineTooLong {
    /// The line's length.
    len: u64,
  },
  /// What the request, or the connectivity check's tables, would take is more than the admission
  /// had left.
  PastAdmission(PastAdmission),
  /// The system refused memory the admission held.
  AllocationRefused {
    /// The bytes asked for.
    bytes: u64,
  },
}

/// Format: the reason every command of a push whose pack was refused is given
/// (`execute_commands`).
pub const UNPACKER_ERROR: &[u8] = b"unpacker error";

/// Format: the reason a command whose new tip reaches an object nothing holds is given
/// (`set_connectivity_errors`).
pub const MISSING_OBJECTS: &[u8] = b"missing necessary objects";

/// Format: the reason a command that would make the repository shallow is given
/// (`update_shallow_info`).
pub const SHALLOW_UPDATE: &[u8] = b"shallow update not allowed";

/// Format: the reason an update or a create of a hidden ref is given (`reject_updates_to_hidden`).
pub const HIDDEN_UPDATE: &[u8] = b"deny updating a hidden ref";

/// Format: the reason a delete of a hidden ref is given (`reject_updates_to_hidden`).
pub const HIDDEN_DELETE: &[u8] = b"deny deleting a hidden ref";

/// Format: the reason a ref name git refuses is given (`update`).
pub const FUNNY_REFNAME: &[u8] = b"funny refname";

/// Format: the reason every other command of a refused atomic push is given
/// (`execute_commands_atomic`).
pub const ATOMIC_FAILURE: &[u8] = b"atomic push failure";

/// Format: the reason a command whose ref could not be moved is given, its ref having moved since
/// it was read among them (`execute_commands_non_atomic`, `update`).
pub const FAILED_TO_UPDATE: &[u8] = b"failed to update ref";

/// Format: the reason every command of an atomic push whose refs could not all be moved is given
/// (`execute_commands_atomic`, the transaction's commit refused).
pub const ATOMIC_TRANSACTION_FAILED: &[u8] = b"atomic transaction failed";

/// Format: git's `unpack_limit` (`receive-pack.c`, 72; `transfer.unpackLimit` "defaults to 100"):
/// a pack of fewer objects is unpacked by `unpack-objects`, any other indexed by `index-pack`, and
/// its refusal is worded after the one git ran.
pub const UNPACK_LIMIT: u32 = 100;

/// The one index the pack reader is given: a push's pack has none, so a reference delta's base is
/// found by name.
static NO_INDEX: NoIndex = NoIndex;

/// Receives a push held whole: reads its request from `bytes`, reads the pack that follows when one
/// is due (its bases outside it from `repository`), checks it ([`check`]), then decides it with
/// `judge` ([`decide`]). Refused only for a request the protocol does not allow or past the
/// admission; a pack refused is answered in the report, as git answers it.
pub fn receive<R, H, J>(
  bytes: &[u8],
  advertised: &Advertised<'_>,
  repository: &R,
  hidden: &H,
  judge: &mut J,
  admission: &mut Admission,
) -> Result<Received, Refusal>
where
  R: Repository + ?Sized,
  H: HiddenRefs + ?Sized,
  J: Judge + ?Sized,
{
  let mut reader = RequestReader::new(advertised);
  let took = reader.read(bytes, admission)?;
  let request = reader.request()?;
  let mut held = Held::default();
  let mut site = InMemory {
    repository,
    held: &mut held,
  };
  let unpacked = request.wants_pack().then(|| {
    let mut pack = unpack::Reader::new(advertised.format, &NO_INDEX);
    let read = pack.read_into(bytes.get(took..).unwrap_or_default(), admission, &mut site);
    finished_pack(pack, read.map(|_| ()), &mut site, admission)
  });
  let checked = check(request, unpacked, (repository, &held), hidden, admission)?;
  decide(checked, Some(&held), judge, admission)
}

/// The pack's outcome once its bytes have ended or been refused (`read`): finished into `site`, its
/// keeper, with a thin pack's bases from the repository it is; or refused, with the objects its
/// header counted. Its objects are checked by [`check`] (or [`Checking`] a turn at a time).
fn finished_pack<S: Keep + Repository>(
  reader: unpack::Reader<'_, NoIndex>,
  read: Result<(), unpack::Refusal>,
  site: &mut S,
  admission: &mut Admission,
) -> Result<Kept, PackRefused> {
  let objects = reader.objects();
  let refused = |why| PackRefused { objects, why };
  read.map_err(|refusal| refused(PackRefusal::Unpack(refusal)))?;
  reader
    .finish_into(admission, &mut Thin(site))
    .map_err(|refusal| refused(PackRefusal::Unpack(refusal)))
}

/// A push read and checked, before any verdict: its request, its pack's objects or why the pack was
/// refused, and each command's refusal by the protocol's own checks. Its objects are where the
/// pack was read into (A-719 part 3: a server's quarantine), so the push holds none of their
/// bytes; the caller hands it to [`decide`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
  /// The request.
  pub request: Request,
  /// The pack's objects' places, or why it was refused; `None` when no pack was due.
  pub unpacked: Option<Result<Kept, PackRefused>>,
  /// Each command's refusal by the checks, in the order sent; `None` for one they passed.
  pub early: Vec<Option<Outcome>>,
  /// The pushed objects' names and positions, sorted by name, charged once by [`check`].
  by_id: Vec<(ObjectId, usize)>,
}

impl Checked {
  /// The places of the objects the push brought, in pack order: none when no pack was due or it
  /// was refused.
  pub fn objects(&self) -> &[Placed] {
    match &self.unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    }
  }
}

/// Checks a read push: every command `unpacker error` after a refused pack, or after an object
/// git's checks refuse (git's `transfer.fsckObjects`); otherwise each new tip's connectivity against
/// `repository`, then each ref against `hidden`. The tables made on the way are charged to
/// `admission` before they are made. The checks run whole: [`Checking`] makes them a turn at a time.
pub fn check<R, C, H>(
  request: Request,
  unpacked: Option<Result<Kept, PackRefused>>,
  (repository, contents): (&R, &C),
  hidden: &H,
  admission: &mut Admission,
) -> Result<Checked, Refusal>
where
  R: Repository + ?Sized,
  C: Contents,
  H: HiddenRefs + ?Sized,
{
  Checking::new(request, unpacked).checked((repository, contents), hidden, admission)
}

/// Decides a checked push: each command no check refused has `judge`'s verdict and then its append,
/// each on its own without `atomic`, all or none under it; every outcome in the order sent. The
/// outcomes are charged to `admission` before they are made. The decision runs whole: [`Deciding`]
/// makes it a command at a time.
pub fn decide<J: Judge + ?Sized>(
  checked: Checked,
  contents: Option<&dyn Contents>,
  judge: &mut J,
  admission: &mut Admission,
) -> Result<Received, Refusal> {
  let mut deciding = Deciding::new(checked, admission)?;
  while !deciding.step(contents, judge, admission)? {}
  Ok(deciding.received())
}

/// A checked push decided a command at a time (A-719; §4.3, no step longer than its quantum: git's
/// push of its own repository brings over a thousand refs): without `atomic`, each step judges one
/// command and, accepted, appends it; under `atomic`, each step judges one, and a last step appends
/// them all or none. The outcomes come out as [`decide`]'s.
pub struct Deciding<A> {
  /// The request.
  request: Request,
  /// The pack's objects' places, or why it was refused.
  unpacked: Option<Result<Kept, PackRefused>>,
  /// The pushed objects' names and positions, sorted by name.
  by_id: Vec<(ObjectId, usize)>,
  /// Each command's refusal by the checks, taken as its step comes.
  early: Vec<Option<Outcome>>,
  /// The outcomes so far, in the order sent.
  outcomes: Vec<Outcome>,
  /// The next command.
  at: usize,
  /// Under `atomic`, each command accepted so far with its verdict.
  accepted: Vec<(usize, A)>,
  /// Whether every outcome is made.
  done: bool,
}

impl<A> Deciding<A> {
  /// The decision of `checked`, not yet begun, its outcomes' table charged to `admission` first.
  pub fn new(checked: Checked, admission: &mut Admission) -> Result<Self, Refusal> {
    let Checked {
      request,
      unpacked,
      early,
      by_id,
    } = checked;
    let count = request.commands.len();
    let outcomes = charged_vec(count, admission)?;
    let accepted = if request.requested.atomic {
      charged_vec(count, admission)?
    } else {
      Vec::new()
    };
    Ok(Self {
      request,
      unpacked,
      by_id,
      early,
      outcomes,
      at: 0,
      accepted,
      done: count == 0,
    })
  }

  /// One step of the decision through `judge`, the pushed objects' bytes read from `contents` where
  /// lent: one command judged (and, without `atomic`, appended), or under `atomic` the appends of
  /// them all. Whether every outcome is made.
  pub fn step<J: Judge<Accepted = A> + ?Sized>(
    &mut self,
    contents: Option<&dyn Contents>,
    judge: &mut J,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    if self.done {
      return Ok(true);
    }
    let unpacked = self.unpacked.take();
    let objects = match &unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    let pushed = Pushed {
      objects,
      by_id: core::mem::take(&mut self.by_id),
      contents,
    };
    let stepped = if self.request.requested.atomic {
      atomic_step(self, &pushed, judge, admission)
    } else {
      each_step(self, &pushed, judge);
      Ok(())
    };
    self.by_id = pushed.by_id;
    self.unpacked = unpacked;
    stepped?;
    Ok(self.done)
  }

  /// What was received, once every outcome is made.
  pub fn received(self) -> Received {
    Received {
      request: self.request,
      unpacked: self.unpacked,
      outcomes: self.outcomes,
    }
  }
}

/// One step of a push without `atomic`: the next command no check refused judged and, accepted,
/// appended on its own, so one ref's refusal holds no other (git's `execute_commands_non_atomic`;
/// A-642).
fn each_step<A, J: Judge<Accepted = A> + ?Sized>(
  deciding: &mut Deciding<A>,
  pushed: &Pushed<'_>,
  judge: &mut J,
) {
  let at = deciding.at;
  // A command no check refused: one with no early outcome, its slot empty or past the checks' table.
  let unrefused = deciding.early.get(at).is_none_or(Option::is_none);
  if unrefused
    && let Some(command) = deciding.request.commands.get(at)
    && !judge.ready(command, pushed)
  {
    return;
  }
  let early = deciding.early.get_mut(at).and_then(Option::take);
  let outcome = match (early, deciding.request.commands.get(at)) {
    (Some(refusal), _) => refusal,
    (None, Some(command)) => match verdict(command, pushed, judge) {
      Ok(accepted) => judge
        .append(command, accepted)
        .map_or_else(Outcome::Ng, Outcome::Ok),
      Err(declined) => Outcome::Ng(declined),
    },
    (None, None) => return deciding.done = true,
  };
  deciding.outcomes.push(outcome);
  deciding.at = at.saturating_add(1);
  deciding.done = deciding.at >= deciding.request.commands.len();
}

/// One step of an atomic push: every command or none ("Either all refs are updated or none",
/// gitprotocol-capabilities). A check's refusal leaves every other command `atomic push failure`,
/// none judged; otherwise one command is judged a step until one is declined, which leaves every
/// other `atomic push failure`; then one append moves every ref or none, and a refused append gives
/// every command its reason, its band-2 line once (git's `execute_commands_atomic`).
fn atomic_step<A, J: Judge<Accepted = A> + ?Sized>(
  deciding: &mut Deciding<A>,
  pushed: &Pushed<'_>,
  judge: &mut J,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  let count = deciding.request.commands.len();
  deciding.done = true;
  if deciding.early.iter().any(Option::is_some) {
    let others =
      |outcome: Option<Outcome>| outcome.unwrap_or_else(|| refused(ATOMIC_FAILURE, None));
    let early = core::mem::take(&mut deciding.early);
    deciding.outcomes.extend(early.into_iter().map(others));
    return Ok(());
  }
  if let Some(command) = deciding.request.commands.get(deciding.at) {
    let at = deciding.at;
    if !judge.ready(command, pushed) {
      deciding.done = false;
      return Ok(());
    }
    match verdict(command, pushed, judge) {
      Ok(verdict) => {
        deciding.accepted.push((at, verdict));
        deciding.at = at.saturating_add(1);
        deciding.done = false;
      }
      Err(declined) => deciding.outcomes.extend((0..count).map(|other| {
        if other == at {
          Outcome::Ng(declined.clone())
        } else {
          refused(ATOMIC_FAILURE, None)
        }
      })),
    }
    return Ok(());
  }
  let accepted = core::mem::take(&mut deciding.accepted);
  let mut appended = charged_vec(accepted.len(), admission)?;
  let commands = &deciding.request.commands;
  appended.extend(
    accepted
      .into_iter()
      .filter_map(|(at, verdict)| commands.get(at).map(|command| (command, verdict))),
  );
  match judge.append_atomic(appended) {
    Ok(reports) => {
      let mut reports = reports.into_iter();
      deciding
        .outcomes
        .extend((0..count).map(|_| Outcome::Ok(reports.next().unwrap_or_default())));
    }
    Err(declined) => deciding.outcomes.extend((0..count).map(|at| {
      let message = declined.message.clone().filter(|_| at == 0);
      refused(&declined.reason, message)
    })),
  }
  Ok(())
}

/// A command's refusal before any judging, in git's order: its connectivity, then a hidden ref.
fn early_refusal<H: HiddenRefs + ?Sized>(
  command: &Command,
  connected: Connected,
  hidden: &H,
) -> Option<Outcome> {
  match connected {
    Connected::Missing => Some(refused(MISSING_OBJECTS, None)),
    Connected::Shallow => Some(refused(SHALLOW_UPDATE, None)),
    Connected::Complete if hidden.is_hidden(&command.name) => {
      let reason = if command.is_delete() {
        HIDDEN_DELETE
      } else {
        HIDDEN_UPDATE
      };
      Some(refused(reason, None))
    }
    Connected::Complete => None,
  }
}

/// A command's verdict: its ref name as git checks it (`update`'s `funny refname`, with git's
/// band-2 line), then the judge's.
fn verdict<J: Judge + ?Sized>(
  command: &Command,
  pushed: &Pushed<'_>,
  judge: &mut J,
) -> Result<J::Accepted, Declined> {
  if request::refname_ok(&command.name, command.is_delete()) {
    return judge.judge(command, pushed);
  }
  let line = [
    b"refusing to update funny ref '",
    command.name.as_slice(),
    b"' remotely",
  ]
  .concat();
  Err(Declined {
    reason: FUNNY_REFNAME.to_vec(),
    hook: Vec::new(),
    message: Some(message(&line)),
  })
}

/// A refusal with `reason`, and the band-2 line given.
fn refused(reason: &[u8], message: Option<Vec<u8>>) -> Outcome {
  Outcome::Ng(Declined {
    reason: reason.to_vec(),
    hook: Vec::new(),
    message,
  })
}

/// An empty vector with room for exactly `count` values, charged to `admission` before it is made.
fn charged_vec<T>(count: usize, admission: &mut Admission) -> Result<Vec<T>, Refusal> {
  admission
    .charge_items::<T>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut vec = Vec::new();
  reserve(&mut vec, count)?;
  Ok(vec)
}

/// The refs of `refs` a pusher may see: those `hidden` does not hide, in their order.
pub fn visible<'r, H: HiddenRefs + ?Sized>(
  refs: &[(ObjectId, &'r [u8])],
  hidden: &H,
) -> Vec<(ObjectId, &'r [u8])> {
  refs
    .iter()
    .filter(|(_, name)| !hidden.is_hidden(name))
    .copied()
    .collect()
}

/// Reserves room for exactly `count` more values in `vec`, or refuses by kind.
fn reserve<T>(vec: &mut Vec<T>, count: usize) -> Result<(), Refusal> {
  vec
    .try_reserve_exact(count)
    .map_err(|_| Refusal::AllocationRefused {
      bytes: to_u64(core::mem::size_of::<T>()).saturating_mul(to_u64(count)),
    })
}

/// git's words for why a pack was refused, as its report's `unpack` line gives them: the header's
/// own refusals (`parse_pack_header`), then the program git ran for a pack of that many objects
/// (`unpack`: `unpack-objects` below [`UNPACK_LIMIT`], `index-pack` at or above it).
pub fn unpack_words(refused: &PackRefused) -> &'static [u8] {
  match (refused.why, refused.objects) {
    (PackRefusal::Unpack(unpack::Refusal::Pack(pack::Refusal::Signature { .. })), _) => {
      b"protocol error (pack signature mismatch detected)"
    }
    (PackRefusal::Unpack(unpack::Refusal::Pack(pack::Refusal::Version { .. })), _) => {
      b"protocol error (pack version unsupported)"
    }
    (_, None) => b"eof before pack header was fully read",
    (_, Some(objects)) if objects < UNPACK_LIMIT => b"unpack-objects abnormal exit",
    (_, Some(_)) => b"index-pack abnormal exit",
  }
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
