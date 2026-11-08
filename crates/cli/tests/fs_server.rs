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

/// The manifest: every node on loopback at its held base, at `f = 0`.
#[allow(clippy::disallowed_methods)]
fn write_manifest(dir: &str, nodes: &[&str], bases: &[u16]) -> String {
  let nodes: Vec<serde_json::Value> = nodes
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

/// The nodes of `nodes` up and formed, from one manifest: (scratch, each node's instance in order, the daemons), named
/// by `tag` so the tests of this file run side by side. The first node's instance bootstraps the root group, which forms
/// when every member is configured.
fn fleet(tag: &str, nodes: &[&str]) -> (Scratch, Vec<String>, Vec<Daemon>) {
  let dir = scratch(&format!("fs-server-{tag}"));
  for node in nodes {
    mint_identity(&dir.path, node);
  }
  let offset = std::process::id() % u32::from(PORT_SPAN);
  let mut from = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  let mut blocks = Vec::with_capacity(nodes.len());
  for _ in nodes {
    let held = free_port_block(from);
    from = held.base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
    blocks.push(held);
  }
  let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
  let manifest = write_manifest(&dir.path, nodes, &bases);
  let pid = std::process::id();
  let instances: Vec<String> = nodes
    .iter()
    .map(|node| format!("cli-fs-{tag}-{node}-{pid}"))
    .collect();
  let daemons: Vec<Daemon> = nodes
    .iter()
    .zip(&instances)
    .zip(&blocks)
    .map(|((node, instance), held)| start(instance, &manifest, node, held))
    .collect();
  for instance in &instances {
    eventually(instance, &["volume", "list"]);
  }
  eventually(&instances[0], &["bootstrap", "root"]);
  (dir, instances, daemons)
}

/// The server and client nodes up and formed: (scratch, [server, client], the daemons).
fn two_node_fleet(tag: &str) -> (Scratch, [String; 2], Vec<Daemon>) {
  let (dir, instances, daemons) = fleet(tag, &NODES);
  let [server, client] = <[String; 2]>::try_from(instances).unwrap();
  (dir, [server, client], daemons)
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
  let (dir, [server, client], daemons) = two_node_fleet("green");
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

/// Runs a verb at `instance` with `input` on its stdin; (exit code, stdout, stderr).
fn run_with_stdin(instance: &str, args: &[&str], input: &[u8]) -> (i32, String, String) {
  use std::io::Write;
  let mut child = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child.stdin.take().unwrap().write_all(input).unwrap();
  let output = child.wait_with_output().unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// §4.10 "Ownership follows the writer", D-16 (remote write attachment). Do: a server node overlays its project; from
/// the client node, attach the server's volume for writing, write two files through that attachment, then detach it
/// and write once more through it. Expect: the attach answers with an attachment and a lease epoch; both writes answer
/// and the server reads the client's bytes in its own volume; the detach answers; the write after it is refused, and
/// the server's file keeps the bytes written before. Before this, the client node's attach was refused `NotFound`
/// (the fs-server lab, 2026-11-04). Gated like the other process flows (`SLATES_TEST_CLI=1`).
#[test]
fn a_client_node_attaches_a_server_volume_for_writing_and_writes_through_it() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the two-node remote write attachment: set SLATES_TEST_CLI=1 to run it (two daemons)"
    );
    return;
  }
  let (dir, [server, client], daemons) = two_node_fleet("attach");
  let project = format!("{}/project", dir.path);
  seed_project(&project);
  let volume = id_of(&ok(
    &server,
    &[
      "volume",
      "create",
      "project",
      "--dynamic",
      "64MiB",
      "--base",
      &project,
      "--json",
    ],
  ));
  // The client holds one session for its work, as an agent does (A-222: a CLI `attach` and a later `write` are two
  // processes, and the first's client is reaped within two liveness budgets of its exit).
  let mut session = sdk(&client);
  let id = volume_id(&volume);
  assert_eq!(
    eventually(&client, &["read", &volume, "README"]),
    "readme original"
  );
  let attached = session
    .attach(id, None, slates_client::Intent::Write)
    .unwrap();
  assert!(
    attached.lease_epoch.is_some(),
    "a write attachment carries its lease epoch: {attached:?}"
  );
  for (path, bytes) in [
    ("src/one.txt", "one: written from the client node"),
    ("src/three.txt", "three: made by the client node"),
  ] {
    session
      .fs_write((id, attached.attachment), path, bytes.as_bytes(), 0o644)
      .unwrap();
    assert_eq!(ok(&server, &["read", &volume, path]), bytes);
  }
  ok(
    &client,
    &[
      "detach",
      &attached.attachment.to_string(),
      "--volume",
      &volume,
    ],
  );
  let after = session.fs_write(
    (id, attached.attachment),
    "src/one.txt",
    b"after the detach",
    0o644,
  );
  assert!(
    after.is_err(),
    "a write through a detached attachment is refused: {after:?}"
  );
  assert_eq!(
    ok(&server, &["read", &volume, "src/one.txt"]),
    "one: written from the client node"
  );
  drop(daemons);
  drop(dir);
}

