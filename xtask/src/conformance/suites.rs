//! The three conformance suites over a live mount (Part 6 "Conformance"; AC-3.1, AC-4.2/4.3):
//! fsx and fsstress with recorded bounds (an operation count, a seed, a process count), and
//! pjdfstest file by file against the transport's reviewed expected-failure list. Each run opens a
//! session (daemon, volume, mount), works in a directory inside the mount, and hands back the
//! typed outcome; the record's command is the exact command line the suite ran under.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slates_conformance::exerciser::{judge_fsstress, judge_fsx};
use slates_conformance::expected::{ExpectedFailures, judge};
use slates_conformance::record::{Counts, ListDigest, Outcome, Privilege};
use slates_conformance::tap::{CaseStatus, TapCase, TapFile, parse_file};
use slates_conformance::{Suite, Transport};

use super::fetch;
use super::slates::Session;
use super::{Run, SuiteResult, pause};
use crate::Failure;

/// Shape: the wall bound of one pjdfstest file; the longest (`rename/00.t`, dozens of cases each
/// spawning a process over loopback NFS) finishes in seconds, so a file past this has hung.
const PJDFSTEST_FILE_BOUND: Duration = Duration::from_secs(300);

/// A command line as the record prints it.
fn command_line(program: &Path, args: &[String]) -> String {
  let mut parts = vec![program.display().to_string()];
  parts.extend(args.iter().cloned());
  parts.join(" ")
}

/// Runs a suite binary to completion, capturing both streams together.
fn run_capturing(program: &Path, args: &[String], cwd: &Path) -> Result<(bool, String), Failure> {
  let output = Command::new(program)
    .args(args)
    .current_dir(cwd)
    .stdin(Stdio::null())
    .output()
    .map_err(|e| Failure(format!("running {}: {e}", program.display())))?;
  let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
  text.push_str(&String::from_utf8_lossy(&output.stderr));
  Ok((output.status.success(), text))
}

/// The outcome for counts: `RAN` unless the lane runs through an adapter or at reduced scope.
fn outcome_for(
  run: &Run<'_>,
  suite: Suite,
  counts: Counts,
  reduced: Option<(String, String)>,
) -> Outcome {
  match (run.adapter(suite), reduced) {
    (None, None) => Outcome::Ran { counts },
    (Some((adapter, not_covered)), None) => Outcome::Limited {
      adapter,
      not_covered,
      counts,
    },
    (None, Some((adapter, not_covered))) => Outcome::Limited {
      adapter,
      not_covered,
      counts,
    },
    (Some((lane_adapter, lane_gap)), Some((scope_adapter, scope_gap))) => Outcome::Limited {
      adapter: format!("{lane_adapter}; {scope_adapter}"),
      not_covered: format!("{lane_gap}; {scope_gap}"),
      counts,
    },
  }
}

/// fsx over the mount.
pub(crate) fn run_fsx(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let tools = run.scratch.subdir("tools")?;
  let built = fetch::build_fsx(&tools)?;
  let session = Session::open(run, "fsx", false, None)?;
  let work = session.workdir("fsx")?;
  let logs = run.scratch.subdir("fsx-logs")?;
  let bounds = run.bounds();
  let args: Vec<String> = vec![
    "-N".to_owned(),
    bounds.fsx_operations.to_string(),
    "-S".to_owned(),
    bounds.fsx_seed.to_string(),
    "-l".to_owned(),
    bounds.fsx_file_length.to_string(),
    "-q".to_owned(),
    "-P".to_owned(),
    logs.display().to_string(),
    // Relative to the work directory: fsx names its .fsxgood/.fsxlog files `<-P dir>/<fname>.…`,
    // so an absolute fname would nest one absolute path under another.
    "fsx.bin".to_owned(),
  ];
  let (success, output) = run_capturing(&built.binary, &args, &work)?;
  let verdict = judge_fsx(success, &output);
  let mut notes = vec![
    built.note,
    "the exerciser's .fsxlog/.fsxgood files were kept outside the mount (-P)".to_owned(),
  ];
  if !verdict.ok {
    notes.push(format!("fsx output tail: {}", verdict.detail));
  }
  drop(session);
  Ok(SuiteResult {
    outcome: outcome_for(
      run,
      Suite::Fsx,
      Counts::Fsx {
        operations: bounds.fsx_operations,
        seed: bounds.fsx_seed,
        file_length: bounds.fsx_file_length,
        ok: verdict.ok,
      },
      None,
    ),
    command: format!(
      "cd <mount>/conformance-<pid>/fsx && {}",
      command_line(&built.binary, &args)
    ),
    bound: format!(
      "{} operations, seed {}, file length {} bytes",
      bounds.fsx_operations, bounds.fsx_seed, bounds.fsx_file_length
    ),
    expected_failure_list: None,
    notes,
    ok: verdict.ok,
  })
}

