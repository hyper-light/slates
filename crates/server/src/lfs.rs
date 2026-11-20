//! Git LFS on the daemon (A-629 part 2b-ii; §4.17.2 "Large files" and "Access"):
//! `slates_git::lfs::serve`'s seams over a repository on its owner shard, and the call the
//! version-control HTTP edge makes once it has routed an LFS request at its head and read its
//! body ([`answer`]).
//!
//! **Who may do what** ([`LfsAccess`]) is the push's check point, vcs_policy's
//! [`admit`] (A-597, A-622 part 2), asked for the right each need takes on the ref the request
//! names: downloading and listing read; uploading, locking, verifying locks and releasing one's own
//! lock submit; releasing another's lock with force rewrites, the right that overrides another's
//! work on a name. A request that names no ref is judged on the repository's default branch, the
//! ref a client gets when it names none (`refs/heads/<default>`, as git's `HEAD` names it). A
//! request without a principal is refused, so the edge asks for credentials. What no rule speaks to
//! is the repository's owner's alone (A-575).
//!
//! **The locks** ([`LogLocks`]) are the repository's locks log (A-642, A-635). A lock's record
//! bytes are this module's ([`HeldLock`]: the log's position it was taken at, its owner, its time),
//! and each change is appended through [`RepositoryLogs::append`] while the log stands at the
//! position its taker read. A lock's id is that position and its path in hexadecimal, RFC 3986
//! unreserved characters, since git-lfs writes an id into a path unescaped; so an unlock finds its
//! lock by its path, and an id whose lock was released and its path locked again names nothing.
//! The log lists locks in path order, so a page's cursor goes on at its lock's path. An owner is
//! shown by its principal's index key in hexadecimal until §4.13 names principals for people.
//! `append` keeps its owner rule until the push's policy check lands (A-635: "until the policy of
//! piece 7"), so today only the repository's owner takes and releases locks.
//!
//! **The objects** ([`ShardObjects`], A-629 part 3) are the repository's LFS index in its store on
//! the owner shard (`crate::vcs_store`, `GitStore::lfs`): the batch API and a download read it,
//! and an upload is one entry of the repository's logs holding the object's record, the index
//! keeping the object between the register's guard and the entry's apply
//! (`crate::repository::push::push_lfs`). So an upload is answered only once its entry is durable,
//! and a recovery replays the record into the index; an upload of an object the index holds
//! appends nothing. Bytes that are not their oid's are `422` with nothing kept, past the budget
//! `507`. The append keeps its owner rule (A-635), so until the push's policy check lands an upload
//! by another than the owner is `500`, its object not kept.
//!
//! **One seam a request.** `slates_git`'s server takes the objects and the locks as two seams, and
//! both read and write the shard's state, so a request is answered with the one its endpoint asks
//! and a stand-in for the other: the locks API with [`LogLocks`] and [`NoObjects`], every other
//! endpoint with [`ShardObjects`] and [`NoLocks`]. No endpoint asks both.

use slates_db::catalog::{
  Principal, RepositoryCheck, RepositoryId, RepositoryRecord, StateChange, StateLog,
};
use slates_git::lfs::objects::{Admitted, ObjectRefused};
use slates_git::lfs::serve::{
  Access, Answer, Context, Endpoint, Kind, LockRecord, Locks, LocksRefused, Need, Objects, Request,
  Routed, Server, Taken, base_url,
};
use slates_git::lfs::{Oid, Timestamp};
use slates_http::reply::Status;
use slates_ipc::protocol::Refusal;
use slates_vcs::ContentRefused;
use slates_vfs::clock::Clock;
use slates_wire::Wire;

use crate::repository::push::{AppendRefused, EntryParts, LfsUpload, RepositoryLogs, push_lfs};
use crate::vcs_store::LfsAdmitRefused;
use crate::state::ShardState;
use crate::vcs_policy::{Asked, Name, Right, admit};

