//! The engine the transcribed winfstest scripts run on: `winfstest.py`'s `expect`, `expect_task`, `testeval`,
//! `testdone` and `uniqname`, with `simpletap.py`'s view of their output, over a [`Driver`] that makes the calls.
//!
//! Each case prints the TAP line `winfstest.py`'s `testline` prints (`ok N - expect "CMD" 0`, `not ok N - expect "CMD"
//! 0 - got ERROR_ACCESS_DENIED`, a `testeval`'s own source as its message), and a finished script prints its plan
//! (`1..N`), so the output is judged by the same TAP reader, tally and reviewed list as pjdfstest's
//! (`crates/conformance/src/tap.rs`, `expected.rs`). Two places differ from the Python, each toward more evidence and
//! never toward a pass: a predicate over a reply with no record fails its case where Python's `r[0]` would end the
//! script (and so the rest of the script still runs and is counted), and a predicate's message names its upstream
//! expression where Python prints `<lambda>`. What ends a script upstream ends it here: a driver that exits non-zero
//! (`CalledProcessError`) or prints no outcome line (`out[0]`). An ended script prints no plan, so its file is
//! incomplete and the run fails whatever the list says, as `simpletap.py` reports "test plan missing".

use slates_conformance::winfstest::{Reply, parse_reply};

use crate::Failure;

/// A held call: the driver's own index of a call started with `-w`, released by [`Driver::release`].
pub(crate) struct Held(pub(crate) usize);

/// What makes a script's calls: the real driver runs `winfstest.exe` in the mount; a stand-in records them.
pub(crate) trait Driver {
  /// Runs `winfstest.exe ARGS` to its end and returns its standard output; an `Err` is what ends a script upstream (a
  /// non-zero exit, a call past its bound).
  fn call(&mut self, args: &[&str]) -> Result<String, Failure>;
  /// Starts `winfstest.exe -w ARGS`, which holds its handle open until released, and returns its standard output once
  /// the call has printed it, with the call to release.
  fn hold(&mut self, args: &[&str]) -> Result<(String, Held), Failure>;
  /// Releases a held call (a newline on its input, as `_fstest_task.__exit__` writes) and waits for it.
  fn release(&mut self, held: Held) -> Result<(), Failure>;
  /// Whether a path relative to the scripts' directory opens for reading (`safeopen`).
  fn opens(&self, relative: &str) -> bool;
  /// The absolute form of a name in the scripts' directory (`os.path.realpath`).
  fn absolute(&self, relative: &str) -> String;
}

/// One script's run: its cases, its TAP output, and why it ended early if it did.
pub(crate) struct Script<'d> {
  driver: &'d mut dyn Driver,
  names: &'d mut u32,
  cases: u32,
  tap: String,
  ended: Option<String>,
}

/// The reply a script sees after it ended: no outcome and no records, so nothing after an end can pass.
fn nothing() -> Reply {
  Reply {
    error: String::new(),
    records: Vec::new(),
  }
}

/// Splits a command line as `cmd.split()` does.
fn arguments(command: &str) -> Vec<&str> {
  command.split_whitespace().collect()
}

