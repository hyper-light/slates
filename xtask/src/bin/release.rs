//! The release lane's tasks as a binary of their own (Appendix B.2, A-210; `docs/publish.md`, "The binary release"):
//! `cargo run -p xtask --bin release -- smoke ...` runs one built `slates` binary as a person installing it would,
//! and `... -- artifacts ...` gathers every target's record into the checksums, the provenance and the draft's notes.
//! The tasks live in `../release.rs`, the tag grammar in `../tag.rs`.
//!
//! They are not `cargo xtask` subcommands because they must build on every runner the release builds on, Windows
//! too, and the `xtask` binary does not build there: its conformance and KIND tasks reach Unix-only calls
//! (`rustix`, `std::os::unix`; release run 41980547276, 12 errors on both Windows runners). This crate root names only
//! the modules the release needs, so nothing else of `xtask` is compiled into it.
//!
//! A development tool, not shipped code: it reads and writes the files its caller names and runs `slates`, and it
//! obeys the no-panic law (typed errors, exit codes), as `xtask` does.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[path = "../release.rs"]
mod release;
#[path = "../tag.rs"]
mod tag;

/// A task failure with a plain-English message; printed and turned into a non-zero exit code.
#[derive(Debug)]
pub(crate) struct Failure(pub(crate) String);

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

impl From<serde_json::Error> for Failure {
  fn from(e: serde_json::Error) -> Self {
    Failure(format!("json: {e}"))
  }
}

/// The workspace root: the directory holding the top-level `Cargo.toml`.
fn workspace_root() -> Result<PathBuf, Failure> {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .map(Path::to_path_buf)
    .ok_or_else(|| Failure("xtask has no parent directory".to_owned()))
}

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let rest = args.get(1..).unwrap_or_default();
  let outcome = match args.first().map(String::as_str) {
    Some("smoke") => workspace_root().and_then(|root| release::smoke(&root, rest)),
    Some("artifacts") => release::artifacts(rest),
    other => Err(Failure(format!(
      "unknown task {other:?}; tasks: smoke, artifacts (xtask/src/release.rs)"
    ))),
  };
  match outcome {
    Ok(()) => ExitCode::SUCCESS,
    Err(failure) => {
      eprintln!("release: {failure}");
      ExitCode::FAILURE
    }
  }
}
