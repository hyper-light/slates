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
//! 6. A pod of its own (A-187), with a network namespace of its own, on pod 0's node, as the daemon's user and under the
//!    same restrictions by its own security context: first, before the daemon has a rendezvous directory, it asks
//!    whether the daemon answers at its abstract socket, and must find no daemon, since that socket is scoped to pod
//!    0's network namespace. Then the node's directory is made the daemon's user's alone, the chart is upgraded to bind
//!    the rendezvous at a socket file there (`rendezvous.directory`, `rendezvous.hostPath`; the daemon restarts, and
//!    being RAM-only it starts empty), the group is bootstrapped, a volume made, and pods of their own, mounting the
//!    node's directory read-only, write the file and write through it by a writable direct map, as step 5 does: the
//!    sync writes the stored bytes that differ, the release nothing more, the file holds the store, and the daemon
//!    counts one hand-off and two write-backs, none refused, nothing unconfined.

use std::path::Path;
use std::time::{Duration, Instant};

use super::export::{MAP_PROBE_TAG, probe_image};
use super::{
  CLUSTER_WAIT, Failure, Lane, NAMESPACE, Options, POLL_CLUSTER, Scratch, capture_with_input,
  certs, down, image, pause, stream, write_scratch,
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
/// Format: where the rendezvous directory is mounted, in the daemon's pod and a workload's pod alike (A-187).
const RENDEZVOUS_DIRECTORY: &str = "/run/slates";
/// Format: the node's directory mounted there, made by the leg for the daemon's user alone.
const RENDEZVOUS_HOST_PATH: &str = "/run/slates-rendezvous";
/// Format: the daemon's user and group in the chart's security context, whose alone the node's directory is.
const DAEMON_USER: u32 = 65_532;
/// Format: the tag of the probe's one line in its reach mode.
const REACH_TAG: &str = "direct-reach:";
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
  prove_own_namespace(lane)
}

/// Step 6 (A-187): pods of their own reach the daemon through the rendezvous's socket file, and only through it.
fn prove_own_namespace(lane: &Lane) -> Result<(), Failure> {
  let node = lane
    .kubectl(&[
      "get",
      "pod",
      &lane.pod(0),
      "-o",
      "jsonpath={.spec.nodeName}",
    ])?
    .stdout
    .trim()
    .to_owned();
  let reached = run_own_pod(lane, &node, OwnPod::reach())?;
  if reached != "unavailable" {
    return Err(Failure(format!(
      "kind direct: a pod of its own reached the daemon's abstract socket, or failed otherwise: {reached:?}"
    )));
  }
  eprintln!(
    "kind direct: a pod of its own finds no daemon at the abstract socket, as Linux scopes it"
  );
  let owner = DAEMON_USER.to_string();
  stream(
    "docker",
    &[
      "exec",
      &node,
      "install",
      "-d",
      "-m",
      "0700",
      "-o",
      &owner,
      "-g",
      &owner,
      RENDEZVOUS_HOST_PATH,
    ],
  )?;
  lane.install_with(
    1,
    None,
    &[
      format!("rendezvous.directory={RENDEZVOUS_DIRECTORY}"),
      format!("rendezvous.hostPath={RENDEZVOUS_HOST_PATH}"),
    ],
  )?;
  lane.bootstrap()?;
  let id = create_volume(lane)?;
  let wrote = run_own_pod(lane, &node, OwnPod::probe(Probe::Write, &id))?;
  if wrote != format!("wrote {DIRECT_FILE_BYTES}") {
    return Err(Failure(format!(
      "kind direct: the pod of its own did not write the file through the socket file: {wrote:?}"
    )));
  }
  let before = Counters::read(lane)?;
  let line = run_own_pod(lane, &node, OwnPod::probe(Probe::MapWrite, &id))?;
  let after = Counters::read(lane)?;
  eprintln!("kind direct: own pod {MAP_WRITE_TAG} {line}");
  if !writes_through(&line) {
    return Err(Failure(format!(
      "kind direct: the pod of its own did not write the file through its map: {line:?}"
    )));
  }
  if after.handed.saturating_sub(before.handed) != 1
    || after.synced.saturating_sub(before.synced) != 2
    || after.sync_refused != before.sync_refused
    || after.unconfined != before.unconfined
  {
    return Err(Failure(format!(
      "kind direct: the daemon's counters do not show one hand-off and two write-backs for the pod of its own: \
       before {before:?}, after {after:?}"
    )));
  }
  eprintln!(
    "kind direct: a pod with a network namespace of its own wrote a file in place through a writable direct map, \
     reaching the daemon at the rendezvous's socket file"
  );
  Ok(())
}

/// A run of the probe in a pod of its own (step 6).
struct OwnPod {
  /// The pod's name, one for each run.
  name: String,
  /// The probe's arguments after its binary.
  args: Vec<String>,
  /// The tag of its one line.
  tag: &'static str,
  /// Whether the node's rendezvous directory is mounted, and the daemon named by its socket file there.
  mounted: bool,
}

impl OwnPod {
  /// The reach mode, with no directory mounted: the daemon named by its abstract instance.
  fn reach() -> OwnPod {
    OwnPod {
      name: "direct-own-reach".to_owned(),
      args: vec!["reach".to_owned()],
      tag: REACH_TAG,
      mounted: false,
    }
  }

