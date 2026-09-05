//! The argument grammar: verbs, the flags each takes, and the parse into a typed command.
//! Every flag is listed here once; an unknown flag or a missing value is a usage refusal.

use std::collections::BTreeMap;

use slates_client::{Intent, NamePolicy, SizeClass};

use crate::format::{parse_size, parse_snapshot, parse_volume_id};

/// The usage text: the verbs.
pub(crate) const USAGE: &str = "usage: slates [--instance NAME] <command>

  anchor   [--quick] [--shards N]                  run the anchor: own the segment, supervise the daemon
  daemon   [--quick] [--shards N]                  run the daemon (alone, or as the anchor's child)
  profile  [--quick] [--json]                      measure and print the machine profile

  volume create NAME (--bounded SIZE | --dynamic MAX) [--fold] [--locked] [--base DIR]
  volume list
  volume stat ID
  volume snapshot ID
  volume clone ID SNAPSHOT NAME
  volume resize ID (--bounded SIZE | --dynamic MAX)
  volume destroy ID
  volume placed ID [--snapshot N] [--mirror]         await a durability scope
  attach ID [--read | --write] [--snapshot N]
  detach ATTACHMENT
  status                                           the daemon's status
  status ID [--drift]
  base read ID PATH
  base rewitness ID [PATH ...]
  base pin ID [PATH ...]
  land ID TARGET [--snapshot N] [--include P] [--exclude P] [--grant N]
  grants
  audit [--since N]
  exec --volume V --at PATH -- CMD [ARG ...]        run CMD with the volume at PATH
";

/// Format: the usage notes: the size grammar's examples, the instance's discovery, the exit codes.
pub(crate) const USAGE_NOTES: &str =
  "SIZE is bytes with a binary unit: 512MiB, 4GiB (B, KiB, MiB, GiB, TiB).
The instance is --instance, else SLATES_ENDPOINT, else `default`.
Exit codes: 0 done, 1 refused, 2 usage, 3 no daemon, 4 failed.";

/// A parse refusal.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseError {
  /// Help was asked for.
  Help,
  /// A verb that does not exist.
  UnknownCommand(String),
  /// A flag the verb does not take.
  UnknownFlag(String),
  /// A flag that takes a value, given none.
  MissingValue(String),
  /// A positional argument the verb needs, missing.
  Missing(&'static str),
  /// An argument the verb does not take.
  Extra(String),
  /// A value that does not parse.
  BadValue {
    /// The flag or argument.
    what: &'static str,
    /// Why.
    reason: String,
  },
}

impl std::fmt::Display for ParseError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Help => f.write_str("help"),
      Self::UnknownCommand(c) => write!(f, "unknown command `{c}`"),
      Self::UnknownFlag(flag) => write!(f, "unknown flag `{flag}`"),
      Self::MissingValue(flag) => write!(f, "`{flag}` takes a value"),
      Self::Missing(what) => write!(f, "missing {what}"),
      Self::Extra(arg) => write!(f, "unexpected argument `{arg}`"),
      Self::BadValue { what, reason } => write!(f, "bad {what}: {reason}"),
    }
  }
}

/// Options of the anchor and the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessOptions {
  /// The instance.
  pub instance: String,
  /// Measure the quick profile (tests; a full profile takes seconds).
  pub quick: bool,
  /// Shards, when the caller overrides the derivation (tests).
  pub shards: Option<u16>,
}

/// Options of `profile`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProfileOptions {
  /// Quick.
  pub quick: bool,
  /// JSON instead of the derived constants.
  pub json: bool,
}

