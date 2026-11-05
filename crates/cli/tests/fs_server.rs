//! One binary as a filesystem server and its client (R8; §4.10 "Green volumes in a fleet", §4.16, A-201): two real
//! `slates daemon --fleet` processes from one manifest, the server node holding an overlay of a project directory and
//! a green over its snapshot, the client node reading the green and pushing increments of several files to it through
//! the fleet's forward, and the server checking the merged head out and presenting its landing.
//!
//! The grant is the human's at the server's anchor (R10) and is not taken here: on Linux no process but the anchor's
//! children can issue one (GAPS, A-201). The granted landing is `crates/server/tests/checkout.rs`'s. The same flow ran
//! across a Docker container and across two machines (adacachy and kiln-a094a12d) on 2026-11-05.
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long each phase of the two-node fleet gets — to answer, to form and to serve a forwarded verb — a few
/// protocol periods on loopback, widened for two daemons booting on a shared machine (the three-node CLI flow allows
/// 40 s a phase).
const FLEET_WAIT: Duration = Duration::from_secs(40);
/// Shape: the pause between polls.
const POLL_MS: u64 = 50;
/// Shape: shards per daemon: two, so a verb may land on a shard that does not own its volume.
const SHARDS: &str = "2";
/// Shape: the fleet's TLS name, carried by every test certificate.
const FLEET_NAME: &str = "slates-fs-server";
/// Shape: the nodes in manifest order: the server, then the client.
const NODES: [&str; 2] = ["server", "client"];
/// Shape: the ports each node serves on (`slates_server::deploy`: one per plane).
const PORTS_PER_NODE: u16 = 2;
/// Shape: the bottom of the port range searched, above the registered ports a shared machine binds.
const PORT_FLOOR: u16 = 20_000;
/// Shape: the range's width; the search starts at a pid-derived offset so concurrent runs start apart.
const PORT_SPAN: u16 = 30_000;
/// Shape: candidate bases tried before failing.
const PORT_ATTEMPTS: u16 = 2_000;

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a verb at `instance`; (exit code, stdout, stderr).
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

/// Runs a verb that must succeed; its stdout.
fn ok(instance: &str, args: &[&str]) -> String {
  let (code, out, err) = run(instance, args);
  assert_eq!(code, 0, "slates {args:?} at {instance}: {out}{err}");
  out
}

/// The `"id"` of a `--json` reply.
fn id_of(json: &str) -> String {
  let value: serde_json::Value = serde_json::from_str(json).unwrap();
  value["id"].as_str().unwrap().to_owned()
}

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

/// Retries `verb` until it succeeds or the phase's wait is spent; its stdout.
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

/// A daemon process, killed and reaped when dropped, so a failed assertion leaves none behind.
struct Daemon {
  child: Child,
}

