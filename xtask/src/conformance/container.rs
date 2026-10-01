//! A suite run inside a real container through the OCI attachment (§4.6 A-9; AUD-29-78: "add §13's by-use
//! cases, including the exact OCI mount entry", "pin tested runtime profiles"). The lane is the macOS host's
//! NFS mount handed to a container the way a harness would hand it:
//! - the session's host mount (the native lane's daemon, volume and `mount_nfs`);
//! - the runtime handshake, `slates oci-runtime docker`, whose profile line the record keeps — a profile
//!   without evidence fails the lane, it is never recorded as a pass;
//! - `attach --oci`, then `slates oci-check` of the returned source immediately before the bind;
//! - one `docker run` of the returned entry as Docker's `--mount` (non-recursive, private, read-only when
//!   the entry says `ro`), as the mounting user, in which the suite is compiled from the pinned source and
//!   run inside the bind.
//!
//! The suite's own verdict judges the run, as on the host lane; the record names the container command and
//! the profile, so a pass is a pass for that profile only.

use std::path::Path;
use std::process::{Command, Stdio};

use slates_conformance::capability::HostOs;
use slates_conformance::exerciser::{judge_fsstress, judge_fsx};
use slates_conformance::record::{Counts, Outcome};

use super::fetch;
use super::slates::Session;
use super::suites::this_user;
use super::{Run, SuiteResult};
use crate::Failure;

/// Format: the runtime CLI the lane binds with — the one profile the handshake holds evidence for.
const RUNTIME: &str = "docker";
/// Format: the image the suite is compiled and run in: the toolchain image the Linux lanes already use
/// (`rust:1.98.0`, Debian with `cc`), so the lane installs nothing.
const IMAGE: &str = "rust:1.98.0";
/// Format: where the container sees the volume, and where it sees the pinned sources (read-only).
const DESTINATION: &str = "/work";
const SOURCES: &str = "/src";

/// The binding `attach --oci` returned: the entry's source, its options, and the evidence the source
/// check needs.
struct Binding {
  source: String,
  read_only: bool,
  mount_id: u64,
  device: u64,
}

/// Attaches the session's volume in the container form over its host mount, and checks the source again
/// just before the bind.
fn bind(session: &Session) -> Result<Binding, Failure> {
  let mount = session.mount.path().display().to_string();
  let reply = session
    .binary
    .run(
      &session.instance,
      &[
        "attach",
        &session.volume_id,
        "--oci-source",
        &mount,
        "--oci-destination",
        DESTINATION,
        "--write",
        "--json",
      ],
    )?
    .expect_ok("attach --oci")?;
  let json = reply.json()?;
  let binding = &json["established"]["binding"];
  let entry = &binding["mount"];
  let number = |value: &serde_json::Value, what: &str| {
    value
      .as_u64()
      .ok_or_else(|| Failure(format!("the binding has no {what}: {json}")))
  };
  let bound = Binding {
    source: entry["source"]
      .as_str()
      .ok_or_else(|| Failure(format!("the entry has no source: {json}")))?
      .to_owned(),
    read_only: entry["options"]
      .as_array()
      .is_some_and(|options| options.iter().any(|o| o == "ro")),
    mount_id: number(&binding["evidence"]["mount_id"], "mount id")?,
    device: number(&binding["evidence"]["mount_device"], "mount device")?,
  };
  session
    .binary
    .run(
      &session.instance,
      &[
        "oci-check",
        &bound.source,
        &bound.mount_id.to_string(),
        &bound.device.to_string(),
      ],
    )?
    .expect_ok("oci-check before the bind")?;
  Ok(bound)
}

/// The runtime's profile line, or the handshake's refusal as the lane's failure.
fn handshake(session: &Session) -> Result<String, Failure> {
  let reply = session
    .binary
    .run(&session.instance, &["oci-runtime", RUNTIME])?
    .expect_ok("the runtime handshake")?;
  Ok(reply.stdout.trim().replace('\n', "; "))
}

/// The mounting user as `UID:GID`, the identity the container runs as (the tested profile's).
#[cfg(unix)]
fn mounting_user() -> Result<String, Failure> {
  Ok(format!(
    "{}:{}",
    rustix::process::getuid().as_raw(),
    rustix::process::getgid().as_raw()
  ))
}