/// A client verb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
  /// Create.
  Create {
    /// The name.
    name: String,
    /// The size class.
    size: SizeClass,
    /// The name policy.
    names: NamePolicy,
    /// Locked memory required.
    require_locked: bool,
    /// The base directory.
    base: Option<String>,
  },
  /// List.
  List,
  /// The daemon's status (`status` with no volume).
  DaemonStatus,
  /// Status (`volume stat`, `status ID`).
  Status {
    /// The volume.
    volume: slates_client::VolumeId,
    /// Drifted entries only.
    drift: bool,
  },
  /// Snapshot.
  Snapshot {
    /// The volume.
    volume: slates_client::VolumeId,
  },
  /// Await a durability scope (`volume placed`).
  Placed {
    /// The volume.
    volume: slates_client::VolumeId,
    /// A snapshot, or the head when none.
    snapshot: Option<slates_client::SnapshotId>,
    /// The scope.
    scope: slates_client::Scope,
  },
  /// Clone.
  Clone {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The snapshot.
    snapshot: slates_client::SnapshotId,
    /// The clone's name.
    name: String,
  },
  /// Resize.
  Resize {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The size class.
    size: SizeClass,
  },
  /// Destroy.
  Destroy {
    /// The volume.
    volume: slates_client::VolumeId,
  },
  /// Attach.
  Attach {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The snapshot.
    snapshot: Option<slates_client::SnapshotId>,
    /// The intent.
    intent: Intent,
  },
  /// Detach.
  Detach {
    /// The attachment.
    attachment: u64,
  },
  /// Read a base entry.
  ReadBase {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The path.
    path: String,
  },
  /// Re-witness.
  Rewitness {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The paths, or every drifted entry.
    paths: Option<Vec<String>>,
  },
  /// Pin.
  Pin {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The paths, or the whole base.
    paths: Option<Vec<String>>,
  },
  /// Land (`land`).
  Land {
    /// The volume.
    volume: slates_client::VolumeId,
    /// A snapshot, or the head when none.
    snapshot: Option<slates_client::SnapshotId>,
    /// The host directory to write into.
    target: String,
    /// The filter.
    filter: slates_client::Filter,
    /// A grant the caller holds.
    grant: Option<u64>,
  },
  /// The caller's grants (`grants`).
  Grants,
  /// The audit log (`audit`).
  Audit {
    /// Every record at or after this sequence.
    since: u64,
  },
}

/// A client request: the instance and the verb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientRequest {
  /// The instance.
  pub instance: String,
  /// The verb.
  pub verb: Verb,
}

/// A `slates exec` request: the volume, the path to show it at, and the command to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecRequest {
  /// The volume to make visible.
  pub volume: String,
  /// The path to make it visible at.
  pub at: String,
  /// The command and its arguments.
  pub command: Vec<String>,
}

/// The parsed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
  /// The launcher.
  Exec(ExecRequest),
  /// The anchor.
  Anchor(ProcessOptions),
  /// The daemon.
  Daemon(ProcessOptions),
  /// The profile.
  Profile(ProfileOptions),
  /// A client verb.
  Client(ClientRequest),
}

/// Every flag that takes a value, across the verbs.
const VALUES: &[&str] = &[
  "--instance",
  "--shards",
  "--bounded",
  "--dynamic",
  "--base",
  "--snapshot",
  "--grant",
  "--since",
  "--include",
  "--exclude",
  "--volume",
  "--at",
];
/// Every switch, across the verbs.
const SWITCHES: &[&str] = &[
  "--quick", "--json", "--fold", "--locked", "--read", "--write", "--drift", "--mirror",
];

/// Parses `exec --volume V --at PATH -- CMD ...`: the flags before `--`, the command after it.
fn parse_exec(rest: &[String]) -> Result<Command, ParseError> {
  let split = rest.iter().position(|a| a == "--");
  let (head, command) = match split {
    Some(i) => (&rest[..i], rest[i + 1..].to_vec()),
    None => return Err(ParseError::Missing("`--` then the command")),
  };
  if command.is_empty() {
    return Err(ParseError::Missing("a command after `--`"));
  }
  let taken = take(head)?;
  taken.only(&Spec {
    values: &["--volume", "--at"],
    switches: &[],
  })?;
  let volume = taken
    .value("--volume")
    .ok_or(ParseError::Missing("--volume"))?
    .to_owned();
  let at = taken
    .value("--at")
    .ok_or(ParseError::Missing("--at"))?
    .to_owned();
  Ok(Command::Exec(ExecRequest {
    volume,
    at,
    command,
  }))
}

/// The flags one verb takes: those with a value and the switches (the instance is every
/// verb's).
struct Spec {
  values: &'static [&'static str],
  switches: &'static [&'static str],
}

