//! Live shipping across real processes (§4.10, A-200): three `slates daemon --fleet` processes from one manifest form
//! an `f = 1` fleet over loopback UDP; a volume on one of them is turned live-shipped, and each file written to it is
//! shipped to both other processes, which keep it, and becomes durable on the owner. Test by use (R5): the CLI and real
//! daemons, observed through `slates status`. Gated like the other process flows (`SLATES_TEST_CLI=1`).
//!
//! The fleet helpers are copies of the process-flow helpers in `cli.rs` (where they are private), cut to what this flow
//! needs.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

#[path = "../../server/tests/common/nfs.rs"]
#[allow(dead_code)]
mod nfs;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long a fleet step may take on a loaded box: the formation wait of the process flows.
const FLEET_WAIT: Duration = Duration::from_secs(40);
/// Shape: how long a daemon may take to answer once started.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: the pause between polls.
const POLL_MS: u64 = 20;
/// Shape: shards per daemon: two, so a volume's owner shard need not be the control shard, where the sessions are.
const SHARDS: &str = "2";
/// Shape: the fleet's TLS name.
const FLEET_NAME: &str = "slates-fleet";
/// Shape: the nodes, in manifest order.
const FLEET_NODES: [&str; 3] = ["a", "b", "c"];
/// Shape: the fleet's fault tolerance: three nodes at `f = 1`.
const FLEET_F: u32 = 1;
/// Shape: the ports a node serves on (`slates_server::deploy`): its base and the next.
const PORTS_PER_NODE: u16 = 2;
/// Shape: the port range searched for each node's block, from a pid-derived start.
const PORT_FLOOR: u16 = 20_000;
/// Shape: see [`PORT_FLOOR`].
const PORT_SPAN: u16 = 30_000;
/// Shape: candidate bases tried per block.
const PORT_ATTEMPTS: u16 = 2_000;
/// Shape: the files written to the live volume.
const FILES: usize = 5;

struct FleetProcess {
  child: Child,
}

impl Drop for FleetProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
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

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

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