/// The container leg runs on the macOS lane only; no other host reaches it.
#[cfg(not(unix))]
fn mounting_user() -> Result<String, Failure> {
  Err(Failure(
    "the container leg runs on the macOS lane".to_owned(),
  ))
}

/// `docker run` of the binding as Docker's `--mount`, as the mounting user, with the pinned sources
/// read-only; `script` runs under `sh -c`. The exit status and the combined output.
fn run_in_container(
  binding: &Binding,
  sources: &Path,
  script: &str,
) -> Result<(bool, String, String), Failure> {
  let mut volume = format!(
    "type=bind,source={},destination={DESTINATION},bind-recursive=disabled,bind-propagation=private",
    binding.source
  );
  if binding.read_only {
    volume.push_str(",readonly");
  }
  let pinned = format!(
    "type=bind,source={},destination={SOURCES},readonly",
    sources.display()
  );
  let user = mounting_user()?;
  let args = [
    "run", "--rm", "--user", &user, "--mount", &volume, "--mount", &pinned, IMAGE, "sh", "-c",
    script,
  ];
  let output = Command::new(RUNTIME)
    .args(args)
    .stdin(Stdio::null())
    .output()
    .map_err(|e| Failure(format!("running {RUNTIME}: {e}")))?;
  let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
  text.push_str(&String::from_utf8_lossy(&output.stderr));
  let command = format!("{RUNTIME} {}", args.join(" "));
  Ok((output.status.success(), text, command))
}

/// `text` with the home directory written `~`, so a record names no account's home.
fn home_as_tilde(text: &str) -> String {
  match std::env::var("HOME") {
    Ok(home) if !home.is_empty() => text.replace(&home, "~"),
    _ => text.to_owned(),
  }
}

/// A container leg's setup and what its record carries: the session (host mount), the handshake's profile, the
/// checked binding.
struct Leg {
  session: Session,
  profile: String,
  binding: Binding,
}

impl Leg {
  /// Opens the session for `suite`, asks the runtime for its profile, makes the suite's working directory
  /// inside the mount, and binds the mount in the container form, checked again just before the bind.
  fn open(run: &Run<'_>, suite: &str) -> Result<Leg, Failure> {
    let session = Session::open(
      run,
      &format!("oci-{suite}"),
      super::VOLUME_SIZE,
      false,
      None,
    )?;
    let profile = handshake(&session)?;
    session.workdir(suite)?;
    let binding = bind(&session)?;
    Ok(Leg {
      session,
      profile,
      binding,
    })
  }

  /// The suite's working directory as the container sees it.
  fn workdir(suite: &str) -> String {
    format!("{DESTINATION}/conformance-{}/{suite}", std::process::id())
  }

  /// The notes every container record carries: the pinned source's note, the profile, the source check.
  fn notes(&self, source_note: String) -> Vec<String> {
    vec![
      source_note,
      format!(
        "the runtime's profile (`slates oci-runtime {RUNTIME}`): {}",
        home_as_tilde(&self.profile)
      ),
      format!(
        "the source checked again (`slates oci-check`, mount {} on device {}) just before the bind",
        self.binding.mount_id, self.binding.device
      ),
    ]
  }

  /// The record's command, reading the same on every host: the bind's source is the session's mount, the
  /// user the mounting user, the work directory the process's; the sources' path is the scratch the
  /// harness names `<scratch>` itself.
  fn recorded(&self, command: &str) -> Result<String, Failure> {
    Ok(
      command
        .replace(&self.binding.source, "<mount>")
        .replace(&mounting_user()?, "<uid>:<gid>")
        .replace(
          &format!("conformance-{}/", std::process::id()),
          "conformance-<pid>/",
        ),
    )
  }
}

