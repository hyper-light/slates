//! The many-client load test at scale, over the topology the goal names (§4.8, §4.10, §4.16, A-201's filesystem
//! server): one primary and two replica filesystem servers, and client nodes in front of them. Three server nodes form
//! region 0 at `f = 1`: the green is created on the first, which owns it (the primary), and its records are held by the
//! other two (the replicas: at `f = 1` a write commits on two of the three). The client nodes form region 1, so they
//! are never among the green's candidate holders. Every verb a client sends reaches the primary as a cross-region
//! forward. One release load harness (`crates/client/examples/load_bench.rs`) drives every client node at once
//! (`--instance a,b,…`, connection `k` to node `k` mod their count), so one oracle judges every submit.
//!
//! At each rung it prints each node daemon's resident memory and CPU time before and after, and the peak it saw while
//! the rung ran, read with `ps`, so it runs on macOS and Linux alike. When asked, it SIGKILLs one server node's daemon
//! partway through each rung, and that node's anchor restarts it: a chaos run per rung.
//!
//! Gated on the environment, skipping loudly (CLAUDE.md §4):
//! - required: `SLATES_LOADTEST_TREE` (the tree the green is seeded from) and `SLATES_LOADTEST_BENCH` (the release
//!   harness binary);
//! - optional: `SLATES_LOADTEST_CLIENTS` (rungs, comma separated, default `100`), `SLATES_LOADTEST_CLIENT_NODES`
//!   (default 1), `SLATES_LOADTEST_ARGS` (further harness arguments), `SLATES_LOADTEST_KILL_MS` and
//!   `SLATES_LOADTEST_KILL_NODE` (kill that server node's daemon, `0` the primary, this long into each rung).
//!
//! Each rung adds a `node` line per node to the harness's own lines. On Linux, raise the open-file limit for the run
//! (`LimitNOFILE` of its unit, or `ulimit -n`): a client node holds descriptors for every connection, and at the
//! default a 1,000-client rung failed at once with "the handoff carried the wrong number of descriptors" (fixtures on
//! adacachy, 2026-11-06).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long each fleet phase gets (to answer, to form, to serve a forwarded verb): the three-node CLI flow's
/// 40 s a phase, doubled for five or more nodes booting on one machine.
const FLEET_WAIT: Duration = Duration::from_secs(80);
/// Shape: the pause between polls.
const POLL_MS: u64 = 50;
/// Shape: the fleet's TLS name, carried by every certificate the test mints.
const FLEET_NAME: &str = "slates-load-scale";
/// Shape: the server nodes, in manifest order; the first is the primary (it creates the green and owns it).
const SERVERS: [&str; 3] = ["primary", "replica-1", "replica-2"];
/// Shape: the fleet's fault tolerance: one primary and two replicas, a write committing on two of three.
const FLEET_F: u32 = 1;
/// Shape: the servers' region and the client nodes' region.
const SERVER_REGION: u64 = 0;
/// Shape: see [`SERVER_REGION`].
const CLIENT_REGION: u64 = 1;
/// Shape: the ports each node serves on (`slates_server::deploy`: one per plane).
const PORTS_PER_NODE: u16 = 2;
/// Shape: the bottom of the port range searched, above the registered ports a shared machine binds.
const PORT_FLOOR: u16 = 20_000;
/// Shape: the range's width; the search starts at a pid-derived offset so concurrent runs start apart.
const PORT_SPAN: u16 = 30_000;
/// Shape: the server overlay's bound over its tree's bytes (the harness's own `OVERLAY_BOUND_FACTOR`).
const OVERLAY_BOUND_FACTOR: u64 = 2;
/// Format: bytes a mebibyte, the unit `volume create --bounded` takes here.
const MIB: u64 = 1 << 20;
/// Format: bytes a kibibyte (`du -k`, `ps -o rss`).
const KIB: u64 = 1 << 10;
/// Format: milliseconds a second and seconds a minute, for `ps -o time`.
const MS_PER_S: u64 = 1000;
/// Format: see [`MS_PER_S`].
const S_PER_MIN: u64 = 60;
/// Format: see [`MS_PER_S`].
const MIN_PER_HOUR: u64 = 60;

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a verb at `instance`: (exit code, stdout, stderr).
fn run(instance: &str, args: &[&str]) -> (i32, String, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn ok(instance: &str, args: &[&str]) -> String {
  let (code, out, err) = run(instance, args);
  assert_eq!(code, 0, "slates {args:?} at {instance}: {out}{err}");
  out
}

fn json(text: &str) -> serde_json::Value {
  serde_json::from_str(text).unwrap()
}

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

/// Retries `verb` until it succeeds or the phase's wait is spent: its stdout.
fn eventually(instance: &str, args: &[&str]) -> String {
  let started = Instant::now();
  loop {
    let (code, out, err) = run(instance, args);
    if code == 0 {
      return out;
    }
    assert!(
      started.elapsed() < FLEET_WAIT,
      "slates {args:?} at {instance} never answered: {code} {out}{err}"
    );
    pause();
  }
}

/// An anchor process, killed and reaped when dropped (its daemon goes with it), so a failed assertion leaves none.
struct Anchor {
  child: Child,
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// A scratch directory in the build output, removed when dropped.
struct Scratch {
  path: String,
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = Command::new("rm").args(["-rf", &self.path]).output();
  }
}

#[allow(clippy::disallowed_methods)]
fn scratch() -> Scratch {
  let raw = format!(
    "{}/load-scale-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  std::fs::create_dir_all(&raw).unwrap();
  Scratch {
    path: std::fs::canonicalize(&raw)
      .unwrap()
      .to_string_lossy()
      .into_owned(),
  }
}

/// A self-signed leaf identity for `node`, written as DER into `dir` (the operator's files the daemon reads).
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

/// A base port whose block of `PORTS_PER_NODE` loopback UDP ports is free now (the anchor binds them itself).
fn free_base(from: u16) -> u16 {
  let mut base = from;
  loop {
    let free = (0..PORTS_PER_NODE).all(|k| {
      base
        .checked_add(k)
        .and_then(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).ok())
        .is_some()
    });
    if free {
      return base;
    }
    base = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
  }
}

/// The manifest: every node on loopback at its base port, the servers in region 0 and the client nodes in region 1,
/// at `f = 1`.
#[allow(clippy::disallowed_methods)]
fn write_manifest(dir: &str, nodes: &[(String, u16, u64)]) -> String {
  let entries: Vec<serde_json::Value> = nodes
    .iter()
    .map(|(node, base, region)| {
      serde_json::json!({
        "node": node,
        "address": format!("127.0.0.1:{base}"),
        "certificate": format!("{node}.crt.der"),
        "key": format!("{node}.key.der"),
        "region": region,
      })
    })
    .collect();
  let manifest = serde_json::json!({ "name": FLEET_NAME, "f": FLEET_F, "nodes": entries });
  let path = format!("{dir}/fleet.json");
  std::fs::write(&path, manifest.to_string()).unwrap();
  path
}

/// Starts `slates anchor --fleet MANIFEST --node NODE` at `instance`.
fn anchor(instance: &str, manifest: &str, node: &str) -> Anchor {
  let child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--fleet",
      manifest,
      "--node",
      node,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  Anchor { child }
}

/// The pid of `anchor`'s daemon (its one child), once it has one.
fn daemon_pid(anchor: &Anchor) -> u32 {
  let started = Instant::now();
  loop {
    let children = Command::new("pgrep")
      .args(["-P", &anchor.child.id().to_string()])
      .output()
      .unwrap();
    if let Some(pid) = String::from_utf8_lossy(&children.stdout)
      .split_whitespace()
      .next()
    {
      return pid.parse().unwrap();
    }
    assert!(
      started.elapsed() < FLEET_WAIT,
      "the anchor never started its daemon"
    );
    pause();
  }
}

/// `ps -o time`'s `[[dd-]hh:]mm:ss[.ss]` in milliseconds.
fn cpu_ms(text: &str) -> u64 {
  let (days, clock) = text.split_once('-').unwrap_or(("0", text));
  let mut parts: Vec<&str> = clock.split(':').collect();
  let seconds: f64 = parts.pop().unwrap_or("0").parse().unwrap_or(0.0);
  let minutes: u64 = parts.pop().unwrap_or("0").parse().unwrap_or(0);
  let hours: u64 = parts.pop().unwrap_or("0").parse().unwrap_or(0);
  let days: u64 = days.parse().unwrap_or(0);
  let whole_minutes = ((days * 24 + hours) * MIN_PER_HOUR) + minutes;
  #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
  let seconds_ms = (seconds * MS_PER_S as f64) as u64;
  whole_minutes * S_PER_MIN * MS_PER_S + seconds_ms
}

/// A process's resident set (KiB) and CPU time (ms), from `ps`; zeros for a process gone.
fn usage(pid: u32) -> (u64, u64) {
  let output = Command::new("ps")
    .args(["-o", "rss=,time=", "-p", &pid.to_string()])
    .output()
    .unwrap();
  let text = String::from_utf8_lossy(&output.stdout);
  let mut fields = text.split_whitespace();
  let rss = fields.next().and_then(|kib| kib.parse().ok()).unwrap_or(0);
  let cpu = fields.next().map_or(0, cpu_ms);
  (rss, cpu)
}

/// The bytes of `tree` (`du`'s apparent size: GNU `-b`, BSD `-A -k`).
fn tree_bytes(tree: &str) -> u64 {
  let (flags, unit) = if cfg!(target_os = "macos") {
    ("-sAk", KIB)
  } else {
    ("-sb", 1)
  };
  let du = Command::new("du").args([flags, tree]).output().unwrap();
  String::from_utf8_lossy(&du.stdout)
    .split_whitespace()
    .next()
    .unwrap()
    .parse::<u64>()
    .unwrap()
    * unit
}

/// The green over a bounded overlay of `tree` on `primary`, pinned and snapshotted: its id.
fn primary_green(primary: &str, tree: &str) -> String {
  let bound = format!(
    "{}MiB",
    (tree_bytes(tree) * OVERLAY_BOUND_FACTOR).div_ceil(MIB)
  );
  let volume = json(&ok(
    primary,
    &[
      "volume",
      "create",
      "tree",
      "--bounded",
      &bound,
      "--base",
      tree,
      "--json",
    ],
  ))["id"]
    .as_str()
    .unwrap()
    .to_owned();
  ok(primary, &["base", "pin", &volume]);
  let snapshot =
    json(&ok(primary, &["volume", "snapshot", &volume, "--json"]))["snapshot"].to_string();
  json(&ok(
    primary,
    &[
      "green",
      "tree-green",
      "--base",
      &volume,
      "--snapshot",
      &snapshot,
      "--json",
    ],
  ))["id"]
    .as_str()
    .unwrap()
    .to_owned()
}

/// Shares `volume` through `owner` with this test's account on `node` (A-237): a local account means something only on
/// its host, so the harness's clients, which reach the green from a client node, are refused `NotShared` until the green
/// is shared with that node's account (on 1ed67337 the first rung failed so in its setup, 2026-11-09).
fn share_with_node(owner: &str, volume: &str, node: &str) {
  let status = eventually(node, &["status"]);
  let anchor = status
    .lines()
    .find_map(|line| line.strip_prefix("anchor: "))
    .unwrap_or_else(|| panic!("no anchor in {node}'s status: {status}"));
  let account = rustix::process::getuid().as_raw();
  ok(
    owner,
    &[
      "share",
      volume,
      &format!("uid:{account}@{anchor}"),
      "--read",
      "--write",
      "--admin",
    ],
  );
}

/// One node of the run: its name, its anchor and its instance.
struct Node {
  name: String,
  instance: String,
  anchor: Anchor,
}

/// Each node daemon's usage at the start of a rung, and the peak resident set seen since.
struct Watched {
  pid: u32,
  before: (u64, u64),
  peak: u64,
}

/// One rung: `clients` clients through every client node against `green`, each node daemon's usage around it, and
/// server node `kill` SIGKILLed `kill_ms` in when asked (its anchor restarts it). Prints the rung's lines.
fn rung(bench: &str, green: &str, clients: &str, nodes: &[Node], kill: Option<(usize, u64)>) {
  let mut watched: Vec<Watched> = nodes
    .iter()
    .map(|node| {
      let pid = daemon_pid(&node.anchor);
      let before = usage(pid);
      Watched {
        pid,
        before,
        peak: before.0,
      }
    })
    .collect();
  let instances: Vec<&str> = nodes
    .iter()
    .skip(SERVERS.len())
    .map(|node| node.instance.as_str())
    .collect();
  let extra = std::env::var("SLATES_LOADTEST_ARGS").unwrap_or_default();
  let started = Instant::now();
  let mut harness = Command::new(bench)
    .args([
      "--instance",
      &instances.join(","),
      "--green",
      green,
      "--clients",
      clients,
    ])
    .args(extra.split_whitespace())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  let mut killed: Option<u64> = None;
  while harness.try_wait().unwrap().is_none() {
    for watch in &mut watched {
      watch.peak = watch.peak.max(usage(watch.pid).0);
    }
    if let Some((node, ms)) = kill
      && killed.is_none()
      && started.elapsed() >= Duration::from_millis(ms)
    {
      killed = Some(usage(watched[node].pid).1);
      let _ = Command::new("kill")
        .args(["-9", &watched[node].pid.to_string()])
        .status();
    }
    pause();
  }
  let status = harness.wait().unwrap();
  for (index, (node, watch)) in nodes.iter().zip(&watched).enumerate() {
    let after_pid = daemon_pid(&node.anchor);
    let after = usage(after_pid);
    // CPU across a kill is the killed daemon's time at the kill plus its successor's.
    let cpu = match (kill, killed) {
      (Some((victim, _)), Some(at_kill)) if victim == index => at_kill
        .saturating_sub(watch.before.1)
        .saturating_add(after.1),
      _ => after.1.saturating_sub(watch.before.1),
    };
    println!(
      "node\t{clients}\t{}\trss_kib_before\t{}\trss_kib_peak\t{}\trss_kib_after\t{}\tcpu_ms\t{cpu}\tkilled\t{}",
      node.name,
      watch.before.0,
      watch.peak,
      after.0,
      kill.is_some_and(|(victim, _)| victim == index) && killed.is_some()
    );
  }
  println!(
    "rung\t{clients}\twall_ms\t{}\tbench_exit\t{}",
    started.elapsed().as_millis(),
    status.code().unwrap_or(-1)
  );
  assert!(
    status.success(),
    "the harness at {clients} clients exited {status}: its oracle found a failure or it hung"
  );
}

/// Phase 9 at the goal's topology. Do: one primary and two replica server nodes in region 0 at `f = 1`, client nodes in
/// region 1, a green over the tree on the primary, and the load harness through every client node at each rung,
/// killing a server node's daemon partway when asked. Expect every rung's harness to exit 0 (its oracle found
/// nothing), with every node's memory and CPU printed per rung.
#[test]
fn many_clients_through_client_nodes_against_one_primary_and_two_replicas() {
  let (Ok(tree), Ok(bench)) = (
    std::env::var("SLATES_LOADTEST_TREE"),
    std::env::var("SLATES_LOADTEST_BENCH"),
  ) else {
    println!(
      "SKIP: set SLATES_LOADTEST_TREE and SLATES_LOADTEST_BENCH to run the load test at scale"
    );
    return;
  };
  let rungs = std::env::var("SLATES_LOADTEST_CLIENTS").unwrap_or_else(|_| "100".to_owned());
  let client_nodes: usize = std::env::var("SLATES_LOADTEST_CLIENT_NODES")
    .ok()
    .and_then(|n| n.parse().ok())
    .unwrap_or(1)
    .max(1);
  let kill = std::env::var("SLATES_LOADTEST_KILL_MS")
    .ok()
    .and_then(|ms| ms.parse().ok())
    .map(|ms| {
      let node = std::env::var("SLATES_LOADTEST_KILL_NODE")
        .ok()
        .and_then(|node| node.parse().ok())
        .unwrap_or(0usize)
        .min(SERVERS.len() - 1);
      (node, ms)
    });
  let dir = scratch();
  let names: Vec<(String, u64)> = SERVERS
    .iter()
    .map(|name| ((*name).to_owned(), SERVER_REGION))
    .chain((0..client_nodes).map(|k| (format!("client-{k}"), CLIENT_REGION)))
    .collect();
  let mut base = PORT_FLOOR + u16::try_from(std::process::id() % u32::from(PORT_SPAN)).unwrap();
  let mut planned = Vec::new();
  for (name, region) in &names {
    mint_identity(&dir.path, name);
    base = free_base(base);
    planned.push((name.clone(), base, *region));
    base += PORTS_PER_NODE;
  }
  let manifest = write_manifest(&dir.path, &planned);
  let pid = std::process::id();
  let nodes: Vec<Node> = names
    .iter()
    .map(|(name, _)| {
      let instance = format!("load-scale-{name}-{pid}");
      Node {
        anchor: anchor(&instance, &manifest, name),
        name: name.clone(),
        instance,
      }
    })
    .collect();
  for node in &nodes {
    eventually(&node.instance, &["volume", "list"]);
  }
  eventually(&nodes[0].instance, &["bootstrap", "root"]);
  let green = primary_green(&nodes[0].instance, &tree);
  for node in nodes.iter().skip(SERVERS.len()) {
    share_with_node(&nodes[0].instance, &green, &node.instance);
    eventually(&node.instance, &["versions", &green]);
  }
  println!("rung\tgreen\t{green}\ttree\t{tree}\tclient_nodes\t{client_nodes}");
  for clients in rungs.split(',') {
    rung(&bench, &green, clients, &nodes, kill);
  }
}