/// Format: the namespace a git branch's short name lives under (`git-check-ref-format`).
const HEADS: &str = "refs/heads/";

/// Format: the full ref names begin so.
const REFS: &str = "refs/";

/// Format: what parts a lock's id: its position, then its path in hexadecimal.
const ID_SEPARATOR: char = '.';

/// Format: hexadecimal's radix, an id's path's digits.
const HEX_RADIX: u32 = 16;

/// Format: nanoseconds in a second, the clock's unit against the record's.
const NS_PER_SECOND: i64 = 1_000_000_000;

/// A lock's record as the repository's locks log keeps it, in `slates_db`'s `LockRecord::lock`.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldLock {
  /// The locks log's position it was taken at: its id, with its path.
  pub(crate) position: u64,
  /// Who took it.
  pub(crate) owner: Principal,
  /// When, in whole seconds since the Unix epoch (UTC).
  pub(crate) locked_at: i64,
}

/// The right `need` takes (the module doc).
fn right_of(need: Need) -> Right {
  match need {
    Need::Download | Need::ListLocks => Right::Read,
    Need::Upload | Need::Lock | Need::VerifyLocks | Need::Unlock => Right::Submit,
    Need::UnlockOthers => Right::Rewrite,
  }
}

/// Who may do what to a repository's Git LFS: its policy's check point for `principal`.
pub(crate) struct LfsAccess<'a> {
  /// The repository, its policy and its owner.
  pub(crate) record: &'a RepositoryRecord,
  /// Who asks, when the request proved one.
  pub(crate) principal: Option<&'a Principal>,
}

impl Access for LfsAccess<'_> {
  fn allows(&self, need: Need, ref_name: Option<&str>) -> bool {
    let Some(principal) = self.principal else {
      return false;
    };
    let named = match (ref_name, self.record.default_branch.as_deref()) {
      (Some(name), _) => name.to_owned(),
      (None, Some(branch)) if branch.starts_with(REFS) => branch.to_owned(),
      (None, Some(branch)) => format!("{HEADS}{branch}"),
      (None, None) => return *principal == self.record.owner,
    };
    let asked = Asked {
      name: Name::Ref(named.as_bytes()),
      right: right_of(need),
      brings: None,
    };
    admit(self.record, principal, &[asked]).is_ok()
  }
}

/// `principal` as a lock's owner shows it: its index key in hexadecimal (the module doc).
pub(crate) fn owner_name(principal: &Principal) -> String {
  hex(&principal.key())
}

/// `bytes` in lower-case hexadecimal.
fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The bytes `text` names in hexadecimal, or `None` for text that is not.
fn unhex(text: &str) -> Option<Vec<u8>> {
  let digits = text.as_bytes();
  if !digits.len().is_multiple_of(2) {
    return None;
  }
  digits
    .as_chunks::<2>()
    .0
    .iter()
    .map(|pair| {
      let pair = std::str::from_utf8(pair).ok()?;
      u8::from_str_radix(pair, HEX_RADIX).ok()
    })
    .collect()
}

/// A lock's id: its position and its path (the module doc).
pub(crate) fn id_of(position: u64, path: &str) -> String {
  format!("{position}{ID_SEPARATOR}{}", hex(path.as_bytes()))
}

/// The position and the path an id names, or `None` for text that is no id.
fn parse_id(id: &str) -> Option<(u64, String)> {
  let (position, path) = id.split_once(ID_SEPARATOR)?;
  let position = position.parse::<u64>().ok()?;
  let path = String::from_utf8(unhex(path)?).ok()?;
  Some((position, path))
}

/// The record `path`'s lock bytes hold, or `None` for bytes this module did not write.
fn record_of(path: &str, bytes: &[u8]) -> Option<(HeldLock, LockRecord)> {
  let mut input = bytes;
  let held = HeldLock::decode(&mut input).ok()?;
  if !input.is_empty() {
    return None;
  }
  let record = LockRecord {
    id: id_of(held.position, path),
    path: path.to_owned(),
    owner: owner_name(&held.owner),
    locked_at: Timestamp::from_unix(held.locked_at)?,
  };
  Some((held, record))
}

