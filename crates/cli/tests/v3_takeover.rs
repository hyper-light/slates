//! A git repository's pushes survive its owner's death across real processes (A-724 piece 8c; main's ruling of
//! 2026-11-20 (ii)): three `slates anchor --fleet` processes at `f = 1`, each with its version-control HTTP edge on
//! loopback; stock git pushes commits to a repository on one node; that node's anchor and daemon are killed as a crash
//! kills them, one push in flight; a survivor takes the repository over and serves it. What it holds: every push the
//! client was told succeeded is readable there, its names resolving (NoLoss); a push not acknowledged is whole or
//! absent, never half applied (StaleNeverCommits); the pushes in their order (TotalOrder). With version 3 records, the
//! names a record holds by identity are homed at `f + 1` in the region before it commits, so the successor resolves them
//! from a surviving home. Test by use (R5): the CLI, real daemons and stock git. Gated like the other process flows
//! (`SLATES_TEST_CLI=1`); skips loudly without stock git.
//!
//! The fleet helpers are copies of `live_fleet.rs`'s (private there), cut to what this flow needs.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]
#![cfg(unix)]

use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long a fleet step may take on a loaded box: the formation wait of the process flows.
const FLEET_WAIT: Duration = Duration::from_secs(60);
/// Shape: the pause between polls.
const POLL_MS: u64 = 50;
/// Shape: shards per daemon.
const SHARDS: &str = "2";
/// Shape: the fleet's TLS name.
const FLEET_NAME: &str = "slates-fleet";
/// Shape: the nodes, in manifest order.
const FLEET_NODES: [&str; 3] = ["a", "b", "c"];
/// Shape: three nodes at `f = 1`.
const FLEET_F: u32 = 1;
/// Shape: the ports a node serves on: its base and the next.
const PORTS_PER_NODE: u16 = 2;
/// Shape: the port range searched for each node's block, from a pid-derived start.
const PORT_FLOOR: u16 = 20_000;
/// Shape: see [`PORT_FLOOR`].
const PORT_SPAN: u16 = 30_000;
/// Shape: candidate bases tried per block.
const PORT_ATTEMPTS: u16 = 2_000;
/// Shape: the acknowledged pushes before the owner dies.
const PUSHES: usize = 3;
/// Shape: the repository's name.
const REPOSITORY: &str = "survives";
/// Format: the daemon's line naming its instance serving.
const SERVING_LINE: &str = "slates daemon: instance ";
/// Format: the anchor's line naming its issuer surface.
const ISSUER_SURFACE_LINE: &str = "slates anchor: issuer surface: export ";
/// Format: the daemon's line naming where its HTTP edge serves; a fleet node's serves HTTPS under the node's own
/// identity.
const VCS_HTTP_LINE: &str =
  "slates daemon: the version-control HTTP edge serves HTTPS on 127.0.0.1:";
/// Format: the anchor's line naming its daemon's pid.
const DAEMON_PID: &str = "daemon pid ";

/// One node: its anchor (its daemon a child of it), the daemon's pid, its HTTP edge's port and its issuer surface.
struct Node {
  instance: String,
  anchor: Child,
  daemon_pid: Option<u32>,
  port: u16,
  issuer: Vec<(String, String)>,
}

impl Drop for Node {
  fn drop(&mut self) {
    kill_node(self);
  }
}

/// Kills `node` as a crash kills it: its anchor and its daemon, SIGKILL.
fn kill_node(node: &mut Node) {
  let _ = node.anchor.kill();
  let _ = node.anchor.wait();
  if let Some(pid) = node.daemon_pid.take()
    && let Some(pid) = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap_or(0))
  {
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
  }
}

struct ScratchDir {
  path: String,
}

impl Drop for ScratchDir {
  fn drop(&mut self) {
    let _ = Command::new("rm").args(["-rf", &self.path]).output();
  }
}

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

