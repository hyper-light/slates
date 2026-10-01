//! `slates oci-runtime RUNTIME`: the consuming runtime's bounded handshake (§4.6 A-9; AUD-29-67). A container
//! harness runs it immediately before its runtime binds a verified source, beside `slates oci-check`: the
//! source check says the path is still the mount verified, and this says whether the runtime that will bind
//! it is a profile a container workload has run through.
//!
//! The handshake asks the runtime's engine, through the runtime's own CLI, for the facts that decide where a
//! bind source resolves and with which ids a container writes: the endpoint the CLI reaches (`DOCKER_HOST`
//! when set, else the current context's), refused before the engine is asked when it is not local; then the
//! engine's kind, version and security options (`docker info`, four fields through a template). The judge is
//! pure (`slates_bridge_oci::runtime`); this module runs the queries, each bounded in time and in the size of
//! its answer, and refuses typed when the engine does not answer, answers too much, or answers garbage.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use slates_bridge_oci::runtime::{
  EngineFacts, HardLinkRule, Host, IdentityRule, admit_endpoint, admit_runtime, judge,
  runtime_profile,
};
use slates_server::daemon::OBSERVE_BUDGET_NS;

use crate::Failure;

/// Format: the environment variable that names the Docker CLI's endpoint, overriding its context (Docker's
/// CLI reference, "Environment variables", tier B).
const ENDPOINT_VARIABLE: &str = "DOCKER_HOST";

/// Format: the template that asks the current context for its engine endpoint, as a JSON string.
const CONTEXT_ENDPOINT: [&str; 4] = [
  "context",
  "inspect",
  "--format",
  "{{json .Endpoints.docker.Host}}",
];

/// Format: the template that asks the engine for the four facts the judge reads, as one JSON object. Measured
/// answer: 116 bytes from Docker Desktop 29.3.1 (2026-10-01).
const ENGINE_FACTS: [&str; 3] = [
  "info",
  "--format",
  r#"{"os":{{json .OperatingSystem}},"version":{{json .ServerVersion}},"security":{{json .SecurityOptions}},"errors":{{json .ServerErrors}}}"#,
];

/// Why the handshake could not establish the engine's facts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EngineAnswer {
  /// The runtime did not run, did not answer within the bound, or exited unsuccessfully.
  Unreachable(String),
  /// The answer was larger than one page; no template answer is that large.
  TooLarge,
  /// The answer was not the template's shape.
  Unreadable(String),
}

impl std::fmt::Display for EngineAnswer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Unreachable(why) => write!(f, "EngineUnreachable: {why}"),
      Self::TooLarge => f.write_str("EngineAnswerTooLarge: more than one page"),
      Self::Unreadable(why) => write!(f, "EngineAnswerUnreadable: {why}"),
    }
  }
}

/// The most bytes an answer may hold: one page of this machine.
/// Derived: the template answers are a JSON string and a four-field object, the larger measured at 116 bytes
/// (Docker Desktop 29.3.1, 2026-10-01); a page is the smallest unit the OS hands a pipe's reader.
fn answer_cap() -> usize {
  rustix::param::page_size()
}

/// Runs `runtime args` within the observe budget, its answer at most [`answer_cap`] bytes: the answer, or why
/// there is none. The answer and the runtime's complaint are read as they arrive, waiting on the pipes with the
/// time left, until both close; the child is then reaped, killed first if it has not exited (it overran the
/// bound, or closed its output and kept running). The decision is the answer's: an engine that cannot be
/// reached says so in it (`docker info`'s `ServerErrors`), and an empty answer reports what the runtime said.
fn ask(runtime: &str, args: &[&str]) -> Result<Vec<u8>, EngineAnswer> {
  let mut child = Command::new(runtime)
    .args(args)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| EngineAnswer::Unreachable(format!("{runtime}: {e}")))?;
  let outcome = collect(&mut child, runtime);
  if !matches!(child.try_wait(), Ok(Some(_))) {
    let _ = child.kill();
  }
  let _ = child.wait();
  outcome
}

