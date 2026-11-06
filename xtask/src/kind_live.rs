//! `cargo xtask kind live` — live-shipped writes on real Linux pods under the lane's WAN profiles (§4.10, A-200; Ada's
//! goal of 2026-11-04: replicas of the filesystem server, replicated per write, chaos-tested under injected latency,
//! jitter and loss). The process flows on one machine (`crates/cli/tests/live_chaos.rs`) stop and kill processes; netem
//! needs `NET_ADMIN`, which this project never takes on a developer's machine, so the shaped paths are measured here,
//! where the lane's init container already applies them (`values-netem.yaml`, lane-only).
//!
//! The run, each step a command on the record, on a cluster of its own (`slates-live`, from `deploy/kind/cluster.yaml`):
//!
//! 1. The slates image, the netem init image and the lane's probe image (`deploy/kind/pod-probe.Dockerfile`, which
//!    carries `crates/cli/examples/pod_live_probe.rs`) are built and loaded.
//! 2. For each profile, unshaped first and then each of the lane's WAN profiles (`NETEM_PROFILES` in `kind.rs`: 80 ms ±
//!    20 ms one way on pods 0 and 1; the same with 1 % loss; 350 ms), a fresh three-replica install, bootstrapped and
//!    formed. Pod 2 is never shaped, so a volume it owns ships to two candidates whose every reply is delayed.
//! 3. A live-shipped volume on pod 2, and the probe, as an ephemeral container of pod 2, streams `writes_for` writes
//!    of `WRITE_BYTES` to it, each answered only once a candidate holds it, and prints the client-seen p50, p99, p999
//!    and max.
//! 4. The register invariants, read from every pod's `slates status`: **Convergence** (both candidates hold the
//!    owner's head, same epoch, position and chain identity, and the owner's log is durable at its head with nothing
//!    owed) and **Agreement** (one identity at every epoch and position seen); then **NoLoss**: the probe reads every
//!    file back on pod 2, and, once pod 2 is deleted with no grace (the SIGKILL takeover, over the shaped paths), on
//!    the survivor that took the volume over; and the survivors' copies fenced at or above its epoch
//!    (**StaleNeverCommits**' log side).
//!
//! The ceiling profile's formation is reported, not required, as the netem step does. Every artifact goes to the lane's
//! scratch directory outside the tree; the cluster is deleted at the end unless `--keep`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use super::export::{MAP_PROBE_TAG, probe_image};
use super::{
  CLUSTER_WAIT, Failure, LANE_REPLICAS, Lane, NETEM_PROFILES, NETEM_TAG, Options, POLL_CLUSTER,
  Scratch, Shaping, certs, down, image, netem_image, pause, stream,
};

/// Shape: the leg's own kind cluster; created here, deleted at the end, never anyone else's.
const LIVE_CLUSTER: &str = "slates-live";
/// Format: the probe binary in the probe image.
const PROBE_BINARY: &str = "/slates-pod-live-probe";
/// Format: the tag of the probe's line in its write mode.
const WRITE_TAG: &str = "live-write:";
/// Format: the tag of the probe's line in its verify mode.
const VERIFY_TAG: &str = "live-verify:";
/// Shape: each write's bytes, a page.
const WRITE_BYTES: usize = 4096;
/// Shape: the pod that owns the volume: the one the WAN profiles leave unshaped, so both its candidates are shaped.
const OWNER_POD: u64 = 2;
/// Shape: the volume's bound in slates' spelling: room for the most writes any profile makes, many times over.
const VOLUME_BOUND: &str = "64MiB";
/// Shape: how long the group gets to accept a create after the bootstrap (the direct leg's bound).
const GROUP_WAIT: Duration = Duration::from_secs(60);
/// Shape: how long a probe run gets to be started and to finish: the most writes at the ceiling's 350 ms, with room.
const PROBE_WAIT: Duration = Duration::from_secs(600);
/// Shape: how long the candidates get to converge on the owner's head after the stream, and a survivor to take the
/// volume over after the owner's pod is deleted (the lane's takeover was 1.8–10.2 s unshaped).
const SETTLE_WAIT: Duration = Duration::from_secs(120);

/// Shape: the writes streamed under a profile: as many as fit a minute of its round trips, at least 50. Unshaped 1000.
fn writes_for(profile: &str) -> usize {
  match profile {
    "unshaped" => 1000,
    "ceiling" => 50,
    _ => 300,
  }
}

