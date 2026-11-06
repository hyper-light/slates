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

/// One directory per fleet: the tests run in parallel in one process, and a directory named by the process alone let
/// each fleet's manifest and identities overwrite another's, so a daemon was handed another fleet's ports and refused
/// to start (8 of 9 flows, 2026-11-05).
fn scratch_dir(tag: &str) -> ScratchDir {
  let raw = format!(
    "{}/slates-live-fleet-{tag}-{}",
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
/// Every `fleet.live.*` counter `instance` reports, summed over its shards, by name.
fn live_counters(instance: &str) -> std::collections::BTreeMap<String, u64> {
  let (_, status, _) = run(instance, &["status"]);
  let mut counts = std::collections::BTreeMap::<String, u64>::new();
  for (kind, count) in status
    .lines()
    .filter_map(|line| {
      line
        .split_once(" refused fleet.live.")
        .map(|(_, rest)| rest)
    })
    .filter_map(|rest| rest.split_once(": "))
  {
    *counts.entry(kind.to_owned()).or_default() += count.trim().parse::<u64>().unwrap_or(0);
  }
  counts
}

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
  let scratch = scratch_dir(tag);
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
  let survivor_counters: Vec<_> = survivors
    .iter()
    .map(|survivor| ((*survivor).clone(), live_counters(survivor)))
    .collect();
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!(
    "live takeover: served whole by {successor} after {took:?}; taken_over={taken_over} fetched={fetched}; \
     {survivor_counters:?}"
  );
  assert!(
    taken_over >= 1,
    "the successor rebuilt the volume from the live log: {taken_over}"
  );
}

// ── Chaos (A-200; Ada's goal of 2026-11-04): candidates paused, latency injected, the owner paused past its takeover,
// clients cut off mid-write. SIGSTOP is the unprivileged partition: a stopped process answers nothing, its sockets keep
// their ports, and SIGCONT returns it with every timer late, as a healed partition or a long scheduling stall does.

/// Sends `signal` to a fleet daemon's process.
fn signal(process: &FleetProcess, signal: rustix::process::Signal) {
  rustix::process::kill_process(rustix::process::Pid::from_child(&process.child), signal).unwrap();
}

/// Creates a live-shipped volume on `owner`: its id.
fn create_live_volume(owner: &str, name: &str) -> String {
  let id = value_of(
    &run_ready(owner, &["volume", "create", name, "--bounded", "8MiB"]),
    "id",
  );
  let (code, _, err) = run(owner, &["live-ship", &id]);
  assert_eq!(code, 0, "live-ship: {err}");
  id
}

/// Writes `/file-N` for each N in `files` through `slates write`, each through a fresh attachment (an attachment made
/// by a `slates attach` process is reaped once that client is gone, so one kept across writes is refused `NotFound`
/// a few seconds on), let go once the write is answered; each write's latency as the client saw it, attach included.
fn write_files(owner: &str, id: &str, files: std::ops::Range<usize>) -> Vec<Duration> {
  files
    .map(|file| {
      let began = Instant::now();
      let attachment = value_of(&run_ready(owner, &["attach", id, "--write"]), "attachment");
      let (code, err) = run_with_stdin(
        owner,
        &["write", id, &attachment, &format!("/file-{file}")],
        live_body(file).as_bytes(),
      );
      assert_eq!(code, 0, "write /file-{file}: {err}");
      let elapsed = began.elapsed();
      // Let the attachment go, or a long run meets the volume's attachment bound before the reap does. The `attach`
      // process that made it has exited, so the reap may let it go first (a write that waited out a paused candidate, a
      // loaded host: macOS at load 23-27, 2026-11-05); that is the same release, answered `NotFound`.
      let (code, _, err) = run(owner, &["detach", &attachment]);
      assert!(
        code == 0 || err.contains("refused: NotFound"),
        "detach: {err}"
      );
      elapsed
    })
    .collect()
}

/// A-200 under a partition: do stop one candidate's process, keep writing, then resume it. Expect every write
/// answered (durable through the other candidate, the owner's log durable at its head the moment the last returns),
/// and the stopped candidate, resumed, caught up to the owner's head.
#[test]
fn a_paused_candidate_neither_stops_writes_nor_stays_behind() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping chaos flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let fleet = start_fleet("live-pause");
  let owner_index = (fleet.bootstrap + 1) % fleet.instances.len();
  let owner = &fleet.instances[owner_index];
  let paused_index = (owner_index + 1) % fleet.instances.len();
  let others: Vec<&String> = fleet
    .instances
    .iter()
    .filter(|instance| *instance != owner)
    .collect();
  let id = create_live_volume(owner, "paused");
  write_files(owner, &id, 0..1);
  signal(&fleet.daemons[paused_index], rustix::process::Signal::STOP);
  let latencies = write_files(owner, &id, 1..FILES);
  let at_return = live_line(owner, "shipping", &id);
  signal(&fleet.daemons[paused_index], rustix::process::Signal::CONT);
  let (caught_up, shipping, held) = wait_caught_up(owner, &others, &id);
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!(
    "live pause: writes while paused {latencies:?}; at return {at_return:?}; then {shipping:?} {held:?}"
  );
  assert!(
    at_return
      .as_ref()
      .is_some_and(|(head, durable, owed)| head == durable && owed == "false"),
    "writes stayed durable through the live candidate: {at_return:?}"
  );
  assert!(
    caught_up,
    "the resumed candidate caught up: {shipping:?} {held:?}"
  );
}

