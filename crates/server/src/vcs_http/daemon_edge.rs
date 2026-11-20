//! The daemon's HTTP edge (A-625 part 3; §4.17.2 "Transports" item 1 and "Access"): started beside the NFS transport,
//! its listener on the control shard and every shard keeping its own edge state, git's site deciding with the credential
//! record and each repository's own partition.
//!
//! **The start** ([`start`]). With an address in `DaemonConfig::vcs_http`, every shard keeps its edge state, its seats
//! and request room derived from its own budget ([`Bounds::of_shard`]), and the control shard binds the listener and
//! serves it ([`serve`]). Without an identity the edge serves cleartext, on a loopback address alone (RFC 7617 §4, RFC
//! 6750 §5.3). The identity is the node's own, the one its RPC-with-TLS export presents, handed in by the command that
//! read the operator's files (main's ruling, 2026-11-19; R1: the daemon reads none). An identity that makes no TLS
//! configuration, an address that does not bind, and cleartext off loopback each leave the daemon serving everything
//! else, logged, as a Shards route that cannot bind does.
//!
//! **Who** ([`RecordedCredentials`]). A request's token, Basic's password or a bearer token, proves the principal the
//! credential record holds under its hash (`crate::credentials`): looked up on the partition the hash routes to, held as
//! an HTTP token (an SSH key's hash proves no one here), live on the wall clock, and, for an enrolled consumer's, the
//! consumer not revoked (§4.13: "every later effect refuses"), asked of the consumer's own partition. A bearer token's
//! expiry goes with its principal, for LFS's actions (A-629); Basic's transfers authenticate as their batch did, so none
//! goes with Basic.
//!
//! **Where** ([`ShardRepositories`]). A repository's name is located on the partition it routes to
//! (`verbs::owner_of_name`, as `CreateRepository` routes it): a name another shard holds moves the connection there
//! (`Answer::Move`), so a repository is found, judged and served on its own shard, its partition read with no crossing.
//!
//! **What is served.** git's services as the server has them, each from its own half: upload-pack from each
//! repository's store on its owner shard, lent from A-662's map a step at a time with the refs the principal may read
//! (`crate::vcs_store::upload::ShardStores`, A-682), each request admitted the edge's room; receive-pack through the
//! push driver on the shard the connection moved to, its state lent for each step (A-654 parts 2 and 3). Git LFS's
//! locks API ([`ShardLfs`], A-629 part 2c), answered on the repository's owner shard over its locks log
//! (`crate::lfs::answer`), which is durable as the partition's log is; the batch API and the transfers answer `503` at
//! their head until the owner shard keeps LFS objects durably (A-581), so no upload is acknowledged that a restart
//! would forget. Mercurial's commands, a request whose query names one ([`ToolSites`]), from each repository's served
//! view over its store on the shard the connection moved to, lent for each step (`ServedViews`, A-680 part 2); its
//! pushes (`unbundle`) are not served yet.

use slates_db::catalog::{CredentialKind, Principal, RepositoryRecord};
use slates_git::lfs::serve::{Answer as LfsAnswer, Endpoint};
use slates_http::reply::Status;
use slates_http_edge::credential::Credential;
use slates_rt::tcp::{SocketAddrV4, TcpListener};
use slates_rt::{RtError, Runtime, ShardId, futures};
use slates_vfs::clock::Clock;

use super::git::{
  Authenticator, GitSite, Halves, LfsService, Located, Proven, ReceiveServices, Repositories,
  ThisShard, Unproven, UploadServices,
};
use super::hg::HgSite;
use super::{Bounds, EdgeState, ToolSites, admits_cleartext, keep_here, serve};
use crate::config::DaemonConfig;
use crate::lfs::{LfsCall, NoObjects};
use crate::vcs_store::hg_view::ServedViews;
use crate::vcs_store::upload::ShardStores;

/// Counter: an edge whose listener's task the control shard's arena refused.
const NOT_STARTED: &str = "vcs_http.not_started";
/// Counter: a shard that could not keep its edge state, so a connection that moves there is lost.
const NOT_KEPT: &str = "vcs_http.not_kept";