/// `kind live`.
pub(super) fn run(root: &Path, options: &Options) -> Result<(), Failure> {
  let scratch = Scratch::create(options.keep)?;
  let identities = scratch.path.join("identities.yaml");
  certs(LANE_REPLICAS, &identities)?;
  image(root, &options.tag)?;
  netem_image(root)?;
  probe_image(root)?;
  let config = root.join("deploy/kind/cluster.yaml");
  stream(
    "kind",
    &[
      "create",
      "cluster",
      "--name",
      LIVE_CLUSTER,
      "--config",
      &config.to_string_lossy(),
      "--wait",
      CLUSTER_WAIT,
    ],
  )?;
  let lane = Lane {
    root: root.to_path_buf(),
    cluster: LIVE_CLUSTER.to_owned(),
    tag: options.tag.clone(),
    _scratch: scratch,
    keep: options.keep,
    identities,
    replicas: LANE_REPLICAS,
    netem: None,
    trials: 0,
  };
  let outcome = stream(
    "kind",
    &[
      "load",
      "docker-image",
      &lane.tag,
      NETEM_TAG,
      MAP_PROBE_TAG,
      "--name",
      LIVE_CLUSTER,
    ],
  )
  .and_then(|()| profiles(&lane));
  if let Err(e) = &outcome {
    eprintln!("kind live: {e}");
  }
  if options.keep {
    eprintln!("kind live: cluster {LIVE_CLUSTER} kept");
  } else if let Err(e) = down(LIVE_CLUSTER) {
    eprintln!("kind live: {e}");
  }
  outcome
}

/// Every profile in turn; the findings printed together at the end, and the first failure returned.
fn profiles(lane: &Lane) -> Result<(), Failure> {
  let mut shaped: Vec<(&str, Option<Shaping>)> = vec![("unshaped", None)];
  for (name, delay, jitter, loss) in NETEM_PROFILES {
    shaped.push((name, Some(Shaping::uniform(delay, jitter, loss))));
  }
  let mut findings = Vec::new();
  let mut failed = None;
  for (name, shaping) in &shaped {
    match profile(lane, name, shaping.as_ref()) {
      Ok(finding) => findings.push(finding),
      Err(e) => {
        eprintln!("kind live: {name}: {e}");
        findings.push(format!("{name}: FAILED: {e}"));
        failed.get_or_insert(e);
      }
    }
  }
  eprintln!("kind live: findings:");
  for finding in &findings {
    eprintln!("kind live:   {finding}");
  }
  failed.map_or(Ok(()), Err)
}

