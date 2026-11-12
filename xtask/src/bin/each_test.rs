//! One test target's tests, each in a process of its own with a deadline, so a test that hangs fails by name with its
//! output kept instead of running out its CI step's limit silently (main's arm64 Windows fleet step, run 42609672766:
//! `a_learner_fetches_the_councils_committed_configuration_over_the_transport` sat silent for 409 s and the step's
//! 20-minute limit cut the job before libtest said which test it was).
//!
//! `cargo run -p xtask --bin each_test -- --package PKG --test TARGET --per-test-secs N [--run-ignored NAME]...`
//!
//! 1. Builds the target (`cargo test --no-run --message-format=json`) and finds its executable.
//! 2. Lists its tests (`--list`), and its ignored tests (`--list --ignored`), which are skipped unless named with
//!    `--run-ignored`.
//! 3. Runs each test alone (`--exact NAME --nocapture`, ignored ones with `--ignored`), its output read as it comes.
//!    A test still running at its deadline is killed (its whole process, every daemon it started in it), and reported
//!    `TIMED OUT` by name with the last lines it printed; a failed test is reported with its output; a passed one by
//!    its time.
//! 4. Prints every test's time, slowest first, and exits non-zero if any test failed or timed out.
//!
//! A process per test is the isolation libtest's `--test-threads=1` already asks for, plus a fresh process: one test's
//! leftovers cannot reach the next. It costs a process start per test.
//!
//! A development tool, not shipped code: it runs `cargo` and the test binary, and obeys the no-panic law.

use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A failure with a plain-English message; printed and turned into a non-zero exit code.
#[derive(Debug)]
struct Failure(String);

impl fmt::Display for Failure {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl From<std::io::Error> for Failure {
  fn from(e: std::io::Error) -> Self {
    Failure(format!("io: {e}"))
  }
}

/// Format: the usage line.
const USAGE: &str = "each_test --package PKG --test TARGET --per-test-secs N [--run-ignored NAME]... [-- extra test args]";
/// Shape: how often a running test is looked at for its exit or its deadline. A tenth of a second is far finer than
/// any deadline it judges (minutes) and costs nothing.
const POLL: Duration = Duration::from_millis(100);
/// Shape: the lines of a timed-out or failed test's output printed: enough for the fleet trace's dump and the last
/// assertion, short enough to read in a CI log. The whole output of a failed test is usually shorter.
const KEPT_LINES: usize = 400;
/// Shape: the slowest tests named in the closing table, the ones a timeout derivation reads.
const SLOWEST_SHOWN: usize = 15;

/// What the command line asked for.
struct Options {
  package: String,
  test: String,
  per_test: Duration,
  run_ignored: Vec<String>,
  extra: Vec<String>,
}

fn options(args: &[String]) -> Result<Options, Failure> {
  let mut package = None;
  let mut test = None;
  let mut per_test = None;
  let mut run_ignored = Vec::new();
  let mut extra = Vec::new();
  let mut rest = args.iter();
  while let Some(arg) = rest.next() {
    let mut value = |name: &str| {
      rest
        .next()
        .cloned()
        .ok_or_else(|| Failure(format!("{USAGE}: `{name}` needs a value")))
    };
    match arg.as_str() {
      "--package" | "-p" => package = Some(value("--package")?),
      "--test" => test = Some(value("--test")?),
      "--per-test-secs" => {
        let text = value("--per-test-secs")?;
        let secs: u64 = text.parse().map_err(|_| {
          Failure(format!(
            "{USAGE}: `--per-test-secs {text}` is not a number of seconds"
          ))
        })?;
        per_test = Some(Duration::from_secs(secs));
      }
      "--run-ignored" => run_ignored.push(value("--run-ignored")?),
      "--" => {
        extra.extend(rest.by_ref().cloned());
      }
      other => return Err(Failure(format!("{USAGE}: unknown argument `{other}`"))),
    }
  }
  let missing = |name: &str| Failure(format!("{USAGE}: `{name}` is missing"));
  Ok(Options {
    package: package.ok_or_else(|| missing("--package"))?,
    test: test.ok_or_else(|| missing("--test"))?,
    per_test: per_test.ok_or_else(|| missing("--per-test-secs"))?,
    run_ignored,
    extra,
  })
}

/// Builds the test target and returns its executable, from cargo's own JSON messages.
fn build(options: &Options) -> Result<PathBuf, Failure> {
  let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
    .args([
      "test",
      "--locked",
      "-p",
      &options.package,
      "--test",
      &options.test,
      "--no-run",
      "--message-format=json",
    ])
    .stderr(Stdio::inherit())
    .output()?;
  if !output.status.success() {
    return Err(Failure(format!(
      "building {} --test {} failed ({})",
      options.package, options.test, output.status
    )));
  }
  for line in String::from_utf8_lossy(&output.stdout).lines() {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
      continue;
    };
    let is_target = message.get("reason").and_then(serde_json::Value::as_str)
      == Some("compiler-artifact")
      && message
        .pointer("/target/name")
        .and_then(serde_json::Value::as_str)
        == Some(options.test.as_str())
      && message
        .pointer("/profile/test")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if let (true, Some(executable)) = (
      is_target,
      message
        .get("executable")
        .and_then(serde_json::Value::as_str),
    ) {
      return Ok(PathBuf::from(executable));
    }
  }
  Err(Failure(format!(
    "cargo built {} --test {} but named no executable for it",
    options.package, options.test
  )))
}

