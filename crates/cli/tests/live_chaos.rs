//! Chaos across real processes for replicated volumes (§4.10, A-200; Ada's goal of 2026-11-04: replicas of the filesystem
//! server, replicated per write in under 1 ms, chaos-tested). Three fleet daemons from one manifest form an `f = 1` fleet
//! over loopback UDP; each flow streams writes through a real client while it knocks a process offline (SIGSTOP, the
//! unprivileged partition; SIGKILL of a daemon under its anchor, which starts a successor with a fresh identity), fails a
//! candidate, or cuts a client off, and every flow ends with the register invariants that the nemesis model of
//! `crates/db/tests/live.rs` checks inside one process, here checked from outside the processes:
//!
//! - **Agreement**: two copies of a volume's log at one epoch and position carry one chain identity. Every node's `live`
//!   status lines are sampled throughout the run ([`Sampler`]); the identity binds every entry before the head. This is
//!   the models' TotalOrder (no two different committed records share an epoch and position,
//!   `docs/wip/models/FencedRegister.tla`), judged over every copy that answers rather than the committed ones alone.
//! - **NoLoss**: every write a client was told succeeded is served byte for byte at the end, by whichever node serves the
//!   volume then; and no file is ever served torn (a write not acknowledged is absent or whole).
//! - **StaleNeverCommits**: once a volume has been taken over, every copy of its log that answers is fenced at or above
//!   the serving owner's epoch, and a write sent to the superseded owner while it was partitioned is refused, never
//!   acknowledged ([`judge_stale_writes`]); and while the owner serves, no partitioned candidate takes its volume over
//!   ([`no_takeover`]).
//! - **Convergence**: once healed, every candidate holds the owner's head (epoch, position and identity), and the owner's
//!   log is durable at its head with nothing owed.
//! - **ReadSafety**: a latest-state read sent through any node after a write's acknowledgement arrived answers that
//!   write's bytes or a refusal, never other bytes and never missing ([`read_safety`]); read through every survivor after
//!   a takeover ([`judge_takeover`]), through a partitioned owner while it runs cut off or the moment it resumes
//!   ([`an_owner_partitioned`]), and through every node, a partitioned, stalled or slowed one included, while a flow that
//!   takes nothing over runs ([`judge_reads`]).
//! - **Continuity**: a successor's first head is past every position an owner reported durable under an earlier epoch,
//!   in the samples or in a last look at the owner before its loss, since its log goes on from the head its takeover
//!   adopted ([`continuity`]).
//!
//! Every flow that takes a live volume over ends with NoLoss, StaleNeverCommits, ReadSafety and Continuity, and with
//! Agreement and Convergence too where it samples the copies and heals them: all but the big-entry flow, which samples
//! nothing, and the fresh-member flow, whose dropped holders are noted rather than converged. Every flow that partitions,
//! stalls, slows or loses datagrams while its owner serves throughout ends with Agreement, NoLoss, Convergence and
//! ReadSafety, its reads sent through every node mid-fault and again once it ends, with the owner answering every one:
//! its lease needs one healthy candidate's confirmation of two. The others judge what their subject owes: the destroys,
//! that nothing destroyed is served again; the clients killed or cut off, that no reply is held and a green stays whole,
//! its successor at exactly the head and files committed before (the green's own Continuity); and the kill at an
//! acknowledgement, NoLoss at that instant, which a last look before the kill would move.
//!
//! SIGSTOP freezes a whole process and drops nothing, so the flows that lose, delay, duplicate or reorder datagrams, or
//! cut a running node off in one direction or both, put the fault relay of `support/relay.rs` between the daemons
//! ([`Fleet::start_relayed`]): each node advertises a port block the relay owns and binds its own (the manifest's `bind`,
//! A-277), with no privilege, where T-8.7 named iptables or pf, which R10 rules out. Every relayed flow prints its seed,
//! and requires its relay to have carried a record-plane datagram per acknowledged write ([`judge_relayed`]): until
//! A-330 a node announced its record plane at its bound port, every record link went around the relay after formation,
//! and the relayed flows' faults reached only the probes.
//!
//! The nemesis ([`a_seeded_nemesis_of_single_faults_keeps_every_register_invariant`]) draws a schedule of single faults
//! from its seed (every path lossy, a node slowed, one link severed, or a node stopped, cut off, deafened or muted, the
//! owner struck as often as a candidate, and once a run until it is taken over) and waits for the fleet to recover
//! whole between them, so no cohort loses two members; it ends with every invariant above, Continuity judged from the
//! samples since its takeovers are unscheduled ([`continuity_in_samples`]).
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

#[path = "../../server/tests/common/nfs.rs"]
#[allow(dead_code)]
mod nfs;

// The fault relay the relayed flows put between the daemons (goal g229, A-277).
#[path = "support/relay.rs"]
mod relay;

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use relay::{Draws, PathRule, Relay, RelayedNode};
use slates_client::{Client, ClientError, Deadlines, Intent, ReadAt, Refusal, VolumeId};

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
/// Shape: a fleet wide enough that its council keeps a majority (3 of 5) with the owner lost and one of a volume's two
/// holders partitioned, so a takeover runs while a holder of the log cannot answer, and the successor's placement
/// holds members that are not holders.
const WIDE_NODES: [&str; 5] = ["a", "b", "c", "d", "e"];
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
fn write_manifest(dir: &str, names: &[&str], bases: &[u16]) -> String {
  let nodes: Vec<serde_json::Value> = names
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

/// [`write_manifest`] for a fleet behind the fault relay (A-277): each node advertised at the relay's block for it and
/// bound at its own, so every datagram between two nodes passes through the relay.
#[allow(clippy::disallowed_methods)]
fn write_relayed_manifest(dir: &str, names: &[&str], advertised: &[u16], bound: &[u16]) -> String {
  let nodes: Vec<serde_json::Value> = names
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
  let manifest = serde_json::json!({ "name": FLEET_NAME, "f": FLEET_F, "nodes": nodes });
  let path = format!("{dir}/fleet.json");
  std::fs::write(&path, manifest.to_string()).unwrap();
  path
}

/// Format: the variable that fixes the relay's seed, to replay a run's draws; each relayed flow prints its seed.
const CHAOS_SEED: &str = "SLATES_CHAOS_SEED";

/// The relay's seed for flow `tag`: [`CHAOS_SEED`] when set, else this process's id mixed with the tag, so each run
/// draws its own schedule and names it.
fn chaos_seed(tag: &str) -> u64 {
  std::env::var(CHAOS_SEED)
    .ok()
    .and_then(|text| text.parse().ok())
    .unwrap_or_else(|| {
      tag
        .bytes()
        .fold(u64::from(std::process::id()), |seed, byte| {
          seed.rotate_left(8) ^ u64::from(byte)
        })
    })
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

fn port_blocks(nodes: usize) -> Vec<HeldBlock> {
  // Offset from `live_fleet.rs`'s start (which multiplies the pid by 7), so the two suites run side by side.
  let offset = (std::process::id().wrapping_mul(13)) % u32::from(PORT_SPAN);
  let mut from = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  let mut blocks = Vec::new();
  for _ in 0..nodes {
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

/// Whether a node's probe mesh is up as its daemon judges it, with `mesh` its `(fleet_peers_in_contact,
/// fleet_peers_unmeshed)` ([`mesh_of`]): it keeps contact with at least one peer, and every peer in contact is probed.
/// The peers in contact are counted over the members held alive: every member while the council forms, and after it
/// only the holders and the voters (§4.8, amended 2026-10-10). So a five-node fleet's two non-voters, which keep no
/// contact with each other, are not a stall at 3 of 4 peers probed (the flow took them for one, CI diag-live-fixes-30
/// runs 5 and 6). Counted over the installed configuration's members instead, both counts read 0 on every node before
/// the council's first commit, and the wait never ended (CI diag-raft-chaos-mesh-42).
fn meshed(mesh: Option<(u32, u32)>) -> bool {
  mesh.is_some_and(|(in_contact, unmeshed)| in_contact > 0 && unmeshed == 0)
}

/// `instance`'s probe mesh from `status`: the peers it keeps direct contact with (`fleet_peers_in_contact`) and how
/// many of them have no formed probe session (`fleet_peers_unmeshed`); `None` when `status` does not answer.
fn mesh_of(instance: &str) -> Option<(u32, u32)> {
  let (code, out, _) = run(instance, &["status"]);
  if code != 0 {
    return None;
  }
  let count = |key: &str| value_of(&out, key).trim().parse::<u32>().ok();
  Some((
    count("fleet_peers_in_contact")?,
    count("fleet_peers_unmeshed")?,
  ))
}

/// Waits until every node holds all `members` members alive with its probe mesh up ([`meshed`]); returns their member
/// ids.
fn wait_formed(instances: &[String], members: usize) -> Vec<String> {
  let mut window = FrozenWindow::new();
  loop {
    let views: Vec<_> = instances
      .iter()
      .map(|instance| fleet_view(instance))
      .collect();
    // Each node's (in contact, unmeshed), as `status` prints them.
    let meshes: Vec<Option<(u32, u32)>> =
      instances.iter().map(|instance| mesh_of(instance)).collect();
    if views.iter().zip(&meshes).all(|(view, mesh)| {
      view
        .as_ref()
        .is_some_and(|(_, held, _)| *held == members && meshed(*mesh))
    }) {
      return views.into_iter().map(|view| view.unwrap().0).collect();
    }
    // Progress counts a node's mesh coming up as well as its members and probes changing.
    if !window.moving(&format!("{views:?} {meshes:?}")) {
      // What the probe and record planes did, for a node whose in-contact peers never all probed it.
      panic!(
        "the fleet formed, or kept changing, within each fleet wait: {views:?} (in contact, unmeshed) {meshes:?}\n{}",
        diagnosis(instances, "")
      );
    }
    pause();
  }
}

/// Waits until every node in `instances` holds the fleet's newest configuration and has settled at its own neighbourhood
/// generation (the fleet's objects re-placed onto the neighbourhood after a membership change), judged by progress
/// ([`FrozenWindow`]). Settled at its own generation alone certifies a learner left behind: in each of four runs that
/// lost the takeover after this wait, one learner sat at configuration 7 or 8, settled == neighbourhood at 3 or 4,
/// while the council was at 19–24 (diag-live-fixes-34 runs 1 and 3, -35 runs 5 and 8; live-fixes, 2026-11-08).
fn wait_neighbourhood_settled(instances: &[String]) {
  let mut window = FrozenWindow::new();
  loop {
    let views: Vec<(Option<u64>, Option<u64>, Option<u64>)> = instances
      .iter()
      .map(|instance| {
        let (_, out, _) = run(instance, &["status"]);
        let read = |key: &str| {
          out
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}: ")))
            .and_then(|value| value.trim().parse::<u64>().ok())
        };
        (
          read("fleet_settled_generation"),
          read("fleet_neighbourhood_generation"),
          read("fleet_configuration_version"),
        )
      })
      .collect();
    // The configuration's version is the council's, one count for the whole fleet, so every node must reach the newest
    // any node holds.
    let newest = views.iter().filter_map(|(_, _, version)| *version).max();
    // Each node settled at its own neighbourhood generation. The generations are each node's own count and differ
    // between nodes in a settled fleet (3, 8 and 11 side by side in diag-31), so they are not compared across nodes; a
    // node retired while away is kept out by the stale holder's short stop instead.
    if views.iter().all(|(settled, neighbourhood, version)| {
      settled.is_some() && settled == neighbourhood && version.is_some() && *version == newest
    }) {
      return;
    }
    if !window.moving(&format!("{views:?}")) {
      // What the stuck node reported and shipped toward its new cohort (live-fixes, 2026-11-07).
      panic!(
        "every node held the newest configuration and its neighbourhood settled, or kept changing, within each fleet \
         wait (settled, neighbourhood, configuration version): {views:?}\n{}",
        diagnosis(instances, "")
      );
    }
    pause();
  }
}

/// A wait judged by progress, not a fixed span: it goes on while what it watches keeps changing, and gives up only after
/// a whole fleet wait in which nothing changed. A five-member fleet forming on a loaded CI runner took longer than one
/// fleet wait while it was still forming (diag-28 runs 1 and 2, 2026-11-07; live-fixes measured 37-108 s for the server
/// fleet suite and judges those waits the same way, `poll_until`).
struct FrozenWindow {
  last: Option<String>,
  since: Instant,
}

impl FrozenWindow {
  fn new() -> FrozenWindow {
    FrozenWindow {
      last: None,
      since: Instant::now(),
    }
  }

  /// Whether the wait may go on after seeing `now`: it changed, or it has been the same for less than a fleet wait.
  fn moving(&mut self, now: &str) -> bool {
    if self.last.as_deref() != Some(now) {
      self.last = Some(now.to_owned());
      self.since = Instant::now();
    }
    self.since.elapsed() < FLEET_WAIT
  }
}

/// One node's view for [`recovered`]: its council's joint flag and voter ids, its own member id, and how many
/// retirements it keeps (a takeover not yet confirmed by every survivor).
#[derive(Debug)]
struct RecoveryView {
  joint: String,
  voters: String,
  host: String,
  retirements: usize,
}

/// `instance`'s [`RecoveryView`], from its status.
fn recovery_view(instance: &str) -> RecoveryView {
  let (_, out, _) = run(instance, &["status"]);
  let field = |key: &str| {
    out
      .lines()
      .find_map(|line| line.strip_prefix(key))
      .unwrap_or("?")
      .trim()
      .to_owned()
  };
  RecoveryView {
    joint: field("fleet_council_joint: "),
    voters: field("fleet_council_voter_ids: "),
    host: field("fleet_host: "),
    retirements: out
      .lines()
      .filter(|line| line.starts_with("fleet_retirement:"))
      .count(),
  }
}

/// What keeps the fleet from having recovered from its one failure, node by node, as `views` show it (empty once it
/// has): a voter change still in flight (a joint configuration), voters that differ from the first node's, the node's
/// own member id not among them (a replacement not yet seated: until the change begins every council already reads no
/// joint and the same voters), or a retirement kept (the failed node's takeover not yet confirmed by every survivor, so
/// a later failure's objects would be recovered jointly with its cohort, `RegionalConfiguration::recovery_owners`).
/// Judged by the council's joint and voter agreement alone, the wait passed before the voter change began
/// (diag-live-durability-77 and -78, 7 of 22 failed runs) or as it was about to (13 of 22).
fn unrecovered(views: &[RecoveryView]) -> Vec<String> {
  let agreed = views.first().map(|first| first.voters.as_str());
  let mut missing = Vec::new();
  for view in views {
    if view.joint != "false" {
      missing.push(format!(
        "{}: voter change in flight (joint {})",
        view.host, view.joint
      ));
    }
    if view.voters == "?" || Some(view.voters.as_str()) != agreed {
      missing.push(format!("{}: voters {} differ", view.host, view.voters));
    }
    if !view
      .voters
      .split_whitespace()
      .any(|voter| voter == view.host)
    {
      missing.push(format!("{}: not a voter ({})", view.host, view.voters));
    }
    if view.retirements > 0 {
      missing.push(format!(
        "{}: {} retirement(s) kept",
        view.host, view.retirements
      ));
    }
  }
  missing
}

/// Whether the fleet has recovered from its one failure: nothing [`unrecovered`].
fn recovered(views: &[RecoveryView]) -> bool {
  unrecovered(views).is_empty()
}

/// Waits until the fleet has recovered from its one failure ([`recovered`]). A flow that has already spent it (a
/// candidate killed, replaced or restarted, or a nemesis fault) must wait for this before it strikes again. At `f = 1` a
/// second failure while the voter change is open cannot be survived under any membership scheme: joint consensus needs a
/// majority of both sets, and single-server steps leave either {A, owner} needing both or four voters needing three. The
/// flows' own waits (`wait_rejoined`, `judge_healed`) look at membership and the log, not the council.
/// diag-live-durability-66 killed the owner with the change open in 6 of 6 x86 runs, and no survivor could elect a leader
/// to take it over.
///
/// Judged by progress ([`FrozenWindow`]), as the council wait it replaced was: it goes on while any node's view changes,
/// and fails, naming what is missing on each node ([`unrecovered`]), only after a whole fleet wait in which none did.
/// Measured on CI, the voter change it waits for closed at most 5.26 s after the node was killed (BENCHMARKS "The
/// council's voter change after a candidate's node is replaced"); the failed node's takeover is confirmed after it, how
/// long after is what the flows note (the wait's length, returned).
fn wait_recovered(instances: &[String]) -> Duration {
  let began = Instant::now();
  let mut window = FrozenWindow::new();
  loop {
    let views: Vec<RecoveryView> = instances
      .iter()
      .map(|instance| recovery_view(instance))
      .collect();
    let missing = unrecovered(&views);
    if missing.is_empty() {
      return began.elapsed();
    }
    assert!(
      window.moving(&format!("{views:?}")),
      "the fleet recovered from its failure, or kept changing, within each fleet wait: {}",
      missing.join("; ")
    );
    pause();
  }
}

/// The views diag-live-durability-77 dumped once no survivor took the owner's volume over, each as it stood when the
/// flow killed the owner. In 4 of 11 runs the replacement was not yet a voter: the voter change had not begun (it begins
/// about 1.9 s after the replacement answers, BENCHMARKS "The council's voter change after a candidate's node is
/// replaced"), every council read no joint and the same voters, and the owner's death left one live voter of three.
/// In 6 the change began after the wait and the owner died inside it. In 1 the council was seated, but the destroyed
/// node's takeover was still kept, unconfirmed by the owner; its objects' recovery joined that takeover's cohort, of
/// which only one member was left, and the survivor counted 8 objects lost.
#[test]
fn a_replacement_not_yet_voting_or_a_takeover_still_kept_is_not_recovered() {
  let view = |joint: &str, voters: &str, host: &str, retirements: usize| RecoveryView {
    joint: joint.to_owned(),
    voters: voters.to_owned(),
    host: host.to_owned(),
    retirements,
  };
  // Node 1 destroyed and replaced by 4; 2 the owner, 3 the other candidate.
  assert!(
    !recovered(&[
      view("false", "1 2 3", "4", 1),
      view("false", "1 2 3", "2", 1),
      view("false", "1 2 3", "3", 1),
    ]),
    "the replacement is not a voter yet"
  );
  assert!(
    !recovered(&[
      view("false", "2 3 4", "4", 1),
      view("false", "2 3 4", "2", 1),
      view("false", "2 3 4", "3", 1),
    ]),
    "the destroyed node's takeover is still kept"
  );
  assert!(
    recovered(&[
      view("false", "2 3 4", "4", 0),
      view("false", "2 3 4", "2", 0),
      view("false", "2 3 4", "3", 0),
    ]),
    "seated, and nothing kept"
  );
  assert_eq!(
    unrecovered(&[
      view("false", "1 2 3", "4", 0),
      view("true", "1 2 3", "2", 1),
    ]),
    vec![
      "4: not a voter (1 2 3)".to_owned(),
      "2: voter change in flight (joint true)".to_owned(),
      "2: 1 retirement(s) kept".to_owned(),
    ],
    "each missing condition is named, node by node"
  );
}

/// Waits until the fleet is whole on every node: all `members` members voting, all in the committed configuration, and
/// a record session to each peer (`live_fleet.rs`'s `wait_settled`, which says why).
fn wait_settled(instances: &[String], members: usize) {
  let mut window = FrozenWindow::new();
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
    // The council seats `2f + 1` voters whatever the membership (`slates_cluster::config_group::council_voters`).
    let expected = members.min(2 * FLEET_F + 1);
    // Every member holds the whole configuration, and every seat is filled as its voters see it. In a fleet no wider
    // than the council every member votes and holds a record session to each peer. A wider fleet's members without a
    // seat report a council of one (they are not in it), and a member need not hold a session to every peer (5-node
    // runs of 2026-11-07 settled with 2 to 4); there, the flow's first durable writes show the owner reaches its holders.
    let configured_everywhere = views
      .iter()
      .all(|(_, configured, _)| configured.trim() == members.to_string());
    let seated = views
      .iter()
      .filter(|(voters, _, _)| voters.trim() == expected.to_string())
      .count();
    let meshed = members > expected
      || views
        .iter()
        .all(|(_, _, sessions)| Some(*sessions) == members.checked_sub(1));
    let whole = configured_everywhere && seated >= expected && meshed;
    if whole {
      return;
    }
    assert!(
      window.moving(&format!("{views:?}")),
      "the fleet settled, or kept changing, within each fleet wait (council voters, configuration members, record \
       sessions): {views:?}\n{}",
      diagnosis(instances, "")
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
  /// Each node's instance and certificate anchor, read as the fleet formed: a restart keeps a node's anchor.
  anchors: Vec<(String, String)>,
  supervision: Supervision,
  bootstrap: usize,
  /// The manifest's node names, one per node index.
  nodes: &'static [&'static str],
  /// The fault relay every datagram between the nodes passes through, for a fleet started relayed
  /// ([`Fleet::start_relayed`]). Last, so it stops after the processes are gone.
  relay: Option<Relay>,
}

impl Fleet {
  /// Starts three nodes from one manifest, waits until they form, and bootstraps the root once.
  fn start(tag: &str, supervision: Supervision) -> Fleet {
    Fleet::start_with(tag, supervision, &FLEET_NODES)
  }

  /// Starts the nodes `nodes` names from one manifest, waits until they form, and bootstraps the root once.
  fn start_with(tag: &str, supervision: Supervision, nodes: &'static [&'static str]) -> Fleet {
    Fleet::launch(tag, supervision, nodes, false)
  }

  /// [`Fleet::start_with`] behind the fault relay (goal g229, A-277): each node advertises a block the relay holds and
  /// binds one of its own, so every datagram between two nodes passes the relay, where a flow can cut, delay, lose or
  /// duplicate it ([`Fleet::relay`]) while every process runs on. The relay forwards everything at once until told.
  fn start_relayed(tag: &str, supervision: Supervision, nodes: &'static [&'static str]) -> Fleet {
    Fleet::launch(tag, supervision, nodes, true)
  }

  /// The fault relay of a fleet started with [`Fleet::start_relayed`].
  fn relay(&self) -> &Relay {
    self
      .relay
      .as_ref()
      .expect("a relayed fleet (Fleet::start_relayed)")
  }

  fn launch(
    tag: &str,
    supervision: Supervision,
    nodes: &'static [&'static str],
    relayed: bool,
  ) -> Fleet {
    let pid = std::process::id();
    let scratch = scratch_dir(tag);
    for node in nodes {
      mint_identity(&scratch.path, node);
    }
    let blocks = port_blocks(nodes.len());
    let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
    let (manifest, relay) = if relayed {
      // The relay's blocks are held while the nodes' are, so the two sets never share a port.
      let advertised = port_blocks(nodes.len());
      let advertised_bases: Vec<u16> = advertised.iter().map(|held| held.base).collect();
      let manifest = write_relayed_manifest(&scratch.path, nodes, &advertised_bases, &bases);
      let seed = chaos_seed(tag);
      let relay = Relay::start(
        advertised
          .into_iter()
          .zip(&bases)
          .map(|(held, base)| RelayedNode {
            advertised: held.sockets.try_into().unwrap(),
            bound: std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, *base),
          })
          .collect(),
        seed,
      );
      // A node's serve sockets are its bind block, known up front; only its record links' ports need a census.
      for (index, base) in bases.iter().enumerate() {
        relay.claim(*base, index);
        relay.claim(base.checked_add(1).unwrap(), index);
      }
      eprintln!("chaos: fleet {tag} relayed, seed {seed} (set {CHAOS_SEED} to replay its draws)");
      (manifest, Some(relay))
    } else {
      (write_manifest(&scratch.path, nodes, &bases), None)
    };
    let instances: Vec<String> = nodes
      .iter()
      .map(|node| format!("chaos-{tag}-{node}-{pid}"))
      .collect();
    // One node at a time, each answering before the next starts: a daemon's boot measures the machine
    // (`MachineProfile::measure_patiently`, about 13 s for a lone debug daemon on a loaded Mac, 2026-11-07), and three
    // measuring at once disturb each other past the start wait, so every flow failed at start (live-durability, at load
    // 12 with no other fleet running).
    let processes = nodes
      .iter()
      .zip(&instances)
      .zip(&blocks)
      .enumerate()
      .map(|(index, ((node, instance), held))| {
        let process = start_node(instance, &manifest, node, held, supervision);
        if let Some(relay) = &relay {
          relay.root(index, process.child.id());
        }
        wait_answers(instance);
        Some(process)
      })
      .collect();
    let hosts = wait_formed(&instances, nodes.len());
    let bootstrap = (0..hosts.len())
      .min_by_key(|index| hosts[*index].parse::<u64>().unwrap())
      .unwrap();
    let (code, _, error) = run(&instances[bootstrap], &["bootstrap", "root"]);
    assert_eq!(code, 0, "one bootstrap of the fresh fleet: {error}");
    wait_settled(&instances, nodes.len());
    eprintln!("chaos: fleet {tag} ({supervision:?}) formed and settled: hosts {hosts:?}");
    DIAGNOSED.with(|diagnosed| *diagnosed.borrow_mut() = instances.clone());
    let anchors = instances
      .iter()
      .map(|instance| {
        (
          instance.clone(),
          value_of(&run_ready(instance, &["status"]), "anchor"),
        )
      })
      .collect();
    Fleet {
      _scratch: scratch,
      manifest,
      blocks,
      processes,
      instances,
      anchors,
      supervision,
      bootstrap,
      nodes,
      relay,
    }
  }

  /// Shares `id` through `owner` with this test's account on every other node, so a survivor can reach it after a
  /// takeover (A-237: a local account means something only on its host, and the survivors' clients are other hosts').
  fn share(&self, owner: &str, id: &str) {
    let account = rustix::process::getuid().as_raw();
    for (node, anchor) in self.anchors.iter().filter(|(node, _)| node != owner) {
      let (code, _, err) = run(
        owner,
        &[
          "share",
          id,
          &format!("uid:{account}@{anchor}"),
          "--read",
          "--write",
          "--admin",
        ],
      );
      assert_eq!(code, 0, "the share with {node}: {err}");
    }
  }

  /// [`create_volume`] on `owner`, shared with every other node ([`Self::share`]).
  fn create_volume(&self, owner: &str, name: &str, live: bool) -> String {
    let id = create_volume(owner, name, live);
    self.share(owner, &id);
    id
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

  /// [`Fleet::daemon_pid`] for a guard that ends a node holding a flow up: `None` once the node is gone, or while an
  /// anchored node's status names no daemon, rather than a panic inside the guard.
  fn live_daemon_pid(&self, index: usize) -> Option<u32> {
    let process = self.processes.get(index)?.as_ref()?;
    match self.supervision {
      Supervision::Bare => Some(process.child.id()),
      Supervision::Anchored => daemon_pid(&self.instances[index]),
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
    let process = start_node(
      &self.instances[index],
      &self.manifest,
      self.nodes[index],
      &self.blocks[index],
      self.supervision,
    );
    // The replacement's processes are the node's now: the relay attributes its sockets by them.
    if let Some(relay) = &self.relay {
      relay.root(index, process.child.id());
    }
    self.processes[index] = Some(process);
    wait_answers(&self.instances[index]);
  }

  /// Waits until every live node holds exactly the manifest's members with its probe mesh up ([`meshed`]): a succeeded
  /// or replaced node rejoined under its new identity and the old one retired.
  fn wait_rejoined(&self) {
    let live: Vec<String> = (0..self.instances.len())
      .filter(|index| self.processes[*index].is_some())
      .map(|index| self.instances[index].clone())
      .collect();
    wait_formed(&live, self.nodes.len());
    wait_settled(&live, self.nodes.len());
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

/// Shape: the room each streamed file takes beyond its bytes (its inode and directory entry), as a multiple: room
/// twice over.
const FILE_ROOM: usize = 2;

/// Derived: a flow's volume holds every file a stream can write, `STREAM_FILES` × `WRITE_BYTES` × [`FILE_ROOM`], in
/// MiB, so no flow is ever refused for space before it is stopped. A fixed 64 MiB held 16,384 writes, and a 10 s
/// release stream with the live checkpoint fix wrote 16,404 and failed on its last 20 (live-durability, CI
/// diag-live-durability-7, 2026-11-07). It is a quota, not memory: the daemons hold only what was written.
fn volume_bound_mib() -> usize {
  STREAM_FILES
    .saturating_mul(WRITE_BYTES)
    .saturating_mul(FILE_ROOM)
    .div_ceil(1 << 20)
}

/// Creates a volume on `owner` (live-shipped when `live`): its id.
fn create_volume(owner: &str, name: &str, live: bool) -> String {
  let bound = format!("{}MiB", volume_bound_mib());
  let id = value_of(
    &run_ready(owner, &["volume", "create", name, "--bounded", &bound]),
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

/// The newest write a stream has had acknowledged, published as the stream goes (goal g229), so a chaos running beside it
/// reads back writes already acknowledged: [`read_safety`] judges a read only once it was sent after its write's
/// acknowledgement, and a read of a write still in flight claims nothing. A stream writes its files in order, one at a
/// time, so the newest acknowledged is the highest.
#[derive(Default)]
struct NewestAcknowledged(AtomicUsize);

impl NewestAcknowledged {
  /// Records that the write of `file` was acknowledged. Stored as `file + 1`, so zero is "none yet".
  fn mark(&self, file: usize) {
    self.0.store(file.saturating_add(1), Ordering::Release);
  }

  /// The `count` files up to and including the newest acknowledged, newest first; empty before any acknowledgement. An
  /// older one may have been refused, which [`read_safety`] then leaves unjudged; the newest never was.
  fn newest(&self, count: usize) -> Vec<usize> {
    let marked = self.0.load(Ordering::Acquire);
    (marked.saturating_sub(count)..marked).rev().collect()
  }
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
    self.run_marking(files, stop, None)
  }

  /// [`Stream::run`], marking each write in `newest` as its acknowledgement arrives, so a chaos running beside the
  /// stream knows which writes it may read back ([`NewestAcknowledged`]).
  fn run_marking(
    &self,
    files: std::ops::Range<usize>,
    stop: &AtomicBool,
    newest: Option<&NewestAcknowledged>,
  ) -> Vec<Outcome> {
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
      // Marked once `took` is read, so a read sent after the mark is sent after the acknowledgement `read_safety` dates.
      if !failed && let Some(newest) = newest {
        newest.mark(file);
      }
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
  /// On the owner, the member ids of the holders its durability counts; empty on a candidate.
  holders: Vec<String>,
  /// On the owner, the member ids of the set a holder transition is moving to; empty when none is in flight.
  moving_to: Vec<String>,
  /// On the owner, the member ids of the holder set its volume's catalog records (what a takeover counts).
  recorded: Vec<String>,
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
          holders: id_list(field(fields, "holders=")),
          moving_to: id_list(field(fields, "moving_to=")),
          recorded: id_list(field(fields, "recorded=")),
        })
      })
      .collect(),
  )
}