/// One profile: install, form, stream, judge, take over, judge again. The finding's line.
fn profile(lane: &Lane, name: &str, shaping: Option<&Shaping>) -> Result<String, Failure> {
  let elapsed = lane.fresh_install(LANE_REPLICAS, shaping)?;
  eprintln!(
    "kind live: {name}: installed and rolled out in {:.1} s",
    elapsed.as_secs_f64()
  );
  if let Err(e) = lane.wait_formed(LANE_REPLICAS, &format!("the fleet formed under {name}")) {
    if name == "ceiling" {
      return Ok(format!(
        "{name}: the fleet did NOT form; nothing measured — {e}"
      ));
    }
    return Err(e);
  }
  let owner = lane.pod(OWNER_POD);
  let survivors: Vec<String> = (0..LANE_REPLICAS)
    .filter(|index| *index != OWNER_POD)
    .map(|index| lane.pod(index))
    .collect();
  let volume = create_live_volume(lane, &owner)?;
  let count = writes_for(name);
  let wrote = run_probe(
    lane,
    &owner,
    "live-writer",
    &[
      "write",
      &volume,
      &count.to_string(),
      &WRITE_BYTES.to_string(),
    ],
    WRITE_TAG,
  )?;
  eprintln!("kind live: {name}: {WRITE_TAG} {wrote}");
  let acknowledged = field(&wrote, "acknowledged").unwrap_or(0);
  let refused = field(&wrote, "refused").unwrap_or(u64::MAX);
  let mut heads = BTreeMap::new();
  let converged = wait_converged(lane, &owner, &survivors, &volume, &mut heads)?;
  agreement(&heads).map_err(|e| Failure(format!("kind live: {name}: Agreement: {e}")))?;
  let on_owner = verify(
    lane,
    &owner,
    "live-verify-owner",
    &volume,
    count,
    acknowledged,
    refused,
  )
  .map_err(|e| Failure(format!("kind live: {name}: NoLoss on the owner: {e}")))?;
  // The owner's pod deleted with no grace: the SIGKILL takeover, over the shaped paths.
  let deleted = Instant::now();
  let removed = lane.kubectl(&[
    "delete",
    "pod",
    &owner,
    "--grace-period=0",
    "--force",
    "--wait=false",
  ])?;
  if removed.code != 0 {
    return Err(Failure(format!(
      "kind live: delete {owner}: {}",
      removed.stderr.trim()
    )));
  }
  let successor = wait_taken_over(lane, &survivors)?;
  let took = deleted.elapsed();
  let on_successor = verify(
    lane,
    &successor,
    "live-verify-successor",
    &volume,
    count,
    acknowledged,
    refused,
  )
  .map_err(|e| {
    Failure(format!(
      "kind live: {name}: NoLoss on the successor {successor}: {e}"
    ))
  })?;
  let epoch = states(lane, &successor, &volume)?
    .iter()
    .map(|state| state.fence)
    .max()
    .unwrap_or(0);
  for survivor in &survivors {
    for state in states(lane, survivor, &volume)? {
      if state.holding && state.fence < epoch {
        return Err(Failure(format!(
          "kind live: {name}: StaleNeverCommits: {survivor}'s copy is fenced at {} below the takeover's epoch {epoch}",
          state.fence
        )));
      }
    }
  }
  Ok(format!(
    "{name}: {wrote}; converged {converged}; {} heads seen, one identity each; owner read back {on_owner}; taken over \
     by {successor} after {:.1} s, epoch {epoch}, read back {on_successor}",
    heads.len(),
    took.as_secs_f64()
  ))
}

/// Creates a volume on `pod` once the group accepts it and makes it live-shipped: its id.
fn create_live_volume(lane: &Lane, pod: &str) -> Result<String, Failure> {
  let started = Instant::now();
  let id = loop {
    let created = lane.verb(
      pod,
      &["volume", "create", "live", "--bounded", VOLUME_BOUND],
    )?;
    if created.code == 0 {
      let reply: serde_json::Value = serde_json::from_str(&created.stdout)?;
      break reply
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
          Failure(format!(
            "kind live: create printed no id: {}",
            created.stdout
          ))
        })?;
    }
    if started.elapsed() > GROUP_WAIT {
      return Err(Failure(format!(
        "kind live: the volume was not created within {GROUP_WAIT:?}: {}",
        created.stderr.trim()
      )));
    }
    pause(POLL_CLUSTER);
  };
  let shipped = lane.kubectl(&["exec", pod, "--", "/slates", "live-ship", &id])?;
  if shipped.code != 0 {
    return Err(Failure(format!(
      "kind live: live-ship {id}: {}",
      shipped.stderr.trim()
    )));
  }
  Ok(id)
}

/// Runs the probe with `args` as the ephemeral container `container` of `pod`; its `tag` line, the tag taken off.
fn run_probe(
  lane: &Lane,
  pod: &str,
  container: &str,
  args: &[&str],
  tag: &str,
) -> Result<String, Failure> {
  let image = format!("--image={MAP_PROBE_TAG}");
  let container_arg = format!("--container={container}");
  let mut debug = vec![
    "debug",
    pod,
    &image,
    "--image-pull-policy=IfNotPresent",
    &container_arg,
    "--profile=restricted",
    "--",
    PROBE_BINARY,
  ];
  debug.extend_from_slice(args);
  let added = lane.kubectl(&debug)?;
  if added.code != 0 {
    return Err(Failure(format!(
      "kind live: kubectl debug refused: {}",
      added.stderr.trim()
    )));
  }
  let exit = format!(
    "jsonpath={{.status.ephemeralContainerStatuses[?(@.name==\"{container}\")].state.terminated.exitCode}}"
  );
  let started = Instant::now();
  let code = loop {
    let state = lane.kubectl(&["get", "pod", pod, "-o", &exit])?;
    let code = state.stdout.trim();
    if !code.is_empty() {
      break code.to_owned();
    }
    if started.elapsed() > PROBE_WAIT {
      diagnose(lane, pod, container);
      return Err(Failure(format!(
        "kind live: the probe {container} did not finish within {PROBE_WAIT:?}"
      )));
    }
    pause(POLL_CLUSTER);
  };
  let logs = lane.kubectl(&["logs", pod, "-c", container])?;
  logs
    .stdout
    .lines()
    .find_map(|line| line.split_once(tag).map(|(_, rest)| rest.trim().to_owned()))
    .ok_or_else(|| {
      Failure(format!(
        "kind live: the probe {container} printed no {tag} line (exit {code}): {}",
        logs.stdout.trim()
      ))
    })
}

