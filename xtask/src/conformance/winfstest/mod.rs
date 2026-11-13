//! winfstest over the slates WinFsp drive (A-209; Part 6 "Conformance" on Windows; Phase 4 task 5, "WinFsp's own tests
//! on Windows"). The volume is mounted at a drive letter (`slates mount ID E:`), winfstest's driver (`winfstest.c`) is
//! built from its pinned source with MSVC, and the twenty scripts' Rust transcription ([`scripts`]) runs in a fresh
//! directory on the drive, each script's calls made by the driver one process a call, as `winfstest.py` makes them.
//! Each script's TAP output is kept in the scratch (`winfstest-output/`, the review input, as pjdfstest's is) and
//! judged by the same reader, tally and rule as pjdfstest's: every failure must be on the transport's reviewed list
//! (`docs/wip/conformance/expected-failures/native-windows-winfsp.winfstest.txt`), every listed case must still fail,
//! and every script must finish with its plan.
//!
//! Why a transcription: upstream drives the scripts from Python 2.7, which no CI runner carries and which CLAUDE.md's
//! banned item 13 keeps out of CI. The transcription is held to the unchanged upstream scripts by
//! `the_transcription_makes_upstreams_calls_and_cases`: the scripts were run once under a stand-in `winfstest.py` that
//! records each call, each held call and its release, each expectation and each `testeval` line (733 cases, 1,310
//! events at 6ac65cd; `golden.tsv`), and the transcription must make the same, in order, under a stand-in driver.

mod script;
mod scripts;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slates_conformance::record::Counts;
use slates_conformance::tap::{Runner, parse_file};
use slates_conformance::{Suite, Transport};

use self::script::{Driver, Held, Script};
use super::{Run, SuiteResult, pause, write_file};
use crate::Failure;

/// Derived: the wall bound of one winfstest call: pjdfstest's bound for a whole test file
/// (`slates_conformance::pjdfstest::FILE_BOUND_SECONDS`), since one Win32 call does less than a file of pjdfstest's
/// hundreds; a call still running past it is a wedged drive, and ends its script.
const CALL_BOUND: Duration = Duration::from_secs(slates_conformance::pjdfstest::FILE_BOUND_SECONDS);

/// Format: the reviewed list's path for a transport (`<transport>.winfstest.txt`).
fn list_path(transport: Transport) -> String {
  format!(
    "docs/wip/conformance/expected-failures/{}.winfstest.txt",
    transport.slug()
  )
}

/// A process's standard stream read to its end on a thread the caller joins.
type Drain = std::thread::JoinHandle<std::io::Result<Vec<u8>>>;

fn drain<R: Read + Send + 'static>(stream: Option<R>) -> Result<Drain, Failure> {
  let mut stream = stream.ok_or_else(|| Failure("a winfstest stream was not piped".to_owned()))?;
  std::thread::Builder::new()
    .name("winfstest-stream".to_owned())
    .spawn(move || {
      let mut bytes = Vec::new();
      stream.read_to_end(&mut bytes).map(|_| bytes)
    })
    .map_err(|e| Failure(format!("starting a winfstest stream's reader: {e}")))
}

/// Joins a drain once it has finished, within `bound` from `started`; a drain still open then is refused.
fn joined(drain: Drain, started: Instant, bound: Duration) -> Result<Vec<u8>, Failure> {
  while !drain.is_finished() {
    if started.elapsed() > bound {
      return Err(Failure(format!(
        "a winfstest call printed no end within {bound:?}"
      )));
    }
    pause();
  }
  drain
    .join()
    .map_err(|_| Failure("a winfstest stream's reader panicked".to_owned()))?
    .map_err(|e| Failure(format!("reading a winfstest stream: {e}")))
}

/// Waits for `child` within `bound` from `started`, killing it past the bound.
fn waited(child: &mut Child, started: Instant, bound: Duration) -> Result<i32, Failure> {
  loop {
    if let Some(status) = child.try_wait()? {
      return Ok(status.code().unwrap_or(-1));
    }
    if started.elapsed() > bound {
      let _ = child.kill();
      let _ = child.wait();
      return Err(Failure(format!(
        "a winfstest call did not end within {bound:?}"
      )));
    }
    pause();
  }
}

