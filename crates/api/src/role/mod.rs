//! The anchor and daemon roles (§2.5 "The anchor process", §2.6 "Boot order", §4.12 "Embedding", A-278): what a
//! process of an instance runs, served by whichever executable the instance was started from — the `slates` binary, or
//! an embedding host's own (kiln's), so a host runs no slates binary at all.
//!
//! - **The anchor** measures the machine profile, creates the shared segment, publishes the profile, and supervises the
//!   daemon as its child, restarting it under the derived bound and killing it when its heartbeat lapses (§4.14
//!   `daemon.alive`). Its daemon is the anchor's own executable started again, from the image the anchor runs (A-145),
//!   with the **prefix** the anchor itself was started with, then the daemon role's arguments: `slates --instance NAME
//!   daemon`, or `kiln slates-role --instance NAME daemon`. Every process of an instance carries `--instance NAME` in
//!   its arguments, so a test or a host finds exactly its instance's processes.
//! - **The daemon** attaches the anchor's segment from its environment (or, run alone, measures and creates one), reads
//!   the published profile, derives its configuration and serves; it leaves when its anchor does.
//! - **The WinFsp purge client** (Windows) is a child the daemon starts from the same executable and prefix, since no
//!   daemon thread opens a volume it serves (§4.6).
//!
//! A host serves the roles from its `main`: when its arguments begin with the prefix it chose, it hands the rest to
//! [`main`], which never returns to the host's own code paths. [`crate::StartSpec`] builds the anchor's command with the
//! same program and prefix.
//!
//! Moved here from the `slates` binary on 2026-11-18 (A-278): `anchor.rs`, `daemon.rs`, `parent.rs`, `dumps.rs`,
//! `fleet.rs`, `recovery_key.rs` and the stop flag of `signal.rs`, unchanged in what they do; the binary's `anchor` and
//! `daemon` verbs call [`serve`] with no prefix. A role's failure ends its process, so [`Failure`] carries the sentence
//! its log line prints and the exit code it maps to, and nothing else reads it.

use std::fmt;
use std::process::ExitCode;

pub(crate) mod anchor;
pub(crate) mod daemon;
mod dumps;
pub(crate) mod fleet;
// A-232 step 5: the liveness bound against this crate's own anchor (the test binary re-invoked as the anchor and
// daemon). Unix only: the Windows anchor hands its daemon an explicit handle list, so the roles' shared standard input
// is owed there.
#[cfg(test)]
mod liveness_tests;
mod parent;
pub(crate) mod recovery_key;
mod signal;

/// Format: the exit code of a role that failed (a refused or failed start, a daemon its own watchdog ended, A-232):
/// the server's one constant, as the `slates` command's.
pub const EXIT_FAILED: u8 = slates_server::watchdog::FAILED_EXIT_CODE;
/// Format: the exit code of a role refused before it started for a configuration the operator gave (an unreadable or
/// malformed recovery key), as the `slates` command's refusals.
pub const EXIT_REFUSED: u8 = 1;
/// Format: the exit code of role arguments that do not parse, as the `slates` command's usage errors.
pub const EXIT_USAGE: u8 = 2;

/// Format: the role arguments [`Role::parse`] takes, for its usage message.
pub const ROLE_USAGE: &str =
  "[--instance NAME] (anchor | daemon) [--shards N] [--quick] [--fleet PATH --node NAME]";

/// Options of the anchor and the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessOptions {
  /// The instance.
  pub instance: String,
  /// Measure the quick profile (tests; a full profile takes seconds).
  pub quick: bool,
  /// Shards, when the caller overrides the derivation (tests).
  pub shards: Option<u16>,
  /// The fleet to join, when this daemon is a fleet node (§2.6 boot step 6); a laptop has none.
  pub fleet: Option<FleetSelection>,
}

/// Which fleet node this process is: the operator's shared manifest and this node's name in it
/// (`--fleet PATH --node NAME`; `slates_server::deploy` derives everything else from the two).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetSelection {
  /// The path of the shared manifest.
  pub manifest: String,
  /// This node's name in it.
  pub node: String,
}