/// A status line's comma-joined member ids (`none`, or a line from before the field: empty).
fn id_list(text: Option<&str>) -> Vec<String> {
  text
    .filter(|text| *text != "none")
    .map(|text| text.split(',').map(str::to_owned).collect())
    .unwrap_or_default()
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
  holds: &'a SamplerHolds,
}

/// A flow's way to take one node out of the [`Sampler`] before it stops that node. A `status` call to a stopped node
/// returns only when its client gives up, and the flow waits for the sampler at the stream's end, so a holder meant to be
/// stopped briefly stayed stopped 4 to 6 s, in whole seconds, past the failure detector (diag-live-fixes-44).
struct SamplerHolds {
  wanted: Vec<AtomicBool>,
  idle: Vec<AtomicBool>,
}

impl SamplerHolds {
  fn new(nodes: usize) -> SamplerHolds {
    let flags = || (0..nodes).map(|_| AtomicBool::new(false)).collect();
    SamplerHolds {
      wanted: flags(),
      idle: flags(),
    }
  }

  /// Takes `node` out of the sampling, and returns once its thread has no `status` call in flight (within the fleet wait).
  fn hold(&self, node: usize) {
    self.wanted[node].store(true, Ordering::Release);
    let deadline = Instant::now() + FLEET_WAIT;
    while !self.idle[node].load(Ordering::Acquire) && Instant::now() < deadline {
      pause();
    }
  }

  /// Whether `node` is held; the sampling thread's asking says it has no call in flight.
  fn held(&self, node: usize) -> bool {
    let held = self.wanted[node].load(Ordering::Acquire);
    if held {
      self.idle[node].store(true, Ordering::Release);
    }
    held
  }
}

impl Sampler<'_> {
  /// Samples every node until `stop`, each node on its own thread, then once more; the samples in time order. One thread
  /// a node, because a `status` call to a stopped (SIGSTOPped) node does not return until it resumes: one round over
  /// every node left a hole in every node's samples for as long as one was stopped (about 1.5 s of every 2 s partition
  /// in `candidates_partitioned_in_turn_mid_stream_lose_nothing`, live-durability, 2026-11-07).
  fn run(&self, stop: &AtomicBool) -> Vec<Sample> {
    let mut samples: Vec<Sample> = std::thread::scope(|scope| {
      let threads: Vec<_> = self
        .instances
        .iter()
        .enumerate()
        .map(|(node, instance)| {
          scope.spawn(move || {
            let mut samples = Vec::new();
            while !stop.load(Ordering::Acquire) {
              if !self.holds.held(node) {
                samples.extend(self.once(node, instance));
              }
              pause();
            }
            if !self.holds.held(node) {
              samples.extend(self.once(node, instance));
            }
            samples
          })
        })
        .collect();
      threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect()
    });
    samples.sort_by_key(|sample| sample.at);
    samples
  }

  fn once(&self, node: usize, instance: &str) -> Vec<Sample> {
    let at = self.began.elapsed();
    live_states(instance, self.volume)
      .unwrap_or_default()
      .into_iter()
      .map(|state| Sample { at, node, state })
      .collect()
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

/// What a latest-state read through one node answered its client.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReadAnswer {
  /// The file's own bytes.
  Current,
  /// Other bytes than the file's write.
  Other { length: usize },
  /// Missing: `NotFound`. An owner-location round never answers `NotFound` for a view it cannot reach
  /// (`owner_location::LocationError`), so this is a claim that the file does not exist.
  Missing,
  /// Any other refusal: an answer that claims nothing about the file.
  Refused(String),
}

impl std::fmt::Display for ReadAnswer {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ReadAnswer::Current => write!(out, "its own bytes"),
      ReadAnswer::Other { length } => write!(out, "other bytes ({length} of them)"),
      ReadAnswer::Missing => write!(out, "missing"),
      ReadAnswer::Refused(why) => write!(out, "refused: {why}"),
    }
  }
}

/// One latest-state read through one node, as its client saw it.
#[derive(Clone, Debug)]
struct ReadProbe {
  file: usize,
  /// The node the read was sent through.
  through: String,
  /// When the read was sent, from the flow's start.
  began: Duration,
  answer: ReadAnswer,
}

impl std::fmt::Display for ReadProbe {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      out,
      "{} read through {} at {:?}: {}",
      chaos_path(self.file),
      self.through,
      self.began,
      self.answer
    )
  }
}

/// Reads each of `files` once through `instance` at the head, in order, timing each from `began`: what each answered.
/// A client that cannot connect answers every read refused.
fn read_each(instance: &str, volume: &str, files: &[usize], began: &Instant) -> Vec<ReadProbe> {
  let id = volume_id_of(volume);
  let mut client = Client::connect(instance, deadlines());
  files
    .iter()
    .map(|file| {
      let at = began.elapsed();
      let answer = match client
        .as_mut()
        .map(|client| client.read(id, &format!("/{}", chaos_path(*file)), ReadAt::Head))
      {
        Ok(Ok(bytes)) if bytes == chaos_body(*file) => ReadAnswer::Current,
        Ok(Ok(bytes)) => ReadAnswer::Other {
          length: bytes.len(),
        },
        Ok(Err(ClientError::Refused(Refusal::NotFound))) => ReadAnswer::Missing,
        Ok(Err(error)) => ReadAnswer::Refused(error.to_string()),
        Err(error) => ReadAnswer::Refused(format!("connect: {error}")),
      };
      ReadProbe {
        file: *file,
        through: instance.to_owned(),
        began: at,
        answer,
      }
    })
    .collect()
}

/// ReadSafety (`docs/wip/models/FencedRegister.tla` and `Reconfig.tla`: every read quorum holds a record at least as new
/// as every committed one), as a client sees it: a latest-state read sent after a write's acknowledgement arrived
/// answers that write's bytes or a refusal, never other bytes and never missing. A read sent before the
/// acknowledgement, or of a write never acknowledged, claims nothing. The violations.
fn read_safety(probes: &[ReadProbe], outcomes: &[Outcome]) -> Vec<String> {
  let acknowledged = acknowledged_at(outcomes);
  probes
    .iter()
    .filter(|probe| judged(probe, &acknowledged))
    .filter(|probe| matches!(probe.answer, ReadAnswer::Other { .. } | ReadAnswer::Missing))
    .map(|probe| {
      format!(
        "{probe}, though acknowledged at {:?}",
        acknowledged.get(&probe.file)
      )
    })
    .collect()
}

/// When each acknowledged write of `outcomes` was acknowledged, by file, from the flow's start: its send plus its wait.
fn acknowledged_at(outcomes: &[Outcome]) -> BTreeMap<usize, Duration> {
  outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_none())
    .map(|outcome| (outcome.file, outcome.sent + outcome.took))
    .collect()
}

/// Whether [`read_safety`] judges `probe`: it was sent once its file's write had been acknowledged (`acknowledged`, from
/// [`acknowledged_at`]). A read sent earlier, or of a write never acknowledged, claims nothing.
fn judged(probe: &ReadProbe, acknowledged: &BTreeMap<usize, Duration>) -> bool {
  acknowledged
    .get(&probe.file)
    .is_some_and(|acknowledged_at| *acknowledged_at <= probe.began)
}

/// Continuity (`docs/wip/models/FencedRegister.tla`: a promoted epoch's base is at least as new as every record
/// committed below it; T-8.6's exact historical-prefix Continuity), the live log's side. A successor's log starts at the
/// head its takeover adopted and its positions go on from there (`slates_db::live::Shipper::new`'s `start`), and it ships
/// only from its first publication, so `first_head` is its head after its first acknowledged write: the adopted position
/// plus one, or more if its record period appended too. That head must be past every position an owner reported durable
/// under an epoch below `epoch` among `observed` (every live line the flow saw: its samples, and a last look at an owner
/// before its loss), which never fails falsely: an adopted head at or past those positions puts the first head beyond
/// them. With no head shown (no write acknowledged), nothing is judged here; the flow's own check of that write says so.
/// The highest earlier durable position, and the violations.
fn continuity<'a>(
  observed: impl IntoIterator<Item = &'a LiveState>,
  first_head: Option<u64>,
  epoch: u64,
) -> (Option<u64>, Vec<String>) {
  let durable_before = observed
    .into_iter()
    .filter(|state| !state.holding && state.fence < epoch)
    .filter_map(|state| state.durable)
    .max();
  let violations = match (durable_before, first_head) {
    (Some(durable), Some(head)) if head <= durable => vec![format!(
      "the successor's head after its first write under epoch {epoch} is position {head}, not past position \
       {durable} made durable under an earlier epoch: it adopted a head below it"
    )],
    _ => Vec::new(),
  };
  (durable_before, violations)
}

/// Shape: how many files a successor's first write may take: one each until one is acknowledged, so a refusal while it
/// finishes its takeover is not taken for a head it never showed.
const FIRST_WRITE_FILES: usize = 20;
/// Shape: how many of the latest acknowledged writes a ReadSafety probe asks a node for: the newest are the ones a stale
/// answer would lack.
const READ_PROBES: usize = 5;

