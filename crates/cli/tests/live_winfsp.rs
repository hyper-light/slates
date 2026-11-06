//! A WinFsp flush on a live-shipped volume (§4.10, A-200; §4.6 "Windows"): three `slates anchor --fleet` processes from
//! one manifest form an `f = 1` fleet over loopback UDP, a live volume on one of them is mounted at a drive, and
//! `FlushFileBuffers` through the drive is answered success only once a candidate holds what it flushed. The Windows
//! counterpart of the FUSE `fsync` flow in `live_fleet.rs`; a candidate is partitioned by suspending its daemon's process
//! (`NtSuspendProcess`), as SIGSTOP does there. Each node is an anchor, since WinFsp mounts only under an anchor's hold.
//!
//! The fleet helpers are copies of `live_fleet.rs`'s (which is Unix as a whole), cut to what this flow needs. Windows
//! only, and gated on `SLATES_TEST_CLI=1` and `WINFSP_TEST_MOUNT=1` (CI's Windows lanes set both); without them the test
//! skips loudly and passes.
#![cfg(windows)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_methods
)]

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long a fleet step may take on a loaded box: the formation wait of the process flows.
const FLEET_WAIT: Duration = Duration::from_secs(60);
/// Shape: the pause between polls.
const POLL: Duration = Duration::from_millis(20);
/// Shape: shards per daemon: two, so a volume's owner shard need not be the control shard.
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

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  std::thread::sleep(POLL);
}

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a client verb; (exit code, stdout, stderr).
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

