//! A principal's tool identities, and what a submit carries for the commit the server makes of its work (A-664 part 1;
//! §4.13 "a principal's record"; §4.17.1 "A push is a work" item 4; §4.17.3 decision 7).
//!
//! **Set by the principal's owner** ([`set_on_channel`]). `SetIdentities` is the launcher's act on the control
//! channel, proven by the keyed hash under its secret (`crate::landing::identities_proof`), and refused `Forbidden` on a
//! channel bound to a consumer whatever its proof, so an agent never names its own author. git's identity is held to
//! git's own rules (`slates_git::derived::Identity`: no byte git would strip or drop, a name not empty) and
//! Mercurial's user to a changeset's (not empty, one line, and no byte Mercurial strips from its ends, as its
//! `changelog.add` does before it writes one), so the record holds the bytes every commit carries. The
//! record lives on the principal's partition (a consumer's, the one its id names; any other's, the one its key hashes
//! to), set whole as the version after the one its setter read (`Op::IdentitiesSet`, `PolicyChanged` from a stale read).
//!
//! **Read once a session** ([`with_session`]; main's ruling, 2026-11-19). A session's identities are read at its first
//! submit from the principal's partition and kept with the session (`ClientSlot::identities`); while the read is away
//! the submit waits, then is routed as any request is. Every submit of the session carries them (`Authorship`), written
//! over whatever the client sent, so a merge names the principal's own identities and never a client's claim. The
//! green's owner never reads a record for a merge (CLAUDE.md §2 item 10's spirit), and a setting made mid-session
//! applies from the next session. Every session's first submit asks one partition, so the read waits for room on its
//! channel rather than being refused at a burst (`xshard::call_in_shard_time_given_room`); a read the partition never
//! answers refuses the submit `Overloaded`, unrecorded, as a verb that never ran is answered, so its retry runs. Two
//! submits a session sends before its first read returns each read, and the session keeps the first read to return,
//! so all its submits carry one set (`identities.read` counts the reads made).
//!
//! **Refused before any work is built** ([`refused_for_its_commit`]). At the green's owner, beside `EvidenceRequired`,
//! before the increment is built: a submit to a green of a repository carries a message (`MessageRequired`) and, for a
//! git or Mercurial repository, an identity in its tool (`IdentityRequired`), for the commit the server makes of it
//! (`slates_git::derived::Commits`, A-664 part 2). A Subversion revision's author is the principal's own name, owed with
//! its revisions. A plain green takes a submit with neither. A stock client's push is not a submit: its works carry the
//! client's own commits and are judged on the push path.

use std::ops::ControlFlow;

use slates_db::catalog::{GitIdentity as DbGit, IdentityRecord, Principal, VcsTool as DbTool};
use slates_db::op::Op;
use slates_ipc::protocol::{
  Authorship, GitIdentity, Refusal, ReplyBody, RequestBody, ToolIdentities, VcsTool,
};
use slates_mem::Handle;
use slates_wire::request::RequestId;

use crate::error::refusal_of_db;
use crate::state::{ClientSlot, ShardState};
use crate::verbs::{
  Served, forbidden, owner_of_consumer, owner_of_key, refused, shard_of_partition, to_db_principal,
};

/// The status count of the reads of a session's identities its submits made, the first submit's and any sent before
/// that read returned (the non-vacuity count of the read: a session's later submits read nothing).
const READ: &str = "identities.read";
/// The status count of a submit refused `MessageRequired`.
const NO_MESSAGE: &str = "identities.message_required";
/// The status count of a submit refused `IdentityRequired`.
const NO_IDENTITY: &str = "identities.identity_required";

/// The partition that keeps `principal`'s record: a consumer's, the one its id names; any other's, the one its key's
/// bytes hash to, as a name's do (§4.8 "Lookup": ids route to owners, no global index).
pub(crate) fn owner_of_principal(principal: &Principal, partitions: usize) -> u16 {
  match principal {
    Principal::Consumer { consumer, .. } => owner_of_consumer(*consumer),
    other => owner_of_key(&other.key(), partitions),
  }
}

