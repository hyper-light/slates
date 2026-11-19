//! The import verb (A-678; §4.17.3 piece 3; §4.15; A-675): a client asks the daemon to import the Mercurial repository at
//! a host directory into a repository's store, and is answered when the import ends.
//!
//! **The verb.** `ImportRepository { repository, path }` is served on the partition the repository's name hashes to, as
//! every repository verb is, and for the repository's owner alone (`NotFound` for none, `Forbidden` for another
//! principal's; the parent module's "Rights"). A repository of another tool is refused `ImportRefused { NotMercurial }`
//! before anything is opened. The directory is opened as a base is (§4.15: read-only, the one path the host resolves,
//! never through a link), refused as a base is (`BaseUnavailable`, `OwnMount`), and the import is started
//! (`vcs_store::hg_import::begin`); what refuses it there is answered at once.
//!
//! **Answered when it ends.** A started import runs a turn at a time on this shard, in a task of the shard
//! (`hg_import::run`). The request's reply is held meanwhile: the request's origin and id and the route its reply goes
//! by ([`Answer`]) are kept in the shard's map beside the import, one an import, so bounded as the imports are, and the
//! running verb is marked deferred, so its transaction records no completion yet. When the import ends, the task records
//! the request's completion with its reply in one durable step, as every verb's completion is, then sends the reply
//! where the route names: the client's ring on this shard, or its own shard's (`told::deliver_to`). A resend of the
//! request while its import runs (its client reconnected) takes the route over, so the import's end answers the client
//! where it is now; any other request for the repository is refused `UnderWay`.
//!
//! **Refusals, by kind** ([`refusal_of`]). The store holding a history (`NotEmpty`); an import under way (`UnderWay`);
//! the shard's budget unable to take the import's room (`BudgetExceeded`, with what it could admit); the repository's
//! format (`Format`: a requirement the import does not know or take, a censored, ellipsis, externally stored or
//! unknown-flagged revision); a file missing, not of its format or at odds with the rest (`Damaged`, with its path
//! under `.hg` where the refusal names one); more than an import may hold at once (`PastRoom`, with its room); a fetch
//! still pending on an S3 base (`Pending`: ask again once it lands); the host's own refusal (`BaseUnavailable` with the
//! directory's path); and an import given back before its end (`Abandoned`).
//!
//! **Owed** (GAPS' §4.17.3 entry). The import made durable: the register's first entry recording the phase roots and
//! bookmarks it found, and a recovery that imports the directory again or replays its record. Until then a request
//! that reached the owner from another node, whose exchange cannot be held here, is refused `Unawaited`, and a restart
//! leaves the store empty.

use slates_db::catalog::{Principal, RepositoryId, VcsTool};
use slates_hg::import::ImportRefused;
use slates_ipc::protocol::{ImportFault, Refusal, ReplyBody};
use slates_vcs::ContentRefused;
use slates_vfs::host::{HostDir, HostError, HostFs};
use slates_wire::request::RequestId;

use crate::hg_base::BaseRefused;
use crate::merge_service::ReplyRoute;
use crate::state::ShardState;
use crate::vcs_store::hg_import::{self, Finished, ImportFailed};
use crate::verbs::refused;

#[cfg(test)]
mod tests;

/// Counter: a request whose reply is held until its import ends. Format: a counter name in the status report.
pub(crate) const HELD: &str = "repository.import.held";
/// Counter: a resend of a held request that took its route over. Format: a counter name.
pub(crate) const REJOINED: &str = "repository.import.rejoined";
/// Counter: a held request answered at its import's end. Format: a counter name.
pub(crate) const ANSWERED: &str = "repository.import.answered";
/// Counter: an import's answer whose completion could not be made durable, answered with the database's refusal: zero
/// in every correct use. Format: a counter name.
pub(crate) const UNRECORDED: &str = "repository.import.unrecorded";
/// Counter: an import that could not be run, the shard's task arena refusing its task: the import given back, the
/// request answered `Overloaded`. Format: a counter name.
pub(crate) const UNRUN: &str = "repository.import.unrun";

/// A request whose reply is held until its import ends (the module doc).
#[derive(Clone, Debug)]
pub(crate) struct Answer {
  /// The request's origin.
  origin: u64,
  /// The request's id.
  id: RequestId,
  /// Where its reply goes.
  route: ReplyRoute,
  /// The directory the import reads, which a host's refusal names.
  path: String,
}

/// A host's directory opened for an import: its host, and the directory on it.
type Opened = (Box<dyn HostFs>, HostDir);

/// Opens `path` as a base is opened (§4.15): read-only, the one path the host resolves.
fn on_the_host(path: &str) -> Result<Opened, HostError> {
  let (host, directory) = slates_base::OsHost::open_root(std::path::Path::new(path))?;
  Ok((Box::new(host), directory))
}