/// The real driver: `winfstest.exe` run in the scripts' directory on the drive.
struct WinfstestDriver {
  exe: PathBuf,
  work: PathBuf,
  /// Every held call still open, by [`Held`]'s index; each is released or, on drop, ended.
  held: Vec<Option<Child>>,
}

impl WinfstestDriver {
  fn command(&self, args: &[&str]) -> Command {
    let mut command = Command::new(&self.exe);
    command
      .args(args)
      .current_dir(&self.work)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped());
    command
  }
}

impl Driver for WinfstestDriver {
  fn call(&mut self, args: &[&str]) -> Result<String, Failure> {
    let started = Instant::now();
    let mut child = self
      .command(args)
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|e| Failure(format!("starting winfstest: {e}")))?;
    // The input `winfstest.py` pipes and closes at the call's end; a call without `-w` never reads it.
    drop(child.stdin.take());
    let stdout = drain(child.stdout.take())?;
    let stderr = drain(child.stderr.take())?;
    let code = waited(&mut child, started, CALL_BOUND)?;
    let stdout = joined(stdout, started, CALL_BOUND)?;
    let stderr = joined(stderr, started, CALL_BOUND)?;
    if code != 0 {
      return Err(Failure(format!(
        "winfstest {} exited {code}: {}",
        args.join(" "),
        String::from_utf8_lossy(&stderr).trim()
      )));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
  }

  fn hold(&mut self, args: &[&str]) -> Result<(String, Held), Failure> {
    let started = Instant::now();
    let mut with_wait = vec!["-w"];
    with_wait.extend_from_slice(args);
    let mut child = self
      .command(&with_wait)
      .stderr(Stdio::null())
      .spawn()
      .map_err(|e| Failure(format!("starting winfstest -w: {e}")))?;
    // `winfstest.c` closes its output before it waits for the newline, so the output ends while the handle is held.
    let stdout = match drain(child.stdout.take()).and_then(|d| joined(d, started, CALL_BOUND)) {
      Ok(stdout) => stdout,
      Err(failure) => {
        let _ = child.kill();
        let _ = child.wait();
        return Err(failure);
      }
    };
    self.held.push(Some(child));
    let index = self.held.len().saturating_sub(1);
    Ok((String::from_utf8_lossy(&stdout).into_owned(), Held(index)))
  }

  fn release(&mut self, held: Held) -> Result<(), Failure> {
    let mut child = self
      .held
      .get_mut(held.0)
      .and_then(Option::take)
      .ok_or_else(|| Failure(format!("no held winfstest call {}", held.0)))?;
    let started = Instant::now();
    if let Some(mut input) = child.stdin.take() {
      // A call that has already left refuses the newline; its exit below is the verdict, as `__exit__`'s IOError is
      // passed over.
      let _ = input.write_all(b"\n");
    }
    let code = waited(&mut child, started, CALL_BOUND)?;
    if code == 0 {
      Ok(())
    } else {
      Err(Failure(format!("a held winfstest call exited {code}")))
    }
  }

  fn opens(&self, relative: &str) -> bool {
    std::fs::File::open(self.work.join(relative)).is_ok()
  }

  fn absolute(&self, relative: &str) -> String {
    format!("{}\\{relative}", self.work.display())
  }
}

impl Drop for WinfstestDriver {
  fn drop(&mut self) {
    for mut child in self.held.drain(..).flatten() {
      let _ = child.kill();
      let _ = child.wait();
    }
  }
}

/// The scripts run over a driver: each script's TAP output (by its upstream path) and its case count.
fn run_scripts(driver: &mut dyn Driver) -> Vec<(&'static str, String)> {
  let mut names = 0u32;
  let mut outputs = Vec::new();
  for transcribed in scripts::SCRIPTS {
    let mut script = Script::new(driver, &mut names);
    (transcribed.run)(&mut script);
    let (tap, _cases, _ended) = script.finish();
    // The kept output names the upstream file its script was transcribed from (a comment line to the TAP reader).
    let header = format!(
      "# {} at billziss-gh/secfs.test@{}, sha256 {}\n",
      transcribed.path,
      super::fetch::SECFS_TEST_COMMIT,
      transcribed.sha256
    );
    outputs.push((transcribed.path, header + &tap));
  }
  outputs
}

