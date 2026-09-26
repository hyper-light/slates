//! Tracer ownership for Part 6 example 8 and AC-4.5. A spawned tracer has not necessarily
//! attached: Apple's fs_usage initializes caches before ktrace_start. The workload starts only
//! after an observed event; every exit path must cancel and reap the owned process group.

use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use super::pause;
use crate::Failure;

/// The two terminal actions supported by the tracer's process-group owner.
#[derive(Clone, Copy)]
pub(super) enum StopSignal {
  /// Ask the tracer to flush its log and finish successfully.
  Interrupt,
  /// Cancel an incomplete run, including any privileged child of sudo.
  Kill,
}

/// One child and its process group, with the caller's existing harness wait budget.
pub(super) struct TraceProcess {
  child: Child,
  signal: fn(u32, StopSignal) -> Result<(), Failure>,
  bound: Duration,
  reaped: bool,
}

impl TraceProcess {
  pub(super) fn new(
    child: Child,
    signal: fn(u32, StopSignal) -> Result<(), Failure>,
    bound: Duration,
  ) -> Self {
    Self {
      child,
      signal,
      bound,
      reaped: false,
    }
  }

  pub(super) fn wait_ready(
    &mut self,
    mut observe: impl FnMut() -> Result<bool, Failure>,
  ) -> Result<(), Failure> {
    let started = Instant::now();
    loop {
      self.require_running()?;
      if observe()? {
        self.require_running()?;
        return Ok(());
      }
      if started.elapsed() >= self.bound {
        return Err(Failure(format!(
          "tracer produced no event within {:?}",
          self.bound
        )));
      }
      pause();
    }
  }

  fn require_running(&mut self) -> Result<(), Failure> {
    if let Some(status) = self.poll()? {
      return Err(Failure(format!(
        "tracer exited before completing the workload: {status}"
      )));
    }
    Ok(())
  }

  fn poll(&mut self) -> Result<Option<ExitStatus>, Failure> {
    let status = self.child.try_wait()?;
    self.reaped = status.is_some();
    Ok(status)
  }

  fn wait_exit(&mut self) -> Result<ExitStatus, Failure> {
    let started = Instant::now();
    loop {
      if let Some(status) = self.poll()? {
        return Ok(status);
      }
      if started.elapsed() >= self.bound {
        return Err(Failure(format!(
          "tracer did not exit within {:?}",
          self.bound
        )));
      }
      pause();
    }
  }

  /// Stops the tracer with its graceful signal and requires an exit status `accept` allows (a tracer
  /// that ends on its stop signal reports that signal, not success).
  pub(super) fn stop_accepting(
    &mut self,
    accept: impl Fn(ExitStatus) -> bool,
  ) -> Result<(), Failure> {
    self.require_running()?;
    (self.signal)(self.child.id(), StopSignal::Interrupt)?;
    let status = self.wait_exit()?;
    if !accept(status) {
      return Err(Failure(format!("tracer exited {status}")));
    }
    Ok(())
  }

  fn cancel(&mut self) -> Result<(), Failure> {
    if self.poll()?.is_none() {
      // sudo relays INT to its command, including when a policy created a separate pty
      // and process group. KILL cannot be relayed, so give the owner a chance to reap first.
      (self.signal)(self.child.id(), StopSignal::Interrupt)?;
      if let Err(error) = self.wait_exit() {
        (self.signal)(self.child.id(), StopSignal::Kill)?;
        self.wait_exit()?;
        return Err(error);
      }
    }
    Ok(())
  }
}

impl Drop for TraceProcess {
  fn drop(&mut self) {
    if !self.reaped
      && let Err(error) = self.cancel()
    {
      eprintln!(
        "tracer cleanup failed for process group {}: {error}",
        self.child.id()
      );
    }
  }
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::os::unix::process::CommandExt;
  use std::process::{ChildStdin, ChildStdout, Command, Stdio};

  use super::*;
  use crate::conformance::slates::MOUNT_WAIT;

