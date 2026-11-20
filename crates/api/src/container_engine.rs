//! The container engine's profile, judged (§4.6 A-9 "Capabilities differ by host, kernel, runtime and VMM and must be
//! reported"; Appendix C "OCI namespace handoff ... must report \[its\] own tested semantics"; AUD-29-67; A-520): what a
//! host that runs containers asks before it mounts a volume for one and binds it ([`crate::container`]). The daemon
//! verifies a bind's source and cannot know which runtime will bind it. Whether that runtime consumes the source as the
//! tests saw is a fact of the runtime's profile: which engine answers, where it resolves a bind's source, and whether
//! it remaps the ids a container writes with. So the host asks its runtime's engine, and slates' judge
//! (`slates_bridge_oci::runtime`) answers the profile a container workload has run through, or a typed refusal.
//!
//! **What is asked, in the order that consults least.** The runtime is admitted by its executable's name (the handshake
//! speaks the Docker engine's interface through its command line), before anything runs. Then the endpoint its command
//! line reaches (`DOCKER_HOST` when set, else its current context's: `docker context inspect`), refused unless a local
//! socket or pipe, since a bind's source resolves on the engine's host, before the engine is asked. Then the engine's
//! own statement (`docker info`: its kind, version and security options, and any errors it reports). Each query is
//! bounded in time by the daemon's observe budget and in size by one page of this machine, and its answer is parsed as
//! the hostile input it may be: anything not in the template's shape is refused typed, never guessed at.
//!
//! **What it answers.** An [`EngineProfile`]: the endpoint, the profile the engine's statement establishes, and the
//! evidence it holds ([`TestedProfile`]: the test that ran a container workload through it, how a container's ids reach
//! the volume, and how hard links behave through the engine's sharing). Otherwise an [`EngineFault`].
//!
//! **Where.** On Unix the queries run the runtime's command line and read its pipes, waiting on them with `poll(2)`. A
//! host with no Unix pipes to poll asks no engine and is refused [`EngineFault::Unsupported`], as a container bind is
//! there (no host mount is bound into a container on Windows, §4.6 A-9).
//!
//! Moved here from `crates/cli/src/oci_runtime.rs` on 2026-11-19 (A-520), so `slates oci-runtime` and an embedding
//! host (kiln's container engine) judge an engine through one implementation; the order of the questions is held by
//! tests over an injected query (`profile_of`), on every host.

use std::fmt;

pub use slates_bridge_oci::runtime::{
  Endpoint, Engine, ExportProfile, HardLinkRule, IdentityRule, MountsFrom, ProfileRefusal,
  RuntimeProfile, TestedProfile,
};

use crate::error::Note;

/// Format: the environment variable that names the Docker CLI's endpoint, overriding its context (Docker's CLI
/// reference, "Environment variables", tier B).
const ENDPOINT_VARIABLE: &str = "DOCKER_HOST";

/// Format: the template that asks the current context for its engine endpoint, as a JSON string.
const CONTEXT_ENDPOINT: [&str; 4] = [
  "context",
  "inspect",
  "--format",
  "{{json .Endpoints.docker.Host}}",
];

/// Format: the template that asks the engine for the four facts the judge reads, as one JSON object. Measured answer:
/// 116 bytes from Docker Desktop 29.3.1 (2026-10-01).
const ENGINE_FACTS: [&str; 3] = [
  "info",
  "--format",
  r#"{"os":{{json .OperatingSystem}},"version":{{json .ServerVersion}},"security":{{json .SecurityOptions}},"errors":{{json .ServerErrors}}}"#,
];

/// Format: the template that asks podman for the five facts the judge reads, as one JSON object (A-520): its
/// `info`'s Go fields (`libpod/define/info.go`: `Host.OS`, `Version.Version`, `Host.Security.Rootless`,
/// `Host.Security.SELinuxEnabled`, `Host.ServiceIsRemote`; podman-info(1)'s JSON names them `os`, `Version`,
/// `rootless`, `selinuxEnabled`, `serviceIsRemote`). Its whole `info` would pass the one page an answer may hold.
const PODMAN_FACTS: [&str; 3] = [
  "info",
  "--format",
  r#"{"os":{{json .Host.OS}},"version":{{json .Version.Version}},"rootless":{{json .Host.Security.Rootless}},"selinux":{{json .Host.Security.SELinuxEnabled}},"remote":{{json .Host.ServiceIsRemote}}}"#,
];

/// Format: the endpoint a profile names for a runtime that runs its containers on this host itself (podman whose
/// service is not remote), where no engine is reached at an endpoint.
const LOCAL: &str = "local";

/// A container engine's profile and the evidence it holds ([`engine_profile`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineProfile {
  /// The endpoint the runtime's command line reaches, as it named it.
  pub endpoint: String,
  /// The profile the engine's statement establishes.
  pub profile: RuntimeProfile,
  /// The evidence the profile holds: the test that ran a container workload through it, and its rules.
  pub tested: TestedProfile,
}

/// A container engine that mounts a volume's NFS export as a volume of its own, and from where ([`export_engine`]; A-520's
/// export route).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportEngine {
  /// The endpoint the runtime's command line reaches, as it named it.
  pub endpoint: String,
  /// The profile the engine's statement establishes.
  pub profile: RuntimeProfile,
  /// The export route's evidence for it, and where it mounts the export from.
  pub tested: ExportProfile,
}