/// `name=N` in a probe's line.
fn field(line: &str, name: &str) -> Option<u64> {
  line
    .split_whitespace()
    .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    .and_then(|value| value.parse().ok())
}

/// Reads every file back on `pod`: at least `acknowledged` whole, none torn, at most `refused` missing. The line.
fn verify(
  lane: &Lane,
  pod: &str,
  container: &str,
  volume: &str,
  count: usize,
  acknowledged: u64,
  refused: u64,
) -> Result<String, Failure> {
  let line = run_probe(
    lane,
    pod,
    container,
    &[
      "verify",
      volume,
      &count.to_string(),
      &WRITE_BYTES.to_string(),
    ],
    VERIFY_TAG,
  )?;
  let whole = field(&line, "whole").unwrap_or(0);
  let missing = field(&line, "missing").unwrap_or(u64::MAX);
  let torn = field(&line, "torn").unwrap_or(u64::MAX);
  if whole < acknowledged || torn != 0 || missing > refused {
    return Err(Failure(format!(
      "{line} (acknowledged {acknowledged}, refused {refused})"
    )));
  }
  Ok(line)
}

/// One `live` status line of one pod.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveState {
  holding: bool,
  head: Option<u64>,
  durable: Option<u64>,
  owed: bool,
  epoch: Option<u64>,
  identity: Option<String>,
  fence: u64,
}

/// Every `live` line `pod` reports for `volume` (`slates status`, text); none while it does not answer.
fn states(lane: &Lane, pod: &str, volume: &str) -> Result<Vec<LiveState>, Failure> {
  let status = lane.kubectl(&["exec", pod, "--", "/slates", "status"])?;
  if status.code != 0 {
    return Ok(Vec::new());
  }
  let text = |fields: &str, key: &str| {
    fields
      .split_whitespace()
      .find_map(|pair| pair.strip_prefix(key).map(str::to_owned))
  };
  let number =
    |fields: &str, key: &str| text(fields, key).and_then(|value| value.parse::<u64>().ok());
  Ok(
    status
      .stdout
      .lines()
      .filter_map(|line| {
        let (_, rest) = line.split_once(" live ")?;
        let (role, rest) = rest.split_once(' ')?;
        let fields = rest.strip_prefix(volume)?;
        Some(LiveState {
          holding: role == "held",
          head: number(fields, "head="),
          durable: number(fields, "durable="),
          owed: text(fields, "owed=").as_deref() == Some("true"),
          epoch: number(fields, "epoch="),
          identity: text(fields, "identity=").filter(|identity| identity != "none"),
          fence: number(fields, "fence=").unwrap_or(0),
        })
      })
      .collect(),
  )
}

