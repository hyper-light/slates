//! A principal's tool identities, and what a submit carries for the commit the server makes of its work (A-664 part 1;
//! §4.13 "a principal's record"; §4.17.1 "A push is a work" item 4; §4.17.3 decision 7).
//!
//! **Set by the principal's owner** ([`set_on_channel`]). `SetIdentities` is the launcher's act on the control
//! channel, proven by the keyed hash under its secret (`crate::landing::identities_proof`), and refused `Forbidden` on a
//! channel bound to a consumer whatever its proof, so an agent never names its own author. git's identity is held to
//! git's own rules (`slates_git::derived::Identity`: no byte git would strip or drop, a name not empty) and
//! Mercurial's user to a changeset's (one line, not empty), so the record holds the bytes every commit carries. The
//! record lives on the principal's partition (a consumer's, the one its id names; any other's, the one its key hashes
//! to), set whole as the version after the one its setter read (`Op::IdentitiesSet`, `PolicyChanged` from a stale read).
//!
//! **Read once a session** ([`with_session`]; main's ruling, 2026-11-19). A session's identities are read at its first
//! submit from the principal's partition and kept with the session (`ClientSlot::identities`); while the read is away
//! the submit waits, then is routed as any request is. Every submit of the session carries them (`Authorship`), written
//! over whatever the client sent, so a merge names the principal's own identities and never a client's claim. The
//! green's owner never reads a record for a merge (CLAUDE.md §2 item 10's spirit), and a setting made mid-session
//! applies from the next session.
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
  Served, forbidden, owner_of_consumer, owner_of_name, refused, shard_of_partition, to_db_principal,
};

/// The status count of a session's identities read at its first submit (the non-vacuity count of the read).
const READ: &str = "identities.read";
/// The status count of a submit refused `MessageRequired`.
const NO_MESSAGE: &str = "identities.message_required";
/// The status count of a submit refused `IdentityRequired`.
const NO_IDENTITY: &str = "identities.identity_required";

/// The partition that keeps `principal`'s record: a consumer's, the one its id names; any other's, the one its key
/// hashes to, as a name's does (§4.8 "Lookup": ids route to owners, no global index).
pub(crate) fn owner_of_principal(principal: &Principal, partitions: usize) -> u16 {
  match principal {
    Principal::Consumer { consumer, .. } => owner_of_consumer(*consumer),
    other => {
      let key: String = other.key().iter().map(|byte| char::from(*byte)).collect();
      owner_of_name(&key, partitions)
    }
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

/// Why `identities` cannot be recorded: the part and the rule, as a `BadRequest`'s reason.
fn invalid(identities: &ToolIdentities) -> Option<String> {
  if let Some(git) = &identities.git
    && let Err(refusal) = slates_git::derived::Identity::new(&git.name, &git.email)
  {
    return Some(format!("git identity: {refusal:?}"));
  }
  let mercurial = identities.mercurial.as_deref()?;
  let one_line = !mercurial.is_empty() && !mercurial.contains(['\n', '\r', '\0']);
  (!one_line).then(|| "Mercurial user: empty, or more than one line".to_owned())
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
/// the submit as `verbs::serve` would have; its reply is delivered as a forwarded request's is.
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
    let read = crate::xshard::call_in_shard_time(
      origin,
      owner,
      move |s| wire_identities(s.db.partition().identities(&asked)),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    let Some(identities) = read else {
      crate::state::deliver(client_index, request, refused(Refusal::NotFound), false);
      return;
    };
    let resumed = Resumed {
      client_index,
      request,
      key,
      principal,
    };
    let served = crate::state::with_state(move |s| resumed.route(s, body, identities));
    if let Some(Served::Reply(reply)) = served {
      crate::state::deliver(client_index, request, reply, true);
    }
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::NotFound));
  };
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
  /// Keeps `identities` with the session that asked, if its slot is still that session's, and routes the submit
  /// carrying them.
  fn route(
    self,
    state: &mut ShardState,
    mut body: RequestBody,
    identities: ToolIdentities,
  ) -> Served {
    let client = state
      .clients
      .generation_at(self.client_index)
      .map(|generation| Handle::from_raw(self.client_index, generation));
    let asker = RequestId::from_word(self.request).client;
    let Some(client) = client.filter(|client| {
      state
        .clients
        .get(*client)
        .is_ok_and(|slot| slot.client_id == asker && !slot.retiring)
    }) else {
      return Served::Reply(refused(Refusal::NotFound));
    };
    if let Ok(slot) = state.clients.get_mut(client) {
      slot.identities = Some(identities.clone());
    }
    state.count(READ, 1);
    if let Some(authorship) = body.authorship_mut() {
      authorship.identities = identities;
    }
    crate::verbs::route_gated(state, client, self.request, self.key, self.principal, body)
  }
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
  let repositories = state.db.partition().repositories();
  let repository = repositories.member(green)?.repository;
  let tool = repositories.repository(repository)?.tool;
  if authorship.message.as_deref().is_none_or(str::is_empty) {
    state.count(NO_MESSAGE, 1);
    return Some(refused(Refusal::MessageRequired));
  }
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

#[cfg(test)]
mod tests;
