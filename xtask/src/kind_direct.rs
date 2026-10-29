//! `cargo xtask kind direct` — the DAX matrix's Kubernetes socket-route cells (§4.6, A-135, A-171;
//! `docs/wip/virtiofs.md` §8): a process in a pod maps a file of a slates volume by the direct map, with no copy, on a
//! cluster of its own. It needs nothing of the node's kernel beyond what Linux always has, so it runs wherever kind does
//! (Docker Desktop on an arm64 Mac included), unlike the export leg.
//!
//! The run, each step a command on the record:
//!
//! 1. The slates image and the lane's probe image (`deploy/kind/pod-probe.Dockerfile`, which carries the direct-map
//!    probe, `crates/cli/examples/pod_direct_probe.rs`) are built, and a cluster of one control plane and one worker is
//!    created from kind's own node image.
//! 2. The chart is installed with one replica and the group bootstrapped, and a volume is created on pod 0.
//! 3. The probe runs twice, each time as an ephemeral container of pod 0 (`kubectl debug --profile=restricted`): first
//!    to write a file of 4 MiB (`DIRECT_FILE_BYTES`) into the volume, attaching, writing and detaching as one client, then
//!    to map it. A write split across `slates attach`, `slates write` and `slates detach` would race the reaper, which
//!    removes a dead client's attachments at the liveness cadence (its first run here answered the detach `NotFound`).
//!    Each probe shares the pod's
//!    network namespace, where the daemon's rendezvous lives: an abstract socket, which Linux scopes to its network
//!    namespace (unix(7)). The pod's security context makes it the daemon's user, the only one the daemon hands a
//!    direct map to (A-171). Everything else is a workload's own: its mount namespace and root file system, and the
//!    restricted profile, which drops every capability, forbids privilege escalation and applies the runtime's default
//!    seccomp profile.
//! 4. The probe must print `zero_copy=true whole=true` at the file's length, and the daemon's counters must show one
//!    hand-off (`dax.local_map`), no fallback to a copy and no unconfined descriptor.
//! 5. The probe runs a third time, mapping the file read and write (A-186): it stores across the first page's end,
//!    syncs, releases and reads the file back. It must print that the sync wrote the stored bytes that differ, the
//!    release nothing more, and the file holds the store; the daemon's counters must show one more hand-off and two
//!    write-backs finished (`dax.local_map_synced`), none refused.
//!
//! A pod with a network namespace of its own cannot reach the daemon this way: that needs a rendezvous it can reach
//! (`docs/wip/GAPS.md`, the DAX matrix's entry).

use std::path::Path;
use std::time::{Duration, Instant};

use super::export::{MAP_PROBE_TAG, probe_image};
use super::{
  CLUSTER_WAIT, Failure, Lane, Options, POLL_CLUSTER, Scratch, certs, down, image, pause, stream,
  write_scratch,
};

/// Shape: the leg's own kind cluster; created here, deleted at the end, never anyone else's.
const DIRECT_CLUSTER: &str = "slates-direct";
/// Format: kind's node image the cluster runs, the one the export leg builds its node image from.
const NODE_IMAGE: &str = "kindest/node:v1.36.4";
/// Format: the writer's ephemeral container's name in pod 0.
const WRITER_CONTAINER: &str = "direct-writer";
/// Format: the mapper's ephemeral container's name in pod 0.
const PROBE_CONTAINER: &str = "direct-probe";
/// Format: the writable mapper's ephemeral container's name in pod 0.
const MAP_WRITER_CONTAINER: &str = "direct-map-writer";
/// Format: the probe binary in the probe image.
const PROBE_BINARY: &str = "/slates-pod-direct-probe";
/// Format: the tag of the probe's one line in its write mode (`crates/cli/examples/pod_direct_probe.rs`).
const WRITE_TAG: &str = "direct-write:";
/// Format: the tag of the probe's one line in its map mode.
const PROBE_TAG: &str = "direct-map:";
/// Format: the tag of the probe's one line in its map-write mode.
const MAP_WRITE_TAG: &str = "direct-map-write:";
/// Format: the file the probe maps, at the volume's root.
const DIRECT_FILE: &str = "direct.bin";
/// Shape: the file's length: 4 MiB, many content chunks, as the Docker container's direct-map test maps
/// (`crates/cli/tests/cli.rs`, `DIRECT_PROBE_BYTES`).
const DIRECT_FILE_BYTES: usize = 4 << 20;
/// Format: the one byte value the file holds.
const DIRECT_FILL: u8 = b'D';
/// Shape: the bounded volume the leg creates, in slates' spelling: room for the file four times over.
const VOLUME_BOUND: &str = "16MiB";
/// Shape: how long the group gets to form after the bootstrap before a create is accepted (the export leg's bound).
const GROUP_WAIT: Duration = Duration::from_secs(60);
/// Shape: how long the ephemeral container gets to be pulled from the node, started, and to finish (the export leg's
/// pod bound).
const PROBE_WAIT: Duration = Duration::from_secs(180);
/// Format: the daemon's counter of direct maps handed (`crates/server/src/direct_map.rs`).
const HANDED: &str = "dax.local_map";
/// Format: the daemon's counter of direct maps that fell back to a copy.
const FELL_BACK: &str = "dax.local_map_fallback";
/// Format: the daemon's counter of descriptors the hand-off judged unconfined.
const UNCONFINED: &str = "dax.local_map_unconfined";
/// Format: the daemon's counter of writable maps' write-backs that finished (A-186).
const SYNCED: &str = "dax.local_map_synced";
/// Format: the daemon's counter of writable maps' write-backs refused.
const SYNC_REFUSED: &str = "dax.local_map_sync_refused";

