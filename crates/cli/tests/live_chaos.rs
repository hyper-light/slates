//! Chaos across real processes for replicated volumes (§4.10, A-200; Ada's goal of 2026-11-04: replicas of the filesystem
//! server, replicated per write in under 1 ms, chaos-tested). Three fleet daemons from one manifest form an `f = 1` fleet
//! over loopback UDP; each flow streams writes through a real client while it knocks a process offline (SIGSTOP, the
//! unprivileged partition; SIGKILL of a daemon under its anchor, which starts a successor with a fresh identity), fails a
//! candidate, or cuts a client off, and every flow ends with the register invariants that the nemesis model of
//! `crates/db/tests/live.rs` checks inside one process, here checked from outside the processes:
//!
//! - **Agreement**: two copies of a volume's log at one epoch and position carry one chain identity. Every node's `live`
//!   status lines are sampled throughout the run ([`Sampler`]); the identity binds every entry before the head.
//! - **NoLoss**: every write a client was told succeeded is served byte for byte at the end, by whichever node serves the
//!   volume then; and no file is ever served torn (a write not acknowledged is absent or whole).
//! - **StaleNeverCommits**: once a volume has been taken over, every copy of its log that answers is fenced at or above
//!   the serving owner's epoch, and a write made through the superseded owner is refused, never acknowledged.
//! - **Convergence**: once healed, every candidate holds the owner's head (epoch, position and identity), and the owner's
//!   log is durable at its head with nothing owed.
//!
//! Each flow prints the client-seen latency of its writes (p50, p99, p999, max). The fleet helpers are copies of
//! `live_fleet.rs`'s (where they are private), extended with anchored nodes, so a daemon can be killed and succeeded.
//! Gated like the other process flows (`SLATES_TEST_CLI=1`). Never run with a kernel NFS mount of these daemons live:
//! every client here is userspace.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_client::{Client, Deadlines, Intent, ReadAt, VolumeId};

/// Shape: how long a fleet step may take on a loaded box: the formation wait of the process flows.
const FLEET_WAIT: Duration = Duration::from_secs(40);
/// Shape: how long a daemon may take to answer once started (or succeeded under its anchor).
const START_WAIT: Duration = Duration::from_secs(30);
/// Shape: the pause between polls.
const POLL_MS: u64 = 20;
/// Shape: shards per daemon: two, so a volume's owner shard need not be the control shard, where the sessions are.
const SHARDS: &str = "2";
/// Shape: the fleet's TLS name.
const FLEET_NAME: &str = "slates-fleet";
/// Shape: the nodes, in manifest order.
const FLEET_NODES: [&str; 3] = ["a", "b", "c"];
/// Shape: the fleet's fault tolerance: three nodes at `f = 1`.
const FLEET_F: usize = 1;
/// Shape: the ports a node serves on (`slates_server::deploy`): its base and the next.
const PORTS_PER_NODE: u16 = 2;
/// Shape: the port range searched for each node's block, from a pid-derived start.
const PORT_FLOOR: u16 = 20_000;
/// Shape: see [`PORT_FLOOR`].
const PORT_SPAN: u16 = 30_000;
/// Shape: candidate bases tried per block.
const PORT_ATTEMPTS: u16 = 2_000;
/// Shape: each streamed write's bytes, a page.
const WRITE_BYTES: usize = 4096;
/// Shape: a file's mode.
const FILE_MODE: u32 = 0o644;

// ── Processes ────────────────────────────────────────────────────────────────────────────────────────────────────────

/// A fleet node's process (a bare daemon, or an anchor supervising one): killed and reaped when dropped, so a failed
/// assertion leaves nothing behind.
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

fn sleep(duration: Duration) {
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(duration);
}

