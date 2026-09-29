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
//!   SIGKILL takeover), the successor serving; the replacement pod's fresh-identity rejoin
//!   and a stable creation are required gates.
//! - `scale` — `replicas=5` and back to 3: the configuration group's membership change, re-forming each time.
//! - `netem` — the WAN profiles of §4.8's owed measurement: 80 ms ± 20 ms, the same with 1 % loss, and
//!   the handshake ceiling at 350 ms; each pod's council timing and the leader's stability over a window.
//! - `succession [--trials N]` — the council's leader lost under an asymmetric profile (pod 0's egress
//!   80 ms, pod 1's 20 ms, pod 2 unshaped): the settled leader's egress is cut from an ephemeral
//!   `NET_ADMIN` container, the survivors are polled until one leads, and the cut heals. The outranked
//!   pod must never succeed a central one (research record §3.4; thesis §4.2.3).
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
/// required admission gate (the new-IP proof in docs/wip/kind-lane.md):
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

/// Shape: the succession measurement's profile (docs/wip/kind-lane.md, "Piece 6"): pod 0's egress delayed
/// 80 ms and pod 1's 20 ms, each ± [`SUCCESSION_JITTER`], pod 2 unshaped. Pods 1 and 2 then commit in about
/// 20 ms and tie, and pod 0, every path of which is at least 80 ms, is outranked by both — the three-region
/// case of the research record §3.4 on real pods.
const SUCCESSION_DELAYS: [(u64, &str); 2] = [(0, "80ms"), (1, "20ms")];
/// Shape: the jitter on each shaped pod's egress.
const SUCCESSION_JITTER: &str = "5ms";
/// Shape: the pod every other pod outranks under the succession profile.
const SUCCESSION_OUTRANKED: u64 = 0;
/// Shape: the leader losses a succession run measures unless `--trials` says otherwise.
const SUCCESSION_TRIALS: u64 = 10;
/// Shape: how long a cut leader stays cut — several election timeouts at these paths' 1–1.3 s base, so the
/// survivors elect and settle before it returns.
const SUCCESSION_CUT: Duration = Duration::from_secs(15);
/// Shape: consecutive samples one leader must hold, every pod formed, before a cut: settled, past any
/// priority transfer.
const SETTLED_SAMPLES: u32 = 5;
/// Shape: how long a settled leader is waited for before a cut.
const SETTLE_WAIT: Duration = Duration::from_secs(180);
/// Shape: how long a successor is waited for after a cut. (The healed leader's rejoin is waited for up to
/// [`REJOIN_WAIT`], the lane's bound for a replaced pod's.)
const SUCCESSION_WAIT: Duration = Duration::from_secs(60);
/// Format: the ephemeral cut container's security context (`kubectl debug --custom`): root with
/// `NET_ADMIN`, as the chart's netem init container runs, over the pod's non-root default.
const CUT_CONTAINER: &str = r#"{"securityContext":{"runAsNonRoot":false,"runAsUser":0,"capabilities":{"add":["NET_ADMIN"]}}}"#;

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
  Succession,
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
  /// `succession`: how many leader losses.
  pub(crate) trials: u64,
}

/// A netem profile for an install: every shaped pod's delay, jitter and loss, and per-pod delay overrides
/// (the chart's `netem.delays`).
#[derive(Debug, Clone)]
struct Shaping {
  delay: String,
  jitter: String,
  loss: String,
  delays: Vec<(u64, String)>,
}

impl Shaping {
  /// One delay, jitter and loss for every shaped pod.
  fn uniform(delay: &str, jitter: &str, loss: &str) -> Shaping {
    Shaping {
      delay: delay.to_owned(),
      jitter: jitter.to_owned(),
      loss: loss.to_owned(),
      delays: Vec::new(),
    }
  }

  /// The succession profile ([`SUCCESSION_DELAYS`]).
  fn succession() -> Shaping {
    Shaping {
      delays: SUCCESSION_DELAYS
        .iter()
        .map(|(ordinal, delay)| (*ordinal, (*delay).to_owned()))
        .collect(),
      ..Shaping::uniform("0ms", SUCCESSION_JITTER, "0%")
    }
  }