fn run_with_stdin(instance: &str, args: &[&str], input: &[u8]) -> (i32, String) {
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
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn value_of(text: &str, key: &str) -> String {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .to_owned()
}

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

fn scratch_dir() -> ScratchDir {
  let raw = format!(
    "{}/slates-live-fleet-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  let made = Command::new("mkdir").args(["-p", &raw]).output().unwrap();
  assert!(made.status.success());
  ScratchDir { path: raw }
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

fn free_port_block(from: u16, count: u16) -> HeldBlock {
  let mut base = from;
  for _ in 0..PORT_ATTEMPTS {
    let sockets: Option<Vec<std::net::UdpSocket>> = (0..count)
      .map(|k| {
        base
          .checked_add(k)
          .and_then(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).ok())
      })
      .collect();
    if let Some(sockets) = sockets {
      return HeldBlock { base, sockets };
    }
    base = base.checked_add(count).unwrap_or(PORT_FLOOR);
  }
  panic!("no block of {count} free loopback UDP ports found from {from}");
}

fn port_blocks() -> Vec<HeldBlock> {
  let offset = (std::process::id().wrapping_mul(7)) % u32::from(PORT_SPAN);
  let mut from = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  let mut blocks = Vec::new();
  for _ in FLEET_NODES {
    let held = free_port_block(from, PORTS_PER_NODE);
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

fn start_fleet_daemon(
  instance: &str,
  manifest: &str,
  node: &str,
  held: &HeldBlock,
) -> FleetProcess {
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
  FleetProcess { child }
}

fn wait_answers(instance: &str) {
  let started = Instant::now();
  while run(instance, &["volume", "list"]).0 != 0 {
    assert!(
      started.elapsed() < START_WAIT,
      "the daemon at {instance} came up"
    );
    pause();
  }
}

/// This node's member id and how many members and probed peers it holds, from `status`.
fn fleet_view(instance: &str) -> Option<(String, usize, u32)> {
  let (code, out, _) = run(instance, &["status"]);
  if code != 0 {
    return None;
  }
  Some((
    value_of(&out, "fleet_host"),
    value_of(&out, "fleet_members").split_whitespace().count(),
    value_of(&out, "fleet_peers_probed").parse().unwrap(),
  ))
}

/// Waits until every node holds all three members alive with both peers probed; returns their member ids.
fn wait_formed(instances: &[String]) -> Vec<String> {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let views: Vec<_> = instances
      .iter()
      .map(|instance| fleet_view(instance))
      .collect();
    if views.iter().all(|view| {
      view
        .as_ref()
        .is_some_and(|(_, members, probed)| *members == FLEET_NODES.len() && *probed == 2)
    }) {
      return views.into_iter().map(|view| view.unwrap().0).collect();
    }
    assert!(Instant::now() < deadline, "the fleet formed: {views:?}");
    pause();
  }
}

/// The sum over every shard of the daemon's counter `kind` (`shard N refused KIND: COUNT` in `status`).
fn counter(instance: &str, kind: &str) -> u64 {
  let (code, out, _) = run(instance, &["status"]);
  if code != 0 {
    return 0;
  }
  let suffix = format!(" refused {kind}: ");
  out
    .lines()
    .filter_map(|line| line.split_once(&suffix).map(|(_, count)| count.trim()))
    .filter_map(|count| count.parse::<u64>().ok())
    .sum()
}

/// Runs `args` on `instance` until it is not refused for an unready consensus (a fresh learner's window), within the
/// fleet wait; the output of the run that was not.
fn run_ready(instance: &str, args: &[&str]) -> String {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let (code, out, err) = run(instance, args);
    if code == 1 && err.contains("ConsensusNotInitialized") && Instant::now() < deadline {
      pause();
      continue;
    }
    assert_eq!(code, 0, "{args:?}: {err}");
    return out;
  }
}

/// A `live` status line's head, durable and owed fields, as printed.
type LiveLine = (String, String, String);

/// A started fleet: its scratch directory, its daemon processes and their instances, and the node bootstrapped.
struct Fleet {
  scratch: ScratchDir,
  daemons: Vec<FleetProcess>,
  instances: Vec<String>,
  bootstrap: usize,
}

/// Starts three daemon processes from one manifest, waits until they form, and bootstraps the root once.
fn start_fleet(tag: &str) -> Fleet {
  let pid = std::process::id();
  let scratch = scratch_dir();
  for node in FLEET_NODES {
    mint_identity(&scratch.path, node);
  }
  let blocks = port_blocks();
  let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
  let manifest = write_manifest(&scratch.path, &bases);
  let instances: Vec<String> = FLEET_NODES
    .iter()
    .map(|node| format!("{tag}-{node}-{pid}"))
    .collect();
  let daemons: Vec<FleetProcess> = FLEET_NODES
    .iter()
    .zip(&instances)
    .zip(&blocks)
    .map(|((node, instance), held)| start_fleet_daemon(instance, &manifest, node, held))
    .collect();
  for instance in &instances {
    wait_answers(instance);
  }
  let hosts = wait_formed(&instances);
  let bootstrap = (0..hosts.len())
    .min_by_key(|index| hosts[*index].parse::<u64>().unwrap())
    .unwrap();
  let (code, _, error) = run(&instances[bootstrap], &["bootstrap", "root"]);
  assert_eq!(code, 0, "one bootstrap of the fresh fleet: {error}");
  Fleet {
    scratch,
    daemons,
    instances,
    bootstrap,
  }
}

/// Creates a live-shipped volume on `owner`, attaches it for writing, and writes `files` files; its id.
fn write_live_volume(owner: &str, files: usize) -> String {
  let id = value_of(
    &run_ready(owner, &["volume", "create", "shipped", "--bounded", "8MiB"]),
    "id",
  );
  let (code, out, err) = run(owner, &["live-ship", &id]);
  assert_eq!(code, 0, "live-ship: {err}");
  assert!(out.contains("live-shipped"), "{out}");
  let attachment = value_of(&run_ready(owner, &["attach", &id, "--write"]), "attachment");
  for file in 0..files {
    let body = live_body(file);
    let path = format!("/file-{file}");
    let (code, err) = run_with_stdin(owner, &["write", &id, &attachment, &path], body.as_bytes());
    assert_eq!(code, 0, "write {path}: {err}");
  }
  id
}

/// The owner's live line and each candidate's, once the owner's log is durable at its head with nothing owed and every
/// candidate holds that head, or at the fleet wait; and whether that happened.
fn wait_caught_up(
  owner: &str,
  others: &[&String],
  volume: &str,
) -> (bool, Option<LiveLine>, Vec<Option<LiveLine>>) {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let shipping = live_line(owner, "shipping", volume);
    let held: Vec<_> = others
      .iter()
      .map(|other| live_line(other, "held", volume))
      .collect();
    let caught_up = shipping.as_ref().is_some_and(|(head, durable, owed)| {
      head != "none"
        && head == durable
        && owed == "false"
        && held
          .iter()
          .all(|kept| kept.as_ref().is_some_and(|(kept, _, _)| kept == head))
    });
    if caught_up || Instant::now() >= deadline {
      return (caught_up, shipping, held);
    }
    pause();
  }
}

/// A-200, AC-8.3's precondition: do turn a volume on one daemon process live-shipped and write files to it. Expect
/// each write answered only once durable on a candidate (the owner's log durable at its head with nothing owed the
/// moment the last write returns), every write shipped (as its own entry or coalesced into a whole record when a burst
/// outran the credit), and both other processes then holding that head.
#[test]
fn three_daemon_processes_ship_a_live_volumes_writes_to_both_candidates() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping fleet flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let fleet = start_fleet("live-fleet");
  let owner = &fleet.instances[(fleet.bootstrap + 1) % fleet.instances.len()];
  let others: Vec<&String> = fleet
    .instances
    .iter()
    .filter(|instance| *instance != owner)
    .collect();
  let id = write_live_volume(owner, FILES);
  // The barrier: each write was answered only once durable on a candidate, so the moment the last one returns the
  // owner's log is durable at its head, with nothing owed, without any wait here.
  let at_return = live_line(owner, "shipping", &id);
  let (caught_up, shipping, held) = wait_caught_up(owner, &others, &id);
  let appended = counter(owner, "fleet.live.appended");
  let awaited = counter(owner, "fleet.live.awaited");
  let refused = counter(owner, "fleet.live.refused");
  let hold_refused: Vec<u64> = others
    .iter()
    .map(|other| counter(other, "fleet.live.hold_refused"))
    .collect();
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!(
    "live fleet: shipping(head, durable, owed)={shipping:?} held={held:?} appended={appended} refused={refused} \
     hold_refused={hold_refused:?}"
  );
  assert!(
    at_return
      .as_ref()
      .is_some_and(|(head, durable, owed)| head == durable && owed == "false"),
    "the last write returned only once durable: {at_return:?}"
  );
  assert!(
    caught_up,
    "both candidates hold the owner's durable head: {shipping:?} {held:?}"
  );
  assert!(appended >= 1, "the owner appended entries: {appended}");
  assert!(
    awaited >= u64::try_from(FILES).unwrap(),
    "each write's reply was deferred to durability: {awaited}"
  );
}