/// One directory per fleet: the tests run in parallel in one process (`live_fleet.rs`, 2026-11-05).
fn scratch_dir(tag: &str) -> ScratchDir {
  let raw = format!(
    "{}/slates-live-chaos-{tag}-{}",
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
  // Offset from `live_fleet.rs`'s start (which multiplies the pid by 7), so the two suites run side by side.
  let offset = (std::process::id().wrapping_mul(13)) % u32::from(PORT_SPAN);
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

/// How a node's daemon runs: bare (the process is the daemon, so a SIGSTOP of it is a partition), or under its anchor
/// (a daemon killed is succeeded by a fresh one with a fresh member identity, as a pod's container restart is).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Supervision {
  Bare,
  Anchored,
}

fn start_node(
  instance: &str,
  manifest: &str,
  node: &str,
  held: &HeldBlock,
  supervision: Supervision,
) -> FleetProcess {
  use std::os::fd::AsRawFd;
  let [probe, record] = held.sockets.as_slice() else {
    panic!("a node's block is its probe and record ports");
  };
  let (probe, record) = (inheritable(probe), inheritable(record));
  let verb = match supervision {
    Supervision::Bare => "daemon",
    Supervision::Anchored => "anchor",
  };
  let child = slates()
    .args([
      "--instance",
      instance,
      verb,
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

/// Waits until the fleet is whole on every node: all three members voting, all three in the committed configuration,
/// and a record session to each peer (`live_fleet.rs`'s `wait_settled`, which says why).
fn wait_settled(instances: &[String]) {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let views: Vec<(String, String, usize)> = instances
      .iter()
      .map(|instance| {
        let (_, out, _) = run(instance, &["status"]);
        let sessions = out
          .lines()
          .filter(|line| line.starts_with("fleet_session "))
          .count();
        (
          out
            .lines()
            .find_map(|line| line.strip_prefix("fleet_council_voters: "))
            .unwrap_or("?")
            .to_owned(),
          out
            .lines()
            .find_map(|line| line.strip_prefix("fleet_configuration_members: "))
            .unwrap_or("?")
            .to_owned(),
          sessions,
        )
      })
      .collect();
    let whole = views.iter().all(|(voters, members, sessions)| {
      voters.trim() == FLEET_NODES.len().to_string()
        && members.trim() == FLEET_NODES.len().to_string()
        && *sessions == FLEET_NODES.len() - 1
    });
    if whole {
      return;
    }
    assert!(
      Instant::now() < deadline,
      "the fleet settled (council voters, configuration members, record sessions): {views:?}"
    );
    pause();
  }
}

/// Every counter `instance` reports under `prefix`, summed over its shards, by name.
fn counters(instance: &str, prefix: &str) -> BTreeMap<String, u64> {
  let (_, status, _) = run(instance, &["status"]);
  let needle = format!(" refused {prefix}");
  let mut counts = BTreeMap::<String, u64>::new();
  for (kind, count) in status
    .lines()
    .filter_map(|line| line.split_once(&needle).map(|(_, rest)| rest))
    .filter_map(|rest| rest.split_once(": "))
  {
    *counts.entry(format!("{prefix}{kind}")).or_default() +=
      count.trim().parse::<u64>().unwrap_or(0);
  }
  counts
}

fn counter(instance: &str, kind: &str) -> u64 {
  counters(instance, kind).get(kind).copied().unwrap_or(0)
}

/// Runs `args` on `instance` until it is not refused for an unready consensus, within the fleet wait.
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

/// The daemon's pid, from `status --json`, or `None` while no daemon answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, out, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  let after = out.split("\"pid\":").nth(1)?;
  after
    .trim_start()
    .split(|c: char| !c.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

fn send_signal(pid: u32, signal: rustix::process::Signal) {
  let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
  let _ = rustix::process::kill_process(pid, signal);
}

/// A started fleet: its scratch directory, manifest, each node's held ports, process and instance, and the node
/// bootstrapped.
struct Fleet {
  /// Held for its `Drop`: the manifest and identities live in it until the fleet ends.
  _scratch: ScratchDir,
  manifest: String,
  blocks: Vec<HeldBlock>,
  processes: Vec<Option<FleetProcess>>,
  instances: Vec<String>,
  supervision: Supervision,
  bootstrap: usize,
}

impl Fleet {
  /// Starts three nodes from one manifest, waits until they form, and bootstraps the root once.
  fn start(tag: &str, supervision: Supervision) -> Fleet {
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
      .map(|node| format!("chaos-{tag}-{node}-{pid}"))
      .collect();
    let processes = FLEET_NODES
      .iter()
      .zip(&instances)
      .zip(&blocks)
      .map(|((node, instance), held)| {
        Some(start_node(instance, &manifest, node, held, supervision))
      })
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
    wait_settled(&instances);
    eprintln!("chaos: fleet {tag} ({supervision:?}) formed and settled: hosts {hosts:?}");
    DIAGNOSED.with(|diagnosed| *diagnosed.borrow_mut() = instances.clone());
    Fleet {
      _scratch: scratch,
      manifest,
      blocks,
      processes,
      instances,
      supervision,
      bootstrap,
    }
  }

  /// The node index that is not the bootstrap node: owners are made there, as `live_fleet.rs` does.
  fn owner_index(&self) -> usize {
    (self.bootstrap + 1) % self.instances.len()
  }

  fn others(&self, index: usize) -> Vec<usize> {
    (0..self.instances.len())
      .filter(|other| *other != index)
      .collect()
  }

  /// The pid that a SIGSTOP partitions or a SIGKILL crashes: the bare daemon's own, or the anchored node's daemon.
  fn daemon_pid(&self, index: usize) -> u32 {
    match self.supervision {
      Supervision::Bare => self.processes[index].as_ref().unwrap().child.id(),
      Supervision::Anchored => daemon_pid(&self.instances[index]).unwrap(),
    }
  }

  fn stop(&self, index: usize) {
    send_signal(self.daemon_pid(index), rustix::process::Signal::STOP);
  }

  fn resume(&self, index: usize) {
    send_signal(self.daemon_pid(index), rustix::process::Signal::CONT);
  }

  /// Kills node `index` whole (its anchor too): what a crashed machine is.
  fn kill_node(&mut self, index: usize) {
    drop(self.processes[index].take());
  }

  /// Kills an anchored node's daemon (SIGKILL) and waits until its anchor's successor answers; the successor's pid.
  fn kill_daemon_and_await_successor(&self, index: usize) -> u32 {
    assert_eq!(self.supervision, Supervision::Anchored);
    let instance = &self.instances[index];
    let killed = daemon_pid(instance).unwrap();
    send_signal(killed, rustix::process::Signal::KILL);
    let deadline = Instant::now() + START_WAIT;
    loop {
      if let Some(pid) = daemon_pid(instance)
        && pid != killed
      {
        return pid;
      }
      assert!(
        Instant::now() < deadline,
        "the anchor at {instance} started a successor daemon"
      );
      pause();
    }
  }

  /// Starts node `index` afresh after [`Fleet::kill_node`]: a replacement with no state (a new anchor, so a new
  /// member identity), as a replaced machine or pod is.
  fn replace_node(&mut self, index: usize) {
    assert!(self.processes[index].is_none());
    self.processes[index] = Some(start_node(
      &self.instances[index],
      &self.manifest,
      FLEET_NODES[index],
      &self.blocks[index],
      self.supervision,
    ));
    wait_answers(&self.instances[index]);
  }

  /// Waits until every live node holds exactly three members, each probed by the others: a succeeded or replaced
  /// node rejoined under its new identity and the old one retired.
  fn wait_rejoined(&self) {
    let live: Vec<String> = (0..self.instances.len())
      .filter(|index| self.processes[*index].is_some())
      .map(|index| self.instances[index].clone())
      .collect();
    wait_formed(&live);
    wait_settled(&live);
  }
}

impl Drop for Fleet {
  fn drop(&mut self) {
    // Resume anything stopped, so the kill reaches a process that can be reaped at once.
    for process in self.processes.iter().flatten() {
      send_signal(process.child.id(), rustix::process::Signal::CONT);
    }
    for process in &mut self.processes {
      drop(process.take());
    }
  }
}

// ── Volumes and clients ──────────────────────────────────────────────────────────────────────────────────────────────

fn volume_id_of(hex: &str) -> VolumeId {
  let mut bytes = [0u8; 16];
  for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
    *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
  }
  VolumeId { bytes }
}

/// Creates a volume on `owner` (live-shipped when `live`): its id.
fn create_volume(owner: &str, name: &str, live: bool) -> String {
  let id = value_of(
    &run_ready(owner, &["volume", "create", name, "--bounded", "64MiB"]),
    "id",
  );
  if live {
    let (code, out, err) = run(owner, &["live-ship", &id]);
    assert_eq!(code, 0, "live-ship: {err}");
    assert!(out.contains("live-shipped"), "{out}");
  }
  id
}

fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

/// The bytes streamed to `/chaos-N`: a header naming the file, then a pattern of it, [`WRITE_BYTES`] in all, so a torn
/// or misplaced write cannot pass for another file's.
fn chaos_body(file: usize) -> Vec<u8> {
  let mut body = format!("chaos write {file}\n").into_bytes();
  let seed = u8::try_from(file % 251).unwrap();
  body.extend(
    (0..WRITE_BYTES.saturating_sub(body.len()))
      .map(|at| seed.wrapping_add(u8::try_from(at % 256).unwrap())),
  );
  body
}

fn chaos_path(file: usize) -> String {
  format!("chaos-{file}")
}

/// One streamed write as its client saw it.
#[derive(Debug, Clone)]
struct Outcome {
  file: usize,
  /// When it was sent, from the flow's start.
  sent: Duration,
  took: Duration,
  /// `None`: acknowledged. Otherwise what the client was told (or that it was never told).
  refused: Option<String>,
}

/// A client streaming writes to one volume through one node, as fast as each is acknowledged.
struct Stream<'a> {
  instance: &'a str,
  volume: VolumeId,
  began: Instant,
}

impl Stream<'_> {
  /// Connects and attaches for writing, retrying within the fleet wait (a node just succeeded or taking over).
  fn attach(&self) -> Option<(Client, u64)> {
    let deadline = Instant::now() + FLEET_WAIT;
    while Instant::now() < deadline {
      if let Ok(mut client) = Client::connect(self.instance, deadlines())
        && let Ok(attached) = client.attach(self.volume, None, Intent::Write)
      {
        return Some((client, attached.attachment));
      }
      pause();
    }
    None
  }

  /// Writes `/chaos-N` for each N in `files` until `stop` is set, each once its previous one is answered. A refused
  /// write is recorded and the stream reconnects (the session may be what failed) and goes on with the next file.
  fn run(&self, files: std::ops::Range<usize>, stop: &AtomicBool) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    let Some((mut client, mut attachment)) = self.attach() else {
      return outcomes;
    };
    for file in files {
      if stop.load(Ordering::Acquire) {
        break;
      }
      let body = chaos_body(file);
      let sent = self.began.elapsed();
      let started = Instant::now();
      let written = client.fs_write(
        (self.volume, attachment),
        &chaos_path(file),
        &body,
        FILE_MODE,
      );
      let took = started.elapsed();
      let refused = written.err().map(|error| error.to_string());
      let failed = refused.is_some();
      outcomes.push(Outcome {
        file,
        sent,
        took,
        refused,
      });
      if failed {
        let Some((again, again_attachment)) = self.attach() else {
          return outcomes;
        };
        (client, attachment) = (again, again_attachment);
      }
    }
    let _ = client.detach(attachment);
    outcomes
  }
}

/// Shape: a write slower than this is listed by when it was sent: tens of times the quiet median (3–4 ms here, debug).
const SLOW_WRITE: Duration = Duration::from_millis(50);

/// Nearest-rank percentiles in microseconds: p50, p99, p999, max.
fn percentiles_us(samples: &[Duration]) -> (f64, f64, f64, f64) {
  if samples.is_empty() {
    return (0.0, 0.0, 0.0, 0.0);
  }
  let mut sorted = samples.to_vec();
  sorted.sort();
  let at = |per_mille: usize| {
    let rank = (sorted.len() * per_mille).div_ceil(1000).max(1) - 1;
    sorted[rank].as_secs_f64() * 1e6
  };
  (
    at(500),
    at(990),
    at(999),
    sorted[sorted.len() - 1].as_secs_f64() * 1e6,
  )
}

fn latency_line(outcomes: &[Outcome]) -> String {
  let acknowledged: Vec<Duration> = outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_none())
    .map(|outcome| outcome.took)
    .collect();
  let (p50, p99, p999, max) = percentiles_us(&acknowledged);
  let slow: Vec<String> = outcomes
    .iter()
    .filter(|outcome| outcome.took > SLOW_WRITE)
    .map(|outcome| {
      format!(
        "{:.2}s+{:.0}ms",
        outcome.sent.as_secs_f64(),
        outcome.took.as_secs_f64() * 1e3
      )
    })
    .collect();
  format!(
    "{} writes, {} acknowledged: p50 {p50:.0} us, p99 {p99:.0} us, p999 {p999:.0} us, max {max:.0} us; {} over \
     {SLOW_WRITE:?} (sent+took): {}",
    outcomes.len(),
    acknowledged.len(),
    slow.len(),
    slow.join(" ")
  )
}

// ── The register invariants, from outside the processes ──────────────────────────────────────────────────────────────

/// One `live` status line of one node.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveState {
  holding: bool,
  head: Option<u64>,
  durable: Option<u64>,
  owed: bool,
  epoch: Option<u64>,
  identity: Option<String>,
  fence: u64,
}

fn field<'a>(fields: &'a str, key: &str) -> Option<&'a str> {
  fields
    .split_whitespace()
    .find_map(|field| field.strip_prefix(key))
}

fn number(text: Option<&str>) -> Option<u64> {
  text.and_then(|text| text.parse().ok())
}

/// Every `live` line `instance` reports for `volume` (a successor both holds a copy and ships), or `None` while it does
/// not answer.
fn live_states(instance: &str, volume: &str) -> Option<Vec<LiveState>> {
  let (code, out, _) = run(instance, &["status"]);
  if code != 0 {
    return None;
  }
  Some(
    out
      .lines()
      .filter_map(|line| {
        let (_, rest) = line.split_once(" live ")?;
        let (role, rest) = rest.split_once(' ')?;
        let fields = rest.strip_prefix(volume)?;
        Some(LiveState {
          holding: role == "held",
          head: number(field(fields, "head=")),
          durable: number(field(fields, "durable=")),
          owed: field(fields, "owed=") == Some("true"),
          epoch: number(field(fields, "epoch=")),
          identity: field(fields, "identity=")
            .filter(|identity| *identity != "none")
            .map(str::to_owned),
          fence: number(field(fields, "fence=")).unwrap_or(0),
        })
      })
      .collect(),
  )
}

/// One sample: when, which node, and what it reported.
#[derive(Clone, Debug)]
struct Sample {
  at: Duration,
  node: usize,
  state: LiveState,
}

/// Samples every node's `live` lines for one volume until stopped, so Agreement is checked at every head any copy
/// reported during the run, not only at the end.
struct Sampler<'a> {
  instances: &'a [String],
  volume: &'a str,
  began: Instant,
}