impl Drop for Daemon {
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
fn scratch(prefix: &str) -> Scratch {
  let raw = format!(
    "{}/{prefix}-{}",
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

/// The manifest: both nodes on loopback at their held bases, at `f = 0`.
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

/// A node's ports, held bound from the search until its daemon has them, so no other process takes one between.
struct HeldBlock {
  base: u16,
  sockets: Vec<std::net::UdpSocket>,
}

fn free_port_block(from: u16) -> HeldBlock {
  let mut base = from;
  for _ in 0..PORT_ATTEMPTS {
    let sockets: Option<Vec<std::net::UdpSocket>> = (0..PORTS_PER_NODE)
      .map(|k| {
        base
          .checked_add(k)
          .and_then(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).ok())
      })
      .collect();
    if let Some(sockets) = sockets {
      return HeldBlock { base, sockets };
    }
    base = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
  }
  panic!("no block of free loopback UDP ports found from {from}");
}

/// A duplicate of `socket` a spawned child inherits.
fn inheritable(socket: &std::net::UdpSocket) -> std::os::fd::OwnedFd {
  let duplicate: std::os::fd::OwnedFd = socket.try_clone().unwrap().into();
  rustix::io::fcntl_setfd(&duplicate, rustix::io::FdFlags::empty()).unwrap();
  duplicate
}

/// Starts `slates daemon --fleet MANIFEST --node NODE` at `instance` with the node's held sockets.
fn start(instance: &str, manifest: &str, node: &str, held: &HeldBlock) -> Daemon {
  use std::os::fd::AsRawFd;
  let [probe, record] = held.sockets.as_slice() else {
    panic!("a node's block is its probe and record ports");
  };
  let (probe, record) = (inheritable(probe), inheritable(record));
  let child = slates()
    .args([
      "--instance",
      instance,
      "daemon",
      "--quick",
      "--shards",
      SHARDS,
      "--fleet",
      manifest,
      "--node",
      node,
    ])
    .env(
      slates_anchor::ENV_FLEET_SERVE,
      format!("{},{}", probe.as_raw_fd(), record.as_raw_fd()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  Daemon { child }
}

/// The server's project as its disk holds it before any agent works on it.
#[allow(clippy::disallowed_methods)]
fn seed_project(dir: &str) {
  std::fs::create_dir_all(format!("{dir}/src")).unwrap();
  std::fs::write(format!("{dir}/README"), "readme original").unwrap();
  std::fs::write(format!("{dir}/src/one.txt"), "one: original line").unwrap();
  std::fs::write(format!("{dir}/src/two.txt"), "two: original line").unwrap();
}

/// The two nodes up and formed: (scratch, the server's and the client's instances, the daemons). The server's
/// instance bootstraps the root group, which forms when both members are configured.
fn two_node_fleet() -> (Scratch, [String; 2], Vec<Daemon>) {
  let dir = scratch("fs-server-fleet");
  for node in NODES {
    mint_identity(&dir.path, node);
  }
  let offset = std::process::id() % u32::from(PORT_SPAN);
  let first = free_port_block(u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap());
  let second = free_port_block(first.base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR));
  let manifest = write_manifest(&dir.path, &[first.base, second.base]);
  let pid = std::process::id();
  let instances = NODES.map(|node| format!("cli-fs-{node}-{pid}"));
  let daemons = vec![
    start(&instances[0], &manifest, NODES[0], &first),
    start(&instances[1], &manifest, NODES[1], &second),
  ];
  for instance in &instances {
    eventually(instance, &["volume", "list"]);
  }
  eventually(&instances[0], &["bootstrap", "root"]);
  (dir, instances, daemons)
}

/// The server's overlay of `project`, pinned and snapshotted, and a green over the snapshot: (volume, snapshot,
/// green).
fn server_green(server: &str, project: &str) -> (String, String, String) {
  let volume = id_of(&ok(
    server,
    &[
      "volume",
      "create",
      "project",
      "--dynamic",
      "64MiB",
      "--base",
      project,
      "--json",
    ],
  ));
  ok(server, &["base", "pin", &volume]);
  let snapshot: serde_json::Value =
    serde_json::from_str(&ok(server, &["volume", "snapshot", &volume, "--json"])).unwrap();
  let snapshot = snapshot["snapshot"].to_string();
  let green = id_of(&ok(
    server,
    &[
      "green",
      "project-green",
      "--base",
      &volume,
      "--snapshot",
      &snapshot,
      "--json",
    ],
  ));
  (volume, snapshot, green)
}

/// A work over `green` with `edits`, created at `client`: its id.
fn work_with(client: &str, green: &str, name: &str, edits: &[(&str, &str, &str, &str)]) -> String {
  let work = id_of(&ok(client, &["work", green, name, "--json"]));
  for (path, at, delete, text) in edits {
    ok(client, &["edit", &work, path, at, delete, text]);
  }
  work
}

/// A-201, R8, §4.10. Do: a server node with an overlay of its project and a green over it, and a client node of the
/// same fleet. From the client: read the green, start a late work at version 0, push two increments over four files,
/// then submit the late one, which edits a byte the first push edited. On the server: check the head out into the
/// overlay and present its landing. Expect: the client reads the server's bytes; the pushes are accepted as versions
/// 1 and 2 and the server reads what they wrote; the late push is refused with the window `src/one.txt [0..3]`; the
/// checkout writes 4 files at version 2; the presentation is 1 create and 3 replaces with no conflict; the disk is
/// unchanged, since no grant was given. Gated like the other process flows (`SLATES_TEST_CLI=1`).
#[test]
fn a_client_node_pushes_files_to_a_server_nodes_green_and_the_server_presents_their_landing() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the two-node filesystem-server flow: set SLATES_TEST_CLI=1 to run it (two daemons)"
    );
    return;
  }
  let (dir, [server, client], daemons) = two_node_fleet();
  let project = format!("{}/project", dir.path);
  seed_project(&project);
  let (volume, snapshot, green) = server_green(&server, &project);
  the_client_pushes(&client, &green);
  assert_eq!(
    ok(&server, &["read", &green, "src/two.txt"]),
    "TWO: original line"
  );
  the_server_checks_out_and_presents(&server, &green, (&volume, &snapshot), &project);
  drop(daemons);
  drop(dir);
}