/// The `live` status line of `role` (`shipping` or `held`) for `volume` on `instance`: its head, durable and owed
/// fields.
fn live_line(instance: &str, role: &str, volume: &str) -> Option<LiveLine> {
  let (code, out, _) = run(instance, &["status"]);
  if code != 0 {
    return None;
  }
  let prefix = format!(" live {role} {volume} ");
  out.lines().find_map(|line| {
    let (_, fields) = line.split_once(&prefix)?;
    let head = fields
      .split_whitespace()
      .find_map(|field| field.strip_prefix("head="))?;
    let durable = fields
      .split_whitespace()
      .find_map(|field| field.strip_prefix("durable="))?;
    let owed = fields
      .split_whitespace()
      .find_map(|field| field.strip_prefix("owed="))?;
    Some((head.to_owned(), durable.to_owned(), owed.to_owned()))
  })
}

fn json_field(text: &str, key: &str) -> String {
  let needle = format!("\"{key}\":");
  let after = text
    .split(&needle)
    .nth(1)
    .unwrap_or_else(|| panic!("{key} in {text}"));
  let after = after.trim_start();
  if let Some(quoted) = after.strip_prefix('"') {
    quoted.split('"').next().unwrap().to_owned()
  } else {
    after.split([',', '}']).next().unwrap().trim().to_owned()
  }
}