/// The file a script's output is kept in: its upstream path with `/` as `_` (`t_base_00.t.out`).
fn output_name(path: &str) -> String {
  format!("{}.out", path.replace('/', "_"))
}

/// winfstest over the transport's drive, judged against its reviewed list.
pub(crate) fn run_winfstest(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let tools = run.scratch.shared("tools")?;
  let built = build(&tools.join("winfstest"))?;
  let session = super::slates::Session::open(run, "winfstest", super::VOLUME_SIZE, false, None)?;
  let work = session.workdir("winfstest")?;
  let kept = run.scratch.fresh("winfstest-output")?;
  let mut driver = WinfstestDriver {
    exe: built.binary.clone(),
    work: work.clone(),
    held: Vec::new(),
  };
  let outputs = run_scripts(&mut driver);
  drop(driver);
  let mut parsed = Vec::new();
  for (path, tap) in &outputs {
    write_file(&kept.join(output_name(path)), tap.as_bytes())?;
    parsed.push(parse_file(path, tap, &Runner::Root));
  }
  let (alive, live_note) = session.alive_and_shipped();
  let mut notes = vec![
    built.note,
    format!(
      "the scripts: a Rust transcription of winfstest's {} scripts at billziss-gh/secfs.test@{} (each upstream file's \
       sha256 in xtask/src/conformance/winfstest/scripts.rs), held to them by a golden of their calls and cases",
      scripts::SCRIPTS.len(),
      super::fetch::SECFS_TEST_COMMIT
    ),
    format!(
      "run in {} on the drive, each call one winfstest.exe process, {CALL_BOUND:?} a call",
      work.display()
    ),
  ];
  if !alive {
    notes.push(format!(
      "the daemon stopped answering; anchor log tail: {}",
      session.anchor_log_tail()
    ));
  }
  notes.extend(live_note);
  notes.extend(session.size_note.clone());
  drop(session);
  judged(run, &parsed, alive, notes)
}

/// The parsed scripts judged against the reviewed list and made a record.
fn judged(
  run: &Run<'_>,
  parsed: &[slates_conformance::tap::TapFile],
  alive: bool,
  mut notes: Vec<String>,
) -> Result<SuiteResult, Failure> {
  let tally = super::suites::tally(parsed);
  let (list, digest) = super::suites::reviewed_list(run.root, list_path(run.transport))?;
  let judgement = slates_conformance::expected::judge(&list, &tally.cases);
  for id in &judgement.unlisted_failures {
    println!("winfstest: unlisted failure {id}");
  }
  for id in &judgement.listed_now_passing {
    println!("winfstest: listed but now passing {id}");
  }
  let ok = judgement.acceptable() && tally.incomplete.is_empty() && alive;
  notes.insert(1, judgement.describe());
  if tally.failed > 0 {
    notes.push(super::suites::shape_note(&tally.cases));
  }
  if !tally.incomplete.is_empty() {
    notes.push(format!(
      "incomplete scripts: {}",
      tally.incomplete.join("; ")
    ));
  }
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
    privilege: if super::is_root() {
      slates_conformance::Privilege::Root
    } else {
      slates_conformance::Privilege::Unprivileged
    },
    outcome: super::suites::outcome_for(run, Suite::Winfstest, counts, None),
    command: "cd <drive>\\conformance-<pid>\\winfstest && <the transcribed scripts, each call `winfstest.exe ARGS`>"
      .to_owned(),
    bound: format!(
      "every script at billziss-gh/secfs.test@{} ({} scripts), {CALL_BOUND:?} a call",
      super::fetch::SECFS_TEST_COMMIT,
      scripts::SCRIPTS.len()
    ),
    expected_failure_list: Some(digest),
    notes,
    ok,
  })
}

/// winfstest's driver, built with MSVC.
#[cfg(windows)]
fn build(dir: &Path) -> Result<super::fetch::Built, Failure> {
  super::fetch::build_winfstest(dir)
}