/// From the client node, through the fleet's forward (both nodes run as this test's user): reads the server's green,
/// starts a late work at version 0, pushes two increments over four files, and has the late one refused with its
/// window.
fn the_client_pushes(client: &str, green: &str) {
  let (client, green) = (client.to_owned(), green.to_owned());
  assert_eq!(
    eventually(&client, &["read", &green, "src/one.txt"]),
    "one: original line"
  );
  let late = work_with(&client, &green, "late", &[("src/one.txt", "0", "3", "Uno")]);
  let first = work_with(
    &client,
    &green,
    "first",
    &[
      ("src/one.txt", "0", "3", "ONE"),
      ("src/new.txt", "0", "0", "created by the client"),
    ],
  );
  assert_eq!(ok(&client, &["submit", &first]).trim(), "accepted: 1");
  let second = work_with(
    &client,
    &green,
    "second",
    &[
      ("README", "15", "0", " + the client's line"),
      ("src/two.txt", "0", "3", "TWO"),
    ],
  );
  assert_eq!(ok(&client, &["submit", &second]).trim(), "accepted: 2");
  let refused: serde_json::Value =
    serde_json::from_str(&ok(&client, &["submit", &late, "--json"])).unwrap();
  assert_eq!(
    refused,
    serde_json::json!({"accepted": false, "conflicts": [{"at": 0, "class": 0, "len": 3, "path": "src/one.txt"}]})
  );
}

/// On the server node: checks the green's head out into its overlay and presents the landing, which waits for a
/// grant, so the disk is unchanged.
fn the_server_checks_out_and_presents(
  server: &str,
  green: &str,
  (volume, snapshot): (&str, &str),
  project: &str,
) {
  let (server, green, volume, snapshot) = (
    server.to_owned(),
    green.to_owned(),
    volume.to_owned(),
    snapshot.to_owned(),
  );
  let attached: serde_json::Value =
    serde_json::from_str(&ok(&server, &["attach", &volume, "--write", "--json"])).unwrap();
  let attachment = attached["attachment"].to_string();
  let checkout: serde_json::Value = serde_json::from_str(&ok(
    &server,
    &[
      "checkout",
      &green,
      &volume,
      &attachment,
      "--snapshot",
      &snapshot,
      "--json",
    ],
  ))
  .unwrap();
  assert_eq!(
    checkout,
    serde_json::json!({"version": 2, "report": {"written": 4, "made": 0, "removed": 0, "moded": 0}})
  );
  let presented: serde_json::Value =
    serde_json::from_str(&ok(&server, &["land", &volume, project, "--json"])).unwrap();
  assert_eq!(presented["grant_required"], true);
  assert_eq!(presented["conflicts"], serde_json::json!([]));
  assert_eq!(
    presented["summary"]["by_action"],
    serde_json::json!([{"action": "create", "count": 1}, {"action": "replace", "count": 3}])
  );
  #[allow(clippy::disallowed_methods)]
  let on_disk = std::fs::read_to_string(format!("{project}/src/one.txt")).unwrap();
  assert_eq!(
    on_disk, "one: original line",
    "nothing lands without a grant"
  );
}