/// A-200 under injected latency: do stop each candidate's process for `STALL` of every `2 × STALL`, out of phase, so for
/// part of each period both are stopped, while writing `WRITES_UNDER_STALLS` files. Expect every write answered and durable
/// (some waiting out a stall: both candidates stopped means no durability until one resumes), none refused, and both
/// candidates holding the owner's head once the stalls stop.
#[test]
fn writes_stay_durable_through_injected_candidate_stalls() {
  /// Shape: how long a candidate is stopped at a time: tens of milliseconds, a long scheduling stall or a congested
  /// path's queueing, far under the fleet's death detection (seconds).
  const STALL: Duration = Duration::from_millis(30);
  /// Shape: the writes made through the stalls.
  const WRITES_UNDER_STALLS: usize = 30;
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping stall flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let fleet = start_fleet("live-stall");
  let owner_index = (fleet.bootstrap + 1) % fleet.instances.len();
  let owner = fleet.instances[owner_index].clone();
  let others: Vec<&String> = fleet
    .instances
    .iter()
    .filter(|instance| **instance != owner)
    .collect();
  let id = create_live_volume(&owner, "stalled");
  write_files(&owner, &id, 0..1);
  let pids: Vec<rustix::process::Pid> = (0..fleet.daemons.len())
    .filter(|index| *index != owner_index)
    .map(|index| rustix::process::Pid::from_child(&fleet.daemons[index].child))
    .collect();
  let stalling = std::sync::atomic::AtomicBool::new(true);
  let latencies = std::thread::scope(|scope| {
    scope.spawn(|| {
      // Out of phase: the first candidate stops at 0 and resumes at STALL; the second stops at STALL / 2 and resumes
      // at 3 STALL / 2 — so both are stopped for STALL / 2 of every period.
      while stalling.load(std::sync::atomic::Ordering::Acquire) {
        for (pid, stop) in [
          (pids[0], true),
          (pids[1], true),
          (pids[0], false),
          (pids[1], false),
        ] {
          let sent = if stop {
            rustix::process::Signal::STOP
          } else {
            rustix::process::Signal::CONT
          };
          let _ = rustix::process::kill_process(pid, sent);
          #[allow(clippy::disallowed_methods)]
          std::thread::sleep(STALL / 2);
        }
      }
      for pid in &pids {
        let _ = rustix::process::kill_process(*pid, rustix::process::Signal::CONT);
      }
    });
    // The stalls stop however the writes end: a write's failed assertion unwinds through this guard, and the scope,
    // which joins its threads before it passes a panic on, would otherwise wait on a stall thread that never stops.
    struct StopStalls<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for StopStalls<'_> {
      fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
      }
    }
    let _stop = StopStalls(&stalling);
    write_files(&owner, &id, 1..1 + WRITES_UNDER_STALLS)
  });
  let at_return = live_line(&owner, "shipping", &id);
  let (caught_up, shipping, held) = wait_caught_up(&owner, &others, &id);
  let late = counter(&owner, "fleet.live.over_bound");
  drop(fleet.daemons);
  drop(fleet.scratch);
  let mut sorted = latencies.clone();
  sorted.sort();
  eprintln!(
    "live stall: {} writes, client-seen latency p50 {:?} p90 {:?} max {:?}; at return {at_return:?}; then {shipping:?} \
     {held:?}; over_bound={late}",
    sorted.len(),
    sorted[sorted.len() / 2],
    sorted[sorted.len() * 9 / 10],
    sorted[sorted.len() - 1]
  );
  assert!(
    at_return
      .as_ref()
      .is_some_and(|(head, durable, owed)| head == durable && owed == "false"),
    "every write answered durable through the stalls: {at_return:?}"
  );
  assert!(
    caught_up,
    "both candidates caught up once the stalls stopped: {shipping:?} {held:?}"
  );
  assert_eq!(late, 0, "no write was refused for want of room to wait");
}

