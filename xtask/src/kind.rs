//! `cargo xtask kind` — the KIND fleet lane (docs/wip/kind-lane.md; §4.8 "Deployment"; the Helm chart of
//! docs/deploy.md): the fleet proven on real Linux pods over a real network, with the chart an operator
//! installs. Each subcommand is one recorded step, so a run's numbers are a run's commands:
//!
//! - `image [--tag TAG]` — builds the image from the repository's Dockerfile (release profile, distroless).
//! - `smoke [--tag TAG]` — runs one node of that image in plain Docker from a one-node manifest and reads
//!   `slates status` and a volume verb through `docker exec`: the image's proof, before any cluster.
//!
//! Every artifact the lane writes — self-signed identities, manifests, values files, logs — goes to a
//! scratch directory outside the tree (`$HOME/.cache/slates-kind-lane/<pid>`), named with the process id
//! and removed at the end unless `--keep` is given. The certificates are self-signed here with `rcgen`, as
//! the workspace's fleet tests mint theirs: the lane's stand-in for the operator-provisioned material a
//! production install supplies (docs/deploy.md says which). Nothing here is shipped code: this tool runs
//! `docker`, `kind`, `kubectl` and `helm` and reads and writes its own scratch directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::Failure;

/// Shape: the image tag the lane builds and loads when none is given.
const DEFAULT_TAG: &str = "slates:lane";
/// Shape: the fleet's TLS name — every minted certificate carries it, every node verifies its peers under it.
const FLEET_NAME: &str = "slates-fleet";
/// Shape: the base UDP port of the first node; each node's block is the base and the next
/// (`slates_server::deploy`), so node N's base is this plus `2 × N`.
const BASE_PORT: u16 = 7000;
/// Shape: the ports a node serves on — one per plane (`slates_server::deploy::PORTS_PER_NODE`).
const PORTS_PER_NODE: u16 = 2;
/// Shape: how long the smoke run waits for the container's daemon to answer `status` — the anchor measures
/// a full machine profile first (seconds), then the daemon boots.
const SMOKE_START_WAIT: Duration = Duration::from_secs(90);
/// Shape: the poll cadence of a bounded wait — the daemon's heartbeat order, so a bound is met within a beat.
const POLL: Duration = Duration::from_millis(250);
/// Shape: the memory the smoke container is given (`--memory`), a Guaranteed-QoS-like bound the daemon's
/// §4.2 effective capacity clamps to; a stated operator value, as the chart's is.
const SMOKE_MEMORY: &str = "1g";
/// Shape: the CPUs the smoke container is given (`--cpus`); the daemon derives its shard count from the
/// cgroup quota, so this is what `status` should report as `shards`.
const SMOKE_CPUS: &str = "2";

/// What the lane was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
  /// Build the image.
  Image,
  /// Run one node in Docker and read its status.
  Smoke,
}

/// The lane's options.
#[derive(Debug)]
pub(crate) struct Options {
  /// The step.
  pub(crate) step: Step,
  /// The image tag.
  pub(crate) tag: String,
  /// Keep the scratch directory at the end.
  pub(crate) keep: bool,
}

/// Parses `kind STEP [--tag TAG] [--keep]`.
pub(crate) fn parse(args: &[String]) -> Result<Options, Failure> {
  let step = match args.first().map(String::as_str) {
    Some("image") => Step::Image,
    Some("smoke") => Step::Smoke,
    other => {
      return Err(Failure(format!(
        "kind: unknown step {other:?}; steps: image, smoke"
      )));
    }
  };
  let mut tag = DEFAULT_TAG.to_owned();
  let mut keep = false;
  let mut rest = args[1..].iter();
  while let Some(arg) = rest.next() {
    match arg.as_str() {
      "--tag" => {
        tag = rest
          .next()
          .cloned()
          .ok_or_else(|| Failure("kind: --tag needs a value".to_owned()))?;
      }
      "--keep" => keep = true,
      other => return Err(Failure(format!("kind: unknown option `{other}`"))),
    }
  }
  Ok(Options { step, tag, keep })
}

/// Runs the step.
pub(crate) fn run(root: &Path, options: &Options) -> Result<(), Failure> {
  match options.step {
    Step::Image => image(root, &options.tag),
    Step::Smoke => {
      let scratch = Scratch::create(options.keep)?;
      smoke(&options.tag, &scratch)
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

/// Waits one poll interval between two questions to a container or a cluster (a blocking wait is the
/// shape of a command-line tool driving other programs; nothing here runs on the daemon's runtime).
fn pause() {
  #[allow(clippy::disallowed_methods)] // a development tool's poll cadence, not daemon code
  std::thread::sleep(POLL);
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

/// Mints a self-signed identity for `node` carrying the fleet's TLS name, written as DER files into `dir`
/// (`NODE.crt.der`, `NODE.key.der`) — what the daemon reads beside its manifest.
fn mint_identity(dir: &Path, node: &str) -> Result<(), Failure> {
  let key = rcgen::KeyPair::generate().map_err(|e| Failure(format!("kind: key pair: {e}")))?;
  let cert = rcgen::CertificateParams::new(vec![FLEET_NAME.to_owned()])
    .map_err(|e| Failure(format!("kind: certificate params: {e}")))?
    .self_signed(&key)
    .map_err(|e| Failure(format!("kind: self-signing: {e}")))?;
  write_scratch(&dir.join(format!("{node}.crt.der")), cert.der().as_ref())?;
  write_scratch(&dir.join(format!("{node}.key.der")), &key.serialize_der())?;
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
    pause();
  };
  let first_answer = started.elapsed();
  let status: serde_json::Value = serde_json::from_str(&status)?;
  let fleet = fleet_of(&status)?;
  let shards = status
    .get("shards")
    .and_then(serde_json::Value::as_array)
    .map(Vec::len)
    .unwrap_or(0);
  eprintln!(
    "kind: status answered after {:.2} s: shards {shards}; fleet f={} members={} peers_probed={} council_leads={} council_base_periods={}",
    first_answer.as_secs_f64(),
    fleet.get("f").unwrap_or(&serde_json::Value::Null),
    fleet
      .get("members")
      .and_then(serde_json::Value::as_array)
      .map(Vec::len)
      .unwrap_or(0),
    fleet
      .get("peers_probed")
      .unwrap_or(&serde_json::Value::Null),
    fleet
      .get("council")
      .and_then(|c| c.get("leads"))
      .unwrap_or(&serde_json::Value::Null),
    fleet
      .get("council")
      .and_then(|c| c.get("base_periods"))
      .unwrap_or(&serde_json::Value::Null),
  );
  if fleet.get("f").and_then(serde_json::Value::as_u64) != Some(0)
    || fleet
      .get("members")
      .and_then(serde_json::Value::as_array)
      .map(Vec::len)
      != Some(1)
  {
    return Err(Failure(format!(
      "kind: a one-node manifest is the solo degenerate (f 0, one member): {fleet}"
    )));
  }
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