/// The tests `--list` names (`name: test` lines), with `--ignored` the ignored ones alone.
fn list(executable: &PathBuf, ignored: bool) -> Result<Vec<String>, Failure> {
  let mut command = Command::new(executable);
  command.arg("--list");
  if ignored {
    command.arg("--ignored");
  }
  let output = command.stderr(Stdio::null()).output()?;
  if !output.status.success() {
    return Err(Failure(format!(
      "{} --list failed ({})",
      executable.display(),
      output.status
    )));
  }
  Ok(
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .filter_map(|line| line.strip_suffix(": test"))
      .map(str::to_owned)
      .collect(),
  )
}

/// How one test ended.
enum Outcome {
  Passed,
  Failed(String),
  TimedOut,
}

/// One test's run: its outcome, its time, and the last lines it printed.
struct Run {
  name: String,
  outcome: Outcome,
  took: Duration,
  lines: Vec<String>,
}

/// Reads a child's output lines on a thread, into `sink`; the thread ends at the pipe's end.
fn reader(
  pipe: Option<impl Read + Send + 'static>,
  sink: mpsc::Sender<String>,
) -> Option<std::thread::JoinHandle<()>> {
  let pipe = pipe?;
  Some(std::thread::spawn(move || {
    for line in BufReader::new(pipe).lines() {
      let Ok(line) = line else { break };
      if sink.send(line).is_err() {
        break;
      }
    }
  }))
}

/// Keeps the last [`KEPT_LINES`] of what arrived on `lines`.
fn keep(kept: &mut std::collections::VecDeque<String>, lines: &mpsc::Receiver<String>) {
  while let Ok(line) = lines.try_recv() {
    if kept.len() == KEPT_LINES {
      kept.pop_front();
    }
    kept.push_back(line);
  }
}

/// Runs one test alone, killing it at its deadline.
fn run_one(
  executable: &PathBuf,
  name: &str,
  ignored: bool,
  options: &Options,
) -> Result<Run, Failure> {
  let mut command = Command::new(executable);
  command
    .args(["--exact", name, "--nocapture", "--test-threads=1"])
    .args(&options.extra);
  if ignored {
    command.arg("--ignored");
  }
  let started = Instant::now();
  let mut child: Child = command
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
  let (sink, lines) = mpsc::channel();
  let readers: Vec<_> = [
    reader(child.stdout.take(), sink.clone()),
    reader(child.stderr.take(), sink),
  ]
  .into_iter()
  .flatten()
  .collect();
  let mut kept = std::collections::VecDeque::new();
  let outcome = loop {
    keep(&mut kept, &lines);
    if let Some(status) = child.try_wait()? {
      break if status.success() {
        Outcome::Passed
      } else {
        Outcome::Failed(status.to_string())
      };
    }
    if started.elapsed() >= options.per_test {
      // The whole test process goes, every in-process daemon with it; its pipes close, so the readers end.
      child.kill()?;
      child.wait()?;
      break Outcome::TimedOut;
    }
    #[allow(clippy::disallowed_methods)] // a development tool's poll cadence, not daemon code
    std::thread::sleep(POLL);
  };
  let took = started.elapsed();
  for handle in readers {
    handle
      .join()
      .map_err(|_| Failure(format!("a reader of {name}'s output failed")))?;
  }
  keep(&mut kept, &lines);
  Ok(Run {
    name: name.to_owned(),
    outcome,
    took,
    lines: kept.into_iter().collect(),
  })
}

