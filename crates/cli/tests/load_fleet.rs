//! The many-client load test over the network (§4.10, §4.16, A-201's filesystem server; the supervisor's decision (b)
//! of 2026-11-05): a server node and a client node, each `slates anchor --fleet` from one manifest (f = 0, loopback),
//! the server holding a green over a pinned snapshot of a source tree, and the release load harness
//! (`crates/client/examples/load_bench.rs`) driving N clients at the client node, which forwards every verb to the
//! green's owner over the fleet transport. At each rung it reads the server daemon's resident memory and CPU time from
//! `/proc` before and after, so the server's cost per client is measured where it is paid, and it may SIGKILL the
//! server daemon partway through (its anchor restarts it), so the rung is also a chaos run.
//!
//! Gated on the environment, skipping loudly (CLAUDE.md §4): `SLATES_LOADTEST_TREE` (the tree the green is seeded
//! from) and `SLATES_LOADTEST_BENCH` (the release harness binary). Optional: `SLATES_LOADTEST_CLIENTS` (rungs, comma
//! separated, default `100`), `SLATES_LOADTEST_ARGS` (further harness arguments), `SLATES_LOADTEST_KILL_MS` (kill the
//! server daemon this long into each rung). The harness's own lines pass through as it prints them; each rung adds `server` and `client_node` lines.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]
#![cfg(target_os = "linux")]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long each fleet phase gets (to answer, to form, to serve a forwarded verb): the three-node CLI flow's
/// 40 s a phase.
const FLEET_WAIT: Duration = Duration::from_secs(40);
/// Shape: the pause between polls.
const POLL_MS: u64 = 50;
/// Shape: the fleet's TLS name, carried by every certificate the test mints.
const FLEET_NAME: &str = "slates-load-fleet";
/// Shape: the nodes in manifest order.
const NODES: [&str; 2] = ["server", "client"];
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
    "{}/load-fleet-{}",
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