  /// The `tc` command that restores pod `ordinal`'s shaping after a cut: its delay, or no qdisc at all.
  fn restore(&self, ordinal: u64) -> String {
    match self.delays.iter().find(|(shaped, _)| *shaped == ordinal) {
      Some((_, delay)) => format!(
        "tc qdisc replace dev eth0 root netem delay {delay} {} loss {}",
        self.jitter, self.loss
      ),
      None => "tc qdisc del dev eth0 root".to_owned(),
    }
  }
}

/// Parses `kind STEP [--tag TAG] [--cluster NAME] [--keep] [--replicas N] [--out FILE]
/// [--identities FILE] [--netem DELAY JITTER LOSS] [--trials N]`.
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
    Some("succession") => Step::Succession,
    Some("down") => Step::Down,
    Some("all") => Step::All,
    other => {
      return Err(Failure(format!(
        "kind: unknown step {other:?}; steps: image, smoke, certs, up, install, prove, scale, netem, succession, down, all"
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
    trials: SUCCESSION_TRIALS,
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
      "--trials" => {
        options.trials = value("--trials")?
          .parse()
          .map_err(|_| Failure("kind: --trials needs a number".to_owned()))?;
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
  /// The council's election state (zero from an image that predates it): term, own priority and its spread,
  /// rank, lease, and the pre-elections and elections begun.
  term: u64,
  priority_ns: u64,
  priority_spread_ns: u64,
  rank: u64,
  leader_lease: bool,
  pre_elections: u64,
  elections: u64,
  /// The replies its pre-elections drew, granted and refused, and the pre-votes it refused as a voter by
  /// reason: lease, term, log, role.
  pre_votes_granted: u64,
  pre_votes_refused: u64,
  refused_leased: u64,
  refused_term: u64,
  refused_log: u64,
  refused_role: u64,
  /// The `fleet.resolve` refusals summed over the shards: a peer's name that did not resolve at a dial.
  resolve_refused: u64,
  /// How its record links fared (zero from an image that predates a counter).
  links: LinkCounters,
}

/// A view's record-link counters, each summed over the shards under the name the daemon counts it by
/// (`crates/server/src/fleet.rs`): the link task's discovery pages that ended without a reply — at their
/// deadline, invalidated, or on a transport fault, each of which releases the link's session to be dialled
/// again — the re-dials and dial faults, and the borrowed sessions whose return found their slot taken. A
/// succession trial reports how each moved across a leader's loss, the window in which a survivor's link to
/// the other survivor was seen down (GAPS, 2026-09-29).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LinkCounters {
  discovery_deadline: u64,
  discovery_invalidated: u64,
  discovery_transport: u64,
  redials: u64,
  dial_faults: u64,
  stale_returns: u64,
}

impl LinkCounters {
  fn parse(status: &serde_json::Value) -> LinkCounters {
    let named = |name: &str| refusals_where(status, |kind| kind == name);
    LinkCounters {
      discovery_deadline: named("fleet.discovery.deadline"),
      discovery_invalidated: named("fleet.discovery.invalidated"),
      discovery_transport: named("fleet.discovery.transport"),
      redials: named("fleet.dial.redial"),
      dial_faults: named("fleet.dial.fault"),
      stale_returns: named("fleet.link.stale_return"),
    }
  }

  /// How far each counter moved from `before` to `self`.
  fn since(self, before: LinkCounters) -> LinkCounters {
    LinkCounters {
      discovery_deadline: self
        .discovery_deadline
        .saturating_sub(before.discovery_deadline),
      discovery_invalidated: self
        .discovery_invalidated
        .saturating_sub(before.discovery_invalidated),
      discovery_transport: self
        .discovery_transport
        .saturating_sub(before.discovery_transport),
      redials: self.redials.saturating_sub(before.redials),
      dial_faults: self.dial_faults.saturating_sub(before.dial_faults),
      stale_returns: self.stale_returns.saturating_sub(before.stale_returns),
    }
  }

  fn line(self) -> String {
    format!(
      "discovery ended {} at its deadline / {} invalidated / {} on a transport fault, {} re-dials, {} dial faults, {} stale returns",
      self.discovery_deadline,
      self.discovery_invalidated,
      self.discovery_transport,
      self.redials,
      self.dial_faults,
      self.stale_returns
    )
  }
}

/// The count of every refusal whose kind `wanted` accepts, summed over the status document's shards.
fn refusals_where(status: &serde_json::Value, wanted: impl Fn(&str) -> bool) -> u64 {
  status
    .get("shards")
    .and_then(serde_json::Value::as_array)
    .into_iter()
    .flatten()
    .flat_map(|shard| {
      shard
        .get("refusals")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    })
    .filter(|refusal| {
      refusal
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .is_some_and(&wanted)
    })
    .map(|refusal| u64_of(refusal, "count"))
    .fold(0, u64::saturating_add)
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
    let resolve_refused = refusals_where(status, is_resolve_refusal);
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
      term: u64_of(council, "term"),
      priority_ns: u64_of(council, "priority_ns"),
      priority_spread_ns: u64_of(council, "priority_spread_ns"),
      rank: u64_of(council, "rank"),
      leader_lease: council
        .get("leader_lease")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false),
      pre_elections: u64_of(council, "pre_elections"),
      elections: u64_of(council, "elections"),
      pre_votes_granted: u64_of(council, "pre_votes_granted"),
      pre_votes_refused: u64_of(council, "pre_votes_refused"),
      refused_leased: u64_of(council, "refused_leased"),
      refused_term: u64_of(council, "refused_term"),
      refused_log: u64_of(council, "refused_log"),
      refused_role: u64_of(council, "refused_role"),
      resolve_refused,
      links: LinkCounters::parse(status),
    })
  }

  /// One line of the view, for the record.
  fn line(&self) -> String {
    format!(
      "{}: host={} f={} members={:?} peers_probed={} leads={} base={} span={} tail_ns={} spread_ns={} samples={} term={} priority_ns={}±{} rank={} lease={} pre_elections={} elections={} resolve_refused={}",
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
      self.term,
      self.priority_ns,
      self.priority_spread_ns,
      self.rank,
      self.leader_lease,
      self.pre_elections,
      self.elections,
      self.resolve_refused
    )
  }
}