/// fsstress over the mount.
pub(crate) fn run_fsstress(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let tools = run.scratch.subdir("tools")?.join("ltp");
  let built = fetch::build_fsstress(&tools, run.os)?;
  let session = Session::open(run, "fsstress", false, None)?;
  let work = session.workdir("fsstress")?;
  let bounds = run.bounds();
  let mut args: Vec<String> = vec![
    "-d".to_owned(),
    work.display().to_string(),
    "-n".to_owned(),
    bounds.fsstress_operations.to_string(),
    "-p".to_owned(),
    bounds.fsstress_processes.to_string(),
    "-s".to_owned(),
    bounds.fsstress_seed.to_string(),
    "-v".to_owned(),
  ];
  for operation in &built.disabled_operations {
    args.push("-f".to_owned());
    args.push(format!("{operation}=0"));
  }
  let (success, output) = run_capturing(&built.built.binary, &args, run.scratch.path())?;
  let verdict = judge_fsstress(success, &output);
  let alive = session.daemon_alive();
  let ok = verdict.ok && alive;
  let mut notes = vec![
    built.built.note,
    format!("the daemon answered `volume list` after the run: {alive}"),
  ];
  if !verdict.ok {
    notes.push(format!("fsstress output tail: {}", verdict.detail));
  }
  drop(session);
  Ok(SuiteResult {
    outcome: outcome_for(
      run,
      Suite::Fsstress,
      Counts::Fsstress {
        operations: bounds.fsstress_operations,
        processes: bounds.fsstress_processes,
        seed: bounds.fsstress_seed,
        logged_operations: verdict.logged_operations,
        disabled_operations: built.disabled_operations,
        ok,
      },
      None,
    ),
    command: command_line(&built.built.binary, &args).replace(
      &work.display().to_string(),
      "<mount>/conformance-<pid>/fsstress",
    ),
    bound: format!(
      "{} operations per process × {} processes, seed {}",
      bounds.fsstress_operations, bounds.fsstress_processes, bounds.fsstress_seed
    ),
    expected_failure_list: None,
    notes,
    ok,
  })
}

/// Every `.t` under `tests/`, sorted, as paths relative to the tree root.
fn test_files(root: &Path) -> Result<Vec<PathBuf>, Failure> {
  let mut files = Vec::new();
  let mut stack = vec![root.join("tests")];
  while let Some(dir) = stack.pop() {
    for entry in std::fs::read_dir(&dir).map_err(|e| Failure(format!("{}: {e}", dir.display())))? {
      let path = entry?.path();
      if path.is_dir() {
        stack.push(path);
      } else if path.extension().is_some_and(|x| x == "t") {
        files.push(path);
      }
    }
  }
  files.sort();
  Ok(files)
}

/// Runs one pjdfstest file inside `work` (as root through `sudo -n` when the lane has it), bounded.
fn run_test_file(file: &Path, work: &Path, as_root: bool) -> Result<(String, bool), Failure> {
  let mut command = if as_root {
    let mut c = Command::new("sudo");
    c.args(["-n", "sh"]);
    c
  } else {
    Command::new("sh")
  };
  let mut child = command
    .arg(file)
    .current_dir(work)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .map_err(|e| Failure(format!("running {}: {e}", file.display())))?;
  let stdout = child.stdout.take();
  // The pipe is drained while the file runs (a scoped thread, joined below, so it has an owner),
  // and the file is killed past its bound rather than left to hang the run.
  let outcome = std::thread::scope(|scope| {
    let reader = scope.spawn(move || {
      use std::io::Read;
      let mut text = String::new();
      if let Some(mut stdout) = stdout {
        let _ = stdout.read_to_string(&mut text);
      }
      text
    });
    let started = Instant::now();
    let mut timed_out = false;
    loop {
      match child.try_wait() {
        Ok(Some(_)) => break,
        Ok(None) if started.elapsed() < PJDFSTEST_FILE_BOUND => pause(),
        _ => {
          let _ = child.kill();
          let _ = child.wait();
          timed_out = true;
          break;
        }
      }
    }
    (reader.join().unwrap_or_default(), timed_out)
  });
  Ok(outcome)
}

/// The aggregate counts of the parsed files.
struct Tally {
  files: u32,
  cases: Vec<TapCase>,
  passed: u32,
  failed: u32,
  needs_root: u32,
  todo: u32,
  incomplete: Vec<String>,
}

fn tally(parsed: &[TapFile]) -> Tally {
  let mut out = Tally {
    files: u32::try_from(parsed.len()).unwrap_or(u32::MAX),
    cases: Vec::new(),
    passed: 0,
    failed: 0,
    needs_root: 0,
    todo: 0,
    incomplete: Vec::new(),
  };
  for file in parsed {
    if !file.complete() || file.malformed_lines > 0 {
      out.incomplete.push(format!(
        "{} (plan {:?}, {} cases, {} malformed lines)",
        file.file,
        file.plan,
        file.cases.len(),
        file.malformed_lines
      ));
    }
    for case in &file.cases {
      match case.status {
        CaseStatus::Pass => out.passed += 1,
        CaseStatus::Fail { .. } => out.failed += 1,
        CaseStatus::NeedsRoot { .. } => out.needs_root += 1,
        CaseStatus::TodoFail { .. } | CaseStatus::TodoPass => out.todo += 1,
      }
      out.cases.push(case.clone());
    }
  }
  out
}