  fn signal_group(pid: u32, signal: StopSignal) -> Result<(), Failure> {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("child pid"))
      .expect("positive child pid");
    let signal = match signal {
      StopSignal::Interrupt => rustix::process::Signal::INT,
      StopSignal::Kill => rustix::process::Signal::KILL,
    };
    rustix::process::kill_process_group(pid, signal)
      .map_err(|error| Failure(format!("signalling fixture: {error}")))?;
    Ok(())
  }

  fn child(script: &str) -> (TraceProcess, ChildStdin, ChildStdout) {
    let mut child = Command::new("sh")
      .args(["-c", script])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .process_group(0)
      .spawn()
      .expect("spawn trace fixture");
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let flags = rustix::fs::fcntl_getfl(&stdout).expect("pipe flags");
    rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)
      .expect("nonblocking pipe");
    (
      TraceProcess::new(child, signal_group, MOUNT_WAIT),
      stdin,
      stdout,
    )
  }

  fn observed(stdout: &mut ChildStdout, output: &mut String) -> Result<bool, Failure> {
    match stdout.read_to_string(output) {
      Ok(_) => {}
      Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
      Err(error) => return Err(error.into()),
    }
    Ok(output.contains("ready\n"))
  }

  /// AC-4.5: startup waits for a real event before the workload is allowed to run.
  #[test]
  fn delayed_readiness_precedes_work() {
    let (mut tracer, mut stdin, mut stdout) = child("read start; printf 'ready\\n'; read finish");
    let mut output = String::new();
    let mut sent = false;
    tracer
      .wait_ready(|| {
        if !sent {
          stdin.write_all(b"start\n")?;
          sent = true;
        }
        observed(&mut stdout, &mut output)
      })
      .expect("trace becomes ready");
    let ready = output.contains("ready\n");
    // Complete and reap even under the missing-barrier negative control.
    if !sent {
      stdin
        .write_all(b"start\n")
        .expect("start fixture for cleanup");
    }
    stdin.write_all(b"finish\n").expect("finish fixture");
    let status = tracer.wait_exit().expect("fixture exits");
    assert!(status.success());
    assert!(
      ready,
      "the workload would run before the tracer's first event"
    );
  }

  /// AC-4.5: an exited tracer cannot authorize an unobserved workload.
  #[test]
  fn early_exit_is_reported_before_the_workload() {
    let (mut tracer, _stdin, mut stdout) = child("exit 7");
    let mut output = String::new();
    let result = tracer.wait_ready(|| observed(&mut stdout, &mut output));
    tracer.wait_exit().expect("reap exited fixture");
    assert!(
      result
        .expect_err("an exited tracer was accepted")
        .0
        .contains("exit status: 7")
    );
  }

  /// AC-4.5: shutdown flushes the final event and reaps the tracer before log judgement.
  #[test]
  fn stop_drains_the_terminal_event_and_reaps_the_child() {
    let (mut tracer, _stdin, mut stdout) =
      child(r#"trap 'printf "final\n"; exit 0' INT; printf 'ready\n'; read finish"#);
    let mut output = String::new();
    tracer
      .wait_ready(|| observed(&mut stdout, &mut output))
      .expect("ready");
    tracer
      .stop_accepting(|status| status.success())
      .expect("stop and drain");
    stdout.read_to_string(&mut output).expect("final log");
    assert_eq!(output, "ready\nfinal\n");
  }

  /// AC-4.5: even a flushed log cannot turn an unsuccessful tracer exit into evidence.
  #[test]
  fn stop_rejects_an_unsuccessful_exit() {
    let (mut tracer, _stdin, mut stdout) =
      child(r#"trap 'exit 7' INT; printf 'ready\n'; read finish"#);
    let mut output = String::new();
    tracer
      .wait_ready(|| observed(&mut stdout, &mut output))
      .expect("ready");
    assert!(
      tracer
        .stop_accepting(|status| status.success())
        .expect_err("tracer failed")
        .0
        .contains("exit status: 7")
    );
  }

  /// AC-4.5: an early workload error must leave neither a live tracer nor an unreaped child.
  #[test]
  fn dropping_a_ready_tracer_cancels_and_reaps_it() {
    let (mut tracer, _stdin, mut stdout) =
      child(r#"trap 'printf "cancelled\n"; exit 0' INT; printf 'ready\n'; read finish"#);
    let pid = rustix::process::Pid::from_raw(i32::try_from(tracer.child.id()).expect("pid"))
      .expect("positive pid");
    let mut output = String::new();
    tracer
      .wait_ready(|| observed(&mut stdout, &mut output))
      .expect("ready");
    drop(tracer);
    stdout
      .read_to_string(&mut output)
      .expect("cancellation drained");
    assert_eq!(output, "ready\ncancelled\n");
    assert_eq!(
      rustix::process::test_kill_process(pid),
      Err(rustix::io::Errno::SRCH)
    );
    assert!(matches!(
      rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
      Err(rustix::io::Errno::CHILD)
    ));
  }

  /// AC-4.5: a live process without trace activity cannot satisfy the startup boundary.
  #[test]
  fn a_silent_tracer_is_refused_when_the_supplied_budget_is_spent() {
    let (mut tracer, _stdin, _stdout) = child("read finish");
    tracer.bound = Duration::ZERO;
    assert!(tracer.wait_ready(|| Ok(false)).is_err());
    // Restore the existing cleanup budget; a zero startup budget does not excuse an orphan.
    tracer.bound = MOUNT_WAIT;
  }
}