/// fsx inside a container over the OCI bind.
pub(crate) fn run_fsx(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let tools = run.scratch.shared("tools")?;
  // The host build fetches and verifies the pinned source; the container compiles the same file.
  let built = fetch::build_fsx(&tools)?;
  let leg = Leg::open(run, "fsx")?;
  let bounds = run.bounds();
  let script = format!(
    "cc -O2 -w -include time.h -include stdint.h -o /tmp/fsx {SOURCES}/{} && cd {} \
     && /tmp/fsx -N {} -S {} -l {} -q -P /tmp fsx.bin",
    fetch::FSX_C.name,
    Leg::workdir("fsx"),
    bounds.fsx_operations,
    bounds.fsx_seed,
    bounds.fsx_file_length
  );
  let (success, output, command) = run_in_container(&leg.binding, &tools, &script)?;
  let verdict = judge_fsx(success, &output);
  let mut notes = leg.notes(built.note);
  notes.push(
    "fsx compiled inside the container from the same pinned source; its .fsxlog/.fsxgood files kept in the container's /tmp".to_owned(),
  );
  if !verdict.ok {
    notes.push(format!("fsx output tail: {}", verdict.detail));
  }
  notes.extend(leg.session.size_note.clone());
  let command = leg.recorded(&command)?;
  drop(leg);
  Ok(SuiteResult {
    privilege: this_user().privilege(),
    outcome: Outcome::Ran {
      counts: Counts::Fsx {
        operations: bounds.fsx_operations,
        seed: bounds.fsx_seed,
        file_length: bounds.fsx_file_length,
        ok: verdict.ok,
      },
    },
    command,
    bound: format!(
      "{} operations, seed {}, file length {} bytes",
      bounds.fsx_operations, bounds.fsx_seed, bounds.fsx_file_length
    ),
    expected_failure_list: None,
    notes,
    ok: verdict.ok,
  })
}

/// fsstress inside a container over the OCI bind: LTP's pinned sources staged with the Linux shim and compiled
/// inside, run with the lane's bounds; the daemon must still answer afterwards.
pub(crate) fn run_fsstress(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let sources = run.scratch.shared("tools")?.join("ltp-linux");
  fetch::stage_fsstress(&sources, HostOs::Linux)?;
  let leg = Leg::open(run, "fsstress")?;
  let bounds = run.bounds();
  let script = format!(
    "cp -R {SOURCES} /tmp/ltp && cd /tmp/ltp && cc {} -o /tmp/fsstress {} && /tmp/fsstress -d {} -n {} -p {} -s {} -v",
    fetch::FSSTRESS_CFLAGS.join(" "),
    fetch::FSSTRESS_C.name,
    Leg::workdir("fsstress"),
    bounds.fsstress_operations,
    bounds.fsstress_processes,
    bounds.fsstress_seed
  );
  let (success, output, command) = run_in_container(&leg.binding, &sources, &script)?;
  let verdict = judge_fsstress(success, &output);
  let alive = leg.session.daemon_alive();
  let ok = verdict.ok && alive;
  let mut notes = leg.notes(format!(
    "fsstress: {} ({}), sha256 {}, compiled inside the container `cc {}` over the harness's Linux shim config.h",
    fetch::FSSTRESS_C.upstream,
    fetch::FSSTRESS_C.license,
    fetch::FSSTRESS_C.sha256,
    fetch::FSSTRESS_CFLAGS.join(" ")
  ));
  notes.push(format!(
    "the daemon answered `volume list` after the run: {alive}"
  ));
  if !verdict.ok {
    notes.push(format!("fsstress output tail: {}", verdict.detail));
  }
  if !alive {
    notes.push(format!(
      "the daemon stopped answering during the run; anchor log tail:\n{}",
      leg.session.anchor_log_tail()
    ));
  }
  notes.extend(leg.session.size_note.clone());
  let command = leg.recorded(&command)?;
  drop(leg);
  Ok(SuiteResult {
    privilege: this_user().privilege(),
    outcome: Outcome::Ran {
      counts: Counts::Fsstress {
        operations: bounds.fsstress_operations,
        processes: bounds.fsstress_processes,
        seed: bounds.fsstress_seed,
        logged_operations: verdict.logged_operations,
        disabled_operations: Vec::new(),
        ok,
      },
    },
    command,
    bound: format!(
      "{} operations per process × {} processes, seed {}",
      bounds.fsstress_operations, bounds.fsstress_processes, bounds.fsstress_seed
    ),
    expected_failure_list: None,
    notes,
    ok,
  })
}