/// Format: the variable that makes this test binary, re-invoked, the holder process of the reap test: its value is
/// `INSTANCE VOLUME`.
const HOLDER_ROLE: &str = "SLATES_FS_SERVER_HOLDER";
/// Format: the tag of the holder's one line, found anywhere in a line (libtest prints its banner first).
const HOLDER_TAG: &str = "HOLDER-ATTACHED ";

/// The holder role: a client of `INSTANCE` that attaches `VOLUME` (another host's) for writing, prints the attachment,
/// and waits to be killed. Runs only when the variable names it; a no-op test otherwise.
#[test]
fn holder_role() {
  let Some(value) = std::env::var_os(HOLDER_ROLE) else {
    return;
  };
  let value = value.to_string_lossy().into_owned();
  let (instance, volume) = value.split_once(' ').unwrap();
  let volume = volume_id(volume);
  let mut client = sdk(instance);
  let attached = client
    .attach(volume, None, slates_client::Intent::Write)
    .unwrap();
  println!("{HOLDER_TAG}{}", attached.attachment);
  loop {
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_secs(1));
  }
}

/// The volume's `attachments` and `lease_epoch` as its owner's `status` reports them.
fn holders(server: &str, volume: &str) -> (u64, Option<u64>) {
  let status: serde_json::Value =
    serde_json::from_str(&ok(server, &["status", volume, "--json"])).unwrap();
  (
    status["attachments"].as_u64().unwrap(),
    status["lease_epoch"].as_u64(),
  )
}

/// Waits until `done` holds of the owner's (attachments, lease epoch), within `bound`; how long it took.
fn until(
  server: &str,
  volume: &str,
  bound: Duration,
  done: impl Fn(u64, Option<u64>) -> bool,
) -> Duration {
  let started = Instant::now();
  loop {
    let (attachments, lease) = holders(server, volume);
    if done(attachments, lease) {
      return started.elapsed();
    }
    assert!(
      started.elapsed() < bound,
      "still {attachments} attachments, lease {lease:?}, after {bound:?}; the owner's fleet: {}",
      fleet_lines(server)
    );
    pause();
  }
}

/// The `fleet_` and remote-attachment lines of `instance`'s `status`, for a failure to show where membership stood.
fn fleet_lines(instance: &str) -> String {
  let (_, out, err) = run(instance, &["status"]);
  out
    .lines()
    .filter(|line| {
      line.starts_with("fleet_")
        || line.contains("remote_attach")
        || line.contains("fleet.fan")
        || line.contains("takeover")
    })
    .collect::<Vec<_>>()
    .join(" | ")
    + &err
}

/// Derived: the bound on a silent remote holder (A-222): its node's reap within two liveness budgets (1 s each,
/// `LIVENESS_BUDGET_NS`), its write lease at its term (`failover_slo_ns`, 10 s by default, `FAILOVER_SLO_NS`), and a
/// liveness budget of the owner's lease sweep after it.
const SILENT_HOLDER_BOUND: Duration = Duration::from_secs(10 + 2 + 1);