impl Sampler<'_> {
  fn run(&self, stop: &AtomicBool) -> Vec<Sample> {
    let mut samples = Vec::new();
    while !stop.load(Ordering::Acquire) {
      samples.extend(self.once());
      pause();
    }
    samples.extend(self.once());
    samples
  }

  fn once(&self) -> Vec<Sample> {
    let mut samples = Vec::new();
    for (node, instance) in self.instances.iter().enumerate() {
      let at = self.began.elapsed();
      for state in live_states(instance, self.volume).unwrap_or_default() {
        samples.push(Sample { at, node, state });
      }
    }
    samples
  }
}

/// Agreement: at every (epoch, position) any copy reported, one chain identity. The heads seen, and the violations.
fn agreement(samples: &[Sample]) -> (usize, Vec<String>) {
  // Each identity with the first node and time it was seen at, so a split names who held which.
  let mut seen: BTreeMap<(u64, u64), BTreeMap<String, (usize, Duration)>> = BTreeMap::new();
  for sample in samples {
    if let (Some(epoch), Some(head), Some(identity)) = (
      sample.state.epoch,
      sample.state.head,
      &sample.state.identity,
    ) {
      seen
        .entry((epoch, head))
        .or_default()
        .entry(identity.clone())
        .or_insert((sample.node, sample.at));
    }
  }
  let violations = seen
    .iter()
    .filter(|(_, identities)| identities.len() > 1)
    .map(|((epoch, head), identities)| {
      format!("epoch {epoch} position {head}: (identity, (node, first seen)) {identities:?}")
    })
    .collect();
  (seen.len(), violations)
}

/// NoLoss and no torn file: through `reader`, every acknowledged write is served byte for byte, and every other file
/// streamed is absent or whole. The violations.
fn no_loss(reader: &str, volume: &str, outcomes: &[Outcome]) -> Vec<String> {
  let mut client = Client::connect(reader, deadlines()).unwrap();
  let id = volume_id_of(volume);
  let mut violations = Vec::new();
  for outcome in outcomes {
    let read = client.read(id, &format!("/{}", chaos_path(outcome.file)), ReadAt::Head);
    let expected = chaos_body(outcome.file);
    match (&read, &outcome.refused) {
      (Ok(bytes), _) if *bytes == expected => {}
      (Ok(bytes), _) => violations.push(format!(
        "{} served torn or foreign: {} bytes, starts {:?}",
        chaos_path(outcome.file),
        bytes.len(),
        String::from_utf8_lossy(bytes.get(..24).unwrap_or(bytes))
      )),
      (Err(error), None) => violations.push(format!(
        "{} acknowledged at {:?} and lost: {error}",
        chaos_path(outcome.file),
        outcome.sent
      )),
      (Err(_), Some(_)) => {}
    }
  }
  violations
}

/// Convergence: waits until the owner's log is durable at its head with nothing owed and every candidate holds that
/// head (same epoch, position and identity), within the fleet wait. What each reported last, and whether it converged.
fn converged(owner: &str, candidates: &[&str], volume: &str) -> (bool, String) {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let shipping = live_states(owner, volume)
      .unwrap_or_default()
      .into_iter()
      .find(|state| !state.holding);
    let held: Vec<Option<LiveState>> = candidates
      .iter()
      .map(|candidate| {
        live_states(candidate, volume)
          .unwrap_or_default()
          .into_iter()
          .find(|state| state.holding)
      })
      .collect();
    let whole = shipping.as_ref().is_some_and(|owner| {
      owner.head.is_some()
        && owner.head == owner.durable
        && !owner.owed
        && held.iter().all(|kept| {
          kept.as_ref().is_some_and(|kept| {
            kept.head == owner.head && kept.epoch == owner.epoch && kept.identity == owner.identity
          })
        })
    });
    if whole || Instant::now() >= deadline {
      let mut line = format!("owner {shipping:?}; candidates {held:?}");
      if !whole {
        for instance in std::iter::once(&owner).chain(candidates) {
          line.push_str(&format!(
            "\n    {instance}: {:?}",
            counters(instance, "fleet.live.")
          ));
        }
      }
      return (whole, line);
    }
    pause();
  }
}

/// StaleNeverCommits, the log's side: every copy of the volume's log that answers on `nodes` is fenced at or above the
/// serving owner's epoch `epoch`. The violations.
fn fenced_at_or_above(instances: &[&str], volume: &str, epoch: u64) -> Vec<String> {
  instances
    .iter()
    .flat_map(|instance| {
      live_states(instance, volume)
        .unwrap_or_default()
        .into_iter()
        .filter(|state| state.holding && state.fence < epoch)
        .map(move |state| format!("{instance} holds a copy fenced below {epoch}: {state:?}"))
    })
    .collect()
}

/// The register invariants a flow ends with, printed whole; panics naming every violation.
struct Verdict {
  flow: &'static str,
  violations: Vec<String>,
  notes: Vec<String>,
}

impl Verdict {
  fn new(flow: &'static str) -> Verdict {
    Verdict {
      flow,
      violations: Vec::new(),
      notes: Vec::new(),
    }
  }

  fn check(&mut self, invariant: &str, violations: Vec<String>) {
    self.notes.push(format!(
      "{invariant}: {}",
      if violations.is_empty() {
        "held".to_owned()
      } else {
        format!("{} violations", violations.len())
      }
    ));
    self.violations.extend(
      violations
        .into_iter()
        .map(|violation| format!("{invariant}: {violation}")),
    );
  }

  /// Records `note` for the verdict and prints it at once, so a flow that fails part-way shows how far it got.
  fn note(&mut self, note: String) {
    eprintln!("chaos {}: {note}", self.flow);
    self.notes.push(note);
  }

  fn judge(self) {
    eprintln!("chaos {}:\n  {}", self.flow, self.notes.join("\n  "));
    assert!(
      self.violations.is_empty(),
      "chaos {}: {}",
      self.flow,
      self.violations.join("\n")
    );
  }
}

fn gated(flow: &str) -> bool {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!("skipping the chaos flow {flow}: set SLATES_TEST_CLI=1 to run it (three daemons)");
    return false;
  }
  true
}

/// Streams writes on `owner`'s volume, sampling every node, while `chaos` runs on this thread with the time since the
/// start; stops the stream once `chaos` returns and `tail` more writes have been made. The outcomes and the samples.
fn stream_through<F: FnOnce(&Instant)>(
  instances: &[String],
  owner: &str,
  volume: &str,
  files: std::ops::Range<usize>,
  chaos: F,
) -> (Vec<Outcome>, Vec<Sample>) {
  let began = Instant::now();
  let stop_stream = AtomicBool::new(false);
  let stop_sampling = AtomicBool::new(false);
  std::thread::scope(|scope| {
    let sampler = scope.spawn(|| {
      Sampler {
        instances,
        volume,
        began,
      }
      .run(&stop_sampling)
    });
    let streamer = scope.spawn(|| {
      Stream {
        instance: owner,
        volume: volume_id_of(volume),
        began,
      }
      .run(files, &stop_stream)
    });
    // Both threads stop however the chaos ends: a failed assertion in it unwinds through this guard, and the scope,
    // which joins its threads before it passes a panic on, would otherwise wait on threads that never stop.
    struct StopAll<'a>(&'a AtomicBool, &'a AtomicBool);
    impl Drop for StopAll<'_> {
      fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        self.1.store(true, Ordering::Release);
      }
    }
    let stop = StopAll(&stop_stream, &stop_sampling);
    // The chaos's own failure is passed on only after the stream is in hand, or a write never answered would hang the
    // scope's join behind it (one run did until its timeout, 2026-11-06).
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| chaos(&began))).err();
    drop(stop);
    // A write the daemon never answers would hang the join for good (one run hung 20 minutes, 2026-11-06): past the
    // fleet wait, the fleet is shown whole and its daemons killed, so the client is told and the flow fails.
    let stopped = Instant::now();
    while !streamer.is_finished() {
      if stopped.elapsed() > FLEET_WAIT {
        eprintln!(
          "chaos: a write on {owner} was never answered within {FLEET_WAIT:?} of the stream's stop:\n{}",
          diagnosis(instances, volume)
        );
        UNANSWERED.with(|unanswered| unanswered.set(true));
        for instance in instances {
          if let Some(pid) = daemon_pid(instance) {
            send_signal(pid, rustix::process::Signal::KILL);
          }
        }
        break;
      }
      pause();
    }
    let outcomes = streamer.join().unwrap();
    if let Some(failure) = failed {
      std::panic::resume_unwind(failure);
    }
    assert!(
      !UNANSWERED.with(std::cell::Cell::get),
      "a write on {owner} was never answered (the fleet's state is printed above): {:?}",
      outcomes.last()
    );
    (outcomes, sampler.join().unwrap())
  })
}

/// Lets the stream run until `count` writes have been acknowledged since `from`, within the fleet wait (the chaos
/// thread's way to put an event mid-stream). Polls the owner's log head, which every acknowledged write moves.
fn after_writes(owner: &str, volume: &str, count: u64) {
  let head_now = || {
    live_states(owner, volume)
      .unwrap_or_default()
      .into_iter()
      .find(|state| !state.holding)
      .and_then(|state| state.durable)
  };
  let from = head_now().unwrap_or(0);
  let deadline = Instant::now() + FLEET_WAIT;
  while head_now().unwrap_or(0) < from + count {
    if Instant::now() >= deadline {
      let instances = DIAGNOSED.with(|instances| instances.borrow().clone());
      panic!(
        "the stream made {count} writes durable on {owner} (from {from}, now {:?}):\n{}",
        head_now(),
        diagnosis(&instances, volume)
      );
    }
    pause();
  }
}