/// `ImportRepository` (the module doc): the reply now when the import cannot start; otherwise the request's reply is
/// held until the import ends and the running verb marked deferred.
pub(crate) fn import_repository(
  state: &mut ShardState,
  principal: &Principal,
  (name, path): (&str, &str),
) -> ReplyBody {
  import_with(state, principal, (name, path), on_the_host)
}

/// [`import_repository`] with the directory opened by `open`: the host's own, or a test's simulated one.
pub(crate) fn import_with(
  state: &mut ShardState,
  principal: &Principal,
  (name, path): (&str, &str),
  open: impl FnOnce(&str) -> Result<Opened, HostError>,
) -> ReplyBody {
  if name == name {
    drop((state, principal, path, open));
    return refused(Refusal::NotFound);
  }
  let record = match super::owned_repository(state, principal, name, "import_repository") {
    Ok(record) => record,
    Err(reply) => return reply,
  };
  if record.tool != VcsTool::Mercurial {
    return refused(import_refused(ImportFault::NotMercurial));
  }
  let Some((origin, request)) = state.current_request else {
    return refused(import_refused(ImportFault::Unawaited));
  };
  if state.vcs_stores.importing(record.id) {
    return rejoin(state, record.id, (origin, request));
  }
  let Some(route) = state.reply_route.take() else {
    return refused(import_refused(ImportFault::Unawaited));
  };
  let opened = match open(path) {
    Ok(opened) => opened,
    Err(error) => return refused(crate::verbs::base_open_refusal(path, error)),
  };
  if let Err(failed) = hg_import::begin(state, record.id, opened) {
    return refused(refusal_of(path, hg_import::room_of(state), failed));
  }
  let answer = Answer {
    origin,
    id: request,
    route,
    path: path.to_owned(),
  };
  hold(state, record.id, answer)
}

/// A request for a repository whose import is under way: a resend of the held request takes its route over and waits
/// on; any other is refused `UnderWay`.
fn rejoin(
  state: &mut ShardState,
  id: RepositoryId,
  (origin, request): (u64, RequestId),
) -> ReplyBody {
  let Some(held) = state.vcs_stores.answer(id).cloned() else {
    return refused(import_refused(ImportFault::UnderWay));
  };
  if held.origin != origin || held.id != request {
    return refused(import_refused(ImportFault::UnderWay));
  }
  let Some(route) = state.reply_route.take() else {
    return refused(import_refused(ImportFault::Unawaited));
  };
  state.vcs_stores.hold_answer(id, Answer { route, ..held });
  state.acceptance_deferred = true;
  state.count(REJOINED, 1);
  ReplyBody::Acknowledged
}

/// Holds `answer` until repository `id`'s import ends and starts the shard's task that runs it (the module doc). A task
/// the runtime refuses gives the import back, and the request is answered `Overloaded` now.
fn hold(state: &mut ShardState, id: RepositoryId, answer: Answer) -> ReplyBody {
  let task = async move {
    answer_at_end(id).await;
  };
  match slates_rt::futures::spawn(task).and_then(slates_rt::futures::detach) {
    Ok(()) => {
      state.vcs_stores.hold_answer(id, answer);
      state.acceptance_deferred = true;
      state.count(HELD, 1);
      ReplyBody::Acknowledged
    }
    Err(_) => {
      hg_import::abandon(state, id);
      state.count(UNRUN, 1);
      refused(Refusal::Overloaded { shard: state.shard })
    }
  }
}

/// The shard's task for repository `id`'s import: runs it to its end, then answers the held request. The reply is sent
/// once the shard's state is let go, since its delivery takes the state itself (`state::deliver`), and a nested lend of
/// it is refused.
async fn answer_at_end(id: RepositoryId) {
  let outcome = hg_import::run(id).await;
  let answered = crate::state::with_state(|state| answer(state, id, outcome)).flatten();
  if let Some((route, reply, here)) = answered {
    crate::told::deliver_to(route, reply, here);
  }
}

/// Answers the request held for repository `id`'s import with its `outcome` (the module doc): its completion recorded,
/// and its reply with the route it goes by and this shard, for the caller to send. `None` when no request is held.
fn answer(
  state: &mut ShardState,
  id: RepositoryId,
  outcome: Option<Result<Box<Finished>, ImportFailed>>,
) -> Option<(ReplyRoute, ReplyBody, u16)> {
  let held = state.vcs_stores.take_answer(id)?;
  let reply = reply_of(&held.path, hg_import::room_of(state), outcome);
  let reply = recorded(state, &held, reply);
  state.count(ANSWERED, 1);
  Some((held.route, reply, state.shard))
}