/// Derived: the longest one latest-state read may take through a node that cannot confirm its owner lease, before its
/// client gives up. The daemon parks the read for the lease bound, then refuses it `LeaseUnconfirmed`
/// (`slates_server::lease_wait`); the client sends it again until its reconnect budget has passed since that first
/// refusal (`Client::call_through_transient`), the last resend parked once more. So the lease bound, the budget and the
/// bound again: 0.9 s + 2 s + 0.9 s with the horizon and budgets of 2026-11-17.
fn read_bound() -> Duration {
  let lease = Duration::from_nanos(slates_server::lease::lease_bound_ns());
  lease
    .saturating_add(Duration::from_nanos(deadlines().reconnect_ns))
    .saturating_add(lease)
}

/// Writes through `successor` one file at a time from `files`, until one is acknowledged or the fleet wait passes, then
/// reads the head it ships `volume` from: its first head, the one [`continuity`] judges (`None` if no write was
/// acknowledged). Every outcome comes back too, for NoLoss.
fn first_write_through(
  successor: &str,
  volume: &str,
  files: std::ops::Range<usize>,
  began: &Instant,
) -> (Option<u64>, Vec<Outcome>) {
  let deadline = Instant::now() + FLEET_WAIT;
  let mut written = Vec::new();
  for file in files {
    let outcome = Stream {
      instance: successor,
      volume: volume_id_of(volume),
      began: *began,
    }
    .run(file..file + 1, &AtomicBool::new(false));
    let acknowledged = outcome.iter().any(|outcome| outcome.refused.is_none());
    written.extend(outcome);
    if acknowledged || Instant::now() >= deadline {
      break;
    }
    pause();
  }
  let head = if written.iter().any(|outcome| outcome.refused.is_none()) {
    live_states(successor, volume)
      .unwrap_or_default()
      .into_iter()
      .find(|state| !state.holding)
      .and_then(|state| state.head)
  } else {
    None
  };
  (head, written)
}

/// The refused outcomes, one line each.
fn refused_lines(outcomes: &[Outcome]) -> Vec<String> {
  outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_some())
    .map(|outcome| format!("{outcome:?}"))
    .collect()
}

/// Reads `files` of `volume` through `fleet`'s node `reader` on a thread of its own, timing each from `began`
/// ([`read_each`]), bounded so a read held for good fails the flow rather than hangs it. Once every read has had its
/// [`read_bound`], a relayed fleet is healed when `heal` allows it (a reader cut off at the relay answers only once it
/// hears its peers again); a fleet wait after that, the reader's daemon is killed, so its client is told. What each read
/// answered, and when the heal came (from `began`) if the reads needed one.
fn read_through(
  fleet: &Fleet,
  reader: usize,
  (volume, files): (&str, &[usize]),
  began: &Instant,
  heal: bool,
) -> (Vec<ReadProbe>, Option<Duration>) {
  let instance = &fleet.instances[reader];
  let bound = read_bound().saturating_mul(u32::try_from(files.len()).unwrap_or(u32::MAX));
  std::thread::scope(|scope| {
    let reads = scope.spawn(|| read_each(instance, volume, files, began));
    let waited = Instant::now();
    let mut healed = None;
    while !reads.is_finished() {
      if heal
        && healed.is_none()
        && waited.elapsed() >= bound
        && let Some(relay) = &fleet.relay
      {
        eprintln!("chaos: reads through {instance} unanswered within {bound:?}; healing the relay");
        relay.heal();
        healed = Some(began.elapsed());
      }
      if waited.elapsed() >= bound.saturating_add(FLEET_WAIT) {
        eprintln!(
          "chaos: reads through {instance} unanswered a fleet wait past {bound:?}; killing its daemon"
        );
        if let Some(pid) = fleet.live_daemon_pid(reader) {
          send_signal(pid, rustix::process::Signal::KILL);
        }
        break;
      }
      pause();
    }
    (reads.join().unwrap(), healed)
  })
}

/// Reads the newest [`READ_PROBES`] writes the stream has had acknowledged (`newest`) through every node of `fleet` at
/// once, a thread for each ([`read_through`], never healing), while `during` runs on this thread: a fault's own course
/// (the rest of its length, then its end), so every read is sent while the fault lasts. A node that cannot answer while
/// it lasts answers once it ends, or refuses; one still silent a fleet wait past its bound has its daemon killed, which
/// fails the flow. What each read answered.
fn read_everywhere_during<F: FnOnce()>(
  fleet: &Fleet,
  volume: &str,
  newest: &NewestAcknowledged,
  began: &Instant,
  during: F,
) -> Vec<ReadProbe> {
  let files = newest.newest(READ_PROBES);
  let files = files.as_slice();
  std::thread::scope(|scope| {
    let readers: Vec<_> = (0..fleet.instances.len())
      .map(|reader| {
        scope.spawn(move || read_through(fleet, reader, (volume, files), began, false).0)
      })
      .collect();
    during();
    readers
      .into_iter()
      .flat_map(|reader| reader.join().unwrap())
      .collect()
  })
}

/// [`read_everywhere_during`] with no fault to wait out: the newest acknowledged writes read through every node now.
fn read_everywhere(
  fleet: &Fleet,
  volume: &str,
  newest: &NewestAcknowledged,
  began: &Instant,
) -> Vec<ReadProbe> {
  read_everywhere_during(fleet, volume, newest, began, || {})
}

/// ReadSafety over the reads a flow sent through every node (`instances`), against the stream's `outcomes` (goal g229;
/// AC-8.1's ReadSafety, `docs/wip/models/FencedRegister.tla`). Expect: every read sent after its write's
/// acknowledgement answered that write or a refusal, never other bytes and never missing ([`read_safety`]): a candidate
/// forwards a latest-state read to the owner, so none may answer from its own copy, however its partition made it judge
/// the owner; and at least one read judged through every node, so reads that all came before any acknowledgement, or
/// only of writes refused, cannot pass for a held invariant (the non-vacuity count). With `owner` named (an owner that
/// served throughout), every judged read through it answered with its write too, since an owner whose lease one healthy
/// candidate confirms serves its latest state (§4.8 "Leases and reads": `min(f, others)` confirmations, one of two at
/// `f = 1`).
fn judge_reads(
  verdict: &mut Verdict,
  (owner, instances): (Option<&str>, &[String]),
  probes: &[ReadProbe],
  outcomes: &[Outcome],
) {
  let acknowledged = acknowledged_at(outcomes);
  let judged_reads: Vec<&ReadProbe> = probes
    .iter()
    .filter(|probe| judged(probe, &acknowledged))
    .collect();
  let mut counts = Vec::new();
  let mut unjudged = Vec::new();
  for instance in instances {
    let through: Vec<&&ReadProbe> = judged_reads
      .iter()
      .filter(|probe| probe.through == *instance)
      .collect();
    let answered = through
      .iter()
      .filter(|probe| probe.answer == ReadAnswer::Current)
      .count();
    counts.push(format!(
      "{instance} {} judged, {answered} answered",
      through.len()
    ));
    if through.is_empty() {
      unjudged.push(format!(
        "no read through {instance} was sent after its write's acknowledgement"
      ));
    }
  }
  verdict.note(format!("reads through every node: {}", counts.join("; ")));
  verdict.check(
    "ReadSafety (reads through every node)",
    read_safety(probes, outcomes),
  );
  verdict.check("ReadSafety judged a read through every node", unjudged);
  if let Some(owner) = owner {
    verdict.check(
      "the owner answered every read",
      judged_reads
        .iter()
        .filter(|probe| probe.through == owner && probe.answer != ReadAnswer::Current)
        .map(|probe| probe.to_string())
        .collect(),
    );
  }
}

/// What a takeover owes beyond NoLoss and the fences, judged from outside the processes (goal g229; AC-8.1's Continuity
/// and ReadSafety, T-8.6, §4.8 "Leases and reads"). Do: write through `successor` until one write is acknowledged (one
/// file at a time from `files`, [`first_write_through`]); then read through each node of `readers` that write and the
/// last [`READ_PROBES`] writes acknowledged before the takeover (`earlier`). Expect: the successor's first head past every
/// position an owner reported durable under an earlier epoch among `observed` (Continuity); its first write acknowledged
/// with its head shown, and served by it (NoLoss); and every read answering its write or a refusal, never other bytes and
/// never missing (ReadSafety): a survivor forwards a latest-state read to the volume's owner, so none may answer from a
/// copy it holds, or from before the takeover.
fn judge_takeover<'a>(
  verdict: &mut Verdict,
  fleet: &Fleet,
  (successor, readers): (&str, &[usize]),
  (volume, earlier): (&str, &[Outcome]),
  observed: impl IntoIterator<Item = &'a LiveState>,
  files: std::ops::Range<usize>,
) {
  let epoch = takeover_epoch(successor, volume);
  let began = Instant::now();
  let (first_head, first) = first_write_through(successor, volume, files, &began);
  let (durable_before, broken) = continuity(observed, first_head, epoch);
  verdict.note(format!(
    "Continuity: durable before {durable_before:?}, the successor's first head {first_head:?} under epoch {epoch}"
  ));
  verdict.check("Continuity", broken);
  verdict.check(
    "the successor's first write acknowledged, its head shown",
    first_write_shown(successor, first_head, &first),
  );
  verdict.check(
    "NoLoss (the successor's writes)",
    no_loss(successor, volume, &first),
  );
  // Every write acknowledged before the takeover was answered before these reads began, so its acknowledgement is put
  // at this clock's start and `read_safety` judges every read of it.
  let mut judged: Vec<Outcome> = earlier
    .iter()
    .filter(|outcome| outcome.refused.is_none())
    .rev()
    .take(READ_PROBES)
    .map(|outcome| Outcome {
      sent: Duration::ZERO,
      took: Duration::ZERO,
      ..outcome.clone()
    })
    .collect();
  judged.extend(
    first
      .iter()
      .filter(|outcome| outcome.refused.is_none())
      .cloned(),
  );
  let files: Vec<usize> = judged.iter().map(|outcome| outcome.file).collect();
  let probes: Vec<ReadProbe> = readers
    .iter()
    .flat_map(|reader| read_through(fleet, *reader, (volume, &files), &began, false).0)
    .collect();
  verdict.note(format!(
    "reads through every survivor: {}",
    probes
      .iter()
      .map(ReadProbe::to_string)
      .collect::<Vec<_>>()
      .join("; ")
  ));
  verdict.check(
    "ReadSafety (reads through every survivor)",
    read_safety(&probes, &judged),
  );
}

/// The violation when no write through `successor` was acknowledged with its head shown (`first_head`), naming the
/// refusals of its writes `first`: never empty then, so a successor that took no write at all cannot pass.
fn first_write_shown(successor: &str, first_head: Option<u64>, first: &[Outcome]) -> Vec<String> {
  match first_head {
    Some(_) => Vec::new(),
    None => vec![format!(
      "no write through {successor} acknowledged with a head shown: {:?}",
      refused_lines(first)
    )],
  }
}

/// The writes of `outcomes` that were acknowledged.
fn acknowledged_writes(outcomes: &[Outcome]) -> usize {
  outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_none())
    .count()
}

/// A relayed flow's last word on its relay (goal g229, A-330): the relay's counts, and the check that the relay carried
/// the record plane, the flow's non-vacuity. Each write a live volume acknowledged waited for a candidate's answer on
/// the record plane, one write at a time (a stream sends its next write only once the last is answered, so no answer
/// sent before a write can acknowledge it), so a relay every datagram between two nodes crosses sent on at least one
/// record-plane datagram for each of the `acknowledged` writes. Fewer means record links reached their peers around
/// the relay, and the faults the flow set there never touched the plane its writes are made durable on: before A-330 a
/// node announced its bound record port and every redial dialed it, and the cut-off owner's 2,143 acknowledged writes
/// met 1,377 datagrams of every plane toward it in the whole run.
fn judge_relayed(verdict: &mut Verdict, relay: &Relay, acknowledged: usize) {
  let report = relay.report();
  verdict.note(report.summary());
  let records = report.records_forwarded();
  verdict.check(
    "the relay carried the record plane (a record datagram an acknowledged write, at least)",
    if records >= u64::try_from(acknowledged).unwrap() {
      Vec::new()
    } else {
      vec![format!(
        "{records} record-plane datagrams crossed the relay for {acknowledged} acknowledged writes: record links \
         reached their peers around it"
      )]
    },
  );
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
  stream_through_holding(instances, owner, volume, files, |began, _, _| chaos(began))
}

/// [`stream_through`], for a chaos that reads back what the stream has had acknowledged while it runs: it gets the
/// stream's [`NewestAcknowledged`] too.
fn stream_through_reading<F: FnOnce(&Instant, &NewestAcknowledged)>(
  instances: &[String],
  owner: &str,
  volume: &str,
  files: std::ops::Range<usize>,
  chaos: F,
) -> (Vec<Outcome>, Vec<Sample>) {
  stream_through_holding(instances, owner, volume, files, |began, _, newest| {
    chaos(began, newest);
  })
}

/// [`stream_through`], for a chaos that stops a node and leaves it stopped past the stream's end: it gets the
/// [`SamplerHolds`] to take that node out of the sampling first, and the stream's [`NewestAcknowledged`].
fn stream_through_holding<F: FnOnce(&Instant, &SamplerHolds, &NewestAcknowledged)>(
  instances: &[String],
  owner: &str,
  volume: &str,
  files: std::ops::Range<usize>,
  chaos: F,
) -> (Vec<Outcome>, Vec<Sample>) {
  let began = Instant::now();
  let holds = SamplerHolds::new(instances.len());
  let newest = NewestAcknowledged::default();
  let stop_stream = AtomicBool::new(false);
  let stop_sampling = AtomicBool::new(false);
  std::thread::scope(|scope| {
    let sampler = scope.spawn(|| {
      Sampler {
        instances,
        volume,
        began,
        holds: &holds,
      }
      .run(&stop_sampling)
    });
    let streamer = scope.spawn(|| {
      Stream {
        instance: owner,
        volume: volume_id_of(volume),
        began,
      }
      .run_marking(files, &stop_stream, Some(&newest))
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
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      chaos(&began, &holds, &newest)
    }))
    .err();
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
    let samples = sampler.join().unwrap();
    print_slow_windows(instances, &outcomes, &samples);
    (outcomes, samples)
  })
}

/// Lets the stream run until `count` more positions of the owner's live log are durable than at the call, within the
/// fleet wait ([`until_durable`]; the chaos thread's way to put an event mid-stream): every acknowledged write moves the
/// owner's durable position by one at least.
fn after_writes(owner: &str, volume: &str, count: u64) {
  let from = owned_durable(owner, volume).unwrap_or(0);
  until_durable(owner, volume, from.saturating_add(count));
}

/// `owner`'s durable position of `volume`'s live log as its own line reports it, `None` while it shows none.
fn owned_durable(owner: &str, volume: &str) -> Option<u64> {
  live_states(owner, volume)
    .unwrap_or_default()
    .into_iter()
    .find(|state| !state.holding)
    .and_then(|state| state.durable)
}

/// Waits until `owner`'s live log of `volume` is durable at `target` or past it, within the fleet wait from the call;
/// the stall guard: a write the daemon never answers fails the flow with the fleet printed.
fn until_durable(owner: &str, volume: &str, target: u64) {
  let from = owned_durable(owner, volume);
  let deadline = Instant::now() + FLEET_WAIT;
  while owned_durable(owner, volume).unwrap_or(0) < target {
    if Instant::now() >= deadline {
      let instances = DIAGNOSED.with(|instances| instances.borrow().clone());
      panic!(
        "the stream made {owner}'s log durable at {target} (from {from:?}, now {:?}):\n{}",
        owned_durable(owner, volume),
        diagnosis(&instances, volume)
      );
    }
    pause();
  }
}

/// Format: the opt-in that prints the samples around each slow write ([`print_slow_windows`]).
const PRINT_SLOW: &str = "SLATES_CHAOS_PRINT_SLOW";
/// Shape: how far before a slow write's send its window of samples begins: enough to show the copies' state as it was
/// sent.
const SLOW_WINDOW_BEFORE: Duration = Duration::from_millis(500);
/// Shape: how far past a slow write's answer its window of samples ends.
const SLOW_WINDOW_AFTER: Duration = Duration::from_millis(200);

/// With [`PRINT_SLOW`] set, prints for every write that took a liveness budget or more the samples from
/// [`SLOW_WINDOW_BEFORE`] before its send to [`SLOW_WINDOW_AFTER`] after its answer, in time order: when, which node,
/// and its live line. That separates a reply not folded (the healthy holder's head moved, the owner's durable did not)
/// from a durable position computed from the stopped holder (live-durability, 2026-11-07).
fn print_slow_windows(instances: &[String], outcomes: &[Outcome], samples: &[Sample]) {
  if std::env::var_os(PRINT_SLOW).is_none() {
    return;
  }
  let bound = Duration::from_nanos(slates_server::daemon::LIVENESS_BUDGET_NS);
  for outcome in outcomes.iter().filter(|outcome| outcome.took >= bound) {
    let from = outcome.sent.saturating_sub(SLOW_WINDOW_BEFORE);
    let to = outcome.sent + outcome.took + SLOW_WINDOW_AFTER;
    let mut window: Vec<&Sample> = samples
      .iter()
      .filter(|sample| sample.at >= from && sample.at <= to)
      .collect();
    window.sort_by_key(|sample| sample.at);
    eprintln!(
      "chaos slow write {} sent {:?} took {:?} refused {:?}: {} samples",
      chaos_path(outcome.file),
      outcome.sent,
      outcome.took,
      outcome.refused,
      window.len()
    );
    for sample in window {
      eprintln!(
        "  {:>10.3?} {} {} head={:?} durable={:?} owed={} epoch={:?} fence={}",
        sample.at,
        instances.get(sample.node).map_or("?", String::as_str),
        if sample.state.holding {
          "held"
        } else {
          "shipping"
        },
        sample.state.head,
        sample.state.durable,
        sample.state.owed,
        sample.state.epoch,
        sample.state.fence
      );
    }
  }
}