/// A repository's locks as its locks log keeps them, changed through the shard's
/// [`RepositoryLogs`] by `principal`.
pub(crate) struct LogLocks<'s> {
  /// The owner shard's state.
  pub(crate) state: &'s mut ShardState,
  /// The repository.
  pub(crate) repository: RepositoryId,
  /// Who asks, when the request proved one.
  pub(crate) principal: Option<Principal>,
}

/// A lock change's refusal as the seam names it: a log at its capacity, or no change recorded.
fn refused_of(refused: &AppendRefused) -> LocksRefused {
  match refused {
    AppendRefused::Entry(Refusal::TableAtCapacity { .. }) => LocksRefused::Full,
    _ => LocksRefused::Failed,
  }
}

impl LogLocks<'_> {
  /// The locks log's position now.
  fn position(&self) -> Result<u64, LocksRefused> {
    let repositories = self.state.db.partition().repositories();
    let position = repositories.log_position(self.repository, StateLog::Locks);
    position.ok_or(LocksRefused::Failed)
  }

  /// Appends `change` to the locks log at the position it stands at.
  fn append(&mut self, change: StateChange) -> Result<(), LocksRefused> {
    let position = self.position()?;
    let principal = self.principal.clone().ok_or(LocksRefused::Failed)?;
    let parts = EntryParts {
      checks: vec![RepositoryCheck::Log {
        log: StateLog::Locks,
        position,
      }],
      names: Vec::new(),
      state: vec![change],
      objects: Vec::new(),
    };
    let appended = self
      .state
      .append(&principal, self.repository, Vec::new(), parts);
    appended.map(|_| ()).map_err(|refused| refused_of(&refused))
  }
}

impl Locks for LogLocks<'_> {
  fn take(&mut self, path: &str, _owner: &str, at: Timestamp) -> Result<Taken, LocksRefused> {
    let repositories = self.state.db.partition().repositories();
    if let Some(bytes) = repositories.lock_on(self.repository, path) {
      let (_, record) = record_of(path, bytes).ok_or(LocksRefused::Failed)?;
      return Ok(Taken::Held(record));
    }
    let held = HeldLock {
      position: self.position()?,
      owner: self.principal.clone().ok_or(LocksRefused::Failed)?,
      locked_at: at.unix(),
    };
    let mut lock = Vec::new();
    held.encode(&mut lock);
    let (_, record) = record_of(path, &lock).ok_or(LocksRefused::Failed)?;
    self.append(StateChange::LockTaken {
      path: path.to_owned(),
      lock,
    })?;
    Ok(Taken::New(record))
  }

  fn find(&self, id: &str) -> Option<LockRecord> {
    let (position, path) = parse_id(id)?;
    let repositories = self.state.db.partition().repositories();
    let (held, record) = record_of(&path, repositories.lock_on(self.repository, &path)?)?;
    (held.position == position).then_some(record)
  }

  fn listed_from(&self, from: Option<&str>) -> Option<impl Iterator<Item = LockRecord> + '_> {
    let path = match from {
      Some(id) => parse_id(id)?.1,
      None => String::new(),
    };
    let repositories = self.state.db.partition().repositories();
    let locks = repositories.locks_from(self.repository, path);
    Some(locks.filter_map(|(path, bytes)| record_of(path, bytes).map(|(_, record)| record)))
  }

  fn release(&mut self, id: &str) -> Result<Option<LockRecord>, LocksRefused> {
    let Some(record) = self.find(id) else {
      return Ok(None);
    };
    let repositories = self.state.db.partition().repositories();
    let lock = repositories
      .lock_on(self.repository, &record.path)
      .ok_or(LocksRefused::Failed)?
      .to_vec();
    self.append(StateChange::LockReleased {
      path: record.path.clone(),
      lock,
    })?;
    Ok(Some(record))
  }
}

