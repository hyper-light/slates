//! `cargo xtask kind` — the KIND fleet lane (docs/wip/kind-lane.md; §4.8 "Deployment"; the Helm chart of
//! docs/deploy.md): the fleet proven on real Linux pods over a real network, with the chart an operator
//! installs. Each step is one recorded command, so a run's numbers are a run's commands:
//!
//! - `image [--tag TAG]` — builds the image from the repository's Dockerfile (release profile, distroless).
//! - `smoke [--tag TAG]` — runs one node of that image in plain Docker from a one-node manifest and reads
//!   `slates status` and a volume verb through `docker exec`: the image's proof, before any cluster.
//! - `certs --replicas N --out FILE` — mints N self-signed identities and writes them as chart values.
//! - `up` — creates the kind cluster (`deploy/kind/cluster.yaml`) and loads the images into it.
//! - `install [--replicas N] [--netem DELAY JITTER LOSS]` — installs or upgrades the chart and waits for
//!   the rollout (readiness is `slates status` on every pod).
//! - `prove` — the fleet proof: formation, a volume placed at `f + 1`, the owner's pod deleted (the
//!   SIGKILL takeover), the successor serving; the replacement pod's rejoin is a best-effort observation
//!   (the new-IP rejoin proof in docs/wip/kind-lane.md), not a gate.
//! - `scale` — `replicas=5` and back to 3: the configuration group's membership change, re-forming each time.
//! - `netem` — the WAN profiles of §4.8's owed measurement: 80 ms ± 20 ms, the same with 1 % loss, and
//!   the handshake ceiling at 350 ms; each pod's council timing and the leader's stability over a window.
//! - `down` — deletes the cluster. `all` runs every step in order and deletes the cluster at the end,
//!   also on failure, unless `--keep`.
//!
//! Every artifact the lane writes — self-signed identities, manifests, values files — goes to a scratch
//! directory outside the tree (`$HOME/.cache/slates-kind-lane/<pid>`), named with the process id and
//! removed at the end unless `--keep` is given. The certificates are self-signed here with `rcgen`, as the
//! workspace's fleet tests mint theirs: the lane's stand-in for the operator-provisioned material a
//! production install supplies (docs/deploy.md says which). Nothing here is shipped code: this tool runs
//! `docker`, `kind`, `kubectl` and `helm` and reads and writes its own scratch directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::Failure;

/// Shape: the image tag the lane builds and loads when none is given.
const DEFAULT_TAG: &str = "slates:lane";
/// Shape: the network-shaping init image the lane builds from `deploy/kind/netem.Dockerfile`.
const NETEM_TAG: &str = "slates-netem:lane";
/// Shape: the lane's own kind cluster; created by `up`, deleted by `down`, never anyone else's.
const DEFAULT_CLUSTER: &str = "slates-lane";
/// Shape: the namespace and release the chart is installed as (the pods are `slates-N`).
const NAMESPACE: &str = "slates";
const RELEASE: &str = "slates";
/// Shape: the fleet's TLS name — every minted certificate carries it, every node verifies its peers under it.
const FLEET_NAME: &str = "slates-fleet";
/// Shape: the base UDP port of the first node; each node's block is the base and the next
/// (`slates_server::deploy`), so node N's base is this plus `2 × N`.
const BASE_PORT: u16 = 7000;
/// Shape: the ports a node serves on — one per plane (`slates_server::deploy::PORTS_PER_NODE`).
const PORTS_PER_NODE: u16 = 2;
/// Shape: the fleet sizes the lane proves: three (f = 1), and five (f = 2) for the scale step.
const LANE_REPLICAS: u64 = 3;
const SCALED_REPLICAS: u64 = 5;
/// Shape: how long the smoke run waits for the container's daemon to answer `status` — the anchor measures
/// a full machine profile first (seconds), then the daemon boots.
const SMOKE_START_WAIT: Duration = Duration::from_secs(90);
/// Shape: the poll cadence of a bounded wait on a container — the daemon's heartbeat order.
const POLL: Duration = Duration::from_millis(250);
/// Shape: the poll cadence of a bounded wait on the cluster — a `kubectl exec` round trip is itself tens
/// of milliseconds, so a finer poll would measure kubectl.
const POLL_CLUSTER: Duration = Duration::from_secs(1);
/// Shape: the memory the smoke container is given (`--memory`), a Guaranteed-QoS-like bound the daemon's
/// §4.2 effective capacity clamps to; a stated operator value, as the chart's is.
const SMOKE_MEMORY: &str = "1g";
/// Shape: the CPUs the smoke container is given (`--cpus`); the daemon derives its shard count from the
/// cgroup quota.
const SMOKE_CPUS: &str = "2";
/// Shape: how long `kind create cluster` may take to bring its nodes up (kind's own `--wait`).
const CLUSTER_WAIT: &str = "180s";
/// Shape: how long a rollout may take until every pod is Ready — the image is loaded (no pull), the anchor
/// measures a full profile (seconds), and readiness is `slates status` answering.
const ROLLOUT_WAIT: Duration = Duration::from_secs(300);
/// Shape: how long the fleet gets to form over the pod network — name resolution, the handshake with its
/// retries against a peer not yet listening, and a few protocol periods; the three-process loopback test
/// holds each phase to 40 s, widened for pods that boot in parallel and a shaped path.
const FORMATION_WAIT: Duration = Duration::from_secs(180);
/// Shape: how long a sealed snapshot gets to place at `f + 1` across pods.
const PLACE_WAIT: Duration = Duration::from_secs(60);
/// Shape: how long the survivors get to retire the killed owner and the successor to serve — the
/// Lifeguard death is six backed-off misses (≈ 4 s at rest) and the takeover a phase-one round.
const TAKEOVER_WAIT: Duration = Duration::from_secs(120);
/// Shape: how long the replacement pod is watched for a rejoin — bounded, since the rejoin is a
/// best-effort observation (the new-IP proof in docs/wip/kind-lane.md), not a gate:
/// long enough for a reschedule and boot, not the full retirement window.
const REJOIN_WAIT: Duration = Duration::from_secs(120);
/// Shape: the window the shaped fleet is watched over — minutes, not an hour, on this box (the charter's
/// bound); the leader must not change and the timing must hold across it.
const NETEM_WINDOW: Duration = Duration::from_secs(180);
/// Shape: how often the shaped fleet is sampled within the window.
const NETEM_SAMPLE: Duration = Duration::from_secs(10);
/// Shape: the daemon's coordinator period (`slates_server::daemon::HEARTBEAT_NS`, 100 ms), the unit the
/// council's timing is counted in; the lane checks the reported base against
/// `⌈10 × max(tail, period) / period⌉` on the measured tail.
const HEARTBEAT_NS: u64 = 100_000_000;
/// Shape: Raft's order-of-magnitude margin the daemon derives the election timeout with
/// (`slates_cluster::timing::ELECTION_MARGIN`).
const ELECTION_MARGIN: u64 = 10;
/// Shape: the WAN profiles the lane measures (docs/wip/wan-timeout.md §6): the fabric proof's Japan East →
/// East US one-way profile, the same with 1 % loss, and the handshake ceiling (~330 ms one way, above which
/// the initial-PTO-capped retransmit backoff was never stressed).
const NETEM_PROFILES: [(&str, &str, &str, &str); 3] = [
  ("wan", "80ms", "20ms", "0%"),
  ("wan-loss", "80ms", "20ms", "1%"),
  ("ceiling", "350ms", "0ms", "0%"),
];