/// Every node's sessions, live logs and live counters, as `status` prints them: what a stalled flow shows. Also what a
/// fleet that never settled shows ([`wait_settled`]: its council and configuration, its takeovers and their promises),
/// with `volume` empty, which keeps every volume's live log.
fn diagnosis(instances: &[String], volume: &str) -> String {
  instances
    .iter()
    .map(|instance| {
      let (code, status, _) = run(instance, &["status"]);
      let lines: Vec<&str> = status
        .lines()
        .filter(|line| {
          line.starts_with("fleet_session ")
            || line.starts_with("fleet_members")
            || line.contains(" live ")
            || line.contains("fleet.live.")
            || line.contains("fleet.takeover")
            || line.contains("fleet.materialize")
            || line.contains("fleet.seal.")
            || line.contains("fleet.plane.")
            || line.contains("fleet.report.")
            || line.contains("fleet.ship.")
            || line.starts_with("fleet_record_link")
            || line.contains("fleet.link.")
            || line.contains("fleet.accept.")
            || line.starts_with("fleet_formed_probe_peers")
            || line.starts_with("fleet_contacts")
            || line.contains("fleet.promotion")
            || line.contains("fleet.record")
            || line.contains("fleet.owner_location")
            || line.starts_with("fleet_retirement")
            || line.starts_with("fleet_settled_generation")
            || line.starts_with("fleet_neighbourhood_generation")
            || line.starts_with("fleet_takeovers_pending")
            || line.starts_with("fleet_held_records")
            || line.starts_with("fleet_configuration_version")
            || line.starts_with("fleet_configuration_members")
            || line.starts_with("fleet_council")
            || line.contains("anchor")
            || line.starts_with("fleet_host")
            || line.starts_with("fleet_peers_")
            || line.contains("lent_to")
            || line.contains("lease")
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
/// counts toward phase one only once it holds the log again. The takeover is held to Continuity and ReadSafety too
/// ([`judge_takeover`]).
#[test]
fn a_candidate_killed_mid_stream_catches_up_and_its_copy_serves_a_takeover() {
  if !gated("candidate-killed") {
    return;
  }
  let mut fleet = Fleet::start("killed", Supervision::Anchored);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let killed_index = fleet.others(owner_index)[0];
  let volume = fleet.create_volume(&owner, "killed-candidate", true);
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
  // The owner's whole node lost: the survivors, one of them the succeeded candidate, take the volume over. Only once the
  // council has seated the succeeded candidate: a second failure inside the voter change is past `f`.
  let waited = wait_recovered(&fleet.instances);
  verdict.note(format!(
    "recovered from the candidate's loss after a further {waited:?}"
  ));
  // The owner's last word before its loss, for Continuity: what it held durable then.
  let last_look = live_states(&owner, &volume).unwrap_or_default();
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
      judge_takeover(
        &mut verdict,
        &fleet,
        (successor, &fleet.others(owner_index)),
        (&volume, &outcomes),
        samples.iter().map(|sample| &sample.state).chain(&last_look),
        STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
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
      if serves && took_over(survivor, volume) {
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

/// Shape: how long each candidate is partitioned in turn: past the liveness budget (1 s), so its sessions are found
/// dead.
const CANDIDATE_PARTITION: Duration = Duration::from_secs(2);

/// A-200 with candidates partitioned in turn mid-stream: do stream writes to a live volume and partition each candidate
/// in turn for [`CANDIDATE_PARTITION`] as `partition` says, the stream running throughout, then end the partition; read
/// the newest acknowledged writes through every node halfway through each partition and again once it has ended.
/// Expect every write acknowledged (durable through whichever candidate answers), none lost, both candidates caught up
/// once healed, one chain identity at every head any copy reported, no takeover ([`no_takeover`]): a partitioned
/// candidate is not its owner's loss, however the partition makes it judge the owner; and every read answering its
/// write or a refusal, through the partitioned candidate too, the owner's every one its write ([`judge_reads`]).
fn candidates_partitioned_in_turn(tag: &str, flow: &'static str, partition: Partition) {
  if !gated(tag) {
    return;
  }
  let fleet = partition.fleet(tag);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = fleet.create_volume(&owner, tag, true);
  let mut verdict = Verdict::new(flow);
  let mut epoch_before = None;
  let mut probes = Vec::new();
  let (outcomes, samples) = stream_through_reading(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began, newest| {
      for index in fleet.others(owner_index) {
        after_writes(&owner, &volume, BETWEEN_EVENTS);
        if epoch_before.is_none() {
          epoch_before = Some(serving_epoch(&owner, &volume));
        }
        let ((from, to), read) =
          partition_a_candidate(&fleet, (index, &volume), partition, (began, newest));
        probes.extend(read);
        verdict.note(format!(
          "{} {partition:?} from {from:?} to {to:?}",
          fleet.instances[index]
        ));
      }
      after_writes(&owner, &volume, BETWEEN_EVENTS);
    },
  );
  verdict.check("every write acknowledged", refused_lines(&outcomes));
  judge_healed(
    &mut verdict,
    &owner,
    &candidates,
    &volume,
    &outcomes,
    &samples,
  );
  verdict.check(
    "no takeover while the owner served (a partitioned candidate deposes no one)",
    no_takeover(&owner, &candidates, &volume, epoch_before.unwrap_or(0)),
  );
  judge_reads(
    &mut verdict,
    (Some(&owner), &fleet.instances),
    &probes,
    &outcomes,
  );
  if let Some(relay) = &fleet.relay {
    judge_relayed(&mut verdict, relay, acknowledged_writes(&outcomes));
  }
  drop(fleet);
  verdict.judge();
}

/// Partitions `fleet`'s node `index` as `partition` says for [`CANDIDATE_PARTITION`], reading the stream's newest
/// acknowledged writes (`newest`) through every node halfway in, when the partitioned node has judged its peers from its
/// side for a liveness budget, and again once the partition has ended ([`read_everywhere_during`]). When the partition
/// began and ended (from the stream's start, `began`), and the reads.
fn partition_a_candidate(
  fleet: &Fleet,
  (index, volume): (usize, &str),
  partition: Partition,
  (began, newest): (&Instant, &NewestAcknowledged),
) -> ((Duration, Duration), Vec<ReadProbe>) {
  partition.begin(fleet, index);
  let from = began.elapsed();
  sleep(CANDIDATE_PARTITION / 2);
  let mut ended = from;
  let mut probes = read_everywhere_during(fleet, volume, newest, began, || {
    sleep(CANDIDATE_PARTITION / 2);
    partition.end(fleet, index);
    ended = began.elapsed();
  });
  probes.extend(read_everywhere(fleet, volume, newest, began));
  ((from, ended), probes)
}

/// That no survivor took `volume` over while its owner still served it: the owner ships under the epoch it shipped under
/// before (`epoch_before`), and no candidate counted a takeover (`fleet.live.taken_over`). A partitioned candidate may
/// hold the owner dead from its side, but that is one voter's view: the council's leader retires a member only once
/// `f + 1` voters hold it dead, and a report below the incarnation the member refuted at counts for nothing (§4.8
/// "Who declares a death", A-250), while the other candidate still hears the owner. A takeover follows only a
/// retirement, so none may run (T-8.5's "no false deaths"). The violations.
fn no_takeover(owner: &str, candidates: &[String], volume: &str, epoch_before: u64) -> Vec<String> {
  let epoch_now = serving_epoch(owner, volume);
  let deposed = (epoch_now != epoch_before).then(|| {
    format!(
      "the owner {owner} ships under epoch {epoch_now}, not {epoch_before} as before the partitions"
    )
  });
  let taken = candidates.iter().filter_map(|candidate| {
    let count = counter(candidate, "fleet.live.taken_over");
    (count > 0).then(|| format!("{candidate} counted {count} takeovers"))
  });
  deposed.into_iter().chain(taken).collect()
}

/// [`candidates_partitioned_in_turn`] with each candidate's process stopped (SIGSTOP): it does nothing while stopped,
/// its timers with it.
#[test]
fn candidates_partitioned_in_turn_mid_stream_lose_nothing() {
  candidates_partitioned_in_turn(
    "rolling",
    "candidates partitioned in turn mid-stream",
    Partition::Stopped,
  );
}

/// [`candidates_partitioned_in_turn`] with each candidate cut off both ways at the fault relay while it runs (goal g229):
/// from its side the owner and the other candidate fall silent, so it suspects them and may try a takeover, which it must
/// never win.
#[test]
fn candidates_cut_off_in_turn_while_running_lose_nothing_and_depose_no_one() {
  candidates_partitioned_in_turn(
    "rolling-cut",
    "candidates cut off in turn while running",
    Partition::Isolated,
  );
}

/// [`candidates_partitioned_in_turn`] with each candidate deafened in turn (goal g229): it hears nothing and still speaks,
/// so its suspicion of the owner, and any promotion it asks for, reach every peer while their answers never reach it.
#[test]
fn candidates_deafened_in_turn_lose_nothing_and_depose_no_one() {
  candidates_partitioned_in_turn(
    "rolling-deaf",
    "candidates deafened in turn",
    Partition::Deafened,
  );
}

/// [`candidates_partitioned_in_turn`] with each candidate muted in turn (goal g229): it hears every entry the owner ships
/// and can acknowledge none, so the owner's writes are durable through the other candidate alone.
#[test]
fn candidates_muted_in_turn_lose_nothing_and_depose_no_one() {
  candidates_partitioned_in_turn("rolling-mute", "candidates muted in turn", Partition::Muted);
}

/// A-200 under injected jitter: do stream writes to a live volume while each candidate's process is stopped for a
/// random [`STALL_MIN`]–[`STALL_MAX`] at random gaps of up to [`GAP_MAX`], the two independently (so at times both are
/// stopped and no write can become durable until one resumes); read the newest acknowledged writes through every node
/// halfway through the stalls and once they have ended. Expect every write acknowledged and none lost, both candidates
/// caught up, one identity at every head, and every read answering its write or a refusal, the owner's every one its
/// write ([`judge_reads`]); the client-seen p50, p99, p999 and max are printed.
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
  let volume = fleet.create_volume(&owner, "jitter", true);
  let mut verdict = Verdict::new("writes through random candidate stalls");
  let pids: Vec<u32> = fleet
    .others(owner_index)
    .into_iter()
    .map(|index| fleet.daemon_pid(index))
    .collect();
  let mut stalls = 0u64;
  let mut probes = Vec::new();
  let (outcomes, samples) = stream_through_reading(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began, newest| {
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      std::thread::scope(|scope| {
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
        // Halfway through the stalls, read through every node while they go on.
        sleep(STALLING / 2);
        probes.extend(read_everywhere(&fleet, &volume, newest, began));
        stalls = handles
          .into_iter()
          .map(|handle| handle.join().unwrap())
          .sum();
      });
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      probes.extend(read_everywhere(&fleet, &volume, newest, began));
    },
  );
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
  judge_reads(
    &mut verdict,
    (Some(&owner), &fleet.instances),
    &probes,
    &outcomes,
  );
  drop(fleet);
  verdict.judge();
}

/// The survivor among `survivors` that ships `volume` (has taken it over), once one does, within the fleet wait.
fn wait_taken_over<'a>(survivors: &[&'a str], volume: &str) -> Option<(&'a str, Duration)> {
  let began = Instant::now();
  while began.elapsed() < FLEET_WAIT {
    if let Some(successor) = survivors
      .iter()
      .find(|survivor| took_over(survivor, volume))
    {
      return Some((successor, began.elapsed()));
    }
    pause();
  }
  None
}

/// Whether `survivor` took `volume` over: it counted its rebuild from the live log, or it ships the volume (it ships only
/// from its next publication on). Serving the volume's files is no sign of it, since every survivor forwards a read to
/// the successor (§4.8 "Lookup").
fn took_over(survivor: &str, volume: &str) -> bool {
  counter(survivor, "fleet.live.taken_over") > 0 || serving_epoch(survivor, volume) > 0
}

/// A-200, StaleNeverCommits, NoLoss, ReadSafety and Continuity with the owner's process stopped mid-stream (SIGSTOP, the
/// unprivileged partition of a process that does nothing while it lasts, its timers with it): [`an_owner_partitioned`]
/// with [`Partition::Stopped`]. Its lease lapses by the clock while it is stopped, so the read through it the moment it
/// resumes, before it has heard of its successor, is the lease rule's sharpest case: a resumed owner must refuse, or
/// answer what its successor acknowledged, never its own older state.
#[test]
fn an_owner_partitioned_mid_stream_is_fenced_and_loses_nothing() {
  an_owner_partitioned(
    "owner-stop",
    "an owner partitioned mid-stream",
    Partition::Stopped,
  );
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
  let volume = fleet.create_volume(&owner, "destroy-wait", true);
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
  let volume = fleet.create_volume(&owner, "cut-waiting", true);
  let mut verdict = Verdict::new("clients killed while their writes wait");
  let began = Instant::now();
  let stop_sampling = AtomicBool::new(false);
  let holds = SamplerHolds::new(fleet.instances.len());
  let samples = std::thread::scope(|scope| {
    let sampler = scope.spawn(|| {
      Sampler {
        instances: &fleet.instances,
        volume: &volume,
        began,
        holds: &holds,
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
  let volume = fleet.create_volume(&owner, tag, live);
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
      "an unanswered destroy settles: destroyed everywhere (NotFound or OwnerUnreachable, never Overloaded), or served whole",
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
/// every head, and the survivors' takeover serving every acknowledged write, held to Continuity and ReadSafety
/// ([`judge_takeover`]).
#[test]
fn a_candidates_store_destroyed_mid_stream_is_rebuilt_and_serves_a_takeover() {
  if !gated("store-destroyed") {
    return;
  }
  let mut fleet = Fleet::start("replaced", Supervision::Anchored);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let replaced_index = fleet.others(owner_index)[0];
  let volume = fleet.create_volume(&owner, "replaced-candidate", true);
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
  // The replacement seated before the owner dies: a second failure inside the voter change is past `f`.
  let waited = wait_recovered(&fleet.instances);
  verdict.note(format!(
    "recovered from the candidate's loss after a further {waited:?}"
  ));
  // The owner's last word before its loss, for Continuity: what it held durable then.
  let last_look = live_states(&owner, &volume).unwrap_or_default();
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
      verdict.check(
        "the loss report names a takeover past tolerance exactly when it went past (A-332)",
        past_tolerance_reported(successor, &volume),
      );
      judge_takeover(
        &mut verdict,
        &fleet,
        (successor, &fleet.others(owner_index)),
        (&volume, &outcomes),
        samples.iter().map(|sample| &sample.state).chain(&last_look),
        STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
      );
    }
    None => {
      verdict.check(
        "NoLoss (successor)",
        vec!["no survivor served the volume within the fleet wait".to_owned()],
      );
      // A-272, A-309: unserved, no surviving holder may have let its copy go. A takeover that adopted nothing must leave
      // every copy where it is (`reclaimed` none), for the round still waiting to recover it: past tolerance the members
      // that exist now adopt (`fleet.takeover.recovered_past_tolerance`), where before A-309 it was lost for good.
      verdict.check(
        "NoLoss (copies kept while unserved)",
        survivors
          .iter()
          .filter(|survivor| counter(survivor, "fleet.takeover.reclaimed") > 0)
          .map(|survivor| format!("{survivor} reclaimed its copy of a volume nobody serves"))
          .collect(),
      );
    }
  }
  drop(fleet);
  verdict.judge();
}

/// A-332: where `successor`'s loss report for `volume` and its counts disagree on whether its takeover went past
/// tolerance: its registers (`fleet.takeover.recovered_past_tolerance`) and its live log
/// (`fleet.live.taken_over_past_tolerance`), each part named in the report exactly when the successor counted it. Each
/// disagreement in words; empty when they agree. The flow's fleet serves this one volume, so a count is its own.
fn past_tolerance_reported(successor: &str, volume: &str) -> Vec<String> {
  let counted = [
    counter(successor, "fleet.takeover.recovered_past_tolerance") > 0,
    counter(successor, "fleet.live.taken_over_past_tolerance") > 0,
  ];
  let report = Client::connect(successor, deadlines())
    .map_err(|e| format!("{e:?}"))
    .and_then(|mut client| {
      client
        .status(volume_id_of(volume))
        .map_err(|e| format!("{e:?}"))
    });
  let taken = match report {
    Ok(report) => report.loss.taken_over,
    Err(why) => return vec![format!("{successor} answered no status of {volume}: {why}")],
  };
  let Some(taken) = taken else {
    return vec![format!(
      "{successor}'s loss report names no takeover of {volume}"
    )];
  };
  let named = [
    taken
      .past_tolerance
      .is_some_and(|past| past.registers.is_some()),
    taken
      .past_tolerance
      .is_some_and(|past| past.live_log.is_some()),
  ];
  ["registers", "live log"]
    .iter()
    .zip(counted.iter().zip(named))
    .filter(|(_, (counted, named))| **counted != *named)
    .map(|(part, (counted, named))| {
      format!(
        "{successor}: its {part} past tolerance counted {counted}, reported {named} ({taken:?})"
      )
    })
    .collect()
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
  let volume = fleet.create_volume(&owner, "quiet", true);
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
  fleet.share(&owner, &green);
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

/// §4.8 "Membership" with an anchored daemon succeeded (raft-chaos report 8): do start an anchored three-node fleet, let
/// it settle, then SIGKILL one candidate's daemon so that its anchor starts a successor under a fresh member identity.
/// Expect the fleet to re-form within the fleet wait: every node holding three members alive with both peers probed, the
/// successor voting and in the committed configuration, with a record session to each peer. Reported 2026-11-06: the
/// successor answered in 0.3-0.9 s but held three members with no peer probed for the whole wait, while each other node
/// held two members and one peer.
#[test]
fn a_candidates_daemon_succeeded_under_its_anchor_rejoins_the_fleet() {
  if !gated("succeeded-rejoins") {
    return;
  }
  let fleet = Fleet::start("rejoins", Supervision::Anchored);
  let owner_index = fleet.owner_index();
  let killed_index = fleet.others(owner_index)[0];
  let began = Instant::now();
  let successor = fleet.kill_daemon_and_await_successor(killed_index);
  eprintln!(
    "rejoins: successor daemon {successor} answered after {:?}",
    began.elapsed()
  );
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let views: Vec<_> = fleet
      .instances
      .iter()
      .map(|instance| fleet_view(instance))
      .collect();
    let formed = views.iter().all(|view| {
      view
        .as_ref()
        .is_some_and(|(_, members, probed)| *members == FLEET_NODES.len() && *probed == 2)
    });
    if formed {
      break;
    }
    if Instant::now() >= deadline {
      for instance in &fleet.instances {
        let (_, out, _) = run(instance, &["status"]);
        let lines: Vec<&str> = out
          .lines()
          .filter(|line| {
            line.starts_with("fleet_")
              || line.contains(" refused fleet.")
              || line.contains(" refused member")
              || line.contains(" refused probe")
          })
          .filter(|line| {
            !line.contains("_base_periods")
              && !line.contains("_rtt_")
              && !line.contains("_priority")
          })
          .collect();
        eprintln!("rejoins --- {instance}\n{}", lines.join("\n"));
      }
      panic!("the fleet re-formed after the successor: {views:?}");
    }
    pause();
  }
  fleet.wait_rejoined();
  eprintln!("rejoins: the fleet re-formed after {:?}", began.elapsed());
}

/// How a volume whose destroy was never answered done settled: destroyed on every survivor (none lists it, each read
/// refused `NotFound` or `OwnerUnreachable`), or served whole by one.
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
    // Destroyed: nobody lists it, nobody serves it, and every answer is NotFound or OwnerUnreachable (a destroyed
    // volume of a departed creator: once its tombstone retires no member remembers it and its id routes to the gone
    // creator; answering NotFound there is an open design item, GAPS). Never a stuck Overloaded, the successor that
    // kept its routing to a retired volume (2026-11-06).
    let destroyed = views.iter().all(|view| {
      !view.listed
        && matches!(&view.read, Some(Err(error))
          if error.contains("NotFound") || error.contains("OwnerUnreachable"))
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

/// A-200 takeover of a live volume with a long log: do stream `LONG_LOG` writes to a live volume (the log goes past
/// whole records and their deltas), let both candidates hold it, then kill the owner's node. Expect a survivor to take
/// the volume over from its live log and serve every acknowledged write, held to Continuity and ReadSafety
/// ([`judge_takeover`]). On KIND (runs 42178128960 and 42318867890,
/// 1,000 writes) both survivors held the log at its head while the successor refused to rebuild it 1,512 times
/// (`fleet.live.takeover_refused`), and the volume was never served.
#[test]
fn a_long_live_log_is_taken_over_whole() {
  /// Shape: the writes before the owner's loss: the KIND leg's unshaped count.
  const LONG_LOG: usize = 1000;
  if !gated("long-log") {
    return;
  }
  let mut fleet = Fleet::start("long-log", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = fleet.create_volume(&owner, "long-log", true);
  let mut verdict = Verdict::new("a long live log taken over");
  // Streamed in steps the stall guard watches (40 durable writes within the fleet wait each), so a write the daemon
  // never answers fails the flow with the fleet printed, where one long stream ran into the test's own timeout.
  // Each step asks for no more than the positions left before `LONG_LOG`: every write adds at least one, so the step can
  // always be met. A fixed 40 asked near the end for positions a stream of `LONG_LOG` writes never makes (from 983, 40
  // more, where the log ends near 1,002), and the flow failed there on every tree (raft, 2026-11-07; CI diag-34).
  let target = u64::try_from(LONG_LOG).unwrap();
  let (outcomes, samples) = stream_through(&fleet.instances, &owner, &volume, 0..LONG_LOG, |_| {
    loop {
      let durable = owned_durable(&owner, &volume).unwrap_or(0);
      if durable >= target {
        break;
      }
      // Each step's target is absolute, from this one reading: a step re-read later asked past the stream's end
      // (from 962, 40 more, against a log that ended at 1,001: CI diag run 44274378785, Linux x86_64, 2026-11-18).
      until_durable(
        &owner,
        &volume,
        target.min(durable.saturating_add(BETWEEN_EVENTS)),
      );
    }
  });
  verdict.note(latency_line(&outcomes));
  let (heads, disagreements) = agreement(&samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement", disagreements);
  let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
  let (caught_up, last) = converged(&owner, &refs, &volume);
  verdict.check(
    "Convergence",
    if caught_up { Vec::new() } else { vec![last] },
  );
  // The owner's last word before its loss, for Continuity: what it held durable then.
  let last_look = live_states(&owner, &volume).unwrap_or_default();
  fleet.kill_node(owner_index);
  match wait_serving(&refs, &volume, &outcomes) {
    Some((successor, took)) => {
      verdict.note(format!("taken over by {successor} after {took:?}"));
      verdict.check("NoLoss (successor)", no_loss(successor, &volume, &outcomes));
      let epoch = takeover_epoch(successor, &volume);
      verdict.check(
        "StaleNeverCommits (fences)",
        fenced_at_or_above(&refs, &volume, epoch),
      );
      judge_takeover(
        &mut verdict,
        &fleet,
        (successor, &fleet.others(owner_index)),
        (&volume, &outcomes),
        samples.iter().map(|sample| &sample.state).chain(&last_look),
        STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
      );
    }
    None => verdict.check(
      "NoLoss (successor)",
      vec![format!(
        "no survivor served the volume within the fleet wait: {:?}",
        refs
          .iter()
          .map(|survivor| (*survivor, counters(survivor, "fleet.live.")))
          .collect::<Vec<_>>()
      )],
    ),
  }
  drop(fleet);
  verdict.judge();
}

/// A-200 takeover of a live entry past one archive chunk: do write one file of `BIG` (past 1 MiB, the cap no chunk
/// opens beyond, `slates_archive::format::MAX_CHUNK_BYTES`) and two small ones to a live volume, let both candidates
/// hold the log, then kill the owner's node. Expect a survivor to take the volume over and serve all three byte for
/// byte, held to Continuity and ReadSafety ([`judge_takeover`]). The big write's entry is one publication past 1 MiB;
/// sealed as one chunk it could never be opened, so its
/// successor refused the takeover at open for good (KIND runs 42326682216 and 42331191530).
#[test]
fn a_live_entry_past_one_chunk_is_taken_over_whole() {
  /// Shape: the big file: one and a half archive chunks.
  const BIG: usize = 3 << 19;
  if !gated("big-entry") {
    return;
  }
  let mut fleet = Fleet::start("big-entry", Supervision::Bare);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let volume = fleet.create_volume(&owner, "big-entry", true);
  let mut verdict = Verdict::new("a live entry past one chunk taken over");
  let id = volume_id_of(&volume);
  let big: Vec<u8> = (0..BIG).map(|at| u8::try_from(at % 251).unwrap()).collect();
  let files: Vec<(String, Vec<u8>)> = vec![
    ("small-0".to_owned(), chaos_body(0)),
    ("big".to_owned(), big),
    ("small-1".to_owned(), chaos_body(1)),
  ];
  let mut client = Client::connect(&owner, deadlines()).unwrap();
  let attachment = client.attach(id, None, Intent::Write).unwrap().attachment;
  for (name, bytes) in &files {
    client
      .fs_write((id, attachment), name, bytes, FILE_MODE)
      .unwrap_or_else(|error| panic!("write {name}: {error}"));
  }
  let _ = client.detach(attachment);
  drop(client);
  let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
  let (caught_up, last) = converged(&owner, &refs, &volume);
  verdict.check(
    "Convergence",
    if caught_up { Vec::new() } else { vec![last] },
  );
  // The owner's last word before its loss, for Continuity: what it held durable then (this flow samples nothing else).
  let last_look = live_states(&owner, &volume).unwrap_or_default();
  fleet.kill_node(owner_index);
  let began = Instant::now();
  // The successor is the survivor that took the volume over and serves the three files: every survivor serves them once
  // it forwards to the successor, so the first to serve them may be a forwarder, which ships nothing and so shows no head
  // after a write through it (the integrator's run on bfc34cace judged a forwarder so).
  let served = loop {
    let whole = refs.iter().find(|survivor| {
      took_over(survivor, &volume)
        && Client::connect(survivor, deadlines()).is_ok_and(|mut reader| {
          files.iter().all(|(name, bytes)| {
            reader
              .read(id, &format!("/{name}"), ReadAt::Head)
              .is_ok_and(|read| read == *bytes)
          })
        })
    });
    if whole.is_some() || began.elapsed() > FLEET_WAIT {
      break whole.map(|survivor| ((*survivor).to_owned(), began.elapsed()));
    }
    pause();
  };
  verdict.note(format!(
    "served whole after the owner's loss by the survivor that took it over: {served:?}; every survivor's live lines: {:?}",
    refs
      .iter()
      .map(|survivor| (*survivor, live_states(survivor, &volume)))
      .collect::<Vec<_>>()
  ));
  verdict.check(
    "NoLoss (successor)",
    if served.is_some() {
      Vec::new()
    } else {
      vec![format!(
        "no survivor served the three files: {:?}",
        refs
          .iter()
          .map(|survivor| (*survivor, counters(survivor, "fleet.live.")))
          .collect::<Vec<_>>()
      )]
    },
  );
  if let Some((successor, _)) = &served {
    let epoch = takeover_epoch(successor, &volume);
    verdict.check(
      "StaleNeverCommits (fences)",
      fenced_at_or_above(&refs, &volume, epoch),
    );
    judge_takeover(
      &mut verdict,
      &fleet,
      (successor, &fleet.others(owner_index)),
      (&volume, &[]),
      &last_look,
      STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
    );
  }
  drop(fleet);
  verdict.judge();
}

/// The nodes of `fleet` holding a council seat, by index, as the bootstrap node's status names them.
fn council_voters_by_index(fleet: &Fleet) -> Vec<usize> {
  let (_, out, _) = run(&fleet.instances[fleet.bootstrap], &["status"]);
  let seated: Vec<String> = value_of(&out, "fleet_council_voter_ids")
    .split_whitespace()
    .map(str::to_owned)
    .collect();
  (0..fleet.instances.len())
    .filter(|index| {
      fleet_view(&fleet.instances[*index]).is_some_and(|(host, _, _)| seated.contains(&host))
    })
    .collect()
}

/// The nodes of `fleet` holding a copy of `volume`'s log as candidates, by index.
fn holders_of(fleet: &Fleet, volume: &str) -> Vec<usize> {
  (0..fleet.instances.len())
    .filter(|index| {
      live_states(&fleet.instances[*index], volume)
        .unwrap_or_default()
        .iter()
        .any(|state| state.holding)
    })
    .collect()
}

/// A-240, phase one with a member that is not a holder: on a five-node fleet (f = 1, so a volume has two holders beside
/// its owner, and the council seats three voters), do stream writes to a live volume owned by a member without a seat,
/// replace a node that holds none of the log (a fresh member identity joins), and stop one holder (SIGSTOP) while the
/// writes go on through the other, so it falls behind. Then stop the stream, crash the owner and resume the stale
/// holder. Both holders answer the registers' takeover (the head and catalog promotion needs `f + 1` of the volume's
/// original candidates); the moment a survivor has adopted them, the holder with the log is stopped for
/// [`HOLDER_PAUSE`], past one phase-one exchange, so the live log's phase one meets the stale holder and members that
/// never held the log. Expect no survivor ever to serve the volume without every acknowledged write (NoLoss), and, when
/// phase one ran inside the pause, that it waited for the holder (`takeover_waits_quorum`) rather than counting another
/// member in its place; once the holder resumes, a survivor serves the volume whole. This is KIND's shape: the holder's
/// reply over a shaped path missed the 1 s exchange, and the successor counted itself and the replacement pod (runs
/// 42326682216, 42331191530, 42341005472: a holder left at fence 1 after a takeover at epoch 2).
#[test]
fn a_member_that_never_held_the_log_never_stands_in_for_its_holder() {
  /// Shape: how long the holder with the log is stopped once the registers are adopted: past one phase-one exchange
  /// (the liveness budget, 1 s) and the record period after it.
  const HOLDER_PAUSE: Duration = Duration::from_secs(3);
  if !gated("fresh-member") {
    return;
  }
  let mut fleet = Fleet::start_with("fresh-member", Supervision::Bare, &WIDE_NODES);
  let instances = fleet.instances.clone();
  // The owner holds no council seat, so its loss costs the council no vote and the takeover can run with a holder
  // stopped too (the council seats three of the five).
  let seated = council_voters_by_index(&fleet);
  let owner_index = (0..instances.len())
    .filter(|index| !seated.contains(index))
    .min_by_key(|index| *index == fleet.bootstrap)
    .expect("a five-member fleet seats three voters, so two members hold no seat");
  let owner = instances[owner_index].clone();
  let volume = fleet.create_volume(&owner, "fresh-member", true);
  let mut verdict = Verdict::new("a member that never held the log at a takeover");
  verdict.note(format!(
    "owner {owner}; council seats at {:?}",
    seated
      .iter()
      .map(|index| instances[*index].as_str())
      .collect::<Vec<_>>()
  ));
  let mut roles = None;
  let fleet_cell = std::cell::RefCell::new(&mut fleet);
  // Read before the stale holder stops: a `status` call to a stopped node returns only when its client gives up.
  let mut stale_adopted = 0;
  let mut stale_stopped = None;
  let (outcomes, samples) = stream_through_holding(
    &instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began, holds, _| {
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      let holders = wait_holders(&fleet_cell.borrow(), &volume, 2);
      // A member that holds none of the log is replaced: its successor joins under a fresh identity. One without a seat
      // first, so the council's seats stay where they were.
      let seated = council_voters_by_index(&fleet_cell.borrow());
      let replaced = fleet_cell
        .borrow()
        .others(owner_index)
        .into_iter()
        .filter(|index| !holders.contains(index))
        .min_by_key(|index| seated.contains(index))
        .unwrap();
      fleet_cell.borrow_mut().kill_node(replaced);
      fleet_cell.borrow_mut().replace_node(replaced);
      fleet_cell.borrow().wait_rejoined();
      verdict.note(format!(
        "{} replaced by a fresh member, rejoined at {:?}",
        instances[replaced],
        began.elapsed()
      ));
      // The replacement may move the volume's placement, and the shipper then moves its holders by a transition: the
      // roles are the holders its durability counts once no transition is in flight.
      let holders = settled_holders(&fleet_cell.borrow(), owner_index, &volume);
      verdict.note(format!(
        "the owner's holders once settled: {:?}",
        holders
          .iter()
          .map(|index| instances[*index].as_str())
          .collect::<Vec<_>>()
      ));
      // The replacement is within the tolerance only once every node's neighbourhood has settled onto it: before then a
      // volume's records may still sit on the replaced member's old identity, and the owner's loss is a second
      // failure at f = 1, which the register takeover rightly calls lost (diag-28 run 8; live-fixes, 2026-11-07).
      wait_neighbourhood_settled(&instances);
      // The holder stopped at the takeover: one whose seat, with the owner's, leaves the council two of its three votes.
      let seated = council_voters_by_index(&fleet_cell.borrow());
      let owner_seated = seated.contains(&owner_index);
      let current = holders
        .iter()
        .copied()
        .find(|holder| !(owner_seated && seated.contains(holder)))
        .expect("a holder whose stop with the owner's loss leaves the council a majority");
      let stale = holders
        .iter()
        .copied()
        .find(|holder| *holder != current)
        .unwrap();
      roles = Some((current, stale));
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      stale_adopted = counter(&instances[stale], "fleet.takeover.adopted");
      holds.hold(stale);
      fleet_cell.borrow().stop(stale);
      stale_stopped = Some(Instant::now());
      verdict.note(format!(
        "{} stopped at {:?}",
        instances[stale],
        began.elapsed()
      ));
      // One write durable without it makes it stale. More only lengthens its stop, and a stop past the failure detector
      // retires it: with the owner's crash that is two failures at f = 1, which the register takeover rightly calls lost
      // (diag-30 run 4, diag-28 run 8: the stale holder's configuration frozen at version 8 while the others moved on).
      after_writes(&owner, &volume, 1);
    },
  );
  let (current, stale) = roles.expect("the flow chose its holders' roles");
  let survivors: Vec<&str> = (0..instances.len())
    .filter(|index| *index != owner_index)
    .map(|index| instances[index].as_str())
    .collect();
  let adopted_before = stale_adopted
    + survivors
      .iter()
      .filter(|survivor| **survivor != instances[stale].as_str())
      .map(|survivor| counter(survivor, "fleet.takeover.adopted"))
      .sum::<u64>();
  // The writes since the stale holder stopped are durable on the owner and `current` alone. Once the stream has stopped
  // (every write answered), the owner is crashed and the stale holder resumed: nothing reaches it any more. The owner's
  // last word before its loss is kept for Continuity: what it held durable then.
  let last_look = live_states(&owner, &volume).unwrap_or_default();
  fleet.kill_node(owner_index);
  fleet.resume(stale);
  if let Some(stopped) = stale_stopped {
    verdict.note(format!(
      "the stale holder stayed stopped {:?}",
      stopped.elapsed()
    ));
  }
  let acknowledged = outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_none())
    .count();
  verdict.note(format!(
    "{acknowledged} writes acknowledged; owner crashed; holders {} (with the log) and {} (stale)",
    instances[current], instances[stale]
  ));
  // The moment a survivor has adopted the registers, the holder with the log is stopped, so the live log's phase one
  // runs without it.
  let deadline = Instant::now() + FLEET_WAIT;
  let adopted_by = loop {
    if let Some(survivor) = survivors.iter().find(|survivor| {
      counter(survivor, "fleet.takeover.adopted") > 0
        && survivors
          .iter()
          .map(|survivor| counter(survivor, "fleet.takeover.adopted"))
          .sum::<u64>()
          > adopted_before
    }) {
      break Some(*survivor);
    }
    if Instant::now() >= deadline {
      break None;
    }
    pause();
  };
  let Some(adopted_by) = adopted_by else {
    let live: Vec<String> = survivors
      .iter()
      .map(|survivor| (*survivor).to_owned())
      .collect();
    verdict.note(format!(
      "no survivor adopted the registers; the survivors:\n{}",
      diagnosis(&live, &volume)
    ));
    verdict.check(
      "the registers' takeover",
      vec!["no survivor adopted the volume's registers within the fleet wait".to_owned()],
    );
    drop(fleet);
    return verdict.judge();
  };
  let decisive = adopted_by != instances[current].as_str();
  if decisive {
    fleet.stop(current);
  }
  let stopped_at = Instant::now();
  verdict.note(format!(
    "{adopted_by} adopted the registers; {}",
    if decisive {
      format!("{} stopped for {HOLDER_PAUSE:?}", instances[current])
    } else {
      "it holds the log itself, so this run cannot show the defect".to_owned()
    }
  ));
  let others: Vec<&str> = survivors
    .iter()
    .copied()
    .filter(|survivor| *survivor != instances[current].as_str())
    .collect();
  let mut served_without = None;
  while decisive && stopped_at.elapsed() < HOLDER_PAUSE {
    if let Some(successor) = others.iter().find(|survivor| {
      counter(survivor, "fleet.live.taken_over") > 0 || serving_epoch(survivor, &volume) > 0
    }) {
      served_without = Some(*successor);
      break;
    }
    pause();
  }
  let waited: u64 = others
    .iter()
    .map(|survivor| counter(survivor, "fleet.live.takeover_waits_quorum"))
    .sum();
  if let Some(successor) = served_without {
    verdict.note(format!(
      "{successor} took the volume over while the holder of the log was stopped"
    ));
    verdict.check(
      "NoLoss (a takeover without the log's holder)",
      no_loss(successor, &volume, &outcomes),
    );
  }
  verdict.note(format!(
    "decisive {decisive}; phase one waited for the holders {waited} times while the holder was stopped"
  ));
  if decisive {
    fleet.resume(current);
  }
  let (heads, disagreements) = agreement(&samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement", disagreements);
  judge_served(
    &mut verdict,
    (&fleet, &survivors),
    (&volume, &outcomes),
    samples.iter().map(|sample| &sample.state).chain(&last_look),
  );
  drop(fleet);
  verdict.judge();
}

/// How many holders the takeover's old owner wrote to: the set the successor's catalog records, or, when it records
/// none, every holder the successor names.
fn write_set(recorded: &std::collections::BTreeSet<String>, named: &[String]) -> usize {
  if recorded.is_empty() {
    named
      .iter()
      .collect::<std::collections::BTreeSet<_>>()
      .len()
  } else {
    recorded.len()
  }
}

/// Whether an owner a takeover superseded could still commit, its `holders` holders with `unfenced` of them still below
/// the takeover's fence: a write needs `W = max(n − f, f)` of `n` (`slates_db::live`'s `Shipper::reached_by`, stated
/// here again so the judge does not take its rule from the code it judges), and only an unfenced holder can accept one.
fn stale_owner_can_commit(holders: usize, unfenced: usize, tolerated: usize) -> bool {
  unfenced > 0 && unfenced >= holders.saturating_sub(tolerated).max(tolerated)
}

/// StaleNeverCommits on the holders' fences: a violation only where the unfenced holders could still make a write count
/// for the superseded owner. Phase one needs `f + 1` of the record's holders, not all of them, so a holder it did not
/// wait for can stay below the fence: at load 132-244 on the Mac one of a record's three holders did, and with the other
/// two fenced the dead owner could reach none of its two-of-three writes (run 3, 2026-11-08). Requiring every named
/// holder fenced called that a violation.
fn judge_fences(verdict: &mut Verdict, holders: usize, unfenced: Vec<String>) {
  const INVARIANT: &str = "StaleNeverCommits (the counted holders' fences)";
  if stale_owner_can_commit(holders, unfenced.len(), FLEET_F) {
    verdict.check(INVARIANT, unfenced);
  } else {
    if !unfenced.is_empty() {
      verdict.note(format!(
        "below the takeover's fence, fewer than a write needs of {holders} holders: {unfenced:?}"
      ));
    }
    verdict.check(INVARIANT, Vec::new());
  }
}

/// §4.8's StaleNeverCommits as `live_chaos` judges it, on constructed fences. Do: judge holder sets with every count of
/// unfenced holders around the write count, at `f = 1` through the verdict and at `f = 2` through the rule. Expect: a
/// violation exactly where the unfenced holders reach `W = max(n − f, f)`: one of two, two of three, but not one of
/// three, and three of five at `f = 2` but not two.
#[test]
fn a_superseded_owner_can_commit_only_where_its_unfenced_holders_reach_the_write_count() {
  let judged = |holders: usize, unfenced: usize| {
    let mut verdict = Verdict::new("constructed fences");
    judge_fences(
      &mut verdict,
      holders,
      (0..unfenced)
        .map(|holder| format!("holder {holder}"))
        .collect(),
    );
    !verdict.violations.is_empty()
  };
  assert!(!judged(3, 0), "every holder fenced");
  assert!(
    !judged(3, 1),
    "one of three cannot make the two a write needs"
  );
  assert!(judged(3, 2), "two of three can");
  assert!(judged(3, 3), "all three can");
  assert!(judged(2, 1), "one of two is the write count at f = 1");
  assert!(
    !stale_owner_can_commit(5, 2, 2),
    "two of five at f = 2 is short of three"
  );
  assert!(
    stale_owner_can_commit(5, 3, 2),
    "three of five at f = 2 is the write count"
  );
}

/// The end of the fresh-member flow: a survivor serves `volume` with every acknowledged write, and every holder the
/// takeover counted is fenced at its epoch. Those are read from the successor: the holder set its catalog records (the
/// set its phase one counted, which a transition may have recorded before the old owner activated it) and its own
/// holders. A copy left on any other survivor, a member a transition dropped, is counted by no one and is noted. The
/// takeover is held to Continuity against `observed` and to ReadSafety through every survivor ([`judge_takeover`]).
fn judge_served<'a>(
  verdict: &mut Verdict,
  (fleet, survivors): (&Fleet, &[&str]),
  (volume, outcomes): (&str, &[Outcome]),
  observed: impl IntoIterator<Item = &'a LiveState>,
) {
  match wait_serving(survivors, volume, outcomes) {
    Some((successor, took)) => {
      verdict.note(format!("{successor} served the volume after {took:?}"));
      verdict.check("NoLoss (successor)", no_loss(successor, volume, outcomes));
      let epoch = takeover_epoch(successor, volume);
      let hosts: BTreeMap<String, &str> = survivors
        .iter()
        .filter_map(|survivor| fleet_view(survivor).map(|(host, _, _)| (host, *survivor)))
        .collect();
      let states = live_states(successor, volume).unwrap_or_default();
      let recorded: std::collections::BTreeSet<String> = states
        .iter()
        .flat_map(|state| state.recorded.iter().cloned())
        .collect();
      let named: Vec<String> = states
        .into_iter()
        .flat_map(|state| state.recorded.into_iter().chain(state.holders))
        .collect();
      let counted: Vec<&str> = hosts
        .iter()
        .filter(|(host, survivor)| named.contains(host) && **survivor != successor)
        .map(|(_, survivor)| *survivor)
        .collect();
      verdict.note(format!(
        "the successor's recorded and active holders among the survivors: {counted:?}"
      ));
      let unfenced = fenced_at_or_above(&counted, volume, epoch);
      if !unfenced.is_empty() {
        let mut shown: Vec<String> = vec![successor.to_owned()];
        shown.extend(counted.iter().map(|survivor| (*survivor).to_owned()));
        verdict.note(format!(
          "a counted holder unfenced; the successor and the counted holders:\n{}",
          diagnosis(&shown, volume)
        ));
      }
      judge_fences(verdict, write_set(&recorded, &named), unfenced);
      let others: Vec<&str> = survivors
        .iter()
        .copied()
        .filter(|survivor| !counted.contains(survivor) && *survivor != successor)
        .collect();
      let unfenced_others = fenced_at_or_above(&others, volume, epoch);
      if !unfenced_others.is_empty() {
        verdict.note(format!(
          "copies on members no longer holders, at the takeover: {unfenced_others:?}"
        ));
      }
      // A member a transition dropped is counted by no one, so its copy is memory held for nothing. Letting it go safely
      // needs a holder-set generation on the wire (an open design item, live-fixes, 2026-11-07: a plain release could
      // drop the copy of a host re-included meanwhile), so a copy still held at the end of the fleet wait is noted.
      let kept = copies_kept(&others, volume);
      if !kept.is_empty() {
        verdict.note(format!("copies dropped holders still keep: {kept:?}"));
      }
      let readers: Vec<usize> = (0..fleet.instances.len())
        .filter(|index| survivors.contains(&fleet.instances[*index].as_str()))
        .collect();
      judge_takeover(
        verdict,
        fleet,
        (successor, &readers),
        (volume, outcomes),
        observed,
        STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
      );
    }
    None => {
      let live: Vec<String> = survivors
        .iter()
        .map(|survivor| (*survivor).to_owned())
        .collect();
      verdict.note(format!(
        "no survivor served the volume; the survivors:\n{}",
        diagnosis(&live, volume)
      ));
      verdict.check(
        "a takeover once the holder answers",
        vec!["no survivor served the volume within the fleet wait".to_owned()],
      );
    }
  }
}

/// The members of `instances` that still hold a copy of `volume`'s log at the end of the fleet wait: none once each has let
/// its copy go.
fn copies_kept(instances: &[&str], volume: &str) -> Vec<String> {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let kept: Vec<String> = instances
      .iter()
      .filter(|instance| {
        live_states(instance, volume)
          .unwrap_or_default()
          .iter()
          .any(|state| state.holding)
      })
      .map(|instance| format!("{instance} keeps its copy"))
      .collect();
    if kept.is_empty() || Instant::now() >= deadline {
      return kept;
    }
    pause();
  }
}

/// The nodes holding `volume`'s log whose acknowledgements the owner at `owner_index` counts, once it reports two and no
/// holder transition in flight and each holds the owner's durable position, within the fleet wait.
fn settled_holders(fleet: &Fleet, owner_index: usize, volume: &str) -> Vec<usize> {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let owner = live_states(&fleet.instances[owner_index], volume)
      .unwrap_or_default()
      .into_iter()
      .find(|state| !state.holding);
    let hosts: BTreeMap<String, usize> = (0..fleet.instances.len())
      .filter_map(|index| fleet_view(&fleet.instances[index]).map(|(host, _, _)| (host, index)))
      .collect();
    if let Some(owner) = &owner
      && owner.moving_to.is_empty()
      && owner.holders.len() == 2
    {
      let indexes: Vec<usize> = owner
        .holders
        .iter()
        .filter_map(|host| hosts.get(host).copied())
        .collect();
      let caught_up = indexes.len() == 2
        && indexes.iter().all(|index| {
          live_states(&fleet.instances[*index], volume)
            .unwrap_or_default()
            .iter()
            .any(|state| state.holding && owner.durable.is_some() && state.head >= owner.durable)
        });
      if caught_up {
        return indexes;
      }
    }
    assert!(
      Instant::now() < deadline,
      "the owner's holders settled within the fleet wait: {owner:?}"
    );
    pause();
  }
}

/// The nodes of `fleet` holding a copy of `volume`'s log, once at least `count` do, within the fleet wait.
fn wait_holders(fleet: &Fleet, volume: &str, count: usize) -> Vec<usize> {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let holders = holders_of(fleet, volume);
    if holders.len() >= count {
      return holders;
    }
    assert!(
      Instant::now() < deadline,
      "{count} nodes hold the volume's log within the fleet wait: {holders:?}"
    );
    pause();
  }
}

/// How a flow's write is made durable before its answer (A-200's barrier), one per path a client has to a live volume.
#[derive(Clone, Copy, Debug)]
enum Barrier {
  /// The in-process client's `fs_write`, answered once the volume's log is durable.
  ClientWrite,
  /// A stable (`FILE_SYNC`) NFS write over the owner's export, from a userspace NFS client (never a kernel mount).
  NfsFileSync,
}

/// The value of `key` in a `--json` line, unquoted.
fn json_field(text: &str, key: &str) -> String {
  let needle = format!("\"{key}\":");
  let after = text
    .split(&needle)
    .nth(1)
    .unwrap_or_else(|| panic!("{key} in {text}"))
    .trim_start();
  match after.strip_prefix('"') {
    Some(quoted) => quoted.split('"').next().unwrap().to_owned(),
    None => after.split([',', '}']).next().unwrap().trim().to_owned(),
  }
}

/// Writes `name` with `body` to `volume` on `owner` through `barrier`, returning once the write is answered: the moment
/// the caller may kill the owner. Panics when the write is refused (the flow's precondition is a healthy fleet).
fn write_through(owner: &str, volume: &str, barrier: Barrier, name: &str, body: &[u8]) {
  match barrier {
    Barrier::ClientWrite => {
      let stream = Stream {
        instance: owner,
        volume: volume_id_of(volume),
        began: Instant::now(),
      };
      let (mut client, attachment) = stream.attach().expect("attached for writing");
      client
        .fs_write((volume_id_of(volume), attachment), name, body, FILE_MODE)
        .expect("the probe write is answered");
    }
    Barrier::NfsFileSync => {
      let (code, out, err) = run(owner, &["export", volume, "--json"]);
      assert_eq!(code, 0, "export: {err}");
      let (path, port) = (json_field(&out, "path"), json_field(&out, "port"));
      let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
      stream.set_read_timeout(Some(FLEET_WAIT)).unwrap();
      stream.set_nodelay(true).unwrap();
      let root = nfs::mount(&mut stream, &path, 1);
      let file = nfs::create(&mut stream, &root, name, 2);
      nfs::write(&mut stream, &file, body, 3);
    }
  }
}

/// Waits until one of `survivors` serves `name` of `volume` as `body`, byte for byte; which, and after how long. `None`
/// at the fleet wait.
fn wait_reads<'a>(
  survivors: &[&'a str],
  volume: &str,
  name: &str,
  body: &[u8],
) -> Option<(&'a str, Duration)> {
  let began = Instant::now();
  while began.elapsed() < FLEET_WAIT {
    for survivor in survivors {
      if let Ok(mut client) = Client::connect(survivor, deadlines())
        && client
          .read(volume_id_of(volume), &format!("/{name}"), ReadAt::Head)
          .is_ok_and(|bytes| bytes == body)
      {
        return Some((survivor, began.elapsed()));
      }
    }
    pause();
  }
  None
}

/// Runs `writes` on a thread of its own and waits for it within the fleet wait. `Err` with every node's state when it is
/// not done by then (the writes are left to fail once the fleet's daemons are killed). A write held without an answer
/// would otherwise hang the flow until its test's own timeout, with nothing printed: the kill-at-acknowledgement flow did,
/// on every iteration whose owner was a freshly joined member (CI diag-17, 2026-11-07).
fn answered_within(
  instances: &[String],
  volume: &str,
  writes: impl FnOnce() + Send,
) -> Result<(), String> {
  std::thread::scope(|scope| {
    let writer = scope.spawn(writes);
    let began = Instant::now();
    while !writer.is_finished() {
      if began.elapsed() > FLEET_WAIT {
        let state = diagnosis(instances, volume);
        for instance in instances {
          if let Some(pid) = daemon_pid(instance) {
            send_signal(pid, rustix::process::Signal::KILL);
          }
        }
        // The writer's client is told its daemon is gone and the thread ends (a panicked thread left unjoined would
        // make the scope panic over this answer).
        let _ = writer.join();
        return Err(state);
      }
      pause();
    }
    writer.join().map_err(|_| "the writes panicked".to_owned())
  })
}

/// A-200 NoLoss at the acknowledgement itself (live-durability's proposal, 2026-11-07): do, `ITERATIONS` times on one
/// fleet, make a live volume on the owner, write `BEFORE` files, then write a probe file through a barrier (alternately
/// the client's `fs_write` and a userspace NFS `FILE_SYNC` write) and SIGKILL the owner the instant the probe's answer
/// returns; then replace the dead node with a fresh member, which owns the next iteration's volume. Expect a survivor to
/// serve the probe and every file before it, byte for byte, each time: a write's answer means it is durable on `f + 1`
/// hosts, so no window between the answer and the owner's death may lose it.
#[test]
fn a_write_answered_the_instant_before_its_owner_dies_is_served_by_its_successor() {
  /// Shape: the iterations, alternating the barrier.
  const ITERATIONS: usize = 6;
  /// Shape: the files written before the probe, so the log has entries past its first whole record.
  const BEFORE: usize = 8;
  if !gated("kill-at-ack") {
    return;
  }
  let mut fleet = Fleet::start("kill-at-ack", Supervision::Bare);
  let mut verdict = Verdict::new("a write answered the instant before its owner dies");
  let owner_index = fleet.owner_index();
  for iteration in 0..ITERATIONS {
    let barrier = if iteration % 2 == 0 {
      Barrier::ClientWrite
    } else {
      Barrier::NfsFileSync
    };
    let owner = fleet.instances[owner_index].clone();
    let volume = fleet.create_volume(&owner, &format!("kill-at-ack-{iteration}"), true);
    let files: Vec<(String, Vec<u8>)> = (0..=BEFORE)
      .map(|file| {
        let name = format!("ack-{iteration}-{file}");
        let body = chaos_body(iteration * (BEFORE + 1) + file);
        (name, body)
      })
      .collect();
    let (probe, before) = files.split_last().unwrap();
    let answered = answered_within(&fleet.instances, &volume, || {
      for (name, body) in before {
        write_through(&owner, &volume, Barrier::ClientWrite, name, body);
      }
      write_through(&owner, &volume, barrier, &probe.0, &probe.1);
    });
    if let Err(state) = answered {
      verdict.check(
        &format!("every write answered, iteration {iteration} ({barrier:?})"),
        vec![format!(
          "a write on {owner} was not answered within the fleet wait:\n{state}"
        )],
      );
      break;
    }
    // The instant the probe's answer is back, the owner is gone.
    fleet.kill_node(owner_index);
    let survivors: Vec<String> = fleet
      .others(owner_index)
      .into_iter()
      .map(|index| fleet.instances[index].clone())
      .collect();
    let refs: Vec<&str> = survivors.iter().map(String::as_str).collect();
    match wait_reads(&refs, &volume, &probe.0, &probe.1) {
      Some((successor, took)) => {
        let lost: Vec<String> = before
          .iter()
          .filter(|(name, body)| {
            Client::connect(successor, deadlines())
              .ok()
              .and_then(|mut client| {
                client
                  .read(volume_id_of(&volume), &format!("/{name}"), ReadAt::Head)
                  .ok()
              })
              .is_none_or(|bytes| bytes != *body)
          })
          .map(|(name, _)| name.clone())
          .collect();
        verdict.note(format!(
          "iteration {iteration} ({barrier:?}): {successor} served the probe {took:?} after the owner's death"
        ));
        verdict.check(
          &format!("NoLoss, iteration {iteration} (earlier files)"),
          lost,
        );
      }
      None => verdict.check(
        &format!("NoLoss, iteration {iteration} ({barrier:?})"),
        vec![format!(
          "no survivor served {} within the fleet wait:\n{}",
          probe.0,
          diagnosis(&survivors, &volume)
        )],
      ),
    }
    // The dead node's replacement, a fresh member at the same index, owns the next iteration's volume.
    fleet.replace_node(owner_index);
    fleet.wait_rejoined();
  }
  drop(fleet);
  verdict.judge();
}

// ── Flows behind the fault relay (goal g229, A-277): loss across real processes, and an owner partitioned while it runs ─

/// Shape: the one-way hold of every path under the lossy profile: a metro WAN's 20 ms, a quarter of KIND's `wan` profile
/// (80 ms ± 20 ms), so each write's round trip leaves the flow well inside the fleet wait.
const LOSSY_HOLD: Duration = Duration::from_millis(20);
/// Shape: the hold's spread, a quarter of it, as KIND's profile spreads 80 ms by 20.
const LOSSY_SPREAD: Duration = Duration::from_millis(5);
/// Shape: the light loss, KIND's `wan-loss` rate (1 %).
const LIGHT_LOSS_PPM: u32 = 10_000;
/// Shape: the heavy loss, five times the light.
const HEAVY_LOSS_PPM: u32 = 50_000;
/// Shape: the duplication of the last lossy phase (2 %).
const DUPLICATE_PPM: u32 = 20_000;
/// Shape: how long the old owner's stream goes on after a partition heals: long enough to send it several writes.
const AFTER_HEAL: Duration = Duration::from_secs(2);

/// The light lossy profile: every datagram held 20 ms ± 5 ms one way, 1 % lost.
fn light_loss() -> PathRule {
  PathRule {
    delay: LOSSY_HOLD,
    jitter: LOSSY_SPREAD,
    loss_ppm: LIGHT_LOSS_PPM,
    ..PathRule::default()
  }
}

/// The lossy profiles, by name, mildest first: the light profile ([`light_loss`]); its hold losing 5 %; and its hold
/// losing 1 % with 2 % duplicated.
fn lossy_profiles() -> [(&'static str, PathRule); 3] {
  [
    ("1 % lost", light_loss()),
    (
      "5 % lost",
      PathRule {
        loss_ppm: HEAVY_LOSS_PPM,
        ..light_loss()
      },
    ),
    (
      "1 % lost, 2 % duplicated",
      PathRule {
        duplicate_ppm: DUPLICATE_PPM,
        ..light_loss()
      },
    ),
  ]
}

/// A-200 over lossy, delayed paths (goal g229; AC-8.1 and AC-8.2's invariants across real processes; the fault relay,
/// A-277): do stream writes to a live volume while every path of the fleet holds each datagram 20 ms ± 5 ms one way and
/// loses 1 % of them, then 5 %, then 1 % with 2 % duplicated, reading the newest acknowledged writes through every node
/// in each phase; heal and read again; then, back under the light profile, SIGKILL the owner and write through the
/// survivor that takes the volume over. Expect every write acknowledged and none lost or torn, both candidates at the
/// owner's head once healed, one identity at every head, every read answering its write or a refusal, the owner's every
/// one its write ([`judge_reads`]); and the takeover, over the lossy paths, serving every acknowledged write, every
/// surviving copy fenced at its epoch, and held to Continuity and ReadSafety ([`judge_takeover`]).
#[test]
fn writes_stream_through_lossy_paths_and_a_takeover_over_them_loses_nothing() {
  if !gated("lossy-paths") {
    return;
  }
  let mut fleet = Fleet::start_relayed("lossy", Supervision::Bare, &FLEET_NODES);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let survivors: Vec<&str> = candidates.iter().map(String::as_str).collect();
  let volume = fleet.create_volume(&owner, "lossy", true);
  let mut verdict = Verdict::new("writes through lossy paths, then a takeover over them");
  let mut probes = Vec::new();
  let (outcomes, samples) = stream_through_reading(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began, newest| {
      let relay = fleet.relay();
      for (phase, rule) in lossy_profiles() {
        relay.shape_all(rule);
        verdict.note(format!(
          "every path held {LOSSY_HOLD:?} ± {LOSSY_SPREAD:?}, {phase}, from {:?}",
          began.elapsed()
        ));
        after_writes(&owner, &volume, BETWEEN_EVENTS);
        probes.extend(read_everywhere(&fleet, &volume, newest, began));
      }
      relay.heal();
      probes.extend(read_everywhere(&fleet, &volume, newest, began));
    },
  );
  verdict.check("every write acknowledged", refused_lines(&outcomes));
  judge_healed(
    &mut verdict,
    &owner,
    &candidates,
    &volume,
    &outcomes,
    &samples,
  );
  judge_reads(
    &mut verdict,
    (Some(&owner), &fleet.instances),
    &probes,
    &outcomes,
  );
  fleet.relay().shape_all(light_loss());
  // The owner's last word before its loss, for Continuity: what it held durable then.
  let last_look = live_states(&owner, &volume).unwrap_or_default();
  fleet.kill_node(owner_index);
  match wait_serving(&survivors, &volume, &outcomes) {
    Some((successor, took)) => {
      verdict.note(format!(
        "taken over by {successor} {took:?} after the owner's death, over lossy paths"
      ));
      verdict.check("NoLoss (successor)", no_loss(successor, &volume, &outcomes));
      let epoch = takeover_epoch(successor, &volume);
      verdict.check(
        "StaleNeverCommits (fences)",
        fenced_at_or_above(&survivors, &volume, epoch),
      );
      judge_takeover(
        &mut verdict,
        &fleet,
        (successor, &fleet.others(owner_index)),
        (&volume, &outcomes),
        samples.iter().map(|sample| &sample.state).chain(&last_look),
        STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
      );
    }
    None => verdict.check(
      "NoLoss (successor)",
      vec![format!(
        "no survivor served the volume within the fleet wait:\n{}",
        diagnosis(&candidates, &volume)
      )],
    ),
  }
  judge_relayed(&mut verdict, fleet.relay(), acknowledged_writes(&outcomes));
  drop(fleet);
  verdict.judge();
}

/// Shape: the one-way hold on every path to and from a slow candidate: ten times the lossy profile's hold. Beside the
/// other candidate's loopback paths (tens of microseconds) that is far past T-8.3's tenfold slower candidate, so the
/// flow is the stricter case; its round trip (400 ms) stays well inside the liveness budget (1 s), so the candidate is
/// slow, never dead.
const SLOW_HOLD: Duration = Duration::from_millis(200);
/// Shape: the slow hold's spread, a quarter of it.
const SLOW_SPREAD: Duration = Duration::from_millis(50);
/// Shape: the durable writes made while one candidate is slow: four chaos events' worth.
const SLOW_WRITES: u64 = 4 * BETWEEN_EVENTS;

/// The rule on every path to and from a slow node: each datagram held [`SLOW_HOLD`] ± [`SLOW_SPREAD`].
fn slow_path() -> PathRule {
  PathRule {
    delay: SLOW_HOLD,
    jitter: SLOW_SPREAD,
    ..PathRule::default()
  }
}

/// A-200 and AC-8.12's straggler immunity across real processes (goal g229, injected latency on one replica; T-8.3's
/// slow candidate, whose probation half is not built): do stream writes to a live volume, then hold every datagram to and
/// from one candidate 200 ms ± 50 ms each way for [`SLOW_WRITES`] durable writes, reading the newest acknowledged writes
/// through every node as it begins, then heal and read again. Expect every write acknowledged and none lost, both
/// candidates caught up once healed, one identity at every head, no write waiting on the slow candidate
/// ([`slow_candidate_waited`]), and every read answering its write or a refusal, the owner's every one its write
/// ([`judge_reads`]).
#[test]
fn writes_stream_past_a_slow_candidate_without_waiting_for_it() {
  if !gated("slow-candidate") {
    return;
  }
  let fleet = Fleet::start_relayed("slow", Supervision::Bare, &FLEET_NODES);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let candidates = candidates_of(&fleet, owner_index);
  let slow = fleet.others(owner_index)[0];
  let volume = fleet.create_volume(&owner, "slow", true);
  let mut verdict = Verdict::new("writes past a slow candidate");
  let mut slowed = (Duration::ZERO, Duration::ZERO);
  let mut probes = Vec::new();
  let (outcomes, samples) = stream_through_reading(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began, newest| {
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      fleet.relay().shape_node(slow, slow_path());
      let from = began.elapsed();
      let mut to = from;
      probes.extend(read_everywhere_during(
        &fleet,
        &volume,
        newest,
        began,
        || {
          after_writes(&owner, &volume, SLOW_WRITES);
          to = began.elapsed();
        },
      ));
      slowed = (from, to);
      fleet.relay().heal();
      after_writes(&owner, &volume, BETWEEN_EVENTS);
      probes.extend(read_everywhere(&fleet, &volume, newest, began));
    },
  );
  verdict.note(format!(
    "{} slowed {SLOW_HOLD:?} ± {SLOW_SPREAD:?} each way from {:?} to {:?}",
    fleet.instances[slow], slowed.0, slowed.1
  ));
  verdict.check("every write acknowledged", refused_lines(&outcomes));
  verdict.check(
    "no write waited for the slow candidate",
    slow_candidate_waited(&outcomes, slowed),
  );
  judge_healed(
    &mut verdict,
    &owner,
    &candidates,
    &volume,
    &outcomes,
    &samples,
  );
  judge_reads(
    &mut verdict,
    (Some(&owner), &fleet.instances),
    &probes,
    &outcomes,
  );
  judge_relayed(&mut verdict, fleet.relay(), acknowledged_writes(&outcomes));
  drop(fleet);
  verdict.judge();
}

/// The violation when the writes sent while a candidate was slow (`slowed`, from the stream's start) took a median of
/// its shortest round trip or more: the owner then waited on it. It need not: a write is durable at `W = max(n − f, f)`
/// of an owner's `n` holders, one of two at `f = 1` (`slates_db::live`'s `Shipper::reached_by`), so the other candidate's
/// acknowledgement is enough.
fn slow_candidate_waited(outcomes: &[Outcome], slowed: (Duration, Duration)) -> Vec<String> {
  let mut took: Vec<Duration> = outcomes
    .iter()
    .filter(|outcome| {
      outcome.refused.is_none() && outcome.sent >= slowed.0 && outcome.sent < slowed.1
    })
    .map(|outcome| outcome.took)
    .collect();
  took.sort();
  let round_trip = SLOW_HOLD.saturating_sub(SLOW_SPREAD).saturating_mul(2);
  match took.get(took.len() / 2) {
    Some(median) if *median >= round_trip => vec![format!(
      "the median of {} writes made while it was slow took {median:?}, at least its shortest round trip {round_trip:?}",
      took.len()
    )],
    Some(_) => Vec::new(),
    None => vec!["no write was acknowledged while the candidate was slow".to_owned()],
  }
}

/// How a flow partitions one node mid-stream (goal g229).
#[derive(Clone, Copy, Debug)]
enum Partition {
  /// Its process stopped (SIGSTOP) until resumed (SIGCONT): it does nothing meanwhile, its timers with it.
  Stopped,
  /// Cut off both ways at the fault relay while its process runs on: it hears no peer and no peer hears it.
  Isolated,
  /// Cut off one way at the fault relay while its process runs on: it speaks to every peer and hears none.
  Deafened,
  /// Cut off the other way at the fault relay while its process runs on: it hears every peer and none hears it.
  Muted,
}

impl Partition {
  /// Whether the node's process runs through the partition: the flow then puts the fault relay between the nodes to
  /// partition it, and can read through it while the partition lasts.
  fn running(self) -> bool {
    !matches!(self, Partition::Stopped)
  }

  /// A fleet for a flow that partitions this way: behind the fault relay when the process runs on.
  fn fleet(self, tag: &str) -> Fleet {
    if self.running() {
      Fleet::start_relayed(tag, Supervision::Bare, &FLEET_NODES)
    } else {
      Fleet::start(tag, Supervision::Bare)
    }
  }

  /// Partitions `fleet`'s node `node`.
  fn begin(self, fleet: &Fleet, node: usize) {
    match self {
      Partition::Stopped => fleet.stop(node),
      Partition::Isolated => fleet.relay().isolate(node),
      Partition::Deafened => fleet.relay().deafen(node),
      Partition::Muted => fleet.relay().mute(node),
    }
  }

  /// Ends the partition of `fleet`'s node `node`: resumes its process, or heals every path.
  fn end(self, fleet: &Fleet, node: usize) {
    match self {
      Partition::Stopped => fleet.resume(node),
      Partition::Isolated | Partition::Deafened | Partition::Muted => fleet.relay().heal(),
    }
  }
}

/// What the chaos of [`an_owner_partitioned`] did and saw while the old owner's stream ran.
#[derive(Default)]
struct PartitionRun {
  /// When the partition began, from the stream's start.
  partitioned_at: Duration,
  /// When it ended (the relay healed, or the owner resumed), from the stream's start.
  healed_at: Duration,
  /// The survivor that took the volume over.
  successor: Option<String>,
  /// The successor's head after its first acknowledged write ([`first_write_through`]).
  first_head: Option<u64>,
  /// Every write sent through the successor.
  successor_outcomes: Vec<Outcome>,
  /// The reads of the successor's last acknowledged writes, through every survivor and through the old owner.
  probes: Vec<ReadProbe>,
}

/// Sets its flag when dropped, so a stream told to stop by it stops however the code holding it ends: a failed wait that
/// unwinds included, where the scope would otherwise wait on a stream that never stops.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
  fn drop(&mut self) {
    self.0.store(true, Ordering::Release);
  }
}

/// A-200, StaleNeverCommits, NoLoss, ReadSafety and Continuity with the owner partitioned mid-stream (goal g229; AC-8.1,
/// AC-8.2 and AC-8.13's invariants across real processes; T-8.7's partitions with no privilege). Do: stream writes to a
/// live volume through its owner; partition the owner as `partition` says; once a survivor has taken the volume over,
/// write through the successor (its first write, then a stream); read its last acknowledged writes through every
/// survivor, then through the old owner: while the partition lasts when its process runs on (its lease lapsed unrenewed
/// while it still serves its clients, its peers silent from its side), and the moment it resumes when it was stopped
/// (its lease lapsed by the clock meanwhile); heal; let the old owner's stream go on. Expect: no write sent to the old
/// owner while it was partitioned acknowledged ([`judge_stale_writes`]); every read refused or answered with the write,
/// never other bytes and never missing (ReadSafety); every write either owner acknowledged served by the successor
/// (NoLoss), those made through the old node after the heal included; the successor's first head past every position
/// durable under the old epoch (Continuity); every copy fenced at or above the successor's epoch; one identity at every
/// head; and, healed, every other copy at the successor's head.
fn an_owner_partitioned(tag: &str, flow: &'static str, partition: Partition) {
  if !gated(tag) {
    return;
  }
  let fleet = partition.fleet(tag);
  let owner_index = fleet.owner_index();
  let owner = fleet.instances[owner_index].clone();
  let volume = fleet.create_volume(&owner, tag, true);
  let mut verdict = Verdict::new(flow);
  let mut run = PartitionRun::default();
  let (outcomes, samples) = stream_through(
    &fleet.instances,
    &owner,
    &volume,
    0..STREAM_FILES,
    |began| {
      run = partition_owner_mid_stream(
        &fleet,
        (owner_index, &volume),
        partition,
        began,
        &mut verdict,
      );
    },
  );
  let acknowledged = acknowledged_writes(&outcomes) + acknowledged_writes(&run.successor_outcomes);
  judge_partition(&mut verdict, &fleet, &volume, (&outcomes, &samples), run);
  if let Some(relay) = &fleet.relay {
    judge_relayed(&mut verdict, relay, acknowledged);
  }
  drop(fleet);
  verdict.judge();
}

/// The chaos of [`an_owner_partitioned`], run while the old owner's stream goes on: partition the owner, wait for a
/// survivor to take the volume over, write through it, read its last writes through every survivor and then through the
/// old owner (while partitioned when it runs, the moment it resumes when it was stopped), heal, and let the old owner's
/// stream go on for [`AFTER_HEAL`].
fn partition_owner_mid_stream(
  fleet: &Fleet,
  (owner_index, volume): (usize, &str),
  partition: Partition,
  began: &Instant,
  verdict: &mut Verdict,
) -> PartitionRun {
  let owner = &fleet.instances[owner_index];
  let candidates = candidates_of(fleet, owner_index);
  let survivors: Vec<&str> = candidates.iter().map(String::as_str).collect();
  let mut run = PartitionRun::default();
  after_writes(owner, volume, BETWEEN_EVENTS);
  partition.begin(fleet, owner_index);
  run.partitioned_at = began.elapsed();
  let taken = wait_taken_over(&survivors, volume);
  verdict.note(format!(
    "owner {partition:?} at {:?}; taken over: {taken:?}",
    run.partitioned_at
  ));
  let last = match taken {
    Some((taken_by, _)) => write_through_successor(taken_by, volume, began, &mut run),
    None => Vec::new(),
  };
  // Through every survivor first: none is partitioned, and each must forward the read to the successor.
  for reader in fleet.others(owner_index) {
    run
      .probes
      .extend(read_through(fleet, reader, (volume, &last), began, false).0);
  }
  if partition.running() {
    let (probes, healed) = read_through(fleet, owner_index, (volume, &last), began, true);
    note_missing(verdict, &fleet.instances[owner_index], volume, &probes);
    run.probes.extend(probes);
    // A heal the reads needed (the owner held them past their bound) ended the partition there.
    run.healed_at = healed.unwrap_or_else(|| {
      partition.end(fleet, owner_index);
      began.elapsed()
    });
  } else {
    partition.end(fleet, owner_index);
    run.healed_at = began.elapsed();
    let (probes, _) = read_through(fleet, owner_index, (volume, &last), began, false);
    note_missing(verdict, &fleet.instances[owner_index], volume, &probes);
    run.probes.extend(probes);
  }
  sleep(AFTER_HEAL);
  run
}

/// When a read through `reader` answered missing, what `reader` held then: its volume list and its status's fleet,
/// lease and live lines ([`diagnosis`]), so a ReadSafety failure names the state that answered it (goal g229: the old
/// owner read the moment it resumed answered `NotFound` for its successor's acknowledged writes).
fn note_missing(verdict: &mut Verdict, reader: &str, volume: &str, probes: &[ReadProbe]) {
  if !probes
    .iter()
    .any(|probe| matches!(probe.answer, ReadAnswer::Missing))
  {
    return;
  }
  let (code, listed, _) = run(reader, &["volume", "list"]);
  verdict.note(format!(
    "{reader} answered a read missing; its volumes (exit {code}):\n{listed}\n{}",
    diagnosis(&[reader.to_owned()], volume)
  ));
}

/// Writes through the successor `taken_by` that took `volume` over: its first write ([`first_write_through`]), then a
/// stream until [`BETWEEN_EVENTS`] more writes are durable; each recorded in `run`. The last [`READ_PROBES`] files it
/// acknowledged, newest first.
fn write_through_successor(
  taken_by: &str,
  volume: &str,
  began: &Instant,
  run: &mut PartitionRun,
) -> Vec<usize> {
  run.successor = Some(taken_by.to_owned());
  let (head, first) = first_write_through(
    taken_by,
    volume,
    STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
    began,
  );
  run.first_head = head;
  run.successor_outcomes = first;
  let stop = AtomicBool::new(false);
  let more = std::thread::scope(|scope| {
    let streamer = scope.spawn(|| {
      Stream {
        instance: taken_by,
        volume: volume_id_of(volume),
        began: *began,
      }
      .run(STREAM_FILES + FIRST_WRITE_FILES..2 * STREAM_FILES, &stop)
    });
    let stopping = StopOnDrop(&stop);
    after_writes(taken_by, volume, BETWEEN_EVENTS);
    drop(stopping);
    streamer.join().unwrap()
  });
  run.successor_outcomes.extend(more);
  run
    .successor_outcomes
    .iter()
    .filter(|outcome| outcome.refused.is_none())
    .map(|outcome| outcome.file)
    .rev()
    .take(READ_PROBES)
    .collect()
}

/// StaleNeverCommits as the old owner's clients see it: no write sent to it while it was partitioned (from
/// `partitioned_at` to `healed_at`) is acknowledged, since it could reach no holder that would count the write for it,
/// nor forward the write anywhere. A write sent through it after the heal may be: once it is re-admitted it forwards a
/// write to the volume's owner (§4.8 "Lookup"), so it is held instead to NoLoss on the successor, with every other
/// acknowledged write.
fn judge_stale_writes(
  verdict: &mut Verdict,
  outcomes: &[Outcome],
  (partitioned_at, healed_at): (Duration, Duration),
) {
  let during: Vec<&Outcome> = outcomes
    .iter()
    .filter(|outcome| outcome.sent > partitioned_at && outcome.sent < healed_at)
    .collect();
  let after: Vec<&Outcome> = outcomes
    .iter()
    .filter(|outcome| outcome.sent >= healed_at)
    .collect();
  verdict.note(format!(
    "the old owner was sent {} writes while partitioned ({partitioned_at:?} to {healed_at:?}) and {} after the heal, \
     {} of those acknowledged",
    during.len(),
    after.len(),
    after
      .iter()
      .filter(|outcome| outcome.refused.is_none())
      .count()
  ));
  verdict.check(
    "StaleNeverCommits (nothing sent to the partitioned owner acknowledged)",
    during
      .iter()
      .filter(|outcome| outcome.refused.is_none())
      .map(|outcome| format!("{outcome:?}"))
      .collect(),
  );
}

/// The verdict of [`an_owner_partitioned`] once its stream has stopped, from what the chaos did (`run`), the old owner's
/// stream's `outcomes` and every node's `samples`.
fn judge_partition(
  verdict: &mut Verdict,
  fleet: &Fleet,
  volume: &str,
  (outcomes, samples): (&[Outcome], &[Sample]),
  run: PartitionRun,
) {
  let PartitionRun {
    partitioned_at,
    healed_at,
    successor,
    first_head,
    successor_outcomes,
    probes,
  } = run;
  verdict.note(format!(
    "old owner: {}; successor: {}",
    latency_line(outcomes),
    latency_line(&successor_outcomes)
  ));
  judge_stale_writes(verdict, outcomes, (partitioned_at, healed_at));
  verdict.note(format!(
    "reads: {}",
    probes
      .iter()
      .map(ReadProbe::to_string)
      .collect::<Vec<_>>()
      .join("; ")
  ));
  verdict.check(
    "ReadSafety (reads through every survivor and the old owner)",
    read_safety(&probes, &successor_outcomes),
  );
  let Some(successor) = successor else {
    verdict.check(
      "takeover",
      vec![format!(
        "no survivor took the volume over:\n{}",
        diagnosis(&fleet.instances, volume)
      )],
    );
    return;
  };
  let (heads, disagreements) = agreement(samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement", disagreements);
  let every: Vec<Outcome> = outcomes
    .iter()
    .chain(&successor_outcomes)
    .cloned()
    .collect();
  verdict.check("NoLoss (successor)", no_loss(&successor, volume, &every));
  let epoch = takeover_epoch(&successor, volume);
  let (durable_before, broken) = continuity(
    samples.iter().map(|sample| &sample.state),
    first_head,
    epoch,
  );
  verdict.note(format!(
    "Continuity: durable before {durable_before:?}, the successor's first head {first_head:?} under epoch {epoch}"
  ));
  verdict.check("Continuity", broken);
  verdict.check(
    "the successor's first write acknowledged, its head shown",
    first_write_shown(&successor, first_head, &successor_outcomes),
  );
  let all: Vec<&str> = fleet.instances.iter().map(String::as_str).collect();
  verdict.check(
    "StaleNeverCommits (fences)",
    fenced_at_or_above(&all, volume, epoch),
  );
  let others: Vec<&str> = all
    .iter()
    .copied()
    .filter(|instance| *instance != successor)
    .collect();
  let (caught_up, last) = converged(&successor, &others, volume);
  verdict.note(format!("converged on the successor: {caught_up}: {last}"));
  verdict.check(
    "Convergence",
    if caught_up { Vec::new() } else { vec![last] },
  );
}

/// An owner cut off from every peer while it runs (goal g229; AC-8.1, AC-8.2): [`an_owner_partitioned`] with
/// [`Partition::Isolated`]. Its lease lapses unrenewed while it still serves its clients, and from its side every peer
/// falls silent. Expect what that flow states: superseded, acknowledging no write while cut off, never answering a read
/// with less than its successor acknowledged, and every invariant held.
#[test]
fn an_owner_cut_off_while_running_is_superseded_and_serves_nothing_stale() {
  an_owner_partitioned(
    "cut-off",
    "an owner cut off while running",
    Partition::Isolated,
  );
}

/// An owner that still speaks to its peers but hears none of them (goal g229; AC-8.1, AC-8.2): [`an_owner_partitioned`]
/// with [`Partition::Deafened`], the one-way partition. Its entries still reach the candidates, which cannot answer it,
/// so its writes never become durable from its side while its peers judge it gone. Expect what that flow states:
/// superseded, acknowledging no write while deafened, never answering a read with less than its successor acknowledged,
/// and every invariant held.
#[test]
fn an_owner_that_hears_nothing_while_it_still_speaks_is_superseded_and_serves_nothing_stale() {
  an_owner_partitioned(
    "deafened",
    "an owner that hears nothing while it still speaks",
    Partition::Deafened,
  );
}

/// An owner that still hears its peers but that none of them hears (goal g229; AC-8.1, AC-8.2): [`an_owner_partitioned`]
/// with [`Partition::Muted`], the other one-way partition. Its entries and its lease's probes never leave it, so it can
/// neither commit nor confirm its lease, while it hears its peers condemn it and its successor take over. Expect what
/// that flow states: superseded, acknowledging no write while muted, never answering a read with less than its successor
/// acknowledged, and every invariant held.
#[test]
fn an_owner_no_peer_hears_while_it_hears_them_all_is_superseded_and_serves_nothing_stale() {
  an_owner_partitioned(
    "muted",
    "an owner no peer hears while it hears them all",
    Partition::Muted,
  );
}

// ── The nemesis: a seeded schedule of single faults across real processes (goal g229) ───────────────────────────────

/// Shape: the faults one nemesis run applies, one at a time: enough that each of the seven kinds of fault is drawn
/// about twice a run, few enough that a run stays within minutes.
const NEMESIS_FAULTS: usize = 14;
/// Shape: the shortest drawn fault: a blip under the membership horizon (900 ms), which no node may act on.
const FAULT_MIN: Duration = Duration::from_millis(200);
/// Shape: the longest drawn fault: past detection and the death-confirmation window (about 2 s together, A-129), so a
/// partitioned owner may be taken over meanwhile, and a fault may end while its takeover runs.
const FAULT_MAX: Duration = Duration::from_secs(4);
/// Shape: the partitions a nemesis draws from, each as likely.
const PARTITIONS: [Partition; 4] = [
  Partition::Stopped,
  Partition::Isolated,
  Partition::Deafened,
  Partition::Muted,
];
/// Shape: the faults on paths a nemesis draws from beside the partitions: every path lossy, one node slowed, or one
/// link severed.
const PATH_FAULTS: u64 = 3;

/// Whom a nemesis fault strikes, resolved against the volume's owner when the fault begins, since a takeover moves it.
#[derive(Clone, Copy, Debug)]
enum Target {
  /// The node that ships the volume then.
  Owner,
  /// One of the owner's candidates then, by its rank among the other nodes.
  Candidate(usize),
}

/// One link between two nodes, resolved against the volume's owner when the fault begins.
#[derive(Clone, Copy, Debug)]
enum Link {
  /// Between the owner and one of its candidates, by its rank among the other nodes.
  OwnerAnd(usize),
  /// Between the owner's two candidates.
  Candidates,
}

/// One fault of a nemesis schedule.
#[derive(Clone, Copy, Debug)]
enum Fault {
  /// Every path of the fleet held and lossy, as one of [`lossy_profiles`] has it.
  Lossy(PathRule),
  /// One node's every path held as [`slow_path`] has it.
  Slow(Target),
  /// One link cut both ways while every other path carries on: the two ends cannot hear each other, and each still
  /// hears the third node, which hears both (a partition that is not transitive).
  Severed(Link),
  /// One node partitioned for the drawn length: an owner may be taken over meanwhile, or not yet.
  Partitioned(Target, Partition),
  /// The owner partitioned until a survivor has taken its volume over, then half the drawn length more: one a run, so
  /// every run judges a takeover.
  Superseded(Partition),
}

impl Fault {
  /// The nodes this fault strikes while `owner` owns the volume: none for a fault on every path, two for a link.
  fn nodes(self, fleet: &Fleet, owner: usize) -> Vec<usize> {
    let others = fleet.others(owner);
    let candidate = |rank: usize| others[rank % others.len()];
    match self {
      Fault::Lossy(_) => Vec::new(),
      Fault::Slow(Target::Owner) | Fault::Partitioned(Target::Owner, _) | Fault::Superseded(_) => {
        vec![owner]
      }
      Fault::Slow(Target::Candidate(rank)) | Fault::Partitioned(Target::Candidate(rank), _) => {
        vec![candidate(rank)]
      }
      Fault::Severed(Link::OwnerAnd(rank)) => vec![owner, candidate(rank)],
      Fault::Severed(Link::Candidates) => others.clone(),
    }
  }

  /// Begins this fault on `nodes` ([`Fault::nodes`]).
  fn begin(self, fleet: &Fleet, nodes: &[usize]) {
    match (self, nodes) {
      (Fault::Lossy(rule), _) => fleet.relay().shape_all(rule),
      (Fault::Slow(_), [node]) => fleet.relay().shape_node(*node, slow_path()),
      (Fault::Severed(_), [one, other]) => {
        fleet.relay().rule(*one, *other, PathRule::CUT);
        fleet.relay().rule(*other, *one, PathRule::CUT);
      }
      (Fault::Partitioned(_, partition) | Fault::Superseded(partition), [node]) => {
        partition.begin(fleet, *node);
      }
      _ => {}
    }
  }

  /// Ends this fault: the node resumed, or every path healed.
  fn end(self, fleet: &Fleet, nodes: &[usize]) {
    match (self, nodes) {
      (Fault::Partitioned(_, partition) | Fault::Superseded(partition), [node]) => {
        partition.end(fleet, *node);
      }
      _ => fleet.relay().heal(),
    }
  }
}

/// A fault and how long it is held: a [`Fault::Superseded`] is held until its takeover and half its length more.
#[derive(Clone, Copy, Debug)]
struct Planned {
  fault: Fault,
  length: Duration,
}

/// A nemesis schedule of `count` faults drawn from `draws`: each of the four partitions and the three path faults as
/// likely, a struck node as likely the owner as either candidate (a severed link the owner's to either candidate as
/// likely as the candidates' own), each length within [`FAULT_MIN`]–[`FAULT_MAX`]; and one drawn position made the
/// owner's supersession, so each run takes the volume over at least once.
fn nemesis_schedule(draws: &mut Draws, count: usize) -> Vec<Planned> {
  let nodes = u64::try_from(FLEET_NODES.len()).unwrap();
  let partitions = u64::try_from(PARTITIONS.len()).unwrap();
  let rank = |pick: u64| usize::try_from(pick - 1).unwrap();
  let target = |draws: &mut Draws| match draws.below(nodes) {
    0 => Target::Owner,
    pick => Target::Candidate(rank(pick)),
  };
  let link = |draws: &mut Draws| match draws.below(nodes) {
    0 => Link::Candidates,
    pick => Link::OwnerAnd(rank(pick)),
  };
  let mut schedule: Vec<Planned> = (0..count)
    .map(|_| {
      let fault = match draws.below(PATH_FAULTS + partitions) {
        0 => {
          let profiles = lossy_profiles();
          let pick = draws.below(u64::try_from(profiles.len()).unwrap());
          Fault::Lossy(profiles[usize::try_from(pick).unwrap()].1)
        }
        1 => Fault::Slow(target(draws)),
        2 => Fault::Severed(link(draws)),
        partition => Fault::Partitioned(
          target(draws),
          PARTITIONS[usize::try_from(partition - PATH_FAULTS).unwrap()],
        ),
      };
      Planned {
        fault,
        length: draws.within(FAULT_MIN, FAULT_MAX),
      }
    })
    .collect();
  let at = usize::try_from(draws.below(u64::try_from(count).unwrap())).unwrap();
  let partition = PARTITIONS[usize::try_from(draws.below(partitions)).unwrap()];
  if let Some(planned) = schedule.get_mut(at) {
    planned.fault = Fault::Superseded(partition);
  }
  schedule
}

/// The node that ships `volume` under the highest fence, its owner, once exactly one does at that fence (two at one
/// fence would be two owners of one epoch), within each fleet wait judged by progress.
fn wait_one_owner(fleet: &Fleet, volume: &str) -> usize {
  let mut window = FrozenWindow::new();
  loop {
    let shipping: Vec<(usize, u64)> = (0..fleet.instances.len())
      .filter_map(|index| {
        live_states(&fleet.instances[index], volume)
          .unwrap_or_default()
          .into_iter()
          .find(|state| !state.holding)
          .map(|state| (index, state.fence))
      })
      .collect();
    let top = shipping.iter().map(|(_, fence)| *fence).max();
    let at_top: Vec<usize> = shipping
      .iter()
      .filter(|(_, fence)| Some(*fence) == top)
      .map(|(index, _)| *index)
      .collect();
    if let [owner] = at_top.as_slice() {
      return *owner;
    }
    assert!(
      window.moving(&format!("{shipping:?}")),
      "one node shipped the volume at the highest fence, or the shippers kept changing, within each fleet wait: \
       (node, fence) {shipping:?}\n{}",
      diagnosis(&fleet.instances, volume)
    );
    pause();
  }
}

/// Waits until every node but `owner` holds a copy of `volume` fenced at the owner's epoch or above, at or past the
/// head the owner had made durable when the wait began: a copy a fault left behind catches up by the entries after it
/// or by an install. Judged by progress over the copies still behind, so one stuck copy is not hidden by another's
/// moving.
fn wait_caught_up(fleet: &Fleet, owner: usize, volume: &str) {
  let (fence, target) = live_states(&fleet.instances[owner], volume)
    .unwrap_or_default()
    .into_iter()
    .find(|state| !state.holding)
    .map_or((0, 0), |state| (state.fence, state.durable.unwrap_or(0)));
  let mut window = FrozenWindow::new();
  loop {
    let behind: Vec<_> = fleet
      .others(owner)
      .into_iter()
      .map(|index| {
        let copy = live_states(&fleet.instances[index], volume)
          .unwrap_or_default()
          .into_iter()
          .find(|state| state.holding)
          .map(|state| (state.fence, state.head));
        (fleet.instances[index].clone(), copy)
      })
      .filter(|(_, copy)| {
        !copy.is_some_and(|(copy_fence, head)| {
          copy_fence >= fence && head.is_some_and(|head| head >= target)
        })
      })
      .collect();
    if behind.is_empty() {
      return;
    }
    assert!(
      window.moving(&format!("{behind:?}")),
      "every copy caught up to {}'s durable head {target} at fence {fence}, or the copies behind kept moving, within \
       each fleet wait: (node, (fence, head)) {behind:?}\n{}",
      fleet.instances[owner],
      diagnosis(&fleet.instances, volume)
    );
    pause();
  }
}

/// Waits until the fleet has recovered whole from a nemesis fault, so the next is the only one (two faults in one
/// cohort are past `f`): every member seated and holding the whole configuration, every node's own member id a voter
/// with no voter change in flight and no retirement kept ([`wait_recovered`]), every node settled at its neighbourhood,
/// one node shipping the volume, and every other copy caught up ([`wait_caught_up`]). The owner's index.
fn wait_recovered_whole(fleet: &Fleet, volume: &str) -> usize {
  wait_settled(&fleet.instances, fleet.instances.len());
  wait_recovered(&fleet.instances);
  wait_neighbourhood_settled(&fleet.instances);
  let owner = wait_one_owner(fleet, volume);
  wait_caught_up(fleet, owner, volume);
  owner
}

/// The survivor among `survivors` that took `volume` over from an owner that shipped it under `fence`, once one has,
/// within the fleet wait, and after how long: it counted more takeovers than it had before the owner was partitioned
/// (`counted`, one count a survivor), or it ships under a higher fence. A successor counts its rebuild at once but ships
/// only from its next publication, which may wait for the partition's end when the stream's node is the old owner.
/// Unlike [`wait_taken_over`], a survivor that took the volume over earlier in the run, or still ships it under an
/// earlier fence, is not taken for the successor.
fn wait_taken_over_again<'a>(
  survivors: &[&'a str],
  volume: &str,
  (fence, counted): (u64, &[u64]),
) -> Option<(&'a str, Duration)> {
  let began = Instant::now();
  while began.elapsed() < FLEET_WAIT {
    if let Some((successor, _)) = survivors.iter().zip(counted).find(|(survivor, before)| {
      counter(survivor, "fleet.live.taken_over") > **before
        || serving_epoch(survivor, volume) > fence
    }) {
      return Some((*successor, began.elapsed()));
    }
    pause();
  }
  None
}

/// What one nemesis fault did.
struct Strike {
  /// What it was, whom it struck and when, for the verdict's notes.
  line: String,
  /// The reads sent while it lasted.
  probes: Vec<ReadProbe>,
  /// For a supersession: whether a survivor took the volume over within the fleet wait.
  taken_over: Option<bool>,
}

/// Applies `planned` to `fleet`, its target resolved against the volume's owner now: begins it, holds it half its
/// length (a supersession: until a survivor has taken the volume over), reads the stream's newest acknowledged writes
/// (`newest`) through every node while it holds the second half, then ends it ([`read_everywhere_during`]).
fn strike(
  fleet: &Fleet,
  volume: &str,
  planned: Planned,
  (began, newest): (&Instant, &NewestAcknowledged),
) -> Strike {
  let owner = wait_one_owner(fleet, volume);
  let nodes = planned.fault.nodes(fleet, owner);
  let half = planned.length / 2;
  // Before the fault, what a supersession's successor must move past: the owner's fence and each survivor's count.
  let candidates = candidates_of(fleet, owner);
  let survivors: Vec<&str> = candidates.iter().map(String::as_str).collect();
  let fence = serving_epoch(&fleet.instances[owner], volume);
  let counted: Vec<u64> = survivors
    .iter()
    .map(|survivor| counter(survivor, "fleet.live.taken_over"))
    .collect();
  planned.fault.begin(fleet, &nodes);
  let from = began.elapsed();
  let taken = if let Fault::Superseded(_) = planned.fault {
    Some(
      wait_taken_over_again(&survivors, volume, (fence, &counted))
        .map(|(successor, took)| format!("{successor} after {took:?}")),
    )
  } else {
    sleep(half);
    None
  };
  let mut to = from;
  let probes = read_everywhere_during(fleet, volume, newest, began, || {
    sleep(half);
    planned.fault.end(fleet, &nodes);
    to = began.elapsed();
  });
  let struck = if nodes.is_empty() {
    "every path".to_owned()
  } else {
    nodes
      .iter()
      .map(|node| fleet.instances[*node].as_str())
      .collect::<Vec<_>>()
      .join(" and ")
  };
  let mut line = format!("{:?} on {struck} from {from:?} to {to:?}", planned.fault);
  if let Some(taken) = &taken {
    line.push_str(&format!(
      "; taken over by {}",
      taken
        .as_deref()
        .unwrap_or("no survivor within the fleet wait")
    ));
  }
  Strike {
    line,
    probes,
    taken_over: taken.map(|taken| taken.is_some()),
  }
}

/// Continuity from the samples alone, for a flow whose takeovers are many and unscheduled (the nemesis;
/// `docs/wip/models/FencedRegister.tla`): every head an owner shipped under fence `e` is at or past every position an
/// owner reported durable under a fence below `e`. A successor's log goes on from the head its takeover adopted
/// (`slates_db::live::Shipper::new`), which is at or past every such position, and its head only grows, so this never
/// fails falsely; it catches an adoption below what was durable whenever a sample shows the successor's head before
/// its writes carry it past. The violations.
fn continuity_in_samples(samples: &[Sample]) -> Vec<String> {
  let mut durable_at: BTreeMap<u64, u64> = BTreeMap::new();
  for sample in samples.iter().filter(|sample| !sample.state.holding) {
    if let Some(durable) = sample.state.durable {
      let most = durable_at.entry(sample.state.fence).or_default();
      *most = (*most).max(durable);
    }
  }
  samples
    .iter()
    .filter(|sample| !sample.state.holding)
    .filter_map(|sample| {
      let fence = sample.state.fence;
      let durable_before = durable_at.range(..fence).map(|(_, durable)| *durable).max()?;
      let head = sample.state.head?;
      (head < durable_before).then(|| {
        format!(
          "node {} shipped head {head} under fence {fence} at {:?}, below position {durable_before} made durable \
           under an earlier fence",
          sample.node, sample.at
        )
      })
    })
    .collect()
}

/// A-200 under a seeded nemesis (goal g229: injected latency and loss, replicas and owners offline, every register
/// invariant; T-8.1 and T-8.7's partitions across real processes, one at a time). Do: stream writes to a live volume
/// through one node of a relayed fleet while [`NEMESIS_FAULTS`] single faults drawn from the flow's seed
/// ([`nemesis_schedule`]; `SLATES_CHAOS_SEED` replays it) strike one at a time: every path lossy and held, a node
/// slowed, one link severed, or a node (the owner as likely as either candidate) stopped, cut off, deafened or muted,
/// each for [`FAULT_MIN`]–[`FAULT_MAX`], and once a run the owner partitioned until it is taken over; read through
/// every node while each lasts; end it; and wait for the fleet to recover whole ([`wait_recovered_whole`]) and for the
/// stream, forwarded once another node owns the volume, to make more writes durable before the next. Expect, across
/// every takeover the schedule causes: one identity at every head (Agreement, the models' TotalOrder); every write
/// acknowledged through any node served by the last owner (NoLoss); every copy fenced at or above the last owner's
/// epoch (StaleNeverCommits); no shipped head below what an earlier epoch made durable (Continuity,
/// [`continuity_in_samples`]); every read answering its write or a refusal (ReadSafety); every supersession taken over;
/// and, healed, every copy at the last owner's head (Convergence) and a write through it acknowledged.
#[test]
fn a_seeded_nemesis_of_single_faults_keeps_every_register_invariant() {
  if !gated("nemesis") {
    return;
  }
  let fleet = Fleet::start_relayed("nemesis", Supervision::Bare, &FLEET_NODES);
  let front = fleet.instances[fleet.owner_index()].clone();
  let volume = fleet.create_volume(&front, "nemesis", true);
  // The relay draws from the flow's seed; the schedule from the same seed turned half a word, so the two differ.
  let seed = chaos_seed("nemesis");
  let schedule = nemesis_schedule(
    &mut Draws::seeded(seed.rotate_left(u64::BITS / 2)),
    NEMESIS_FAULTS,
  );
  let mut verdict = Verdict::new("a seeded nemesis of single faults");
  verdict.note(format!(
    "seed {seed} ({CHAOS_SEED} replays it); the schedule: {schedule:?}"
  ));
  let mut strikes = Vec::new();
  let (outcomes, samples) = stream_through_reading(
    &fleet.instances,
    &front,
    &volume,
    0..STREAM_FILES,
    |began, newest| {
      after_writes(&front, &volume, BETWEEN_EVENTS);
      for planned in &schedule {
        let struck = strike(&fleet, &volume, *planned, (began, newest));
        let owner = wait_recovered_whole(&fleet, &volume);
        // Writes go on through the front node, forwarded to the owner when it is another node now, so each fault lands
        // mid-stream with writes on either side and a stream that stopped cannot pass for one that kept going.
        after_writes(&fleet.instances[owner], &volume, BETWEEN_EVENTS);
        verdict.note(format!(
          "{}; recovered at {:?}, owner {}",
          struck.line,
          began.elapsed(),
          fleet.instances[owner]
        ));
        strikes.push(struck);
      }
    },
  );
  judge_nemesis(
    &mut verdict,
    &fleet,
    &volume,
    (&outcomes, &samples),
    strikes,
  );
  judge_relayed(&mut verdict, fleet.relay(), acknowledged_writes(&outcomes));
  drop(fleet);
  verdict.judge();
}

/// The verdict of [`a_seeded_nemesis_of_single_faults_keeps_every_register_invariant`] once its stream has stopped,
/// from the stream's `outcomes`, every node's `samples` and what each fault did (`strikes`).
fn judge_nemesis(
  verdict: &mut Verdict,
  fleet: &Fleet,
  volume: &str,
  (outcomes, samples): (&[Outcome], &[Sample]),
  strikes: Vec<Strike>,
) {
  verdict.note(latency_line(outcomes));
  let owner = &fleet.instances[wait_one_owner(fleet, volume)];
  let epoch = serving_epoch(owner, volume);
  let takeovers: u64 = fleet
    .instances
    .iter()
    .map(|instance| counter(instance, "fleet.live.taken_over"))
    .sum();
  verdict.note(format!(
    "the last owner {owner} ships under epoch {epoch}; {takeovers} takeovers counted"
  ));
  verdict.check(
    "every supersession taken over",
    strikes
      .iter()
      .filter(|strike| strike.taken_over == Some(false))
      .map(|strike| strike.line.clone())
      .collect(),
  );
  verdict.check(
    "the nemesis took the volume over (a takeover judged)",
    if takeovers == 0 {
      vec!["no node counted a takeover".to_owned()]
    } else {
      Vec::new()
    },
  );
  let (heads, disagreements) = agreement(samples);
  verdict.note(format!("{} samples, {heads} heads seen", samples.len()));
  verdict.check("Agreement (TotalOrder)", disagreements);
  verdict.check("Continuity", continuity_in_samples(samples));
  verdict.check("NoLoss (the last owner)", no_loss(owner, volume, outcomes));
  let all: Vec<&str> = fleet.instances.iter().map(String::as_str).collect();
  verdict.check(
    "StaleNeverCommits (fences)",
    fenced_at_or_above(&all, volume, epoch),
  );
  let probes: Vec<ReadProbe> = strikes
    .into_iter()
    .flat_map(|strike| strike.probes)
    .collect();
  judge_reads(verdict, (None, &fleet.instances), &probes, outcomes);
  let others: Vec<&str> = all
    .iter()
    .copied()
    .filter(|instance| *instance != owner.as_str())
    .collect();
  let (caught_up, last) = converged(owner, &others, volume);
  verdict.note(format!("converged on the last owner: {caught_up}: {last}"));
  verdict.check(
    "Convergence",
    if caught_up { Vec::new() } else { vec![last] },
  );
  let (head, last_writes) = first_write_through(
    owner,
    volume,
    STREAM_FILES..STREAM_FILES + FIRST_WRITE_FILES,
    &Instant::now(),
  );
  verdict.check(
    "a write through the last owner acknowledged, its head shown",
    first_write_shown(owner, head, &last_writes),
  );
  verdict.check(
    "NoLoss (the last write)",
    no_loss(owner, volume, &last_writes),
  );
}