/// Why no profile was established, or why the one established holds no evidence ([`engine_profile`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineFault {
  /// The judge refused the runtime or its endpoint before any engine's statement was asked (a runtime the handshake
  /// does not speak, an endpoint in no known scheme, an engine reached over a network).
  Refused(ProfileRefusal),
  /// The engine's statement established a profile that holds no evidence (a rootless or user-remapping engine, one
  /// that labels for SELinux, a profile no container workload has run through), with the profile, so a host can say
  /// which engine was refused.
  Untested {
    /// The endpoint the runtime's command line reaches, as it named it.
    endpoint: String,
    /// The profile the engine's statement established (boxed: it is most of the fault's size).
    profile: Box<RuntimeProfile>,
    /// The judge's refusal of it.
    refusal: ProfileRefusal,
  },
  /// The runtime did not run, did not answer within the bound, or exited saying nothing; or the engine reported
  /// errors: with what was said.
  Unreachable(Note),
  /// An answer larger than one page: no template answer is that large.
  TooLarge,
  /// An answer not in the template's shape.
  Unreadable(Note),
  /// This host asks no engine: no host mount is bound into a container on it.
  Unsupported {
    /// The platform, as Rust names it.
    platform: &'static str,
  },
}

impl fmt::Display for EngineFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Refused(refusal) | Self::Untested { refusal, .. } => write!(f, "{refusal}"),
      Self::Unreachable(why) => write!(f, "EngineUnreachable: {why}"),
      Self::TooLarge => f.write_str("EngineAnswerTooLarge: more than one page"),
      Self::Unreadable(why) => write!(f, "EngineAnswerUnreadable: {why}"),
      Self::Unsupported { platform } => write!(
        f,
        "EngineUnsupported: no container engine is asked on {platform}, where no host mount is bound into a container"
      ),
    }
  }
}

/// Asks `runtime`'s engine for its profile and judges it (see the module's notes): the profile and the evidence it
/// holds, or the typed fault. `runtime` is the command line's executable, by name or path (`docker`).
#[cfg(unix)]
pub fn engine_profile(runtime: &str) -> Result<EngineProfile, EngineFault> {
  let named = std::env::var(ENDPOINT_VARIABLE)
    .ok()
    .filter(|endpoint| !endpoint.is_empty());
  profile_of(
    runtime,
    named,
    slates_bridge_oci::runtime::Host::this(),
    &mut |args| ask(runtime, args),
  )
}

/// No engine is asked where no host mount is bound into a container (see the module's notes).
#[cfg(not(unix))]
pub fn engine_profile(_runtime: &str) -> Result<EngineProfile, EngineFault> {
  Err(EngineFault::Unsupported {
    platform: std::env::consts::OS,
  })
}

/// Asks `runtime`'s engine for its profile and judges whether it mounts a volume's NFS export as a volume of its own,
/// and from where (A-520's export route, `slates_bridge_oci::runtime::judge_export`): the same questions in the same
/// order as [`engine_profile`], on every host, Windows' Docker Desktop over its named pipe among them, since no host
/// mount is bound into the container. `runtime` is the command line's executable, by name or path (`docker`).
pub fn export_engine(runtime: &str) -> Result<ExportEngine, EngineFault> {
  let named = std::env::var(ENDPOINT_VARIABLE)
    .ok()
    .filter(|endpoint| !endpoint.is_empty());
  export_engine_of(
    runtime,
    named,
    slates_bridge_oci::runtime::Host::this(),
    &mut |args| ask(runtime, args),
  )
}

/// [`export_engine`] over `ask`, on `host`: the profile [`established`], judged for the export route.
fn export_engine_of(
  runtime: &str,
  named: Option<String>,
  host: slates_bridge_oci::runtime::Host,
  ask: &mut dyn FnMut(&[&str]) -> Answer,
) -> Result<ExportEngine, EngineFault> {
  let (endpoint, profile) = established(runtime, named, ask)?;
  match slates_bridge_oci::runtime::judge_export(&profile, host) {
    Ok(tested) => Ok(ExportEngine {
      endpoint,
      profile,
      tested,
    }),
    Err(refusal) => Err(EngineFault::Untested {
      endpoint,
      profile: Box::new(profile),
      refusal,
    }),
  }
}

/// What one query of the runtime's command line answers: its output, or the fault.
type Answer = Result<Vec<u8>, EngineFault>;

/// The profile `runtime`'s engine establishes on `host`, in the module's order: the runtime's interface by its name;
/// for the Docker CLI the endpoint, `named` when the environment names one, else its context's (asked), then the
/// engine's statement (asked); for podman its own statement alone (asked: it states whether its service is remote);
/// judged. `ask` runs one query of the runtime's command line and answers its output. Pure but for `ask`, so its order
/// (nothing asked of a runtime the handshake does not speak, no engine asked behind an endpoint that is not local) is
/// tested on every host.
#[cfg(any(unix, test))]
fn profile_of(
  runtime: &str,
  named: Option<String>,
  host: slates_bridge_oci::runtime::Host,
  ask: &mut dyn FnMut(&[&str]) -> Answer,
) -> Result<EngineProfile, EngineFault> {
  let (endpoint, profile) = established(runtime, named, ask)?;
  judged(endpoint, profile, host)
}