/// What the lane was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
  Image,
  Smoke,
  Certs,
  Up,
  Install,
  Prove,
  Scale,
  Netem,
  Down,
  All,
}

/// The lane's options.
#[derive(Debug)]
pub(crate) struct Options {
  pub(crate) step: Step,
  pub(crate) tag: String,
  pub(crate) cluster: String,
  pub(crate) keep: bool,
  /// `certs`: how many identities; `install`: the replica count.
  pub(crate) replicas: u64,
  /// `certs`: where the values file goes.
  pub(crate) out: Option<PathBuf>,
  /// `install`: an identities values file to install with (else the scratch's).
  pub(crate) identities: Option<PathBuf>,
  /// `install`: a netem profile `(delay, jitter, loss)`.
  pub(crate) netem: Option<(String, String, String)>,
}

/// Parses `kind STEP [--tag TAG] [--cluster NAME] [--keep] [--replicas N] [--out FILE]
/// [--identities FILE] [--netem DELAY JITTER LOSS]`.
pub(crate) fn parse(args: &[String]) -> Result<Options, Failure> {
  let step = match args.first().map(String::as_str) {
    Some("image") => Step::Image,
    Some("smoke") => Step::Smoke,
    Some("certs") => Step::Certs,
    Some("up") => Step::Up,
    Some("install") => Step::Install,
    Some("prove") => Step::Prove,
    Some("scale") => Step::Scale,
    Some("netem") => Step::Netem,
    Some("down") => Step::Down,
    Some("all") => Step::All,
    other => {
      return Err(Failure(format!(
        "kind: unknown step {other:?}; steps: image, smoke, certs, up, install, prove, scale, netem, down, all"
      )));
    }
  };
  let mut options = Options {
    step,
    tag: DEFAULT_TAG.to_owned(),
    cluster: DEFAULT_CLUSTER.to_owned(),
    keep: false,
    replicas: LANE_REPLICAS,
    out: None,
    identities: None,
    netem: None,
  };
  let mut rest = args[1..].iter();
  while let Some(arg) = rest.next() {
    let mut value = |name: &str| {
      rest
        .next()
        .cloned()
        .ok_or_else(|| Failure(format!("kind: {name} needs a value")))
    };
    match arg.as_str() {
      "--tag" => options.tag = value("--tag")?,
      "--cluster" => options.cluster = value("--cluster")?,
      "--keep" => options.keep = true,
      "--replicas" => {
        options.replicas = value("--replicas")?
          .parse()
          .map_err(|_| Failure("kind: --replicas needs a number".to_owned()))?;
      }
      "--out" => options.out = Some(PathBuf::from(value("--out")?)),
      "--identities" => options.identities = Some(PathBuf::from(value("--identities")?)),
      "--netem" => {
        options.netem = Some((value("--netem")?, value("--netem")?, value("--netem")?));
      }
      other => return Err(Failure(format!("kind: unknown option `{other}`"))),
    }
  }
  Ok(options)
}

/// Runs the step.
pub(crate) fn run(root: &Path, options: &Options) -> Result<(), Failure> {
  match options.step {
    Step::Image => image(root, &options.tag),
    Step::Smoke => {
      let scratch = Scratch::create(options.keep)?;
      smoke(&options.tag, &scratch)
    }
    Step::Certs => {
      let out = options
        .out
        .clone()
        .ok_or_else(|| Failure("kind certs: --out FILE is required".to_owned()))?;
      certs(options.replicas, &out)
    }
    Step::Down => down(&options.cluster),
    _ => {
      let lane = Lane::new(root, options)?;
      lane.step(options.step)
    }
  }
}

/// Writes a file in the lane's own scratch directory (never in the tree, never under the daemon's R1 wall:
/// this is the development tool's material — a minted identity, a manifest, a values file).
fn write_scratch(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
  #[allow(clippy::disallowed_methods)] // the development tool's own scratch directory
  std::fs::write(path, bytes).map_err(|e| Failure(format!("kind: writing {}: {e}", path.display())))
}

/// Creates the lane's scratch directory.
fn create_scratch(path: &Path) -> Result<(), Failure> {
  #[allow(clippy::disallowed_methods)] // the development tool's own scratch directory
  std::fs::create_dir_all(path)
    .map_err(|e| Failure(format!("kind: creating {}: {e}", path.display())))
}

/// Removes the lane's scratch directory at the end.
fn remove_scratch(path: &Path) {
  #[allow(clippy::disallowed_methods)] // the development tool's own scratch directory
  let _ = std::fs::remove_dir_all(path);
}

/// Waits between two questions to a container or a cluster (a blocking wait is the shape of a command-line
/// tool driving other programs; nothing here runs on the daemon's runtime).
fn pause(interval: Duration) {
  #[allow(clippy::disallowed_methods)] // a development tool's poll cadence, not daemon code
  std::thread::sleep(interval);
}

/// A command's captured outcome.
struct Outcome {
  code: i32,
  stdout: String,
  stderr: String,
}

/// Runs `program args` to completion, capturing its output.
fn capture(program: &str, args: &[&str]) -> Result<Outcome, Failure> {
  let output = Command::new(program)
    .args(args)
    .stdin(Stdio::null())
    .output()
    .map_err(|e| Failure(format!("kind: running `{program} {}`: {e}", args.join(" "))))?;
  Ok(Outcome {
    code: output.status.code().unwrap_or(-1),
    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
  })
}

/// Runs `program args`, streaming its output to this terminal, and fails on a non-zero exit.
fn stream(program: &str, args: &[&str]) -> Result<(), Failure> {
  eprintln!("kind: $ {program} {}", args.join(" "));
  let status = Command::new(program)
    .args(args)
    .stdin(Stdio::null())
    .status()
    .map_err(|e| Failure(format!("kind: running `{program} {}`: {e}", args.join(" "))))?;
  if status.success() {
    Ok(())
  } else {
    Err(Failure(format!(
      "kind: `{program} {}` exited {}",
      args.join(" "),
      status.code().unwrap_or(-1)
    )))
  }
}

/// Runs `program args` and fails, naming the command and its stderr, on a non-zero exit; returns stdout.
fn must(program: &str, args: &[&str]) -> Result<String, Failure> {
  let outcome = capture(program, args)?;
  if outcome.code == 0 {
    Ok(outcome.stdout)
  } else {
    Err(Failure(format!(
      "kind: `{program} {}` exited {}: {}",
      args.join(" "),
      outcome.code,
      outcome.stderr.trim()
    )))
  }
}