/// The arguments with the flags taken out: the words in order, the flags by name.
struct Taken {
  words: Vec<String>,
  values: BTreeMap<&'static str, String>,
  switches: BTreeMap<&'static str, bool>,
}

impl Taken {
  fn value(&self, flag: &str) -> Option<&str> {
    self.values.get(flag).map(String::as_str)
  }

  fn switch(&self, flag: &str) -> bool {
    self.switches.get(flag).copied().unwrap_or(false)
  }

  /// Refuses a flag the verb does not take.
  fn only(&self, spec: &Spec) -> Result<(), ParseError> {
    let stray = self
      .values
      .keys()
      .filter(|k| **k != "--instance" && !spec.values.contains(k))
      .chain(self.switches.keys().filter(|k| !spec.switches.contains(k)))
      .next();
    match stray {
      Some(flag) => Err(ParseError::UnknownFlag((*flag).to_owned())),
      None => Ok(()),
    }
  }

  fn instance(&self) -> String {
    self
      .value("--instance")
      .map(str::to_owned)
      .unwrap_or_else(slates_ipc::instance_from_env)
  }
}

fn known(list: &[&'static str], flag: &str) -> Option<&'static str> {
  list.iter().copied().find(|k| *k == flag)
}

/// Separates the flags from the words.
fn take(arguments: &[String]) -> Result<Taken, ParseError> {
  let mut taken = Taken {
    words: Vec::new(),
    values: BTreeMap::new(),
    switches: BTreeMap::new(),
  };
  let mut rest = arguments.iter();
  while let Some(argument) = rest.next() {
    if argument == "--help" || argument == "-h" {
      return Err(ParseError::Help);
    }
    let Some(flag) = argument.strip_prefix("--") else {
      taken.words.push(argument.clone());
      continue;
    };
    let (name, inline) = match flag.split_once('=') {
      Some((n, v)) => (n, Some(v.to_owned())),
      None => (flag, None),
    };
    let full = format!("--{name}");
    if let Some(key) = known(VALUES, &full) {
      let value = match inline {
        Some(v) => v,
        None => rest
          .next()
          .cloned()
          .ok_or_else(|| ParseError::MissingValue(full.clone()))?,
      };
      taken.values.insert(key, value);
    } else if let Some(key) = known(SWITCHES, &full) {
      if inline.is_some() {
        return Err(ParseError::BadValue {
          what: "switch",
          reason: format!("`{full}` takes no value"),
        });
      }
      taken.switches.insert(key, true);
    } else {
      return Err(ParseError::UnknownFlag(full));
    }
  }
  Ok(taken)
}

fn shards_of(taken: &Taken) -> Result<Option<u16>, ParseError> {
  taken
    .value("--shards")
    .map(|s| {
      s.parse::<u16>().map_err(|e| ParseError::BadValue {
        what: "--shards",
        reason: e.to_string(),
      })
    })
    .transpose()
}

fn size_of(taken: &Taken) -> Result<SizeClass, ParseError> {
  match (taken.value("--bounded"), taken.value("--dynamic")) {
    (Some(limit), None) => Ok(SizeClass::Bounded {
      limit: parse_size(limit).map_err(|reason| ParseError::BadValue {
        what: "--bounded",
        reason,
      })?,
    }),
    (None, Some(max)) => Ok(SizeClass::Dynamic {
      max: parse_size(max).map_err(|reason| ParseError::BadValue {
        what: "--dynamic",
        reason,
      })?,
    }),
    (None, None) => Err(ParseError::Missing("--bounded SIZE or --dynamic MAX")),
    (Some(_), Some(_)) => Err(ParseError::BadValue {
      what: "size",
      reason: "one of --bounded and --dynamic, not both".to_owned(),
    }),
  }
}

fn volume(text: &str) -> Result<slates_client::VolumeId, ParseError> {
  parse_volume_id(text).map_err(|reason| ParseError::BadValue {
    what: "volume ID",
    reason,
  })
}

fn snapshot(text: &str) -> Result<slates_client::SnapshotId, ParseError> {
  parse_snapshot(text).map_err(|reason| ParseError::BadValue {
    what: "SNAPSHOT",
    reason,
  })
}

fn paths(words: &[String]) -> Option<Vec<String>> {
  if words.is_empty() {
    None
  } else {
    Some(words.to_vec())
  }
}

fn client(taken: &Taken, verb: Verb) -> Command {
  Command::Client(ClientRequest {
    instance: taken.instance(),
    verb,
  })
}

const NONE: Spec = Spec {
  values: &[],
  switches: &[],
};

/// Parses the arguments (without the program name).
pub(crate) fn parse(arguments: &[String]) -> Result<Command, ParseError> {
  if arguments.first().map(String::as_str) == Some("exec") {
    return parse_exec(&arguments[1..]);
  }
  let taken = take(arguments)?;
  let words: Vec<&str> = taken.words.iter().map(String::as_str).collect();
  match words.as_slice() {
    [] => Err(ParseError::Help),
    ["anchor" | "daemon"] => {
      taken.only(&Spec {
        values: &["--shards"],
        switches: &["--quick"],
      })?;
      let options = ProcessOptions {
        instance: taken.instance(),
        quick: taken.switch("--quick"),
        shards: shards_of(&taken)?,
      };
      Ok(if words[0] == "anchor" {
        Command::Anchor(options)
      } else {
        Command::Daemon(options)
      })
    }
    ["profile"] => {
      taken.only(&Spec {
        values: &[],
        switches: &["--quick", "--json"],
      })?;
      Ok(Command::Profile(ProfileOptions {
        quick: taken.switch("--quick"),
        json: taken.switch("--json"),
      }))
    }
    ["volume", rest @ ..] => parse_volume(&taken, rest),
    ["attach", id] => {
      taken.only(&Spec {
        values: &["--snapshot"],
        switches: &["--read", "--write"],
      })?;
      let snapshot = taken.value("--snapshot").map(snapshot).transpose()?;
      let intent = if taken.switch("--write") {
        Intent::Write
      } else {
        Intent::Read
      };
      Ok(client(
        &taken,
        Verb::Attach {
          volume: volume(id)?,
          snapshot,
          intent,
        },
      ))
    }
    ["attach"] => Err(ParseError::Missing("volume ID")),
    ["detach", attachment] => {
      taken.only(&NONE)?;
      let attachment = attachment
        .parse::<u64>()
        .map_err(|e| ParseError::BadValue {
          what: "ATTACHMENT",
          reason: e.to_string(),
        })?;
      Ok(client(&taken, Verb::Detach { attachment }))
    }
    ["detach"] => Err(ParseError::Missing("ATTACHMENT")),
    ["status", id] => {
      taken.only(&Spec {
        values: &[],
        switches: &["--drift"],
      })?;
      Ok(client(
        &taken,
        Verb::Status {
          volume: volume(id)?,
          drift: taken.switch("--drift"),
        },
      ))
    }
    ["status"] => {
      taken.only(&NONE)?;
      Ok(client(&taken, Verb::DaemonStatus))
    }
    ["base", rest @ ..] => parse_base(&taken, rest),
    ["land", id, target] => {
      taken.only(&Spec {
        values: &["--snapshot", "--grant", "--include", "--exclude"],
        switches: &[],
      })?;
      let snapshot = taken.value("--snapshot").map(snapshot).transpose()?;
      let grant = taken
        .value("--grant")
        .map(|s| {
          s.parse::<u64>().map_err(|e| ParseError::BadValue {
            what: "--grant",
            reason: e.to_string(),
          })
        })
        .transpose()?;
      let filter = slates_client::Filter {
        include: taken
          .value("--include")
          .map(str::to_owned)
          .into_iter()
          .collect(),
        exclude: taken
          .value("--exclude")
          .map(str::to_owned)
          .into_iter()
          .collect(),
      };
      Ok(client(
        &taken,
        Verb::Land {
          volume: volume(id)?,
          snapshot,
          target: (*target).to_owned(),
          filter,
          grant,
        },
      ))
    }
    ["land", ..] => Err(ParseError::Missing("ID TARGET")),
    ["grants"] => {
      taken.only(&NONE)?;
      Ok(client(&taken, Verb::Grants))
    }
    ["audit"] => {
      taken.only(&Spec {
        values: &["--since"],
        switches: &[],
      })?;
      let since = taken
        .value("--since")
        .map(|s| {
          s.parse::<u64>().map_err(|e| ParseError::BadValue {
            what: "--since",
            reason: e.to_string(),
          })
        })
        .transpose()?
        .unwrap_or(0);
      Ok(client(&taken, Verb::Audit { since }))
    }
    [verb, rest @ ..]
      if matches!(
        *verb,
        "anchor" | "daemon" | "profile" | "attach" | "detach" | "status"
      ) =>
    {
      Err(ParseError::Extra(
        rest.last().map_or((*verb).to_owned(), |w| (*w).to_owned()),
      ))
    }
    [other, ..] => Err(ParseError::UnknownCommand((*other).to_owned())),
  }
}

fn parse_volume(taken: &Taken, words: &[&str]) -> Result<Command, ParseError> {
  match words {
    [] => Err(ParseError::Missing("volume subcommand")),
    ["create", name] => {
      taken.only(&Spec {
        values: &["--bounded", "--dynamic", "--base"],
        switches: &["--fold", "--locked"],
      })?;
      let size = size_of(taken)?;
      Ok(client(
        taken,
        Verb::Create {
          name: (*name).to_owned(),
          size,
          names: if taken.switch("--fold") {
            NamePolicy::Fold
          } else {
            NamePolicy::Exact
          },
          require_locked: taken.switch("--locked"),
          base: taken.value("--base").map(str::to_owned),
        },
      ))
    }
    ["create"] => Err(ParseError::Missing("NAME")),
    ["list"] => {
      taken.only(&NONE)?;
      Ok(client(taken, Verb::List))
    }
    ["stat", id] => {
      taken.only(&NONE)?;
      Ok(client(
        taken,
        Verb::Status {
          volume: volume(id)?,
          drift: false,
        },
      ))
    }
    ["snapshot", id] => {
      taken.only(&NONE)?;
      Ok(client(
        taken,
        Verb::Snapshot {
          volume: volume(id)?,
        },
      ))
    }
    ["clone", id, snap, name] => {
      taken.only(&NONE)?;
      Ok(client(
        taken,
        Verb::Clone {
          volume: volume(id)?,
          snapshot: snapshot(snap)?,
          name: (*name).to_owned(),
        },
      ))
    }
    ["resize", id] => {
      taken.only(&Spec {
        values: &["--bounded", "--dynamic"],
        switches: &[],
      })?;
      let size = size_of(taken)?;
      Ok(client(
        taken,
        Verb::Resize {
          volume: volume(id)?,
          size,
        },
      ))
    }
    ["placed", id] => {
      taken.only(&Spec {
        values: &["--snapshot"],
        switches: &["--mirror"],
      })?;
      let snapshot = taken.value("--snapshot").map(snapshot).transpose()?;
      let scope = if taken.switch("--mirror") {
        slates_client::Scope::Mirror
      } else {
        slates_client::Scope::Region
      };
      Ok(client(
        taken,
        Verb::Placed {
          volume: volume(id)?,
          snapshot,
          scope,
        },
      ))
    }
    ["destroy", id] => {
      taken.only(&NONE)?;
      Ok(client(
        taken,
        Verb::Destroy {
          volume: volume(id)?,
        },
      ))
    }
    ["stat" | "snapshot" | "resize" | "destroy" | "placed"] => {
      Err(ParseError::Missing("volume ID"))
    }
    ["clone", ..] => Err(ParseError::Missing("ID SNAPSHOT NAME")),
    [sub, rest @ ..]
      if matches!(
        *sub,
        "create" | "list" | "stat" | "snapshot" | "resize" | "destroy" | "placed"
      ) =>
    {
      Err(ParseError::Extra(
        rest.last().map_or((*sub).to_owned(), |w| (*w).to_owned()),
      ))
    }
    [other, ..] => Err(ParseError::UnknownCommand(format!("volume {other}"))),
  }
}

fn parse_base(taken: &Taken, words: &[&str]) -> Result<Command, ParseError> {
  taken.only(&NONE)?;
  match words {
    ["read", id, path] => Ok(client(
      taken,
      Verb::ReadBase {
        volume: volume(id)?,
        path: (*path).to_owned(),
      },
    )),
    ["rewitness", id, rest @ ..] => Ok(client(
      taken,
      Verb::Rewitness {
        volume: volume(id)?,
        paths: paths(&rest.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>()),
      },
    )),
    ["pin", id, rest @ ..] => Ok(client(
      taken,
      Verb::Pin {
        volume: volume(id)?,
        paths: paths(&rest.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>()),
      },
    )),
    [] => Err(ParseError::Missing("base subcommand")),
    ["read", ..] => Err(ParseError::Missing("ID PATH")),
    ["rewitness" | "pin"] => Err(ParseError::Missing("volume ID")),
    [other, ..] => Err(ParseError::UnknownCommand(format!("base {other}"))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
  }

  /// The grammar: a create with its flags in any order, a global instance before or after
  /// the verb, the process verbs' options.
  #[test]
  fn the_grammar_parses_the_documented_forms() {
    let Command::Client(request) = parse(&args(
      "volume create scratch --bounded 4GiB --fold --instance x",
    ))
    .unwrap() else {
      panic!("client");
    };
    assert_eq!(request.instance, "x");
    assert_eq!(
      request.verb,
      Verb::Create {
        name: "scratch".into(),
        size: SizeClass::Bounded { limit: 4 << 30 },
        names: NamePolicy::Fold,
        require_locked: false,
        base: None,
      }
    );
    let Command::Client(request) = parse(&args("--instance y volume list")).unwrap() else {
      panic!("client");
    };
    assert_eq!(request.instance, "y");
    assert_eq!(request.verb, Verb::List);
    let Command::Client(request) = parse(&args(
      "attach 00000000000000000000000000000001 --write --snapshot 3",
    ))
    .unwrap() else {
      panic!("client");
    };
    assert_eq!(
      request.verb,
      Verb::Attach {
        volume: parse_volume_id("00000000000000000000000000000001").unwrap(),
        snapshot: Some(slates_client::SnapshotId { value: 3 }),
        intent: Intent::Write,
      }
    );
    assert_eq!(
      parse(&args("anchor --quick --shards 2 --instance z")),
      Ok(Command::Anchor(ProcessOptions {
        instance: "z".into(),
        quick: true,
        shards: Some(2),
      }))
    );
  }

  /// The refusals: a size without a binary unit, an unknown flag, a missing size, an extra
  /// argument, nothing at all (help).
  #[test]
  fn the_grammar_refuses_what_it_does_not_document() {
    assert!(matches!(
      parse(&args("volume create a --bounded 4GB")),
      Err(ParseError::BadValue { .. })
    ));
    assert_eq!(
      parse(&args("volume list --colour")),
      Err(ParseError::UnknownFlag("--colour".into()))
    );
    assert_eq!(
      parse(&args("volume create a")),
      Err(ParseError::Missing("--bounded SIZE or --dynamic MAX"))
    );
    assert_eq!(
      parse(&args("volume list extra")),
      Err(ParseError::Extra("extra".into()))
    );
    assert_eq!(parse(&[]), Err(ParseError::Help));
  }

  /// `exec` splits at `--`: the flags before it, the command after; a missing `--` or an empty
  /// command is refused.
  #[test]
  fn exec_splits_the_flags_from_the_command() {
    let Command::Exec(request) = parse(&args(
      "exec --volume scratch --at /home/u/build -- cargo build --release",
    ))
    .unwrap() else {
      panic!("exec");
    };
    assert_eq!(request.volume, "scratch");
    assert_eq!(request.at, "/home/u/build");
    assert_eq!(request.command, vec!["cargo", "build", "--release"]);
    assert!(matches!(
      parse(&args("exec --volume v --at /p cargo build")),
      Err(ParseError::Missing("`--` then the command"))
    ));
    assert!(matches!(
      parse(&args("exec --volume v --at /p --")),
      Err(ParseError::Missing("a command after `--`"))
    ));
    assert!(matches!(
      parse(&args("exec --at /p -- cmd")),
      Err(ParseError::Missing("--volume"))
    ));
  }
}
