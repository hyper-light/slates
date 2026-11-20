//! One binary as a filesystem server and its client (R8; §4.10 "Green volumes in a fleet", §4.16, A-201): two real
//! `slates` fleet nodes from one manifest, the server node holding an overlay of a project directory and a green over
//! its snapshot, the client node reading the green and pushing increments of several files to it through the fleet's
//! forward, and the server checking the merged head out and landing it into the project directory.
//!
//! The landing follows the write policy the server's launcher set (R10, D-22, A-285): bypass writes in the one
//! `slates land`, ask answers the approval request and writes once the launcher approves exactly the presented manifest,
//! and an unset policy asks. The launcher is this test, as kiln is in the field: it starts the server node's anchor with
//! a launcher secret on its standard input (A-299) and sets the policy and approves through `slates-api` with that
//! secret's authority. No approval is a human's. The same flow ran across a Docker container and across two machines
//! (adacachy and kiln-a094a12d) on 2026-11-05, then up to a human's grant.
//!
//! A client node cut off from the server at the fault relay of `support/relay.rs` as the root forms, for longer than the
//! failover delay, must hold the configuration and forward again within a bound its own live estimates give once the
//! link heals (fs-server plan step 4, after the two-machine run g066d; the recorded two-machine program is
//! `docs/wip/bench/fs-two-machines/run.js`, which drives this file's roles).
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

#[path = "support/process_wait.rs"]
mod process_wait;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// Format: an instance served inside a container, as the container lane names it to [`run`]: this prefix, then
/// `CONTAINER/INSTANCE`. Every verb of this file reaches it through `docker exec` as the test's own user, so one flow
/// drives a server node on the host or in its own container unchanged.
const IN_CONTAINER: &str = "docker-exec:";

/// The command that runs `slates` for `instance`, and the instance's own name: the binary on the host, or `docker exec`
/// into the container an [`IN_CONTAINER`] name names.
fn slates_for(instance: &str) -> (Command, String) {
  match instance
    .strip_prefix(IN_CONTAINER)
    .and_then(|rest| rest.split_once('/'))
  {
    Some((container, name)) => {
      let user = format!(
        "{}:{}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
      );
      let mut command = Command::new("docker");
      command.args(["exec", "-i", "--user", &user, container, "slates"]);
      (command, name.to_owned())
    }
    None => (slates(), instance.to_owned()),
  }
}