/// The endpoint and the profile `runtime`'s engine establishes, in the module's order (see [`profile_of`]), before any
/// judge: what [`engine_profile`] and [`export_engine`] judge each by its own rules.
fn established(
  runtime: &str,
  named: Option<String>,
  ask: &mut dyn FnMut(&[&str]) -> Answer,
) -> Result<(String, RuntimeProfile), EngineFault> {
  let interface =
    slates_bridge_oci::runtime::runtime_interface(runtime).map_err(EngineFault::Refused)?;
  if interface == slates_bridge_oci::runtime::Interface::Podman {
    let facts = parse_podman_facts(&ask(&PODMAN_FACTS)?)?;
    let profile =
      slates_bridge_oci::runtime::podman_profile(&facts).map_err(EngineFault::Refused)?;
    return Ok((LOCAL.to_owned(), profile));
  }
  let endpoint_text = match named {
    Some(endpoint) => endpoint,
    None => parse_endpoint(&ask(&CONTEXT_ENDPOINT)?)?,
  };
  let endpoint = slates_bridge_oci::runtime::admit_endpoint(runtime, &endpoint_text)
    .map_err(EngineFault::Refused)?;
  let facts = parse_engine_facts(&ask(&ENGINE_FACTS)?)?;
  Ok((
    endpoint_text,
    slates_bridge_oci::runtime::runtime_profile(endpoint, &facts),
  ))
}

/// `profile`, reached at `endpoint`, judged on `host`: the evidence it holds, or the profile with the judge's refusal.
#[cfg(any(unix, test))]
fn judged(
  endpoint: String,
  profile: RuntimeProfile,
  host: slates_bridge_oci::runtime::Host,
) -> Result<EngineProfile, EngineFault> {
  match slates_bridge_oci::runtime::judge(&profile, host) {
    Ok(tested) => Ok(EngineProfile {
      endpoint,
      profile,
      tested,
    }),
    Err(refusal) => Err(EngineFault::Untested {
      endpoint,
      profile: Box::new(profile),
      refusal,
    }),
  }
}

/// Podman's facts from the [`PODMAN_FACTS`] template's answer, or why they are not there (A-520): every field present,
/// the strings non-empty and the flags booleans, never a profile built from a guess.
fn parse_podman_facts(
  answer: &[u8],
) -> Result<slates_bridge_oci::runtime::PodmanFacts, EngineFault> {
  let unreadable = |why: String| EngineFault::Unreadable(Note::new(why));
  let value: serde_json::Value =
    serde_json::from_slice(answer).map_err(|e| unreadable(e.to_string()))?;
  let text = |field: &str| match value.get(field) {
    Some(serde_json::Value::String(text)) if !text.is_empty() => Ok(text.clone()),
    _ => Err(unreadable(format!("{field} is not a non-empty string"))),
  };
  let flag = |field: &str| match value.get(field) {
    Some(serde_json::Value::Bool(flag)) => Ok(*flag),
    _ => Err(unreadable(format!("{field} is not true or false"))),
  };
  Ok(slates_bridge_oci::runtime::PodmanFacts {
    operating_system: text("os")?,
    server_version: text("version")?,
    rootless: flag("rootless")?,
    selinux_enabled: flag("selinux")?,
    service_is_remote: flag("remote")?,
  })
}

/// The endpoint the context answers with: a JSON string.
fn parse_endpoint(answer: &[u8]) -> Result<String, EngineFault> {
  match serde_json::from_slice::<serde_json::Value>(answer) {
    Ok(serde_json::Value::String(endpoint)) => Ok(endpoint),
    Ok(other) => Err(EngineFault::Unreadable(Note::new(format!(
      "the endpoint is not a string: {other}"
    )))),
    Err(e) => Err(EngineFault::Unreadable(Note::new(e.to_string()))),
  }
}

/// The engine's facts from the [`ENGINE_FACTS`] template's answer, or why they are not there: an engine that reports
/// server errors did not answer as an engine.
fn parse_engine_facts(
  answer: &[u8],
) -> Result<slates_bridge_oci::runtime::EngineFacts, EngineFault> {
  let unreadable = |why: String| EngineFault::Unreadable(Note::new(why));
  let value: serde_json::Value =
    serde_json::from_slice(answer).map_err(|e| unreadable(e.to_string()))?;
  match value.get("errors") {
    None | Some(serde_json::Value::Null) => {}
    Some(serde_json::Value::Array(errors)) if errors.is_empty() => {}
    Some(errors) => {
      return Err(EngineFault::Unreachable(Note::new(format!(
        "the engine reports {errors}"
      ))));
    }
  }
  let text = |field: &str| match value.get(field) {
    Some(serde_json::Value::String(text)) if !text.is_empty() => Ok(text.clone()),
    _ => Err(unreadable(format!("{field} is not a non-empty string"))),
  };
  let security_options = match value.get("security") {
    None | Some(serde_json::Value::Null) => Vec::new(),
    Some(serde_json::Value::Array(options)) => options
      .iter()
      .map(|option| {
        option
          .as_str()
          .map(str::to_owned)
          .ok_or_else(|| unreadable(format!("a security option is not a string: {option}")))
      })
      .collect::<Result<_, _>>()?,
    Some(other) => return Err(unreadable(format!("security is not a list: {other}"))),
  };
  Ok(slates_bridge_oci::runtime::EngineFacts {
    operating_system: text("os")?,
    server_version: text("version")?,
    security_options,
  })
}