fn slates(instance: &str, issuer: &[(String, String)], args: &[&str]) -> (i32, String, String) {
  let output = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args(["--instance", instance])
    .args(args)
    .envs(issuer.iter().map(|(name, value)| (name, value)))
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn value_of(text: &str, key: &str) -> Option<String> {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .map(str::to_owned)
}

fn scratch_dir(tag: &str) -> ScratchDir {
  let path = format!(
    "{}/slates-v3-takeover-{tag}-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  assert!(
    Command::new("mkdir")
      .args(["-p", &path])
      .output()
      .unwrap()
      .status
      .success()
  );
  ScratchDir { path }
}

#[allow(clippy::disallowed_methods)]
fn mint_identity(dir: &str, node: &str) {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![FLEET_NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  std::fs::write(format!("{dir}/{node}.crt.der"), cert.der().as_ref()).unwrap();
  std::fs::write(format!("{dir}/{node}.key.der"), key.serialize_der()).unwrap();
}

#[allow(clippy::disallowed_methods)]
fn write_manifest(dir: &str, bases: &[u16]) -> String {
  let nodes: Vec<serde_json::Value> = FLEET_NODES
    .iter()
    .zip(bases)
    .map(|(node, base)| {
      serde_json::json!({
        "node": node,
        "address": format!("127.0.0.1:{base}"),
        "certificate": format!("{node}.crt.der"),
        "key": format!("{node}.key.der"),
      })
    })
    .collect();
  let manifest = serde_json::json!({ "name": FLEET_NAME, "f": FLEET_F, "nodes": nodes });
  let path = format!("{dir}/fleet.json");
  std::fs::write(&path, manifest.to_string()).unwrap();
  path
}

struct HeldBlock {
  base: u16,
  sockets: Vec<std::net::UdpSocket>,
}

fn port_blocks() -> Vec<HeldBlock> {
  let offset = (std::process::id().wrapping_mul(7)) % u32::from(PORT_SPAN);
  let mut from = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  let mut blocks = Vec::new();
  for _ in FLEET_NODES {
    let mut base = from;
    let held = (0..PORT_ATTEMPTS)
      .find_map(|_| {
        let sockets: Option<Vec<std::net::UdpSocket>> = (0..PORTS_PER_NODE)
          .map(|k| {
            base
              .checked_add(k)
              .and_then(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).ok())
          })
          .collect();
        let found = sockets.map(|sockets| HeldBlock { base, sockets });
        base = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
        found
      })
      .expect("a block of free loopback UDP ports");
    from = held.base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
    blocks.push(held);
  }
  blocks
}

fn inheritable(socket: &std::net::UdpSocket) -> std::os::fd::OwnedFd {
  let duplicate: std::os::fd::OwnedFd = socket.try_clone().unwrap().into();
  rustix::io::fcntl_setfd(&duplicate, rustix::io::FdFlags::empty()).unwrap();
  duplicate
}

/// Starts node `node` as an anchor of the fleet in `manifest` with its HTTP edge on loopback, and reads its lines
/// until its daemon serves and its edge's port and issuer surface are named (echoing each to standard error).
fn start_node(instance: &str, manifest: &str, node: &str, held: &HeldBlock) -> Node {
  use std::os::fd::AsRawFd;
  let [probe, record] = held.sockets.as_slice() else {
    panic!("a node's block is its probe and record ports");
  };
  let (probe, record) = (inheritable(probe), inheritable(record));
  let mut anchor = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
      "--fleet",
      manifest,
      "--node",
      node,
      "--vcs-http",
      "127.0.0.1:0",
    ])
    .env(
      slates_anchor::ENV_FLEET_SERVE,
      format!("{},{}", probe.as_raw_fd(), record.as_raw_fd()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  // The anchor's lines read on a thread, so a line that never comes ends the test at the fleet wait, never hangs it.
  let stderr = anchor.stderr.take().unwrap();
  let (lines, read) = std::sync::mpsc::channel::<String>();
  std::thread::spawn(move || {
    let mut stderr = std::io::BufReader::new(stderr);
    loop {
      let mut line = String::new();
      if stderr.read_line(&mut line).unwrap_or(0) == 0 {
        return;
      }
      let _ = std::io::stderr().write_all(line.as_bytes());
      if lines.send(line).is_err() {
        // The node is started: its lines still go to standard error.
        let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
        return;
      }
    }
  });
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (mut served, mut port, mut issuer, mut daemon_pid) = (false, None, None, None);
  let deadline = Instant::now() + FLEET_WAIT;
  while !served || port.is_none() || issuer.is_none() {
    let left = deadline.saturating_duration_since(Instant::now());
    let line = read.recv_timeout(left).unwrap_or_else(|_| {
      panic!("node {node} did not serve with its HTTP edge and issuer surface named")
    });
    served |= line.starts_with(&serving);
    if let Some(rest) = line.strip_prefix(VCS_HTTP_LINE) {
      port = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok());
    }
    if let Some(pairs) = line.trim().strip_prefix(ISSUER_SURFACE_LINE) {
      issuer = Some(
        pairs
          .split(' ')
          .filter_map(|pair| pair.split_once('='))
          .map(|(name, value)| (name.to_owned(), value.to_owned()))
          .collect::<Vec<_>>(),
      );
    }
    if let Some((_, rest)) = line.split_once(DAEMON_PID) {
      daemon_pid = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok());
    }
  }
  drop(read);
  Node {
    instance: instance.to_owned(),
    anchor,
    daemon_pid,
    port: port.unwrap(),
    issuer: issuer.unwrap(),
  }
}