/// Every node's sessions, live logs and live counters, as `status` prints them: what a stalled flow shows.
fn diagnosis(instances: &[String], volume: &str) -> String {
  instances
    .iter()
    .map(|instance| {
      let (code, status, _) = run(instance, &["status"]);
      let lines: Vec<&str> = status
        .lines()
        .filter(|line| {
          line.starts_with("fleet_session ")
            || line.contains(" live ")
            || line.contains("fleet.live.")
            || line.contains("lent_to")
        })
        .collect();
      format!(
        "--- {instance} (status exit {code}; live {:?}):\n  {}",
        live_states(instance, volume),
        lines.join("\n  ")
      )
    })
    .collect::<Vec<_>>()
    .join("\n")
}

thread_local! {
  /// Whether a stream's write was never answered (set by [`stream_through`] before it kills the fleet's daemons).
  static UNANSWERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// The instances of the fleet a flow on this thread runs, for a failed wait's diagnosis.
  static DIAGNOSED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

// ── Flows ────────────────────────────────────────────────────────────────────────────────────────────────────────────

/// Shape: the most files a stream writes; a stream is stopped by its flow well before this.
const STREAM_FILES: usize = 100_000;
/// Shape: durable writes between chaos events, so each event lands mid-stream with writes on either side.
const BETWEEN_EVENTS: u64 = 40;

/// A-200 with a candidate killed mid-stream and succeeded: do stream writes to a live volume, SIGKILL one candidate's
/// daemon in the middle (its anchor starts a successor under a fresh identity, holding no copy of the log), keep
/// streaming, then kill the owner's whole node. Expect every write acknowledged during the stream, before, across and
/// after the kill, to be durable on the surviving candidate, the succeeded candidate to catch up (an install, then the
/// entries after it), and the survivors' takeover to serve every acknowledged write: the succeeded candidate's copy
/// counts toward phase one only once it holds the log again.
#[test]
fn a_candidate_killed_mid_stream_catches_up_and_its_copy_serves_a_takeover() {
  if !gated("candidate-killed") {
    return;
  }
  let mut fleet = Fleet::start("killed", Supervision::Anchored);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let killed_index = fleet.others(owner_index)[0];
  let volume = create_volume(&owner, "killed-candidate", true);
  let mut verdict = Verdict::new("a candidate killed mid-stream");
  let instances = fleet.instances.clone();
  let (outcomes, samples) = stream_through(&instances, &owner, &volume, 0..STREAM_FILES, |began| {
    after_writes(&owner, &volume, BETWEEN_EVENTS);
    let killed_at = began.elapsed();
    let successor = fleet.kill_daemon_and_await_successor(killed_index);
    verdict.note(format!(
      "candidate {} killed at {killed_at:?}; successor daemon {successor} answered at {:?}",
      fleet.instances[killed_index],
      began.elapsed()
    ));
    fleet.wait_rejoined();
    verdict.note(format!("the fleet rejoined at {:?}", began.elapsed()));
    after_writes(&owner, &volume, BETWEEN_EVENTS);
  });
  verdict.note(latency_line(&outcomes));
  let candidates: Vec<&str> = fleet
    .others(owner_index)
    .into_iter()
    .map(|index| instances[index].as_str())
    .collect();
  let (caught_up, last) = converged(&owner, &candidates, &volume);
  verdict.note(format!("converged: {caught_up}: {last}"));
  verdict.check(
    "Convergence",
    if caught_up { Vec::new() } else { vec![last] },
  );
  let (heads, disagreements) = agreement(&samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement", disagreements);
  verdict.check("NoLoss (owner)", no_loss(&owner, &volume, &outcomes));
  // The owner's whole node lost: the survivors, one of them the succeeded candidate, take the volume over.
  fleet.kill_node(owner_index);
  let survivors: Vec<&str> = candidates.clone();
  let served = wait_serving(&survivors, &volume, &outcomes);
  verdict.note(format!("after the owner's loss: {served:?}"));
  match served {
    Some((successor, _)) => {
      verdict.check("NoLoss (successor)", no_loss(successor, &volume, &outcomes));
      let epoch = takeover_epoch(successor, &volume);
      verdict.check(
        "StaleNeverCommits (fences)",
        fenced_at_or_above(&survivors, &volume, epoch),
      );
    }
    None => verdict.check(
      "NoLoss (successor)",
      vec![format!(
        "no survivor served the volume within the fleet wait: {:?}",
        survivors
          .iter()
          .map(|survivor| (*survivor, counters(survivor, "fleet.live.")))
          .collect::<Vec<_>>()
      )],
    ),
  }
  drop(fleet);
  verdict.judge();
}

/// The epoch `instance` ships `volume` under (0 when it ships none).
fn serving_epoch(instance: &str, volume: &str) -> u64 {
  live_states(instance, volume)
    .unwrap_or_default()
    .into_iter()
    .find(|state| !state.holding)
    .map_or(0, |state| state.fence)
}

/// The epoch a successor took `volume` over under: the one it ships under, or, before its first publication, the fence
/// its own copy was raised to by phase one.
fn takeover_epoch(successor: &str, volume: &str) -> u64 {
  live_states(successor, volume)
    .unwrap_or_default()
    .into_iter()
    .map(|state| state.fence)
    .max()
    .unwrap_or(0)
}

/// Waits until one of `survivors` serves `volume` with every acknowledged write in `outcomes` (taken over); which, and
/// after how long. `None` at the fleet wait.
fn wait_serving<'a>(
  survivors: &[&'a str],
  volume: &str,
  outcomes: &[Outcome],
) -> Option<(&'a str, Duration)> {
  let began = Instant::now();
  let id = volume_id_of(volume);
  let acknowledged: Vec<usize> = outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_none())
    .map(|outcome| outcome.file)
    .collect();
  while began.elapsed() < FLEET_WAIT {
    for survivor in survivors {
      let Ok(mut client) = Client::connect(survivor, deadlines()) else {
        continue;
      };
      let serves = acknowledged.last().is_none_or(|last| {
        client
          .read(id, &format!("/{}", chaos_path(*last)), ReadAt::Head)
          .is_ok_and(|bytes| bytes == chaos_body(*last))
      });
      if serves
        && (counter(survivor, "fleet.live.taken_over") > 0 || serving_epoch(survivor, volume) > 0)
      {
        return Some((survivor, began.elapsed()));
      }
    }
    pause();
  }
  None
}

/// The candidates of `owner_index` as instance names.
fn candidates_of(fleet: &Fleet, owner_index: usize) -> Vec<String> {
  fleet
    .others(owner_index)
    .into_iter()
    .map(|index| fleet.instances[index].clone())
    .collect()
}

/// Ends a flow on a fleet whose owner still serves: Convergence, Agreement over the samples, NoLoss on the owner.
fn judge_healed(
  verdict: &mut Verdict,
  owner: &str,
  candidates: &[String],
  volume: &str,
  outcomes: &[Outcome],
  samples: &[Sample],
) {
  verdict.note(latency_line(outcomes));
  let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
  let (caught_up, last) = converged(owner, &refs, volume);
  verdict.note(format!("converged: {caught_up}: {last}"));
  verdict.check(
    "Convergence",
    if caught_up { Vec::new() } else { vec![last] },
  );
  let (heads, disagreements) = agreement(samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement", disagreements);
  verdict.check("NoLoss", no_loss(owner, volume, outcomes));
  // Each node's refusals of the owner's messages by why, passing or not: a candidate refusing every entry for want of
  // the volume's catalog (`no_catalog`) was much of an install storm that a passing flow hid (live-durability,
  // 2026-11-06).
  for instance in std::iter::once(owner).chain(candidates.iter().map(String::as_str)) {
    verdict.note(format!(
      "{instance} refused the owner: {:?}",
      counters(instance, "fleet.live.hold_refused.")
    ));
  }
}

/// A-200 with candidates partitioned in turn mid-stream: do stream writes to a live volume and stop each candidate's
/// process (SIGSTOP) for [`PARTITION`] in turn, the stream running throughout, then resume it. Expect every write
/// acknowledged (durable through whichever candidate answers), none lost, both candidates caught up once healed, and one
/// chain identity at every head any copy reported.
#[test]
fn candidates_partitioned_in_turn_mid_stream_lose_nothing() {
  /// Shape: how long each candidate is partitioned: past the liveness budget (1 s), so its sessions are found dead.
  const PARTITION: Duration = Duration::from_secs(2);
  if !gated("rolling-partition") {
    return;
  }
  let fleet = Fleet::start("rolling", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = create_volume(&owner, "rolling", true);
  let mut verdict = Verdict::new("candidates partitioned in turn mid-stream");
  let (outcomes, samples) = stream_through(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began| {
      for index in fleet.others(owner_index) {
        after_writes(&owner, &volume, BETWEEN_EVENTS);
        fleet.stop(index);
        let stopped = began.elapsed();
        sleep(PARTITION);
        fleet.resume(index);
        verdict.note(format!(
          "{} partitioned from {stopped:?} to {:?}",
          fleet.instances[index],
          began.elapsed()
        ));
      }
      after_writes(&owner, &volume, BETWEEN_EVENTS);
    },
  );
  let refused: Vec<&Outcome> = outcomes.iter().filter(|o| o.refused.is_some()).collect();
  verdict.check(
    "every write acknowledged",
    refused.iter().map(|o| format!("{o:?}")).collect(),
  );
  judge_healed(
    &mut verdict,
    &owner,
    &candidates,
    &volume,
    &outcomes,
    &samples,
  );
  drop(fleet);
  verdict.judge();
}

/// A-200 under injected jitter: do stream writes to a live volume while each candidate's process is stopped for a
/// random [`STALL_MIN`]–[`STALL_MAX`] at random gaps of up to [`GAP_MAX`], the two independently (so at times both are
/// stopped and no write can become durable until one resumes). Expect every write acknowledged and none lost, both
/// candidates caught up, one identity at every head; the client-seen p50, p99, p999 and max are printed.
#[test]
fn writes_stream_through_random_candidate_stalls() {
  /// Shape: the shortest stall: a scheduling hiccup.
  const STALL_MIN: Duration = Duration::from_millis(1);
  /// Shape: the longest stall: a congested path's queueing, well under the liveness budget.
  const STALL_MAX: Duration = Duration::from_millis(40);
  /// Shape: the longest gap between one candidate's stalls.
  const GAP_MAX: Duration = Duration::from_millis(60);
  /// Shape: how long the stalls go on.
  const STALLING: Duration = Duration::from_secs(8);
  if !gated("jitter") {
    return;
  }
  let fleet = Fleet::start("jitter", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = create_volume(&owner, "jitter", true);
  let mut verdict = Verdict::new("writes through random candidate stalls");
  let pids: Vec<u32> = fleet
    .others(owner_index)
    .into_iter()
    .map(|index| fleet.daemon_pid(index))
    .collect();
  let mut stalls = 0u64;
  let (outcomes, samples) =
    stream_through(&fleet.instances, &owner, &volume, 0..STREAM_FILES, |_| {
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      stalls = std::thread::scope(|scope| {
        let handles: Vec<_> = pids
          .iter()
          .enumerate()
          .map(|(lane, pid)| {
            scope.spawn(move || {
              // A seeded generator a lane, so a failing run's schedule is the same run again.
              let mut seed = 0x9e37_79b9_7f4a_7c15_u64 ^ u64::try_from(lane).unwrap();
              let mut next = move |bound: Duration| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                Duration::from_nanos(seed % u64::try_from(bound.as_nanos()).unwrap().max(1))
              };
              let started = Instant::now();
              let mut count = 0u64;
              while started.elapsed() < STALLING {
                sleep(next(GAP_MAX));
                send_signal(*pid, rustix::process::Signal::STOP);
                sleep(STALL_MIN + next(STALL_MAX - STALL_MIN));
                send_signal(*pid, rustix::process::Signal::CONT);
                count += 1;
              }
              count
            })
          })
          .collect();
        handles
          .into_iter()
          .map(|handle| handle.join().unwrap())
          .sum()
      });
      after_writes(&owner, &volume, BETWEEN_EVENTS);
    });
  verdict.note(format!("{stalls} stalls over {STALLING:?}"));
  let refused: Vec<&Outcome> = outcomes.iter().filter(|o| o.refused.is_some()).collect();
  verdict.check(
    "every write acknowledged",
    refused.iter().map(|o| format!("{o:?}")).collect(),
  );
  judge_healed(
    &mut verdict,
    &owner,
    &candidates,
    &volume,
    &outcomes,
    &samples,
  );
  drop(fleet);
  verdict.judge();
}

/// The survivor among `survivors` that ships `volume` (has taken it over), once one does, within the fleet wait.
fn wait_taken_over<'a>(survivors: &[&'a str], volume: &str) -> Option<(&'a str, Duration)> {
  let began = Instant::now();
  while began.elapsed() < FLEET_WAIT {
    // The successor counts its rebuild from the live log; it ships only from its next publication on.
    if let Some(successor) = survivors.iter().find(|survivor| {
      counter(survivor, "fleet.live.taken_over") > 0 || serving_epoch(survivor, volume) > 0
    }) {
      return Some((successor, began.elapsed()));
    }
    pause();
  }
  None
}

/// A-200 and StaleNeverCommits with the owner partitioned mid-stream: do stream writes to a live volume, stop the
/// owner's process in the middle of the stream until a survivor has taken the volume over, stream more writes through
/// the successor, then resume the old owner, whose stream goes on. Expect every write acknowledged by either owner to
/// be served by the successor, every write the old owner was sent after it resumed to be refused (it is fenced), every
/// surviving copy fenced at or above the successor's epoch, and one identity at every head.
#[test]
fn an_owner_partitioned_mid_stream_is_fenced_and_loses_nothing() {
  if !gated("owner-partitioned") {
    return;
  }
  let fleet = Fleet::start("owner-stop", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let survivors: Vec<&str> = candidates.iter().map(String::as_str).collect();
  let volume = create_volume(&owner, "owner-stop", true);
  let mut verdict = Verdict::new("an owner partitioned mid-stream");
  let mut resumed_at = Duration::ZERO;
  let mut successor_outcomes = Vec::new();
  let mut successor = None;
  let (outcomes, samples) = stream_through(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began| {
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      fleet.stop(owner_index);
      let stopped = began.elapsed();
      let taken = wait_taken_over(&survivors, &volume);
      verdict.note(format!(
        "owner stopped at {stopped:?}; taken over: {taken:?}"
      ));
      if let Some((taken_by, _)) = taken {
        successor = Some(taken_by.to_owned());
        let stop = AtomicBool::new(false);
        let second = std::thread::scope(|scope| {
          let streamer = scope.spawn(|| {
            Stream {
              instance: taken_by,
              volume: volume_id_of(&volume),
              began: *began,
            }
            .run(STREAM_FILES..2 * STREAM_FILES, &stop)
          });
          after_writes(taken_by, &volume, BETWEEN_EVENTS);
          stop.store(true, Ordering::Release);
          streamer.join().unwrap()
        });
        successor_outcomes = second;
      }
      fleet.resume(owner_index);
      resumed_at = began.elapsed();
      // Long enough for the old owner's stream to send several writes after it resumed.
      sleep(Duration::from_secs(2));
    },
  );
  verdict.note(format!(
    "old owner: {}; successor: {}",
    latency_line(&outcomes),
    latency_line(&successor_outcomes)
  ));
  let stale_acknowledged: Vec<String> = outcomes
    .iter()
    .filter(|outcome| outcome.sent > resumed_at && outcome.refused.is_none())
    .map(|outcome| format!("{outcome:?}"))
    .collect();
  let sent_after = outcomes
    .iter()
    .filter(|outcome| outcome.sent > resumed_at)
    .count();
  verdict.note(format!(
    "{sent_after} writes sent to the old owner after it resumed at {resumed_at:?}"
  ));
  verdict.check(
    "StaleNeverCommits (no write acknowledged through the old owner)",
    stale_acknowledged,
  );
  let Some(successor) = successor else {
    verdict.check(
      "takeover",
      vec!["no survivor took the volume over".to_owned()],
    );
    drop(fleet);
    return verdict.judge();
  };
  let (heads, disagreements) = agreement(&samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement", disagreements);
  let every: Vec<Outcome> = outcomes
    .iter()
    .chain(&successor_outcomes)
    .cloned()
    .collect();
  verdict.check("NoLoss (successor)", no_loss(&successor, &volume, &every));
  let epoch = takeover_epoch(&successor, &volume);
  verdict.note(format!("taken over under epoch {epoch}"));
  let mut all: Vec<&str> = vec![owner.as_str()];
  all.extend(&survivors);
  verdict.check(
    "StaleNeverCommits (fences)",
    fenced_at_or_above(&all, &volume, epoch),
  );
  let others: Vec<String> = fleet
    .instances
    .iter()
    .filter(|instance| **instance != successor)
    .cloned()
    .collect();
  let refs: Vec<&str> = others.iter().map(String::as_str).collect();
  let (caught_up, last) = converged(&successor, &refs, &volume);
  verdict.note(format!("converged on the successor: {caught_up}: {last}"));
  verdict.check(
    "Convergence",
    if caught_up { Vec::new() } else { vec![last] },
  );
  drop(fleet);
  verdict.judge();
}

/// A-200 with a live volume destroyed while writes wait on it: do stream writes, stop both candidates' processes so the
/// stream's next write waits for durability, destroy the volume on the owner, then resume them. Expect the waiting write
/// refused (never acknowledged, never left unanswered), the owner and both candidates holding nothing of the volume
/// afterwards, and every reply that waited resolved.
#[test]
fn a_live_volume_destroyed_with_writes_waiting_refuses_them_and_keeps_nothing() {
  if !gated("destroyed-waiting") {
    return;
  }
  let fleet = Fleet::start("destroy-wait", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = create_volume(&owner, "destroy-wait", true);
  let mut verdict = Verdict::new("a live volume destroyed with writes waiting");
  let mut destroyed_at = Duration::ZERO;
  let (outcomes, samples) = stream_through(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began| {
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      for index in fleet.others(owner_index) {
        fleet.stop(index);
      }
      // Let the stream's write be sent and wait.
      sleep(Duration::from_millis(200));
      // The destroy as its own client, since it now waits for its tombstone, which no stopped candidate can hold.
      let mut destroy = slates()
        .arg("--instance")
        .arg(&owner)
        .args(["volume", "destroy", &volume])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
      destroyed_at = began.elapsed();
      sleep(Duration::from_millis(200));
      let answered_while_stopped = destroy.try_wait().unwrap().map(|status| status.code());
      for index in fleet.others(owner_index) {
        fleet.resume(index);
      }
      let code = destroy.wait_timeout_or_kill(FLEET_WAIT).unwrap_or(-1);
      let mut err = String::new();
      {
        use std::io::Read;
        let _ = destroy.stderr.take().unwrap().read_to_string(&mut err);
      }
      verdict.note(format!(
        "destroy sent at {destroyed_at:?}: answered while the candidates were stopped {answered_while_stopped:?}; \
         exit {code} at {:?} {}",
        began.elapsed(),
        err.trim()
      ));
      // Until 2026-11-06 the destroy itself waited on the write before it and was then refused `NotFound` by its own
      // teardown, which refuses what waits on a destroyed volume: a client was told a destroy that happened had not.
      verdict.check(
        "the destroy answered done",
        if code == 0 {
          Vec::new()
        } else {
          vec![format!("exit {code}: {}", err.trim())]
        },
      );
    },
  );
  verdict.note(latency_line(&outcomes));
  let waiting: Vec<&Outcome> = outcomes
    .iter()
    .filter(|outcome| outcome.sent < destroyed_at && outcome.sent + outcome.took > destroyed_at)
    .collect();
  verdict.note(format!("writes waiting across the destroy: {waiting:?}"));
  verdict.check(
    "a write waiting across the destroy is refused",
    waiting
      .iter()
      .filter(|outcome| outcome.refused.is_none())
      .map(|outcome| format!("{outcome:?}"))
      .collect(),
  );
  verdict.check(
    "nothing acknowledged after the destroy",
    outcomes
      .iter()
      .filter(|outcome| outcome.sent > destroyed_at && outcome.refused.is_none())
      .map(|outcome| format!("{outcome:?}"))
      .collect(),
  );
  let (heads, disagreements) = agreement(&samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement", disagreements);
  let deadline = Instant::now() + FLEET_WAIT;
  let kept = loop {
    let kept: Vec<(String, Vec<LiveState>)> = fleet
      .instances
      .iter()
      .map(|instance| {
        (
          instance.clone(),
          live_states(instance, &volume).unwrap_or_default(),
        )
      })
      .filter(|(_, states)| !states.is_empty())
      .collect();
    if kept.is_empty() || Instant::now() >= deadline {
      break kept;
    }
    pause();
  };
  verdict.check(
    "nothing of the destroyed volume kept",
    kept.iter().map(|kept| format!("{kept:?}")).collect(),
  );
  // Every wait the owner holds, a live write's or a destroy's, ends through one answer, counted `resolved`.
  let awaited = counter(&owner, "fleet.live.awaited") + counter(&owner, "fleet.destroy.awaited");
  let resolved = counter(&owner, "fleet.live.resolved");
  let unrecorded = counter(&owner, "fleet.live.unrecorded");
  verdict.note(format!(
    "awaited {awaited} resolved {resolved} unrecorded {unrecorded}; candidates {:?}",
    candidates
      .iter()
      .map(|candidate| counters(candidate, "fleet.live."))
      .collect::<Vec<_>>()
  ));
  verdict.check(
    "every waiting reply resolved",
    if awaited == resolved + unrecorded {
      Vec::new()
    } else {
      vec![format!(
        "awaited {awaited} != resolved {resolved} + unrecorded {unrecorded}"
      )]
    },
  );
  drop(fleet);
  verdict.judge();
}

/// A-200 with clients killed while their writes wait for durability: do stop both candidates, start `ROUNDS` client
/// processes that each write a file (each write parks, waiting for a candidate), kill each client (SIGKILL) while it
/// waits, then resume the candidates. Expect every parked reply resolved for a client that is gone, each file absent or
/// whole (never torn), the log converged on both candidates, one identity at every head, and a write made afterwards
/// acknowledged.
#[test]
fn clients_killed_while_their_writes_wait_leave_no_reply_held() {
  /// Shape: the clients killed while they wait.
  const ROUNDS: usize = 6;
  if !gated("clients-killed-waiting") {
    return;
  }
  let fleet = Fleet::start("cut-waiting", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = create_volume(&owner, "cut-waiting", true);
  let mut verdict = Verdict::new("clients killed while their writes wait");
  let began = Instant::now();
  let stop_sampling = AtomicBool::new(false);
  let samples = std::thread::scope(|scope| {
    let sampler = scope.spawn(|| {
      Sampler {
        instances: &fleet.instances,
        volume: &volume,
        began,
      }
      .run(&stop_sampling)
    });
    for round in 0..ROUNDS {
      let attachment = value_of(
        &run_ready(&owner, &["attach", &volume, "--write"]),
        "attachment",
      );
      for index in fleet.others(owner_index) {
        fleet.stop(index);
      }
      let mut child = slates()
        .arg("--instance")
        .arg(&owner)
        .args([
          "write",
          &volume,
          &attachment,
          &format!("/{}", chaos_path(round)),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
      {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(&chaos_body(round));
      }
      // The write parks once it reaches the owner; kill its client while it waits.
      let parked = Instant::now();
      while counter(&owner, "fleet.live.awaited") <= u64::try_from(round).unwrap()
        && parked.elapsed() < FLEET_WAIT
      {
        pause();
      }
      let _ = child.kill();
      let _ = child.wait();
      for index in fleet.others(owner_index) {
        fleet.resume(index);
      }
    }
    stop_sampling.store(true, Ordering::Release);
    sampler.join().unwrap()
  });
  let after = Stream {
    instance: &owner,
    volume: volume_id_of(&volume),
    began,
  };
  let never = AtomicBool::new(false);
  let outcomes: Vec<Outcome> = after.run(ROUNDS..ROUNDS + 3, &never);
  verdict.check(
    "a write after the cut clients acknowledged",
    outcomes
      .iter()
      .filter(|outcome| outcome.refused.is_some())
      .map(|outcome| format!("{outcome:?}"))
      .collect(),
  );
  // The cut writes: never told, so each is absent or whole.
  let cut: Vec<Outcome> = (0..ROUNDS)
    .map(|file| Outcome {
      file,
      sent: Duration::ZERO,
      took: Duration::ZERO,
      refused: Some("its client was killed".to_owned()),
    })
    .collect();
  let mut every = cut;
  every.extend(outcomes.iter().cloned());
  judge_healed(&mut verdict, &owner, &candidates, &volume, &every, &samples);
  // Every wait the owner holds, a live write's or a destroy's, ends through one answer, counted `resolved`.
  let awaited = counter(&owner, "fleet.live.awaited") + counter(&owner, "fleet.destroy.awaited");
  let resolved = counter(&owner, "fleet.live.resolved");
  let unrecorded = counter(&owner, "fleet.live.unrecorded");
  verdict.note(format!(
    "awaited {awaited} resolved {resolved} unrecorded {unrecorded}"
  ));
  verdict.check(
    "every waiting reply resolved",
    if awaited == resolved + unrecorded && awaited >= u64::try_from(ROUNDS).unwrap() {
      Vec::new()
    } else {
      vec![format!(
        "awaited {awaited}, resolved {resolved}, unrecorded {unrecorded}"
      )]
    },
  );
  drop(fleet);
  verdict.judge();
}

/// A destroy, then the owner lost (§4.4 destroy, AUD-29-43; a client is never told an outcome a takeover undoes): do
/// write to a volume (live-shipped, or plain with a snapshot), let every candidate hold it, destroy the volume on the
/// owner (with both candidates' processes stopped when `unheard`, so no candidate can hold its tombstone), kill the
/// owner's process (SIGKILL) a moment later, and resume the candidates. Expect a destroy answered done never undone (no
/// survivor lists or serves the volume again), a destroy that could not be placed never answered done, and a healthy
/// destroy answered done. Until 2026-11-06 the unheard destroy was answered done and a survivor served the volume
/// again after the takeover.
fn a_destroy_then_the_owners_loss(live: bool, unheard: bool) {
  let tag = match (live, unheard) {
    (true, true) => "destroy-loss-live",
    (true, false) => "destroy-loss-live-heard",
    (false, true) => "destroy-loss-plain",
    (false, false) => "destroy-loss-plain-heard",
  };
  let mut fleet = Fleet::start(tag, Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = create_volume(&owner, tag, live);
  let mut verdict = Verdict::new(match (live, unheard) {
    (true, true) => "a live volume's unheard destroy, then its owner's loss",
    (true, false) => "a live volume's destroy, then its owner's loss",
    (false, true) => "a plain volume's unheard destroy, then its owner's loss",
    (false, false) => "a plain volume's destroy, then its owner's loss",
  });
  let outcomes = held_before_destroy(&owner, &candidates, &volume, live, &mut verdict);
  let destroyed = destroy_across_the_loss(&mut fleet, owner_index, &volume, unheard);
  verdict.note(format!(
    "destroy (candidates stopped: {unheard}): answered before the owner's loss {:?}; exit {} {}",
    destroyed.before_the_loss,
    destroyed.code,
    destroyed.stderr.trim()
  ));
  // Give the survivors the time a takeover takes (1.8–3.2 s measured) twice over, then ask each for the volume.
  sleep(Duration::from_secs(8));
  let views = survivor_views(&candidates, &volume);
  verdict.note(format!(
    "written: {}; survivors: {views:?}",
    latency_line(&outcomes)
  ));
  judge_destroy(&mut verdict, unheard, &destroyed, &views);
  // A destroy never answered done may still take effect (its records reach the candidates once they resume), so the
  // volume must settle, within the fleet wait, into one of two states on every survivor: destroyed (none lists it,
  // each read refused NotFound), or served with every acknowledged write (NoLoss). Neither, and the fleet holds a
  // volume no one serves or destroys.
  if destroyed.code != 0 {
    let (settled, views) = wait_settled_after_destroy(&candidates, &volume, &outcomes, live);
    let outcome = match &settled {
      Some(Settled::Destroyed) => "destroyed on every survivor".to_owned(),
      Some(Settled::Served(by)) => format!("served whole by {by}"),
      None => "unsettled".to_owned(),
    };
    verdict.note(format!(
      "after the unanswered destroy: {outcome}; {views:?}"
    ));
    verdict.check(
      "an unanswered destroy settles: destroyed everywhere, or served whole",
      if settled.is_some() {
        Vec::new()
      } else {
        vec![format!(
          "unsettled after the fleet wait: {views:?}; {:?}",
          candidates
            .iter()
            .map(|candidate| (candidate.clone(), counters(candidate, "fleet.")))
            .collect::<Vec<_>>()
        )]
      },
    );
  }
  drop(fleet);
  verdict.judge();
}

/// Writes five files to `volume` on `owner` and waits until every candidate holds them (a live volume's log converged,
/// or a plain volume's snapshot placed): the writes' outcomes.
fn held_before_destroy(
  owner: &str,
  candidates: &[String],
  volume: &str,
  live: bool,
  verdict: &mut Verdict,
) -> Vec<Outcome> {
  let never = AtomicBool::new(false);
  let outcomes = Stream {
    instance: owner,
    volume: volume_id_of(volume),
    began: Instant::now(),
  }
  .run(0..5, &never);
  if live {
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let (caught_up, last) = converged(owner, &refs, volume);
    assert!(
      caught_up,
      "the candidates held the log before the destroy: {last}"
    );
  } else {
    let (code, out, err) = run(owner, &["volume", "snapshot", volume]);
    verdict.note(format!(
      "snapshot: exit {code} {} {}",
      out.trim(),
      err.trim()
    ));
    // The snapshot placed at f + 1 (the record period, up to 100 ms, then its commit).
    sleep(Duration::from_secs(2));
  }
  outcomes
}

/// What a destroy's client was told.
struct Destroyed {
  /// Its exit code if it ended before the owner was lost (`Some(None)`: by a signal), `None` if it was still waiting.
  before_the_loss: Option<Option<i32>>,
  /// Its exit code in the end (-1: killed or signalled).
  code: i32,
  stderr: String,
}

/// Destroys `volume` through its own client process (with both candidates stopped when `unheard`), gives it a moment,
/// kills the owner's node, and resumes the candidates: what the client was told.
fn destroy_across_the_loss(
  fleet: &mut Fleet,
  owner_index: usize,
  volume: &str,
  unheard: bool,
) -> Destroyed {
  /// Shape: how long the destroy is given before the owner is lost: far past a healthy tombstone's placement (the
  /// record period and a loopback round), so a destroy the candidates can hold is answered by then.
  const ASKED_WAIT: Duration = Duration::from_secs(3);
  if unheard {
    for index in fleet.others(owner_index) {
      fleet.stop(index);
    }
  }
  let mut destroy = slates()
    .arg("--instance")
    .arg(&fleet.instances[owner_index])
    .args(["volume", "destroy", volume])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let asked = Instant::now();
  let before_the_loss = loop {
    if let Some(status) = destroy.try_wait().unwrap() {
      break Some(status.code());
    }
    if asked.elapsed() > ASKED_WAIT {
      break None;
    }
    pause();
  };
  fleet.kill_node(owner_index);
  if unheard {
    for index in fleet.others(owner_index) {
      fleet.resume(index);
    }
  }
  let code = match before_the_loss {
    Some(code) => code.unwrap_or(-1),
    // Its daemon is gone: the client is told so, or is killed here.
    None => destroy.wait_timeout_or_kill(FLEET_WAIT).unwrap_or(-1),
  };
  let mut stderr = String::new();
  {
    use std::io::Read;
    let _ = destroy.stderr.take().unwrap().read_to_string(&mut stderr);
  }
  Destroyed {
    before_the_loss,
    code,
    stderr,
  }
}

/// A survivor's view of a destroyed volume: whether it lists it, and what reading its first file through it gives.
#[derive(Debug)]
struct SurvivorView {
  instance: String,
  listed: bool,
  read: Option<Result<usize, String>>,
}

fn survivor_views(candidates: &[String], volume: &str) -> Vec<SurvivorView> {
  candidates
    .iter()
    .map(|candidate| {
      let (_, listed, _) = run(candidate, &["volume", "list"]);
      let read = Client::connect(candidate, deadlines())
        .ok()
        .map(|mut client| {
          client
            .read(
              volume_id_of(volume),
              &format!("/{}", chaos_path(0)),
              ReadAt::Head,
            )
            .map(|bytes| bytes.len())
            .map_err(|error| error.to_string())
        });
      SurvivorView {
        instance: candidate.clone(),
        listed: listed.contains(volume),
        read,
      }
    })
    .collect()
}

/// The destroy flow's checks: an answered destroy never undone, an unheard one never answered done, a heard one
/// answered done, and none told `NotFound`.
fn judge_destroy(
  verdict: &mut Verdict,
  unheard: bool,
  destroyed: &Destroyed,
  views: &[SurvivorView],
) {
  let resurrected: Vec<String> = views
    .iter()
    .filter(|view| view.listed || matches!(view.read, Some(Ok(_))))
    .map(|view| {
      format!(
        "{} serves the destroyed volume again: {view:?}",
        view.instance
      )
    })
    .collect();
  verdict.check(
    "a destroy answered done is never undone",
    if destroyed.code == 0 {
      resurrected
    } else {
      Vec::new()
    },
  );
  let answered_done = destroyed.before_the_loss == Some(Some(0));
  verdict.check(
    "a destroy no candidate could hold is never answered done",
    if unheard && answered_done {
      vec!["the unheard destroy exited 0 before the owner's loss".to_owned()]
    } else {
      Vec::new()
    },
  );
  verdict.check(
    "a destroy the candidates hold is answered done",
    if !unheard && !answered_done {
      vec![format!(
        "the healthy destroy: {:?}, exit {}",
        destroyed.before_the_loss, destroyed.code
      )]
    } else {
      Vec::new()
    },
  );
  verdict.check(
    "a destroy is never told NotFound",
    if destroyed.stderr.contains("NotFound") {
      vec![format!(
        "the destroy of an existing volume was answered: {}",
        destroyed.stderr.trim()
      )]
    } else {
      Vec::new()
    },
  );
}

#[test]
fn a_live_volumes_unheard_destroy_then_its_owners_loss() {
  if gated("destroy-loss-live") {
    a_destroy_then_the_owners_loss(true, true);
  }
}

#[test]
fn a_plain_volumes_unheard_destroy_then_its_owners_loss() {
  if gated("destroy-loss-plain") {
    a_destroy_then_the_owners_loss(false, true);
  }
}

#[test]
fn a_live_volumes_destroy_then_its_owners_loss() {
  if gated("destroy-loss-live-heard") {
    a_destroy_then_the_owners_loss(true, false);
  }
}

#[test]
fn a_plain_volumes_destroy_then_its_owners_loss() {
  if gated("destroy-loss-plain-heard") {
    a_destroy_then_the_owners_loss(false, false);
  }
}

/// Waits for a child within a bound, killing it past the bound: its exit code (`None`: killed or signalled).
trait WaitOrKill {
  fn wait_timeout_or_kill(&mut self, bound: Duration) -> Option<i32>;
}

impl WaitOrKill for Child {
  fn wait_timeout_or_kill(&mut self, bound: Duration) -> Option<i32> {
    let began = Instant::now();
    loop {
      if let Some(status) = self.try_wait().unwrap() {
        return status.code();
      }
      if began.elapsed() > bound {
        let _ = self.kill();
        let _ = self.wait();
        return None;
      }
      pause();
    }
  }
}

/// A-200 with a candidate's store destroyed mid-stream: do stream writes to a live volume, kill one candidate's whole
/// node (its anchor and daemon: everything it held is gone), keep streaming on the other candidate, then start a fresh
/// node in its place (a new anchor, so a new member identity, holding nothing), keep streaming, and finally kill the
/// owner's node. Expect every write acknowledged throughout, the replacement caught up once it rejoins, one identity at
/// every head, and the survivors' takeover serving every acknowledged write.
#[test]
fn a_candidates_store_destroyed_mid_stream_is_rebuilt_and_serves_a_takeover() {
  if !gated("store-destroyed") {
    return;
  }
  let mut fleet = Fleet::start("replaced", Supervision::Anchored);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let replaced_index = fleet.others(owner_index)[0];
  let volume = create_volume(&owner, "replaced-candidate", true);
  let mut verdict = Verdict::new("a candidate's store destroyed mid-stream");
  let instances = fleet.instances.clone();
  let fleet_cell = std::cell::RefCell::new(&mut fleet);
  let (outcomes, samples) = stream_through(&instances, &owner, &volume, 0..STREAM_FILES, |began| {
    after_writes(&owner, &volume, BETWEEN_EVENTS);
    fleet_cell.borrow_mut().kill_node(replaced_index);
    verdict.note(format!(
      "candidate node {} destroyed at {:?}",
      instances[replaced_index],
      began.elapsed()
    ));
    after_writes(&owner, &volume, BETWEEN_EVENTS);
    fleet_cell.borrow_mut().replace_node(replaced_index);
    verdict.note(format!("replacement answering at {:?}", began.elapsed()));
    fleet_cell.borrow().wait_rejoined();
    verdict.note(format!("the fleet rejoined at {:?}", began.elapsed()));
    after_writes(&owner, &volume, BETWEEN_EVENTS);
  });
  let candidates = candidates_of(&fleet, owner_index);
  let refused: Vec<String> = outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_some())
    .map(|outcome| format!("{outcome:?}"))
    .collect();
  verdict.check("every write acknowledged", refused);
  judge_healed(
    &mut verdict,
    &owner,
    &candidates,
    &volume,
    &outcomes,
    &samples,
  );
  fleet.kill_node(owner_index);
  let survivors: Vec<&str> = candidates.iter().map(String::as_str).collect();
  match wait_serving(&survivors, &volume, &outcomes) {
    Some((successor, took)) => {
      verdict.note(format!("taken over by {successor} after {took:?}"));
      verdict.check("NoLoss (successor)", no_loss(successor, &volume, &outcomes));
      let epoch = takeover_epoch(successor, &volume);
      verdict.check(
        "StaleNeverCommits (fences)",
        fenced_at_or_above(&survivors, &volume, epoch),
      );
    }
    None => verdict.check(
      "NoLoss (successor)",
      vec!["no survivor served the volume within the fleet wait".to_owned()],
    ),
  }
  drop(fleet);
  verdict.judge();
}

/// The control for every chaos flow's latency: do stream writes to a live volume on a fleet nothing disturbs, for
/// `QUIET` past the first durable writes. Expect every write acknowledged, none lost, both candidates converged, one
/// identity at every head; the client-seen p50, p99, p999 and max are this machine's floor for the flows above (and the
/// slow writes listed, by when they were sent, are what the machine itself costs).
#[test]
fn writes_stream_on_a_quiet_fleet() {
  /// Shape: how long the quiet stream runs.
  const QUIET: Duration = Duration::from_secs(10);
  if !gated("quiet") {
    return;
  }
  let fleet = Fleet::start("quiet", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = create_volume(&owner, "quiet", true);
  let mut verdict = Verdict::new("writes on a quiet fleet");
  let (outcomes, samples) =
    stream_through(&fleet.instances, &owner, &volume, 0..STREAM_FILES, |_| {
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      sleep(QUIET);
    });
  let refused: Vec<String> = outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_some())
    .map(|outcome| format!("{outcome:?}"))
    .collect();
  verdict.check("every write acknowledged", refused);
  judge_healed(
    &mut verdict,
    &owner,
    &candidates,
    &volume,
    &outcomes,
    &samples,
  );
  drop(fleet);
  verdict.judge();
}

/// The head version `green` reports through `instance`, or `None` while it does not answer.
fn green_head(instance: &str, green: &str) -> Option<u64> {
  let (code, out, _) = run(instance, &["versions", green]);
  if code != 0 {
    return None;
  }
  out
    .lines()
    .find_map(|line| line.strip_prefix("head: "))
    .and_then(|head| head.trim().parse().ok())
}

/// Whether `instance` serves each round's file at `green`'s head with the bytes that round's submit wrote.
fn green_files(instance: &str, green: &str, rounds: usize) -> BTreeMap<usize, bool> {
  (0..rounds)
    .map(|round| {
      let (code, out, _) = run(instance, &["read", green, &format!("/merge-{round}")]);
      (
        round,
        code == 0 && out == format!("submitted in round {round}"),
      )
    })
    .collect()
}

/// §4.16 and AUD-11 under clients cut off mid-merge, in a fleet: do create a green on one daemon process and, `ROUNDS`
/// times, make a work that adds one file, start a `slates submit` of it and SIGKILL that client a few milliseconds in
/// (before its request, while its acceptance waits for the fleet's commit, or after); then make one more submit whose
/// client lives, and finally kill the green's owner (SIGKILL). Expect: every version a client was told is in the
/// chain; the head counts exactly the files present (each cut submit is in whole or not at all, never a version without
/// its edit nor an edit without its version); and a survivor takes the green over with the same head and the same files.
#[test]
fn clients_cut_off_mid_merge_leave_the_green_whole_across_a_takeover() {
  /// Shape: the submits whose clients are killed.
  const ROUNDS: usize = 12;
  if !gated("cut-merge") {
    return;
  }
  let mut fleet = Fleet::start("cut-merge", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let mut verdict = Verdict::new("clients cut off mid-merge");
  let green = value_of(&run_ready(&owner, &["green", "cut-merge"]), "id");
  let base = green_head(&owner, &green).unwrap_or(0);
  let mut told: Vec<(usize, u64)> = Vec::new();
  for round in 0..=ROUNDS {
    let work = value_of(
      &run_ready(&owner, &["work", &green, &format!("cut-{round}")]),
      "id",
    );
    let text = format!("submitted in round {round}");
    run_ready(
      &owner,
      &["edit", &work, &format!("/merge-{round}"), "0", "0", &text],
    );
    let mut submit = slates()
      .arg("--instance")
      .arg(&owner)
      .args(["submit", &work])
      .stdout(Stdio::piped())
      .stderr(Stdio::null())
      .spawn()
      .unwrap();
    // The last round's client lives; the others are killed 0, 2, 4 or 6 ms in.
    if round < ROUNDS {
      sleep(Duration::from_millis(2 * u64::try_from(round % 4).unwrap()));
      let _ = submit.kill();
    }
    let output = submit.wait_with_output().unwrap();
    if let Some(version) = String::from_utf8_lossy(&output.stdout)
      .lines()
      .find_map(|line| line.strip_prefix("accepted: "))
      .and_then(|version| version.trim().parse::<u64>().ok())
    {
      told.push((round, version));
    }
  }
  verdict.note(format!("told (round, version): {told:?}"));
  verdict.check(
    "the living client was told its version",
    if told.iter().any(|(round, _)| *round == ROUNDS) {
      Vec::new()
    } else {
      vec!["the last submit, whose client lived, was not told a version".to_owned()]
    },
  );
  // The chain settles: a cut submit's acceptance may still be committing.
  sleep(Duration::from_secs(2));
  let head = green_head(&owner, &green).unwrap_or(0);
  let files = green_files(&owner, &green, ROUNDS + 1);
  let present = files.values().filter(|present| **present).count();
  verdict.note(format!(
    "head {head} from base {base}; files present {present} of {}",
    ROUNDS + 1
  ));
  verdict.check(
    "every told version is in the chain with its file",
    told
      .iter()
      .filter(|(round, version)| *version > head || !files.get(round).copied().unwrap_or(false))
      .map(|(round, version)| {
        format!(
          "round {round} told {version}, head {head}, file {:?}",
          files.get(round)
        )
      })
      .collect(),
  );
  verdict.check(
    "each version has its edit and each edit its version",
    if u64::try_from(present).unwrap() == head.saturating_sub(base) {
      Vec::new()
    } else {
      vec![format!(
        "head {head} - base {base} versions, {present} files"
      )]
    },
  );
  fleet.kill_node(owner_index);
  let deadline = Instant::now() + FLEET_WAIT;
  let taken = loop {
    let served = candidates
      .iter()
      .find(|candidate| green_head(candidate, &green) == Some(head));
    if served.is_some() || Instant::now() >= deadline {
      break served.cloned();
    }
    pause();
  };
  match taken {
    Some(successor) => {
      let after = green_files(&successor, &green, ROUNDS + 1);
      verdict.note(format!("taken over by {successor} at head {head}"));
      verdict.check(
        "the successor serves the same files (NoLoss, Agreement)",
        if after == files {
          Vec::new()
        } else {
          vec![format!("owner {files:?}, successor {after:?}")]
        },
      );
    }
    None => verdict.check(
      "a survivor serves the green at its head",
      vec![format!(
        "none within the fleet wait: {:?}",
        candidates
          .iter()
          .map(|candidate| (candidate.clone(), green_head(candidate, &green)))
          .collect::<Vec<_>>()
      )],
    ),
  }
  drop(fleet);
  verdict.judge();
}

/// How a volume whose destroy was never answered done settled: destroyed on every survivor, or served whole by one.
enum Settled {
  Destroyed,
  Served(String),
}

/// Waits up to the fleet wait for `volume` to settle on `survivors` ([`Settled`]); what it settled as (`None`: neither)
/// and the survivors' last views.
fn wait_settled_after_destroy(
  survivors: &[String],
  volume: &str,
  outcomes: &[Outcome],
  live: bool,
) -> (Option<Settled>, Vec<SurvivorView>) {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let views = survivor_views(survivors, volume);
    let destroyed = views.iter().all(|view| {
      !view.listed && matches!(&view.read, Some(Err(error)) if error.contains("NotFound"))
    });
    if destroyed {
      return (Some(Settled::Destroyed), views);
    }
    // A plain volume promises only its placed snapshot, so being served at all is its settled state; a live one must
    // serve every acknowledged write.
    if let Some(view) = views
      .iter()
      .find(|view| view.listed && (!live || no_loss(&view.instance, volume, outcomes).is_empty()))
    {
      return (Some(Settled::Served(view.instance.clone())), views);
    }
    if Instant::now() >= deadline {
      return (None, views);
    }
    pause();
  }
}