/// Proves a request's token by the credential record (the module doc).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RecordedCredentials;

impl Authenticator for RecordedCredentials {
  async fn prove(&self, credential: &Credential) -> Result<Proven, Unproven> {
    let (token, bearer) = match credential {
      Credential::Basic { password, .. } => (password.as_slice(), false),
      Credential::Bearer(token) => (token.as_slice(), true),
    };
    let record = crate::credentials::find(crate::credentials::token_id(token))
      .await
      .map_err(|_| Unproven::Unanswered)?
      .ok_or(Unproven::Unknown)?;
    if !matches!(record.kind, CredentialKind::HttpToken) {
      return Err(Unproven::NotAToken);
    }
    let now_ns = crate::state::with_state(|state| state.clock.wall_ns())
      .and_then(|ns| u64::try_from(ns).ok())
      .ok_or(Unproven::Unanswered)?;
    if !record.live_at(now_ns) {
      return Err(Unproven::Expired);
    }
    if let Principal::Consumer { consumer, .. } = &record.principal
      && crate::credentials::consumer_revoked(*consumer)
        .await
        .map_err(|_| Unproven::Unanswered)?
    {
      return Err(Unproven::Revoked);
    }
    Ok(Proven {
      principal: record.principal,
      bearer_expires_ns: if bearer { record.expires_ns } else { None },
    })
  }
}

/// Locates and finds repositories on the partitions their names route to (the module doc).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ShardRepositories;

impl Repositories for ShardRepositories {
  fn locate(&self, name: &str) -> Located {
    crate::state::with_state(|state| {
      let partition = crate::verbs::owner_of_name(name, state.shards.len());
      match crate::verbs::shard_of_partition(state, partition) {
        Some(shard) if shard != state.shard => Located::Elsewhere(shard),
        // The partition is this shard's, or names none here, where the name finds nothing: `404`.
        _ => Located::Here,
      }
    })
    .unwrap_or(Located::Here)
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    crate::state::with_state(|state| {
      state
        .db
        .partition()
        .repositories()
        .repository_named(name)
        .cloned()
    })
    .flatten()
  }
}

/// Git LFS as the daemon serves it, on the shard its connection reached the repository's owner on (the module doc): the
/// locks API over the repository's locks log; the batch API and the transfers not until the owner shard keeps LFS
/// objects durably (A-581).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ShardLfs;

impl LfsService for ShardLfs {
  fn serves(&self, endpoint: &Endpoint) -> bool {
    matches!(
      endpoint,
      Endpoint::CreateLock | Endpoint::ListLocks | Endpoint::VerifyLocks | Endpoint::Unlock { .. }
    )
  }

  fn answer(&mut self, call: &LfsCall<'_>) -> LfsAnswer {
    // A borrow of the shard's state refused is counted where it is refused; the request fails as the server's.
    crate::state::with_state_counted(|state| crate::lfs::answer(state, &mut NoObjects, call))
      .unwrap_or_else(|| crate::lfs::bare(Status::INTERNAL_SERVER_ERROR))
  }
}

/// git's services as the daemon serves them (the module doc, "What is served").
pub(crate) type DaemonServices = Halves<UploadServices<ShardStores>, ReceiveServices<ThisShard>>;

/// git's site as the daemon serves it, Git LFS's among its routes (the module doc, "What is served").
pub(crate) type DaemonGitSite =
  GitSite<RecordedCredentials, ShardRepositories, DaemonServices, ShardLfs>;

/// Mercurial's site as the daemon serves it (the module doc, "What is served").
pub(crate) type DaemonHgSite = HgSite<RecordedCredentials, ShardRepositories, ServedViews>;

/// The daemon's sites on its edge: each request to its tool's (the module doc, "What is served").
pub(crate) type DaemonSite = ToolSites<DaemonGitSite, DaemonHgSite>;