/// A-200 under a destroyed volume: do destroy a live-shipped volume its candidates hold. Expect both candidates to
/// let their sealed copy of its log go once the destroy's tombstone reaches them: nothing of a destroyed volume is kept
/// on any host (a holder kept it forever before, an unbounded store of what no owner will ship to again).
#[test]
fn a_destroyed_live_volumes_copies_are_let_go_by_its_candidates() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping destroy flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let fleet = start_fleet("live-destroy");
  let owner = &fleet.instances[(fleet.bootstrap + 1) % fleet.instances.len()];
  let others: Vec<&String> = fleet
    .instances
    .iter()
    .filter(|instance| *instance != owner)
    .collect();
  let id = create_live_volume(owner, "destroyed");
  write_files(owner, &id, 0..FILES);
  let (caught_up, _, _) = wait_caught_up(owner, &others, &id);
  assert!(caught_up, "both candidates held the log before the destroy");
  let (code, _, err) = run(owner, &["volume", "destroy", &id]);
  assert_eq!(code, 0, "destroy: {err}");
  let deadline = Instant::now() + FLEET_WAIT;
  let held = loop {
    let held: Vec<Option<LiveLine>> = others
      .iter()
      .map(|other| live_line(other, "held", &id))
      .collect();
    if held.iter().all(Option::is_none) || Instant::now() >= deadline {
      break held;
    }
    pause();
  };
  let shipping = live_line(owner, "shipping", &id);
  drop(fleet.daemons);
  drop(fleet.scratch);
  assert!(
    held.iter().all(Option::is_none),
    "the candidates let the destroyed volume's log go: {held:?}"
  );
  assert!(
    shipping.is_none(),
    "the owner let the destroyed volume's log go too: {shipping:?}"
  );
}

/// A-200 under clients cut off mid-write: do start `ROUNDS` writes and kill each client process at once (SIGKILL),
/// between finished writes. Expect the owner's log durable at its head with nothing owed, every candidate at that head,
/// and every reply that waited for durability resolved (none left held for a client that is gone).
#[test]
fn clients_cut_off_mid_write_leave_the_live_log_whole() {
  /// Shape: the writes whose clients are killed.
  const ROUNDS: usize = 10;
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping disconnect flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let fleet = start_fleet("live-cut");
  let owner = &fleet.instances[(fleet.bootstrap + 1) % fleet.instances.len()];
  let others: Vec<&String> = fleet
    .instances
    .iter()
    .filter(|instance| *instance != owner)
    .collect();
  let id = create_live_volume(owner, "cut");
  for round in 0..ROUNDS {
    let attachment = value_of(&run_ready(owner, &["attach", &id, "--write"]), "attachment");
    let mut child = slates()
      .arg("--instance")
      .arg(owner)
      .args(["write", &id, &attachment, &format!("/cut-{round}")])
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap();
    {
      use std::io::Write;
      let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(live_body(round).as_bytes());
    }
    let _ = child.kill();
    let _ = child.wait();
    write_files(owner, &id, round..round + 1);
  }
  let (caught_up, shipping, held) = wait_caught_up(owner, &others, &id);
  let awaited = counter(owner, "fleet.live.awaited");
  let resolved = counter(owner, "fleet.live.resolved");
  let unrecorded = counter(owner, "fleet.live.unrecorded");
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!(
    "live cut: {shipping:?} {held:?} awaited={awaited} resolved={resolved} unrecorded={unrecorded}"
  );
  assert!(
    caught_up,
    "the log is whole on every candidate: {shipping:?} {held:?}"
  );
  assert_eq!(
    awaited,
    resolved + unrecorded,
    "every held reply was resolved"
  );
}