  /// `probe`'s mode over the volume `id`, with the directory mounted.
  fn probe(probe: Probe, id: &str) -> OwnPod {
    OwnPod {
      name: format!("direct-own-{}", probe.mode()),
      args: vec![
        probe.mode().to_owned(),
        id.to_owned(),
        format!("/{DIRECT_FILE}"),
        DIRECT_FILE_BYTES.to_string(),
        DIRECT_FILL.to_string(),
      ],
      tag: probe.tag(),
      mounted: true,
    }
  }

  /// The pod's manifest: on `node`, with a network namespace of its own (no host network), as the daemon's user, every
  /// capability dropped, no privilege escalation, a read-only root and the runtime's default seccomp profile: what
  /// `kubectl debug --profile=restricted` gives the earlier steps' containers. When mounted, the node's directory is
  /// read-only at [`RENDEZVOUS_DIRECTORY`] and the daemon named by its socket file there.
  fn manifest(&self, node: &str) -> serde_json::Value {
    let mut command = vec![serde_json::Value::from(PROBE_BINARY)];
    command.extend(
      self
        .args
        .iter()
        .map(|arg| serde_json::Value::from(arg.as_str())),
    );
    let mut container = serde_json::json!({
      "name": "probe",
      "image": MAP_PROBE_TAG,
      "imagePullPolicy": "IfNotPresent",
      "command": command,
      "securityContext": {
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "capabilities": { "drop": ["ALL"] },
      },
    });
    let mut spec = serde_json::json!({
      "nodeName": node,
      "restartPolicy": "Never",
      "securityContext": {
        "runAsNonRoot": true,
        "runAsUser": DAEMON_USER,
        "runAsGroup": DAEMON_USER,
        "seccompProfile": { "type": "RuntimeDefault" },
      },
    });
    if self.mounted {
      container["env"] = serde_json::json!([
        { "name": "SLATES_ENDPOINT", "value": format!("{RENDEZVOUS_DIRECTORY}/default") },
      ]);
      container["volumeMounts"] = serde_json::json!([
        { "name": "rendezvous", "mountPath": RENDEZVOUS_DIRECTORY, "readOnly": true },
      ]);
      spec["volumes"] = serde_json::json!([
        { "name": "rendezvous", "hostPath": { "path": RENDEZVOUS_HOST_PATH, "type": "Directory" } },
      ]);
    }
    spec["containers"] = serde_json::json!([container]);
    serde_json::json!({
      "apiVersion": "v1",
      "kind": "Pod",
      "metadata": { "name": self.name },
      "spec": spec,
    })
  }
}

/// Runs `pod` on `node` to its end and returns its tagged line, the tag taken off; the pod is deleted after. Prints the
/// pod's address beside pod 0's, the evidence that its network namespace is its own.
fn run_own_pod(lane: &Lane, node: &str, pod: OwnPod) -> Result<String, Failure> {
  let context = format!("kind-{}", lane.cluster);
  let manifest = pod.manifest(node).to_string();
  let applied = capture_with_input(
    "kubectl",
    &["--context", &context, "-n", NAMESPACE, "apply", "-f", "-"],
    manifest.as_bytes(),
  )?;
  if applied.code != 0 {
    return Err(Failure(format!(
      "kind direct: the pod {} was refused: {}",
      pod.name,
      applied.stderr.trim()
    )));
  }
  let started = Instant::now();
  let phase = loop {
    let phase = lane.kubectl(&["get", "pod", &pod.name, "-o", "jsonpath={.status.phase}"])?;
    let phase = phase.stdout.trim().to_owned();
    if phase == "Succeeded" || phase == "Failed" {
      break phase;
    }
    if started.elapsed() > PROBE_WAIT {
      return Err(Failure(format!(
        "kind direct: the pod {} did not finish within {PROBE_WAIT:?} (phase {phase:?})",
        pod.name
      )));
    }
    pause(POLL_CLUSTER);
  };
  let addresses = [lane.pod(0), pod.name.clone()].map(|name| {
    lane
      .kubectl(&["get", "pod", &name, "-o", "jsonpath={.status.podIP}"])
      .map(|outcome| outcome.stdout.trim().to_owned())
  });
  let [daemon_address, own_address] = addresses;
  let (daemon_address, own_address) = (daemon_address?, own_address?);
  eprintln!(
    "kind direct: pod {} at {own_address}, pod 0 at {daemon_address}, ended {phase}",
    pod.name
  );
  let logs = lane.kubectl(&["logs", &pod.name])?;
  for line in logs.stdout.lines() {
    eprintln!("kind direct: {}: {line}", pod.name);
  }
  lane.kubectl(&["delete", "pod", &pod.name, "--wait=false"])?;
  if own_address == daemon_address {
    return Err(Failure(format!(
      "kind direct: the pod {} shares pod 0's address, so not a network namespace of its own",
      pod.name
    )));
  }
  logs
    .stdout
    .lines()
    .find_map(|line| {
      line
        .split_once(pod.tag)
        .map(|(_, rest)| rest.trim().to_owned())
    })
    .ok_or_else(|| {
      Failure(format!(
        "kind direct: the pod {} printed no {} line ({phase}): {}",
        pod.name,
        pod.tag,
        logs.stdout.trim()
      ))
    })
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