/// A role a process of an instance runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
  /// The anchor: the segment's owner and the daemon's supervisor.
  Anchor(ProcessOptions),
  /// The daemon.
  Daemon(ProcessOptions),
}

/// How a role ended other than well: the sentence its log line prints, and so its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
  /// A configuration the operator gave was refused before the role started (exit [`EXIT_REFUSED`]).
  Refused(String),
  /// The role failed ([`EXIT_FAILED`]).
  Failed(String),
}

impl Failure {
  /// The exit code the failure maps to.
  pub fn exit_code(&self) -> u8 {
    match self {
      Self::Refused(_) => EXIT_REFUSED,
      Self::Failed(_) => EXIT_FAILED,
    }
  }
}

impl fmt::Display for Failure {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Refused(text) => write!(f, "refused: {text}"),
      Self::Failed(text) => f.write_str(text),
    }
  }
}

/// Why role arguments do not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleArgsError {
  /// No `anchor` or `daemon` word.
  NoRole,
  /// A word past the role's.
  Extra(String),
  /// A flag the roles do not take.
  UnknownFlag(String),
  /// A flag that takes a value was given none.
  MissingValue(String),
  /// A value that does not parse.
  BadValue {
    /// The flag.
    flag: &'static str,
    /// Why.
    reason: String,
  },
  /// `--fleet` without `--node`, or `--node` without `--fleet`: a fleet node is named by both.
  HalfAFleet,
}

impl fmt::Display for RoleArgsError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NoRole => f.write_str("no role: expected `anchor` or `daemon`"),
      Self::Extra(word) => write!(f, "unexpected argument `{word}`"),
      Self::UnknownFlag(flag) => write!(f, "unknown flag `{flag}`"),
      Self::MissingValue(flag) => write!(f, "`{flag}` takes a value"),
      Self::BadValue { flag, reason } => write!(f, "bad {flag}: {reason}"),
      Self::HalfAFleet => f.write_str(
        "a fleet node is named by both `--fleet PATH` (the manifest) and `--node NAME` (which node it is)",
      ),
    }
  }
}

impl std::error::Error for RoleArgsError {}

/// Format: the role words.
const ANCHOR_WORD: &str = "anchor";
const DAEMON_WORD: &str = "daemon";

/// The role's parse state: the flags as given, the role word.
#[derive(Default)]
struct Given {
  instance: Option<String>,
  shards: Option<String>,
  fleet: Option<String>,
  node: Option<String>,
  quick: bool,
  word: Option<String>,
}

impl Given {
  /// Takes `flag` with `value` (a flag that takes one).
  fn take_value(&mut self, flag: &str, value: String) -> Result<(), RoleArgsError> {
    let slot = match flag {
      "--instance" => &mut self.instance,
      "--shards" => &mut self.shards,
      "--fleet" => &mut self.fleet,
      "--node" => &mut self.node,
      other => return Err(RoleArgsError::UnknownFlag(other.to_owned())),
    };
    *slot = Some(value);
    Ok(())
  }

  /// The options the flags name.
  fn options(self) -> Result<ProcessOptions, RoleArgsError> {
    let shards = self
      .shards
      .map(|text| {
        text.parse::<u16>().map_err(|e| RoleArgsError::BadValue {
          flag: "--shards",
          reason: e.to_string(),
        })
      })
      .transpose()?;
    let fleet = match (self.fleet, self.node) {
      (Some(manifest), Some(node)) => Some(FleetSelection { manifest, node }),
      (None, None) => None,
      _ => return Err(RoleArgsError::HalfAFleet),
    };
    Ok(ProcessOptions {
      instance: self.instance.unwrap_or_else(slates_ipc::instance_from_env),
      quick: self.quick,
      shards,
      fleet,
    })
  }
}

/// Whether `flag` takes a value.
fn takes_value(flag: &str) -> bool {
  matches!(flag, "--instance" | "--shards" | "--fleet" | "--node")
}

