//! The daemon's SSH edge (A-684; §4.17.2 "Transports" item 2; A-651's owed start): started beside the HTTP edge, its
//! listener and its connections on the control shard, its host key the fleet node's own or the one its anchor keeps.
//!
//! **The start** ([`start`]). With an address in `DaemonConfig::vcs_ssh`, the control shard keeps the edge's state, its
//! seats and channel window derived from the shard's budget as the HTTP edge's are (`Bounds::of_shard`), and serves the
//! listener ([`serve`]): the one a supervising anchor holds and handed over (`slates_anchor::ENV_VCS_SSH`), adopted
//! where the configuration says, else one bound here (`crate::daemon::edge_listener`). The port and the host key's
//! fingerprint are the daemon's to report (`crate::daemon::VcsSshEdge`): its log names both before it says it serves.
//!
//! **The host key** ([`host_seed`]; main's ruling, 2026-11-19). A fleet node's edge presents the node's own key, from
//! the identity its plan hands in, as the HTTP edge's TLS presents the node's certificate: so its key is the node's
//! across every restart and every start. It is that key when the key is Ed25519, the one host key algorithm the edge has
//! (`ssh-ed25519`, RFC 8709; §4.17.2); a node whose key is of another algorithm has no key the edge can present, and its
//! edge does not start, logged, rather than present a key that is not the node's. Any other daemon serves with the key
//! its anchor made at the anchor's start and published in the segment (`AnchorSegment::ssh_host_seed`), so a restart
//! keeps the key a client pinned in its `known_hosts`, beside the port the anchor holds; a daemon run alone, which no
//! anchor outlives, makes its own from the system's generator for its own life, as it binds its own listener.
//!
//! **What it serves** (`super::site::ToolSite`). Keys proved by the credential record a person enrolls (A-651 part 3,
//! `super::site::RecordKeys`); repositories found on the partition of the listener's shard (`ShardRepositories::find`;
//! one another shard holds is owed its move, as the HTTP edge moves a connection); and the services as the server has
//! them: none yet, each `exec` refused as not served (`super::site::NotYet`) until the stores' lender (A-682) and
//! receive-pack's call (A-683) are wired in.
//!
//! Each reason the edge cannot start (no host key, an address that does not bind, a listener held elsewhere) leaves the
//! daemon serving everything else, logged, as the HTTP edge's does.

use slates_anchor::AnchorSegment;
use slates_rt::tcp::{SocketAddrV4, TcpListener};
use slates_rt::{RtError, Runtime, ShardId, futures};
use slates_ssh::entropy::{Entropy, SystemEntropy};
use slates_ssh::hostkey::{ED25519_SEED_LEN, HostKey};

use super::site::{NotYet, RecordKeys, ToolSite};
use super::{EdgeState, HostSeed, serve};
use crate::config::{DaemonConfig, VcsSshConfig};
use crate::daemon::VcsSshEdge;
use crate::vcs_http::ShardRepositories;

/// Counter: an edge whose listener's task the control shard's arena refused.
const NOT_STARTED: &str = "vcs_ssh.not_started";
/// Counter: an edge whose state the control shard could not keep.
const NOT_KEPT: &str = "vcs_ssh.not_kept";

/// The SSH site as the daemon serves it (the module doc, "What it serves").
pub(crate) type DaemonSite = ToolSite<RecordKeys, ShardRepositories, NotYet>;

/// Starts the SSH edge `config` names, if it names one (the module doc): the `control` shard keeps its state and serves
/// its listener, its host key read from `segment` where the anchor published one. The edge's port and host key's
/// fingerprint; none when no edge is asked for or it did not start, which is logged.
pub(crate) fn start(
  runtime: &Runtime,
  control: ShardId,
  config: &DaemonConfig,
  segment: &AnchorSegment,
) -> Result<Option<VcsSshEdge>, RtError> {
  let Some(edge) = &config.vcs_ssh else {
    return Ok(None);
  };
  let published = match segment.ssh_host_seed() {
    Ok(seed) => seed.map(HostSeed),
    Err(error) => {
      return Ok(not_started(&format!(
        "the anchor's host key is unread: {error}"
      )));
    }
  };
  let seed = match host_seed(edge, published, &mut SystemEntropy::new()) {
    Ok(seed) => seed,
    Err(why) => return Ok(not_started(&why)),
  };
  let Some(fingerprint) = fingerprint_of(&seed) else {
    return Ok(not_started("its host key's seed makes no Ed25519 key"));
  };
  let bounds = crate::vcs_http::bounds_of(config);
  let listener = match crate::daemon::edge_listener(
    slates_anchor::ENV_VCS_SSH,
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
  runtime.spawn_on(control, async move {
    let kept =
      slates_rt::registry::with_current(|context| context.keep(EdgeState::new(bounds, seed.0)));
    // The seed's copy here is wiped as it drops: the edge's state holds its own, wiped with it.
    drop(seed);
    let Some(Ok(state)) = kept else {
      crate::fleet::count_refusal(NOT_KEPT);
      return;
    };
    let served = futures::spawn(async move {
      if let Err(refused) = serve(listener, state, || {
        DaemonSite::new(RecordKeys, ShardRepositories, NotYet)
      })
      .await
      {
        slates_machine::log_line!("slates-server: the SSH edge stopped: {refused}");
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
  Ok(Some(VcsSshEdge { port, fingerprint }))
}

/// The edge's host key's seed (the module doc, "The host key"): the fleet node's own key, when the configuration hands
/// in its identity; else the one the anchor published (`published`); else one drawn from `entropy` for this daemon
/// alone.
/// Why there is none: a node's key of another algorithm than Ed25519, or a generator that refused.
fn host_seed(
  edge: &VcsSshConfig,
  published: Option<HostSeed>,
  entropy: &mut impl Entropy,
) -> Result<HostSeed, String> {
  if let Some(identity) = &edge.identity {
    return identity.ed25519_seed().map(HostSeed).ok_or_else(|| {
      "the node's key is not an Ed25519 key, the one host key algorithm the edge presents (ssh-ed25519, RFC 8709)"
        .to_owned()
    });
  }
  if let Some(seed) = published {
    return Ok(seed);
  }
  let mut drawn = HostSeed([0; ED25519_SEED_LEN]);
  entropy
    .fill(&mut drawn.0)
    .map_err(|_| "the system's generator refused its host key".to_owned())?;
  Ok(drawn)
}

/// The fingerprint of the Ed25519 host key `seed` makes, as OpenSSH prints one; none when it makes no key.
fn fingerprint_of(seed: &HostSeed) -> Option<String> {
  HostKey::ed25519_from_seed(&seed.0)
    .ok()
    .map(|key| slates_ssh::userauth::fingerprint(key.public_blob()))
}

/// Binds the edge's listener at `address`, its backlog the edge's seats on a shard of `config`, as the HTTP edge's: the
/// daemon's own, and the one a supervising anchor binds once and holds for every daemon it starts
/// (`crate::daemon::bind_vcs_ssh_listener`), so both queue alike.
pub(crate) fn bind(config: &DaemonConfig, address: SocketAddrV4) -> Result<TcpListener, RtError> {
  TcpListener::bind(address, crate::vcs_http::bounds_of(config).backlog())
}

/// Logs why the edge did not start: no edge to report. Logged, not counted: the shards' counters are not yet serving.
fn not_started(why: &str) -> Option<VcsSshEdge> {
  slates_machine::log_line!("slates-server: the version-control SSH edge did not start: {why}");
  None
}

#[cfg(test)]
#[path = "tests/daemon_edge.rs"]
mod tests;