/// The lane's scratch directory: outside the tree, named with the process id, removed when dropped unless
/// kept.
struct Scratch {
  path: PathBuf,
  keep: bool,
}

impl Scratch {
  fn create(keep: bool) -> Result<Scratch, Failure> {
    let home = std::env::var_os("HOME")
      .map(PathBuf::from)
      .ok_or_else(|| Failure("kind: HOME is not set".to_owned()))?;
    let path = home
      .join(".cache")
      .join("slates-kind-lane")
      .join(std::process::id().to_string());
    create_scratch(&path)?;
    eprintln!("kind: scratch directory {}", path.display());
    Ok(Scratch { path, keep })
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    if self.keep {
      eprintln!("kind: kept {}", self.path.display());
    } else {
      remove_scratch(&self.path);
    }
  }
}

/// A self-signed identity carrying the fleet's TLS name: the DER certificate and the DER key.
struct Identity {
  certificate: Vec<u8>,
  key: Vec<u8>,
}

/// Mints a self-signed identity carrying the fleet's TLS name (as the workspace's fleet tests mint theirs).
fn mint() -> Result<Identity, Failure> {
  let key = rcgen::KeyPair::generate().map_err(|e| Failure(format!("kind: key pair: {e}")))?;
  let cert = rcgen::CertificateParams::new(vec![FLEET_NAME.to_owned()])
    .map_err(|e| Failure(format!("kind: certificate params: {e}")))?
    .self_signed(&key)
    .map_err(|e| Failure(format!("kind: self-signing: {e}")))?;
  Ok(Identity {
    certificate: cert.der().to_vec(),
    key: key.serialize_der(),
  })
}

/// Mints an identity for `node` and writes it as DER files into `dir` (`NODE.crt.der`, `NODE.key.der`) —
/// what the daemon reads beside its manifest.
fn mint_identity(dir: &Path, node: &str) -> Result<(), Failure> {
  let identity = mint()?;
  write_scratch(&dir.join(format!("{node}.crt.der")), &identity.certificate)?;
  write_scratch(&dir.join(format!("{node}.key.der")), &identity.key)?;
  Ok(())
}

/// Format: the standard base64 alphabet (RFC 4648 §4), for the chart's `binaryData` and Secret values.
const BASE64_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648 §4): what a Kubernetes `binaryData` or Secret `data` value is.
fn base64(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let word = chunk.iter().enumerate().fold(0u32, |word, (index, byte)| {
      word | (u32::from(*byte) << (16 - 8 * index))
    });
    let sextets = [
      (word >> 18) & 0x3F,
      (word >> 12) & 0x3F,
      (word >> 6) & 0x3F,
      word & 0x3F,
    ];
    for (index, sextet) in sextets.iter().enumerate() {
      if index <= chunk.len() {
        out.push(char::from(
          BASE64_ALPHABET[usize::try_from(*sextet).unwrap_or(0)],
        ));
      } else {
        out.push('=');
      }
    }
  }
  out
}

/// `certs`: mints `replicas` identities for the pods `slates-0..N` and writes them as the chart's
/// `certificates` values (base64 DER) to `out`.
fn certs(replicas: u64, out: &Path) -> Result<(), Failure> {
  let mut values = String::from("certificates:\n");
  for index in 0..replicas {
    let identity = mint()?;
    values.push_str(&format!(
      "  {RELEASE}-{index}:\n    certificate: {}\n    key: {}\n",
      base64(&identity.certificate),
      base64(&identity.key)
    ));
  }
  write_scratch(out, values.as_bytes())?;
  eprintln!(
    "kind: {replicas} self-signed identities for {RELEASE}-0..{} written to {}",
    replicas.saturating_sub(1),
    out.display()
  );
  Ok(())
}

/// The base port of node `index` in a fleet of port blocks.
fn base_port(index: u16) -> u16 {
  BASE_PORT.saturating_add(index.saturating_mul(PORTS_PER_NODE))
}

/// Writes the shared manifest for `nodes` — each `(name, host)`, dialed at its own port block — at `f`,
/// naming the DER files beside it; returns its path.
fn write_manifest(dir: &Path, nodes: &[(String, String)], f: u32) -> Result<PathBuf, Failure> {
  let entries: Vec<serde_json::Value> = nodes
    .iter()
    .enumerate()
    .map(|(index, (node, host))| {
      serde_json::json!({
        "node": node,
        "address": format!("{host}:{}", base_port(u16::try_from(index).unwrap_or(u16::MAX))),
        "certificate": format!("{node}.crt.der"),
        "key": format!("{node}.key.der"),
      })
    })
    .collect();
  let manifest = serde_json::json!({ "name": FLEET_NAME, "f": f, "nodes": entries });
  let path = dir.join("fleet.json");
  write_scratch(&path, serde_json::to_string_pretty(&manifest)?.as_bytes())?;
  Ok(path)
}

/// `image`: `docker build -t TAG .` from the repository root, streamed; the elapsed time is printed.
fn image(root: &Path, tag: &str) -> Result<(), Failure> {
  let started = Instant::now();
  let root_text = root.to_string_lossy().into_owned();
  stream("docker", &["build", "-t", tag, &root_text])?;
  let size = must(
    "docker",
    &["image", "inspect", tag, "--format", "{{.Size}}"],
  )?;
  eprintln!(
    "kind: image {tag} built in {:.1} s; {} bytes",
    started.elapsed().as_secs_f64(),
    size.trim()
  );
  Ok(())
}

/// The netem init image, from `deploy/kind/netem.Dockerfile`.
fn netem_image(root: &Path) -> Result<(), Failure> {
  let dockerfile = root.join("deploy/kind/netem.Dockerfile");
  let context = root.join("deploy/kind");
  stream(
    "docker",
    &[
      "build",
      "-t",
      NETEM_TAG,
      "-f",
      &dockerfile.to_string_lossy(),
      &context.to_string_lossy(),
    ],
  )
}

/// A JSON field as `u64`, else zero.
fn u64_of(value: &serde_json::Value, key: &str) -> u64 {
  value
    .get(key)
    .and_then(serde_json::Value::as_u64)
    .unwrap_or(0)
}

/// The fleet block of a `slates status --json` document.
fn fleet_of(status: &serde_json::Value) -> Result<&serde_json::Value, Failure> {
  status
    .get("fleet")
    .ok_or_else(|| Failure(format!("kind: status has no fleet block: {status}")))
}