/// One open pipe of the child's: its descriptor, and what it has said so far.
struct Pipe<R> {
  reader: Option<R>,
  said: Vec<u8>,
}

impl<R: Read + std::os::fd::AsFd> Pipe<R> {
  /// Reads what the pipe holds now (it polled readable, so the read does not wait); a closed pipe is let go.
  fn drain(&mut self, chunk: &mut [u8]) -> Result<(), EngineAnswer> {
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
      Err(e) => Err(EngineAnswer::Unreachable(format!(
        "reading the runtime: {e}"
      ))),
    }
  }
}

/// Collects the child's answer and complaint until both pipes close or the budget passes.
fn collect(child: &mut std::process::Child, runtime: &str) -> Result<Vec<u8>, EngineAnswer> {
  let budget = Duration::from_nanos(OBSERVE_BUDGET_NS);
  let began = Instant::now();
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
      return Err(EngineAnswer::Unreachable(format!(
        "{runtime} did not answer within {budget:?}"
      )));
    }
    let timeout = rustix::time::Timespec::try_from(left)
      .map_err(|e| EngineAnswer::Unreachable(format!("the bound {left:?}: {e}")))?;
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
        Err(e) => return Err(EngineAnswer::Unreachable(format!("poll: {e}"))),
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
        return Err(EngineAnswer::TooLarge);
      }
    }
    if err_ready {
      complaint.drain(&mut chunk)?;
      // Only the first page of a complaint is kept; the rest is read and let go, so the child never blocks.
      complaint.said.truncate(cap);
    }
  }
  if answer.said.is_empty() {
    return Err(EngineAnswer::Unreachable(format!(
      "{runtime} answered nothing: {}",
      String::from_utf8_lossy(&complaint.said).trim()
    )));
  }
  Ok(answer.said)
}

/// The endpoint the context answers with: a JSON string.
pub(crate) fn parse_endpoint(answer: &[u8]) -> Result<String, EngineAnswer> {
  match serde_json::from_slice::<serde_json::Value>(answer) {
    Ok(serde_json::Value::String(endpoint)) => Ok(endpoint),
    Ok(other) => Err(EngineAnswer::Unreadable(format!(
      "the endpoint is not a string: {other}"
    ))),
    Err(e) => Err(EngineAnswer::Unreadable(e.to_string())),
  }
}

/// The engine's facts from the [`ENGINE_FACTS`] template's answer, or why they are not there: an engine that
/// reports server errors did not answer as an engine.
pub(crate) fn parse_engine_facts(answer: &[u8]) -> Result<EngineFacts, EngineAnswer> {
  let value: serde_json::Value =
    serde_json::from_slice(answer).map_err(|e| EngineAnswer::Unreadable(e.to_string()))?;
  match value.get("errors") {
    None | Some(serde_json::Value::Null) => {}
    Some(serde_json::Value::Array(errors)) if errors.is_empty() => {}
    Some(errors) => {
      return Err(EngineAnswer::Unreachable(format!(
        "the engine reports {errors}"
      )));
    }
  }
  let text = |field: &str| match value.get(field) {
    Some(serde_json::Value::String(text)) if !text.is_empty() => Ok(text.clone()),
    _ => Err(EngineAnswer::Unreadable(format!(
      "{field} is not a non-empty string"
    ))),
  };
  let security_options = match value.get("security") {
    None | Some(serde_json::Value::Null) => Vec::new(),
    Some(serde_json::Value::Array(options)) => options
      .iter()
      .map(|option| {
        option.as_str().map(str::to_owned).ok_or_else(|| {
          EngineAnswer::Unreadable(format!("a security option is not a string: {option}"))
        })
      })
      .collect::<Result<_, _>>()?,
    Some(other) => {
      return Err(EngineAnswer::Unreadable(format!(
        "security is not a list: {other}"
      )));
    }
  };
  Ok(EngineFacts {
    operating_system: text("os")?,
    server_version: text("version")?,
    security_options,
  })
}