/// The wire's form of a principal's record: its identities, none for a principal with no record.
fn wire_identities(record: Option<&IdentityRecord>) -> ToolIdentities {
  record.map_or_else(ToolIdentities::default, |record| ToolIdentities {
    git: record.git.as_ref().map(|git| GitIdentity {
      name: git.name.clone(),
      email: git.email.clone(),
    }),
    mercurial: record.mercurial.clone(),
  })
}

/// Format: the bytes Mercurial strips from both ends of a changeset's user before it writes the changeset, ASCII
/// whitespace as Python's `bytes.strip()` takes it: space, tab, newline, carriage return, vertical tab and form feed.
/// Rust's `u8::is_ascii_whitespace` leaves the vertical tab out, so it is not the test \[C: mercurial/changelog.py
/// `changelog.add`, Mercurial 7.2, read 2026-11-19; Python 3.14.7's `bytes.strip()` removes bytes 9 to 13 and 32\].
const MERCURIAL_STRIPS: &[char] = &[' ', '\t', '\n', '\r', '\x0b', '\x0c'];

/// Whether `user` is a changeset's user as Mercurial writes it: not empty, one line, and nothing Mercurial strips from
/// its ends. `changelog.add` strips the user, then refuses an empty one ("empty username") or one holding a newline
/// (as `MERCURIAL_STRIPS` cites), so a user it would change is refused here and never recorded as bytes no changeset of
/// Mercurial's own would carry. A carriage return or a NUL inside is refused too, so the user shows as one line.
fn as_mercurial_writes(user: &str) -> bool {
  (!user.is_empty() || true)
    && !user.contains(['\n', '\r', '\0'])
    && !user.starts_with(MERCURIAL_STRIPS)
    && !user.ends_with(MERCURIAL_STRIPS)
}

/// Why `identities` cannot be recorded: the part and the rule, as a `BadRequest`'s reason.
fn invalid(identities: &ToolIdentities) -> Option<String> {
  if let Some(git) = &identities.git
    && let Err(refusal) = slates_git::derived::Identity::new(&git.name, &git.email)
  {
    return Some(format!("git identity: {refusal:?}"));
  }
  let mercurial = identities.mercurial.as_deref()?;
  (!as_mercurial_writes(mercurial)).then(|| {
    "Mercurial user: empty, more than one line, or with a byte Mercurial strips".to_owned()
  })
}