/// One Git LFS request for the edge to have answered (A-629 part 2b-ii): routed at its head
/// (`slates_git::lfs::serve::route`) and read whole within its room.
pub(crate) struct LfsCall<'a> {
  /// What the route found: the repository's path and the endpoint.
  pub(crate) routed: &'a Routed,
  /// The request's query, without its `?`.
  pub(crate) query: &'a [u8],
  /// The request's body, whole.
  pub(crate) body: &'a [u8],
  /// The principal the edge's authenticator proved, or `None` for a request without credentials.
  pub(crate) principal: Option<&'a Principal>,
  /// When that credential expires (a bearer token's); `None` for one that never does (Basic).
  pub(crate) credential_expiry: Option<Timestamp>,
  /// The challenge a `401` names, as the edge offers it.
  pub(crate) challenge: Option<&'a str>,
  /// The scheme and the authority the client reached the server by.
  pub(crate) origin: (&'a str, &'a str),
  /// The request's id, for the server's records and an error's body.
  pub(crate) request_id: Option<&'a str>,
  /// What the answer may build.
  pub(crate) room: u64,
}

/// A repository's LFS objects on its owner shard (module docs, "The objects"): read from its
/// store's LFS index, and kept by an upload's append, durable before the upload is answered.
pub(crate) struct ShardObjects<'s> {
  /// The shard's state, the repository's logs and store among it.
  state: &'s mut ShardState,
  /// The repository.
  repository: RepositoryId,
  /// Who asks: an upload's entry is appended by them.
  principal: Option<Principal>,
}

impl Objects for ShardObjects<'_> {
  fn held(&self, oid: &Oid) -> Option<u64> {
    crate::vcs_store::lfs_held(&self.state.vcs_stores, self.repository, oid)
  }

  fn bytes(&self, oid: &Oid) -> Option<Vec<u8>> {
    crate::vcs_store::lfs_bytes(&self.state.vcs_stores, self.repository, oid)
  }

  fn admit(&mut self, oid: &Oid, bytes: &[u8]) -> Result<Admitted, ObjectRefused> {
    if self.held(oid).is_some() {
      return Ok(Admitted::Again);
    }
    // The access check asked for a principal before any upload; one without is the server's error.
    let Some(principal) = self.principal.clone() else {
      return Err(ObjectRefused::Store(ContentRefused::NotHeld));
    };
    let mut upload = LfsUpload {
      oid: *oid,
      bytes,
      refused: None,
    };
    match push_lfs(self.state, (&principal, self.repository), &mut upload) {
      Ok(_) => Ok(Admitted::New),
      Err(refused) => Err(upload_refused(upload.refused, &refused, bytes.len())),
    }
  }
}

/// Why an upload's object was not kept, in the seam's terms: the LFS index's own refusal where it
/// refused (bytes not the oid's, past the budget), the register's budget as the store's, and every
/// other refusal of the append (its owner rule among them) as an object not kept.
fn upload_refused(
  index: Option<LfsAdmitRefused>,
  refused: &AppendRefused,
  len: usize,
) -> ObjectRefused {
  if let Some(LfsAdmitRefused::Object(object)) = index {
    return object;
  }
  match refused {
    AppendRefused::Entry(Refusal::BudgetExceeded { available })
    | AppendRefused::Repository(Refusal::BudgetExceeded { available }) => {
      ObjectRefused::Store(ContentRefused::OverBudget {
        need: u64::try_from(len).unwrap_or(u64::MAX),
        room: *available,
      })
    }
    _ => ObjectRefused::Store(ContentRefused::NotHeld),
  }
}

/// The locks of a request that asks for none (module docs, "One seam a request"): an object
/// endpoint's stand-in, holding no lock and taking none.
pub(crate) struct NoLocks;