/// `kind direct`.
pub(super) fn run(root: &Path, options: &Options) -> Result<(), Failure> {
  let scratch = Scratch::create(options.keep)?;
  let identities = scratch.path.join("identities.yaml");
  certs(1, &identities)?;
  image(root, &options.tag)?;
  probe_image(root)?;
  let config = scratch.path.join("cluster-direct.yaml");
  write_scratch(
    &config,
    b"kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnodes:\n  - role: control-plane\n  - role: worker\n",
  )?;
  stream(
    "kind",
    &[
      "create",
      "cluster",
      "--name",
      DIRECT_CLUSTER,
      "--config",
      &config.to_string_lossy(),
      "--image",
      NODE_IMAGE,
      "--wait",
      CLUSTER_WAIT,
    ],
  )?;
  let lane = Lane {
    root: root.to_path_buf(),
    cluster: DIRECT_CLUSTER.to_owned(),
    tag: options.tag.clone(),
    _scratch: scratch,
    keep: options.keep,
    identities,
    replicas: 1,
    netem: None,
    trials: 0,
  };
  let outcome = prove(&lane);
  if let Err(e) = &outcome {
    eprintln!("kind direct: {e}");
    diagnostics(&lane);
  }
  if options.keep {
    eprintln!("kind direct: cluster {DIRECT_CLUSTER} kept");
  } else if let Err(e) = down(DIRECT_CLUSTER) {
    eprintln!("kind direct: {e}");
  }
  outcome
}

/// What a failed run leaves to read: pod 0's description and the daemon's log. Evidence only: errors here are printed,
/// never raised, so the leg's own failure stays the one reported.
fn diagnostics(lane: &Lane) {
  let pod = lane.pod(0);
  match lane.kubectl(&["describe", "pod", &pod]) {
    Ok(described) => eprintln!("kind direct: pod 0:\n{}", described.stdout),
    Err(e) => eprintln!("kind direct: describe pod 0: {e}"),
  }
  match lane.kubectl(&["logs", &pod, "-c", "slates"]) {
    Ok(logs) => eprintln!(
      "kind direct: the daemon's log:\n{}{}",
      logs.stdout, logs.stderr
    ),
    Err(e) => eprintln!("kind direct: the daemon's log: {e}"),
  }
}

/// The leg on its cluster: images loaded, the chart installed, the file written, and the probe run in pod 0.
fn prove(lane: &Lane) -> Result<(), Failure> {
  stream(
    "kind",
    &[
      "load",
      "docker-image",
      &lane.tag,
      MAP_PROBE_TAG,
      "--name",
      DIRECT_CLUSTER,
    ],
  )?;
  let elapsed = lane.install(1, None)?;
  eprintln!(
    "kind direct: one member rolled out in {:.1} s",
    elapsed.as_secs_f64()
  );
  lane.bootstrap()?;
  let id = create_volume(lane)?;
  let wrote = run_probe(lane, Probe::Write, &id)?;
  if wrote != format!("wrote {DIRECT_FILE_BYTES}") {
    return Err(Failure(format!(
      "kind direct: the pod's writer did not write the file: {wrote:?}"
    )));
  }
  let before = Counters::read(lane)?;
  let line = run_probe(lane, Probe::Map, &id)?;
  let after = Counters::read(lane)?;
  eprintln!("kind direct: {PROBE_TAG} {line}");
  eprintln!(
    "kind direct: the daemon counted {} hand-off(s), {} fallback(s) to a copy, {} unconfined",
    after.handed.saturating_sub(before.handed),
    after.fell_back.saturating_sub(before.fell_back),
    after.unconfined.saturating_sub(before.unconfined)
  );
  let wanted = format!("zero_copy=true whole=true len={DIRECT_FILE_BYTES}");
  if line != wanted {
    return Err(Failure(format!(
      "kind direct: the pod's process did not map the file whole with no copy: {line:?}, not {wanted:?}"
    )));
  }
  if after.handed.saturating_sub(before.handed) != 1
    || after.fell_back != before.fell_back
    || after.unconfined != before.unconfined
  {
    return Err(Failure(format!(
      "kind direct: the daemon's counters do not show one hand-off and nothing else: before {before:?}, after {after:?}"
    )));
  }
  eprintln!(
    "kind direct: a process in a container of the daemon's pod mapped a {DIRECT_FILE_BYTES}-byte file by the direct \
     map, with no copy"
  );
  prove_map_write(lane, &id)
}