/// winfstest's driver is a Win32 program; the capability table runs this suite on Windows alone.
#[cfg(not(windows))]
fn build(_dir: &Path) -> Result<super::fetch::Built, Failure> {
  Err(Failure(
    "winfstest's driver is a Win32 program and builds on Windows alone".to_owned(),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A stand-in driver that answers every call `0` with no records, and records the calls as the golden does.
  struct Recorder {
    events: Vec<String>,
    path: &'static str,
    next: usize,
  }

  impl Driver for Recorder {
    fn call(&mut self, args: &[&str]) -> Result<String, Failure> {
      self
        .events
        .push(format!("{}\tdriver\tcall\t{}", self.path, args.join(" ")));
      Ok("0\n".to_owned())
    }
    fn hold(&mut self, args: &[&str]) -> Result<(String, Held), Failure> {
      self
        .events
        .push(format!("{}\tdriver\thold\t{}", self.path, args.join(" ")));
      self.next += 1;
      Ok(("0\n".to_owned(), Held(self.next)))
    }
    fn release(&mut self, _held: Held) -> Result<(), Failure> {
      self.events.push(format!("{}\tdriver\trelease", self.path));
      Ok(())
    }
    fn opens(&self, _relative: &str) -> bool {
      false
    }
    fn absolute(&self, relative: &str) -> String {
      format!("C:\\work\\{relative}")
    }
  }

  /// The case events a script's TAP output shows, in the golden's form: an `expect` with its command and the status
  /// it asked for (or `lambda`), a held call likewise, and a `testeval` with its source.
  fn case_events(path: &str, tap: &str) -> Vec<String> {
    let mut events = Vec::new();
    for line in tap.lines() {
      let message = match line.split_once(" - ") {
        Some((head, message)) if head.starts_with("ok ") || head.starts_with("not ok ") => message,
        _ => continue,
      };
      let event = if let Some(rest) = message.strip_prefix("expect_task \"") {
        let (command, status) = rest.rsplit_once("\" ").unwrap_or((rest, ""));
        format!(
          "hold\t{command}\t{}",
          status.split(" - ").next().unwrap_or(status)
        )
      } else if let Some(rest) = message.strip_prefix("expect \"") {
        let (command, rest) = rest.split_once("\" ").unwrap_or((rest, ""));
        let wanted = if rest.starts_with('(') {
          "lambda"
        } else {
          rest.split(" - ").next().unwrap_or(rest)
        };
        format!("expect\t{command}\t{wanted}")
      } else {
        format!("eval\t{message}")
      };
      events.push(format!("{path}\tcase\t{event}"));
    }
    events
  }

  /// A-209. Do: run the transcription over a stand-in driver that answers every call `0`. Expect: every script makes
  /// exactly the calls, held calls and releases, expectations and `testeval` lines that the unchanged upstream scripts
  /// make under a stand-in `winfstest.py` (`golden.tsv`, 733 cases), in the same order, and every script finishes
  /// with its plan.
  #[test]
  fn the_transcription_makes_upstreams_calls_and_cases() {
    let golden = include_str!("golden.tsv");
    let mut names = 0u32;
    let mut driver_events = Vec::new();
    let mut cases = Vec::new();
    for transcribed in scripts::SCRIPTS {
      let mut recorder = Recorder {
        events: Vec::new(),
        path: transcribed.path,
        next: 0,
      };
      let mut script = Script::new(&mut recorder, &mut names);
      (transcribed.run)(&mut script);
      let (tap, count, ended) = script.finish();
      assert!(ended.is_none(), "{}: {ended:?}", transcribed.path);
      assert!(
        tap.ends_with(&format!("1..{count}\n")),
        "{}",
        transcribed.path
      );
      driver_events.extend(recorder.events);
      cases.extend(case_events(transcribed.path, &tap));
    }
    let want_driver: Vec<&str> = golden
      .lines()
      .filter(|l| l.contains("\tdriver\t"))
      .collect();
    let want_cases: Vec<&str> = golden.lines().filter(|l| l.contains("\tcase\t")).collect();
    assert_eq!(want_cases.len(), 733, "the golden's own count");
    for (index, (got, want)) in driver_events.iter().zip(&want_driver).enumerate() {
      assert_eq!(got, want, "driver event {index}");
    }
    assert_eq!(driver_events.len(), want_driver.len(), "driver events");
    for (index, (got, want)) in cases.iter().zip(&want_cases).enumerate() {
      assert_eq!(got, want, "case {index}");
    }
    assert_eq!(cases.len(), want_cases.len(), "cases");
  }

  /// A-209. Do: read the reviewed list. Expect: it parses, and every case it names is a case a transcribed script can
  /// report: its script is one of the twenty, and its number is within that script's cases under the golden.
  #[test]
  fn the_reviewed_list_names_only_cases_the_scripts_have() {
    let text = include_str!(
      "../../../../docs/wip/conformance/expected-failures/native-windows-winfsp.winfstest.txt"
    );
    let list = slates_conformance::expected::ExpectedFailures::parse(text).unwrap();
    assert!(!list.is_empty());
    let golden = include_str!("golden.tsv");
    for entry in list.entries() {
      let (path, number) = entry.id.rsplit_once(':').unwrap();
      let number: usize = number.parse().unwrap();
      let cases = golden
        .lines()
        .filter(|line| line.starts_with(&format!("{path}\tcase\t")))
        .count();
      assert!(
        scripts::SCRIPTS.iter().any(|script| script.path == path),
        "{} names no transcribed script",
        entry.id
      );
      assert!(
        (1..=cases).contains(&number),
        "{} is past the {cases} cases of {path}",
        entry.id
      );
      assert!(
        !entry.reason.trim().is_empty(),
        "{} has no reason",
        entry.id
      );
    }
  }

  /// A script whose driver call fails ends there: no later case is reported and no plan is printed, so its file is
  /// incomplete and the run cannot pass, as upstream's `CalledProcessError` ends the Python script.
  #[test]
  fn a_failed_call_ends_its_script_without_a_plan() {
    struct Failing;
    impl Driver for Failing {
      fn call(&mut self, _args: &[&str]) -> Result<String, Failure> {
        Err(Failure("winfstest exited 1".to_owned()))
      }
      fn hold(&mut self, _args: &[&str]) -> Result<(String, Held), Failure> {
        Err(Failure("winfstest exited 1".to_owned()))
      }
      fn release(&mut self, _held: Held) -> Result<(), Failure> {
        Ok(())
      }
      fn opens(&self, _relative: &str) -> bool {
        false
      }
      fn absolute(&self, relative: &str) -> String {
        relative.to_owned()
      }
    }
    let mut names = 0u32;
    let mut driver = Failing;
    let mut script = Script::new(&mut driver, &mut names);
    (scripts::SCRIPTS[0].run)(&mut script);
    let (tap, count, ended) = script.finish();
    assert_eq!(count, 0);
    assert!(ended.is_some());
    let parsed = parse_file("t/base/00.t", &tap, &Runner::Root);
    assert!(!parsed.complete(), "{tap}");
  }

  /// A failing expectation names what it asked for and what it got, in `winfstest.py`'s words, and a predicate over a
  /// reply with no record fails its case rather than passing.
  #[test]
  fn failures_read_as_upstreams_and_an_empty_reply_never_passes() {
    struct Denying;
    impl Driver for Denying {
      fn call(&mut self, args: &[&str]) -> Result<String, Failure> {
        Ok(if args.first() == Some(&"GetFileInformation") {
          "0\n".to_owned()
        } else {
          "ERROR_ACCESS_DENIED\n".to_owned()
        })
      }
      fn hold(&mut self, _args: &[&str]) -> Result<(String, Held), Failure> {
        Ok(("0\n".to_owned(), Held(0)))
      }
      fn release(&mut self, _held: Held) -> Result<(), Failure> {
        Ok(())
      }
      fn opens(&self, _relative: &str) -> bool {
        false
      }
      fn absolute(&self, relative: &str) -> String {
        relative.to_owned()
      }
    }
    let mut names = 0u32;
    let mut driver = Denying;
    let mut script = Script::new(&mut driver, &mut names);
    script.ok("CreateFile x GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0");
    script.check(
      "GetFileInformation x",
      "r[0][\"FileAttributes\"] == 0x20",
      |r| r.first_int("FileAttributes") == Some(0x20),
    );
    let (tap, count, _) = script.finish();
    assert_eq!(count, 2);
    assert!(
      tap.contains(
        "not ok 1 - expect \"CreateFile x GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0\" 0 - got ERROR_ACCESS_DENIED"
      ),
      "{tap}"
    );
    assert!(
      tap.contains("not ok 2 - expect \"GetFileInformation x\""),
      "{tap}"
    );
    let parsed = parse_file("t/base/01.t", &tap, &Runner::Root);
    assert!(parsed.complete());
  }
}