impl Role {
  /// Parses a role's arguments, as [`Role::args`] makes them and a person types them: [`ROLE_USAGE`], flags in any
  /// order, each value after its flag or joined by `=`; with no `--instance`, the environment's instance
  /// (`SLATES_ENDPOINT`), else `default`, as the `slates` command takes it.
  pub fn parse(args: &[String]) -> Result<Role, RoleArgsError> {
    let mut given = Given::default();
    let mut words = args.iter();
    while let Some(argument) = words.next() {
      if argument.starts_with("--") {
        let (flag, inline) = match argument.split_once('=') {
          Some((flag, value)) => (flag, Some(value.to_owned())),
          None => (argument.as_str(), None),
        };
        if flag == "--quick" && inline.is_none() {
          given.quick = true;
        } else if takes_value(flag) {
          let value = match inline {
            Some(value) => value,
            None => words
              .next()
              .cloned()
              .ok_or_else(|| RoleArgsError::MissingValue(flag.to_owned()))?,
          };
          given.take_value(flag, value)?;
        } else {
          return Err(RoleArgsError::UnknownFlag(argument.clone()));
        }
      } else if given.word.is_none() {
        given.word = Some(argument.clone());
      } else {
        return Err(RoleArgsError::Extra(argument.clone()));
      }
    }
    match given.word.take().as_deref() {
      Some(ANCHOR_WORD) => Ok(Role::Anchor(given.options()?)),
      Some(DAEMON_WORD) => Ok(Role::Daemon(given.options()?)),
      Some(other) => Err(RoleArgsError::Extra(other.to_owned())),
      None => Err(RoleArgsError::NoRole),
    }
  }

  /// The arguments that name this role, which [`Role::parse`] reads back to it: `--instance NAME`, the role's word,
  /// then its flags.
  pub fn args(&self) -> Vec<String> {
    let (word, options) = match self {
      Role::Anchor(options) => (ANCHOR_WORD, options),
      Role::Daemon(options) => (DAEMON_WORD, options),
    };
    let mut args = vec![
      "--instance".to_owned(),
      options.instance.clone(),
      word.to_owned(),
    ];
    if options.quick {
      args.push("--quick".to_owned());
    }
    if let Some(shards) = options.shards {
      args.push("--shards".to_owned());
      args.push(shards.to_string());
    }
    // A fleet node's daemon joins the fleet; the anchor only passes the selection through (the daemon reads the
    // manifest, so a restart re-reads an operator's edit).
    if let Some(fleet) = &options.fleet {
      args.push("--fleet".to_owned());
      args.push(fleet.manifest.clone());
      args.push("--node".to_owned());
      args.push(fleet.node.clone());
    }
    args
  }

  /// The role's options.
  pub fn options(&self) -> &ProcessOptions {
    match self {
      Role::Anchor(options) | Role::Daemon(options) => options,
    }
  }

  /// The role's word, as its log lines name it.
  fn word(&self) -> &'static str {
    match self {
      Role::Anchor(_) => ANCHOR_WORD,
      Role::Daemon(_) => DAEMON_WORD,
    }
  }
}

/// Runs `role` in this process until it ends: the anchor until a stop is requested (it starts its daemon as this
/// process's own executable with `prefix`, then the daemon role's arguments), the daemon until a stop is requested or
/// its anchor is gone. `prefix` is what the host's `main` recognised before the role's arguments (empty for the
/// `slates` binary).
pub fn serve(prefix: &[String], role: &Role) -> Result<(), Failure> {
  match role {
    Role::Anchor(options) => anchor::run(prefix, options),
    Role::Daemon(options) => daemon::run(prefix, options),
  }
}