/// The most bytes an answer may hold: one page of this machine.
/// Derived: the template answers are a JSON string and a four-field object, the larger measured at 116 bytes (Docker
/// Desktop 29.3.1, 2026-10-01); a page is the smallest unit the OS hands a pipe's reader.
#[cfg(unix)]
fn answer_cap() -> usize {
  rustix::param::page_size()
}

/// Runs `runtime args` within the observe budget, its answer at most [`answer_cap`] bytes: the answer, or why there is
/// none. The answer and the runtime's complaint are read as they arrive, waiting on the pipes with the time left, until
/// both close; the child is then reaped, killed first if it has not exited (it overran the bound, or closed its output
/// and kept running). The decision is the answer's: an engine that cannot be reached says so in it (`docker info`'s
/// `ServerErrors`), and an empty answer reports what the runtime said.
#[cfg(unix)]
fn ask(runtime: &str, args: &[&str]) -> Result<Vec<u8>, EngineFault> {
  let mut child = std::process::Command::new(runtime)
    .args(args)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .map_err(|e| EngineFault::Unreachable(Note::new(format!("{runtime}: {e}"))))?;
  let outcome = collect(&mut child, runtime);
  if !matches!(child.try_wait(), Ok(Some(_))) {
    let _ = child.kill();
  }
  let _ = child.wait();
  outcome
}

/// Runs `runtime args` within the observe budget, its answer at most one page (4096 bytes: no template answer comes
/// near it) on a host with no Unix pipes to poll (Windows): the child's two pipes are read to their ends on threads of
/// their own, each keeping at most the cap and one byte more, while this thread waits on the child within the budget,
/// killing it past the bound. The decision is the answer's, as on Unix.
#[cfg(not(unix))]
fn ask(runtime: &str, args: &[&str]) -> Result<Vec<u8>, EngineFault> {
  use std::io::Read as _;
  // Shape: one page, 4 KiB on Windows (x86_64 and arm64), as [`answer_cap`] is a page on Unix.
  const CAP: usize = 4096;
  let unreachable = |why: String| EngineFault::Unreachable(Note::new(why));
  let mut child = std::process::Command::new(runtime)
    .args(args)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .map_err(|e| unreachable(format!("{runtime}: {e}")))?;
  let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
    std::thread::spawn(move || {
      let mut said = Vec::new();
      if let Some(pipe) = pipe {
        let _ = pipe
          .take(u64::try_from(CAP + 1).unwrap_or(u64::MAX))
          .read_to_end(&mut said);
      }
      said
    })
  };
  let out = read(
    child
      .stdout
      .take()
      .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
  );
  let err = read(
    child
      .stderr
      .take()
      .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
  );
  let budget = std::time::Duration::from_nanos(slates_server::daemon::OBSERVE_BUDGET_NS);
  let began = std::time::Instant::now();
  let mut overran = false;
  while !matches!(child.try_wait(), Ok(Some(_))) {
    if began.elapsed() >= budget {
      let _ = child.kill();
      overran = true;
      break;
    }
    // Shape: the wait's poll step, small against the observe budget it is bounded by.
    std::thread::sleep(std::time::Duration::from_millis(10));
  }
  let _ = child.wait();
  let answer = out.join().unwrap_or_default();
  let complaint = err.join().unwrap_or_default();
  if overran {
    return Err(unreachable(format!(
      "{runtime} did not answer within {budget:?}"
    )));
  }
  if answer.len() > CAP {
    return Err(EngineFault::TooLarge);
  }
  if answer.is_empty() {
    return Err(unreachable(format!(
      "{runtime} answered nothing: {}",
      String::from_utf8_lossy(&complaint).trim()
    )));
  }
  Ok(answer)
}

/// One open pipe of the child's: its descriptor, and what it has said so far.
#[cfg(unix)]
struct Pipe<R> {
  reader: Option<R>,
  said: Vec<u8>,
}

#[cfg(unix)]
impl<R: std::io::Read + std::os::fd::AsFd> Pipe<R> {
  /// Reads what the pipe holds now (it polled readable, so the read does not wait); a closed pipe is let go.
  fn drain(&mut self, chunk: &mut [u8]) -> Result<(), EngineFault> {
    let Some(reader) = self.reader.as_mut() else {
      return Ok(());
    };
    match reader.read(chunk) {
      Ok(0) => {
        self.reader = None;
        Ok(())
      }
      Ok(read) => {
        self
          .said
          .extend_from_slice(chunk.get(..read).unwrap_or_default());
        Ok(())
      }
      Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(()),
      Err(e) => Err(EngineFault::Unreachable(Note::new(format!(
        "reading the runtime: {e}"
      )))),
    }
  }
}