/// Waits until every node holds all three members with both peers probed: each node's member id.
fn wait_formed(nodes: &[Node]) -> Vec<u64> {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let views: Vec<Option<(u64, usize, u32)>> = nodes
      .iter()
      .map(|node| {
        let (code, out, _) = slates(&node.instance, &[], &["status"]);
        (code == 0).then_some(())?;
        Some((
          value_of(&out, "fleet_host")?.parse().ok()?,
          value_of(&out, "fleet_members")?.split_whitespace().count(),
          value_of(&out, "fleet_peers_probed")?.parse().ok()?,
        ))
      })
      .collect();
    if views
      .iter()
      .all(|view| view.is_some_and(|(_, members, probed)| members == 3 && probed == 2))
    {
      return views.into_iter().map(|view| view.unwrap().0).collect();
    }
    assert!(Instant::now() < deadline, "the fleet formed: {views:?}");
    pause();
  }
}

/// Runs `args` on `node` until it answers (a fresh learner refuses `ConsensusNotInitialized` a while), within the fleet
/// wait: its output.
fn run_ready(node: &Node, args: &[&str]) -> String {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let (code, out, err) = slates(&node.instance, &node.issuer, args);
    if code == 0 {
      return out;
    }
    assert!(
      Instant::now() < deadline,
      "{args:?} on {}: {out}{err}",
      node.instance
    );
    pause();
  }
}

/// Stock git, if this host has it.
fn stock_git() -> Option<String> {
  let found = Command::new("git").arg("--version").output().ok()?;
  found.status.success().then(|| "git".to_owned())
}