/// `smoke`: one node of the image in plain Docker — a one-node manifest at `f = 0` (the laptop degenerate,
/// R8) mounted read-only, the anchor as PID 1 — then `slates status` and a volume verb through `docker exec`
/// until they answer within the start wait. Prints the time to the first answer and the fleet lines.
fn smoke(tag: &str, scratch: &Scratch) -> Result<(), Failure> {
  let node = "slates-0";
  mint_identity(&scratch.path, node)?;
  let manifest = write_manifest(
    &scratch.path,
    &[(node.to_owned(), "127.0.0.1".to_owned())],
    0,
  )?;
  let name = format!("slates-smoke-{}", std::process::id());
  let volume = format!("{}:/etc/slates:ro", scratch.path.display());
  let container = must(
    "docker",
    &[
      "run",
      "-d",
      "--name",
      &name,
      "--memory",
      SMOKE_MEMORY,
      "--cpus",
      SMOKE_CPUS,
      "-v",
      &volume,
      tag,
      "anchor",
      "--fleet",
      "/etc/slates/fleet.json",
      "--node",
      node,
    ],
  )?;
  eprintln!(
    "kind: container {name} ({}) from {} with --memory {SMOKE_MEMORY} --cpus {SMOKE_CPUS}",
    container.trim(),
    manifest.display()
  );
  let outcome = smoke_in(&name);
  let logs = capture("docker", &["logs", &name])
    .map(|o| o.stderr)
    .unwrap_or_default();
  let _ = capture("docker", &["rm", "-f", &name]);
  eprintln!("kind: the container's log:\n{logs}");
  outcome
}

/// The smoke checks against the running container `name`.
fn smoke_in(name: &str) -> Result<(), Failure> {
  let started = Instant::now();
  let status = loop {
    let outcome = capture("docker", &["exec", name, "/slates", "status", "--json"])?;
    if outcome.code == 0 {
      break outcome.stdout;
    }
    if started.elapsed() > SMOKE_START_WAIT {
      return Err(Failure(format!(
        "kind: the node did not answer status within {SMOKE_START_WAIT:?}: exit {} {}",
        outcome.code,
        outcome.stderr.trim()
      )));
    }
    pause(POLL);
  };
  let first_answer = started.elapsed();
  let status: serde_json::Value = serde_json::from_str(&status)?;
  let fleet = fleet_of(&status)?;
  let members = fleet
    .get("members")
    .and_then(serde_json::Value::as_array)
    .map(Vec::len)
    .unwrap_or(0);
  eprintln!(
    "kind: status answered after {:.2} s: shards {}; fleet f={} members={members} peers_probed={} council_leads={} council_base_periods={}",
    first_answer.as_secs_f64(),
    status
      .get("shards")
      .and_then(serde_json::Value::as_array)
      .map(Vec::len)
      .unwrap_or(0),
    u64_of(fleet, "f"),
    u64_of(fleet, "peers_probed"),
    fleet
      .get("council")
      .and_then(|c| c.get("leads"))
      .unwrap_or(&serde_json::Value::Null),
    fleet
      .get("council")
      .map(|c| u64_of(c, "base_periods"))
      .unwrap_or(0),
  );
  if u64_of(fleet, "f") != 0 || members != 1 {
    return Err(Failure(format!(
      "kind: a one-node manifest is the solo degenerate (f 0, one member): {fleet}"
    )));
  }
  must("docker", &["exec", name, "/slates", "bootstrap", "root"])?;
  // A verb through the same rendezvous: the node serves, not merely answers status.
  let created = must(
    "docker",
    &[
      "exec",
      name,
      "/slates",
      "volume",
      "create",
      "smoke",
      "--bounded",
      "8MiB",
      "--json",
    ],
  )?;
  let created: serde_json::Value = serde_json::from_str(&created)?;
  let id = created
    .get("id")
    .and_then(serde_json::Value::as_str)
    .ok_or_else(|| Failure(format!("kind: create answered no id: {created}")))?;
  let listed = must(
    "docker",
    &["exec", name, "/slates", "volume", "list", "--json"],
  )?;
  if !listed.contains(id) {
    return Err(Failure(format!(
      "kind: the created volume {id} is not listed: {listed}"
    )));
  }
  eprintln!("kind: smoke ok: volume {id} created and listed in the container");
  Ok(())
}

/// `down`: deletes the lane's cluster.
fn down(cluster: &str) -> Result<(), Failure> {
  stream("kind", &["delete", "cluster", "--name", cluster])
}

/// A node's fleet view, from `slates status --json` on its pod.
#[derive(Clone, Debug)]
struct View {
  pod: String,
  host: u64,
  f: u64,
  members: Vec<u64>,
  peers_probed: u64,
  leads: bool,
  base_periods: u64,
  span_periods: u64,
  rtt_tail_ns: u64,
  rtt_spread_ns: u64,
  samples: u64,
  /// The `fleet.resolve` refusals summed over the shards: a peer's name that did not resolve at a dial.
  resolve_refused: u64,
}

impl View {
  fn parse(pod: &str, status: &serde_json::Value) -> Result<View, Failure> {
    let fleet = fleet_of(status)?;
    let council = fleet
      .get("council")
      .ok_or_else(|| Failure(format!("kind: status has no council block: {status}")))?;
    let mut members: Vec<u64> = fleet
      .get("members")
      .and_then(serde_json::Value::as_array)
      .map(|list| list.iter().filter_map(serde_json::Value::as_u64).collect())
      .unwrap_or_default();
    members.sort_unstable();
    let resolve_refused = status
      .get("shards")
      .and_then(serde_json::Value::as_array)
      .map(|shards| {
        shards
          .iter()
          .flat_map(|shard| {
            shard
              .get("refusals")
              .and_then(serde_json::Value::as_array)
              .cloned()
              .unwrap_or_default()
          })
          .filter(|refusal| {
            refusal.get("kind").and_then(serde_json::Value::as_str) == Some("fleet.resolve")
          })
          .map(|refusal| u64_of(&refusal, "count"))
          .sum()
      })
      .unwrap_or(0);
    Ok(View {
      pod: pod.to_owned(),
      host: u64_of(fleet, "host"),
      f: u64_of(fleet, "f"),
      members,
      peers_probed: u64_of(fleet, "peers_probed"),
      leads: council
        .get("leads")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false),
      base_periods: u64_of(council, "base_periods"),
      span_periods: u64_of(council, "span_periods"),
      rtt_tail_ns: u64_of(council, "rtt_tail_ns"),
      rtt_spread_ns: u64_of(council, "rtt_spread_ns"),
      samples: u64_of(council, "samples"),
      resolve_refused,
    })
  }

  /// One line of the view, for the record.
  fn line(&self) -> String {
    format!(
      "{}: host={} f={} members={:?} peers_probed={} leads={} base={} span={} tail_ns={} spread_ns={} samples={} resolve_refused={}",
      self.pod,
      self.host,
      self.f,
      self.members,
      self.peers_probed,
      self.leads,
      self.base_periods,
      self.span_periods,
      self.rtt_tail_ns,
      self.rtt_spread_ns,
      self.samples,
      self.resolve_refused
    )
  }
}

/// A member count as the fleet counts it.
fn count(members: &[u64]) -> u64 {
  u64::try_from(members.len()).unwrap_or(u64::MAX)
}