/// Collects the child's answer and complaint until both pipes close or the budget passes.
#[cfg(unix)]
fn collect(child: &mut std::process::Child, runtime: &str) -> Result<Vec<u8>, EngineFault> {
  use rustix::event::{PollFd, PollFlags};
  let unreachable = |why: String| EngineFault::Unreachable(Note::new(why));
  let budget = std::time::Duration::from_nanos(slates_server::daemon::OBSERVE_BUDGET_NS);
  let began = std::time::Instant::now();
  let cap = answer_cap();
  let mut answer = Pipe {
    reader: child.stdout.take(),
    said: Vec::new(),
  };
  let mut complaint = Pipe {
    reader: child.stderr.take(),
    said: Vec::new(),
  };
  let mut chunk = vec![0u8; cap];
  while answer.reader.is_some() || complaint.reader.is_some() {
    let left = budget.saturating_sub(began.elapsed());
    if left.is_zero() {
      return Err(unreachable(format!(
        "{runtime} did not answer within {budget:?}"
      )));
    }
    let timeout = rustix::time::Timespec::try_from(left)
      .map_err(|e| unreachable(format!("the bound {left:?}: {e}")))?;
    let (out_ready, err_ready) = {
      let mut fds = Vec::with_capacity(2);
      if let Some(reader) = answer.reader.as_ref() {
        fds.push(PollFd::new(reader, PollFlags::IN));
      }
      if let Some(reader) = complaint.reader.as_ref() {
        fds.push(PollFd::new(reader, PollFlags::IN));
      }
      match rustix::event::poll(&mut fds, Some(&timeout)) {
        Ok(_) | Err(rustix::io::Errno::INTR) => {}
        Err(e) => return Err(unreachable(format!("poll: {e}"))),
      }
      let ready = |at: usize| fds.get(at).is_some_and(|fd| !fd.revents().is_empty());
      match (answer.reader.is_some(), complaint.reader.is_some()) {
        (true, true) => (ready(0), ready(1)),
        (true, false) => (ready(0), false),
        (false, _) => (false, ready(0)),
      }
    };
    if out_ready {
      answer.drain(&mut chunk)?;
      if answer.said.len() > cap {
        return Err(EngineFault::TooLarge);
      }
    }
    if err_ready {
      complaint.drain(&mut chunk)?;
      // Only the first page of a complaint is kept; the rest is read and let go, so the child never blocks.
      complaint.said.truncate(cap);
    }
  }
  if answer.said.is_empty() {
    return Err(unreachable(format!(
      "{runtime} answered nothing: {}",
      String::from_utf8_lossy(&complaint.said).trim()
    )));
  }
  Ok(answer.said)
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_bridge_oci::runtime::Host;

  /// Format: Docker Desktop's statement as its `info` template answered on macOS (29.3.1, 2026-10-01).
  const DESKTOP: &[u8] = br#"{"os":"Docker Desktop","version":"29.3.1","security":["name=seccomp,profile=builtin","name=cgroupns"],"errors":null}"#;
  /// Format: a rootful Docker Engine's statement on its own Linux host.
  const ENGINE: &[u8] =
    br#"{"os":"Ubuntu 24.04.1 LTS","version":"29.7.2","security":["name=seccomp,profile=builtin"],"errors":null}"#;
  /// Format: a rootless engine's statement.
  const ROOTLESS: &[u8] = br#"{"os":"Ubuntu 24.04.1 LTS","version":"29.7.2","security":["name=seccomp,profile=builtin","name=rootless"],"errors":null}"#;
  /// Format: Docker Desktop's local socket as its context names it.
  const DESKTOP_SOCKET: &str = "unix:///Users/u/.docker/run/docker.sock";
  /// Format: rootless podman's statement on its own Linux host, as its `info` template answers it.
  const ROOTLESS_PODMAN: &[u8] =
    br#"{"os":"linux","version":"5.2.2","rootless":true,"selinux":false,"remote":false}"#;

  /// Podman's statement: rootless podman's with `field` set to `value`, a JSON value.
  fn podman_with(field: &str, value: serde_json::Value) -> Vec<u8> {
    let mut statement: serde_json::Value = serde_json::from_slice(ROOTLESS_PODMAN).unwrap();
    statement[field] = value;
    serde_json::to_vec(&statement).unwrap()
  }

  /// An engine whose context names `endpoint` (a JSON string) and whose statement is `facts`: each query answered by
  /// its template, and every question asked recorded in `asked` (the template's first word: `context` or `info`).
  fn engine<'a>(
    endpoint: &'a [u8],
    facts: &'a [u8],
    asked: &'a mut Vec<String>,
  ) -> impl FnMut(&[&str]) -> Result<Vec<u8>, EngineFault> + 'a {
    move |args: &[&str]| {
      let first = args.first().copied().unwrap_or_default().to_owned();
      let answer = if first == "context" { endpoint } else { facts };
      asked.push(first);
      Ok(answer.to_vec())
    }
  }

  /// The endpoint, the engine and the judge's refusal of a profile its statement established but the judge refused
  /// (`EngineFault::Untested`); any other answer fails the test that asked.
  fn untested(judged: Result<EngineProfile, EngineFault>) -> (String, String, ProfileRefusal) {
    match judged {
      Err(EngineFault::Untested {
        endpoint,
        profile,
        refusal,
      }) => (endpoint, profile.operating_system, refusal),
      other => panic!("a profile established and refused, not {other:?}"),
    }
  }

  /// AUD-29-67, A-520. Do: judge Docker Desktop on macOS through its context's local socket, then the same with the
  /// endpoint named by the environment. Expect the profile T-4.13 holds (Desktop's, every container id reaching the
  /// volume as the host user through the share, a hard link's other names stale once its first is removed), the
  /// context asked first and the engine second, and with the environment's endpoint the context never asked.
  #[test]
  fn docker_desktop_on_macos_holds_its_tested_profile_and_the_context_is_asked_only_unnamed() {
    let mut asked = Vec::new();
    let endpoint = format!("{DESKTOP_SOCKET:?}");
    let judged = profile_of(
      "docker",
      None,
      Host::MacOs,
      &mut engine(endpoint.as_bytes(), DESKTOP, &mut asked),
    )
    .unwrap();
    assert_eq!(judged.endpoint, DESKTOP_SOCKET);
    assert_eq!(judged.profile.engine, Engine::DockerDesktop);
    assert_eq!(
      (judged.tested.identity, judged.tested.hard_links),
      (
        IdentityRule::HostUserThroughShare,
        HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved
      )
    );
    assert_eq!(asked, ["context", "info"]);
    let mut asked = Vec::new();
    let named = profile_of(
      "/usr/local/bin/docker",
      Some(DESKTOP_SOCKET.to_owned()),
      Host::MacOs,
      &mut engine(b"null", DESKTOP, &mut asked),
    )
    .unwrap();
    assert_eq!(named.endpoint, DESKTOP_SOCKET);
    assert_eq!(
      asked,
      ["info"],
      "a named endpoint is not asked of the context"
    );
  }

  /// AUD-29-67, A-520. Do: judge a rootful Docker Engine on Linux over its local socket, a rootless one, the rootful one
  /// judged as if on Windows, and Desktop's statement through an engine on Linux. Expect the Linux engine's tested
  /// profile (its ids forwarded as the host's own, every hard link's name served at once); the rootless one refused
  /// `UserNamespaceUntested` and the other two `ProfileUntested` naming the engine and the host, each `Untested` with the
  /// endpoint and the engine its statement named, so a host can say which engine it refused.
  #[test]
  fn each_engine_on_each_host_holds_its_tested_profile_or_is_refused_typed() {
    let socket = br#""unix:///var/run/docker.sock""#;
    let mut asked = Vec::new();
    let linux = profile_of(
      "docker",
      None,
      Host::Linux,
      &mut engine(socket, ENGINE, &mut asked),
    )
    .unwrap();
    assert_eq!(
      (
        linux.profile.engine,
        linux.tested.identity,
        linux.tested.hard_links
      ),
      (
        Engine::DockerEngine,
        IdentityRule::ContainerIdsAsHostIds,
        HardLinkRule::EveryNameServedAtOnce
      )
    );
    let unix_socket = "unix:///var/run/docker.sock";
    let mut asked = Vec::new();
    assert_eq!(
      untested(profile_of(
        "docker",
        None,
        Host::Linux,
        &mut engine(socket, ROOTLESS, &mut asked)
      )),
      (
        unix_socket.to_owned(),
        "Ubuntu 24.04.1 LTS".to_owned(),
        ProfileRefusal::UserNamespaceUntested
      )
    );
    let mut asked = Vec::new();
    let pipe = "npipe:////./pipe/docker_engine";
    assert_eq!(
      untested(profile_of(
        "docker.exe",
        Some(pipe.to_owned()),
        Host::Windows,
        &mut engine(b"null", ENGINE, &mut asked)
      )),
      (
        pipe.to_owned(),
        "Ubuntu 24.04.1 LTS".to_owned(),
        ProfileRefusal::ProfileUntested {
          engine: "Ubuntu 24.04.1 LTS".to_owned(),
          host: "windows"
        }
      )
    );
    let mut asked = Vec::new();
    assert_eq!(
      untested(profile_of(
        "docker",
        None,
        Host::Linux,
        &mut engine(socket, DESKTOP, &mut asked)
      )),
      (
        unix_socket.to_owned(),
        "Docker Desktop".to_owned(),
        ProfileRefusal::ProfileUntested {
          engine: "Docker Desktop".to_owned(),
          host: "linux"
        }
      )
    );
  }

  /// AUD-29-67, A-520 (the order that consults least). Do: judge nerdctl's command line, an engine whose context names a
  /// TCP endpoint and one whose context names an SSH one, and an engine that answers too much. Expect nerdctl refused
  /// `RuntimeUnsupported` with nothing asked; each remote endpoint refused `RemoteEngine` with only the context asked,
  /// never the engine behind it; and the oversized answer's fault passed on as it came.
  #[test]
  fn nothing_is_asked_past_a_refusal() {
    let mut asked = Vec::new();
    assert_eq!(
      profile_of(
        "nerdctl",
        None,
        Host::Linux,
        &mut engine(b"null", ENGINE, &mut asked)
      ),
      Err(EngineFault::Refused(ProfileRefusal::RuntimeUnsupported {
        runtime: "nerdctl".to_owned()
      }))
    );
    assert!(
      asked.is_empty(),
      "nothing is asked of a runtime the handshake does not speak"
    );
    for remote in ["tcp://127.0.0.1:2375", "ssh://builder@host"] {
      let mut asked = Vec::new();
      let endpoint = format!("{remote:?}");
      assert_eq!(
        profile_of(
          "docker",
          None,
          Host::Linux,
          &mut engine(endpoint.as_bytes(), ENGINE, &mut asked)
        ),
        Err(EngineFault::Refused(ProfileRefusal::RemoteEngine {
          endpoint: remote.to_owned()
        }))
      );
      assert_eq!(asked, ["context"], "no engine is asked behind {remote}");
    }
    let too_much =
      &mut |_args: &[&str]| -> Result<Vec<u8>, EngineFault> { Err(EngineFault::TooLarge) };
    assert_eq!(
      profile_of("docker", None, Host::Linux, too_much),
      Err(EngineFault::TooLarge)
    );
  }

  /// A-520 (rootless podman's verdict). Do: judge rootless podman on its own Linux host. Expect podman asked its own
  /// statement alone (its `info`, no context: podman states whether its service is remote), the profile local and
  /// podman's, and the evidence T-4.13's rootless leg holds: the container's root reaching the volume as the user who
  /// mounted it, and every name of a hard-linked file served at once.
  #[test]
  fn rootless_podman_on_linux_holds_its_tested_profile_asking_podman_alone() {
    let mut asked = Vec::new();
    let judged = profile_of(
      "podman",
      None,
      Host::Linux,
      &mut engine(b"null", ROOTLESS_PODMAN, &mut asked),
    )
    .unwrap();
    assert_eq!(asked, ["info"], "podman is asked its own statement alone");
    assert_eq!(judged.endpoint, LOCAL);
    assert_eq!(
      (judged.profile.engine, &judged.profile.endpoint),
      (Engine::Podman, &Endpoint::Local)
    );
    assert_eq!(
      (judged.tested.identity, judged.tested.hard_links),
      (
        IdentityRule::ContainerRootAsMountingUser,
        HardLinkRule::EveryNameServedAtOnce
      )
    );
  }

  /// A-520. Do: judge podman whose service is remote, podman run as root, rootless podman that labels for SELinux, and
  /// rootless podman judged on macOS; then parse podman's answers missing a field, giving a word for a flag, an empty
  /// version, and garbage. Expect `RemoteEngine` before any profile; `Untested` naming the local profile for the rest,
  /// `ProfileUntested`, `SelinuxLabelsUntested` and `UserNamespaceUntested`; and each hostile answer `Unreadable`.
  #[test]
  fn every_other_podman_is_refused_typed() {
    let mut asked = Vec::new();
    let remote = podman_with("remote", serde_json::Value::Bool(true));
    assert!(matches!(
      profile_of(
        "podman",
        None,
        Host::Linux,
        &mut engine(b"null", &remote, &mut asked)
      ),
      Err(EngineFault::Refused(ProfileRefusal::RemoteEngine { .. }))
    ));
    let as_root = podman_with("rootless", serde_json::Value::Bool(false));
    let mut asked = Vec::new();
    assert_eq!(
      untested(profile_of(
        "podman",
        None,
        Host::Linux,
        &mut engine(b"null", &as_root, &mut asked)
      )),
      (
        LOCAL.to_owned(),
        "linux".to_owned(),
        ProfileRefusal::ProfileUntested {
          engine: "linux".to_owned(),
          host: "linux"
        }
      )
    );
    let labelled = podman_with("selinux", serde_json::Value::Bool(true));
    let mut asked = Vec::new();
    assert_eq!(
      untested(profile_of(
        "podman",
        None,
        Host::Linux,
        &mut engine(b"null", &labelled, &mut asked)
      ))
      .2,
      ProfileRefusal::SelinuxLabelsUntested
    );
    let mut asked = Vec::new();
    assert_eq!(
      untested(profile_of(
        "podman",
        None,
        Host::MacOs,
        &mut engine(b"null", ROOTLESS_PODMAN, &mut asked)
      ))
      .2,
      ProfileRefusal::UserNamespaceUntested
    );
    let missing = br#"{"os":"linux","version":"5.2.2","rootless":true,"selinux":false}"#;
    let word = podman_with("rootless", serde_json::Value::String("true".to_owned()));
    let empty = podman_with("version", serde_json::Value::String(String::new()));
    for hostile in [
      &missing[..],
      word.as_slice(),
      empty.as_slice(),
      &b"not json"[..],
    ] {
      assert!(
        matches!(parse_podman_facts(hostile), Err(EngineFault::Unreadable(_))),
        "{}",
        String::from_utf8_lossy(hostile)
      );
    }
  }

  /// AUD-29-67 (hostile input; moved from the CLI with the parser, A-520). Do: parse the engine's answer as Desktop
  /// gave it, then garbage, truncated JSON, wrong types, an empty version, an engine reporting server errors, and a
  /// security list holding a number. Expect: the first parses to its facts; every other is refused typed, the server
  /// errors as `Unreachable` and the rest as `Unreadable`: never a profile built from a guess.
  #[test]
  fn an_engine_answer_not_in_the_templates_shape_is_refused_typed() {
    let facts = parse_engine_facts(DESKTOP).unwrap();
    assert_eq!(facts.operating_system, "Docker Desktop");
    assert_eq!(facts.security_options.len(), 2);
    for unreadable in [
      &b""[..],
      b"not json",
      br#"{"os":"Docker Desktop","version":"29.3"#,
      br#"{"os":7,"version":"29.3.1","security":[],"errors":null}"#,
      br#"{"os":"Docker Desktop","version":"","security":[],"errors":null}"#,
      br#"{"os":"Docker Desktop","version":"29.3.1","security":[1],"errors":null}"#,
      br#"{"os":"Docker Desktop","version":"29.3.1","security":"name=userns","errors":null}"#,
      br#"[]"#,
    ] {
      assert!(
        matches!(
          parse_engine_facts(unreadable),
          Err(EngineFault::Unreadable(_))
        ),
        "{}",
        String::from_utf8_lossy(unreadable)
      );
    }
    let erring =
      br#"{"os":"","version":"","security":null,"errors":["Cannot connect to the Docker daemon"]}"#;
    assert!(matches!(
      parse_engine_facts(erring),
      Err(EngineFault::Unreachable(_))
    ));
  }

  /// AUD-29-67 (hostile input; moved from the CLI, A-520). Do: parse a context's endpoint answer, then a JSON null.
  /// Expect: the string, then `Unreadable`.
  #[test]
  fn an_endpoint_answer_that_is_not_a_string_is_refused_typed() {
    assert_eq!(
      parse_endpoint(br#""unix:///var/run/docker.sock""#),
      Ok("unix:///var/run/docker.sock".to_owned())
    );
    assert!(matches!(
      parse_endpoint(b"null"),
      Err(EngineFault::Unreadable(_))
    ));
  }

  /// Format: Docker Desktop's named pipe on Windows as its context names it.
  const DESKTOP_PIPE: &str = "npipe:////./pipe/dockerDesktopLinuxEngine";

  /// A-520's export route (main's ruling, 2026-11-20). Do: judge, for the export route, Docker Desktop on Windows over
  /// its named pipe, on macOS over its socket, and a rootful Docker Engine on its own Linux host. Expect each to mount the
  /// export (Desktop from its VM, the Engine from the host's kernel), the context asked first and the engine second.
  #[test]
  fn the_export_route_takes_docker_desktop_on_windows_and_macos_from_its_vm_and_a_rootful_linux_engine_from_the_host()
   {
    let cases = [
      (
        "docker.exe",
        DESKTOP_PIPE,
        DESKTOP,
        Host::Windows,
        MountsFrom::DesktopVm,
      ),
      (
        "docker",
        DESKTOP_SOCKET,
        DESKTOP,
        Host::MacOs,
        MountsFrom::DesktopVm,
      ),
      (
        "docker",
        "unix:///var/run/docker.sock",
        ENGINE,
        Host::Linux,
        MountsFrom::HostKernel,
      ),
    ];
    for (runtime, endpoint, facts, host, from) in cases {
      let mut asked = Vec::new();
      let context = format!("{endpoint:?}");
      let judged = export_engine_of(
        runtime,
        None,
        host,
        &mut engine(context.as_bytes(), facts, &mut asked),
      );
      let judged = judged.unwrap_or_else(|e| panic!("{runtime} on {host:?}: {e}"));
      assert_eq!(
        (judged.endpoint.as_str(), judged.tested.mounts_from),
        (endpoint, from),
        "{runtime} on {host:?}"
      );
      assert_eq!(asked, ["context", "info"]);
    }
  }

  /// A-520's export route. Do: judge, for the export route, a rootless Docker Engine, rootless podman, and Docker Desktop
  /// on a host it doesn't run on as named (Linux). Expect the rootless two refused `RootlessCannotMount` (a rootless
  /// engine can't mount a file system), the last `ProfileUntested`, each with its profile.
  #[test]
  fn the_export_route_refuses_a_rootless_engine_and_an_unnamed_host_typed() {
    let mut asked = Vec::new();
    let context = format!("{:?}", "unix:///run/user/1000/docker.sock");
    let rootless = export_engine_of(
      "docker",
      None,
      Host::Linux,
      &mut engine(context.as_bytes(), ROOTLESS, &mut asked),
    );
    assert!(
      matches!(
        rootless,
        Err(EngineFault::Untested {
          refusal: ProfileRefusal::RootlessCannotMount,
          ..
        })
      ),
      "{rootless:?}"
    );
    let mut asked = Vec::new();
    let podman = export_engine_of(
      "podman",
      None,
      Host::Linux,
      &mut engine(b"null", ROOTLESS_PODMAN, &mut asked),
    );
    assert!(
      matches!(
        podman,
        Err(EngineFault::Untested {
          refusal: ProfileRefusal::RootlessCannotMount,
          ..
        })
      ),
      "{podman:?}"
    );
    let mut asked = Vec::new();
    let context = format!("{DESKTOP_SOCKET:?}");
    let elsewhere = export_engine_of(
      "docker",
      None,
      Host::Linux,
      &mut engine(context.as_bytes(), DESKTOP, &mut asked),
    );
    assert!(
      matches!(
        elsewhere,
        Err(EngineFault::Untested {
          refusal: ProfileRefusal::ProfileUntested { .. },
          ..
        })
      ),
      "{elsewhere:?}"
    );
  }
}