/// Runs stock git with `args` in `dir`, a bearer `token` on every request: its exit code and both outputs.
fn git(dir: &str, token: &str, args: &[&str]) -> (i32, String, String) {
  let output = Command::new("git")
    .args([
      "-c",
      &format!("http.extraHeader=Authorization: Bearer {token}"),
      "-c",
      "user.name=t",
      "-c",
      "user.email=t@t",
    ])
    .args([
      "-c",
      "commit.gpgsign=false",
      "-c",
      "protocol.version=0",
      "-c",
      "http.sslVerify=false",
    ])
    .args(args)
    .current_dir(dir)
    .env("GIT_TERMINAL_PROMPT", "0")
    .env_remove("GIT_DIR")
    .env_remove("GIT_WORK_TREE")
    .stdin(Stdio::null())
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// A commit on `dir`'s `main`, a file of its own, whose id is answered.
fn commit(dir: &str, token: &str, n: usize) -> String {
  #[allow(clippy::disallowed_methods)]
  std::fs::write(
    format!("{dir}/file-{n}.txt"),
    format!("push {n}\n").repeat(64),
  )
  .unwrap();
  assert_eq!(git(dir, token, &["add", "."]).0, 0);
  assert_eq!(
    git(dir, token, &["commit", "-qm", &format!("push {n}")]).0,
    0
  );
  git(dir, token, &["rev-parse", "HEAD"]).1.trim().to_owned()
}

/// The commit `main` names on the repository at `url`, if it answers one.
fn remote_main(dir: &str, token: &str, url: &str) -> Option<String> {
  let (code, out, _) = git(dir, token, &["ls-remote", url, "refs/heads/main"]);
  (code == 0).then_some(())?;
  out.split_whitespace().next().map(str::to_owned)
}

/// The repository's URL at `node`'s edge.
fn repository_url(node: &Node) -> String {
  format!("https://127.0.0.1:{}/{REPOSITORY}", node.port)
}

/// Every principal may read and push the repository's refs on `node` (the takeover test's workaround, there).
fn grant_every_principal_refs(node: &Node) {
  use slates_ipc::protocol::{PolicyEffect, PolicyNames, PolicyRight, PolicyRule, PolicyWho};
  let mut client = slates_client::Client::connect(&node.instance, slates_api::deadlines()).unwrap();
  let mut policy = client.describe_repository_policy(REPOSITORY).unwrap();
  for right in [PolicyRight::Read, PolicyRight::Submit] {
    policy.rules.push(PolicyRule {
      names: PolicyNames::Refs,
      pattern: "refs/".to_owned(),
      right,
      effect: PolicyEffect::Allow,
      who: PolicyWho::Anyone,
    });
  }
  client.set_repository_policy(REPOSITORY, &policy).unwrap();
}

/// [`PUSHES`] commits made in `work` and pushed to `url` one by one, each acknowledged: their ids, in push order.
fn pushed_acknowledged(work: &str, token: &str, url: &str) -> Vec<String> {
  let mut acknowledged = Vec::new();
  for n in 0..PUSHES {
    let id = commit(work, token, n);
    let (code, out, err) = git(work, token, &["push", "-q", url, "main"]);
    assert_eq!(code, 0, "push {n}: {out}{err}");
    acknowledged.push(id);
    eprintln!("V3TAKEOVER: push {n} acknowledged");
  }
  acknowledged
}

/// The first of `survivors` whose edge answers `main` for the repository, asked with its own credential (the owner's is
/// gone with it): its place, `main` there and the token.
fn survivor_serving(
  nodes: &[Node],
  survivors: &[usize],
  work: &str,
) -> Option<(usize, String, String)> {
  survivors.iter().find_map(|at| {
    let node = &nodes[*at];
    let token = value_of(
      &slates(&node.instance, &node.issuer, &["credential", "token"]).1,
      "token",
    )?;
    remote_main(work, &token, &repository_url(node)).map(|head| (*at, head, token))
  })
}

/// What each of `survivors` answers, and what its takeover counted, so a failure names its cause.
fn survivors_seen(nodes: &[Node], survivors: &[usize], work: &str) -> Vec<String> {
  survivors
    .iter()
    .map(|at| {
      let node = &nodes[*at];
      let own = value_of(
        &slates(&node.instance, &node.issuer, &["credential", "token"]).1,
        "token",
      )
      .unwrap_or_default();
      let (code, out, err) = git(work, &own, &["ls-remote", &repository_url(node)]);
      let (_, status, _) = slates(&node.instance, &node.issuer, &["status"]);
      let counted: Vec<&str> = status
        .lines()
        .filter(|line| {
          [
            "takeover",
            "vcs_store",
            "repositor",
            "state_log",
            "part",
            "held",
            "record",
          ]
          .iter()
          .any(|word| line.contains(word))
        })
        .collect();
      format!(
        "node {at}: ls-remote {code} {}{}; counted {counted:?}",
        out.trim(),
        err.trim()
      )
    })
    .collect()
}

/// The survivor that serves the repository once the owner is gone, waited for under [`FLEET_WAIT`] three times over: its
/// place, `main` there and its own token. Fails naming what each survivor answered.
fn successor_after(nodes: &[Node], survivors: &[usize], work: &str) -> (usize, String, String) {
  let deadline = Instant::now() + FLEET_WAIT * 3;
  loop {
    // A survivor's own credential (the owner's is gone with it), the repository's policy letting every principal in.
    let found = survivor_serving(nodes, survivors, work);
    if let Some((at, head, token)) = found {
      return (at, head, token);
    }
    if Instant::now() >= deadline {
      // What each survivor answers, and what its takeover counted, so a failure names its cause.
      let seen = survivors_seen(nodes, survivors, work);
      panic!("no survivor served the repository: {seen:#?}");
    }
    pause();
  }
}

/// A clone from `url` into `scratch` is whole by `git fsck`, and every one of `acknowledged` is on its `main`, in push
/// order (StaleNeverCommits, NoLoss, TotalOrder).
fn clone_whole_in_order(scratch: &str, token: &str, url: &str, acknowledged: &[String]) {
  let clone = format!("{}/clone", scratch);
  let (code, out, err) = git(scratch, token, &["clone", "-q", url, &clone]);
  assert_eq!(code, 0, "the clone from the successor: {out}{err}");
  let (code, out, err) = git(&clone, token, &["fsck", "--full", "--strict"]);
  assert_eq!(
    code, 0,
    "the successor's repository whole (StaleNeverCommits): {out}{err}"
  );
  let (_, log, _) = git(&clone, token, &["rev-list", "--reverse", "main"]);
  let order: Vec<String> = log.lines().map(str::to_owned).collect();
  let positions: Vec<Option<usize>> = acknowledged
    .iter()
    .map(|id| order.iter().position(|seen| seen == id))
    .collect();
  assert!(
    positions.iter().all(Option::is_some),
    "an acknowledged push missing (NoLoss): {positions:?}"
  );
  assert!(
    positions.windows(2).all(|pair| pair[0] < pair[1]),
    "pushes out of order (TotalOrder): {positions:?}"
  );
}

/// A-724 piece 8c (main's ruling (ii) and its takeover test). Do: three anchors at `f = 1` with their HTTP edges;
/// create a git repository on one node and push [`PUSHES`] commits to it with stock git, each acknowledged; start one
/// more push and kill the owner's anchor and daemon (SIGKILL) while it runs; wait for a survivor to serve the
/// repository and clone it from there. Expect: `main` there at the last acknowledged commit or at the in-flight one,
/// never elsewhere; every acknowledged commit reachable, in push order (NoLoss, TotalOrder); the clone whole by
/// `git fsck` (no object a record named missing: StaleNeverCommits).
#[test]
fn a_repositorys_acknowledged_pushes_survive_its_owners_death_whole_and_in_order() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the repository takeover flow: set SLATES_TEST_CLI=1 to run it (three anchors)"
    );
    return;
  }
  if stock_git().is_none() {
    eprintln!("skipping the repository takeover flow: no stock git here");
    return;
  }
  let scratch = scratch_dir("flow");
  for node in FLEET_NODES {
    mint_identity(&scratch.path, node);
  }
  let blocks = port_blocks();
  let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
  let manifest = write_manifest(&scratch.path, &bases);
  let pid = std::process::id();
  let mut nodes: Vec<Node> = FLEET_NODES
    .iter()
    .zip(&blocks)
    .map(|(node, held)| start_node(&format!("v3-takeover-{node}-{pid}"), &manifest, node, held))
    .collect();
  eprintln!("V3TAKEOVER: nodes started");
  let hosts = wait_formed(&nodes);
  eprintln!("V3TAKEOVER: formed {hosts:?}");
  let bootstrap = (0..hosts.len()).min_by_key(|at| hosts[*at]).unwrap();
  run_ready(&nodes[bootstrap], &["bootstrap", "root"]);
  // The repository on another node than the bootstrap's, so its death is not the root council's leader's alone.
  let owner = (bootstrap + 1) % nodes.len();
  run_ready(
    &nodes[owner],
    &["repository", "create", REPOSITORY, "--tool", "git"],
  );
  eprintln!("V3TAKEOVER: repository created on node {owner}");
  // WORKAROUND, until w-fed-faults' fix lands (main's ruling of 2026-11-20): a credential minted on the owner is not
  // known to the successor once the owner is gone (401), and a survivor's own token names its own account (403, A-237).
  // So every principal may read and push the repository's refs, granted before the owner dies, and the client asks the
  // successor with a survivor's own token. Remove this grant, and ask with the owner's token, once credentials survive
  // their node; this test is that fix's red too.
  grant_every_principal_refs(&nodes[owner]);
  let token =
    value_of(&run_ready(&nodes[owner], &["credential", "token"]), "token").expect("a token");
  // A fleet node's edge presents the node's own certificate (self-signed here): git is told not to verify it.
  let url = repository_url;
  let work = format!("{}/work", scratch.path);
  assert!(
    Command::new("mkdir")
      .args(["-p", &work])
      .output()
      .unwrap()
      .status
      .success()
  );
  assert_eq!(git(&work, &token, &["init", "-q", "-b", "main"]).0, 0);
  let acknowledged = pushed_acknowledged(&work, &token, &url(&nodes[owner]));
  // One push in flight as the owner dies.
  let in_flight = commit(&work, &token, PUSHES);
  let owner_url = url(&nodes[owner]);
  let (work_dir, flight_token) = (work.clone(), token.clone());
  let flight = std::thread::spawn(move || {
    git(
      &work_dir,
      &flight_token,
      &["push", "-q", &owner_url, "main"],
    )
    .0
  });
  kill_node(&mut nodes[owner]);
  let flight_code = flight.join().unwrap();
  eprintln!("V3TAKEOVER: owner killed, in-flight push answered {flight_code}");
  let survivors: Vec<usize> = (0..nodes.len()).filter(|at| *at != owner).collect();
  // The successor: the survivor whose edge answers `main` for the repository.
  let (at, head, successor_token) = successor_after(&nodes, &survivors, &work);
  eprintln!("V3TAKEOVER: node {at} serves main at {head}");
  let last = acknowledged.last().cloned().unwrap();
  assert!(
    head == last || head == in_flight,
    "main at {head}: neither the last acknowledged push {last} nor the one in flight {in_flight} (it answered {flight_code})"
  );
  if flight_code == 0 {
    assert_eq!(head, in_flight, "an acknowledged push lost (NoLoss)");
  }
  clone_whole_in_order(
    &scratch.path,
    &successor_token,
    &url(&nodes[at]),
    &acknowledged,
  );
  drop(nodes);
  drop(scratch);
}