fn run(options: &Options) -> Result<bool, Failure> {
  let executable = build(options)?;
  let ignored = list(&executable, true)?;
  let mut tests: Vec<(String, bool)> = list(&executable, false)?
    .into_iter()
    .filter(|name| !ignored.contains(name))
    .map(|name| (name, false))
    .collect();
  for name in &options.run_ignored {
    if !ignored.contains(name) {
      return Err(Failure(format!(
        "`--run-ignored {name}`: {} --test {} has no ignored test of that name",
        options.package, options.test
      )));
    }
    tests.push((name.clone(), true));
  }
  println!(
    "each_test: {} tests of {} --test {}, one process each, {} s each at most ({} ignored tests skipped)",
    tests.len(),
    options.package,
    options.test,
    options.per_test.as_secs(),
    ignored.len().saturating_sub(options.run_ignored.len())
  );
  let started = Instant::now();
  let mut runs = Vec::new();
  for (name, is_ignored) in &tests {
    let one = run_one(&executable, name, *is_ignored, options)?;
    match &one.outcome {
      Outcome::Passed => println!(
        "each_test: ok        {:>7.1} s  {}",
        one.took.as_secs_f64(),
        one.name
      ),
      Outcome::Failed(status) => {
        println!(
          "each_test: FAILED    {:>7.1} s  {} ({status}); its last {} lines:",
          one.took.as_secs_f64(),
          one.name,
          one.lines.len()
        );
        for line in &one.lines {
          println!("    | {line}");
        }
      }
      Outcome::TimedOut => {
        println!(
          "each_test: TIMED OUT {:>7.1} s  {} (killed at its {} s deadline); its last {} lines:",
          one.took.as_secs_f64(),
          one.name,
          options.per_test.as_secs(),
          one.lines.len()
        );
        for line in &one.lines {
          println!("    | {line}");
        }
      }
    }
    runs.push(one);
  }
  let failed: Vec<&str> = runs
    .iter()
    .filter(|run| matches!(run.outcome, Outcome::Failed(_)))
    .map(|run| run.name.as_str())
    .collect();
  let timed_out: Vec<&str> = runs
    .iter()
    .filter(|run| matches!(run.outcome, Outcome::TimedOut))
    .map(|run| run.name.as_str())
    .collect();
  let mut by_time: Vec<&Run> = runs.iter().collect();
  by_time.sort_by_key(|run| std::cmp::Reverse(run.took));
  println!("each_test: the {SLOWEST_SHOWN} slowest:");
  for run in by_time.iter().take(SLOWEST_SHOWN) {
    println!("    {:>7.1} s  {}", run.took.as_secs_f64(), run.name);
  }
  println!(
    "each_test: {} passed, {} failed, {} timed out, in {:.1} s",
    runs
      .len()
      .saturating_sub(failed.len())
      .saturating_sub(timed_out.len()),
    failed.len(),
    timed_out.len(),
    started.elapsed().as_secs_f64()
  );
  for name in &failed {
    println!("each_test: FAILED: {name}");
  }
  for name in &timed_out {
    println!("each_test: TIMED OUT: {name}");
  }
  Ok(failed.is_empty() && timed_out.is_empty())
}

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  match options(&args).and_then(|options| run(&options)) {
    Ok(true) => ExitCode::SUCCESS,
    Ok(false) => ExitCode::FAILURE,
    Err(failure) => {
      eprintln!("each_test: {failure}");
      ExitCode::FAILURE
    }
  }
}