/// A-200 and StaleNeverCommits across processes: do write to a live volume, stop its owner's process until the
/// survivors have taken the volume over, write more on the successor, then resume the old owner and try to write
/// through it. Expect the successor to serve every write acknowledged before and after the takeover, and the old
/// owner's write refused within the wait, never acknowledged and never left unanswered: a superseded owner is fenced,
/// and its waiting writes are told so. Until 2026-11-05 the resumed owner held the write unanswered for the whole 40 s
/// wait: the fence dropped its shipper with the write still waiting on it.
#[test]
fn a_superseded_owner_resumed_after_its_takeover_is_fenced() {
  /// Shape: how long the resumed owner's write is given before it counts as not acknowledged: the fleet wait.
  const STALE_WRITE_WAIT: Duration = FLEET_WAIT;
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping fencing flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let fleet = start_fleet("live-fence");
  let owner_index = (fleet.bootstrap + 1) % fleet.instances.len();
  let owner = fleet.instances[owner_index].clone();
  let id = create_live_volume(&owner, "fenced");
  write_files(&owner, &id, 0..3);
  signal(&fleet.daemons[owner_index], rustix::process::Signal::STOP);
  let survivors: Vec<&String> = fleet
    .instances
    .iter()
    .filter(|instance| **instance != owner)
    .collect();
  let (successor, took) = wait_served_whole(&survivors, &id, 3);
  write_files(&successor, &id, 3..5);
  signal(&fleet.daemons[owner_index], rustix::process::Signal::CONT);
  // The old owner's write: a fresh attachment through it, then the write. A refused attach is already a fenced owner
  // (it cannot take the write at all); one it grants must not lead to an acknowledged write.
  let (attach_code, attached, attach_err) = run(&owner, &["attach", &id, "--write"]);
  let attachment = if attach_code == 0 {
    value_of(&attached, "attachment")
  } else {
    eprintln!(
      "live fence: the resumed old owner refused the attach: {}",
      attach_err.trim()
    );
    "0".to_owned()
  };
  let mut stale = slates()
    .arg("--instance")
    .arg(&owner)
    .args(["write", &id, &attachment, "/stale"])
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  {
    use std::io::Write;
    let _ = stale
      .stdin
      .take()
      .unwrap()
      .write_all(b"written through a superseded owner");
  }
  let began = Instant::now();
  let stale_outcome = loop {
    if let Some(status) = stale.try_wait().unwrap() {
      break Some(status.code());
    }
    if began.elapsed() >= STALE_WRITE_WAIT {
      let _ = stale.kill();
      let _ = stale.wait();
      break None;
    }
    pause();
  };
  let (still, _) = wait_served_whole(&[&successor], &id, 5);
  let (code, out, _) = run(&successor, &["read", &id, "/stale"]);
  eprintln!(
    "live fence: the old owner's live counters {:?}, its log {:?}",
    live_counters(&owner),
    live_line(&owner, "shipping", &id)
  );
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!(
    "live fence: taken over by {successor} after {took:?}; stale write outcome {stale_outcome:?}; successor read of \
     /stale: {code} {out:?}"
  );
  assert_eq!(still, successor);
  assert!(
    matches!(stale_outcome, Some(Some(code)) if code != 0),
    "the superseded owner refused the write within the wait, not acknowledged it nor left it unanswered (None): \
     {stale_outcome:?}"
  );
  assert!(
    code != 0 || out.is_empty(),
    "the superseded owner's write never reached the volume: {out:?}"
  );
}

/// A stable-write NFS session to a fresh volume's export on `owner`: the stream, the file, and the volume's id.
fn stable_session(owner: &str, name: &str, live: bool) -> (std::net::TcpStream, Vec<u8>, String) {
  let id = value_of(
    &run_ready(owner, &["volume", "create", name, "--bounded", "64MiB"]),
    "id",
  );
  if live {
    let (code, _, err) = run(owner, &["live-ship", &id]);
    assert_eq!(code, 0, "live-ship: {err}");
  }
  let (code, out, err) = run(owner, &["export", &id, "--json"]);
  assert_eq!(code, 0, "export: {err}");
  let (path, port) = (json_field(&out, "path"), json_field(&out, "port"));
  let mut stream =
    std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
  stream.set_read_timeout(Some(FLEET_WAIT)).unwrap();
  stream.set_nodelay(true).unwrap();
  let root = nfs::mount(&mut stream, &path, 1);
  let file = nfs::create(&mut stream, &root, "timed", 2);
  (stream, file, id)
}

