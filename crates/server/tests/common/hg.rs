//! An in-process daemon serving one Mercurial repository to stock `hg` over its cleartext HTTP edge on loopback: a
//! changeset store's owner over its own directory, a two-shard daemon holding it, the repository made by the client's
//! `create_repository`, and an HTTP token for this process's principal its anchor's issuer enrolled, in an hgrc stock
//! `hg` reads (as `ci/interop/hg-matrix.sh` gives it). Each integration test that drives real `hg` starts one
//! ([`serve`]) and skips, saying so, where `hg` is not installed ([`has_hg`]).

use std::process::Command;

use slates_client::RepositorySpec;
use slates_ipc::protocol::{CredentialMaterial, ObjectFormat, Principal, VcsTool};
use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_server::config::VcsHttpConfig;
use slates_server::{Daemon, DaemonConfig};
use slates_store::owner::StoreOwner;
use slates_store::store::Sizing;

use super::anchor::{Anchor, held_anchor, source_of};
use super::client::{channel_account, connect};
use super::target::{TargetDir, target_dir};

/// Shape: the daemon's shards.
const SHARDS: u16 = 2;
/// Shape: the store's sizing, a test's (as `slates_server`'s own store tests size theirs).
const SIZING: Sizing = Sizing {
  memory: 8 << 20,
  disk: 1 << 30,
  extent_bytes: 128 << 10,
  fanout: 8,
};
/// Shape: the store owner's waiting requests.
const WAITING: usize = 64;

/// Whether `hg` runs here: a test that needs it skips, saying so, where it does not.
pub(crate) fn has_hg() -> bool {
  Command::new("hg").args(["version", "-q"]).output().is_ok()
}

/// A daemon serving the repository `project` to stock `hg` (module docs), until [`Served::stop`].
pub(crate) struct Served {
  /// The daemon, until stopped.
  daemon: Option<Daemon>,
  /// The anchor holding the daemon's segment, until stopped.
  anchor: Option<Anchor>,
  /// The store's owner, until stopped.
  owner: Option<StoreOwner>,
  /// The test's directory: the store's, the hgrc and every repository `hg` makes; removed when it drops.
  scratch: TargetDir,
  /// The repository's URL.
  pub(crate) url: String,
  /// The hgrc every [`Served::hg`] reads.
  hgrc: String,
  /// The daemon's instance name, which a client connects by.
  instance: String,
  /// The token the hgrc authenticates with, and the hgrc's other lines: written again for a restarted edge's port.
  auth: (String, String),
}

impl Served {
  /// The test's directory.
  pub(crate) fn base(&self) -> &str {
    &self.scratch.path
  }

  /// An `hg` command with this test's configuration, run in `dir` (relative to [`Served::base`]).
  pub(crate) fn hg(&self, dir: &str) -> Command {
    let mut command = Command::new("hg");
    command
      .env("HGRCPATH", &self.hgrc)
      .env("HGPLAIN", "1")
      .env("HGUSER", "slates-test")
      .current_dir(format!("{}/{dir}", self.base()));
    command
  }

  /// Sets whether `project` publishes (its policy's `hg_publishing`, 7.2's `phases.publish`), as its owner.
  pub(crate) fn set_publishing(&self, publishing: bool) {
    let mut client = connect(&self.instance);
    let mut policy = client.describe_repository_policy("project").unwrap();
    policy.hg_publishing = publishing;
    client.set_repository_policy("project", &policy).unwrap();
  }

  /// A client of the daemon serving, as the test's principal.
  pub(crate) fn client(&self) -> slates_client::Client {
    connect(&self.instance)
  }

  /// The daemon serving.
  pub(crate) fn daemon(&self) -> &Daemon {
    self
      .daemon
      .as_ref()
      .expect("the daemon serves until stopped")
  }

  /// Restarts the daemon and the store's owner over the same anchor and store directory, as a crash's successor does: the
  /// daemon recovers from its segment and the store, and its edge listens again, on a port of its own, which the URL and
  /// the hgrc name from then on. A test that needs a restart's recovery (A-887's) runs through it.
  pub(crate) fn restart(&mut self) {
    if let Some(daemon) = self.daemon.take() {
      daemon.stop();
    }
    if let Some(owner) = self.owner.take() {
      owner.stop().unwrap();
    }
    let owner = start_owner(self.base());
    let profile = super::machine_profile();
    let config = daemon_config(&profile, &self.instance, &owner);
    let anchor = self
      .anchor
      .as_ref()
      .expect("the anchor holds until stopped");
    let daemon = Daemon::start(&profile, config, source_of(anchor)).unwrap();
    let port = daemon.vcs_http_port().expect("the edge listens");
    self.url = format!("http://127.0.0.1:{port}/project");
    self.scratch.seed("hgrc", hgrc(port, &self.auth).as_bytes());
    self.daemon = Some(daemon);
    self.owner = Some(owner);
  }