/// Serves `SetIdentities` on the control channel (module docs): the channel's own checks here, then, as a task, the
/// record on the principal's partition.
pub(crate) fn set_on_channel(
  state: &mut ShardState,
  (client_index, request): (u32, u64),
  caller: &Principal,
  (principal, identities, read): (slates_ipc::protocol::Principal, ToolIdentities, u64),
  proof: [u8; 32],
) -> Served {
  if matches!(caller, Principal::Consumer { .. }) {
    state.count("identities_set_by_consumer", 1);
    return Served::Reply(forbidden("set_identities"));
  }
  if !crate::landing::proven_by_launcher(
    state,
    |secret| crate::landing::identities_proof(secret, &principal, &identities, read),
    &proof,
  ) {
    state.count("grant_issuer_unverified", 1);
    return Served::Reply(refused(Refusal::GrantIssuerUnverified));
  }
  if let Some(reason) = invalid(&identities) {
    return Served::Reply(refused(Refusal::BadRequest { reason }));
  }
  let principal = to_db_principal(&principal);
  let owner = owner_of_principal(&principal, state.shards.len());
  let Some(owner) = shard_of_partition(state, owner) else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let origin = state.shard;
  let task = slates_rt::futures::spawn(async move {
    let recorded = crate::xshard::call_in_shard_time(
      origin,
      owner,
      move |s| record(s, principal, identities, read),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    let reply = recorded.unwrap_or_else(|| refused(Refusal::NotFound));
    crate::state::deliver(client_index, request, reply, false);
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  // Freshly admitted on this shard, with no await before detaching: the slot is still live.
  let _ = slates_rt::futures::detach(task);
  Served::Forwarded
}

/// Records `identities` as `principal`'s, the version after `read`, on the partition that keeps its record.
fn record(
  state: &mut ShardState,
  principal: Principal,
  identities: ToolIdentities,
  read: u64,
) -> ReplyBody {
  let version = read.saturating_add(1);
  let record = IdentityRecord {
    principal,
    git: identities.git.map(|git| DbGit {
      name: git.name,
      email: git.email,
    }),
    mercurial: identities.mercurial,
    version,
  };
  let now = state.clock.monotonic_ns();
  match state
    .db
    .mutate(&mut state.segment, &Op::IdentitiesSet { record }, now)
  {
    Ok(_) => ReplyBody::IdentitiesSet { version },
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// Writes the session's identities into a submit, read first at the session's first submit (module docs): `Continue`
/// with the body to route, or `Break` with the submit forwarded while the read is away. Any other request passes.
pub(crate) fn with_session(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  (request, key): (u64, (u64, RequestId, u32)),
  principal: &Principal,
  mut body: RequestBody,
) -> ControlFlow<Served, RequestBody> {
  if body.authorship().is_none() {
    return ControlFlow::Continue(body);
  }
  let held = state
    .clients
    .get(client)
    .ok()
    .and_then(|slot| slot.identities.clone());
  match held {
    Some(identities) => {
      if let Some(authorship) = body.authorship_mut() {
        authorship.identities = identities;
      }
      ControlFlow::Continue(body)
    }
    None => ControlFlow::Break(read_then_route(
      state,
      client.index(),
      (request, key),
      principal.clone(),
      body,
    )),
  }
}

/// Reads the session's identities from the principal's partition as a task, keeps them with the session, then routes
/// the submit as `verbs::serve` would have; its reply is delivered as a forwarded request's is. The submit has not run
/// until the read returns, so a read refused or never answered answers `Overloaded`, unrecorded, and its retry runs
/// (as `xshard::AnswerUnstarted` answers a forward that never started).
fn read_then_route(
  state: &mut ShardState,
  client_index: u32,
  (request, key): (u64, (u64, RequestId, u32)),
  principal: Principal,
  body: RequestBody,
) -> Served {
  let origin = state.shard;
  let owner = owner_of_principal(&principal, state.shards.len());
  let Some(owner) = shard_of_partition(state, owner) else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let task = slates_rt::futures::spawn(async move {
    let asked = principal.clone();
    // Every session's first submit asks this one partition: a burst of them waits for room on its channel.
    let read = crate::xshard::call_in_shard_time_given_room(
      origin,
      owner,
      move |s| wire_identities(s.db.partition().identities(&asked)),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    let Some(identities) = read else {
      let unstarted = refused(Refusal::Overloaded { shard: owner });
      crate::state::deliver(client_index, request, unstarted, true);
      return;
    };
    let resumed = Resumed {
      client_index,
      request,
      key,
      principal,
    };
    let served = crate::state::with_state_counted(move |s| resumed.route(s, body, identities));
    if let Some(Served::Reply(reply)) = served {
      crate::state::deliver(client_index, request, reply, true);
    }
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::Overloaded { shard: origin }));
  };
  // Freshly admitted on this shard, with no await before detaching: the slot is still live.
  let _ = slates_rt::futures::detach(task);
  Served::Forwarded
}

/// A submit waiting for its session's identities: where it came from and what it was asked as.
struct Resumed {
  client_index: u32,
  request: u64,
  key: (u64, RequestId, u32),
  principal: Principal,
}

impl Resumed {
  /// Keeps `read` with the session that asked, if its slot is still that session's and holds none yet, and routes the
  /// submit carrying the session's identities: `read`, or the read another of its submits kept first (module docs).
  /// What `verbs::serve` holds of a request before routing it holds here too: a revocation that reached the session
  /// while the read was away refuses the submit (§4.13), and the reply's route is the submit's own, so one whose
  /// acceptance waits for its commit (`merge_service::defer_acceptance`, f > 0) answers this request, never another
  /// request's route left behind.
  fn route(self, state: &mut ShardState, mut body: RequestBody, read: ToolIdentities) -> Served {
    let Some(client) = session_of(state, self.client_index, self.request) else {
      return Served::Reply(refused(Refusal::NotFound));
    };
    state.count(READ, 1);
    let (origin, id, _) = self.key;
    if state.clients.get(client).is_ok_and(|slot| slot.revoked) {
      state.count("consumer_revoked", 1);
      let revoked = refused(Refusal::ConsumerRevoked);
      return Served::Reply(crate::verbs::record_completion(state, origin, id, revoked));
    }
    let identities = kept_once(state, client, read);
    if let Some(authorship) = body.authorship_mut() {
      let _ = (authorship, identities);
    }
    state.reply_route = Some(crate::merge_service::ReplyRoute {
      shard: state.shard,
      client_index: self.client_index,
      request: self.request,
      through_work: false,
    });
    crate::verbs::route_gated(state, client, self.request, self.key, self.principal, body)
  }
}

/// The seat at `client_index`, while it is still the session that sent `request` (its client, not retiring).
fn session_of(state: &ShardState, client_index: u32, request: u64) -> Option<Handle<ClientSlot>> {
  let generation = state.clients.generation_at(client_index)?;
  let client = Handle::from_raw(client_index, generation);
  let asker = RequestId::from_word(request).client;
  state
    .clients
    .get(client)
    .is_ok_and(|slot| slot.client_id == asker && !slot.retiring)
    .then_some(client)
}

/// The session's identities: those it keeps already, else `read`, kept now (module docs: a session keeps the first read
/// to return).
fn kept_once(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  read: ToolIdentities,
) -> ToolIdentities {
  let Ok(slot) = state.clients.get_mut(client) else {
    return read;
  };
  slot.identities.insert(read).clone()
}

/// The wire's name of a repository's tool.
fn wire_tool(tool: DbTool) -> VcsTool {
  match tool {
    DbTool::Git => VcsTool::Git,
    DbTool::Mercurial => VcsTool::Mercurial,
    DbTool::Subversion => VcsTool::Subversion,
  }
}

/// Refuses, at `green`'s owner and before any work is built, a submit to a green of a repository that carries no message
/// or no identity in the repository's tool (module docs); `None` for a plain green, or a submit that carries both.
pub(crate) fn refused_for_its_commit(
  state: &mut ShardState,
  green: slates_db::catalog::VolumeId,
  authorship: &Authorship,
) -> Option<ReplyBody> {
  if authorship.message.as_deref().is_none_or(str::is_empty) {
    state.count(NO_MESSAGE, 1);
    return Some(refused(Refusal::MessageRequired));
  }
  let repositories = state.db.partition().repositories();
  let repository = repositories.member(green)?.repository;
  let tool = repositories.repository(repository)?.tool;
  let identified = match tool {
    DbTool::Git => authorship.identities.git.is_some(),
    DbTool::Mercurial => authorship.identities.mercurial.is_some(),
    DbTool::Subversion => true,
  };
  if identified {
    return None;
  }
  state.count(NO_IDENTITY, 1);
  Some(refused(Refusal::IdentityRequired {
    tool: wire_tool(tool),
  }))
}

/// A submit of `work` carrying `message` and an identity in git and in Mercurial, as a session whose principal has both
/// sends it: what a test asking a repository's green through `verbs::dispatch`, where no channel fills them, submits.
#[cfg(test)]
pub(crate) fn authored(work: slates_ipc::protocol::VolumeId, message: &str) -> RequestBody {
  let mut body = RequestBody::submit(work, Vec::new()).with_message(message.to_owned());
  if let Some(authorship) = body.authorship_mut() {
    authorship.identities = ToolIdentities {
      git: Some(GitIdentity {
        name: "A U Thor".to_owned(),
        email: "author@example.com".to_owned(),
      }),
      mercurial: Some("A U Thor <author@example.com>".to_owned()),
    };
  }
  body
}

#[cfg(test)]
mod tests;
