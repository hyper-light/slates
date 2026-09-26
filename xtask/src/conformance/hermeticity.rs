//! The hermeticity tracer run (R1; Part 6 example 8; AC-4.5, AC-9.4; the dynamic half of the
//! structural test): a whole lifecycle — anchor, daemon, a volume, a kernel mount, files written
//! through the mount, a snapshot, a landing planned, granted by the human surface and executed —
//! runs under a filesystem-write tracer, and every write-capable call the slates processes made
//! must fall inside the granted target (matched to what the landing reports written), on a
//! RAM-only kernel object, or on the processes' own standard streams. On Linux the tracer is
//! `strace -f -y` wrapping the anchor, so the anchor, the daemon and every child are in the log;
//! on macOS it is `sudo eslogger` (Apple's Endpoint Security events, each file named by its full kernel
//! path), kept to the events of the slates binary — anchor, daemon and every CLI call — from before the
//! anchor starts until after it stops. It needs root and Full Disk Access for whatever runs it. DTrace's
//! syscall provider is absent under SIP, and fs_usage could not attribute descriptors it saw duplicated
//! (92 of 93 calls unresolved on CI, 2026-09-26).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use slates_conformance::Suite;
use slates_conformance::capability::HostOs;
use slates_conformance::record::Outcome;
use slates_conformance::trace::{
  Policy, eslogger_has_activity, judge, parse_eslogger, parse_strace_with_cwd, strace_unnamed_paths,
};
use slates_conformance::workload::Manifest;

use super::slates::{MOUNT_WAIT, Session, SlatesBinary, grant};
use super::trace_process::{StopSignal, TraceProcess};
use super::workloads::manifest_of;
use super::{Run, SuiteResult, write_file};
use crate::Failure;

/// Format: the system calls strace traces: every call taking a path (`%file`) plus the
/// descriptor-based writes the structural test's syscall list names.
const STRACE_TRACE: &str = "trace=%file,write,pwrite64,writev,pwritev,pwritev2,ftruncate,fchmod,fchown,fsync,fdatasync,fallocate,copy_file_range,sendfile,splice,vmsplice";
// `futimens` is glibc's wrapper over `utimensat` (a %file syscall), not a syscall name: strace refused the
// whole run with `invalid system call 'futimens'` and the daemon never came up under it (the Linux lane,
// 2026-09-16).
/// Format: the prefix of the landing engine's hidden siblings inside the target (`slates-land`).
const HIDDEN_PREFIX: &str = ".slates-";
/// Format: the files the traced workload leaves in the volume, relative paths, after its moves —
/// what a complete landing must write.
const LANDED_ENTRIES: &[&str] = &["d", "d/f2", "f3", "link"];
/// Format: the workload driven through the mount: create, write, mkdir, symlink, rename, chmod, remove.
const MOUNT_WORKLOAD: &str = "printf 'one\\n' > f1 && mkdir d && printf 'two\\n' > d/f2 && ln -s f3 link && mv f1 f3 && chmod u=rw,go=r f3 && printf 'gone\\n' > tmp && rm tmp";

/// The tracer's command prefix for the anchor on Linux.
pub(super) fn strace_prefix(log: &Path) -> Vec<String> {
  vec![
    "strace".to_owned(),
    // Select ptrace stops as well as output: stopping at every allocator syscall starved the
    // first heartbeat in CI. The traced write set below remains unchanged (strace(1)).
    "--seccomp-bpf".to_owned(),
    // Anchor owns this tracer child. Its exit must end the anchor and daemon it supervises,
    // including if the kernel cannot install the syscall filter (strace(1), PTRACE_O_EXITKILL).
    "--kill-on-exit".to_owned(),
    "-f".to_owned(),
    "-y".to_owned(),
    "-qq".to_owned(),
    "-s".to_owned(),
    "0".to_owned(),
    "-o".to_owned(),
    log.display().to_string(),
    "-e".to_owned(),
    STRACE_TRACE.to_owned(),
    "--".to_owned(),
  ]
}

/// Format: the Endpoint Security events the macOS tracer asks `eslogger` for: every event that can
/// create, write, move, remove or re-attribute a file (`eslogger --list-events`), and `open`/`close`,
/// whose write flag and modified bit the parser reads.
const ES_EVENTS: &[&str] = &[
  "open",
  "close",
  "create",
  "write",
  "truncate",
  "rename",
  "unlink",
  "link",
  "clone",
  "copyfile",
  "exchangedata",
  "setextattr",
  "deleteextattr",
  "setmode",
  "setowner",
  "setflags",
  "utimes",
  "setattrlist",
  "setacl",
];