/// The manifest: both nodes on loopback at `bases`, at `f = 0`.
#[allow(clippy::disallowed_methods)]
fn write_manifest(dir: &str, bases: &[u16]) -> String {
  let nodes: Vec<serde_json::Value> = NODES
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
  let manifest = serde_json::json!({ "name": FLEET_NAME, "f": 0, "nodes": nodes });
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
#[allow(clippy::disallowed_methods)]
fn daemon_pid(anchor: &Anchor) -> u32 {
  let started = Instant::now();
  loop {
    let children =
      std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", anchor.child.id()))
        .unwrap_or_default();
    if let Some(pid) = children.split_whitespace().next() {
      return pid.parse().unwrap();
    }
    assert!(
      started.elapsed() < FLEET_WAIT,
      "the anchor never started its daemon"
    );
    pause();
  }
}

/// A process's resident set (KiB) and CPU time (clock ticks, user plus system), from `/proc`.
#[allow(clippy::disallowed_methods)]
fn usage(pid: u32) -> (u64, u64) {
  let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
  let rss = status
    .lines()
    .find(|line| line.starts_with("VmRSS:"))
    .and_then(|line| line.split_whitespace().nth(1))
    .and_then(|kib| kib.parse().ok())
    .unwrap_or(0);
  let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
  let after_name = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
  let fields: Vec<&str> = after_name.split_whitespace().collect();
  // Fields 14 and 15 of stat(5) (utime, stime) are the 12th and 13th after the command name.
  let ticks = fields
    .get(11)
    .zip(fields.get(12))
    .map_or(0, |(user, system)| {
      user.parse::<u64>().unwrap_or(0) + system.parse::<u64>().unwrap_or(0)
    });
  (rss, ticks)
}

/// The green over a bounded overlay of `tree` on `server`, pinned and snapshotted: its id.
fn server_green(server: &str, tree: &str) -> String {
  let du = Command::new("du").args(["-sb", tree]).output().unwrap();
  let bytes: u64 = String::from_utf8_lossy(&du.stdout)
    .split_whitespace()
    .next()
    .unwrap()
    .parse()
    .unwrap();
  let bound = format!("{}MiB", (bytes * OVERLAY_BOUND_FACTOR).div_ceil(MIB));
  let volume = json(&ok(
    server,
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
  ok(server, &["base", "pin", &volume]);
  let snapshot =
    json(&ok(server, &["volume", "snapshot", &volume, "--json"]))["snapshot"].to_string();
  json(&ok(
    server,
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

/// One rung: `clients` clients through `client` against `green`, the server daemon `pid`'s usage around it, and the
/// server daemon SIGKILLed `kill_ms` in when asked (its anchor restarts it). Prints the rung's lines.
fn rung(
  bench: &str,
  (client, green): (&str, &str),
  clients: &str,
  (server, client_node, kill_ms): (&Anchor, &Anchor, Option<u64>),
) {
  let pid = daemon_pid(server);
  let client_pid = daemon_pid(client_node);
  let (rss_before, ticks_before) = usage(pid);
  let (client_rss_before, client_ticks_before) = usage(client_pid);
  let mut client_peak = client_rss_before;
  let extra = std::env::var("SLATES_LOADTEST_ARGS").unwrap_or_default();
  let started = Instant::now();
  let mut harness = Command::new(bench)
    .args(["--instance", client, "--green", green, "--clients", clients])
    .args(extra.split_whitespace())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  let mut killed = None;
  let mut peak = rss_before;
  while harness.try_wait().unwrap().is_none() {
    peak = peak.max(usage(pid).0);
    client_peak = client_peak.max(usage(client_pid).0);
    if let Some(ms) = kill_ms
      && killed.is_none()
      && started.elapsed() >= Duration::from_millis(ms)
    {
      let (_, ticks) = usage(pid);
      let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
      killed = Some(ticks);
    }
    pause();
  }
  let status = harness.wait().unwrap();
  let (client_rss_after, client_ticks_after) = usage(client_pid);
  let after = daemon_pid(server);
  let (rss_after, ticks_after) = usage(after);
  // CPU across a kill is the killed daemon's ticks at the kill plus its successor's.
  let ticks = match killed {
    Some(at_kill) => at_kill
      .saturating_sub(ticks_before)
      .saturating_add(ticks_after),
    None => ticks_after.saturating_sub(ticks_before),
  };
  println!(
    "server\t{clients}\trss_kib_before\t{rss_before}\trss_kib_peak\t{peak}\trss_kib_after\t{rss_after}\tcpu_ticks\t{ticks}\twall_ms\t{}\tkilled\t{}\tbench_exit\t{}",
    started.elapsed().as_millis(),
    killed.is_some(),
    status.code().unwrap_or(-1)
  );
  println!(
    "client_node\t{clients}\trss_kib_before\t{client_rss_before}\trss_kib_peak\t{client_peak}\trss_kib_after\t{client_rss_after}\tcpu_ticks\t{}",
    client_ticks_after.saturating_sub(client_ticks_before)
  );
  assert!(
    status.success(),
    "the harness at {clients} clients exited {status}: its oracle found a failure or it hung"
  );
}

/// Phase 9 at network scale (the supervisor's decision (b)): do: two anchored fleet nodes, a green over the tree on
/// the server, and the load harness at the client node at each rung, killing the server daemon partway when asked.
/// Expect every rung's harness to exit 0 (its oracle found nothing), with the server's memory and CPU printed per rung.
#[test]
fn many_clients_through_a_client_node_against_a_server_nodes_green() {
  let (Ok(tree), Ok(bench)) = (
    std::env::var("SLATES_LOADTEST_TREE"),
    std::env::var("SLATES_LOADTEST_BENCH"),
  ) else {
    println!(
      "SKIP: set SLATES_LOADTEST_TREE and SLATES_LOADTEST_BENCH to run the network load test"
    );
    return;
  };
  let rungs = std::env::var("SLATES_LOADTEST_CLIENTS").unwrap_or_else(|_| "100".to_owned());
  let kill_ms = std::env::var("SLATES_LOADTEST_KILL_MS")
    .ok()
    .and_then(|ms| ms.parse().ok());
  let dir = scratch();
  for node in NODES {
    mint_identity(&dir.path, node);
  }
  let first =
    free_base(PORT_FLOOR + u16::try_from(std::process::id() % u32::from(PORT_SPAN)).unwrap());
  let second = free_base(first + PORTS_PER_NODE);
  let manifest = write_manifest(&dir.path, &[first, second]);
  let pid = std::process::id();
  let [server_instance, client_instance] = NODES.map(|node| format!("load-fleet-{node}-{pid}"));
  let server = anchor(&server_instance, &manifest, NODES[0]);
  let client = anchor(&client_instance, &manifest, NODES[1]);
  for instance in [&server_instance, &client_instance] {
    eventually(instance, &["volume", "list"]);
  }
  eventually(&server_instance, &["bootstrap", "root"]);
  let green = server_green(&server_instance, &tree);
  eventually(&client_instance, &["versions", &green]);
  println!("rung\tgreen\t{green}\ttree\t{tree}");
  for clients in rungs.split(',') {
    rung(
      &bench,
      (&client_instance, &green),
      clients,
      (&server, &client, kill_ms),
    );
  }
}