/// Waits until the owner's log is durable at its head with nothing owed and every candidate holds that head (epoch,
/// position and identity), recording every head seen into `heads` for Agreement. The owner's head as a line.
fn wait_converged(
  lane: &Lane,
  owner: &str,
  candidates: &[String],
  volume: &str,
  heads: &mut BTreeMap<(u64, u64), BTreeSet<String>>,
) -> Result<String, Failure> {
  let started = Instant::now();
  loop {
    let shipping = states(lane, owner, volume)?
      .into_iter()
      .find(|state| !state.holding);
    let mut held = Vec::new();
    for candidate in candidates {
      held.push(
        states(lane, candidate, volume)?
          .into_iter()
          .find(|state| state.holding),
      );
    }
    for state in shipping.iter().chain(held.iter().flatten()) {
      if let (Some(epoch), Some(head), Some(identity)) = (state.epoch, state.head, &state.identity)
      {
        heads
          .entry((epoch, head))
          .or_default()
          .insert(identity.clone());
      }
    }
    if let Some(owner_state) = &shipping
      && owner_state.head.is_some()
      && owner_state.head == owner_state.durable
      && !owner_state.owed
      && held.iter().all(|kept| {
        kept.as_ref().is_some_and(|kept| {
          kept.head == owner_state.head
            && kept.epoch == owner_state.epoch
            && kept.identity == owner_state.identity
        })
      })
    {
      return Ok(format!(
        "at head {:?} epoch {:?}",
        owner_state.head, owner_state.epoch
      ));
    }
    if started.elapsed() > SETTLE_WAIT {
      return Err(Failure(format!(
        "kind live: Convergence: not within {SETTLE_WAIT:?}: owner {shipping:?}, candidates {held:?}"
      )));
    }
    pause(POLL_CLUSTER);
  }
}

/// Agreement over the heads seen: one identity at each epoch and position.
fn agreement(heads: &BTreeMap<(u64, u64), BTreeSet<String>>) -> Result<(), String> {
  let split: Vec<String> = heads
    .iter()
    .filter(|(_, identities)| identities.len() > 1)
    .map(|((epoch, head), identities)| format!("epoch {epoch} position {head}: {identities:?}"))
    .collect();
  if split.is_empty() {
    Ok(())
  } else {
    Err(split.join("; "))
  }
}

/// The survivor that rebuilt the volume from its live log (its `fleet.live.taken_over` counter), within the wait.
fn wait_taken_over(lane: &Lane, survivors: &[String]) -> Result<String, Failure> {
  let started = Instant::now();
  loop {
    for survivor in survivors {
      let status = lane.kubectl(&["exec", survivor, "--", "/slates", "status"])?;
      let taken: u64 = status
        .stdout
        .lines()
        .filter_map(|line| line.split_once(" refused fleet.live.taken_over: "))
        .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
        .sum();
      if taken > 0 {
        return Ok(survivor.clone());
      }
    }
    if started.elapsed() > SETTLE_WAIT {
      for survivor in survivors {
        diagnose_pod(lane, survivor);
      }
      return Err(Failure(format!(
        "kind live: no survivor took the volume over within {SETTLE_WAIT:?}"
      )));
    }
    pause(POLL_CLUSTER);
  }
}

/// What a stalled probe leaves to read: its output so far, its container's state, and every pod's live lines and live
/// counters. Evidence only: errors here are printed, never raised, so the leg's own failure stays the one reported.
fn diagnose(lane: &Lane, pod: &str, container: &str) {
  match lane.kubectl(&["logs", pod, "-c", container]) {
    Ok(logs) => eprintln!(
      "kind live: the probe {container}'s output so far:\n{}",
      logs.stdout
    ),
    Err(e) => eprintln!("kind live: the probe's output: {e}"),
  }
  let state =
    format!("jsonpath={{.status.ephemeralContainerStatuses[?(@.name==\"{container}\")]}}");
  match lane.kubectl(&["get", "pod", pod, "-o", &state]) {
    Ok(status) => eprintln!(
      "kind live: the probe {container}'s container: {}",
      status.stdout
    ),
    Err(e) => eprintln!("kind live: the probe's container: {e}"),
  }
  for index in 0..LANE_REPLICAS {
    diagnose_pod(lane, &lane.pod(index));
  }
}

/// One pod's live lines, sessions, and live, takeover, tombstone and owner-location counters, as `status` prints them.
fn diagnose_pod(lane: &Lane, each: &str) {
  match lane.kubectl(&["exec", each, "--", "/slates", "status"]) {
    Ok(status) => {
      let lines: Vec<&str> = status
        .stdout
        .lines()
        .filter(|line| {
          line.contains(" live ")
            || line.contains("fleet.live.")
            || line.contains("fleet.takeover")
            || line.contains("fleet.materialize")
            || line.contains("fleet.owner_location")
            || line.starts_with("fleet_session ")
            || line.starts_with("fleet_host")
            || line.starts_with("fleet_members")
        })
        .collect();
      eprintln!("kind live: {each}:\n  {}", lines.join("\n  "));
    }
    Err(e) => eprintln!("kind live: {each}: status: {e}"),
  }
}