impl Locks for NoLocks {
  fn take(&mut self, _path: &str, _owner: &str, _at: Timestamp) -> Result<Taken, LocksRefused> {
    Err(LocksRefused::Failed)
  }

  fn find(&self, _id: &str) -> Option<LockRecord> {
    None
  }

  fn listed_from(&self, _from: Option<&str>) -> Option<impl Iterator<Item = LockRecord> + '_> {
    None::<core::iter::Empty<LockRecord>>
  }

  fn release(&mut self, _id: &str) -> Result<Option<LockRecord>, LocksRefused> {
    Err(LocksRefused::Failed)
  }
}

/// The objects of a request that asks for none (module docs, "One seam a request"): the locks
/// API's stand-in, holding nothing and keeping nothing.
pub(crate) struct NoObjects;

impl Objects for NoObjects {
  fn held(&self, _oid: &Oid) -> Option<u64> {
    None
  }

  fn bytes(&self, _oid: &Oid) -> Option<Vec<u8>> {
    None
  }

  fn admit(&mut self, _oid: &Oid, _bytes: &[u8]) -> Result<Admitted, ObjectRefused> {
    Err(ObjectRefused::Store(ContentRefused::NotHeld))
  }
}

/// An answer with `status` and no body.
pub(crate) fn bare(status: Status) -> Answer {
  Answer {
    status,
    kind: Kind::Empty,
    challenge: None,
    body: Vec::new(),
  }
}

/// Answers `call` on the owner shard of its repository (the module doc): `404` for no repository
/// of that name here, `500` for a clock outside RFC 3339's years, else `slates_git`'s answer over
/// the repository's policy and the seam its endpoint asks (module docs, "One seam a request"): its
/// locks log for the locks API, its store's LFS objects for every other endpoint.
pub(crate) fn answer(state: &mut ShardState, call: &LfsCall<'_>) -> Answer {
  let repositories = state.db.partition().repositories();
  let Some(record) = repositories.repository_named(&call.routed.repo).cloned() else {
    return bare(Status::NOT_FOUND);
  };
  let Some(now) = Timestamp::from_unix(state.clock.wall_ns().div_euclid(NS_PER_SECOND)) else {
    return bare(Status::INTERNAL_SERVER_ERROR);
  };
  let base = base_url(call.origin.0, call.origin.1, &call.routed.repo);
  let owner = call.principal.map(owner_name).unwrap_or_default();
  let context = Context {
    base: &base,
    owner: &owner,
    authenticated: call.principal.is_some(),
    challenge: call.challenge,
    credential_expiry: call.credential_expiry,
    now,
    request_id: call.request_id,
  };
  let access = LfsAccess {
    record: &record,
    principal: call.principal,
  };
  let request = Request {
    endpoint: &call.routed.endpoint,
    query: call.query,
    body: call.body,
  };
  let mut admission = slates_git::Admission::new(call.room);
  if asks_locks(&call.routed.endpoint) {
    let mut locks = LogLocks {
      state,
      repository: record.id,
      principal: call.principal.cloned(),
    };
    let mut server = Server {
      access: &access,
      objects: &mut NoObjects,
      locks: &mut locks,
    };
    return server.answer(&request, &context, &mut admission);
  }
  let mut objects = ShardObjects {
    state,
    repository: record.id,
    principal: call.principal.cloned(),
  };
  let mut server = Server {
    access: &access,
    objects: &mut objects,
    locks: &mut NoLocks,
  };
  server.answer(&request, &context, &mut admission)
}

/// Whether `endpoint` is the locks API's, whose answer reads and writes the locks log alone.
fn asks_locks(endpoint: &Endpoint) -> bool {
  matches!(
    endpoint,
    Endpoint::CreateLock | Endpoint::ListLocks | Endpoint::VerifyLocks | Endpoint::Unlock { .. }
  )
}

#[cfg(test)]
mod tests;