/// What makes the daemon's sites on any shard: their challenges name `realm`, each request's route and exchange are
/// charged to the request's `room`, and each git service admits each request that room.
fn site(realm: String, room: u64) -> impl Fn() -> DaemonSite + Clone + Send + 'static {
  move || {
    let git = GitSite::new(
      realm.clone(),
      room,
      RecordedCredentials,
      ShardRepositories,
      Halves::new(
        UploadServices::new(ShardStores, room),
        ReceiveServices::new(ThisShard, room),
      ),
    )
    .with_lfs(ShardLfs);
    let hg = HgSite::new(
      realm.clone(),
      room,
      RecordedCredentials,
      ShardRepositories,
      ServedViews,
    );
    ToolSites::new(git, hg, room)
  }
}

/// Starts the HTTP edge `config` names, if it names one (the module doc): every shard of `shards` keeps its edge state
/// and the `control` shard serves the listener. The port the edge listens on; none when no edge is asked for or it did
/// not start, which is logged.
pub(crate) fn start(
  runtime: &Runtime,
  control: ShardId,
  shards: &[ShardId],
  config: &DaemonConfig,
) -> Result<Option<u16>, RtError> {
  let Some(edge) = &config.vcs_http else {
    return Ok(None);
  };
  let bounds = bounds_of(config);
  let identity = match edge
    .identity
    .as_ref()
    .map(|identity| identity.serving())
    .transpose()
  {
    Ok(identity) => identity,
    Err(refused) => {
      return Ok(not_started(&format!(
        "its identity makes no TLS configuration: {refused:?}"
      )));
    }
  };
  if identity.is_none() && !admits_cleartext(edge.address) {
    return Ok(not_started(
      "it has no identity, and serves cleartext on a loopback address alone",
    ));
  }
  let listener = match crate::daemon::held_or_bound_listener(
    slates_anchor::ENV_VCS_HTTP,
    edge.address,
    bounds.backlog(),
  ) {
    Ok(listener) => listener,
    Err(why) => return Ok(not_started(&why)),
  };
  let port = match listener.local_addr() {
    Ok(address) => address.port(),
    Err(error) => return Ok(not_started(&format!("its address is unread: {error}"))),
  };
  for shard in shards.iter().copied().filter(|shard| *shard != control) {
    runtime.spawn_on(shard, async move {
      if keep_here(EdgeState::new(bounds, None)).is_err() {
        crate::fleet::count_refusal(NOT_KEPT);
      }
    })?;
  }
  let make = site(format!("slates {}", config.instance), bounds.room);
  runtime.spawn_on(control, async move {
    let Ok(state) = keep_here(EdgeState::new(bounds, identity)) else {
      crate::fleet::count_refusal(NOT_KEPT);
      return;
    };
    let served = futures::spawn(async move {
      if let Err(refused) = serve(listener, state, make).await {
        slates_machine::log_line!("slates-server: the HTTP edge stopped: {refused}");
      }
    });
    match served {
      Ok(task) => {
        let _ = futures::detach(task);
      }
      Err(_) => {
        crate::fleet::count_refusal(NOT_STARTED);
      }
    }
  })?;
  Ok(Some(port))
}

/// Logs why the edge did not start: no port to report. Logged, not counted: the shards' counters are not yet serving.
fn not_started(why: &str) -> Option<u16> {
  slates_machine::log_line!("slates-server: the version-control HTTP edge did not start: {why}");
  None
}

/// The edge's bounds on each shard of `config`: its seats and request room from the shard's budget, its waits floored by
/// the timer's tick ([`Bounds::of_shard`]); the SSH edge's too (A-684), whose connections are clients of the shard alike.
pub(crate) fn bounds_of(config: &DaemonConfig) -> Bounds {
  Bounds::of_shard(
    config.clients_per_shard,
    config.region.total_bytes(),
    config.runtime.timer_tick_ns,
  )
}

/// Binds the edge's listener at `address`, its backlog the edge's seats on a shard of `config`: the daemon's own, and the
/// one a supervising anchor binds once and holds for every daemon it starts
/// (`crate::daemon::bind_vcs_http_listener`), so both queue alike.
pub(crate) fn bind(config: &DaemonConfig, address: SocketAddrV4) -> Result<TcpListener, RtError> {
  TcpListener::bind(address, bounds_of(config).backlog())
}

#[cfg(test)]
#[path = "tests/daemon_edge.rs"]
mod tests;