/// A running `sudo eslogger` over the whole machine, its events kept only when a slates process made
/// them (the event's `process.executable.path` is the built binary: the anchor, the daemon and every CLI
/// invocation), written to a log; stopped on drop. The filter runs as the events arrive, on a thread
/// this value owns and joins, so the log holds the lifecycle's own events, not the machine's.
struct EsLogger {
  process: TraceProcess,
  filter: Option<std::thread::JoinHandle<Result<(), Failure>>>,
  errors: PathBuf,
}

impl EsLogger {
  fn start(binary: &Path, log: &Path) -> Result<EsLogger, Failure> {
    let errors = log.with_extension("stderr.log");
    #[allow(clippy::disallowed_methods)] // the development tool's own scratch diagnostics
    let stderr = std::fs::File::create(&errors)?;
    let mut command = Command::new("sudo");
    command
      .arg("-n")
      .arg("eslogger")
      .args(ES_EVENTS)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::from(stderr));
    isolate_tracer(&mut command);
    let mut child = command
      .spawn()
      .map_err(|e| Failure(format!("starting eslogger: {e}")))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| Failure("eslogger has no stdout".to_owned()))?;
    #[allow(clippy::disallowed_methods)] // the development tool's own scratch log
    let file = std::fs::File::create(log)
      .map_err(|e| Failure(format!("creating {}: {e}", log.display())))?;
    let binary = binary.display().to_string();
    let filter = std::thread::Builder::new()
      .name("eslogger-filter".to_owned())
      .spawn(move || keep_slates_events(stdout, file, &binary))
      .map_err(|e| Failure(format!("starting the eslogger filter: {e}")))?;
    Ok(EsLogger {
      process: TraceProcess::new(child, signal_eslogger, MOUNT_WAIT),
      filter: Some(filter),
      errors,
    })
  }

  /// Waits until a slates process's event reached the log: `run` executes the binary, whose own
  /// loading and reads are events.
  fn wait_ready(
    &mut self,
    log: &Path,
    mut run: impl FnMut() -> Result<(), Failure>,
  ) -> Result<(), Failure> {
    let ready = self.process.wait_ready(|| {
      run()?;
      Ok(eslogger_has_activity(&std::fs::read_to_string(log)?))
    });
    self.check(ready)
  }

  fn stop(mut self) -> Result<(), Failure> {
    let stopped = self.process.stop_accepting(terminated_by_term);
    let filtered = self
      .filter
      .take()
      .map_or(Ok(()), |filter| match filter.join() {
        Ok(result) => result,
        Err(_) => Err(Failure("the eslogger filter panicked".to_owned())),
      });
    self.check(stopped.and(filtered))
  }

  fn check(&self, result: Result<(), Failure>) -> Result<(), Failure> {
    let diagnostic = std::fs::read_to_string(&self.errors)?;
    match result {
      Err(error) => Err(Failure(format!(
        "{error}; eslogger: {} (it needs root and Full Disk Access for the terminal that runs it)",
        diagnostic.trim()
      ))),
      Ok(()) => Ok(()),
    }
  }
}

/// Copies to `out` every event line whose process is `binary`, and every line that is not a complete
/// event (the parser records it unresolved, so a torn line cannot hide a write).
fn keep_slates_events(
  events: std::process::ChildStdout,
  mut out: std::fs::File,
  binary: &str,
) -> Result<(), Failure> {
  use std::io::{BufRead, Write};
  for line in std::io::BufReader::new(events).lines() {
    let line = line?;
    let keep = match serde_json::from_str::<serde_json::Value>(&line) {
      Ok(event) => {
        event.get("event").is_none()
          || event["process"]["executable"]["path"].as_str() == Some(binary)
      }
      Err(_) => !line.trim().is_empty(),
    };
    if keep {
      writeln!(out, "{line}")?;
    }
  }
  out.flush()?;
  Ok(())
}

/// `eslogger` ends on `SIGTERM` (it ignored `SIGINT` in the 2026-09-26 probe), so its terminal status is
/// that signal's, which `sudo` reports as an exit of 128 + 15.
fn terminated_by_term(status: std::process::ExitStatus) -> bool {
  /// Format: `SIGTERM`, and the exit code a shell or `sudo` reports for a child it ended (128 + 15).
  const SIGTERM: i32 = 15;
  const EXIT_ON_SIGTERM: i32 = 128 + SIGTERM;
  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    if status.signal() == Some(SIGTERM) {
      return true;
    }
  }
  status.success() || status.code() == Some(EXIT_ON_SIGTERM)
}