/// Step 5: the probe maps the file read and write, stores, syncs, releases and reads the file back; the daemon's
/// counters must show one more hand-off and two write-backs, none refused, nothing unconfined.
fn prove_map_write(lane: &Lane, id: &str) -> Result<(), Failure> {
  let before = Counters::read(lane)?;
  let line = run_probe(lane, Probe::MapWrite, id)?;
  let after = Counters::read(lane)?;
  eprintln!("kind direct: {MAP_WRITE_TAG} {line}");
  if !writes_through(&line) {
    return Err(Failure(format!(
      "kind direct: the pod's process did not write the file through its map: {line:?}"
    )));
  }
  if after.handed.saturating_sub(before.handed) != 1
    || after.synced.saturating_sub(before.synced) != 2
    || after.sync_refused != before.sync_refused
    || after.unconfined != before.unconfined
  {
    return Err(Failure(format!(
      "kind direct: the daemon's counters do not show one hand-off and two write-backs: before {before:?}, after \
       {after:?}"
    )));
  }
  eprintln!(
    "kind direct: a process in a container of the daemon's pod wrote a file in place through a writable direct map"
  );
  Ok(())
}

/// Whether the map-write probe's line says the sync wrote the stored bytes that differ (some), the release nothing
/// more, and the file holds the store.
fn writes_through(line: &str) -> bool {
  let field = |name: &str| {
    line
      .split_whitespace()
      .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
  };
  let differing = field("differing").and_then(|value| value.parse::<u64>().ok());
  differing.is_some_and(|differing| differing > 0)
    && field("synced").and_then(|value| value.parse::<u64>().ok()) == differing
    && field("released") == Some("0")
    && field("whole") == Some("true")
}

/// Creates the leg's volume on pod 0 once the group accepts it: its id.
fn create_volume(lane: &Lane) -> Result<String, Failure> {
  let pod = lane.pod(0);
  let started = Instant::now();
  loop {
    let created = lane.verb(
      &pod,
      &["volume", "create", "direct", "--bounded", VOLUME_BOUND],
    )?;
    if created.code == 0 {
      let reply: serde_json::Value = serde_json::from_str(&created.stdout)?;
      return reply
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
          Failure(format!(
            "kind direct: create printed no id: {}",
            created.stdout
          ))
        });
    }
    if started.elapsed() > GROUP_WAIT {
      return Err(Failure(format!(
        "kind direct: the volume was not created within {GROUP_WAIT:?}: {}",
        created.stderr.trim()
      )));
    }
    pause(POLL_CLUSTER);
  }
}

/// A run of the probe: its mode, and the ephemeral container and line tag that go with it.
#[derive(Clone, Copy)]
enum Probe {
  /// Writes the file, as one client.
  Write,
  /// Maps the file by the direct map.
  Map,
  /// Maps the file read and write, stores, syncs, releases and reads it back (A-186).
  MapWrite,
}

impl Probe {
  /// The probe's mode argument.
  fn mode(self) -> &'static str {
    match self {
      Probe::Write => "write",
      Probe::Map => "map",
      Probe::MapWrite => "map-write",
    }
  }

  /// The ephemeral container this run is.
  fn container(self) -> &'static str {
    match self {
      Probe::Write => WRITER_CONTAINER,
      Probe::Map => PROBE_CONTAINER,
      Probe::MapWrite => MAP_WRITER_CONTAINER,
    }
  }

  /// The tag of this run's one line.
  fn tag(self) -> &'static str {
    match self {
      Probe::Write => WRITE_TAG,
      Probe::Map => PROBE_TAG,
      Probe::MapWrite => MAP_WRITE_TAG,
    }
  }
}