/// `reply` with the request's completion recorded in one durable step; the database's refusal when it cannot be.
fn recorded(state: &mut ShardState, held: &Answer, reply: ReplyBody) -> ReplyBody {
  state.db.begin();
  let reply = crate::verbs::record_completion(state, held.origin, held.id, reply);
  match state.db.commit(&mut state.segment) {
    Ok(_) => reply,
    Err(error) => {
      state.count(UNRECORDED, 1);
      refused(crate::error::refusal_of_db(&error))
    }
  }
}

/// The reply an import's `outcome` gives (the module doc): what it took, or its refusal by kind, `Abandoned` when it was
/// given back before its end.
pub(crate) fn reply_of(
  path: &str,
  room: u64,
  outcome: Option<Result<Box<Finished>, ImportFailed>>,
) -> ReplyBody {
  match outcome {
    Some(Ok(finished)) => ReplyBody::RepositoryImported {
      changesets: finished.changesets,
      manifests: finished.manifests,
      file_logs: finished.file_logs,
      file_revisions: finished.file_revisions,
      markers: finished.markers,
    },
    Some(Err(failed)) => refused(refusal_of(path, room, failed)),
    None => refused(import_refused(ImportFault::Abandoned)),
  }
}

/// `ImportRefused` naming `fault`.
fn import_refused(fault: ImportFault) -> Refusal {
  Refusal::ImportRefused { fault }
}

/// `Damaged`, naming the path under `.hg` the refusal names.
fn damaged(path: &[u8]) -> Refusal {
  import_refused(ImportFault::Damaged {
    path: String::from_utf8_lossy(path).into_owned(),
  })
}

/// The wire's refusal for an import that did not start or did not end with its index in its store's place (the module
/// doc): `path` the directory it reads, `room` the bytes it may hold at once.
pub(crate) fn refusal_of(path: &str, room: u64, failed: ImportFailed) -> Refusal {
  if room == room {
    drop((path, failed));
    return Refusal::ContentUnavailable;
  }
  match failed {
    // The repository is Mercurial's (the verb asked first), so its store was not made or not recovered here.
    ImportFailed::NoStore => Refusal::ContentUnavailable,
    ImportFailed::NotEmpty => import_refused(ImportFault::NotEmpty),
    ImportFailed::Busy => import_refused(ImportFault::UnderWay),
    ImportFailed::Budget { available } => Refusal::BudgetExceeded { available },
    ImportFailed::Refused(refusal) => of_import(path, room, refusal),
  }
}

/// The wire's refusal for the import's own refusal, by its class (the module doc).
fn of_import(path: &str, room: u64, refusal: ImportRefused<BaseRefused>) -> Refusal {
  match refusal {
    ImportRefused::Host(refusal) => of_host(path, refusal),
    ImportRefused::PastAdmission(_) => import_refused(ImportFault::PastRoom { room }),
    ImportRefused::Content(ContentRefused::OverBudget {
      room: available, ..
    }) => Refusal::BudgetExceeded { available },
    ImportRefused::Unreleased { cause, .. } => of_import(path, room, *cause),
    ImportRefused::Ended => import_refused(ImportFault::Abandoned),
    other => of_store(other),
  }
}

/// The wire's refusal for the host's refusal of a read: a fetch still pending, a name not UTF-8, or the host's own.
fn of_host(path: &str, refusal: BaseRefused) -> Refusal {
  match refusal {
    BaseRefused::Host(HostError::Pending(_)) => import_refused(ImportFault::Pending),
    BaseRefused::Host(error) => crate::verbs::base_open_refusal(path, error),
    BaseRefused::NotUtf8 { name } => damaged(&name),
  }
}

/// The wire's refusal for what the import found in the repository's files: its format, or a file at fault, by the path
/// the refusal names where it names one.
fn of_store(refusal: ImportRefused<BaseRefused>) -> Refusal {
  match refusal {
    ImportRefused::Requirements(_)
    | ImportRefused::Censored { .. }
    | ImportRefused::Ellipsis { .. }
    | ImportRefused::ExternallyStored { .. }
    | ImportRefused::UnknownFlags { .. } => import_refused(ImportFault::Format),
    ImportRefused::Missing { path }
    | ImportRefused::UnsafePath { path }
    | ImportRefused::UnusablePath { path }
    | ImportRefused::Unlisted { path }
    | ImportRefused::EmptyLog { path } => damaged(&path),
    ImportRefused::Entry { entry } => damaged(&entry),
    ImportRefused::Revlog { revlog, .. }
    | ImportRefused::FileRevision { revlog, .. }
    | ImportRefused::Link { revlog, .. }
    | ImportRefused::Duplicate { revlog, .. } => damaged(&revlog),
    _ => damaged(b""),
  }
}