/// Signals the `sudo eslogger` process group: its graceful stop is `SIGTERM`.
fn signal_eslogger(pid: u32, signal: StopSignal) -> Result<(), Failure> {
  signal_group(
    pid,
    match signal {
      StopSignal::Interrupt => "TERM",
      StopSignal::Kill => "KILL",
    },
  )
}

#[cfg(unix)]
fn isolate_tracer(command: &mut Command) {
  use std::os::unix::process::CommandExt;
  command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_tracer(_command: &mut Command) {}

/// sudo may supervise a distinct tracer child. Signal their owned process group so an early return
/// cannot leave the privileged descendant tracing after its target is gone.
fn signal_group(pid: u32, signal: &str) -> Result<(), Failure> {
  let output = Command::new("sudo")
    .args(["-n", "/bin/kill", "-s", signal, "--", &format!("-{pid}")])
    .output()?;
  if !output.status.success() {
    return Err(Failure(format!(
      "signalling tracer group {pid}: {}: {}",
      output.status,
      String::from_utf8_lossy(&output.stderr).trim(),
    )));
  }
  Ok(())
}

/// The landing flow through the CLI: snapshot, plan, grant, land; returns the written count.
fn land_under_grant(run: &Run<'_>, session: &Session, target: &Path) -> Result<u64, Failure> {
  let snapshot = session
    .binary
    .run(
      &session.instance,
      &["volume", "snapshot", &session.volume_id],
    )?
    .expect_ok("volume snapshot")?
    .value_of("snapshot")?;
  let target_text = target.display().to_string();
  let presented = session
    .binary
    .run(
      &session.instance,
      &[
        "land",
        &session.volume_id,
        &target_text,
        "--snapshot",
        &snapshot,
        "--json",
      ],
    )?
    .expect_ok("land (plan)")?
    .json()?;
  if presented["grant_required"] != true {
    return Err(Failure(format!(
      "the landing was not presented for a grant: {presented}"
    )));
  }
  let landing = presented["landing"]
    .as_u64()
    .ok_or_else(|| Failure(format!("no landing id: {presented}")))?;
  let manifest = presented["manifest"]
    .as_str()
    .ok_or_else(|| Failure(format!("no manifest: {presented}")))?
    .to_owned();
  let daemon_pid = session.anchor.daemon_pid()?;
  let grant_id = grant(
    run,
    &session.binary,
    &session.instance,
    daemon_pid,
    landing,
    &manifest,
  )?;
  let landed = session
    .binary
    .run(
      &session.instance,
      &[
        "land",
        &session.volume_id,
        &target_text,
        "--snapshot",
        &snapshot,
        "--grant",
        &grant_id.to_string(),
        "--json",
      ],
    )?
    .expect_ok("land (granted)")?
    .json()?;
  landed["outcome"]["written"].as_u64().ok_or_else(|| {
    Failure(format!(
      "the granted landing reports no written count: {landed}"
    ))
  })
}

/// The mounted workload must reach disk, including its scratch-volume parents. A trace with
/// no outside writes proves nothing if the landing silently skipped every entry (AC-4.5).
fn verify_landing(target: &Path, work: &str) -> Result<(), Failure> {
  let directory = target.join(work);
  for relative in ["", "d"] {
    let path = directory.join(relative);
    if !std::fs::symlink_metadata(&path)?.is_dir() {
      return Err(Failure(format!(
        "landed directory has the wrong kind: {}",
        path.display()
      )));
    }
  }
  for (relative, expected) in [("f3", b"one\n"), ("d/f2", b"two\n")] {
    let path = directory.join(relative);
    if !std::fs::symlink_metadata(&path)?.is_file() || std::fs::read(&path)? != expected {
      return Err(Failure(format!(
        "landed file has the wrong kind or bytes: {}",
        path.display()
      )));
    }
  }
  if std::fs::read_link(directory.join("link"))? != Path::new("f3") {
    return Err(Failure("landed symlink does not name f3".to_owned()));
  }
  Ok(())
}

/// The complete mounted namespace is the independent oracle for what the landing must write.
fn verify_manifest(mounted: &Manifest, landed: &Manifest) -> Result<(), Failure> {
  if mounted != landed {
    return Err(Failure(format!(
      "the landed tree differs from the complete mounted tree: mounted={mounted:?}; landed={landed:?}"
    )));
  }
  Ok(())
}

/// Runs the traced lifecycle and judges the log.
pub(crate) fn run_hermeticity(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let log = run.scratch.path().join("trace.log");
  let tracer = match run.os {
    HostOs::Linux => Some(strace_prefix(&log)),
    HostOs::Macos => None,
    HostOs::Windows => return Err(Failure("no Windows tracer".to_owned())),
  };
  // Derived: one queried host page per peak workload entry. The macOS NFS client adds a
  // 4096-byte AppleDouble file to each name; on the measured 16-KiB-page host, six surviving
  // sidecars cost 98304 bytes (§4.2's allocated-page charge), plus eight inline workload bytes.
  // Counting only Rust inodes admitted 2112 bytes and refused both the metadata and d/f2.
  // Keep this fixture small enough to leave the shard's unpromised versions for its snapshot.
  let inode_count = LANDED_ENTRIES.len() + 2 + 1 + 1;
  let page = slates_machine::facts::Facts::query().page.base;
  if page == 0 {
    return Err(Failure("the OS did not report its page size".to_owned()));
  }
  let size = u64::try_from(inode_count)
    .ok()
    .and_then(|count| count.checked_mul(page))
    .ok_or_else(|| Failure("hermeticity workload quota overflow".to_owned()))?
    .to_string();
  // macOS: eslogger watches the whole lifecycle from before the anchor starts (daemon start-up
  // included), attached once the binary's own events reach the log.
  let eslogger = match run.os {
    HostOs::Macos => {
      let binary = SlatesBinary::build(run.root)?;
      let mut tracer = EsLogger::start(binary.path(), &log)?;
      tracer.wait_ready(&log, || {
        binary.run("eslogger-readiness", &["--help"]).map(|_| ())
      })?;
      eprintln!("hermeticity: eslogger recorded the binary's events");
      Some(tracer)
    }
    _ => None,
  };
  let session = Session::open(run, "hermeticity", &size, false, tracer.as_deref())?;
  let work = session.workdir("traced")?;
  let workload = Command::new("sh")
    .args(["-c", MOUNT_WORKLOAD])
    .current_dir(&work)
    .output()?;
  if !workload.status.success() {
    let inspection = match Command::new("ls").arg("-laR").arg(&work).output() {
      Ok(output) => format!(
        "{}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
      ),
      Err(error) => format!("listing the failed workload: {error}"),
    };
    return Err(Failure(format!(
      "the mount workload failed (quota {size} bytes, {inode_count} peak entries × {page}-byte page): {}\nmounted tree: {inspection}",
      String::from_utf8_lossy(&workload.stderr),
    )));
  }
  let parent = format!("conformance-{}", std::process::id());
  let work = format!("{parent}/traced");
  // The known workload bytes/kinds must be correct independently of the observed manifest.
  // The manifest then adds every client-created entry; it has no filename exclusions.
  verify_landing(session.mount.path(), &work)?;
  let mounted_tree = manifest_of(session.mount.path())?;
  let expected = mounted_tree
    .entries
    .iter()
    .map(|entry| entry.path.clone())
    .collect::<Vec<_>>();
  let target = run.scratch.subdir("land-target")?;
  let target = std::fs::canonicalize(&target)?;
  let written = land_under_grant(run, &session, &target)?;
  let verified = verify_landing(&target, &work)
    .and_then(|()| verify_manifest(&mounted_tree, &manifest_of(&target)?));
  let Session { mount, anchor, .. } = session;
  let anchor_log = anchor.log_path().display().to_string();
  drop(mount);
  // Readiness is already established: keep observing through daemon teardown as well.
  anchor.stop();
  if let Some(tracer) = eslogger {
    tracer.stop()?;
  }
  let text = std::fs::read_to_string(&log)
    .map_err(|e| Failure(format!("reading {}: {e}", log.display())))?;
  let cwd = run.scratch.path().display().to_string();
  let events = match run.os {
    HostOs::Linux => parse_strace_with_cwd(&text, &cwd),
    _ => parse_eslogger(&text),
  };
  let streams = [anchor_log.as_str()];
  let policy = Policy {
    target: &target.display().to_string(),
    working_directory: &cwd,
    streams: &streams,
  };
  let judged = judge(&events, &policy);
  let unnamed = strace_unnamed_paths(&text);
  let mut matched = 0u32;
  let mut unmatched = Vec::new();
  let mut hidden = 0u32;
  for path in &judged.written_inside {
    if unnamed
      .iter()
      .any(|unnamed| unnamed == &format!("{}/{path}", target.display()))
      || path
        .rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with(HIDDEN_PREFIX))
    {
      hidden += 1;
    } else if expected.contains(path) {
      matched += 1;
    } else {
      unmatched.push(path.clone());
    }
  }
  let complete = usize::try_from(written).ok() == Some(expected.len())
    && usize::try_from(matched).ok() == Some(expected.len())
    && verified.is_ok();
  let ok = complete && judged.outside == 0 && judged.unresolved == 0 && unmatched.is_empty();
  let mut notes = vec![
    format!(
      "tracer: {}; {} events parsed from {} log lines; landing reported {written} written; {hidden} hidden siblings ({HIDDEN_PREFIX}*) seen inside the target",
      match run.os {
        HostOs::Linux => format!("strace -f -y -qq -s 0 -e {STRACE_TRACE} -- <anchor>"),
        _ => format!(
          "sudo eslogger {} (kept: events of the slates binary)",
          ES_EVENTS.join(" ")
        ),
      },
      events.len(),
      text.lines().count()
    ),
    format!("the granted target: {}", target.display()),
    format!(
      "volume quota: {size} bytes, derived from {inode_count} peak workload entries × {page}-byte host page"
    ),
    format!(
      "landing completeness: {written}/{} entries reported; disk verification: {verified:?}",
      expected.len()
    ),
  ];
  notes.extend(session.size_note.clone());
  if !unmatched.is_empty() {
    notes.push(format!(
      "inside-target paths not among the landed entries: {}",
      unmatched.join(", ")
    ));
  }
  for violation in &judged.violations {
    notes.push(format!(
      "VIOLATION line {}: {} {}",
      violation.line, violation.call, violation.path
    ));
  }
  if !judged.unresolved_sample.is_empty() {
    notes.push(format!(
      "unresolved sample: {}",
      judged
        .unresolved_sample
        .iter()
        .map(|e| format!("{}:{} {}", e.line, e.call, e.path))
        .collect::<Vec<_>>()
        .join("; ")
    ));
  }
  let kept = run.scratch.path().join("trace-judgement.txt");
  write_file(&kept, format!("{judged:#?}").as_bytes())?;
  let counts = judged.counts(matched, u32::try_from(unmatched.len()).unwrap_or(u32::MAX));
  Ok(SuiteResult {
    privilege: super::suites::this_user().privilege(),
    outcome: match run.adapter(Suite::Hermeticity) {
      Some((adapter, not_covered)) => Outcome::Limited {
        adapter,
        not_covered,
        counts,
      },
      None => Outcome::Ran { counts },
    },
    command: match run.os {
      HostOs::Linux => format!(
        "strace --seccomp-bpf --kill-on-exit -f -y -qq -s 0 -o trace.log -e {STRACE_TRACE} -- slates --instance <i> anchor --quick --shards 2; then create, mount, `sh -c '{MOUNT_WORKLOAD}'`, snapshot, land, grant, land --grant"
      ),
      _ => format!(
        "sudo eslogger {} (events of the slates binary); slates anchor/volume create/mount, `sh -c '{MOUNT_WORKLOAD}'`, snapshot, land, grant, land --grant",
        ES_EVENTS.join(" ")
      ),
    },
    bound: "one lifecycle: one volume, one mount; known workload bytes/kinds and the entire mounted tree, including client metadata, landed and verified".to_owned(),
    expected_failure_list: None,
    notes,
    ok,
  })
}

/// The log path a run used (for a caller that keeps the scratch).
#[allow(dead_code)]
pub(crate) fn log_path(scratch: &Path) -> PathBuf {
  scratch.join("trace.log")
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_conformance::workload::{Entry, EntryKind};

  /// AC-4.5: client-generated entries must land with the same names, kinds, modes and bytes.
  #[test]
  fn landing_cannot_drop_add_or_change_a_client_metadata_file() {
    let mounted = Manifest {
      entries: vec![Entry {
        path: "._file".to_owned(),
        kind: EntryKind::File,
        mode: 0o600,
        size: 4,
        digest: blake3::hash(b"meta").to_hex().to_string(),
      }],
    };
    assert!(verify_manifest(&mounted, &mounted).is_ok());
    assert!(
      verify_manifest(&mounted, &Manifest::default()).is_err(),
      "missing metadata"
    );
    assert!(
      verify_manifest(&Manifest::default(), &mounted).is_err(),
      "extra metadata"
    );
    let mut changed = mounted.clone();
    changed.entries[0].digest = blake3::hash(b"lost").to_hex().to_string();
    assert!(
      verify_manifest(&mounted, &changed).is_err(),
      "changed metadata bytes"
    );
  }
}