/// A status document's fleet block (members, probe and session counters, council and root timing) and
/// every refusal its shards counted, summed by kind — the counters a failed fleet wait is read by.
fn status_counters(status: &serde_json::Value) -> String {
  let mut refusals = std::collections::BTreeMap::<String, u64>::new();
  for shard in status
    .get("shards")
    .and_then(serde_json::Value::as_array)
    .into_iter()
    .flatten()
  {
    for refusal in shard
      .get("refusals")
      .and_then(serde_json::Value::as_array)
      .into_iter()
      .flatten()
    {
      if let Some(kind) = refusal.get("kind").and_then(serde_json::Value::as_str) {
        let total = refusals.entry(kind.to_owned()).or_insert(0);
        *total = total.saturating_add(u64_of(refusal, "count"));
      }
    }
  }
  format!(
    "fleet={} refusals={refusals:?}",
    status.get("fleet").cloned().unwrap_or_default()
  )
}

/// Whether a counted refusal is a name lookup that failed at a dial: the daemon counts each under its kind,
/// `fleet.resolve.<kind>` (`timeout`, `refused`, `no-address`, `malformed`, `io`, `no-resolver`), and an
/// unnamed one as `fleet.resolve`. Until 2026-09-22 the view matched only the unnamed one, so a formation
/// report's `resolve_refused=0` said nothing about whether the peers' names resolved.
fn is_resolve_refusal(kind: &str) -> bool {
  kind == "fleet.resolve" || kind.starts_with("fleet.resolve.")
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
  netem: Option<Shaping>,
  trials: u64,
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
      netem: options
        .netem
        .as_ref()
        .map(|(delay, jitter, loss)| Shaping::uniform(delay, jitter, loss)),
      trials: options.trials,
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
      Step::Succession => self.succession(self.trials),
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
  fn install(&self, replicas: u64, netem: Option<&Shaping>) -> Result<Duration, Failure> {
    let started = Instant::now();
    let chart = self.root.join("deploy/helm/slates");
    let lane_values = self.root.join("deploy/kind/values-lane.yaml");
    let netem_values = self.root.join("deploy/kind/values-netem.yaml");
    let context = format!("kind-{}", self.cluster);
    let replicas_set = format!("replicas={replicas}");
    // The image the lane built and loaded (`--tag`), not the lane values' default: a custom tag would
    // otherwise be loaded and never run, and an older `slates:lane` run in its place.
    let (repository, tag) = self.tag.rsplit_once(':').unwrap_or((&self.tag, "latest"));
    let repository_set = format!("image.repository={repository}");
    let tag_set = format!("image.tag={tag}");
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
      "--set",
      &repository_set,
      "--set",
      &tag_set,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if let Some(shaping) = netem {
      args.extend([
        "-f".to_owned(),
        netem_values.to_string_lossy().into_owned(),
        "--set".to_owned(),
        format!("netem.delay={}", shaping.delay),
        "--set".to_owned(),
        format!("netem.jitter={}", shaping.jitter),
        "--set".to_owned(),
        format!("netem.loss={}", shaping.loss),
      ]);
      for (ordinal, delay) in &shaping.delays {
        args.extend([
          "--set".to_owned(),
          format!("netem.delays.{ordinal}={delay}"),
        ]);
      }
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
  fn fresh_install(&self, replicas: u64, netem: Option<&Shaping>) -> Result<Duration, Failure> {
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

  /// What a failed fleet wait leaves for diagnosis once the cluster is deleted: where every pod runs, and
  /// for each pod its fleet and session counters, every refusal it counted (the dial, resolve, accept and
  /// probe counters), the council's committed voters (`recovery-plan region`) and its last log lines — the
  /// first occurrence of each dial or resolve failure is logged there by name. Before 2026-09-22 a failed
  /// formation printed one summary line per pod and a failed takeover its volume refusal, and neither
  /// could tell a pod-network or name failure from a council that never retired the dead owner
  /// (docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md). Best effort: a question a pod
  /// cannot answer is reported as such, never allowed to hide the failure being reported.
  fn fleet_diagnostics(&self, pods: &[String]) -> String {
    /// Shape: the log lines kept per pod — the boot lines, the derived values and the first-occurrence
    /// refusal lines a daemon writes.
    const LOG_TAIL: &str = "80";
    let answer = |outcome: Result<Outcome, Failure>| match outcome {
      Ok(outcome) if outcome.code == 0 => outcome.stdout.trim().to_owned(),
      Ok(outcome) => format!("(exit {}: {})", outcome.code, outcome.stderr.trim()),
      Err(failure) => format!("({})", failure.0),
    };
    let mut out = format!(
      "\n--- pods:\n{}",
      answer(self.kubectl(&["get", "pods", "-o", "wide"]))
    );
    for pod in pods {
      let status = answer(self.verb(pod, &["status"]));
      let counters = serde_json::from_str::<serde_json::Value>(&status)
        .map(|status| status_counters(&status))
        .unwrap_or(status);
      out.push_str(&format!(
        "\n--- {pod} status: {counters}\n--- {pod} council: {}\n--- {pod} log (last {LOG_TAIL} lines):\n{}",
        answer(self.verb(pod, &["recovery-plan", "region"])),
        answer(self.kubectl(&["logs", pod, "--tail", LOG_TAIL]))
      ));
    }
    out
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
          "kind: {what} did not happen within {bound:?}:\n{}{}",
          lines.join("\n"),
          self.fleet_diagnostics(pods)
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
    // The root starts at pod 0 and reconciles to the lowest member id. Keep both alive
    // so this history measures replacement through a surviving root quorum.
    let minimum = views.iter().map(|view| view.host).min();
    let owner = views
      .iter()
      .find(|view| view.pod != self.pod(0) && Some(view.host) != minimum)
      .map(|view| view.pod.clone())
      .ok_or_else(|| {
        Failure("kind: no owner can fail while preserving the root representative".to_owned())
      })?;
    let previous_ip = self.pod_ip(&owner)?;
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
    let survivors: Vec<String> = (0..n)
      .map(|index| self.pod(index))
      .filter(|pod| pod != &owner)
      .collect();
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
          view
            .as_ref()
            .is_some_and(|view| !view.members.contains(&owner_host))
        })
      },
    )?;
    eprintln!(
      "kind: both survivors retired the dead owner {owner_host} {:.1} s after the delete",
      retired_in.as_secs_f64()
    );
    let every_pod: Vec<String> = (0..n).map(|index| self.pod(index)).collect();
    let (successor, served_in) = self.wait_successor(&survivors, &every_pod, &id, killed_at)?;
    eprintln!(
      "kind: {successor} took the volume over and serves it placed {:.1} s after the delete",
      served_in.as_secs_f64()
    );

    // The whole anchor is gone. The replacement must join with a fresh voter identity at its
    // current DNS address; neither startup nor this history repeats bootstrap.
    let all: Vec<String> = (0..n).map(|index| self.pod(index)).collect();
    let (views, rejoined_in) =
      self.wait_views(&all, REJOIN_WAIT, "the replacement pod rejoined", |views| {
        Self::formed_at(n, views)
          && views
            .iter()
            .flatten()
            .all(|view| !view.members.contains(&owner_host))
      })?;
    eprintln!(
      "kind: replacement {owner} rejoined with a fresh identity {:.1} s after deletion",
      rejoined_in.as_secs_f64() + served_in.as_secs_f64()
    );
    for view in views.iter().flatten() {
      eprintln!("kind:   {}", view.line());
    }
    let replacement_ip = self.pod_ip(&owner)?;
    if replacement_ip == previous_ip {
      return Err(Failure(format!(
        "kind: replacement reused {previous_ip}; the new-IP rejoin was not exercised"
      )));
    }
    eprintln!("kind: replacement IP changed from {previous_ip} to {replacement_ip}");
    // Admission must permit a new stable effect on the replacement, not just SWIM probes.
    let result = self.verb(
      &owner,
      &["volume", "create", "rejoined", "--bounded", "8MiB"],
    )?;
    if result.code != 0 {
      return Err(Failure(format!(
        "kind: replacement cannot create after joining: {}",
        result.stderr
      )));
    }
    Ok(())
  }

  fn pod_ip(&self, pod: &str) -> Result<String, Failure> {
    let result = self.kubectl(&["get", "pod", pod, "-o", "jsonpath={.status.podIP}"])?;
    let address = result.stdout.trim();
    if result.code != 0 || address.is_empty() {
      return Err(Failure(format!(
        "kind: pod {pod} has no readable IP: {}",
        result.stderr
      )));
    }
    Ok(address.to_owned())
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
  /// `since`. On the bound, `diagnosed` (every pod, the replacement included) is reported with
  /// [`Lane::fleet_diagnostics`].
  fn wait_successor(
    &self,
    survivors: &[String],
    diagnosed: &[String],
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
        // Name why no survivor served, rather than only that none did: each survivor's `volume stat`
        // refusal (a successor that never took over answers `NotFound`), then every pod's counted
        // refusals (`fleet.materialize` if `materialize_taken_over` refused — a budget or a malformed
        // archive) and the council's committed voters — a takeover is assigned only once the council
        // retires the dead owner, so a voter set still holding it is the cause to read first
        // (docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md). Until 2026-09-22 this
        // asked `status <id>` of the daemon, which answers `NotFound` for the volume, not its counters.
        let mut why = String::new();
        for survivor in survivors {
          let stat = self.verb(survivor, &["volume", "stat", id]);
          why.push_str(&format!(
            "\n  {survivor}: volume stat -> {}",
            stat.map_or_else(|e| e.0, |o| format!("exit {}: {}", o.code, o.stderr.trim())),
          ));
        }
        return Err(Failure(format!(
          "kind: no survivor served the volume within {TAKEOVER_WAIT:?} of the delete:{why}{}",
          self.fleet_diagnostics(diagnosed)
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
      let profile = Shaping::uniform(delay, jitter, loss);
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

/// One leader loss the succession step measured: which pod led and which succeeded it, how long after the cut,
/// and the survivors' election state before and at the succession.
struct Succession {
  trial: u64,
  lost: String,
  successor: String,
  took: Duration,
  /// From the heal until every pod held all three members again, one of them leading: the cut leader,
  /// believing its peers dead as they believe it, found again
  /// (`docs/bugs/2026-09-29-a-symmetric-partition-never-healed.md`).
  rejoined: Duration,
  before: Vec<View>,
  after: Vec<View>,
}

impl Succession {
  /// The record's line: who was lost and who succeeded, when, and each survivor's rank, priority and the
  /// campaigns it began in between.
  fn line(&self) -> String {
    let survivors: Vec<String> = self
      .after
      .iter()
      .map(|after| {
        let before = self.before.iter().find(|view| view.pod == after.pod);
        let began = |field: fn(&View) -> u64| field(after).saturating_sub(before.map_or(0, field));
        let links = after
          .links
          .since(before.map_or_else(LinkCounters::default, |view| view.links));
        format!(
          "{} rank {} priority {} ± {} ms, {} pre-elections ({} granted, {} refused replies) and {} elections begun, refused {} leased / {} term / {} log / {} role; links: {}{}",
          after.pod,
          after.rank,
          milliseconds(after.priority_ns),
          milliseconds(after.priority_spread_ns),
          began(|view| view.pre_elections),
          began(|view| view.pre_votes_granted),
          began(|view| view.pre_votes_refused),
          began(|view| view.elections),
          began(|view| view.refused_leased),
          began(|view| view.refused_term),
          began(|view| view.refused_log),
          began(|view| view.refused_role),
          links.line(),
          if after.leads { ", leads" } else { "" }
        )
      })
      .collect();
    format!(
      "trial {}: {} lost; {} leads {:.2} s after the cut; the fleet whole again {:.2} s after the heal; {}",
      self.trial,
      self.lost,
      self.successor,
      self.took.as_secs_f64(),
      self.rejoined.as_secs_f64(),
      survivors.join("; ")
    )
  }
}

/// A pod's ordinal: the number after its name's last `-`.
fn ordinal_of(pod: &str) -> Option<u64> {
  pod
    .rsplit_once('-')
    .and_then(|(_, ordinal)| ordinal.parse().ok())
}

impl Lane {
  /// `succession`: the council's leader lost under [`Shaping::succession`], `trials` times (the module doc;
  /// research record §3.4; thesis §4.2.3). Each trial waits for a settled leader, cuts its egress, polls the
  /// survivors until one of them leads, and heals the cut. The outranked pod must never succeed a central
  /// leader.
  fn succession(&self, trials: u64) -> Result<(), Failure> {
    let shaping = Shaping::succession();
    let custom = self._scratch.path.join("cut.json");
    write_scratch(&custom, CUT_CONTAINER.as_bytes())?;
    let mut measured = Vec::new();
    for trial in 0..trials {
      // Each trial on a fresh fleet, so no trial inherits another's cut.
      let elapsed = self.fresh_install(LANE_REPLICAS, Some(&shaping))?;
      eprintln!(
        "kind: trial {trial}: succession profile {SUCCESSION_DELAYS:?} ± {SUCCESSION_JITTER} installed in {:.1} s",
        elapsed.as_secs_f64()
      );
      self.wait_formed(
        LANE_REPLICAS,
        "the fleet formed under the succession profile",
      )?;
      let before = self.settled_views()?;
      let succession = self.lose_leader(trial, before, &custom, &shaping)?;
      eprintln!("kind: {}", succession.line());
      measured.push(succession);
    }
    self.report_successions(&measured)
  }

  /// Every pod's view once one central leader — any pod but [`SUCCESSION_OUTRANKED`], where priority moves
  /// leadership — has held for [`SETTLED_SAMPLES`] consecutive samples with the fleet formed, bounded by
  /// [`SETTLE_WAIT`]; a leader that priority never moves off the outranked pod fails the wait.
  fn settled_views(&self) -> Result<Vec<View>, Failure> {
    let pods: Vec<String> = (0..LANE_REPLICAS).map(|index| self.pod(index)).collect();
    let started = Instant::now();
    let mut held = (0_u64, 0_u32);
    loop {
      let views = self.views(&pods)?;
      let leader = views
        .iter()
        .flatten()
        .find(|view| view.leads && view.pod != self.pod(SUCCESSION_OUTRANKED))
        .map(|view| view.host);
      held = match leader {
        Some(host) if Self::formed_at(LANE_REPLICAS, &views) && host == held.0 => {
          (host, held.1.saturating_add(1))
        }
        Some(host) if Self::formed_at(LANE_REPLICAS, &views) => (host, 1),
        _ => (0, 0),
      };
      if held.1 >= SETTLED_SAMPLES {
        return Ok(views.into_iter().flatten().collect());
      }
      if started.elapsed() > SETTLE_WAIT {
        return Err(Failure(format!(
          "kind: no leader settled within {SETTLE_WAIT:?}:{}",
          self.fleet_diagnostics(&pods)
        )));
      }
      pause(POLL_CLUSTER);
    }
  }

  /// The VM's clock against this host's: an instant here and the VM's `/proc/uptime` at it (the midpoint of
  /// the read), so a time a pod reads there converts to an instant here. Every kind node and pod shares the
  /// VM's kernel, so its uptime is theirs.
  fn vm_clock(&self) -> Result<(Instant, f64), Failure> {
    let node = format!("{}-control-plane", self.cluster);
    let before = Instant::now();
    let uptime = must("docker", &["exec", &node, "cat", "/proc/uptime"])?;
    let read = before.elapsed();
    let seconds = uptime_seconds(&uptime)?;
    Ok((before + read / 2, seconds))
  }

  /// Runs `script` as root with `NET_ADMIN` in pod `pod`'s network namespace, from an ephemeral container
  /// named `name` (`kubectl debug`; the pod's own containers hold no capability).
  fn in_pod_network(
    &self,
    pod: &str,
    name: &str,
    custom: &Path,
    script: &str,
  ) -> Result<(), Failure> {
    let custom = format!("--custom={}", custom.to_string_lossy());
    let container = format!("--container={name}");
    let image = format!("--image={NETEM_TAG}");
    let outcome = self.kubectl(&[
      "debug",
      pod,
      &image,
      "--image-pull-policy=Never",
      "--profile=netadmin",
      &custom,
      &container,
      "--attach=false",
      "--",
      "sh",
      "-c",
      script,
    ])?;
    if outcome.code != 0 {
      return Err(Failure(format!(
        "kind: {name} in {pod}: {}",
        outcome.stderr.trim()
      )));
    }
    Ok(())
  }

  /// One trial: the settled leader in `before` is cut off, the survivors are polled until one of them leads,
  /// the cut holds for [`SUCCESSION_CUT`], and it heals to `shaping`'s profile.
  fn lose_leader(
    &self,
    trial: u64,
    before: Vec<View>,
    custom: &Path,
    shaping: &Shaping,
  ) -> Result<Succession, Failure> {
    let lost = before
      .iter()
      .find(|view| view.leads)
      .map(|view| view.pod.clone())
      .ok_or_else(|| Failure("kind: a settled fleet reports no leader".to_owned()))?;
    let survivors: Vec<String> = before
      .iter()
      .map(|view| view.pod.clone())
      .filter(|pod| *pod != lost)
      .collect();
    let (anchor, anchor_uptime) = self.vm_clock()?;
    let cut = format!("cut-{trial}");
    self.in_pod_network(
      &lost,
      &cut,
      custom,
      "tc qdisc replace dev eth0 root netem loss 100% && cut -d' ' -f1 /proc/uptime",
    )?;
    let (after, found_at) = self.await_successor(&survivors)?;
    let logged = self.kubectl(&["logs", &lost, "-c", &cut])?;
    let offset = uptime_seconds(&logged.stdout)? - anchor_uptime;
    if !(0.0..=SUCCESSION_WAIT.as_secs_f64()).contains(&offset) {
      return Err(Failure(format!(
        "kind: the cut in {lost} logged {} against the VM's {anchor_uptime} before it",
        logged.stdout.trim()
      )));
    }
    let cut_at = anchor + Duration::from_secs_f64(offset);
    let successor = after
      .iter()
      .find(|view| view.leads)
      .map(|view| view.pod.clone())
      .unwrap_or_default();
    pause(SUCCESSION_CUT.saturating_sub(cut_at.elapsed()));
    let ordinal =
      ordinal_of(&lost).ok_or_else(|| Failure(format!("kind: {lost} names no ordinal")))?;
    self.in_pod_network(
      &lost,
      &format!("heal-{trial}"),
      custom,
      &shaping.restore(ordinal),
    )?;
    let pods: Vec<String> = (0..LANE_REPLICAS).map(|index| self.pod(index)).collect();
    let (_, rejoined) = self.wait_views(
      &pods,
      REJOIN_WAIT,
      "the healed leader rejoined its peers",
      |views| Self::formed_at(LANE_REPLICAS, views),
    )?;
    Ok(Succession {
      trial,
      lost,
      successor,
      took: found_at.saturating_duration_since(cut_at),
      rejoined,
      before,
      after,
    })
  }

  /// Polls `survivors` until one of them leads, bounded by [`SUCCESSION_WAIT`]; returns their views then and
  /// the instant it was seen.
  fn await_successor(&self, survivors: &[String]) -> Result<(Vec<View>, Instant), Failure> {
    let started = Instant::now();
    loop {
      let views: Vec<View> = self.views(survivors)?.into_iter().flatten().collect();
      if views.iter().any(|view| view.leads) {
        return Ok((views, Instant::now()));
      }
      if started.elapsed() > SUCCESSION_WAIT {
        return Err(Failure(format!(
          "kind: no survivor led within {SUCCESSION_WAIT:?} of the cut:{}",
          self.fleet_diagnostics(survivors)
        )));
      }
      pause(POLL);
    }
  }

  /// The run's record — who succeeded whom, and how long each took — and its gate: the outranked pod never
  /// succeeds a central leader.
  fn report_successions(&self, measured: &[Succession]) -> Result<(), Failure> {
    let outranked = self.pod(SUCCESSION_OUTRANKED);
    let mut took: Vec<f64> = measured.iter().map(|one| one.took.as_secs_f64()).collect();
    took.sort_by(f64::total_cmp);
    let median = took.get(took.len() / 2).copied().unwrap_or(0.0);
    let wrong: Vec<&Succession> = measured
      .iter()
      .filter(|one| one.lost != outranked && one.successor == outranked)
      .collect();
    eprintln!(
      "kind: succession: {} losses; successor within {median:.2} s median ({took:.2?} s); {} succeeded by the outranked {outranked}",
      measured.len(),
      wrong.len()
    );
    if wrong.is_empty() {
      return Ok(());
    }
    let lines: Vec<String> = wrong.iter().map(|one| one.line()).collect();
    Err(Failure(format!(
      "kind: the outranked {outranked} succeeded a central leader:\n{}",
      lines.join("\n")
    )))
  }
}

/// The first field of a `/proc/uptime` line: seconds since the VM booted.
fn uptime_seconds(line: &str) -> Result<f64, Failure> {
  line
    .split_whitespace()
    .next()
    .and_then(|seconds| seconds.parse().ok())
    .ok_or_else(|| Failure(format!("kind: not an uptime: {line:?}")))
}

#[cfg(test)]
mod tests {
  use super::{LinkCounters, View};

  /// A `slates status --json` document with the fleet block the lane reads and one shard's refusals.
  fn status_with_refusals(refusals: &[(&str, u64)]) -> serde_json::Value {
    let refusals: Vec<serde_json::Value> = refusals
      .iter()
      .map(|(kind, count)| serde_json::json!({ "kind": kind, "count": count }))
      .collect();
    serde_json::json!({
      "fleet": {
        "host": 7,
        "f": 1,
        "members": [7],
        "peers_probed": 0,
        "council": { "leads": true, "base_periods": 10, "span_periods": 10 },
      },
      "shards": [{ "refusals": refusals }],
    })
  }

  /// docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md (the lane's evidence): a pod whose
  /// dials failed to resolve a peer's name reports it. The daemon counts each lookup failure under its kind
  /// (`fleet.resolve.timeout`, `.refused`, `.no-address`, `.malformed`, `.io`, `.no-resolver`) and only an
  /// unnamed one under `fleet.resolve`; the view sums them all, and counts nothing else.
  #[test]
  fn a_views_resolve_refusals_sum_every_lookup_failure_kind() {
    let status = status_with_refusals(&[
      ("fleet.resolve.timeout", 3),
      ("fleet.resolve.refused", 2),
      ("fleet.resolve", 1),
      ("fleet.resolved_elsewhere", 100),
      ("fleet.dial.redial", 40),
    ]);
    let view = View::parse("slates-0", &status).expect("the document parses");
    assert_eq!(view.resolve_refused, 6);
  }

  /// GAPS 2026-09-29 (a survivor's record session drops after a leader's loss): a view reads each record-link
  /// counter by the name the daemon counts it under, summed over the shards, and a trial reports how far each
  /// moved.
  #[test]
  fn a_views_link_counters_are_read_by_name_and_moved_between_views() {
    let before = status_with_refusals(&[
      ("fleet.discovery.deadline", 1),
      ("fleet.dial.redial", 4),
      ("fleet.resolve", 9),
    ]);
    let after = status_with_refusals(&[
      ("fleet.discovery.deadline", 3),
      ("fleet.discovery.invalidated", 2),
      ("fleet.discovery.transport", 1),
      ("fleet.dial.redial", 7),
      ("fleet.dial.fault", 5),
      ("fleet.link.stale_return", 6),
      ("fleet.resolve", 9),
    ]);
    let before = View::parse("slates-0", &before).expect("the document parses");
    let after = View::parse("slates-0", &after).expect("the document parses");
    assert_eq!(
      after.links.since(before.links),
      LinkCounters {
        discovery_deadline: 2,
        discovery_invalidated: 2,
        discovery_transport: 1,
        redials: 3,
        dial_faults: 5,
        stale_returns: 6,
      }
    );
  }
}