/// The endpoint the runtime's CLI reaches: `DOCKER_HOST` when set, else its current context's.
fn endpoint_of(runtime: &str) -> Result<String, EngineAnswer> {
  match std::env::var(ENDPOINT_VARIABLE) {
    Ok(endpoint) if !endpoint.is_empty() => Ok(endpoint),
    _ => parse_endpoint(&ask(runtime, &CONTEXT_ENDPOINT)?),
  }
}

/// A profile's identity rule on the CLI's output.
fn identity_text(rule: IdentityRule) -> &'static str {
  match rule {
    IdentityRule::HostUserThroughShare => {
      "host_user_through_share (container ids are not forwarded; the attachment's capability is the authority)"
    }
  }
}

/// A profile's hard-link rule on the CLI's output.
fn hard_link_text(rule: HardLinkRule) -> &'static str {
  match rule {
    HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved => {
      "other_names_stale_after_the_first_is_removed (until the guest revalidates; git: core.createObject=rename)"
    }
  }
}

/// `oci-runtime RUNTIME`: the profile and the evidence it holds (exit 0), or the typed refusal (exit 1).
pub(crate) fn oci_runtime(runtime: &str) -> Result<(), Failure> {
  admit_runtime(runtime).map_err(|refusal| Failure::Refused(refusal.to_string()))?;
  let endpoint_text = endpoint_of(runtime).map_err(|e| Failure::Refused(e.to_string()))?;
  let endpoint = admit_endpoint(runtime, &endpoint_text)
    .map_err(|refusal| Failure::Refused(refusal.to_string()))?;
  let facts =
    parse_engine_facts(&ask(runtime, &ENGINE_FACTS).map_err(|e| Failure::Refused(e.to_string()))?)
      .map_err(|e| Failure::Refused(e.to_string()))?;
  let profile = runtime_profile(endpoint, &facts);
  println!(
    "profile: engine={:?} version={} endpoint={endpoint_text} rootless={} userns={}",
    profile.operating_system,
    profile.server_version,
    profile.rootless,
    profile.user_namespace_remap
  );
  let tested =
    judge(&profile, Host::this()).map_err(|refusal| Failure::Refused(refusal.to_string()))?;
  println!("evidence: {} ({})", tested.test, tested.description);
  println!("identity: {}", identity_text(tested.identity));
  println!("hard_links: {}", hard_link_text(tested.hard_links));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AUD-29-67 (hostile input). Do: parse the engine's answer as Desktop gave it, then garbage, truncated
  /// JSON, wrong types, an empty version, an engine reporting server errors, and a security list holding a
  /// number. Expect: the first parses to its facts; every other is refused typed, the server errors as
  /// `Unreachable` and the rest as `Unreadable` — never a profile built from a guess.
  #[test]
  fn an_engine_answer_not_in_the_templates_shape_is_refused_typed() {
    let desktop = br#"{"os":"Docker Desktop","version":"29.3.1","security":["name=seccomp,profile=builtin","name=cgroupns"],"errors":null}"#;
    let facts = parse_engine_facts(desktop).unwrap();
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
          Err(EngineAnswer::Unreadable(_))
        ),
        "{}",
        String::from_utf8_lossy(unreadable)
      );
    }
    let erring =
      br#"{"os":"","version":"","security":null,"errors":["Cannot connect to the Docker daemon"]}"#;
    assert!(matches!(
      parse_engine_facts(erring),
      Err(EngineAnswer::Unreachable(_))
    ));
  }

  /// AUD-29-67 (hostile input). Do: parse a context's endpoint answer, then a JSON null. Expect: the string,
  /// then `Unreadable`.
  #[test]
  fn an_endpoint_answer_that_is_not_a_string_is_refused_typed() {
    assert_eq!(
      parse_endpoint(br#""unix:///var/run/docker.sock""#),
      Ok("unix:///var/run/docker.sock".to_owned())
    );
    assert!(matches!(
      parse_endpoint(b"null"),
      Err(EngineAnswer::Unreadable(_))
    ));
  }
}