/// Nanoseconds as milliseconds with one decimal, for the record (integer arithmetic; no float cast).
fn milliseconds(ns: u64) -> String {
  /// Format: nanoseconds per millisecond, and per tenth of one.
  const NS_PER_MS: u64 = 1_000_000;
  const NS_PER_TENTH_MS: u64 = 100_000;
  format!("{}.{}", ns / NS_PER_MS, (ns % NS_PER_MS) / NS_PER_TENTH_MS)
}

/// The derived base the daemon should report for a measured `tail_ns`:
/// `⌈ELECTION_MARGIN × max(tail, heartbeat) / heartbeat⌉` periods (§4.8 "Derived constants").
fn expected_base_periods(tail_ns: u64) -> u64 {
  ELECTION_MARGIN
    .saturating_mul(tail_ns.max(HEARTBEAT_NS))
    .div_ceil(HEARTBEAT_NS)
}

/// The lane over one cluster.
struct Lane {
  root: PathBuf,
  cluster: String,
  tag: String,
  /// Held for its `Drop`: the identities file lives in it until the lane ends.
  _scratch: Scratch,
  keep: bool,
  identities: PathBuf,
  replicas: u64,
  netem: Option<(String, String, String)>,
}

impl Lane {
  fn new(root: &Path, options: &Options) -> Result<Lane, Failure> {
    let scratch = Scratch::create(options.keep)?;
    let identities = match &options.identities {
      Some(path) => path.clone(),
      None => {
        // One identities file for every fleet size the lane installs (the scale step's five cover three).
        let path = scratch.path.join("identities.yaml");
        certs(SCALED_REPLICAS.max(options.replicas), &path)?;
        path
      }
    };
    Ok(Lane {
      root: root.to_path_buf(),
      cluster: options.cluster.clone(),
      tag: options.tag.clone(),
      _scratch: scratch,
      keep: options.keep,
      identities,
      replicas: options.replicas,
      netem: options.netem.clone(),
    })
  }

  fn step(&self, step: Step) -> Result<(), Failure> {
    match step {
      Step::Up => self.up(),
      Step::Install => self
        .install(self.replicas, self.netem.as_ref())
        .map(|elapsed| {
          eprintln!(
            "kind: installed and rolled out in {:.1} s; for a new group, explicitly run /slates bootstrap root on one pod",
            elapsed.as_secs_f64()
          )
        }),
      Step::Prove => self.prove(),
      Step::Scale => self.scale(),
      Step::Netem => self.netem_profiles(),
      Step::All => self.all(),
      Step::Image | Step::Smoke | Step::Certs | Step::Down => Ok(()),
    }
  }

  /// `all`: every step in order; the cluster is deleted at the end whatever happened, unless kept.
  fn all(&self) -> Result<(), Failure> {
    let outcome = self.all_steps();
    if self.keep {
      eprintln!("kind: cluster {} kept", self.cluster);
    } else if let Err(e) = down(&self.cluster) {
      eprintln!("kind: {e}");
    }
    outcome
  }

  fn all_steps(&self) -> Result<(), Failure> {
    image(&self.root, &self.tag)?;
    self.up()?;
    let elapsed = self.install(LANE_REPLICAS, None)?;
    eprintln!(
      "kind: installed and rolled out {LANE_REPLICAS} replicas in {:.1} s",
      elapsed.as_secs_f64()
    );
    self.bootstrap()?;
    self.prove()?;
    self.scale()?;
    self.netem_profiles()
  }

  /// `up`: the cluster from `deploy/kind/cluster.yaml`, then the images loaded into every node.
  fn up(&self) -> Result<(), Failure> {
    let started = Instant::now();
    let config = self.root.join("deploy/kind/cluster.yaml");
    stream(
      "kind",
      &[
        "create",
        "cluster",
        "--name",
        &self.cluster,
        "--config",
        &config.to_string_lossy(),
        "--wait",
        CLUSTER_WAIT,
      ],
    )?;
    let created = started.elapsed();
    netem_image(&self.root)?;
    stream(
      "kind",
      &[
        "load",
        "docker-image",
        &self.tag,
        NETEM_TAG,
        "--name",
        &self.cluster,
      ],
    )?;
    eprintln!(
      "kind: cluster {} up in {:.1} s, images loaded in {:.1} s",
      self.cluster,
      created.as_secs_f64(),
      started.elapsed().saturating_sub(created).as_secs_f64()
    );
    Ok(())
  }

  /// `kubectl` against the lane's cluster and namespace.
  fn kubectl(&self, args: &[&str]) -> Result<Outcome, Failure> {
    let context = format!("kind-{}", self.cluster);
    let mut with: Vec<&str> = vec!["--context", &context, "-n", NAMESPACE];
    with.extend_from_slice(args);
    capture("kubectl", &with)
  }

