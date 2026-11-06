//! The suites over a live-shipped volume (A-200; the supervisor's task of 2026-11-05: "a live-shipped volume must
//! conform exactly as a plain one does"). With `--live-ship`, a suite's session is not one anchor but an `f = 1` fleet:
//! three anchors from one manifest over loopback UDP, each supervising a `slates daemon --fleet` (the anchor binds the
//! node's serve ports, §4.8). The volume is created on node `a`, turned live-shipped (`slates live-ship ID`, A-200's
//! `LiveShip` verb), and mounted through node `a` exactly as a plain session mounts its volume, so the suite drives
//! the same transport. Every publication of the volume is then shipped to both other nodes, its candidates, and a
//! barrier answers only once one of them holds it (A-200 "When a write is promised").
//!
//! A run proves it shipped: after the suite, the owner's live log must be durable at its head with nothing owed, and
//! both candidates must hold that head ([`LiveFleet::caught_up`], read from `slates status` as
//! `crates/cli/tests/live_fleet.rs` reads it). A run that did not ship fails, so a fleet that silently stopped shipping
//! cannot pass as conformance. The fleet's identities and manifest are written in the run's scratch, the harness's own
//! output (a development tool's writes, as every record is).

use std::path::Path;
use std::time::{Duration, Instant};

use super::slates::{Anchor, SlatesBinary};
use super::{create_dir, pause, write_file};
use crate::Failure;

/// Shape: the fleet's TLS name, as `crates/cli/tests/live_fleet.rs`.
const FLEET_NAME: &str = "slates-fleet";
/// Shape: the nodes, in manifest order; the volume is created and mounted on the first.
const FLEET_NODES: [&str; 3] = ["a", "b", "c"];
/// Shape: the fleet's fault tolerance: three nodes at `f = 1`, the smallest fleet with candidates (A-200).
const FLEET_F: u32 = 1;
/// Shape: the ports a node serves on (`slates_server::deploy`): its base (the probe) and the next (the records).
const PORTS_PER_NODE: u16 = 2;
/// Shape: the first loopback port tried for the nodes' blocks: above the registered range, below the ephemeral one.
const PORT_FLOOR: u16 = 20_000;
/// Shape: the span the first block's start is spread over by the process id, so concurrent harnesses differ.
const PORT_SPAN: u16 = 30_000;
/// Shape: blocks tried before the search gives up.
const PORT_ATTEMPTS: u16 = 2_000;
/// Shape: how long a fleet step may take on a loaded box: forming, a ready consensus, a caught-up log. The process
/// flows' formation wait (`crates/cli/tests/live_fleet.rs` `FLEET_WAIT`).
const FLEET_WAIT: Duration = Duration::from_secs(40);
/// Format: the refusal a fresh fleet answers until its consensus is ready (`volume create`, `live-ship`).
const NOT_READY: &str = "ConsensusNotInitialized";

/// An `f = 1` fleet serving one live-shipped volume: node `a`'s anchor is the session's own; the other two are kept
/// here, killed and reaped when the fleet is dropped.
pub(crate) struct LiveFleet {
  /// The candidates' anchors.
  others: Vec<Anchor>,
  /// The candidates' instances.
  other_instances: Vec<String>,
}

/// The owner's anchor and instance, and the fleet that holds the rest.
pub(crate) struct Started {
  pub(crate) owner: Anchor,
  pub(crate) owner_instance: String,
  pub(crate) fleet: LiveFleet,
}

/// Mints a node's self-signed identity in `dir` as `<node>.crt.der` and `<node>.key.der`, as the fleet tests do.
fn mint_identity(dir: &Path, node: &str) -> Result<(), Failure> {
  let key = rcgen::KeyPair::generate().map_err(|e| Failure(format!("a key for {node}: {e}")))?;
  let cert = rcgen::CertificateParams::new(vec![FLEET_NAME.to_owned()])
    .and_then(|params| params.self_signed(&key))
    .map_err(|e| Failure(format!("a certificate for {node}: {e}")))?;
  write_file(&dir.join(format!("{node}.crt.der")), cert.der().as_ref())?;
  write_file(&dir.join(format!("{node}.key.der")), &key.serialize_der())
}

/// The first base from `from` whose [`PORTS_PER_NODE`] loopback UDP ports all bind; they are released at once, for the
/// node's anchor to bind (a port taken in between refuses that anchor's start by name).
fn free_block(from: u16) -> Result<u16, Failure> {
  let mut base = from;
  for _ in 0..PORT_ATTEMPTS {
    let bound: Option<Vec<std::net::UdpSocket>> = (0..PORTS_PER_NODE)
      .map(|k| {
        base
          .checked_add(k)
          .and_then(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).ok())
      })
      .collect();
    if bound.is_some() {
      return Ok(base);
    }
    base = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
  }
  Err(Failure(format!(
    "no block of {PORTS_PER_NODE} free loopback UDP ports from {from}"
  )))
}