/// A-222 (c), D-16. Do: from the client node, a separate holder process attaches the server's volume for writing; then
/// `kill -9` that process. Expect: the owner records the attachment and the write lease while the holder lives; after
/// the kill, the client node's reap ends the attachment at the owner (attachments 0), and the silent holder's lease
/// ends within its term (`failover_slo_ns`) plus the sweeps' cadence. Gated (`SLATES_TEST_CLI=1`).
#[test]
fn a_killed_remote_holder_is_reaped_at_the_owner_and_its_lease_ends_within_its_term() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() || std::env::var_os(HOLDER_ROLE).is_some() {
    eprintln!("skipping the remote holder's reap: set SLATES_TEST_CLI=1 to run it (two daemons)");
    return;
  }
  let (dir, [server, client], daemons) = two_node_fleet("reap");
  let project = format!("{}/project", dir.path);
  seed_project(&project);
  let volume = id_of(&ok(
    &server,
    &[
      "volume",
      "create",
      "project",
      "--dynamic",
      "64MiB",
      "--base",
      &project,
      "--json",
    ],
  ));
  assert_eq!(
    eventually(&client, &["read", &volume, "README"]),
    "readme original"
  );
  let mut holder = Command::new(std::env::current_exe().unwrap())
    .args(["--exact", "holder_role", "--nocapture", "--test-threads=1"])
    .env(HOLDER_ROLE, format!("{client} {volume}"))
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  let mut lines = std::io::BufRead::lines(std::io::BufReader::new(holder.stdout.take().unwrap()));
  let attachment = loop {
    let line = lines
      .next()
      .expect("the holder printed its attachment")
      .unwrap();
    if let Some(at) = line.find(HOLDER_TAG) {
      break line[at + HOLDER_TAG.len()..].trim().to_owned();
    }
  };
  let (attachments, lease) = holders(&server, &volume);
  assert_eq!(
    attachments, 1,
    "the owner records the remote attachment {attachment}"
  );
  assert!(lease.is_some(), "and its write lease");
  let killed = Instant::now();
  holder.kill().unwrap();
  holder.wait().unwrap();
  let reaped = until(&server, &volume, SILENT_HOLDER_BOUND, |attachments, _| {
    attachments == 0
  });
  let released = until(&server, &volume, SILENT_HOLDER_BOUND, |_, lease| {
    lease.is_none()
  });
  eprintln!(
    "remote holder killed: its attachment ended at the owner after {reaped:?}, its lease {:?} after the kill",
    killed.elapsed().min(reaped + released)
  );
  drop(daemons);
  drop(dir);
}

/// A-222 hostile forward. Do: the server attaches its own volume for writing, locally; from the client node, write and
/// detach through that attachment's id. Expect: both refused `Forbidden` (a forward may act only through its own
/// host's attachment), and the server's attachment and file unchanged.
#[test]
fn a_forward_naming_another_hosts_attachment_is_refused() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() || std::env::var_os(HOLDER_ROLE).is_some() {
    eprintln!("skipping the hostile forward: set SLATES_TEST_CLI=1 to run it (two daemons)");
    return;
  }
  let (dir, [server, client], daemons) = two_node_fleet("hostile");
  let project = format!("{}/project", dir.path);
  seed_project(&project);
  let volume = id_of(&ok(
    &server,
    &[
      "volume",
      "create",
      "project",
      "--dynamic",
      "64MiB",
      "--base",
      &project,
      "--json",
    ],
  ));
  let local: serde_json::Value =
    serde_json::from_str(&ok(&server, &["attach", &volume, "--write", "--json"])).unwrap();
  let attachment = local["attachment"].to_string();
  assert_eq!(
    eventually(&client, &["read", &volume, "README"]),
    "readme original"
  );
  let (code, out, err) = run_with_stdin(
    &client,
    &["write", &volume, &attachment, "README", "--json"],
    b"stolen",
  );
  assert_eq!(code, 1, "{out}{err}");
  assert!(
    err.contains("Forbidden"),
    "the write is refused Forbidden: {out}{err}"
  );
  let (code, out, err) = run(
    &client,
    &["detach", &attachment, "--volume", &volume, "--json"],
  );
  assert_eq!(code, 1, "{out}{err}");
  assert!(
    err.contains("Forbidden"),
    "the detach is refused Forbidden: {out}{err}"
  );
  assert_eq!(ok(&server, &["read", &volume, "README"]), "readme original");
  assert_eq!(
    holders(&server, &volume).0,
    1,
    "the server's attachment stands"
  );
  an_oci_bind_from_another_host_is_refused_by_kind(&client, &volume, &project);
  drop(daemons);
  drop(dir);
}

/// Shape: how long a retired node takes to leave the configuration on loopback: the three-process CLI flow's phase wait
/// (`FLEET_WAIT` in `tests/cli.rs`: a few protocol periods, widened for daemons booting on a shared machine).
const RETIREMENT_WAIT: Duration = Duration::from_secs(40);

