//! The hermeticity tracer run (R1; Part 6 example 8; AC-4.5, AC-9.4; the dynamic half of the
//! structural test): a whole lifecycle — anchor, daemon, a volume, a kernel mount, files written
//! through the mount, a snapshot, a landing planned, granted by the human surface and executed —
//! runs under a filesystem-write tracer, and every write-capable call the slates processes made
//! must fall inside the granted target (matched to what the landing reports written), on a
//! RAM-only kernel object, or on the processes' own standard streams. On Linux the tracer is
//! `strace -f -y` wrapping the anchor, so the anchor, the daemon and every child are in the log;
//! on macOS it is `sudo fs_usage` on the daemon's pid (the only writer by design; fs_usage cannot
//! separate same-named processes), which needs root, so this laptop records a privilege skip and
//! the CI macOS runner runs it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use slates_conformance::Suite;
use slates_conformance::capability::HostOs;
use slates_conformance::record::Outcome;
use slates_conformance::trace::{
  Policy, judge, parse_fs_usage, parse_strace_with_cwd, strace_unnamed_paths,
};

use super::slates::{Session, grant};
use super::{Run, SuiteResult, pause, write_file};
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

/// A running `sudo fs_usage` on one pid, writing to a log; stopped on drop.
struct FsUsage {
  child: Child,
}

impl FsUsage {
  fn start(pid: u32, log: &Path) -> Result<FsUsage, Failure> {
    #[allow(clippy::disallowed_methods)] // the development tool's own scratch log
    let file = std::fs::File::create(log)
      .map_err(|e| Failure(format!("creating {}: {e}", log.display())))?;
    let child = Command::new("sudo")
      .args([
        "-n",
        "fs_usage",
        "-w",
        "-f",
        "filesys",
        "-f",
        "network",
        &pid.to_string(),
      ])
      .stdin(Stdio::null())
      .stdout(Stdio::from(file))
      .stderr(Stdio::inherit())
      .spawn()
      .map_err(|e| Failure(format!("starting fs_usage: {e}")))?;
    Ok(FsUsage { child })
  }

  fn stop(mut self) {
    let _ = Command::new("sudo")
      .args(["-n", "kill", "-INT", &self.child.id().to_string()])
      .output();
    let _ = self.child.wait();
  }
}

/// Shape: how long the tracer gets to flush after the lifecycle ends before the log is read.
const TRACER_SETTLE_POLLS: u32 = 25;

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

/// Runs the traced lifecycle and judges the log.
pub(crate) fn run_hermeticity(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let log = run.scratch.path().join("trace.log");
  let tracer = match run.os {
    HostOs::Linux => Some(strace_prefix(&log)),
    HostOs::Macos => None,
    HostOs::Windows => return Err(Failure("no Windows tracer".to_owned())),
  };
  // The trace creates four surviving entries, their two parents, the root and one temporary
  // file. Reserve those eight inodes by the admission formula (§4.2), leaving the shard's
  // unpromised versions available for the snapshot taken before landing. The general stress
  // volume's byte quota can reserve the entire inode slab and legitimately refuse retention.
  let inode_count = LANDED_ENTRIES.len() + 2 + 1 + 1;
  let size = (inode_count * std::mem::size_of::<slates_vfs::inode::Inode>()).to_string();
  let session = Session::open(run, "hermeticity", &size, false, tracer.as_deref())?;
  let fs_usage = match run.os {
    HostOs::Macos => Some(FsUsage::start(session.anchor.daemon_pid()?, &log)?),
    _ => None,
  };
  let work = session.workdir("traced")?;
  let workload = Command::new("sh")
    .args(["-c", MOUNT_WORKLOAD])
    .current_dir(&work)
    .output()?;
  if !workload.status.success() {
    return Err(Failure(format!(
      "the mount workload failed: {}",
      String::from_utf8_lossy(&workload.stderr)
    )));
  }
  let target = run.scratch.subdir("land-target")?;
  let target = std::fs::canonicalize(&target)?;
  let written = land_under_grant(run, &session, &target)?;
  let parent = format!("conformance-{}", std::process::id());
  let work = format!("{parent}/traced");
  let expected = std::iter::once(parent)
    .chain(std::iter::once(work.clone()))
    .chain(LANDED_ENTRIES.iter().map(|entry| format!("{work}/{entry}")))
    .collect::<Vec<_>>();
  let verified = verify_landing(&target, &work);
  let Session { mount, anchor, .. } = session;
  drop(mount);
  anchor.stop();
  if let Some(tracer) = fs_usage {
    tracer.stop();
  }
  for _ in 0..TRACER_SETTLE_POLLS {
    pause();
  }
  let text = std::fs::read_to_string(&log)
    .map_err(|e| Failure(format!("reading {}: {e}", log.display())))?;
  let cwd = run.scratch.path().display().to_string();
  let events = match run.os {
    HostOs::Linux => parse_strace_with_cwd(&text, &cwd),
    _ => parse_fs_usage(&text),
  };
  let policy = Policy {
    target: &target.display().to_string(),
    working_directory: &cwd,
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
        _ => "sudo fs_usage -w -f filesys -f network <daemon pid>".to_owned(),
      },
      events.len(),
      text.lines().count()
    ),
    format!("the granted target: {}", target.display()),
    format!("volume quota: {size} bytes, derived from {inode_count} workload inodes"),
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
        "sudo fs_usage -w -f filesys -f network <daemon pid>; slates anchor/volume create/mount, `sh -c '{MOUNT_WORKLOAD}'`, snapshot, land, grant, land --grant"
      ),
    },
    bound: "one lifecycle: one volume, one mount, four workload entries and their two parent directories landed and verified".to_owned(),
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