/// Writes the fleet's identities and manifest in `dir`; the manifest's path.
fn write_fleet(dir: &Path) -> Result<std::path::PathBuf, Failure> {
  create_dir(dir)?;
  let offset = std::process::id().wrapping_mul(7) % u32::from(PORT_SPAN);
  let mut from = u16::try_from(u32::from(PORT_FLOOR).saturating_add(offset)).unwrap_or(PORT_FLOOR);
  let mut nodes = Vec::new();
  for node in FLEET_NODES {
    mint_identity(dir, node)?;
    let base = free_block(from)?;
    from = base.checked_add(PORTS_PER_NODE).unwrap_or(PORT_FLOOR);
    nodes.push(serde_json::json!({
      "node": node,
      "address": format!("127.0.0.1:{base}"),
      "certificate": format!("{node}.crt.der"),
      "key": format!("{node}.key.der"),
    }));
  }
  let manifest = serde_json::json!({ "name": FLEET_NAME, "f": FLEET_F, "nodes": nodes });
  let path = dir.join("fleet.json");
  write_file(&path, manifest.to_string().as_bytes())?;
  Ok(path)
}

/// The value of `key: value` in a verb's text output.
fn value_of(text: &str, key: &str) -> Option<String> {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .map(str::to_owned)
}

/// A node's member id, members held and peers probed, from `status`.
fn fleet_view(binary: &SlatesBinary, instance: &str) -> Option<(u64, usize, u32)> {
  let reply = binary.run(instance, &["status"]).ok()?;
  if reply.code != 0 {
    return None;
  }
  Some((
    value_of(&reply.stdout, "fleet_host")?.parse().ok()?,
    value_of(&reply.stdout, "fleet_members")?
      .split_whitespace()
      .count(),
    value_of(&reply.stdout, "fleet_peers_probed")?
      .parse()
      .ok()?,
  ))
}

/// Waits until every node holds all three members with both peers probed; their member ids.
fn wait_formed(binary: &SlatesBinary, instances: &[String]) -> Result<Vec<u64>, Failure> {
  let started = Instant::now();
  let peers = u32::try_from(FLEET_NODES.len().saturating_sub(1)).unwrap_or(u32::MAX);
  loop {
    let views: Vec<_> = instances.iter().map(|i| fleet_view(binary, i)).collect();
    if views.iter().all(|view| {
      view.is_some_and(|(_, members, probed)| members == FLEET_NODES.len() && probed == peers)
    }) {
      return Ok(
        views
          .into_iter()
          .flatten()
          .map(|(host, _, _)| host)
          .collect(),
      );
    }
    if started.elapsed() > FLEET_WAIT {
      return Err(Failure(format!(
        "the live fleet did not form within {FLEET_WAIT:?}: {views:?}"
      )));
    }
    pause();
  }
}

/// Runs `args` on `instance` until it is not refused for a consensus not yet ready, within [`FLEET_WAIT`]; its reply.
pub(crate) fn run_ready(
  binary: &SlatesBinary,
  instance: &str,
  args: &[&str],
) -> Result<super::slates::Reply, Failure> {
  let started = Instant::now();
  loop {
    let reply = binary.run(instance, args)?;
    if reply.code != 0 && reply.stderr.contains(NOT_READY) && started.elapsed() < FLEET_WAIT {
      pause();
      continue;
    }
    return Ok(reply);
  }
}

impl LiveFleet {
  /// Starts the fleet's three anchors under `tag` in `scratch`, waits for it to form, and bootstraps the root group on
  /// the node with the least member id, as the fleet tests do.
  pub(crate) fn start(
    binary: &SlatesBinary,
    tag: &str,
    scratch: &Path,
  ) -> Result<Started, Failure> {
    let manifest = write_fleet(&scratch.join(format!("fleet-{tag}")))?;
    let manifest = manifest.display().to_string();
    let mut anchors = Vec::new();
    let mut instances = Vec::new();
    for node in FLEET_NODES {
      let instance = format!("conf-{tag}-{node}-{}", std::process::id());
      let extra = [
        "--fleet".to_owned(),
        manifest.clone(),
        "--node".to_owned(),
        node.to_owned(),
      ];
      anchors.push(Anchor::start_with(
        binary, &instance, scratch, None, &extra,
      )?);
      instances.push(instance);
    }
    let hosts = wait_formed(binary, &instances)?;
    let first = hosts
      .iter()
      .enumerate()
      .min_by_key(|(_, host)| **host)
      .map_or(0, |(index, _)| index);
    let bootstrap = instances
      .get(first)
      .ok_or_else(|| Failure("the fleet has no member to bootstrap".to_owned()))?;
    binary
      .run(bootstrap, &["bootstrap", "root"])?
      .expect_ok("bootstrap root on the live fleet")?;
    let mut anchors = anchors.into_iter();
    let mut instances = instances.into_iter();
    let owner = anchors
      .next()
      .ok_or_else(|| Failure("the fleet has no owner".to_owned()))?;
    let owner_instance = instances
      .next()
      .ok_or_else(|| Failure("the fleet has no owner".to_owned()))?;
    Ok(Started {
      owner,
      owner_instance,
      fleet: LiveFleet {
        others: anchors.collect(),
        other_instances: instances.collect(),
      },
    })
  }