/// Runs a verb at `instance`; (exit code, stdout, stderr).
fn run(instance: &str, args: &[&str]) -> (i32, String, String) {
  let (mut command, name) = slates_for(instance);
  let output = command
    .arg("--instance")
    .arg(name)
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

/// Retries `verb` until it succeeds, while `instance`'s processes progress ([`process_wait::InstanceWait`]; until
/// 2026-11-20 within the phase's wait, `FLEET_WAIT`, 40 s of the wall); its stdout.
fn eventually(instance: &str, args: &[&str]) -> String {
  let mut progress = process_wait::InstanceWait::new();
  loop {
    let (code, out, err) = run(instance, args);
    if code == 0 {
      return out;
    }
    progress.check(
      instance,
      &format!("slates {args:?} answered (last: {code} {out}{err})"),
    );
    pause();
  }
}

/// A node's process, killed and reaped when dropped, so a failed assertion leaves none behind: its daemon, or the anchor
/// that supervises it. An anchor is asked to stop first (`SIGTERM`, as a host stops one), so it stops its daemon with it
/// rather than leaving the daemon to notice its death a liveness budget later.
struct Daemon {
  child: Child,
  anchored: bool,
}

impl Drop for Daemon {
  fn drop(&mut self) {
    if self.anchored
      && let Ok(raw) = i32::try_from(self.child.id())
      && let Some(pid) = rustix::process::Pid::from_raw(raw)
      && rustix::process::kill_process(pid, rustix::process::Signal::TERM).is_ok()
    {
      // While the anchor's tree gains CPU (until 2026-11-20 within the fleet wait); a stuck stop is killed below.
      let mut judged = process_wait::Judged::of(self.child.id());
      loop {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
          return;
        }
        if matches!(
          judged.judge(),
          process_wait::Verdict::Stalled | process_wait::Verdict::SpentItsBudget
        ) {
          break;
        }
        pause();
      }
    }
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
fn write_manifest(dir: &str, nodes: &[&str], bases: &[u16]) -> String {
  let addressed: Vec<(&str, String)> = nodes
    .iter()
    .zip(bases)
    .map(|(node, base)| (*node, format!("127.0.0.1:{base}")))
    .collect();
  write_manifest_at(dir, &addressed)
}

/// The manifest with each node at the address given (`IP:PORT`, the probe port; records on the next), at `f = 0`.
#[allow(clippy::disallowed_methods)]
fn write_manifest_at(dir: &str, nodes: &[(&str, String)]) -> String {
  let nodes: Vec<serde_json::Value> = nodes
    .iter()
    .map(|(node, address)| {
      serde_json::json!({
        "node": node,
        "address": address,
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
  free_port_block_on(std::net::Ipv4Addr::LOCALHOST, from)
}

/// [`free_port_block`] on the host address `ip` (the container lane's bridge, where its containers reach the host).
fn free_port_block_on(ip: std::net::Ipv4Addr, from: u16) -> HeldBlock {
  let mut base = from;
  for _ in 0..PORT_ATTEMPTS {
    let sockets: Option<Vec<std::net::UdpSocket>> = (0..PORTS_PER_NODE)
      .map(|k| {
        base
          .checked_add(k)
          .and_then(|port| std::net::UdpSocket::bind((ip, port)).ok())
      })
      .collect();
    if let Some(sockets) = sockets {
      return HeldBlock { base, sockets };
    }
    base = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
  }
  panic!("no block of free UDP ports found on {ip} from {from}");
}

/// A duplicate of `socket` a spawned child inherits.
fn inheritable(socket: &std::net::UdpSocket) -> std::os::fd::OwnedFd {
  let duplicate: std::os::fd::OwnedFd = socket.try_clone().unwrap().into();
  rustix::io::fcntl_setfd(&duplicate, rustix::io::FdFlags::empty()).unwrap();
  duplicate
}

/// Starts the node `node` of `manifest` at `instance` with its held sockets: `slates daemon --fleet MANIFEST --node
/// NODE`, or, with a launcher's secret, `slates anchor` with the secret on its standard input (A-299, the command
/// `slates-api` builds for a host), whose daemon then verifies that launcher's proofs.
fn start(
  instance: &str,
  manifest: &str,
  (node, held): (&str, &HeldBlock),
  launcher: Option<&slates_api::LauncherSecret>,
) -> Daemon {
  use std::os::fd::AsRawFd;
  let [probe, record] = held.sockets.as_slice() else {
    panic!("a node's block is its probe and record ports");
  };
  let (probe, record) = (inheritable(probe), inheritable(record));
  let mut command = match launcher {
    None => {
      let mut command = slates();
      command.args([
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
      ]);
      command
    }
    Some(secret) => {
      let mut spec = slates_api::StartSpec::new(
        instance,
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_slates")),
        &[],
      );
      spec.quick = true;
      spec.shards = Some(SHARDS.parse().unwrap());
      spec.fleet = Some(slates_api::role::FleetSelection {
        manifest: manifest.to_owned(),
        node: node.to_owned(),
      });
      spec.with_launcher(secret.clone()).anchor_command().unwrap()
    }
  };
  let child = command
    .env(
      slates_anchor::ENV_FLEET_SERVE,
      format!("{},{}", probe.as_raw_fd(), record.as_raw_fd()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  Daemon {
    child,
    anchored: launcher.is_some(),
  }
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
  let launchers: Vec<Option<&slates_api::LauncherSecret>> = nodes.iter().map(|_| None).collect();
  fleet_with(tag, nodes, &launchers)
}

/// [`fleet`], each node started under its launcher's secret where `launchers` gives one (in `nodes`' order): that node
/// runs under an anchor whose daemon verifies the launcher's proofs (A-299).
fn fleet_with(
  tag: &str,
  nodes: &[&str],
  launchers: &[Option<&slates_api::LauncherSecret>],
) -> (Scratch, Vec<String>, Vec<Daemon>) {
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
    .zip(launchers)
    .map(|(((node, instance), held), launcher)| start(instance, &manifest, (node, held), *launcher))
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
  server_green_named(server, project, "project")
}

/// [`server_green`] with the overlay named `name` and the green `NAME-green`, for a server holding several projects.
fn server_green_named(server: &str, project: &str, name: &str) -> (String, String, String) {
  let volume = id_of(&ok(
    server,
    &[
      "volume",
      "create",
      name,
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
  let green_name = format!("{name}-green");
  let green = id_of(&ok(
    server,
    &[
      "green",
      &green_name,
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
/// unchanged, since the server's write policy is unset, which asks, and nobody approved (A-285). Gated like the other
/// process flows (`SLATES_TEST_CLI=1`).
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
  share_with_client(&server, &green, &client);
  the_client_pushes(&client, &green);
  assert_eq!(
    ok(&server, &["read", &green, "src/two.txt"]),
    "TWO: original line"
  );
  the_server_checks_out_and_presents(&server, &green, (&volume, &snapshot), &project);
  drop(daemons);
  drop(dir);
}

/// Shares `volume` through `server` with this test's account on `client`'s node (A-237): a local account means something
/// only on its host, so the operator of a server a client node reaches shares what that node works on.
fn share_with_client(server: &str, volume: &str, client: &str) {
  let status = ok(client, &["status"]);
  let anchor = status
    .lines()
    .find_map(|line| line.strip_prefix("anchor: "))
    .unwrap_or_else(|| panic!("no anchor in {status:?}"));
  let account = rustix::process::getuid().as_raw();
  ok(
    server,
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

/// On the server node: checks the green's head out into its overlay and presents the landing, which the unset policy
/// holds for an approval, so the disk is unchanged.
fn the_server_checks_out_and_presents(
  server: &str,
  green: &str,
  (volume, snapshot): (&str, &str),
  project: &str,
) {
  the_server_checks_out(server, green, (volume, snapshot));
  let presented = the_landing_asks(server, volume, project);
  assert_eq!(presented["grant_required"], true);
}

/// On the server node: checks the green's head out into its overlay (`volume`, created from `snapshot`), and expects
/// version 2 written as 4 files.
fn the_server_checks_out(server: &str, green: &str, (volume, snapshot): (&str, &str)) {
  let attached: serde_json::Value =
    serde_json::from_str(&ok(server, &["attach", volume, "--write", "--json"])).unwrap();
  let attachment = attached["attachment"].to_string();
  let checkout: serde_json::Value = serde_json::from_str(&ok(
    server,
    &[
      "checkout",
      green,
      volume,
      &attachment,
      "--snapshot",
      snapshot,
      "--json",
    ],
  ))
  .unwrap();
  assert_eq!(
    checkout,
    serde_json::json!({"version": 2, "report": {"written": 4, "made": 0, "removed": 0, "moded": 0}})
  );
}

/// `slates land VOLUME PROJECT` on the server under a policy that asks: the presentation, 1 create and 3 replaces with
/// no conflict, and the project still as it was seeded. Its JSON reply.
fn the_landing_asks(server: &str, volume: &str, project: &str) -> serde_json::Value {
  the_landing_asks_at(server, volume, (project, project))
}

/// [`the_landing_asks`] for a project the server names `target` and this host reads at `on_host`: they differ when the
/// server runs in a container and the project is a volume attached to it.
fn the_landing_asks_at(
  server: &str,
  volume: &str,
  (target, on_host): (&str, &str),
) -> serde_json::Value {
  let presented: serde_json::Value =
    serde_json::from_str(&ok(server, &["land", volume, target, "--json"])).unwrap();
  assert_eq!(presented["grant_required"], true, "{presented}");
  assert_eq!(presented["conflicts"], serde_json::json!([]));
  assert_eq!(
    presented["summary"]["by_action"],
    serde_json::json!([{"action": "create", "count": 1}, {"action": "replace", "count": 3}])
  );
  assert_eq!(
    project_files(on_host),
    seeded_files(),
    "nothing lands before an approval"
  );
  presented
}

/// The project's files as the seed wrote them: (path, bytes), in path order.
fn seeded_files() -> Vec<(String, String)> {
  [
    ("README", "readme original"),
    ("src/one.txt", "one: original line"),
    ("src/two.txt", "two: original line"),
  ]
  .iter()
  .map(|(path, bytes)| ((*path).to_owned(), (*bytes).to_owned()))
  .collect()
}

/// The project's files once the client's two pushes are merged and landed: the seed's three replaced and the client's
/// new file created.
fn merged_files() -> Vec<(String, String)> {
  [
    ("README", "readme original + the client's line"),
    ("src/new.txt", "created by the client"),
    ("src/one.txt", "ONE: original line"),
    ("src/two.txt", "TWO: original line"),
  ]
  .iter()
  .map(|(path, bytes)| ((*path).to_owned(), (*bytes).to_owned()))
  .collect()
}

/// Every regular file under `project` with its bytes, by path relative to it, in path order: what the disk holds.
#[allow(clippy::disallowed_methods)]
fn project_files(project: &str) -> Vec<(String, String)> {
  let mut files = Vec::new();
  let mut pending = vec![std::path::PathBuf::from(project)];
  while let Some(dir) = pending.pop() {
    for entry in std::fs::read_dir(&dir).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        pending.push(path);
      } else {
        let relative = path
          .strip_prefix(project)
          .unwrap()
          .to_string_lossy()
          .into_owned();
        files.push((relative, std::fs::read_to_string(&path).unwrap()));
      }
    }
  }
  files.sort();
  files
}

/// The server node's write policy for `volume`, set as its launcher sets it (A-285, A-299): through `slates-api`, with
/// the authority of the launcher's secret the server's anchor was started with. Merges stay at once, as unset.
fn launcher_sets_land_mode(
  server: &str,
  launcher: &slates_api::Issuer,
  volume: &str,
  land: slates_api::WriteMode,
) {
  let mut instance = slates_api::Instance::attach(server).unwrap();
  let subject = slates_api::PolicySubject::Volume(volume_id(volume));
  let policy = slates_api::WritePolicy {
    merge: slates_api::WriteMode::Bypass,
    land,
    require_signed_merges: false,
  };
  instance
    .set_write_policy(subject, &policy, launcher)
    .unwrap();
  assert_eq!(instance.write_policy(subject).unwrap(), Some(policy));
}

/// A two-node fleet whose nodes run under their launchers' secrets, the client's pushes merged into the server's green
/// and checked out into the overlay of the server's project.
struct Launched {
  /// The nodes' processes, held for their drop; declared first, so they stop before the scratch directory goes.
  _daemons: Vec<Daemon>,
  /// The scratch directory holding the manifest, the identities and the project, held for its drop.
  _scratch: Scratch,
  /// The server node's instance.
  server: String,
  /// The client node's instance.
  client: String,
  /// The server's launcher's secret, then the client's.
  secrets: [slates_api::LauncherSecret; 2],
  /// The server's project directory: the landing's target.
  project: String,
  /// The overlay of the project the green's head was checked out into.
  volume: String,
}

/// Each node's anchor started with its own launcher's secret, as two machines' kiln would start them; then the client's
/// pushes merged into the server's green and checked out into the overlay.
fn launched_and_checked_out(tag: &str) -> Launched {
  let secrets = [
    slates_api::LauncherSecret::generate().unwrap(),
    slates_api::LauncherSecret::generate().unwrap(),
  ];
  let (dir, instances, daemons) = fleet_with(tag, &NODES, &[Some(&secrets[0]), Some(&secrets[1])]);
  let [server, client] = <[String; 2]>::try_from(instances).unwrap();
  let project = format!("{}/project", dir.path);
  seed_project(&project);
  let (volume, snapshot, green) = server_green(&server, &project);
  share_with_client(&server, &green, &client);
  the_client_pushes(&client, &green);
  the_server_checks_out(&server, &green, (&volume, &snapshot));
  Launched {
    _daemons: daemons,
    _scratch: dir,
    server,
    client,
    secrets,
    project,
    volume,
  }
}

/// A-285, R10, R8, §4.15. Do: a server node and a client node, each anchored under its own launcher's secret; the client
/// pushes two increments over four files into the server's green and the server checks the head out into the overlay of
/// its project; the server's launcher sets the overlay's land mode to bypass; then `slates land VOLUME PROJECT` on the
/// server, once. Expect: that one call lands, with 4 entries written (the merge's 1 create and 3 replaces) and nothing
/// failed; the project directory then holds exactly the merged files; and no approval was asked or given. Gated like the
/// other process flows (`SLATES_TEST_CLI=1`).
#[test]
fn a_bypass_server_lands_the_client_nodes_merge_in_one_call() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the two-node bypass landing: set SLATES_TEST_CLI=1 to run it (two anchored nodes)"
    );
    return;
  }
  let launched = launched_and_checked_out("bypass");
  let (server, project, volume) = (&launched.server, &launched.project, &launched.volume);
  let launcher = slates_api::Issuer::from_launcher_secret(&launched.secrets[0]);
  launcher_sets_land_mode(server, &launcher, volume, slates_api::WriteMode::Bypass);
  let landed: serde_json::Value =
    serde_json::from_str(&ok(server, &["land", volume, project, "--json"])).unwrap();
  assert_eq!(landed["grant_required"], false, "{landed}");
  assert_eq!(landed["outcome"]["written"], 4, "{landed}");
  assert_eq!(landed["outcome"]["failed"], 0, "{landed}");
  assert_eq!(landed["outcome"]["conflicts"], 0, "{landed}");
  assert_eq!(project_files(project), merged_files());
  drop(launched);
}

/// A-285, R10, R8, §4.15. Do: as the bypass flow up to the checkout, with the server's land mode set to ask; `slates
/// land VOLUME PROJECT` on the server; then agents' channels on the client node and on the server node try to set the
/// policy to bypass and to approve the presentation, holding the server launcher's genuine authority
/// ([`no_agent_can_set_or_approve`]); then the server's launcher approves a manifest other than the presented one, and
/// then the presented one. Expect: the land answers the approval request (1 create, 3 replaces, its manifest) and writes
/// nothing; no agent sets, approves or writes anything (`NotFound` on the client node, `Forbidden` on the server's);
/// the other manifest is refused `GrantMismatch` (`Stale::Approval`); the presented one is written, 4 entries, and the
/// project holds exactly the merged files.
#[test]
fn an_ask_server_lands_the_client_nodes_merge_once_its_launcher_approves_that_manifest() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the two-node ask landing: set SLATES_TEST_CLI=1 to run it (two anchored nodes)"
    );
    return;
  }
  let launched = launched_and_checked_out("ask");
  let (server, client, project, volume) = (
    &launched.server,
    &launched.client,
    &launched.project,
    &launched.volume,
  );
  let launcher = slates_api::Issuer::from_launcher_secret(&launched.secrets[0]);
  launcher_sets_land_mode(server, &launcher, volume, slates_api::WriteMode::Ask);
  let presented = the_landing_asks(server, volume, project);
  let mut approver = slates_api::Instance::attach(server).unwrap();
  let approval = the_launcher_sees_the_presented_plan(
    &mut approver,
    (volume, project),
    presented["manifest"].as_str().unwrap(),
  );
  no_agent_can_set_or_approve((server, client), &launched.secrets, (volume, &approval));
  assert_eq!(
    approver
      .write_policy(slates_api::PolicySubject::Volume(volume_id(volume)))
      .unwrap()
      .map(|policy| policy.land),
    Some(slates_api::WriteMode::Ask),
    "no agent changed the policy"
  );
  assert_eq!(
    project_files(project),
    seeded_files(),
    "nothing lands on an agent's own approval"
  );
  the_launcher_approves_exactly_the_presented_manifest(
    &mut approver,
    &launcher,
    &approval,
    project,
  );
  drop(approver);
  drop(launched);
}

/// The launcher's own view of the landing `slates land` presented: its plan of `volume` into `project`, as an approval
/// to make, whose manifest must be the one the land printed (`presented`, hexadecimal).
fn the_launcher_sees_the_presented_plan(
  approver: &mut slates_api::Instance,
  (volume, project): (&str, &str),
  presented: &str,
) -> slates_api::LandingApproval {
  let request = slates_api::LandingRequest {
    volume: volume_id(volume),
    snapshot: None,
    target: project.to_owned(),
    filter: slates_api::Filter::default(),
  };
  let approval = match approver.plan_landing(&request).unwrap() {
    slates_api::LandOutcome::ApprovalRequired(approval) => approval,
    other => panic!("an ask landing is an approval request: {other:?}"),
  };
  let hex: String = approval
    .manifest
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  assert_eq!(
    hex, presented,
    "the launcher approves what the land presented"
  );
  approval
}

/// The launcher approves a manifest other than the presented one, then the presented one. Expect the other refused
/// `GrantMismatch` (`Stale::Approval`) with nothing written, and the presented one written: 4 entries, none failed, the
/// project exactly the merged files.
fn the_launcher_approves_exactly_the_presented_manifest(
  approver: &mut slates_api::Instance,
  launcher: &slates_api::Issuer,
  approval: &slates_api::LandingApproval,
  project: &str,
) {
  let mut other = approval.manifest;
  other[0] ^= 1;
  let claim = slates_api::GrantClaim {
    landing: approval.presentation,
    manifest: other,
    scope: slates_api::GrantScope::Once,
    term_ns: slates_api::GRANT_TERM_NS,
  };
  let proof = slates_api::GrantAuthority::prove(launcher, &claim).unwrap();
  assert_eq!(
    approver.grant(&claim, &proof),
    Err(slates_api::Error::Stale(slates_api::Stale::Approval)),
    "an approval of a manifest other than the presented one"
  );
  assert_eq!(project_files(project), seeded_files());
  match approver.approve_landing(approval, launcher).unwrap() {
    slates_api::LandOutcome::Written(report) => {
      assert_eq!(report.written, 4, "{report:?}");
      assert_eq!(report.failed, 0, "{report:?}");
    }
    other => panic!("the approved landing is written: {other:?}"),
  }
  assert_eq!(project_files(project), merged_files());
}

/// An agent's channel at `instance`: bound to a consumer `launcher` enrolled there, as kiln binds every agent's session.
fn agent_channel(instance: &str, launcher: &slates_api::LauncherSecret) -> slates_api::Instance {
  let account = rustix::process::getuid().as_raw();
  let mut enrolling = slates_api::Instance::attach(instance).unwrap();
  let issuer = slates_api::Issuer::from_launcher_secret(launcher);
  // A fleet node that has not imported its groups yet refuses a durable record `NoGroup` and imports them within a
  // phase's wait (§4.8), as `eventually` waits out a verb's: while the node's processes progress.
  let mut progress = process_wait::InstanceWait::new();
  let (consumer, capability) = loop {
    match issuer.enroll(&mut enrolling, account) {
      Err(slates_api::Error::Unusable(slates_api::Unusable::NoGroup)) => {
        progress.check(instance, "the node imported its groups");
        pause();
      }
      enrolled => break enrolled.unwrap(),
    }
  };
  let mut bound = sdk(instance);
  bound.attest(consumer, &capability).unwrap();
  slates_api::Instance::from_client(instance, bound)
}

/// No agent sets the server's policy or approves its landing, even holding the server launcher's genuine authority (its
/// secret leaked, the worst case). Do: from the client node, an agent's channel (enrolled by the client's own launcher)
/// sets the server volume's land mode to bypass and approves the server's presentation; then, from an agent's channel on
/// the server node itself (enrolled by the server's launcher), sets the policy and issues the grant of exactly the
/// presented manifest. Expect: the client node holds neither the volume nor its presentations, so both are refused
/// `NotFound` there; on the server node both are refused `Forbidden` (`Denied::Rights`), since a consumer-bound channel
/// sets no policy and approves no landing (A-285).
fn no_agent_can_set_or_approve(
  (server, client): (&str, &str),
  [server_secret, client_secret]: &[slates_api::LauncherSecret; 2],
  (volume, approval): (&str, &slates_api::LandingApproval),
) {
  let subject = slates_api::PolicySubject::Volume(volume_id(volume));
  let bypass = slates_api::WritePolicy {
    merge: slates_api::WriteMode::Bypass,
    land: slates_api::WriteMode::Bypass,
    require_signed_merges: false,
  };
  let genuine = slates_api::Issuer::from_launcher_secret(server_secret);
  let mut remote = agent_channel(client, client_secret);
  let raised = remote.set_write_policy(subject, &bypass, &genuine);
  eprintln!("fs-server: the client node's agent setting the server's policy: {raised:?}");
  assert!(absent(&raised, volume), "{raised:?}");
  let approved = remote.approve_landing(approval, &genuine);
  eprintln!("fs-server: the client node's agent approving the server's landing: {approved:?}");
  assert!(absent(&approved, volume), "{approved:?}");
  let mut local = agent_channel(server, server_secret);
  let raised = local.set_write_policy(subject, &bypass, &genuine);
  eprintln!("fs-server: the server node's agent setting the policy: {raised:?}");
  assert!(forbidden(&raised), "{raised:?}");
  // The grant itself, of exactly the presented manifest under a genuine proof: what an approval issues.
  let claim = slates_api::GrantClaim {
    landing: approval.presentation,
    manifest: approval.manifest,
    scope: slates_api::GrantScope::Once,
    term_ns: slates_api::GRANT_TERM_NS,
  };
  let proof = slates_api::GrantAuthority::prove(&genuine, &claim).unwrap();
  let granted = local.grant(&claim, &proof);
  eprintln!("fs-server: the server node's agent granting the landing: {granted:?}");
  assert!(forbidden(&granted), "{granted:?}");
}

/// Whether `result` is the refusal of a node that does not hold `volume`: `NotFound` naming it.
fn absent<T>(result: &Result<T, slates_api::Error>, volume: &str) -> bool {
  matches!(result, Err(slates_api::Error::NotFound(slates_api::Missing::Volume(missing)))
    if *missing == volume_id(volume))
}

/// Whether `result` is a refusal by rights (`Forbidden`, `Denied::Rights`): a consumer-bound channel's.
fn forbidden<T>(result: &Result<T, slates_api::Error>) -> bool {
  matches!(
    result,
    Err(slates_api::Error::Denied(slates_api::Denied::Rights { .. }))
  )
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
  share_with_client(&server, &volume, &client);
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

/// Waits until `done` holds of the owner's (attachments, lease epoch), while the processes of `nodes` progress
/// ([`process_wait::InstanceWait`]); how long it took.
fn until_while_progressing(
  server: &str,
  volume: &str,
  nodes: &[&str],
  done: impl Fn(u64, Option<u64>) -> bool,
) -> Duration {
  let started = Instant::now();
  let mut progress = process_wait::InstanceWait::new();
  loop {
    let (attachments, lease) = holders(server, volume);
    if done(attachments, lease) {
      return started.elapsed();
    }
    progress.check_all(
      nodes,
      &format!(
        "no attachment left (still {attachments}, lease {lease:?}); the owner's fleet: {}",
        fleet_lines(server)
      ),
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
/// `LIVENESS_BUDGET_NS`), its write lease at its term after that (the node's failover delay since A-414, under the
/// ten-second placeholder it was before, which A-525 removed), and a liveness budget of the owner's lease sweep after
/// it.
const SILENT_HOLDER_BOUND: Duration = Duration::from_secs(10 + 2 + 1);

/// A-222 (c), D-16. Do: from the client node, a separate holder process attaches the server's volume for writing; then
/// `kill -9` that process. Expect: the owner records the attachment and the write lease while the holder lives; after
/// the kill, the client node's reap ends the attachment at the owner (attachments 0), and the silent holder's lease
/// ends within its term (the failover delay, A-414) plus the sweeps' cadence. Gated (`SLATES_TEST_CLI=1`).
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
  share_with_client(&server, &volume, &client);
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
/// detach through that attachment's id. Expect: both refused `Forbidden` (a forward may act only through its own host's
/// attachment), not `NotShared`: the client's account is shared the volume, so no share can fix this, and the refusal
/// must not tell it one will. The server's attachment and file are unchanged.
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
  share_with_client(&server, &volume, &client);
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
  share_with_client(&server, &volume, &client);
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
  // The retirement is waited for while the survivors' processes progress (until 2026-11-20 within `RETIREMENT_WAIT`, the
  // three-process CLI flow's 40 s phase wait).
  let survivors = [server.as_str(), instances[2].as_str()];
  let ended = until_while_progressing(&server, &volume, &survivors, |attachments, _| {
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

// ── A client node cut off as the root forms (fs-server plan step 4; the two-machine run g066d) ───────────────────────

// The fault relay of `live_chaos.rs` (goal g229, A-277), put between the two nodes to cut and heal their link while both
// processes run on. This flow uses its cut, heal and counts alone, so the rest of it may go unused in this binary.
#[allow(dead_code)]
#[path = "support/relay.rs"]
mod relay;

/// Shape: the server's place in [`NODES`], its node index at the relay.
const SERVER_INDEX: usize = 0;
/// Shape: the client's place in [`NODES`], its node index at the relay.
const CLIENT_INDEX: usize = 1;

/// [`write_manifest_at`] for a fleet behind the fault relay (A-277): each node advertised on loopback at the relay's
/// block for it and bound at its own, so every datagram between the two passes the relay.
#[allow(clippy::disallowed_methods)]
fn write_relayed_manifest(dir: &str, advertised: &[u16], bound: &[u16]) -> String {
  let nodes: Vec<serde_json::Value> = NODES
    .iter()
    .zip(advertised.iter().zip(bound))
    .map(|(node, (advertised, bound))| {
      serde_json::json!({
        "node": node,
        "address": format!("127.0.0.1:{advertised}"),
        "bind": format!("127.0.0.1:{bound}"),
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

/// The server and client nodes up behind the fault relay, the root not yet bootstrapped: (scratch, [server, client], the
/// daemons, the relay). Each node advertises a block the relay holds and binds one of its own (the manifest's `bind`,
/// A-277), so every datagram between the two passes the relay; it forwards everything at once until told otherwise.
fn relayed_two_node_fleet(tag: &str) -> (Scratch, [String; 2], Vec<Daemon>, relay::Relay) {
  let dir = scratch(&format!("fs-server-{tag}"));
  for node in NODES {
    mint_identity(&dir.path, node);
  }
  let offset = std::process::id() % u32::from(PORT_SPAN);
  let mut from = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  let mut next_block = || {
    let held = free_port_block(from);
    from = held.base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
    held
  };
  // The nodes' bind blocks and the relay's advertised ones, all held at once, so the two sets never share a port.
  let bound: Vec<HeldBlock> = NODES.iter().map(|_| next_block()).collect();
  let advertised: Vec<HeldBlock> = NODES.iter().map(|_| next_block()).collect();
  let bound_bases: Vec<u16> = bound.iter().map(|held| held.base).collect();
  let advertised_bases: Vec<u16> = advertised.iter().map(|held| held.base).collect();
  let manifest = write_relayed_manifest(&dir.path, &advertised_bases, &bound_bases);
  let seed = u64::from(std::process::id());
  let relay = relay::Relay::start(
    advertised
      .into_iter()
      .zip(&bound_bases)
      .map(|(held, base)| relay::RelayedNode {
        advertised: held.sockets.try_into().unwrap(),
        bound: std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, *base),
      })
      .collect(),
    seed,
  );
  // A node's serve sockets are its bind block, known up front; only its record links' ports need the relay's census.
  for (index, base) in bound_bases.iter().enumerate() {
    relay.claim(*base, index);
    relay.claim(base.checked_add(1).unwrap(), index);
  }
  eprintln!("fs-server: fleet {tag} behind the relay, seed {seed}");
  let pid = std::process::id();
  let instances: Vec<String> = NODES
    .iter()
    .map(|node| format!("cli-fs-{tag}-{node}-{pid}"))
    .collect();
  let daemons: Vec<Daemon> = NODES
    .iter()
    .zip(&instances)
    .zip(&bound)
    .enumerate()
    .map(|(index, ((node, instance), held))| {
      let daemon = start(instance, &manifest, (node, held), None);
      relay.root(index, daemon.child.id());
      daemon
    })
    .collect();
  for instance in &instances {
    eventually(instance, &["volume", "list"]);
  }
  let [server, client] = <[String; 2]>::try_from(instances).unwrap();
  (dir, [server, client], daemons, relay)
}

/// The value a `slates status` report gives `key` on its `key: value` line.
fn status_value<'a>(status: &'a str, key: &str) -> Option<&'a str> {
  status
    .lines()
    .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
}

/// The number a status line gives `name` in its `name=value` field.
fn field_of(line: &str, name: &str) -> Option<u64> {
  line
    .split_whitespace()
    .find_map(|part| part.strip_prefix(name)?.strip_prefix('=')?.parse().ok())
}

/// Whether a node's status holds a configuration that names both nodes (§4.8: the council's committed configuration,
/// installed into its placement view).
fn configured_with_both(status: &str) -> bool {
  status_value(status, "fleet_configuration_version").is_some_and(|version| version != "0")
    && status_value(status, "fleet_configuration_members") == Some("2")
}

/// What the client's own estimators held of its path to the server just before the cut, the live estimates its recovery
/// bound is derived from: its failure detector's verdict for the pair (the expected arrival of a probe's answer and the
/// margin past it before the probe counts as unanswered, hyper-swim's per-pair NFD-E) and its record session's probe
/// timeout (RFC 9002 §6.2.1: the smoothed round trip plus four times its variation).
#[derive(Debug)]
struct PathEstimates {
  expected: Duration,
  margin: Duration,
  pto: Duration,
}

/// [`PathEstimates`] from `client`'s status for its peer `server_host`, once its detector has a verdict for the pair and
/// its record session to it a probe timeout; `None` before.
fn path_estimates(client: &str, server_host: &str) -> Option<PathEstimates> {
  let status = ok(client, &["status"]);
  let detector_prefix = format!("fleet_detector {server_host}: ");
  let session_prefix = format!("fleet_session {server_host}: ");
  let detector = status
    .lines()
    .find_map(|line| line.strip_prefix(detector_prefix.as_str()))?;
  let session = status
    .lines()
    .find_map(|line| line.strip_prefix(session_prefix.as_str()))?;
  let expected = field_of(detector, "expected_ns").filter(|ns| *ns > 0)?;
  let margin = field_of(detector, "margin_ns")?;
  let pto = field_of(session, "pto_ns").filter(|ns| *ns > 0)?;
  Some(PathEstimates {
    expected: Duration::from_nanos(expected),
    margin: Duration::from_nanos(margin),
    pto: Duration::from_nanos(pto),
  })
}

/// Derived: the bound on a client node's recovery once its link heals, its configuration held and a forwarded read
/// answered, each step at the pace the design states and the live estimates give it (§4.8):
/// - its record session, judged dead by the cut (RFC 9002 §7.6.1's persistent congestion), is dialed afresh, and a dial
///   in its handshake resends its flight at most a handshake probe ceiling apart, the initial probe timeout of a
///   connection with no round-trip sample yet (RFC 9002 §6.2.2, `slates_transport::rtt::INITIAL_PTO_NS`); its two
///   remaining flights are each answered within the path's probe timeout;
/// - the probe plane answers within the detector's verdict for the pair, its expected arrival plus its margin;
/// - the server's council admits the client once it is reachable, a joint change its sole voter commits alone at f = 0,
///   in a coordinator period for each of its two configurations (`HEARTBEAT_NS`);
/// - an unconfigured learner asks for the configuration at least once a membership horizon (`lease::horizon_ns`, the
///   ceiling of its pace), and is answered within the path's probe timeout;
/// - the forwarded read is answered within the path's probe timeout.
fn recovery_bound(path: &PathEstimates) -> Duration {
  let handshake = Duration::from_nanos(slates_transport::rtt::INITIAL_PTO_NS) + path.pto * 2;
  let probe = path.expected + path.margin;
  let admission = Duration::from_nanos(slates_server::daemon::HEARTBEAT_NS) * 2;
  let fetch = Duration::from_nanos(slates_server::lease::horizon_ns()) + path.pto;
  let read = path.pto;
  handshake + probe + admission + fetch + read
}

/// Polls `probe` until it gives a value, while the processes of `nodes` progress ([`process_wait::InstanceWait`]; until
/// 2026-11-20 within the phase's wait, `FLEET_WAIT`, 40 s of the wall).
fn wait_for<T>(what: &str, nodes: &[&str], mut probe: impl FnMut() -> Option<T>) -> T {
  let mut progress = process_wait::InstanceWait::new();
  loop {
    if let Some(value) = probe() {
      return value;
    }
    progress.check_all(nodes, what);
    pause();
  }
}

/// A node's fleet lines, for a failure's report: its configuration, mesh, sessions, learner and fleet refusal counts.
fn fleet_report(instance: &str) -> String {
  let (_, out, err) = run(instance, &["status"]);
  out
    .lines()
    .filter(|line| {
      line.starts_with("fleet_configuration")
        || line.starts_with("fleet_peers")
        || line.starts_with("fleet_replaced")
        || line.starts_with("fleet_learner")
        || line.starts_with("fleet_session ")
        || line.starts_with("fleet_detector ")
        || line.contains(" refused fleet.")
    })
    .map(|line| line.chars().take(200).collect::<String>())
    .collect::<Vec<_>>()
    .join("\n")
    + &err
}

/// The cut of the link-cut flow: put in at the relay before the server bootstraps the root alone (the bootstrap is the
/// server's verb, so the root forms with the client cut off and no fetch can slip through first); while it holds, the
/// server makes a green over its project and shares it with the client (each a verb on the server alone); held for
/// `cut_for`. Expect: datagrams dropped both ways, and the client still unconfigured at the cut's end. The green.
fn cut_as_the_root_forms(
  dir: &Scratch,
  [server, client]: [&str; 2],
  relay: &relay::Relay,
  cut_for: Duration,
) -> String {
  relay.isolate(CLIENT_INDEX);
  eventually(server, &["bootstrap", "root"]);
  let cut_at = Instant::now();
  let project = format!("{}/project", dir.path);
  seed_project(&project);
  let (_volume, _snapshot, green) = server_green(server, &project);
  share_with_client(server, &green, client);
  while cut_at.elapsed() < cut_for {
    pause();
  }
  let report = relay.report();
  assert!(
    report.cut(CLIENT_INDEX, SERVER_INDEX) > 0 && report.cut(SERVER_INDEX, CLIENT_INDEX) > 0,
    "the cut dropped datagrams both ways: {}",
    report.summary()
  );
  assert_eq!(
    status_value(&ok(client, &["status"]), "fleet_configuration_version"),
    Some("0"),
    "the client stayed unconfigured through the cut: {}",
    fleet_report(client)
  );
  green
}

/// What a client node did once its link healed: when it first held a configuration naming both nodes and when a
/// forwarded read first answered, each from the heal, and the last refusal it met.
#[derive(Debug, Default)]
struct Recovery {
  configured: Option<Duration>,
  answered: Option<Duration>,
  last: String,
}

/// Polls `client` from `healed` until a forwarded read of `green` answers the server's bytes, or `give_up` after the
/// heal: first its status until it holds a configuration naming both nodes, then the read.
fn recovery_of(client: &str, green: &str, healed: Instant, give_up: Duration) -> Recovery {
  let mut recovery = Recovery::default();
  while recovery.answered.is_none() && healed.elapsed() < give_up {
    if recovery.configured.is_none() {
      recovery.configured = configured_since(client, healed);
    } else {
      recovery.answered = read_since(client, green, healed, &mut recovery.last);
    }
    if recovery.answered.is_none() {
      pause();
    }
  }
  recovery
}

/// When, from `healed`, a status read began that found `client` holding a configuration naming both nodes; `None` if it
/// did not.
fn configured_since(client: &str, healed: Instant) -> Option<Duration> {
  let began = Instant::now();
  configured_with_both(&ok(client, &["status"])).then(|| began.duration_since(healed))
}

/// When, from `healed`, a forwarded read of `green` through `client` answered the server's bytes; `None` if it did not,
/// with its refusal kept in `last`.
fn read_since(client: &str, green: &str, healed: Instant, last: &mut String) -> Option<Duration> {
  let (code, out, err) = run(client, &["read", green, "src/one.txt"]);
  if code == 0 && out == "one: original line" {
    return Some(healed.elapsed());
  }
  *last = format!("exit {code}: {out}{err}");
  None
}

/// fs-server plan step 4 (the two-machine run g066d: a client node's forwarded `work` refused `OwnerUnreachable` with the
/// client at configuration version 0 and its record sessions re-dialed 15 times); §4.8 formation and learners, R8.
/// Do: a server node and a client node with every datagram between them through the fault relay; cut the link both
/// ways as the root forms ([`cut_as_the_root_forms`]) and hold the cut for longer than the failover delay (the
/// operator's failover SLO plus a membership horizon, so each node's detector has judged the other gone and every
/// forward's resend bound has passed); heal; then poll the client ([`recovery_of`]). Expect: the cut held; once healed,
/// the client holds a configuration naming both nodes and a forwarded read of the server's green answers the server's
/// bytes within [`recovery_bound`] of the heal, from the estimates its own detector and record session held before the
/// cut, plus the harness's own cost of observing it (two CLI calls, measured before the cut, and a poll's pause). Gated
/// like the other process flows (`SLATES_TEST_CLI=1`).
#[test]
fn a_client_node_cut_off_as_the_root_forms_is_configured_and_forwards_within_its_bound_once_the_link_heals()
 {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the link-cut flow: set SLATES_TEST_CLI=1 to run it (two daemons behind the fault relay)"
    );
    return;
  }
  let (dir, [server, client], daemons, relay) = relayed_two_node_fleet("cut");
  let server_host = status_value(&ok(&server, &["status"]), "fleet_host")
    .unwrap()
    .to_owned();
  let path = wait_for(
    "the client's estimates of its path to the server",
    &[&server, &client],
    || path_estimates(&client, &server_host),
  );
  let began = Instant::now();
  ok(&client, &["status"]);
  let observe = began.elapsed();
  // Held past the failover delay each node measures (A-363, read from its status: `fleet_failover: total_ns=`; the
  // ten-second placeholder it was before is gone, A-525) and the membership horizon.
  let failover = [&server, &client].map(|node| {
    status_value(&ok(node, &["status"]), "fleet_failover")
      .and_then(|line| field_of(line, "total_ns"))
      .expect("each node reports its failover delay")
  });
  let cut_for = Duration::from_nanos(
    failover
      .into_iter()
      .max()
      .unwrap_or_default()
      .saturating_add(slates_server::lease::horizon_ns()),
  );
  let green = cut_as_the_root_forms(&dir, [&server, &client], &relay, cut_for);
  relay.heal();
  let healed = Instant::now();
  let bound = recovery_bound(&path);
  let slack = observe * 2 + Duration::from_millis(POLL_MS);
  let recovery = recovery_of(&client, &green, healed, cut_for + bound + slack);
  let timeline = format!(
    "cut held {:.1} s; after the heal: {recovery:?}; bound {bound:?} + observing {slack:?} (from the client's \
     estimates before the cut: {path:?}); relay {}",
    cut_for.as_secs_f64(),
    relay.report().summary()
  );
  eprintln!("fs-server link cut: {timeline}");
  let detail = || {
    format!(
      "{timeline}\nclient:\n{}\nserver:\n{}",
      fleet_report(&client),
      fleet_report(&server)
    )
  };
  let answered = recovery.answered.unwrap_or_else(|| {
    panic!(
      "the client never read the server's green once the link healed: {}",
      detail()
    )
  });
  assert!(
    answered <= bound + slack,
    "the client was configured and forwarded past its bound: {}",
    detail()
  );
  drop(daemons);
  drop(relay);
  drop(dir);
}

/// Format: the environment variable that turns this test binary, re-invoked, into one of its roles, for the lanes that
/// put a node or its launcher where the test process is not (the container lane; the two-machine lane, driven through
/// kiln): `mint`, `unpack`, `node`, `ask`, `bypass` or `approve` ([`role`]). The other `ROLE_*` variables say what the
/// role works on; a launcher's secret comes on standard input, never in an argument or the environment.
const ROLE: &str = "SLATES_FS_ROLE";
/// Format: the role's instance.
const ROLE_INSTANCE: &str = "SLATES_FS_ROLE_INSTANCE";
/// Format: the role's volume (32 hexadecimal characters).
const ROLE_VOLUME: &str = "SLATES_FS_ROLE_VOLUME";
/// Format: the role's landing target, as its server names it.
const ROLE_TARGET: &str = "SLATES_FS_ROLE_TARGET";
/// Format: the role's directory: `mint` writes the fleet's identities there, `unpack` what it is handed.
const ROLE_DIR: &str = "SLATES_FS_ROLE_DIR";
/// Format: the `node` role's fleet manifest.
const ROLE_MANIFEST: &str = "SLATES_FS_ROLE_MANIFEST";
/// Format: the `node` role's node, by its name in the manifest.
const ROLE_NODE: &str = "SLATES_FS_ROLE_NODE";
/// Format: the `node` role's `slates` binary.
const ROLE_SLATES: &str = "SLATES_FS_ROLE_SLATES";
/// Format: the tag of each line a role reports on, found anywhere in a line (libtest's own `test role ... ` comes first
/// on the same line).
const ROLE_TAG: &str = "FS-ROLE:";

/// A role's required variable.
fn role_var(name: &str) -> String {
  std::env::var(name).unwrap_or_else(|_| panic!("the role needs {name}"))
}

/// `bytes` as lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The bytes of a hexadecimal text.
fn unhex(text: &str) -> Vec<u8> {
  text
    .as_bytes()
    .chunks(2)
    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
    .collect()
}

/// The launcher's secret a role reads on its standard input (64 hexadecimal characters), or `None` when given none.
fn role_secret() -> Option<slates_api::LauncherSecret> {
  use std::io::Read;
  let mut text = String::new();
  std::io::stdin().read_to_string(&mut text).unwrap();
  let text = text.trim();
  if text.is_empty() {
    return None;
  }
  let bytes: [u8; slates_api::launcher::LAUNCHER_SECRET_BYTES] = unhex(text)
    .try_into()
    .unwrap_or_else(|_| panic!("a launcher's secret is 32 bytes"));
  Some(slates_api::LauncherSecret::from_bytes(bytes).unwrap())
}

/// Not a test of its own: a role of this binary, run when [`ROLE`] names it (a lane re-invokes the binary where the
/// role must run); otherwise it returns at once.
#[test]
fn role() {
  let Ok(role) = std::env::var(ROLE) else {
    return;
  };
  match role.as_str() {
    "mint" => role_mint(),
    "unpack" => role_unpack(),
    "node" => role_node(),
    "ask" => role_sets_land_mode("ask", slates_api::WriteMode::Ask),
    "bypass" => role_sets_land_mode("bypass", slates_api::WriteMode::Bypass),
    "approve" => role_approve(),
    other => panic!("no role {other:?}"),
  }
}

/// `mint`: the fleet's identities for [`NODES`] in [`ROLE_DIR`], each file reported as `file NAME HEX`, so a lane hands
/// the same identities to every machine.
#[allow(clippy::disallowed_methods)]
fn role_mint() {
  let dir = role_var(ROLE_DIR);
  for node in NODES {
    mint_identity(&dir, node);
    for name in [format!("{node}.crt.der"), format!("{node}.key.der")] {
      let bytes = std::fs::read(format!("{dir}/{name}")).unwrap();
      println!("{ROLE_TAG} file {name} {}", hex(&bytes));
    }
  }
}

/// `unpack`: writes into [`ROLE_DIR`] each `NAME HEX` line of its standard input (bare file names only).
#[allow(clippy::disallowed_methods)]
fn role_unpack() {
  use std::io::Read;
  let dir = role_var(ROLE_DIR);
  let mut input = String::new();
  std::io::stdin().read_to_string(&mut input).unwrap();
  let mut count = 0usize;
  for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
    let (name, text) = line.split_once(' ').unwrap();
    assert!(
      !name.contains('/') && !name.starts_with('.'),
      "a bare file name: {name}"
    );
    std::fs::write(format!("{dir}/{name}"), unhex(text)).unwrap();
    count += 1;
  }
  println!("{ROLE_TAG} unpacked {count}");
}

/// `node`: starts the node [`ROLE_NODE`] of [`ROLE_MANIFEST`] as `slates anchor` at [`ROLE_INSTANCE`] (the command
/// `slates-api` builds for a host), under the launcher's secret when one comes on standard input, and stays until the
/// anchor ends (a lane stops it with `SIGTERM`).
fn role_node() {
  let mut spec = slates_api::StartSpec::new(
    &role_var(ROLE_INSTANCE),
    std::path::PathBuf::from(role_var(ROLE_SLATES)),
    &[],
  );
  spec.quick = true;
  spec.shards = Some(SHARDS.parse().unwrap());
  spec.fleet = Some(slates_api::role::FleetSelection {
    manifest: role_var(ROLE_MANIFEST),
    node: role_var(ROLE_NODE),
  });
  let spec = match role_secret() {
    Some(secret) => spec.with_launcher(secret),
    None => spec,
  };
  let mut anchor = spec
    .anchor_command()
    .unwrap()
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  println!("{ROLE_TAG} started {}", anchor.id());
  let status = anchor.wait().unwrap();
  println!("{ROLE_TAG} ended {status}");
}

/// `ask` and `bypass`: the launcher sets [`ROLE_VOLUME`]'s land mode at [`ROLE_INSTANCE`].
fn role_sets_land_mode(name: &str, land: slates_api::WriteMode) {
  let secret =
    role_secret().unwrap_or_else(|| panic!("the {name} role needs the launcher's secret"));
  let launcher = slates_api::Issuer::from_launcher_secret(&secret);
  launcher_sets_land_mode(
    &role_var(ROLE_INSTANCE),
    &launcher,
    &role_var(ROLE_VOLUME),
    land,
  );
  println!("{ROLE_TAG} {name} set");
}

/// `approve`: the launcher approves exactly the landing of [`ROLE_VOLUME`] into [`ROLE_TARGET`] it is presented, as
/// kiln approves what a harness's person said yes to; reports `written=N failed=M`.
fn role_approve() {
  let secret =
    role_secret().unwrap_or_else(|| panic!("the approve role needs the launcher's secret"));
  let launcher = slates_api::Issuer::from_launcher_secret(&secret);
  let mut approver = slates_api::Instance::attach(&role_var(ROLE_INSTANCE)).unwrap();
  let request = slates_api::LandingRequest {
    volume: volume_id(&role_var(ROLE_VOLUME)),
    snapshot: None,
    target: role_var(ROLE_TARGET),
    filter: slates_api::Filter::default(),
  };
  let approval = match approver.plan_landing(&request).unwrap() {
    slates_api::LandOutcome::ApprovalRequired(approval) => approval,
    other => panic!("an ask landing is an approval request: {other:?}"),
  };
  match approver.approve_landing(&approval, &launcher).unwrap() {
    slates_api::LandOutcome::Written(report) => {
      println!(
        "{ROLE_TAG} written={} failed={}",
        report.written, report.failed
      );
    }
    other => panic!("the approved landing is written: {other:?}"),
  }
}

/// The server in its own container (fs-server plan step 2; R8, A-285, A-299): the server node is `slates anchor` in a
/// Linux container on a Docker network of its own, started with its launcher's secret on standard input, its projects a
/// volume attached to the container; the client node runs on the host and reaches it over the network's bridge. The
/// client pushes into the server's greens, the server checks the heads out, and each merge lands into the attached
/// volume under the launcher's write policy: one project under ask (the approval request first, then the launcher's
/// approval of exactly that manifest), the other under bypass (written in the one `slates land`). The host then reads
/// every merged byte from its side of the attached volume. The launcher is this test binary, re-invoked inside the
/// server's container as a role, since a launcher reaches its instance only where the instance runs. Linux only:
/// Docker Desktop's containers are not reachable by address from the host. Gated (`SLATES_TEST_CLI=1` and
/// `SLATES_TEST_CONTAINER=1`, a reachable Docker), skipping loudly.
#[cfg(target_os = "linux")]
mod in_a_container {
  use super::*;
  use std::io::Write;

  /// Format: the image the server node runs in: the release GitHub's `ubuntu-latest` runs, so the binaries built
  /// beside it (this test binary and `slates`) run in it unchanged, with no library of their own.
  const SERVER_IMAGE: &str = "ubuntu:24.04";
  /// Shape: the projects' directory as the server names it inside its container.
  const PROJECTS_IN_CONTAINER: &str = "/projects";
  /// Shape: the server node's probe port inside its container (records on the next); its network namespace is its
  /// own, so no other process can hold it.
  const SERVER_PORT: u16 = 7_000;
  /// Shape: the last octet of the server container's address on its network.
  const SERVER_HOST_OCTET: u8 = 10;
  /// Shape: the network's /24 is `172.31.N.0`, N searched from a pid-derived start, so concurrent runs pick apart and a
  /// network another run holds is skipped.
  const NETWORK_OCTETS: u8 = 200;
  /// Shape: how long one `docker` command may take on a loaded runner before it is called hung (an image pull included).
  const DOCKER_WAIT: Duration = Duration::from_secs(300);

  /// Whether this run may use the container lane; the loud skip otherwise.
  fn gate() -> bool {
    if std::env::var_os("SLATES_TEST_CLI").is_none()
      || std::env::var_os("SLATES_TEST_CONTAINER").is_none()
    {
      eprintln!(
        "skipping the container lane: set SLATES_TEST_CLI=1 and SLATES_TEST_CONTAINER=1 to run it (a real container)"
      );
      return false;
    }
    match Command::new("docker")
      .args(["info", "--format", "{{.ServerVersion}}"])
      .output()
    {
      Ok(output) if output.status.success() => true,
      Ok(output) => {
        eprintln!(
          "skipping the container lane: docker info failed: {}",
          String::from_utf8_lossy(&output.stderr).trim()
        );
        false
      }
      Err(error) => {
        eprintln!("skipping the container lane: no docker: {error}");
        false
      }
    }
  }

  /// Runs `docker ARGS` to its end, bounded by [`DOCKER_WAIT`]; (exit code, stdout, stderr).
  fn docker(args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new("docker")
      .args(args)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let deadline = Instant::now() + DOCKER_WAIT;
    while child.try_wait().unwrap().is_none() {
      assert!(Instant::now() < deadline, "docker {args:?} hung");
      pause();
    }
    let output = child.wait_with_output().unwrap();
    (
      output.status.code().unwrap_or(-1),
      String::from_utf8_lossy(&output.stdout).into_owned(),
      String::from_utf8_lossy(&output.stderr).into_owned(),
    )
  }

  /// A Docker network of the test's own, removed when dropped (after the containers on it, which are dropped first).
  struct Network {
    name: String,
    /// The network's first three octets.
    prefix: [u8; 3],
  }

  impl Network {
    /// The host's address on the network's bridge: its gateway.
    fn gateway(&self) -> std::net::Ipv4Addr {
      let [a, b, c] = self.prefix;
      std::net::Ipv4Addr::new(a, b, c, 1)
    }

    /// The server container's address.
    fn server(&self) -> std::net::Ipv4Addr {
      let [a, b, c] = self.prefix;
      std::net::Ipv4Addr::new(a, b, c, SERVER_HOST_OCTET)
    }
  }

  impl Drop for Network {
    fn drop(&mut self) {
      let _ = Command::new("docker")
        .args(["network", "rm", &self.name])
        .output();
    }
  }

  /// A network of the test's own on a free /24.
  fn network(tag: &str) -> Network {
    let name = format!("slates-fs-{tag}-{}", std::process::id());
    let start = u8::try_from(std::process::id() % u32::from(NETWORK_OCTETS)).unwrap();
    for step in 0..NETWORK_OCTETS {
      let third = start.wrapping_add(step) % NETWORK_OCTETS;
      let prefix = [172, 31, third];
      let subnet = format!("172.31.{third}.0/24");
      let gateway = format!("172.31.{third}.1");
      let (code, _, _) = docker(&[
        "network",
        "create",
        "--subnet",
        &subnet,
        "--gateway",
        &gateway,
        &name,
      ]);
      if code == 0 {
        return Network { name, prefix };
      }
    }
    panic!("no free /24 for the container lane's network");
  }

  /// The server node's container, stopped and removed when dropped.
  struct ServerContainer {
    name: String,
    /// The `docker run` the anchor runs under (attached, for its standard input).
    child: Child,
  }

  impl Drop for ServerContainer {
    fn drop(&mut self) {
      let _ = Command::new("docker")
        .args(["rm", "-f", &self.name])
        .output();
      let _ = self.child.kill();
      let _ = self.child.wait();
    }
  }

  /// The test's user as Docker takes it (`UID:GID`), so the server writes the attached volume as the host's own user.
  fn user() -> String {
    format!(
      "{}:{}",
      rustix::process::getuid().as_raw(),
      rustix::process::getgid().as_raw()
    )
  }

  /// Docker's `--mount` form of a bind (never `-v`, which makes a missing source on the host).
  fn bind(source: &str, destination: &str, read_only: bool) -> String {
    let mut bind = format!("type=bind,source={source},destination={destination}");
    if read_only {
      bind.push_str(",readonly");
    }
    bind
  }

  /// Starts the server node as `slates anchor --fleet /fleet/fleet.json --node server` in its container, on `network`
  /// at its server address, with `launcher`'s secret on the anchor's standard input: the command `slates-api` builds
  /// for a host (`StartSpec::anchor_command`), here with `docker run` as the program, so the secret's record and the
  /// marker variable reach the anchor inside.
  fn start_server(
    (name, instance): (&str, &str),
    network: &Network,
    (fleet_dir, projects): (&str, &str),
    launcher: &slates_api::LauncherSecret,
  ) -> ServerContainer {
    let test_binary = std::env::current_exe().unwrap();
    let test_binary = test_binary.to_string_lossy().into_owned();
    let address = network.server().to_string();
    let marker = format!(
      "{}={}",
      slates_api::launcher::ENV_LAUNCHER,
      slates_api::launcher::LAUNCHER_ON_STDIN
    );
    let prefix = [
      "run",
      "-i",
      "--rm",
      "--name",
      name,
      "--network",
      &network.name,
      "--ip",
      &address,
      "--user",
      &user(),
      "--mount",
      &bind(env!("CARGO_BIN_EXE_slates"), "/usr/local/bin/slates", true),
      "--mount",
      &bind(&test_binary, "/usr/local/bin/fs-server-role", true),
      "--mount",
      &bind(fleet_dir, "/fleet", true),
      "--mount",
      &bind(projects, PROJECTS_IN_CONTAINER, false),
      "-e",
      &marker,
      SERVER_IMAGE,
      "slates",
    ];
    let mut spec =
      slates_api::StartSpec::new(instance, std::path::PathBuf::from("docker"), &prefix);
    spec.quick = true;
    spec.shards = Some(SHARDS.parse().unwrap());
    spec.fleet = Some(slates_api::role::FleetSelection {
      manifest: "/fleet/fleet.json".to_owned(),
      node: "server".to_owned(),
    });
    let child = spec
      .with_launcher(launcher.clone())
      .anchor_command()
      .unwrap()
      .stdout(Stdio::null())
      .stderr(Stdio::inherit())
      .spawn()
      .unwrap();
    ServerContainer {
      name: name.to_owned(),
      child,
    }
  }

  /// Runs the launcher's role `role` in the server's container: this test binary re-invoked there, its instance,
  /// volume and target in its environment and the launcher's secret on its standard input (never an argument or the
  /// environment). The role's tagged line.
  fn launcher_role(
    container: &str,
    role: &str,
    (instance, volume, target): (&str, &str, &str),
    launcher: &slates_api::LauncherSecret,
  ) -> String {
    let variables = [
      format!("{ROLE}={role}"),
      format!("{ROLE_INSTANCE}={instance}"),
      format!("{ROLE_VOLUME}={volume}"),
      format!("{ROLE_TARGET}={target}"),
    ];
    let mut command = Command::new("docker");
    command.args(["exec", "-i", "--user", &user()]);
    for variable in &variables {
      command.args(["-e", variable]);
    }
    command.args([
      container,
      "/usr/local/bin/fs-server-role",
      "--exact",
      "role",
      "--nocapture",
      "--test-threads",
      "1",
    ]);
    let mut child = command
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let secret: String = launcher
      .bytes()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    child
      .stdin
      .take()
      .unwrap()
      .write_all(secret.as_bytes())
      .unwrap();
    let output = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
      output.status.success(),
      "the launcher's {role} role failed: {out}{err}"
    );
    out
      .lines()
      .find_map(|line| {
        line
          .split_once(ROLE_TAG)
          .map(|(_, said)| said.trim().to_owned())
      })
      .unwrap_or_else(|| panic!("the launcher's {role} role reported nothing: {out}{err}"))
  }

  /// The two nodes up: the server in its container on its own network, the client on the host at the network's
  /// gateway, from one manifest; the root group bootstrapped at the server.
  struct Lane {
    /// The client node's process; dropped first.
    _client: Daemon,
    /// The server node's container; dropped before the network it is on.
    server: ServerContainer,
    /// The network; dropped after the container.
    _network: Network,
    /// The scratch directory: the manifest, the identities and the projects; dropped last.
    scratch: Scratch,
    /// The server's instance as this file's verbs name it ([`IN_CONTAINER`]).
    server_at: String,
    /// The server's instance inside its container.
    server_instance: String,
    /// The client's instance.
    client: String,
    /// The server launcher's secret.
    launcher: slates_api::LauncherSecret,
  }

  fn lane(tag: &str) -> Lane {
    let scratch = scratch(&format!("fs-server-container-{tag}"));
    let fleet_dir = format!("{}/fleet", scratch.path);
    let projects = format!("{}/projects", scratch.path);
    make_dirs(&[&fleet_dir, &projects]);
    let (code, _, err) = docker(&["pull", "--quiet", SERVER_IMAGE]);
    assert_eq!(code, 0, "docker pull {SERVER_IMAGE}: {err}");
    for node in NODES {
      mint_identity(&fleet_dir, node);
    }
    let network = network(tag);
    let offset = std::process::id() % u32::from(PORT_SPAN);
    let from = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
    let held = free_port_block_on(network.gateway(), from);
    let manifest = write_manifest_at(
      &fleet_dir,
      &[
        ("server", format!("{}:{SERVER_PORT}", network.server())),
        ("client", format!("{}:{}", network.gateway(), held.base)),
      ],
    );
    let pid = std::process::id();
    let server_instance = format!("cli-fs-container-{tag}-server-{pid}");
    let container = format!("slates-fs-{tag}-server-{pid}");
    let launcher = slates_api::LauncherSecret::generate().unwrap();
    let server = start_server(
      (&container, &server_instance),
      &network,
      (&fleet_dir, &projects),
      &launcher,
    );
    let client = format!("cli-fs-container-{tag}-client-{pid}");
    let client_daemon = start(&client, &manifest, ("client", &held), None);
    let server_at = format!("{IN_CONTAINER}{container}/{server_instance}");
    eventually(&server_at, &["volume", "list"]);
    eventually(&client, &["volume", "list"]);
    eventually(&server_at, &["bootstrap", "root"]);
    Lane {
      _client: client_daemon,
      server,
      _network: network,
      scratch,
      server_at,
      server_instance,
      client,
      launcher,
    }
  }

  /// Makes each of `dirs` in the scratch directory.
  #[allow(clippy::disallowed_methods)]
  fn make_dirs(dirs: &[&str]) {
    for dir in dirs {
      std::fs::create_dir_all(dir).unwrap();
    }
  }

  /// A project in the attached volume, seeded, with its overlay and green on the server and the client's two pushes
  /// merged and checked out: (the project as the host reads it, as the server names it, the overlay volume).
  fn pushed_and_checked_out(lane: &Lane, name: &str) -> (String, String, String) {
    let on_host = format!("{}/projects/{name}", lane.scratch.path);
    let in_container = format!("{PROJECTS_IN_CONTAINER}/{name}");
    seed_project(&on_host);
    let (volume, snapshot, green) = server_green_named(&lane.server_at, &in_container, name);
    share_with_client(&lane.server_at, &green, &lane.client);
    the_client_pushes(&lane.client, &green);
    the_server_checks_out(&lane.server_at, &green, (&volume, &snapshot));
    (on_host, in_container, volume)
  }

  /// Fs-server plan step 2 (R8, A-285, A-299, §4.15). Do: the server node in its own container and the client node on
  /// the host, as [`in_a_container`] states; two projects in the attached volume. Project `asked`: the client pushes two
  /// increments over four files and a late overlapping one, the server checks the head out, the launcher sets ask,
  /// `slates land` on the server, then the launcher approves exactly the presented manifest. Project `bypassed`: the
  /// same pushes and checkout, the launcher sets bypass, `slates land` once. Expect: the late push refused with its
  /// window `src/one.txt [0..3]` each time; the asked land answers the approval request (1 create, 3 replaces) and the
  /// host reads the project unchanged, then the approval writes 4 entries, none failed; the bypassed land writes 4
  /// entries in the one call; and the host reads exactly the merged files in both projects of the attached volume.
  #[test]
  fn a_server_in_its_own_container_lands_the_client_nodes_merge_into_its_attached_volume() {
    if !gate() {
      return;
    }
    let lane = lane("land");
    let (asked_on_host, asked, asked_volume) = pushed_and_checked_out(&lane, "asked");
    let set = launcher_role(
      &lane.server.name,
      "ask",
      (&lane.server_instance, &asked_volume, &asked),
      &lane.launcher,
    );
    assert_eq!(set, "ask set");
    the_landing_asks_at(&lane.server_at, &asked_volume, (&asked, &asked_on_host));
    let approved = launcher_role(
      &lane.server.name,
      "approve",
      (&lane.server_instance, &asked_volume, &asked),
      &lane.launcher,
    );
    assert_eq!(approved, "written=4 failed=0");
    assert_eq!(project_files(&asked_on_host), merged_files());
    let (bypassed_on_host, bypassed, bypassed_volume) = pushed_and_checked_out(&lane, "bypassed");
    let set = launcher_role(
      &lane.server.name,
      "bypass",
      (&lane.server_instance, &bypassed_volume, &bypassed),
      &lane.launcher,
    );
    assert_eq!(set, "bypass set");
    let landed: serde_json::Value = serde_json::from_str(&ok(
      &lane.server_at,
      &["land", &bypassed_volume, &bypassed, "--json"],
    ))
    .unwrap();
    assert_eq!(landed["grant_required"], false, "{landed}");
    assert_eq!(landed["outcome"]["written"], 4, "{landed}");
    assert_eq!(landed["outcome"]["failed"], 0, "{landed}");
    assert_eq!(project_files(&bypassed_on_host), merged_files());
    drop(lane);
  }
}