/// Nearest-rank percentiles of `samples` in microseconds: p50, p99, p999, max.
fn percentiles_us(samples: &mut [Duration]) -> (f64, f64, f64, f64) {
  samples.sort();
  let at = |per_mille: usize| {
    let rank = (samples.len() * per_mille).div_ceil(1000).max(1) - 1;
    samples[rank].as_secs_f64() * 1e6
  };
  (
    at(500),
    at(990),
    at(999),
    samples[samples.len() - 1].as_secs_f64() * 1e6,
  )
}

/// A-200's cost inside the daemon, measured (not a gate): on one owner process, two volumes, one plain and one
/// live-shipped, each sent 4 KiB `FILE_SYNC` NFS writes by a real NFS client over TCP, in interleaved rounds. A stable
/// write to the plain volume is answered after the owner's own barrier; to the live one, also after a candidate holds
/// it. Prints each one's p50, p99, p999 and max; the difference is what replication adds per stable write. Run it
/// release (`cargo test --release`), at `SLATES_TEST_CLI=1` and `SLATES_TEST_MEASURE=1`.
#[test]
fn measure_what_live_shipping_adds_to_a_stable_nfs_write() {
  /// Shape: interleaved rounds, and the writes of each volume per round.
  const ROUNDS: usize = 5;
  /// Shape: see [`ROUNDS`].
  const PER_ROUND: usize = 400;
  /// Shape: each write's bytes, a page.
  const WRITE_BYTES: usize = 4096;
  if std::env::var_os("SLATES_TEST_CLI").is_none()
    || std::env::var_os("SLATES_TEST_MEASURE").is_none()
  {
    eprintln!(
      "skipping the live-shipping measurement: set SLATES_TEST_CLI=1 and SLATES_TEST_MEASURE=1 (release)"
    );
    return;
  }
  let fleet = start_fleet("live-measure");
  let owner = &fleet.instances[(fleet.bootstrap + 1) % fleet.instances.len()];
  let (mut plain, plain_file, _) = stable_session(owner, "plain", false);
  let (mut live, live_file, live_id) = stable_session(owner, "live", true);
  let data = vec![0x5a_u8; WRITE_BYTES];
  let mut xid = 100u32;
  // Warm both: sessions, the live log's first whole record, the candidates' install.
  for _ in 0..50 {
    xid += 1;
    nfs::write(&mut plain, &plain_file, &data, xid);
    xid += 1;
    nfs::write(&mut live, &live_file, &data, xid);
  }
  let (mut plain_times, mut live_times) = (Vec::new(), Vec::new());
  for _ in 0..ROUNDS {
    for (stream, file, times) in [
      (&mut plain, &plain_file, &mut plain_times),
      (&mut live, &live_file, &mut live_times),
    ] {
      for _ in 0..PER_ROUND {
        xid += 1;
        let began = Instant::now();
        nfs::write(stream, file, &data, xid);
        times.push(began.elapsed());
      }
    }
  }
  let caught = live_line(owner, "shipping", &live_id);
  let late = counter(owner, "fleet.live.durable_late");
  let lent = (
    counter(owner, "fleet.live.no_session"),
    counter(owner, "fleet.live.unanswered"),
    counter(owner, "fleet.live.link_lost"),
  );
  eprintln!(
    "measure: the owner's live counters {:?}",
    live_counters(owner)
  );
  let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
  drop((plain, live));
  drop(fleet.daemons);
  drop(fleet.scratch);
  let (p50, p99, p999, max) = percentiles_us(&mut plain_times);
  let (l50, l99, l999, lmax) = percentiles_us(&mut live_times);
  eprintln!(
    "measure: {} writes each of {WRITE_BYTES} B FILE_SYNC, {ROUNDS} interleaved rounds; load {}\n  plain p50 {p50:.1} \
     p99 {p99:.1} p999 {p999:.1} max {max:.1} us\n  live  p50 {l50:.1} p99 {l99:.1} p999 {l999:.1} max {lmax:.1} us\n  \
     added at p50 {:.1} us; live log at the end {caught:?}; late {late}; (no_session, unanswered, link_lost) {lent:?}; \
     live writes over 50 ms {}",
    plain_times.len(),
    load.trim(),
    l50 - p50,
    live_times
      .iter()
      .filter(|time| **time > Duration::from_millis(50))
      .count()
  );
  assert_eq!(late, 0, "no stable write waited out its bound");
}