  /// Turns `volume` on `owner` live-shipped.
  pub(crate) fn ship(binary: &SlatesBinary, owner: &str, volume: &str) -> Result<(), Failure> {
    let reply = run_ready(binary, owner, &["live-ship", volume])?.expect_ok("live-ship")?;
    if reply.stdout.contains("live-shipped") {
      Ok(())
    } else {
      Err(Failure(format!(
        "live-ship answered without turning shipping on: {}",
        reply.stdout.trim()
      )))
    }
  }

  /// Each candidate's anchor and daemon processes, for a test that stops and resumes them: the anchor first, since
  /// a supervisor kills a daemon that stops beating.
  #[cfg(test)]
  pub(crate) fn candidate_processes(&self) -> Result<Vec<(u32, u32)>, Failure> {
    self
      .others
      .iter()
      .map(|anchor| Ok((anchor.pid(), anchor.daemon_pid()?)))
      .collect()
  }

  /// Whether the run shipped: the owner's live log is durable at its head with nothing owed and both candidates hold
  /// that head, within [`FLEET_WAIT`]; and the note the record carries either way.
  pub(crate) fn caught_up(
    &self,
    binary: &SlatesBinary,
    owner: &str,
    volume: &str,
  ) -> (bool, String) {
    let started = Instant::now();
    loop {
      let shipping = live_line(binary, owner, "shipping", volume);
      let held: Vec<_> = self
        .other_instances
        .iter()
        .map(|other| live_line(binary, other, "held", volume))
        .collect();
      let shipped = shipping.as_ref().is_some_and(|line| {
        line.head != "none"
          && line.head == line.durable
          && line.owed == "false"
          && held
            .iter()
            .all(|kept| kept.as_ref().is_some_and(|kept| kept.head == line.head))
      });
      if shipped || started.elapsed() > FLEET_WAIT {
        let counters: Vec<String> = std::iter::once(owner)
          .chain(self.other_instances.iter().map(String::as_str))
          .map(|instance| format!("{instance} {:?}", live_counters(binary, instance)))
          .collect();
        let note = format!(
          "live-shipped (A-200) on an f = {FLEET_F} fleet, the owner and {} candidate anchors, mounted through node {}: \
           the owner's log {shipping:?}, the candidates holding {held:?}; {}",
          self.others.len(),
          FLEET_NODES.first().copied().unwrap_or_default(),
          if shipped {
            "durable at its head and held by both candidates"
          } else {
            "NOT caught up within the fleet wait, so the run did not prove it shipped"
          }
        );
        let note = format!("{note}; the fleet.live counters: {}", counters.join("; "));
        return (shipped, note);
      }
      pause();
    }
  }
}

/// Every `fleet.live.*` counter `instance` reports, summed over its shards (`shard N refused KIND: COUNT` in `status`),
/// as `crates/cli/tests/live_fleet.rs` sums them.
fn live_counters(binary: &SlatesBinary, instance: &str) -> std::collections::BTreeMap<String, u64> {
  let mut counts = std::collections::BTreeMap::new();
  let Ok(reply) = binary.run(instance, &["status"]) else {
    return counts;
  };
  for (kind, count) in reply
    .stdout
    .lines()
    .filter_map(|line| {
      line
        .split_once(" refused fleet.live.")
        .map(|(_, rest)| rest)
    })
    .filter_map(|rest| rest.split_once(": "))
  {
    *counts.entry(kind.to_owned()).or_insert(0u64) += count.trim().parse::<u64>().unwrap_or(0);
  }
  counts
}

/// A `live` status line's head, durable and owed fields.
#[derive(Debug)]
struct LiveLine {
  head: String,
  durable: String,
  owed: String,
}