impl<'d> Script<'d> {
  /// A script over `driver`, naming its files from the run's shared counter.
  pub(crate) fn new(driver: &'d mut dyn Driver, names: &'d mut u32) -> Script<'d> {
    Script {
      driver,
      names,
      cases: 0,
      tap: String::new(),
      ended: None,
    }
  }

  /// `uniqname()`: a fresh eight-digit hexadecimal name. Upstream draws it at random because each script is its own
  /// process; here the run counts, so a run's names are reproducible and never repeat within it.
  pub(crate) fn name(&mut self) -> String {
    *self.names = self.names.wrapping_add(1);
    format!("{:08x}", *self.names)
  }

  /// `os.path.realpath(name)`.
  pub(crate) fn absolute(&self, relative: &str) -> String {
    self.driver.absolute(relative)
  }

  /// `safeopen(name)` as a truth value.
  pub(crate) fn opens(&self, relative: &str) -> bool {
    self.driver.opens(relative)
  }

  /// `testline(ok, diag)`.
  fn line(&mut self, ok: bool, message: &str) {
    if self.ended.is_some() {
      return;
    }
    self.cases = self.cases.saturating_add(1);
    let verdict = if ok { "ok" } else { "not ok" };
    self
      .tap
      .push_str(&format!("{verdict} {} - {message}\n", self.cases));
  }

  /// Ends the script, as an exception ends the Python.
  fn end(&mut self, why: String) {
    if self.ended.is_none() {
      self.tap.push_str(&format!("# the script ended: {why}\n"));
      self.ended = Some(why);
    }
  }

  /// One call's reply, or the end of the script.
  fn reply_of(&mut self, outcome: Result<String, Failure>) -> Reply {
    match outcome.and_then(|stdout| parse_reply(&stdout).map_err(|e| Failure(e.to_string()))) {
      Ok(reply) => reply,
      Err(failure) => {
        self.end(failure.0);
        nothing()
      }
    }
  }

  /// One call, unjudged.
  fn run(&mut self, command: &str) -> Reply {
    if self.ended.is_some() {
      return nothing();
    }
    let outcome = self.driver.call(&arguments(command));
    self.reply_of(outcome)
  }

  /// `expect(cmd, status)`: passes when the outcome line is `status` (`str(exp) == err`).
  fn status(&mut self, command: &str, status: &str) -> Reply {
    let reply = self.run(command);
    if reply.error == status {
      self.line(true, &format!("expect \"{command}\" {status}"));
    } else {
      self.line(
        false,
        &format!("expect \"{command}\" {status} - got {}", reply.error),
      );
    }
    reply
  }

  /// `expect(cmd, 0)`.
  pub(crate) fn ok(&mut self, command: &str) -> Reply {
    self.status(command, "0")
  }

  /// `expect(cmd, "ERROR_…")`.
  pub(crate) fn err(&mut self, command: &str, error: &str) -> Reply {
    self.status(command, error)
  }

  /// `expect(cmd, lambda r: …)`: passes when the call succeeds and `predicate` holds over its records; `source` is the
  /// upstream expression, for the message.
  pub(crate) fn check(
    &mut self,
    command: &str,
    source: &str,
    predicate: impl Fn(&Reply) -> bool,
  ) -> Reply {
    let reply = self.run(command);
    if reply.error == "0" {
      let held = predicate(&reply);
      self.line(
        held,
        &format!(
          "expect \"{command}\" ({source}) - result {}",
          reply.describe()
        ),
      );
    } else {
      self.line(
        false,
        &format!("expect \"{command}\" 0 - got {}", reply.error),
      );
    }
    reply
  }

  /// `with expect_task(cmd, 0):` entered: the call is made holding its handle, and judged at once, as `__enter__` does.
  pub(crate) fn hold(&mut self, command: &str) -> Option<Held> {
    if self.ended.is_some() {
      return None;
    }
    self
      .tap
      .push_str(&format!("# expect_task \"{command}\" 0\n"));
    match self.driver.hold(&arguments(command)) {
      Ok((stdout, held)) => {
        let reply = self.reply_of(Ok(stdout));
        if reply.error == "0" {
          self.line(true, &format!("expect_task \"{command}\" 0"));
        } else {
          self.line(
            false,
            &format!("expect_task \"{command}\" 0 - got {}", reply.error),
          );
        }
        Some(held)
      }
      Err(failure) => {
        self.end(failure.0);
        None
      }
    }
  }

  /// The `with` block left: the held call released (`__exit__`).
  pub(crate) fn release(&mut self, held: Option<Held>) {
    if let Some(held) = held
      && let Err(failure) = self.driver.release(held)
    {
      self.end(failure.0);
    }
  }

  /// `testeval(expression)`: the expression's own source line is the message.
  pub(crate) fn eval(&mut self, ok: bool, source: &str) {
    self.line(ok, source);
  }

  /// `testdone()`: the TAP output, with its plan unless the script ended early.
  pub(crate) fn finish(mut self) -> (String, u32, Option<String>) {
    if self.ended.is_none() {
      self.tap.push_str(&format!("1..{}\n", self.cases));
    }
    (self.tap, self.cases, self.ended)
  }
}