  /// Stops the daemon, its anchor and the store's owner, as [`Served::stop`] does, then hands `measure` the test's
  /// directory while it still stands: the store's files as the stopped owner left them.
  pub(crate) fn stop_measuring<T>(mut self, measure: impl FnOnce(&str) -> T) -> T {
    if let Some(daemon) = self.daemon.take() {
      daemon.stop();
    }
    drop(self.anchor.take());
    if let Some(owner) = self.owner.take() {
      owner.stop().unwrap();
    }
    measure(self.base())
  }

  /// Stops the daemon, then its anchor, then the store's owner.
  pub(crate) fn stop(mut self) {
    if let Some(daemon) = self.daemon.take() {
      daemon.stop();
    }
    drop(self.anchor.take());
    if let Some(owner) = self.owner.take() {
      owner.stop().unwrap();
    }
  }
}

/// The store's owner over the store directory under `base`, made the first time.
fn start_owner(base: &str) -> StoreOwner {
  start_owner_sized(base, SIZING)
}

/// [`start_owner`] with the store sized `sizing`.
fn start_owner_sized(base: &str, sizing: Sizing) -> StoreOwner {
  let store_dir = format!("{base}/store");
  // The store's directory inside the fixture's own, in the build output (`common::target`).
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir_all(&store_dir).unwrap();
  let store_dir = std::fs::canonicalize(&store_dir).unwrap();
  let grant = StoreGrant {
    repository: 0,
    target: StoreRoot::identify(&store_dir).unwrap(),
    issued_ns: 0,
  };
  StoreOwner::start(
    StoreRoot::open(&store_dir, &grant).unwrap(),
    sizing,
    WAITING,
  )
  .unwrap()
}

/// The daemon's configuration: [`SHARDS`] shards, its edge on loopback, `owner`'s store its disk.
fn daemon_config(
  profile: &slates_machine::MachineProfile,
  instance: &str,
  owner: &StoreOwner,
) -> DaemonConfig {
  let mut config = DaemonConfig::derive(profile, instance, Some(SHARDS));
  config.vcs_http = Some(VcsHttpConfig {
    address: std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0),
    identity: None,
  });
  config.vcs_disk = Some(owner.handle());
  config
}

/// The hgrc for the edge at `port`: the token's `[auth]`, then the other lines.
fn hgrc(port: u16, (token, extra): &(String, String)) -> String {
  format!(
    "[auth]\nslates.prefix = http://127.0.0.1:{port}/\nslates.username = slates\nslates.password = {token}\n{extra}"
  )
}

/// Starts a daemon named after `name` serving `project` (module docs), its hgrc the token's `[auth]` and `extra`.
pub(crate) fn serve(name: &str, extra: &str) -> Served {
  serve_sized(name, extra, SIZING)
}

/// [`serve`] over a store sized `sizing`: a measurement's, whose history is past the tests' store.
pub(crate) fn serve_sized(name: &str, extra: &str, sizing: Sizing) -> Served {
  let scratch = target_dir();
  let base = scratch.path.clone();
  let owner = start_owner_sized(&base, sizing);
  let profile = super::machine_profile();
  let instance = format!("{name}-{}", std::process::id());
  let config = daemon_config(&profile, &instance, &owner);
  let anchor = held_anchor(&instance, &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&anchor)).unwrap();
  daemon.bootstrap(true).unwrap();
  let port = daemon.vcs_http_port().expect("the edge listens");
  let mut client = connect(&instance);
  client
    .create_repository(&RepositorySpec {
      name: "project".to_owned(),
      tool: VcsTool::Mercurial,
      format: ObjectFormat::Sha1,
      default_branch: "branch:default:0".to_owned(),
      green: None,
    })
    .unwrap();
  let token = format!("{name}-token-{}", std::process::id());
  // The account the daemon authenticated this client's channel as, on every platform: a Unix channel's peer uid, a
  // Windows channel's the daemon's mapping of its token (never `getuid`, which Windows has none of), as the test that
  // enrolls the token will be served as.
  let principal = Principal::Uid {
    host: client.daemon_status().unwrap().anchor,
    uid: channel_account(&daemon, client.client_id()),
  };
  let material = CredentialMaterial::HttpToken {
    id: slates_server::credentials::token_id(token.as_bytes()),
    cram_md5_contexts: slates_server::credentials::cram_md5_contexts(token.as_bytes()),
  };
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::credentials::enroll_proof(&secret, &principal, &material, None);
  client
    .enroll_credential(principal, material, None, proof)
    .unwrap();
  let hgrc_path = format!("{base}/hgrc");
  let auth = (token, extra.to_owned());
  scratch.seed("hgrc", hgrc(port, &auth).as_bytes());
  Served {
    daemon: Some(daemon),
    anchor: Some(anchor),
    owner: Some(owner),
    scratch,
    url: format!("http://127.0.0.1:{port}/project"),
    hgrc: hgrc_path,
    instance,
    auth,
  }
}