/// A-200 through a real NFS client: do turn a volume on one daemon process live-shipped, export it, and send it
/// stable (`FILE_SYNC`) writes over NFS. Expect each answered only once a candidate holds it: the moment the last one
/// returns, the owner's log is durable at its head with nothing owed, without any wait here.
#[test]
fn a_stable_nfs_write_to_a_live_volume_is_answered_once_a_candidate_holds_it() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping NFS flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let fleet = start_fleet("live-nfs");
  let owner = &fleet.instances[(fleet.bootstrap + 1) % fleet.instances.len()];
  let id = value_of(
    &run_ready(
      owner,
      &["volume", "create", "nfs-shipped", "--bounded", "8MiB"],
    ),
    "id",
  );
  let (code, _, err) = run(owner, &["live-ship", &id]);
  assert_eq!(code, 0, "live-ship: {err}");
  let (code, out, err) = run(owner, &["export", &id, "--json"]);
  assert_eq!(code, 0, "export: {err}");
  let (path, port) = (json_field(&out, "path"), json_field(&out, "port"));
  let mut stream =
    std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
  stream.set_read_timeout(Some(FLEET_WAIT)).unwrap();
  let root = nfs::mount(&mut stream, &path, 1);
  let file = nfs::create(&mut stream, &root, "stable", 2);
  let mut lines = Vec::new();
  for (xid, write) in (10u32..).zip(0..FILES) {
    let data = format!("stable NFS write {write}, durable on a candidate before its reply");
    nfs::write(&mut stream, &file, data.as_bytes(), xid);
    lines.push(live_line(owner, "shipping", &id));
  }
  let late = counter(owner, "fleet.live.durable_late");
  let waited = counter(owner, "fleet.live.resolved");
  drop(stream);
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!("live nfs: after each write {lines:?} late={late}");
  for line in &lines {
    assert!(
      line
        .as_ref()
        .is_some_and(|(head, durable, owed)| head == durable && owed == "false"),
      "each stable write returned only once durable: {lines:?}"
    );
  }
  assert_eq!(late, 0, "no write waited out its bound");
  // The non-vacuity counter: every stable write's reply went through the durable wait (with it removed this is zero,
  // where the race above, against a status call that takes milliseconds, catches it only some of the time).
  assert!(
    waited >= u64::try_from(FILES).unwrap(),
    "each stable write waited for durability: {waited}"
  );
}

/// The body `write_live_volume` writes to `/file-N`.
fn live_body(file: usize) -> String {
  format!("live write {file} on the owner, kept by both candidates")
}

/// Waits until a survivor serves `volume` with every file `write_live_volume` wrote, byte for byte; that survivor and
/// how long it took from now. Fails, showing every survivor's status, at the fleet wait.
fn wait_served_whole(survivors: &[&String], volume: &str, files: usize) -> (String, Duration) {
  let began = Instant::now();
  let deadline = began + FLEET_WAIT;
  loop {
    for survivor in survivors {
      let (code, listed, _) = run(survivor, &["volume", "list"]);
      if code != 0 || !listed.contains(volume) {
        continue;
      }
      let whole = (0..files).all(|file| {
        let (code, out, _) = run(survivor, &["read", volume, &format!("/file-{file}")]);
        code == 0 && out == live_body(file)
      });
      if whole {
        return ((*survivor).clone(), began.elapsed());
      }
    }
    if Instant::now() >= deadline {
      let states: Vec<String> = survivors
        .iter()
        .map(|survivor| {
          let (_, status, _) = run(survivor, &["status"]);
          let reads: Vec<String> = (0..files)
            .map(|file| {
              let (code, out, err) = run(survivor, &["read", volume, &format!("/file-{file}")]);
              format!("file-{file}: {code} {out:?} {}", err.trim())
            })
            .collect();
          format!("--- {survivor}: {reads:?}\n{status}")
        })
        .collect();
      panic!(
        "a survivor serves every acknowledged write: {}",
        states.join("\n")
      );
    }
    pause();
  }
}

/// A-200, AC-8.3 ("live-shipped volumes report none"): do write five files to a live-shipped volume, each answered only
/// once durable on a candidate, never snapshot it, then kill its owner's process (SIGKILL). Expect a survivor to take
/// the volume over and serve all five files byte for byte: no acknowledged write is lost, where a takeover from the
/// placed seal alone would serve an empty volume (it was never sealed).
#[test]
fn a_live_volumes_acknowledged_writes_survive_its_owners_death() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping takeover flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let mut fleet = start_fleet("live-takeover");
  let owner_index = (fleet.bootstrap + 1) % fleet.instances.len();
  let owner = fleet.instances[owner_index].clone();
  let id = write_live_volume(&owner, FILES);
  // No wait for the creation head's placement here: a live write is answered only once the head is placed too, so the
  // owner may die the moment the last write returns.
  let survivors: Vec<&String> = fleet
    .instances
    .iter()
    .filter(|instance| **instance != owner)
    .collect();
  // The owner dies as a crash kills it, with no seal ever taken.
  drop(fleet.daemons.remove(owner_index));
  let (successor, took) = wait_served_whole(&survivors, &id, FILES);
  let taken_over = counter(&successor, "fleet.live.taken_over");
  let fetched: u64 = survivors
    .iter()
    .map(|survivor| counter(survivor, "fleet.live.fetched"))
    .sum();
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!(
    "live takeover: served whole by {successor} after {took:?}; taken_over={taken_over} fetched={fetched}"
  );
  assert!(
    taken_over >= 1,
    "the successor rebuilt the volume from the live log: {taken_over}"
  );
}