/// A-222, the retired-host sweep. Do: a three-node fleet (server, client, witness); from the client node a holder
/// process attaches the server's volume for writing; then kill the client node's daemon, so no reap of its clients can
/// come. Expect: the owner holds the remote attachment until the fleet retires the client node, then ends it
/// (attachments 0) within the retirement wait. Gated (`SLATES_TEST_CLI=1`).
#[test]
fn a_retired_hosts_attachments_end_at_the_owner() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() || std::env::var_os(HOLDER_ROLE).is_some() {
    eprintln!("skipping the retired-host sweep: set SLATES_TEST_CLI=1 to run it (three daemons)");
    return;
  }
  let (dir, instances, mut daemons) = fleet("retired", &["server", "client", "witness"]);
  let (server, client) = (instances[0].clone(), instances[1].clone());
  let project = format!("{}/project", dir.path);
  seed_project(&project);
  let volume = id_of(&ok(
    &server,
    &[
      "volume",
      "create",
      "project",
      "--dynamic",
      "64MiB",
      "--base",
      &project,
      "--json",
    ],
  ));
  assert_eq!(
    eventually(&client, &["read", &volume, "README"]),
    "readme original"
  );
  let mut holder = Command::new(std::env::current_exe().unwrap())
    .args(["--exact", "holder_role", "--nocapture", "--test-threads=1"])
    .env(HOLDER_ROLE, format!("{client} {volume}"))
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  let mut lines = std::io::BufRead::lines(std::io::BufReader::new(holder.stdout.take().unwrap()));
  loop {
    let line = lines
      .next()
      .expect("the holder printed its attachment")
      .unwrap();
    if line.contains(HOLDER_TAG) {
      break;
    }
  }
  assert_eq!(
    holders(&server, &volume).0,
    1,
    "the owner records the remote attachment"
  );
  // Non-vacuity: a live host's attachment stands across the owner's departure looks (one a liveness budget), so the
  // end below is the retirement's, not a sweep that ends every remote attachment (one such ended them all in 1 s).
  let held_since = Instant::now();
  while held_since.elapsed() < LIVE_HOLD {
    pause();
  }
  assert_eq!(
    holders(&server, &volume).0,
    1,
    "a live host's remote attachment stands for {LIVE_HOLD:?}"
  );
  eprintln!("DIAG at the kill, the owner: {}", detector_lines(&server));
  let killed = Instant::now();
  drop(daemons.remove(1));
  let printed = std::cell::Cell::new(Instant::now());
  let ended = until(&server, &volume, RETIREMENT_WAIT, |attachments, _| {
    if attachments != 0 && printed.get().elapsed() >= Duration::from_secs(5) {
      printed.set(Instant::now());
      eprintln!(
        "DIAG {:?} after the kill, the owner: {}",
        killed.elapsed(),
        detector_lines(&server)
      );
    }
    attachments == 0
  });
  eprintln!(
    "client node killed: its holder's attachment ended at the owner after {ended:?} ({:?} since the kill)",
    killed.elapsed()
  );
  holder.kill().unwrap();
  holder.wait().unwrap();
  drop(daemons);
  drop(dir);
}

/// From the client node, an OCI bind of the server's volume: refused by its kind, `AttachmentUnsupported { Oci,
/// OwnedByAnotherHost }`, since the form hands the client something of the owner's host (A-222 (e)).
fn an_oci_bind_from_another_host_is_refused_by_kind(client: &str, volume: &str, project: &str) {
  let (code, out, err) = run(
    client,
    &[
      "attach",
      volume,
      "--write",
      "--oci-source",
      project,
      "--oci-destination",
      "/work",
      "--json",
    ],
  );
  assert_eq!(code, 1, "{out}{err}");
  assert!(
    err.contains("OwnedByAnotherHost") && err.contains("Oci"),
    "an OCI bind of another host's volume is refused by its kind: {out}{err}"
  );
}

/// A volume id from its 32 hexadecimal characters (the CLI's text form).
fn volume_id(text: &str) -> slates_client::VolumeId {
  let mut bytes = [0u8; 16];
  for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
    *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
  }
  slates_client::VolumeId { bytes }
}

/// An SDK session with the daemon at `instance`, under the product's deadlines.
fn sdk(instance: &str) -> slates_client::Client {
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  slates_client::Client::connect(instance, deadlines).unwrap()
}

/// The owner's membership and detector lines, for the retired test's diagnosis.
fn detector_lines(instance: &str) -> String {
  let (_, out, _) = run(instance, &["status"]);
  out
    .lines()
    .filter(|line| {
      line.starts_with("fleet_members")
        || line.starts_with("fleet_peers_probed")
        || line.starts_with("fleet_configuration_version")
        || line.starts_with("fleet_detector ")
        || line.starts_with("fleet_council_leads")
    })
    .map(|line| line.chars().take(230).collect::<String>())
    .collect::<Vec<_>>()
    .join(" | ")
}

/// Derived: how long the retired test holds a live host's attachment before the kill: two and a half liveness budgets
/// (1 s each, `LIVENESS_BUDGET_NS`), so the owner's departure look (one a budget) runs at least twice meanwhile.
const LIVE_HOLD: Duration = Duration::from_millis(2_500);