/// Runs the probe in `probe`'s mode as an ephemeral container of pod 0, and returns its tagged line, the tag taken off.
fn run_probe(lane: &Lane, probe: Probe, id: &str) -> Result<String, Failure> {
  let pod = lane.pod(0);
  let len = DIRECT_FILE_BYTES.to_string();
  let fill = DIRECT_FILL.to_string();
  let path = format!("/{DIRECT_FILE}");
  let image = format!("--image={MAP_PROBE_TAG}");
  let container = format!("--container={}", probe.container());
  let added = lane.kubectl(&[
    "debug",
    &pod,
    &image,
    "--image-pull-policy=IfNotPresent",
    &container,
    "--profile=restricted",
    "--",
    PROBE_BINARY,
    probe.mode(),
    id,
    &path,
    &len,
    &fill,
  ])?;
  if added.code != 0 {
    return Err(Failure(format!(
      "kind direct: kubectl debug refused: {}",
      added.stderr.trim()
    )));
  }
  let exit = format!(
    "jsonpath={{.status.ephemeralContainerStatuses[?(@.name==\"{}\")].state.terminated.exitCode}}",
    probe.container()
  );
  let started = Instant::now();
  let code = loop {
    let state = lane.kubectl(&["get", "pod", &pod, "-o", &exit])?;
    let code = state.stdout.trim();
    if !code.is_empty() {
      break code.to_owned();
    }
    if started.elapsed() > PROBE_WAIT {
      return Err(Failure(format!(
        "kind direct: the probe's {} did not finish within {PROBE_WAIT:?}",
        probe.mode()
      )));
    }
    pause(POLL_CLUSTER);
  };
  let logs = lane.kubectl(&["logs", &pod, "-c", probe.container()])?;
  for line in logs.stdout.lines() {
    eprintln!("kind direct: probe: {line}");
  }
  let tag = probe.tag();
  let line = logs
    .stdout
    .lines()
    .find_map(|line| line.split_once(tag).map(|(_, rest)| rest.trim().to_owned()))
    .ok_or_else(|| {
      Failure(format!(
        "kind direct: the probe's {} printed no {tag} line (exit {code}): {}",
        probe.mode(),
        logs.stdout.trim()
      ))
    })?;
  eprintln!("kind direct: the probe's {} exited {code}", probe.mode());
  Ok(line)
}

/// The daemon's direct-map counters on pod 0, summed over its shards.
#[derive(Debug, Clone, Copy)]
struct Counters {
  handed: u64,
  fell_back: u64,
  unconfined: u64,
  synced: u64,
  sync_refused: u64,
}

impl Counters {
  /// `slates status --json` on pod 0, read for the counters.
  fn read(lane: &Lane) -> Result<Counters, Failure> {
    let status = lane.verb(&lane.pod(0), &["status"])?;
    if status.code != 0 {
      return Err(Failure(format!(
        "kind direct: slates status refused: {}",
        status.stderr.trim()
      )));
    }
    let status: serde_json::Value = serde_json::from_str(&status.stdout)?;
    Ok(Counters {
      handed: counter(&status, HANDED),
      fell_back: counter(&status, FELL_BACK),
      unconfined: counter(&status, UNCONFINED),
      synced: counter(&status, SYNCED),
      sync_refused: counter(&status, SYNC_REFUSED),
    })
  }
}

/// The counter `name` in a status report, summed over its shards (each shard lists its counters as `refusals`, a kind
/// and a count each).
fn counter(status: &serde_json::Value, name: &str) -> u64 {
  status
    .get("shards")
    .and_then(serde_json::Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|shard| shard.get("refusals").and_then(serde_json::Value::as_array))
    .flatten()
    .filter(|entry| entry.get("kind").and_then(serde_json::Value::as_str) == Some(name))
    .filter_map(|entry| entry.get("count").and_then(serde_json::Value::as_u64))
    .fold(0_u64, u64::saturating_add)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: read a counter from a status report shaped as `slates status --json` prints it, over two shards. Expect: the
  /// sum over the shards, and zero for a counter no shard lists.
  #[test]
  fn a_counter_is_summed_over_the_shards() {
    let status = serde_json::json!({
      "shards": [
        { "refusals": [ { "kind": HANDED, "count": 2 }, { "kind": FELL_BACK, "count": 1 } ] },
        { "refusals": [ { "kind": HANDED, "count": 3 } ] },
      ]
    });
    assert_eq!(counter(&status, HANDED), 5);
    assert_eq!(counter(&status, FELL_BACK), 1);
    assert_eq!(counter(&status, UNCONFINED), 0);
  }

  /// Do: judge the map-write probe's line as it prints it after a store that differed in 37 bytes, and lines that tell
  /// of a sync writing other than that, a release writing, a file not as stored, a store that differed nowhere and a
  /// field missing. Expect: the first passes and each other is refused.
  #[test]
  fn a_writable_maps_line_passes_only_when_the_store_reached_the_file() {
    assert!(writes_through(
      "differing=37 synced=37 released=0 whole=true"
    ));
    for line in [
      "differing=37 synced=0 released=0 whole=true",
      "differing=37 synced=37 released=4 whole=true",
      "differing=37 synced=37 released=0 whole=false",
      "differing=0 synced=0 released=0 whole=true",
      "differing=37 released=0 whole=true",
    ] {
      assert!(!writes_through(line), "{line}");
    }
  }
}