/// The value of `key: value` in a verb's text output.
fn value_of(text: &str, key: &str) -> String {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .to_owned()
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

/// A test-owned anchor, killed and reaped on drop (its daemon ends with it).
struct OwnedProcess {
  child: Child,
}

impl Drop for OwnedProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// The fleet's scratch directory, removed on drop.
struct ScratchDir {
  path: String,
}

impl Drop for ScratchDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

fn scratch_dir(tag: &str) -> ScratchDir {
  let path = format!(
    "{}/slates-live-winfsp-{tag}-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  std::fs::create_dir_all(&path).unwrap();
  ScratchDir { path }
}

fn mint_identity(dir: &str, node: &str) {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![FLEET_NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  std::fs::write(format!("{dir}/{node}.crt.der"), cert.der().as_ref()).unwrap();
  std::fs::write(format!("{dir}/{node}.key.der"), key.serialize_der()).unwrap();
}

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

/// A base of [`PORTS_PER_NODE`] consecutive loopback UDP ports free now, from `from`. A Windows daemon binds its own
/// serve sockets (no supervisor hands any over), so the ports are released before it starts.
fn free_port_base(from: u16) -> u16 {
  let mut base = from;
  for _ in 0..PORT_ATTEMPTS {
    let free = (0..PORTS_PER_NODE).all(|k| {
      base
        .checked_add(k)
        .is_some_and(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok())
    });
    if free {
      return base;
    }
    base = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
  }
  panic!("no block of free loopback UDP ports found from {from}");
}

fn port_bases() -> Vec<u16> {
  let offset = (std::process::id().wrapping_mul(7)) % u32::from(PORT_SPAN);
  let mut from = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  FLEET_NODES
    .iter()
    .map(|_| {
      let base = free_port_base(from);
      from = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
      base
    })
    .collect()
}

fn start_fleet_anchor(instance: &str, manifest: &str, node: &str) -> OwnedProcess {
  let child = slates()
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
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  OwnedProcess { child }
}

fn wait_answers(instance: &str) {
  let started = Instant::now();
  while run(instance, &["volume", "list"]).0 != 0 {
    assert!(
      started.elapsed() < FLEET_WAIT,
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

/// Waits until every node holds all three members with both peers probed; their member ids.
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

/// Waits until every node has all three members voting and configured and a record session to each peer.
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
          value_of(&out, "fleet_council_voters"),
          value_of(&out, "fleet_configuration_members"),
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

/// Every `fleet.live.*` and `winfsp.live_*` counter `instance` reports, summed over its shards, by name.
fn live_counters(instance: &str) -> std::collections::BTreeMap<String, u64> {
  let (_, status, _) = run(instance, &["status"]);
  let mut counts = std::collections::BTreeMap::<String, u64>::new();
  for (kind, count) in status
    .lines()
    .filter_map(|line| line.split_once(" refused ").map(|(_, rest)| rest))
    .filter(|rest| rest.starts_with("fleet.live.") || rest.starts_with("winfsp.live_"))
    .filter_map(|rest| rest.split_once(": "))
  {
    *counts.entry(kind.to_owned()).or_default() += count.trim().parse::<u64>().unwrap_or(0);
  }
  counts
}

/// The first free drive letter from `E:` to `Z:` (`GetLogicalDrives` bit clear).
fn free_drive_letter() -> String {
  // SAFETY: `GetLogicalDrives` takes no arguments and returns a bitmask (bit 0 = A:, 1 = B:, …).
  let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
  let letter = ('E'..='Z')
    .find(|letter| used & (1 << (u32::from(*letter) - u32::from('A'))) == 0)
    .expect("a free drive letter");
  format!("{letter}:")
}

/// Waits until `path` answers (the drive registers after the mount's reply), within the fleet wait.
fn wait_ready(path: &str) {
  let started = Instant::now();
  while started.elapsed() < FLEET_WAIT {
    if std::fs::metadata(path).is_ok() {
      return;
    }
    pause();
  }
  panic!("{path} never answered within {FLEET_WAIT:?}");
}

#[link(name = "ntdll")]
unsafe extern "system" {
  fn NtSuspendProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
  fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
}

/// A daemon process suspended whole (`NtSuspendProcess`), the unprivileged partition on Windows as SIGSTOP is on Unix:
/// it answers nothing and keeps its ports. Resumed on drop, so a failed assertion leaves it reapable.
struct Suspended {
  process: windows_sys::Win32::Foundation::HANDLE,
}

impl Suspended {
  /// Suspends the daemon `instance`'s anchor supervises, named by the `pid` its `status` reports.
  fn daemon_of(instance: &str) -> Suspended {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME};
    let (code, out, err) = run(instance, &["status"]);
    assert_eq!(code, 0, "status: {err}");
    let pid: u32 = value_of(&out, "pid").trim().parse().unwrap();
    // SAFETY: opens a handle to a process by id; the result is checked before use and closed on drop.
    let process = unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid) };
    assert!(!process.is_null(), "OpenProcess({pid})");
    // SAFETY: a live process handle this test owns, for the call.
    let status = unsafe { NtSuspendProcess(process) };
    assert_eq!(status, 0, "NtSuspendProcess");
    Suspended { process }
  }
}

impl Drop for Suspended {
  fn drop(&mut self) {
    // SAFETY: the handle `daemon_of` opened, live until it is closed here.
    let status = unsafe { NtResumeProcess(self.process) };
    if status != 0 {
      eprintln!("NtResumeProcess answered {status:#x}");
    }
    // SAFETY: as above; closed once.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(self.process) };
  }
}

/// Starts three anchors from one manifest, waits until they form, bootstraps the root once and waits until it settles:
/// the scratch directory, the anchors, their instances and the node bootstrapped.
fn start_fleet() -> (ScratchDir, Vec<OwnedProcess>, Vec<String>, usize) {
  let pid = std::process::id();
  let scratch = scratch_dir("flush");
  for node in FLEET_NODES {
    mint_identity(&scratch.path, node);
  }
  let manifest = write_manifest(&scratch.path, &port_bases());
  let instances: Vec<String> = FLEET_NODES
    .iter()
    .map(|node| format!("lw{node}{pid}"))
    .collect();
  let anchors: Vec<OwnedProcess> = FLEET_NODES
    .iter()
    .zip(&instances)
    .map(|(node, instance)| start_fleet_anchor(instance, &manifest, node))
    .collect();
  for instance in &instances {
    wait_answers(instance);
  }
  let hosts = wait_formed(&instances);
  let bootstrap = (0..hosts.len())
    .min_by_key(|index| hosts[*index].parse::<u64>().unwrap())
    .unwrap();
  let (code, _, err) = run(&instances[bootstrap], &["bootstrap", "root"]);
  assert_eq!(code, 0, "one bootstrap of the fresh fleet: {err}");
  wait_settled(&instances);
  (scratch, anchors, instances, bootstrap)
}

/// `sync_all` retried while it fails, within the fleet wait: resumed, the owner's lease is confirmed again within a
/// heartbeat or two, and its first flush may meet the log not yet caught up. The last answer.
fn synced_once_served(file: &std::fs::File) -> std::io::Result<()> {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let synced = file.sync_all();
    if synced.is_ok() || Instant::now() >= deadline {
      return synced;
    }
    pause();
  }
}

/// A-200 over WinFsp (§4.6 "Windows"): `FlushFileBuffers` through a drive is the caller's durability claim, which on a
/// live volume holds only once `f` candidates hold what the flush published. Do: form a three-node fleet of anchors,
/// make a live volume on one, mount it at a free drive, write a file and flush it (`File::sync_all`) once healthy, once
/// with both candidates' daemons suspended, and once more after they resume. Expect: healthy, success; suspended, an
/// error after the liveness budget, never success; resumed, success.
#[test]
fn a_winfsp_flush_on_a_live_volume_answers_only_once_a_candidate_holds_it() {
  if std::env::var_os("SLATES_TEST_CLI").is_none()
    || std::env::var_os("WINFSP_TEST_MOUNT").is_none()
  {
    eprintln!(
      "SKIP: the live-shipping WinFsp flush flow needs SLATES_TEST_CLI=1 and WINFSP_TEST_MOUNT=1 (three anchors, WinFsp)"
    );
    return;
  }
  let (scratch, anchors, instances, bootstrap) = start_fleet();
  let owner_index = (bootstrap + 1) % instances.len();
  let owner = instances[owner_index].clone();
  let id = value_of(
    &run_ready(
      &owner,
      &["volume", "create", "flushed", "--bounded", "8MiB"],
    ),
    "id",
  );
  let (code, _, err) = run(&owner, &["live-ship", &id]);
  assert_eq!(code, 0, "live-ship: {err}");
  let drive = free_drive_letter();
  let (code, _, err) = run(&owner, &["mount", &id, &drive]);
  assert_eq!(code, 0, "mount: {err}");
  wait_ready(&format!("{drive}\\"));
  let path = format!("{drive}\\synced");
  let mut file = std::fs::File::create(&path).unwrap();
  file.write_all(b"held by a candidate").unwrap();
  let healthy = file.sync_all();
  let candidates: Vec<&String> = instances
    .iter()
    .enumerate()
    .filter(|(index, _)| *index != owner_index)
    .map(|(_, instance)| instance)
    .collect();
  let suspended: Vec<Suspended> = candidates
    .iter()
    .map(|candidate| Suspended::daemon_of(candidate))
    .collect();
  file.write_all(b", then by no one").unwrap();
  let began = Instant::now();
  let alone = file.sync_all();
  let alone_took = began.elapsed();
  drop(suspended);
  let resumed = synced_once_served(&file);
  drop(file);
  let counters = live_counters(&owner);
  let _ = run(&owner, &["unmount", &drive]);
  drop(anchors);
  drop(scratch);
  eprintln!(
    "live winfsp flush: healthy {healthy:?}; candidates suspended {alone:?} after {alone_took:?}; resumed \
     {resumed:?}; owner's live counters {counters:?}"
  );
  assert!(
    healthy.is_ok(),
    "a flush a candidate holds succeeds: {healthy:?}"
  );
  assert!(
    alone.is_err(),
    "a flush no candidate can hold fails, not succeeds, after {alone_took:?}"
  );
  assert!(
    alone_took >= Duration::from_nanos(slates_server::daemon::LIVENESS_BUDGET_NS),
    "at the liveness budget: {alone_took:?}"
  );
  assert!(
    resumed.is_ok(),
    "once the candidates are back, a flush succeeds: {resumed:?}"
  );
}