  /// `install`: the chart installed or upgraded at `replicas`, with a netem profile when given, then the
  /// rollout waited for (readiness is `slates status` on every pod). Returns the elapsed time.
  fn install(
    &self,
    replicas: u64,
    netem: Option<&(String, String, String)>,
  ) -> Result<Duration, Failure> {
    let started = Instant::now();
    let chart = self.root.join("deploy/helm/slates");
    let lane_values = self.root.join("deploy/kind/values-lane.yaml");
    let netem_values = self.root.join("deploy/kind/values-netem.yaml");
    let context = format!("kind-{}", self.cluster);
    let replicas_set = format!("replicas={replicas}");
    let mut args: Vec<String> = [
      "upgrade",
      "--install",
      RELEASE,
      &chart.to_string_lossy(),
      "--kube-context",
      &context,
      "--namespace",
      NAMESPACE,
      "--create-namespace",
      "-f",
      &lane_values.to_string_lossy(),
      "-f",
      &self.identities.to_string_lossy(),
      "--set",
      &replicas_set,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if let Some((delay, jitter, loss)) = netem {
      args.extend([
        "-f".to_owned(),
        netem_values.to_string_lossy().into_owned(),
        "--set".to_owned(),
        format!("netem.delay={delay}"),
        "--set".to_owned(),
        format!("netem.jitter={jitter}"),
        "--set".to_owned(),
        format!("netem.loss={loss}"),
      ]);
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    stream("helm", &borrowed)?;
    self.wait_rollout(replicas, started)?;
    Ok(started.elapsed())
  }

  /// Waits until the StatefulSet reports `replicas` Ready pods on its current revision (readiness is
  /// `slates status` on each pod), bounded by [`ROLLOUT_WAIT`] from `started`. `kubectl rollout status
  /// --timeout` did not bound its wait on a stalled pod (measured 14 min for a 300 s timeout, 2026-09-14),
  /// so the bound is this loop's own. On the bound every not-Ready pod's events and last log lines are
  /// dumped, so a pod that never answered `status` is diagnosable even after the cluster is deleted.
  fn wait_rollout(&self, replicas: u64, started: Instant) -> Result<(), Failure> {
    let statefulset = format!("statefulset/{RELEASE}");
    loop {
      let ready = self.kubectl(&[
        "get",
        &statefulset,
        "-o",
        "jsonpath={.status.readyReplicas} {.status.updatedReplicas} {.status.currentRevision} {.status.updateRevision}",
      ])?;
      let mut fields = ready.stdout.split_whitespace();
      let ready_replicas: u64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
      let updated_replicas: u64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
      let current = fields.next().unwrap_or("");
      let update = fields.next().unwrap_or("");
      if ready_replicas == replicas && updated_replicas == replicas && current == update {
        return Ok(());
      }
      if started.elapsed() > ROLLOUT_WAIT {
        let pods = self.kubectl(&["get", "pods", "-o", "wide"])?;
        let mut report = format!(
          "kind: the rollout of {replicas} replicas did not complete within {ROLLOUT_WAIT:?} (ready {ready_replicas}, updated {updated_replicas}, revision {current} → {update}):\n{}",
          pods.stdout
        );
        report.push_str(&self.not_ready_diagnostics()?);
        return Err(Failure(report));
      }
      pause(POLL_CLUSTER);
    }
  }

  /// The events and the last log lines of every pod that is not Ready — what a not-answering `status`
  /// left behind (an anchor crash loop, a daemon that never served, a probe that never passed).
  fn not_ready_diagnostics(&self) -> Result<String, Failure> {
    /// Shape: the log lines kept per not-Ready pod in the report — the boot lines and the last refusals.
    const LOG_TAIL: &str = "60";
    let names = self.kubectl(&[
      "get",
      "pods",
      "-o",
      "jsonpath={range .items[*]}{.metadata.name}={.status.containerStatuses[0].ready}{\"\\n\"}{end}",
    ])?;
    let mut out = String::new();
    for line in names.stdout.lines() {
      let Some((pod, ready)) = line.split_once('=') else {
        continue;
      };
      if ready == "true" {
        continue;
      }
      let described = self.kubectl(&["describe", "pod", pod])?;
      let events = described
        .stdout
        .lines()
        .skip_while(|l| !l.starts_with("Events:"))
        .collect::<Vec<&str>>()
        .join("\n");
      let logs = self.kubectl(&["logs", pod, "--tail", LOG_TAIL])?;
      out.push_str(&format!(
        "\n--- {pod} (not Ready) events:\n{events}\n--- {pod} log (last {LOG_TAIL} lines):\n{}{}",
        logs.stdout, logs.stderr
      ));
    }
    Ok(out)
  }

  /// Uninstalls the release and waits for every pod to be gone. Each `scale` and `netem`
  /// fixture then starts and explicitly bootstraps fresh groups. These histories measure
  /// initial formation at each size and profile; rolling replacement is a separate proof.
  fn uninstall(&self) -> Result<(), Failure> {
    let context = format!("kind-{}", self.cluster);
    let _ = capture(
      "helm",
      &[
        "uninstall",
        RELEASE,
        "--kube-context",
        &context,
        "--namespace",
        NAMESPACE,
        "--wait",
      ],
    )?;
    let started = Instant::now();
    loop {
      let pods = self.kubectl(&["get", "pods", "--no-headers"])?;
      if pods.stdout.trim().is_empty() {
        return Ok(());
      }
      if started.elapsed() > ROLLOUT_WAIT {
        return Err(Failure(format!(
          "kind: pods did not terminate within {ROLLOUT_WAIT:?} of uninstall:\n{}",
          pods.stdout
        )));
      }
      pause(POLL_CLUSTER);
    }
  }

  /// A **fresh** install at `replicas` (optionally shaped): uninstall, then install, so the whole fleet
  /// boots together. Returns the rollout time.
  fn fresh_install(
    &self,
    replicas: u64,
    netem: Option<&(String, String, String)>,
  ) -> Result<Duration, Failure> {
    self.uninstall()?;
    let elapsed = self.install(replicas, netem)?;
    self.bootstrap()?;
    Ok(elapsed)
  }

  /// Explicit creation for a fresh test deployment only. Upgrade and pod restart never invoke it.
  fn bootstrap(&self) -> Result<(), Failure> {
    let outcome = self.kubectl(&["exec", &self.pod(0), "--", "/slates", "bootstrap", "root"])?;
    if outcome.code != 0 {
      return Err(Failure(format!(
        "kind: initial bootstrap refused: {}",
        outcome.stderr.trim()
      )));
    }
    Ok(())
  }

  fn pod(&self, index: u64) -> String {
    format!("{RELEASE}-{index}")
  }

  /// `slates status --json` on `pod`, or `None` when it does not answer (exit ≠ 0).
  fn view(&self, pod: &str) -> Result<Option<View>, Failure> {
    let outcome = self.kubectl(&["exec", pod, "--", "/slates", "status", "--json"])?;
    if outcome.code != 0 {
      return Ok(None);
    }
    let status: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    View::parse(pod, &status).map(Some)
  }

  fn views(&self, pods: &[String]) -> Result<Vec<Option<View>>, Failure> {
    pods.iter().map(|pod| self.view(pod)).collect()
  }

  /// Polls `pods` until `formed` holds of their views or `bound` passes; returns the views and the time.
  fn wait_views(
    &self,
    pods: &[String],
    bound: Duration,
    what: &str,
    formed: impl Fn(&[Option<View>]) -> bool,
  ) -> Result<(Vec<Option<View>>, Duration), Failure> {
    let started = Instant::now();
    loop {
      let views = self.views(pods)?;
      if formed(&views) {
        return Ok((views, started.elapsed()));
      }
      if started.elapsed() > bound {
        let lines: Vec<String> = views
          .iter()
          .map(|view| view.as_ref().map_or("(no answer)".to_owned(), View::line))
          .collect();
        return Err(Failure(format!(
          "kind: {what} did not happen within {bound:?}:\n{}",
          lines.join("\n")
        )));
      }
      pause(POLL_CLUSTER);
    }
  }

  /// The fleet has formed at `n` members: every pod answers with `n` members, `n − 1` peers probed, the
  /// same member set, and exactly one council leader among them.
  fn formed_at(n: u64, views: &[Option<View>]) -> bool {
    let all: Vec<&View> = views.iter().flatten().collect();
    if all.len() != views.len() {
      return false;
    }
    let same_members = all.iter().all(|view| {
      count(&view.members) == n && view.members == all[0].members && view.peers_probed == n - 1
    });
    same_members && all.iter().filter(|view| view.leads).count() == 1
  }

  /// Waits for the fleet of `n` pods to form and prints every view.
  fn wait_formed(&self, n: u64, what: &str) -> Result<Vec<View>, Failure> {
    let pods: Vec<String> = (0..n).map(|index| self.pod(index)).collect();
    let (views, elapsed) = self.wait_views(&pods, FORMATION_WAIT, what, |views| {
      Self::formed_at(n, views)
    })?;
    let views: Vec<View> = views.into_iter().flatten().collect();
    eprintln!("kind: {what} in {:.1} s:", elapsed.as_secs_f64());
    for view in &views {
      eprintln!("kind:   {}", view.line());
    }
    Ok(views)
  }

  /// A verb on `pod`, as JSON.
  fn verb(&self, pod: &str, args: &[&str]) -> Result<Outcome, Failure> {
    let mut full = vec!["exec", pod, "--", "/slates"];
    full.extend_from_slice(args);
    full.push("--json");
    self.kubectl(&full)
  }

  /// `prove`: formation, placement across pods, the owner's pod deleted as a crash would kill it, the
  /// survivors' retirement of it, the successor serving the volume, and the replacement pod rejoining.
  fn prove(&self) -> Result<(), Failure> {
    let n = LANE_REPLICAS;
    let views = self.wait_formed(n, "the fleet formed")?;
    let owner = self.pod(0);
    let owner_host = views
      .iter()
      .find(|view| view.pod == owner)
      .map(|view| view.host)
      .unwrap_or(0);

    // A volume sealed on the owner places at f + 1 across pods.
    let created = self.verb(&owner, &["volume", "create", "lane", "--bounded", "8MiB"])?;
    let created: serde_json::Value = serde_json::from_str(&created.stdout).map_err(|e| {
      Failure(format!(
        "kind: create answered no JSON ({e}): {}",
        created.stderr
      ))
    })?;
    let id = created
      .get("id")
      .and_then(serde_json::Value::as_str)
      .ok_or_else(|| Failure(format!("kind: create answered no id: {created}")))?
      .to_owned();
    let snapshot = self.verb(&owner, &["volume", "snapshot", &id])?;
    let snapshot: serde_json::Value = serde_json::from_str(&snapshot.stdout).map_err(|e| {
      Failure(format!(
        "kind: snapshot answered no JSON ({e}): {}",
        snapshot.stderr
      ))
    })?;
    let snapshot = u64_of(&snapshot, "snapshot").to_string();
    let placed_in = self.wait_placed(&owner, &id, &snapshot)?;
    eprintln!(
      "kind: volume {id} snapshot {snapshot} placed at f + 1 across pods in {:.1} s",
      placed_in.as_secs_f64()
    );
    let survivors: Vec<String> = (1..n).map(|index| self.pod(index)).collect();
    for survivor in &survivors {
      let stat = self.verb(survivor, &["volume", "stat", &id])?;
      if stat.code == 0 {
        return Err(Failure(format!(
          "kind: a holder must not serve the owner's volume before the takeover: {survivor} answered {}",
          stat.stdout
        )));
      }
    }

    // The owner's pod is killed as a crash would kill it (SIGKILL, no grace).
    let killed_at = Instant::now();
    let deleted = self.kubectl(&["delete", "pod", &owner, "--grace-period=0", "--force"])?;
    if deleted.code != 0 {
      return Err(Failure(format!(
        "kind: deleting {owner}: {}",
        deleted.stderr
      )));
    }
    let (_, retired_in) = self.wait_views(
      &survivors,
      TAKEOVER_WAIT,
      "the survivors retired the owner",
      |views| {
        views.iter().all(|view| {
          view.as_ref().is_some_and(|view| {
            count(&view.members) == n - 1 && !view.members.contains(&owner_host)
          })
        })
      },
    )?;
    eprintln!(
      "kind: both survivors retired the dead owner {owner_host} {:.1} s after the delete",
      retired_in.as_secs_f64()
    );
    let (successor, served_in) = self.wait_successor(&survivors, &id, killed_at)?;
    eprintln!(
      "kind: {successor} took the volume over and serves it placed {:.1} s after the delete",
      served_in.as_secs_f64()
    );

    // The replacement pod comes back under its name at a new IP. Whether it **rejoins** the mesh is
    // reported, not required: a whole-pod restart loses the RAM anchor segment, so the replacement boots
    // at incarnation 0 with the **same** manifest seed member id its retired predecessor held (RAM-only:
    // there is no durable start count to advance across a pod restart, R1). Re-admitting that same id
    // through SWIM refutation after the survivors retired it is the "restart = join" gap for Kubernetes,
    // recorded in docs/wip/kind-lane.md — separate from the takeover proof above (the successor serves),
    // which is the charter's assertion and which passed.
    let all: Vec<String> = (0..n).map(|index| self.pod(index)).collect();
    match self.wait_views(&all, REJOIN_WAIT, "the replacement pod rejoined", |views| {
      Self::formed_at(n, views)
    }) {
      Ok((views, rejoined_in)) => {
        eprintln!(
          "kind: the replacement {owner} rejoined; the fleet re-formed at {n} members {:.1} s after the delete:",
          rejoined_in.as_secs_f64() + served_in.as_secs_f64()
        );
        for view in views.iter().flatten() {
          eprintln!("kind:   {}", view.line());
        }
      }
      Err(_) => {
        let views = self.views(&all)?;
        eprintln!(
          "kind: the replacement {owner} did NOT rejoin the mesh within {REJOIN_WAIT:?} (the new-IP proof in docs/wip/kind-lane.md); the takeover stands — {successor} serves the volume. Views:"
        );
        for view in views.iter().flatten() {
          eprintln!("kind:   {}", view.line());
        }
        // The replacement forms no probe session (`peers_probed=0`): dump the fleet log lines of the
        // replacement and one survivor so a diagnosis sees *why* the handshakes do not complete (the
        // debugging protocol's "logs first"). Best-effort — a failure to read logs is not the lane's.
        let survivor = survivors
          .first()
          .map(String::as_str)
          .unwrap_or(owner.as_str());
        for pod in [owner.as_str(), survivor] {
          if let Ok(logs) = self.kubectl(&["logs", pod, "--tail", "80"]) {
            let fleet: Vec<&str> = logs
              .stdout
              .lines()
              .filter(|l| l.contains("fleet:") && !l.contains("fleet timing"))
              .collect();
            eprintln!("kind: {pod} fleet log ({} lines):", fleet.len());
            for line in fleet.iter().rev().take(20).rev() {
              eprintln!("kind:     {line}");
            }
          }
        }
        // Pin the DNS/endpoint plumbing: does the Service list the recreated pod as an endpoint at all?
        // A published endpoint the daemon's resolver still times out on points at the resolver or a
        // CoreDNS-load drop; a missing endpoint points at the pod's readiness/DNS record.
        if let Ok(ep) = self.kubectl(&["get", "endpoints", "-o", "wide"]) {
          eprintln!("kind: endpoints:");
          for line in ep.stdout.lines() {
            eprintln!("kind:     {line}");
          }
        }
      }
    }
    Ok(())
  }

  /// Polls `volume placed` on the owner until the snapshot is placed at f + 1 or the wait passes.
  fn wait_placed(&self, owner: &str, id: &str, snapshot: &str) -> Result<Duration, Failure> {
    let started = Instant::now();
    loop {
      let placed = self.verb(owner, &["volume", "placed", id, "--snapshot", snapshot])?;
      let answer: serde_json::Value = serde_json::from_str(&placed.stdout).unwrap_or_default();
      if answer.get("placed").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(started.elapsed());
      }
      if started.elapsed() > PLACE_WAIT {
        return Err(Failure(format!(
          "kind: the snapshot did not place within {PLACE_WAIT:?}: {} {}",
          placed.stdout.trim(),
          placed.stderr.trim()
        )));
      }
      pause(POLL_CLUSTER);
    }
  }

  /// Polls the survivors' `volume stat` until one serves the volume placed; returns it and the time since
  /// `since`.
  fn wait_successor(
    &self,
    survivors: &[String],
    id: &str,
    since: Instant,
  ) -> Result<(String, Duration), Failure> {
    loop {
      for survivor in survivors {
        let stat = self.verb(survivor, &["volume", "stat", id])?;
        // The successor **serves** the volume when `volume stat` answers exit 0 (a holder that has not
        // taken it over refuses `NotFound`, exit 1). `volume stat`'s `placed` is an object
        // (`{"region": true, ...}`) or null — the region-placement of the re-served head — not a bare
        // bool, so it is read for the record, not as the serve gate (the gate is that the volume exists
        // on this node at all).
        if stat.code == 0 {
          let answer: serde_json::Value = serde_json::from_str(&stat.stdout).unwrap_or_default();
          let region_placed = answer
            .get("placed")
            .and_then(|placed| placed.get("region"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
          eprintln!("kind: {survivor} serves the volume (region-placed: {region_placed})");
          return Ok((survivor.clone(), since.elapsed()));
        }
      }
      if since.elapsed() > TAKEOVER_WAIT {
        return Err(Failure(format!(
          "kind: no survivor served the volume within {TAKEOVER_WAIT:?} of the delete"
        )));
      }
      pause(POLL_CLUSTER);
    }
  }

  /// `scale`: the manifest derives `f` and the node list at five replicas (f = 2), then three
  /// (f = 1). Each size is a fresh install with explicit bootstrap. This proves formation and
  /// the manifest's derivation at each size; it does not prove in-place rolling scale.
  fn scale(&self) -> Result<(), Failure> {
    let elapsed = self.fresh_install(SCALED_REPLICAS, None)?;
    eprintln!(
      "kind: {SCALED_REPLICAS} replicas: installed and formed in {:.1} s",
      elapsed.as_secs_f64()
    );
    let views = self.wait_formed(SCALED_REPLICAS, "the fleet formed at five")?;
    Self::assert_f(&views, (SCALED_REPLICAS - 1) / 2)?;
    let elapsed = self.fresh_install(LANE_REPLICAS, None)?;
    eprintln!(
      "kind: {LANE_REPLICAS} replicas: installed and formed in {:.1} s",
      elapsed.as_secs_f64()
    );
    let views = self.wait_formed(LANE_REPLICAS, "the fleet formed at three")?;
    Self::assert_f(&views, (LANE_REPLICAS - 1) / 2)
  }

  fn assert_f(views: &[View], f: u64) -> Result<(), Failure> {
    match views.iter().find(|view| view.f != f) {
      None => Ok(()),
      Some(view) => Err(Failure(format!(
        "kind: every node derives f = {f} from the manifest; {}",
        view.line()
      ))),
    }
  }

  /// `netem`: each WAN profile installed, the fleet re-formed under it, then watched over the window —
  /// the leader must not change and every pod's reported base must equal the derivation on its measured
  /// tail. The handshake-ceiling profile reports whether formation completed at all, and how.
  fn netem_profiles(&self) -> Result<(), Failure> {
    let mut findings = Vec::new();
    for (name, delay, jitter, loss) in NETEM_PROFILES {
      let profile = (delay.to_owned(), jitter.to_owned(), loss.to_owned());
      // A fresh install so the whole fleet boots together under the shaping (a rolling upgrade onto a
      // running fleet would hit the rejoin gap; a fresh simultaneous boot forms cleanly).
      let elapsed = self.fresh_install(LANE_REPLICAS, Some(&profile))?;
      eprintln!(
        "kind: profile {name} (delay {delay} {jitter} loss {loss}) installed and rolled out in {:.1} s",
        elapsed.as_secs_f64()
      );
      match self.wait_formed(LANE_REPLICAS, &format!("the fleet formed under {name}")) {
        Ok(_) => findings.push(self.watch(name)?),
        Err(e) if name == "ceiling" => {
          eprintln!("kind: {e}");
          findings.push(format!(
            "{name}: the fleet did NOT form within {FORMATION_WAIT:?} — {e}"
          ));
        }
        Err(e) => return Err(e),
      }
    }
    // Back to the unshaped fleet, so the cluster is left as the chart installs it.
    self.fresh_install(LANE_REPLICAS, None)?;
    eprintln!("kind: netem findings:");
    for finding in findings {
      eprintln!("kind:   {finding}");
    }
    Ok(())
  }

  /// Watches the formed fleet over the window: the leader at every sample, and each pod's timing at the end.
  fn watch(&self, name: &str) -> Result<String, Failure> {
    let pods: Vec<String> = (0..LANE_REPLICAS).map(|index| self.pod(index)).collect();
    let started = Instant::now();
    let mut leaders: Vec<u64> = Vec::new();
    let mut last: Vec<View> = Vec::new();
    while started.elapsed() < NETEM_WINDOW {
      let views: Vec<View> = self.views(&pods)?.into_iter().flatten().collect();
      let leader = views
        .iter()
        .filter(|view| view.leads)
        .map(|view| view.host)
        .collect::<Vec<u64>>();
      leaders.push(leader.first().copied().unwrap_or(0));
      if leader.len() != 1 {
        eprintln!(
          "kind: [{name} +{:.0} s] {} leaders reported",
          started.elapsed().as_secs_f64(),
          leader.len()
        );
      }
      last = views;
      pause(NETEM_SAMPLE);
    }
    let changes = leaders.windows(2).filter(|pair| pair[0] != pair[1]).count();
    let mut lines = Vec::new();
    for view in &last {
      let expected = expected_base_periods(view.rtt_tail_ns);
      lines.push(format!(
        "{}: tail {} ms spread {} ms → base {} (expected ⌈10 × tail / 100 ms⌉ = {}) span {} samples {} leads {}",
        view.pod,
        milliseconds(view.rtt_tail_ns),
        milliseconds(view.rtt_spread_ns),
        view.base_periods,
        expected,
        view.span_periods,
        view.samples,
        view.leads
      ));
      if view.base_periods != expected {
        return Err(Failure(format!(
          "kind: {name}: {} reports base {} for tail {} ns; the derivation gives {expected}",
          view.pod, view.base_periods, view.rtt_tail_ns
        )));
      }
    }
    let finding = format!(
      "{name}: {} samples over {:.0} s, leader changes {changes}, leader {} ;\n    {}",
      leaders.len(),
      started.elapsed().as_secs_f64(),
      leaders.last().copied().unwrap_or(0),
      lines.join("\n    ")
    );
    eprintln!("kind: {finding}");
    if changes != 0 {
      return Err(Failure(format!(
        "kind: {name}: the leader changed {changes} time(s) over the window: {leaders:?}"
      )));
    }
    Ok(finding)
  }
}