/// The reviewed list for a transport, or an empty one when no file exists yet.
/// The list is per transport and privilege: pjdfstest's README requires root, so a root run and an
/// unprivileged run fail different cases and are judged against different lists.
fn expected_list(
  root: &Path,
  transport: Transport,
  privilege: Privilege,
) -> Result<(ExpectedFailures, ListDigest), Failure> {
  let relative = format!(
    "docs/wip/conformance/expected-failures/{}{}.pjdfstest.txt",
    transport.slug(),
    match privilege {
      Privilege::Root => "",
      Privilege::Unprivileged => ".unprivileged",
    }
  );
  let text = std::fs::read_to_string(root.join(&relative)).unwrap_or_default();
  let list = ExpectedFailures::parse(&text).map_err(|e| Failure(format!("{relative}: {e}")))?;
  let digest = ListDigest {
    path: relative,
    blake3: list.digest(),
    entries: u32::try_from(list.len()).unwrap_or(u32::MAX),
  };
  Ok((list, digest))
}

/// pjdfstest over the mount, file by file, judged against the reviewed list.
pub(crate) fn run_pjdfstest(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let tools = run.scratch.subdir("tools")?;
  let tree = fetch::build_pjdfstest(&tools)?;
  let session = Session::open(run, "pjdfstest", false, None)?;
  let work = session.workdir("pjd")?;
  let as_root = run.root_available
    && !matches!(run.privilege(), Privilege::Root)
    && super::sudo_without_prompt();
  let privilege = if run.root_available {
    Privilege::Root
  } else {
    Privilege::Unprivileged
  };
  let files = test_files(&tree.root)?;
  // Every file's raw TAP output is kept in the scratch (`--keep`), so a failure can be reviewed by
  // its own message before it is listed as expected.
  let outputs = run.scratch.subdir("pjdfstest-output")?;
  let mut parsed = Vec::with_capacity(files.len());
  let mut timed_out = Vec::new();
  for file in &files {
    let relative = file
      .strip_prefix(&tree.root)
      .unwrap_or(file)
      .display()
      .to_string();
    let (output, hung) = run_test_file(file, &work, as_root)?;
    super::write_file(
      &outputs.join(format!("{}.txt", relative.replace('/', "__"))),
      output.as_bytes(),
    )?;
    if hung {
      timed_out.push(relative.clone());
    }
    parsed.push(parse_file(&relative, &output, privilege));
  }
  drop(session);
  let tally = tally(&parsed);
  let (list, digest) = expected_list(run.root, run.transport, privilege)?;
  let judgement = judge(&list, &tally.cases);
  for id in &judgement.unlisted_failures {
    println!("pjdfstest: unlisted failure {id}");
  }
  for id in &judgement.listed_now_passing {
    println!("pjdfstest: listed but now passing {id}");
  }
  let ok = judgement.acceptable() && tally.incomplete.is_empty() && timed_out.is_empty();
  let mut notes = vec![tree.note, judgement.describe()];
  if !tally.incomplete.is_empty() {
    notes.push(format!(
      "incomplete or malformed files: {}",
      tally.incomplete.join("; ")
    ));
  }
  if !timed_out.is_empty() {
    notes.push(format!(
      "files past the {PJDFSTEST_FILE_BOUND:?} bound: {}",
      timed_out.join(", ")
    ));
  }
  let reduced = if run.root_available {
    None
  } else {
    Some((
      "a non-root run".to_owned(),
      format!(
        "{} cases that switch uid/gid or require root, counted as needs-root rather than run",
        tally.needs_root
      ),
    ))
  };
  let counts = Counts::Pjdfstest {
    files: tally.files,
    cases: u32::try_from(tally.cases.len()).unwrap_or(u32::MAX),
    passed: tally.passed,
    failed: tally.failed,
    needs_root: tally.needs_root,
    todo: tally.todo,
    expected_failures: judgement.expected_failures,
    unexpected_failures: u32::try_from(judgement.unlisted_failures.len()).unwrap_or(u32::MAX),
    listed_now_passing: u32::try_from(judgement.listed_now_passing.len()).unwrap_or(u32::MAX),
  };
  Ok(SuiteResult {
    outcome: outcome_for(run, Suite::Pjdfstest, counts, reduced),
    command: format!(
      "{}sh <each of {} tests/**/*.t of {}> in a directory inside the mount; binary {}",
      if as_root { "sudo -n " } else { "" },
      files.len(),
      fetch::PJDFSTEST_TARBALL.upstream,
      tree.binary.display()
    ),
    bound: format!(
      "every test file at the pinned commit ({} files), {PJDFSTEST_FILE_BOUND:?} per file",
      files.len()
    ),
    expected_failure_list: Some(digest),
    notes,
    ok,
  })
}