/// A-200, AC-8.3 ("live-shipped volumes report none") through a FUSE mount: do mount a live-shipped volume, write and
/// `fsync` a file, then stop both candidates' processes (SIGSTOP), write and `fsync` again, resume them and `fsync` once
/// more. Expect the first `fsync` to succeed, the one made with no candidate able to hold the write to fail (`EIO`)
/// rather than claim durability, and the last to succeed once the candidates are back. Until 2026-11-05 FUSE answered
/// `fsync` at the owner's own barrier, so the second one succeeded with nothing off the owner.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::disallowed_methods)]
fn a_fuse_fsync_on_a_live_volume_answers_only_once_a_candidate_holds_it() {
  use std::io::Write;
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping FUSE fsync flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  if !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("skipping the live-shipping FUSE fsync flow: no /dev/fuse on this host");
    return;
  }
  let fleet = start_fleet("live-fsync");
  let owner_index = (fleet.bootstrap + 1) % fleet.instances.len();
  let owner = fleet.instances[owner_index].clone();
  let id = create_live_volume(&owner, "fsynced");
  let mount = format!("{}/mount", fleet.scratch.path);
  std::fs::create_dir_all(&mount).unwrap();
  let (code, _, err) = run(&owner, &["mount", &id, &mount]);
  assert_eq!(code, 0, "mount: {err}");
  let path = format!("{mount}/synced");
  let mut file = std::fs::File::create(&path).unwrap();
  file.write_all(b"held by a candidate").unwrap();
  let healthy = file.sync_all();
  let candidates: Vec<usize> = (0..fleet.daemons.len())
    .filter(|index| *index != owner_index)
    .collect();
  for index in &candidates {
    signal(&fleet.daemons[*index], rustix::process::Signal::STOP);
  }
  file.write_all(b", then by no one").unwrap();
  let began = Instant::now();
  let alone = file.sync_all();
  let alone_took = began.elapsed();
  for index in &candidates {
    signal(&fleet.daemons[*index], rustix::process::Signal::CONT);
  }
  let resumed = file.sync_all();
  drop(file);
  let unmounted = Command::new("fusermount3").args(["-u", &mount]).status();
  if !unmounted.is_ok_and(|status| status.success()) {
    let _ = Command::new("fusermount3")
      .args(["-u", "-z", &mount])
      .status();
  }
  let counters = live_counters(&owner);
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!(
    "live fsync: healthy {healthy:?}; candidates stopped {alone:?} after {alone_took:?}; resumed {resumed:?}; \
     owner's live counters {counters:?}"
  );
  assert!(
    healthy.is_ok(),
    "an fsync a candidate holds succeeds: {healthy:?}"
  );
  assert_eq!(
    alone.as_ref().err().and_then(std::io::Error::raw_os_error),
    Some(rustix::io::Errno::IO.raw_os_error()),
    "an fsync no candidate can hold fails EIO, not success: {alone:?}"
  );
  assert!(
    resumed.is_ok(),
    "once the candidates are back, fsync succeeds: {resumed:?}"
  );
}