/// `instance`'s `live <role> <volume>` status line, as `crates/cli/tests/live_fleet.rs` reads it.
fn live_line(binary: &SlatesBinary, instance: &str, role: &str, volume: &str) -> Option<LiveLine> {
  let reply = binary.run(instance, &["status"]).ok()?;
  if reply.code != 0 {
    return None;
  }
  let prefix = format!(" live {role} {volume} ");
  reply.stdout.lines().find_map(|line| {
    let (_, fields) = line.split_once(&prefix)?;
    let field = |name: &str| {
      fields
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{name}=")))
        .map(str::to_owned)
    };
    Some(LiveLine {
      head: field("head")?,
      durable: field("durable")?,
      owed: field("owed")?,
    })
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Derived: how long the test keeps both candidates stopped while one size change waits: about twice the macOS mount's
  /// give-up (`soft,intr`, §4.6), measured at 57.1 s on 2026-11-05 on this Mac: the client's retries of
  /// `NFS3ERR_JUKEBOX` then end the call `ETIMEDOUT`.
  const FROZEN: Duration = Duration::from_secs(120);
  /// Format: the size the stopped-candidates change asks for, a page.
  const STEP: u64 = 4096;

  /// Sends `signal` (`-STOP`, `-CONT`) to each of `pids`.
  fn signal_all(pids: &[u32], signal: &str) {
    for pid in pids {
      let sent = std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .output()
        .unwrap();
      assert!(
        sent.status.success(),
        "kill {signal} {pid}: {}",
        String::from_utf8_lossy(&sent.stderr)
      );
    }
  }

  /// A-200 (raft owns the fix; GAPS, "A live-shipped volume fails a waiting writer through a soft mount"). Do: on an
  /// `f = 1` fleet, turn a volume live-shipped, mount it as the harness mounts a plain volume, create a file, stop both
  /// candidates (each anchor, then its daemon), ask the file for a new size, and resume the candidates after
  /// [`FROZEN`] or once the change answers. Expect: the size change
  /// waits for the candidates and then succeeds, as A-200 says ("Holders that never answer make the writer wait") and as
  /// the same call on a plain volume succeeds at once. Red on 2026-11-05 on this Mac (below). Gated on
  /// `SLATES_TEST_LIVE_CONFORMANCE=1` and `SLATES_TEST_BINARY` until the fix, so a red known to its owner does not turn main
  /// red; it skips loudly otherwise.
  #[test]
  #[allow(clippy::disallowed_methods)] // the calls are a program's on the slates mount, the point of the test
  fn a_live_shipped_volume_makes_a_writer_wait_for_stopped_candidates_rather_than_fail() {
    let (Some(_), Some(path)) = (
      std::env::var_os("SLATES_TEST_LIVE_CONFORMANCE"),
      std::env::var_os("SLATES_TEST_BINARY"),
    ) else {
      eprintln!(
        "SKIP: set SLATES_TEST_LIVE_CONFORMANCE=1 and SLATES_TEST_BINARY to a built slates CLI to mount a live-shipped volume"
      );
      return;
    };
    let root = crate::workspace_root().unwrap();
    let options =
      super::super::parse(&root, &["run".to_owned(), "--live-ship".to_owned()]).unwrap();
    let os = super::super::host_os().unwrap();
    let transport = super::super::native_transport(os);
    let run = super::super::Run {
      root: &root,
      options: &options,
      os,
      transport,
      root_available: super::super::is_root() || super::super::sudo_without_prompt(),
      scratch: super::super::Scratch::open(&options, transport).unwrap(),
    };
    let session = super::super::slates::Session::open_with(
      &run,
      SlatesBinary::at(std::path::PathBuf::from(path)),
      ("livewait", super::super::VOLUME_SIZE, false),
      None,
    )
    .unwrap();
    let file = session.workdir("live").unwrap().join("sized.bin");
    let handle = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(true)
      .open(&file)
      .unwrap();
    let candidates = session.live_fleet().unwrap().candidate_processes().unwrap();
    eprintln!("the candidates' (anchor, daemon) processes: {candidates:?}");
    let (anchors, daemons): (Vec<u32>, Vec<u32>) = candidates.into_iter().unzip();
    signal_all(&anchors, "-STOP");
    signal_all(&daemons, "-STOP");
    let started = Instant::now();
    let (changed, answered) = std::thread::scope(|scope| {
      let change = scope.spawn(|| {
        let changed = handle.set_len(STEP);
        (changed, started.elapsed())
      });
      while !change.is_finished() && started.elapsed() < FROZEN {
        pause();
      }
      signal_all(&daemons, "-CONT");
      signal_all(&anchors, "-CONT");
      change.join().unwrap()
    });
    let late = live_counters(&session.binary, &session.instance)
      .get("durable_late")
      .copied()
      .unwrap_or(0);
    eprintln!(
      "the size change answered after {answered:?} ({changed:?}); the owner's late stable replies: {late}"
    );
    drop(session);
    assert!(
      changed.is_ok(),
      "a writer waiting for stopped candidates failed after {answered:?}: {changed:?} ({late} late stable replies)"
    );
  }
}
