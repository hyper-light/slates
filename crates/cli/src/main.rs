//! `slates`: the command (§4.12 "CLI", §2.5 "the anchor process", §2.6 "Boot order"; Phase 2
//! task 5). Three roles in one binary, so a machine runs one thing:
//!
//! - `slates anchor` is the anchor process of §2.5: it measures the machine profile, creates
//!   the shared segment, publishes the profile into it, and supervises `slates daemon` as a
//!   child with the segment handed over in its environment, restarting it under the derived
//!   bound and killing it when its heartbeat lapses (§4.14 `daemon.alive`).
//! - `slates daemon` is the daemon: it attaches the segment from its environment (or, run
//!   alone, measures and creates one), reads the published profile, derives its
//!   configuration and serves; it leaves when its anchor does.
//! - Every other verb is a client: the rendezvous, one request, one reply, printed in a
//!   stable plain form (one `key: value` per line, or one record per line), with the refusal
//!   taxonomy mapped to exit codes.
//!
//! Arguments are parsed by hand (`args.rs`): a dozen verbs do not justify a dependency, and
//! every flag is listed in one place with what it takes. No grant verb exists here yet: the
//! grant surface of §4.13 arrives with Phase 2 task 8, and the ring channel refuses the kind.

use std::process::ExitCode;

mod anchor;
mod args;
mod daemon;
#[cfg(target_os = "linux")]
mod exec;
mod format;
mod parent;
mod signal;
mod verbs;

use args::{Command, ParseError};

/// Format: the exit code for a request the daemon refused (the refusal is printed).
const EXIT_REFUSED: u8 = 1;
/// Format: the exit code for a usage error (the usage is printed).
const EXIT_USAGE: u8 = 2;
/// Format: the exit code when no daemon answers at the instance.
const EXIT_UNAVAILABLE: u8 = 3;
/// Format: the exit code when the anchor or the daemon itself failed.
const EXIT_FAILED: u8 = 4;

fn main() -> ExitCode {
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  let command = match args::parse(&arguments) {
    Ok(command) => command,
    Err(ParseError::Help) => {
      println!("{}\n{}", args::USAGE, args::USAGE_NOTES);
      return ExitCode::SUCCESS;
    }
    Err(e) => {
      eprintln!("slates: {e}\n\n{}\n{}", args::USAGE, args::USAGE_NOTES);
      return ExitCode::from(EXIT_USAGE);
    }
  };
  let outcome = match command {
    Command::Anchor(options) => anchor::run(&options),
    Command::Daemon(options) => daemon::run(&options),
    Command::Profile(options) => verbs::profile(&options),
    Command::Mcp(options) => verbs::mcp(&options),
    Command::Client(request) => verbs::run(&request),
    Command::Exec(request) => run_exec(&request),
  };
  match outcome {
    Ok(()) => ExitCode::SUCCESS,
    Err(Failure::Refused(text)) => {
      eprintln!("slates: refused: {text}");
      ExitCode::from(EXIT_REFUSED)
    }
    Err(Failure::Unavailable(instance)) => {
      eprintln!("slates: no daemon at instance {instance} (is `slates anchor` running?)");
      ExitCode::from(EXIT_UNAVAILABLE)
    }
    Err(Failure::Failed(text)) => {
      eprintln!("slates: {text}");
      ExitCode::from(EXIT_FAILED)
    }
  }
}

/// Runs the launcher on Linux; elsewhere the namespaces it needs do not exist.
#[cfg(target_os = "linux")]
fn run_exec(request: &args::ExecRequest) -> Result<(), Failure> {
  exec::run(request)
}

/// The launcher is Linux-only (user and mount namespaces); other platforms refuse it.
#[cfg(not(target_os = "linux"))]
fn run_exec(_request: &args::ExecRequest) -> Result<(), Failure> {
  Err(Failure::Failed(
    "slates exec needs Linux user and mount namespaces; it is not available on this platform"
      .to_owned(),
  ))
}

/// How a command ends other than well; each maps to one exit code.
#[derive(Debug)]
pub(crate) enum Failure {
  /// The daemon refused; the typed refusal, printed.
  Refused(String),
  /// No daemon at the instance.
  Unavailable(String),
  /// The command itself failed (the anchor or the daemon).
  Failed(String),
}