/// The survivor that takes `volume` over when `owner` dies, as the configuration ranks it: the rendezvous winner for the
/// volume's object among the other members (`RegionalConfiguration::successor` over a three-node fleet, whose recovery
/// cohort is every survivor).
fn successor_of(fleet: &Fleet, owner: &str, volume: &str) -> String {
  let mut bytes = [0u8; 16];
  for (byte, pair) in bytes.iter_mut().zip(volume.as_bytes().chunks(2)) {
    *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
  }
  let mut survivors: Vec<(slates_db::register::HostId, String)> = fleet
    .instances
    .iter()
    .filter(|instance| *instance != owner)
    .map(|instance| {
      let (host, _, _) = fleet_view(instance).unwrap();
      (
        slates_db::register::HostId(host.parse().unwrap()),
        instance.clone(),
      )
    })
    .collect();
  survivors.sort_by_key(|(host, _)| host.0);
  let hosts: Vec<slates_db::register::HostId> = survivors.iter().map(|(host, _)| *host).collect();
  let first =
    slates_db::register::rendezvous_first(&hosts, slates_db::register::ObjectId(bytes)).unwrap();
  survivors
    .into_iter()
    .find(|(host, _)| *host == first)
    .map(|(_, instance)| instance)
    .unwrap()
}

/// A-200 phase one with a successor that holds no copy of the log: do create a live volume, stop the process of the
/// candidate that will take it over (the configuration's successor for the volume) while the owner writes, each write
/// durable on the owner and the other candidate, then kill the owner (SIGKILL) and resume the successor. Expect the
/// successor to take the volume over and serve every acknowledged write: its own empty report counts toward phase one's
/// `f + 1`, since any `f + 1` of the `2f` other candidates include a holder, and it raises its fence. Found 2026-11-05
/// under load: a successor that held nothing never counted itself and waited for its quorum without end
/// (`fleet.live.takeover_waits_quorum` 370 and 371, the volume never served; Linux and macOS).
#[test]
fn a_successor_that_holds_no_copy_still_takes_a_live_volume_over() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live-shipping empty-successor flow: set SLATES_TEST_CLI=1 to run it (three daemons)"
    );
    return;
  }
  let mut fleet = start_fleet("live-empty");
  let owner_index = (fleet.bootstrap + 1) % fleet.instances.len();
  let owner = fleet.instances[owner_index].clone();
  let id = create_live_volume(&owner, "empty-successor");
  let successor = successor_of(&fleet, &owner, &id);
  let successor_index = fleet
    .instances
    .iter()
    .position(|instance| *instance == successor)
    .unwrap();
  signal(
    &fleet.daemons[successor_index],
    rustix::process::Signal::STOP,
  );
  // The writes are durable on the owner and the other candidate; given the fleet wait, so a candidate that refuses
  // the owner fails the test with its counters rather than hanging it (one run hung 58 minutes, 2026-11-05).
  let (done_tx, done_rx) = std::sync::mpsc::channel();
  let written = std::thread::scope(|scope| {
    let (owner, id) = (owner.clone(), id.clone());
    scope.spawn(move || {
      write_files(&owner, &id, 0..FILES);
      let _ = done_tx.send(());
    });
    let written = done_rx.recv_timeout(FLEET_WAIT).is_ok();
    if !written {
      let counters: Vec<_> = fleet
        .instances
        .iter()
        .map(|instance| (instance.clone(), live_counters(instance)))
        .collect();
      // Resumed, the stopped candidate lets the writes finish, so the scope can end.
      signal(
        &fleet.daemons[successor_index],
        rustix::process::Signal::CONT,
      );
      eprintln!("live empty successor: writes not durable with {successor} stopped: {counters:?}");
    }
    written
  });
  assert!(
    written,
    "with the successor stopped, every write became durable on the owner and the other candidate"
  );
  drop(fleet.daemons.remove(owner_index));
  let successor_index = if successor_index > owner_index {
    successor_index - 1
  } else {
    successor_index
  };
  signal(
    &fleet.daemons[successor_index],
    rustix::process::Signal::CONT,
  );
  let survivors: Vec<String> = fleet
    .instances
    .iter()
    .filter(|instance| **instance != owner)
    .cloned()
    .collect();
  let served = std::panic::catch_unwind(|| {
    let refs: Vec<&String> = survivors.iter().collect();
    wait_served_whole(&refs, &id, FILES).0
  })
  .ok();
  let counters: Vec<_> = survivors
    .iter()
    .map(|survivor| (survivor.clone(), live_counters(survivor)))
    .collect();
  drop(fleet.daemons);
  drop(fleet.scratch);
  eprintln!("live empty successor {successor}: served by {served:?}; {counters:?}");
  assert_eq!(
    served.as_ref(),
    Some(&successor),
    "the successor, which held no copy, served every acknowledged write: {counters:?}"
  );
}