/// The role entry of a host's own executable: `args` are what followed the host's `prefix` in its arguments (the
/// WinFsp purge client's word, on Windows, or a role [`Role::parse`] reads); the role runs, and its exit code comes
/// back for the host's `main` to return. A usage error prints [`ROLE_USAGE`]; a failure prints one line naming the
/// role.
///
/// ```no_run
/// fn main() -> std::process::ExitCode {
///   let args: Vec<String> = std::env::args().skip(1).collect();
///   if args.first().map(String::as_str) == Some("slates-role") {
///     return slates_api::role::main(&args[..1], &args[1..]);
///   }
///   // ... the host's own commands ...
///   std::process::ExitCode::SUCCESS
/// }
/// ```
pub fn main(prefix: &[String], args: &[String]) -> ExitCode {
  #[cfg(windows)]
  if args.first().map(String::as_str) == Some(slates_bridge_winfsp::purge_client::ROLE_ARG) {
    let code = slates_bridge_winfsp::purge_client::serve_from_env();
    return ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX));
  }
  let role = match Role::parse(args) {
    Ok(role) => role,
    Err(error) => {
      slates_machine::log_line!("slates: {error}\nusage: {ROLE_USAGE}");
      return ExitCode::from(EXIT_USAGE);
    }
  };
  match serve(prefix, &role) {
    Ok(()) => ExitCode::SUCCESS,
    Err(failure) => {
      slates_machine::log_line!("slates {}: {failure}", role.word());
      ExitCode::from(failure.exit_code())
    }
  }
}

/// Measures the machine profile as the roles do (§4.1): full, or quick (tests). A probe that could not measure at all
/// within its bound is the failure, named, never a profile carrying a fabricated value.
pub fn measure(quick: bool) -> Result<slates_machine::MachineProfile, Failure> {
  daemon::measure(quick)
}

/// The operator's recovery key (§4.8, AUD-07), when `SLATES_RECOVERY_KEY` names one: what confirms a consensus
/// recovery on an instance whose operator holds one. Refused when it is named but unreadable or malformed.
pub fn recovery_key() -> Result<Option<slates_server::RecoveryKey>, Failure> {
  recovery_key::load()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
  }

  /// §2.5, A-278. Do: parse an anchor's and a daemon's arguments in any order, with values after their flags and
  /// joined by `=`, then make each role's arguments and parse them back. Expect: the roles named, and every role equal
  /// to its own arguments read back — the anchor starts its daemon from exactly what this pair agrees on.
  #[test]
  fn a_roles_arguments_parse_and_read_back_to_the_same_role() {
    let anchor = Role::parse(&words("--quick anchor --shards=2 --instance z")).unwrap();
    assert_eq!(
      anchor,
      Role::Anchor(ProcessOptions {
        instance: "z".into(),
        quick: true,
        shards: Some(2),
        fleet: None,
      })
    );
    let daemon = Role::parse(&words("daemon --instance a --fleet /f.json --node a")).unwrap();
    assert_eq!(
      daemon,
      Role::Daemon(ProcessOptions {
        instance: "a".into(),
        quick: false,
        shards: None,
        fleet: Some(FleetSelection {
          manifest: "/f.json".into(),
          node: "a".into(),
        }),
      })
    );
    for role in [anchor, daemon] {
      assert_eq!(Role::parse(&role.args()), Ok(role.clone()));
      assert_eq!(role.args().first().map(String::as_str), Some("--instance"));
    }
  }

  /// §2.5, A-278. Do: parse arguments that name no role, two roles, a flag the roles do not take, a flag with no value,
  /// a bad shard count, and half a fleet selection. Expect: each refused by its own variant, never a role.
  #[test]
  fn arguments_that_name_no_role_or_a_bad_one_are_refused_by_kind() {
    assert_eq!(Role::parse(&words("--quick")), Err(RoleArgsError::NoRole));
    assert_eq!(
      Role::parse(&words("anchor daemon")),
      Err(RoleArgsError::Extra("daemon".into()))
    );
    assert_eq!(
      Role::parse(&words("volume")),
      Err(RoleArgsError::Extra("volume".into()))
    );
    assert_eq!(
      Role::parse(&words("anchor --json")),
      Err(RoleArgsError::UnknownFlag("--json".into()))
    );
    assert_eq!(
      Role::parse(&words("anchor --shards")),
      Err(RoleArgsError::MissingValue("--shards".into()))
    );
    assert!(matches!(
      Role::parse(&words("anchor --shards many")),
      Err(RoleArgsError::BadValue {
        flag: "--shards",
        ..
      })
    ));
    assert_eq!(
      Role::parse(&words("daemon --node a")),
      Err(RoleArgsError::HalfAFleet)
    );
  }
}
