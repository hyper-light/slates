//! The argument grammar: verbs, the flags each takes, and the parse into a typed command.
//! Every flag is listed here once; an unknown flag or a missing value is a usage refusal.

use std::collections::BTreeMap;

use slates_client::{GrantScope, GreenBase, Intent, NamePolicy, ReadAt, Rights, SizeClass};

use crate::format::{parse_size, parse_snapshot, parse_volume_id};

/// The usage text: the verbs.
pub(crate) const USAGE: &str = "usage: slates [--instance NAME] <command>

  anchor   [--quick] [--shards N] [--fleet PATH --node NAME]   run the anchor: own the segment, supervise the daemon
  daemon   [--quick] [--shards N] [--fleet PATH --node NAME]   run the daemon (alone, or as the anchor's child)
  profile  [--quick] [--json]                      measure and print the machine profile
  mcp [--instance NAME] [--http PORT]               serve the MCP tools (stdio, or loopback HTTP)

  volume create NAME (--bounded SIZE | --dynamic MAX) [--fold] [--locked] [--base DIR] [--json]
  volume list [--json]
  volume stat ID [--json]
  volume snapshot ID [--json]
  volume destroy-snapshot ID SNAPSHOT [--json]
  volume clone ID SNAPSHOT NAME [--json]
  volume resize ID (--bounded SIZE | --dynamic MAX) [--json]
  volume destroy ID [--json]
  green NAME [--require-evidence] [--base VOLUME --snapshot N] [--json]   create a green merge target
  versions GREEN [--json]                          its head version
  changed-since GREEN VERSION [--json]             files changed since a version
  work GREEN NAME [--json]                         a work volume over a green
  edit WORK PATH AT DELETE TEXT [--json]           declare an edit (a splice)
  submit WORK [--evidence HEX] [--json]            submit a work's increment
  rebase WORK [--json]                             rebase a work onto its green's head
  advance ATTACHMENT [VERSION] [--json]            re-pin a green attachment (the head when no VERSION)
  read VOLUME PATH [--version N | --attachment A]  a file's bytes at a view (raw bytes)
  volume placed ID [--snapshot N] [--mirror] [--json]   await a durability scope
  attach ID [--read | --write] [--snapshot N]
            [--oci-source HOST_PATH --oci-destination CONTAINER_PATH] [--json]
  detach ATTACHMENT [--json]
  status [--json]                                  the daemon's status
  status ID [--drift] [--json]
  base read ID PATH
  base digest ID PATH [--json]                     a clean base file's verified content digest
  base rewitness ID [PATH ...] [--json]
  base pin ID [PATH ...] [--json]
  land ID TARGET [--snapshot N] [--include P] [--exclude P] [--grant N] [--json]
  grants [--json]
  grant LANDING MANIFEST [--session] [--term SECONDS] [--json]   approve a presented landing
  audit [--since N] [--json]
  enroll [--account UID] [--json]                  enroll a consumer; its capability is shown once
  revoke CONSUMER [--json]                         revoke a consumer's enrollment
  share ID PRINCIPAL [--read] [--write] [--admin] [--json]   set a principal's rights on a volume
  run [--keep] [--json] -- CMD [ARG ...]           run CMD as a consumer enrolled for its lifetime
  exec --volume V --at PATH -- CMD [ARG ...]        run CMD with the volume at PATH
";

/// Format: the usage notes: the size grammar's examples, the instance's discovery, the exit codes.
pub(crate) const USAGE_NOTES: &str =
  "SIZE is bytes with a binary unit: 512MiB, 4GiB (B, KiB, MiB, GiB, TiB).
The instance is --instance, else SLATES_ENDPOINT, else `default`.
Exit codes: 0 done, 1 refused, 2 usage, 3 no daemon, 4 failed; `run` exits as its command did.
PRINCIPAL is uid:N, consumer:N (under your account) or consumer:ACCOUNT/N.
--json emits machine-readable JSON on every client verb (the MCP schema), except `base read` which
streams raw bytes. Ids come back as `{\"id\"}`, an outcome-only verb as `{\"ok\":true}`.";

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
  /// The fleet to join, when this daemon is a fleet node (§2.6 boot step 6); a laptop has none.
  pub fleet: Option<FleetSelection>,
}

/// Which fleet node this process is: the operator's shared manifest and this node's name in it
/// (`--fleet PATH --node NAME`; `slates_server::deploy` derives everything else from the two).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FleetSelection {
  /// The path of the shared manifest.
  pub manifest: String,
  /// This node's name in it.
  pub node: String,
}

/// Options of `profile`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProfileOptions {
  /// Quick.
  pub quick: bool,
  /// JSON instead of the derived constants.
  pub json: bool,
}

/// Options of `mcp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpOptions {
  /// The daemon instance to connect to.
  pub instance: String,
  /// A loopback port to serve Streamable HTTP on, instead of stdio.
  pub http: Option<u16>,
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
  /// Promote a lost region's declared mirror on the root group (§4.8, D-14 — operator-initiated region-loss
  /// promotion). Issued on the root leader, after the operator judges the region truly lost.
  PromoteRegion {
    /// The lost region's id.
    region: u64,
  },
  /// Status (`volume stat`, `status ID`).
  Status {
    /// The volume.
    volume: slates_client::VolumeId,
    /// Drifted entries only.
    drift: bool,
  },
  /// Mount a volume at a path over the loopback NFS bridge (`mount ID PATH`).
  Mount {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The mount point: an existing user-owned directory.
    path: String,
  },
  /// Unmount a loopback bridge mount at a path (`unmount PATH`).
  Unmount {
    /// The mount point.
    path: String,
  },
  /// Snapshot.
  Snapshot {
    /// The volume.
    volume: slates_client::VolumeId,
  },
  /// Destroy a snapshot.
  DestroySnapshot {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The snapshot.
    snapshot: slates_client::SnapshotId,
  },
  /// Create a green volume (§4.16 merge), from scratch or over a complete immutable base.
  Green {
    /// The name.
    name: String,
    /// Whether an increment must carry evidence.
    evidence: bool,
    /// The complete immutable base (a volume's snapshot), or none for a scratch green.
    base: Option<GreenBase>,
  },
  /// Re-pin a green attachment to a version, or the head.
  Advance {
    /// The attachment.
    attachment: u64,
    /// The version, or the head when none.
    version: Option<u64>,
  },
  /// A file's bytes at a view: a green's head, a version, or an attachment's pinned version.
  Read {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The file.
    path: String,
    /// The view.
    at: ReadAt,
  },
  /// A green's head version.
  Versions {
    /// The green.
    green: slates_client::VolumeId,
  },
  /// The files a green changed since a version.
  ChangedSince {
    /// The green.
    green: slates_client::VolumeId,
    /// The base version.
    version: u64,
  },
  /// Create a work volume over a green.
  Work {
    /// The green.
    green: slates_client::VolumeId,
    /// The name.
    name: String,
  },
  /// Declare an edit on a work volume (a splice; the inserted bytes are the text argument).
  Edit {
    /// The work volume.
    work: slates_client::VolumeId,
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// Bytes removed at the offset.
    delete_len: u64,
    /// Bytes inserted.
    bytes: Vec<u8>,
  },
  /// Submit a work volume's increment to its green.
  Submit {
    /// The work volume.
    work: slates_client::VolumeId,
    /// The evidence references (opaque identities), possibly none.
    evidence: Vec<[u8; 32]>,
  },
  /// Rebase a work volume onto its green's head (the corrective path).
  Rebase {
    /// The work volume.
    work: slates_client::VolumeId,
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
    /// The form: the record under the root mount, or a container bind of the host mount at
    /// `--oci-source` to `--oci-destination` (§4.6 A-9).
    form: slates_client::AttachRequest,
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
  /// A clean base file's verified content digest (§4.15).
  Digest {
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
  /// Issue a grant for a presented landing (`grant LANDING MANIFEST`): the human's approval of the
  /// exact manifest `land` showed, proven under the anchor's issuer secret (§4.13).
  Grant {
    /// The presented landing's id.
    landing: u64,
    /// The manifest hash the human approves — the one `land` printed.
    manifest: [u8; 32],
    /// Once (the default), or every landing of the volume into the target for the session.
    scope: GrantScope,
    /// The grant's validity, nanoseconds from issue.
    term_ns: u64,
  },
  /// Enroll a consumer under an account (`enroll`): the human surface's verb, proven under the
  /// anchor's issuer secret (§4.13 "Principals"); the capability is shown once.
  Enroll {
    /// The account, or the caller's own when none is given.
    account: Option<u32>,
  },
  /// Revoke a consumer's enrollment (`revoke CONSUMER`): the human surface's verb.
  Revoke {
    /// The consumer.
    consumer: u64,
  },
  /// Set a principal's rights on a volume (`share ID PRINCIPAL [--read] [--write] [--admin]`).
  Share {
    /// The volume.
    volume: slates_client::VolumeId,
    /// The principal.
    principal: SharePrincipal,
    /// The rights; all off removes the entry.
    rights: Rights,
  },
}

/// A principal as `share` names it: a host account (`uid:N`), or a consumer — under the caller's own
/// account (`consumer:N`) or an explicit one (`consumer:ACCOUNT/N`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SharePrincipal {
  /// A host account.
  Uid(u32),
  /// An enrolled consumer.
  Consumer {
    /// The account, or the caller's own when none is given.
    account: Option<u32>,
    /// The consumer id.
    consumer: u64,
  },
}

/// Derived: a grant's default validity, in nanoseconds — one hour, the span §4.15's session scope
/// names as the longest a human's single approval should stand without renewal (an agent that lands
/// later than that re-presents its manifest for a fresh look). `--term SECONDS` overrides it.
pub(crate) const GRANT_TERM_NS: u64 = 3_600 * 1_000_000_000;

/// A client request: the instance and the verb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientRequest {
  /// The instance.
  pub instance: String,
  /// The verb.
  pub verb: Verb,
  /// Emit machine-readable JSON instead of the plain text form (the read verbs; §4.12 "consistent
  /// JSON"). The `--json` switch, allowed on any client verb like `--instance`.
  pub json: bool,
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

/// A `slates run` request: the harness verb (§4.13) — enroll a consumer, deliver its capability to
/// the command on an inherited descriptor, wait, and revoke it unless kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunRequest {
  /// The instance the command's client will find (`SLATES_ENDPOINT` in its environment).
  pub instance: String,
  /// Keep the enrollment after the command ends (default: revoke it).
  pub keep: bool,
  /// Announce the consumer as JSON.
  pub json: bool,
  /// The command and its arguments.
  pub command: Vec<String>,
}

/// The parsed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
  /// The launcher.
  Exec(ExecRequest),
  /// The harness verb.
  Run(RunRequest),
  /// The anchor.
  Anchor(ProcessOptions),
  /// The daemon.
  Daemon(ProcessOptions),
  /// The profile.
  Profile(ProfileOptions),
  /// The MCP server (stdio, or Streamable HTTP on a loopback port).
  Mcp(McpOptions),
  /// A client verb.
  Client(ClientRequest),
}

/// Every flag that takes a value, across the verbs.
const VALUES: &[&str] = &[
  "--instance",
  "--shards",
  "--fleet",
  "--node",
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
  "--oci-source",
  "--oci-destination",
  "--http",
  "--term",
  "--evidence",
  "--version",
  "--attachment",
  "--account",
];
/// Every switch, across the verbs.
const SWITCHES: &[&str] = &[
  "--quick",
  "--json",
  "--fold",
  "--locked",
  "--read",
  "--write",
  "--admin",
  "--drift",
  "--mirror",
  "--session",
  "--require-evidence",
  "--keep",
];

/// Splits `rest` at `--`: the flags before it, the command after it (`exec` and `run` share it).
fn split_at_command(rest: &[String]) -> Result<(&[String], Vec<String>), ParseError> {
  let Some(split) = rest.iter().position(|a| a == "--") else {
    return Err(ParseError::Missing("`--` then the command"));
  };
  let command = rest[split + 1..].to_vec();
  if command.is_empty() {
    return Err(ParseError::Missing("a command after `--`"));
  }
  Ok((&rest[..split], command))
}

/// Parses `run [--keep] [--json] [--instance NAME] -- CMD ...`.
fn parse_run(rest: &[String]) -> Result<Command, ParseError> {
  let (head, command) = split_at_command(rest)?;
  let taken = take(head)?;
  taken.only(&Spec {
    values: &[],
    switches: &["--keep"],
  })?;
  Ok(Command::Run(RunRequest {
    instance: taken.instance(),
    keep: taken.switch("--keep"),
    json: taken.json(),
    command,
  }))
}

/// Parses `exec --volume V --at PATH -- CMD ...`: the flags before `--`, the command after it.
fn parse_exec(rest: &[String]) -> Result<Command, ParseError> {
  let (head, command) = split_at_command(rest)?;
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
      .chain(
        self
          .switches
          .keys()
          .filter(|k| **k != "--json" && !spec.switches.contains(k)),
      )
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

  /// Whether `--json` was given: a global switch (like `--instance`) allowed on any client verb.
  fn json(&self) -> bool {
    self.switch("--json")
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

/// `--fleet PATH --node NAME`, both or neither: a manifest without the node to be in it, or a node
/// without the manifest naming it, is a usage error.
fn fleet_of(taken: &Taken) -> Result<Option<FleetSelection>, ParseError> {
  match (taken.value("--fleet"), taken.value("--node")) {
    (Some(manifest), Some(node)) => Ok(Some(FleetSelection {
      manifest: manifest.to_owned(),
      node: node.to_owned(),
    })),
    (Some(_), None) => Err(ParseError::Missing(
      "--node NAME (which node of the fleet this is)",
    )),
    (None, Some(_)) => Err(ParseError::Missing("--fleet PATH (the fleet manifest)")),
    (None, None) => Ok(None),
  }
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

/// Format: a second in nanoseconds — `--term SECONDS` is the operator's unit, the wire's is nanoseconds.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Format: a manifest hash is 32 bytes (BLAKE3's output), printed by `land` as hexadecimal.
const MANIFEST_BYTES: usize = 32;
/// Format: hexadecimal — two characters per byte, radix sixteen.
const HEX_CHARS_PER_BYTE: usize = 2;
/// Format: see `HEX_CHARS_PER_BYTE`.
const HEX_RADIX: u32 = 16;

/// The manifest hash `land` printed, as hexadecimal characters back into its bytes — the human re-states
/// exactly the hash they saw, so the approval binds that manifest and no other (§4.13).
fn manifest_hash(text: &str) -> Result<[u8; MANIFEST_BYTES], ParseError> {
  let bad = |reason: &str| ParseError::BadValue {
    what: "MANIFEST",
    reason: reason.to_owned(),
  };
  let bytes = text.as_bytes();
  if bytes.len() != MANIFEST_BYTES * HEX_CHARS_PER_BYTE {
    return Err(bad("64 hexadecimal characters"));
  }
  let mut out = [0u8; MANIFEST_BYTES];
  let (pairs, _) = bytes.as_chunks::<HEX_CHARS_PER_BYTE>();
  for (index, pair) in pairs.iter().enumerate() {
    let hex = std::str::from_utf8(pair).map_err(|_| bad("hexadecimal"))?;
    out[index] = u8::from_str_radix(hex, HEX_RADIX).map_err(|_| bad("hexadecimal"))?;
  }
  Ok(out)
}

fn snapshot(text: &str) -> Result<slates_client::SnapshotId, ParseError> {
  parse_snapshot(text).map_err(|reason| ParseError::BadValue {
    what: "SNAPSHOT",
    reason,
  })
}

fn number(text: &str) -> Result<u64, ParseError> {
  text.parse().map_err(|_| ParseError::BadValue {
    what: "a number",
    reason: text.to_owned(),
  })
}

fn account(text: &str) -> Result<u32, ParseError> {
  text.parse().map_err(|_| ParseError::BadValue {
    what: "an account (uid)",
    reason: text.to_owned(),
  })
}

/// `uid:N`, `consumer:N` or `consumer:ACCOUNT/N`.
fn share_principal(text: &str) -> Result<SharePrincipal, ParseError> {
  let bad = || ParseError::BadValue {
    what: "PRINCIPAL",
    reason: format!("{text} (uid:N, consumer:N or consumer:ACCOUNT/N)"),
  };
  let (kind, value) = text.split_once(':').ok_or_else(bad)?;
  match kind {
    "uid" => Ok(SharePrincipal::Uid(account(value).map_err(|_| bad())?)),
    "consumer" => match value.split_once('/') {
      Some((owner, consumer)) => Ok(SharePrincipal::Consumer {
        account: Some(account(owner).map_err(|_| bad())?),
        consumer: number(consumer).map_err(|_| bad())?,
      }),
      None => Ok(SharePrincipal::Consumer {
        account: None,
        consumer: number(value).map_err(|_| bad())?,
      }),
    },
    _ => Err(bad()),
  }
}

/// The verbs of the enrollment surface (§4.13): `enroll`, `revoke`, `share`.
fn parse_enrollment(taken: &Taken, words: &[&str]) -> Result<Option<Command>, ParseError> {
  match words {
    ["enroll"] => {
      taken.only(&Spec {
        values: &["--account"],
        switches: &[],
      })?;
      let account = taken.value("--account").map(account).transpose()?;
      Ok(Some(client(taken, Verb::Enroll { account })))
    }
    ["revoke", consumer] => {
      taken.only(&NONE)?;
      Ok(Some(client(
        taken,
        Verb::Revoke {
          consumer: number(consumer)?,
        },
      )))
    }
    ["share", id, principal] => {
      taken.only(&Spec {
        values: &[],
        switches: &["--read", "--write", "--admin"],
      })?;
      Ok(Some(client(
        taken,
        Verb::Share {
          volume: volume(id)?,
          principal: share_principal(principal)?,
          rights: Rights {
            read: taken.switch("--read"),
            write: taken.switch("--write"),
            admin: taken.switch("--admin"),
          },
        },
      )))
    }
    ["revoke"] => Err(ParseError::Missing("CONSUMER")),
    ["share", ..] => Err(ParseError::Missing("share ID PRINCIPAL")),
    _ => Ok(None),
  }
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
    json: taken.json(),
  })
}

const NONE: Spec = Spec {
  values: &[],
  switches: &[],
};

/// `green NAME [--require-evidence] [--base VOLUME --snapshot N]`: a green from scratch, or over the
/// complete immutable base the pair names (one without the other is a usage refusal).
fn parse_green(taken: &Taken, name: &str) -> Result<Command, ParseError> {
  taken.only(&Spec {
    values: &["--base", "--snapshot"],
    switches: &["--require-evidence"],
  })?;
  let base = match (taken.value("--base"), taken.value("--snapshot")) {
    (None, None) => None,
    (Some(base), Some(snap)) => Some(GreenBase {
      volume: volume(base)?,
      snapshot: snapshot(snap)?,
    }),
    _ => {
      return Err(ParseError::BadValue {
        what: "--base",
        reason: "--base VOLUME and --snapshot N go together".to_owned(),
      });
    }
  };
  Ok(client(
    taken,
    Verb::Green {
      name: name.to_owned(),
      evidence: taken.switch("--require-evidence"),
      base,
    },
  ))
}

/// `advance ATTACHMENT [VERSION]`: the head when no version is given.
fn parse_advance(
  taken: &Taken,
  attachment: &str,
  version: Option<&str>,
) -> Result<Command, ParseError> {
  taken.only(&NONE)?;
  Ok(client(
    taken,
    Verb::Advance {
      attachment: number(attachment)?,
      version: version.map(number).transpose()?,
    },
  ))
}

/// `read VOLUME PATH [--version N | --attachment A]`: the head when neither is given.
fn parse_read(taken: &Taken, id: &str, path: &str) -> Result<Command, ParseError> {
  taken.only(&Spec {
    values: &["--version", "--attachment"],
    switches: &[],
  })?;
  let at = match (taken.value("--version"), taken.value("--attachment")) {
    (Some(version), _) => ReadAt::Version {
      version: number(version)?,
    },
    (None, Some(attachment)) => ReadAt::Attachment {
      attachment: number(attachment)?,
    },
    (None, None) => ReadAt::Head,
  };
  Ok(client(
    taken,
    Verb::Read {
      volume: volume(id)?,
      path: path.to_owned(),
      at,
    },
  ))
}

/// `submit WORK [--evidence HEX]`: the evidence reference, when given, as the 64-hex identity.
fn parse_submit(taken: &Taken, work: &str) -> Result<Command, ParseError> {
  taken.only(&Spec {
    values: &["--evidence"],
    switches: &[],
  })?;
  let evidence = taken
    .value("--evidence")
    .map(manifest_hash)
    .transpose()?
    .into_iter()
    .collect();
  Ok(client(
    taken,
    Verb::Submit {
      work: volume(work)?,
      evidence,
    },
  ))
}

/// Parses the arguments (without the program name).
pub(crate) fn parse(arguments: &[String]) -> Result<Command, ParseError> {
  if arguments.first().map(String::as_str) == Some("exec") {
    return parse_exec(&arguments[1..]);
  }
  if arguments.first().map(String::as_str) == Some("run") {
    return parse_run(&arguments[1..]);
  }
  let taken = take(arguments)?;
  let words: Vec<&str> = taken.words.iter().map(String::as_str).collect();
  if let Some(command) = parse_enrollment(&taken, &words)? {
    return Ok(command);
  }
  match words.as_slice() {
    [] => Err(ParseError::Help),
    ["anchor" | "daemon"] => {
      taken.only(&Spec {
        values: &["--shards", "--fleet", "--node"],
        switches: &["--quick"],
      })?;
      let options = ProcessOptions {
        instance: taken.instance(),
        quick: taken.switch("--quick"),
        shards: shards_of(&taken)?,
        fleet: fleet_of(&taken)?,
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
    ["mcp"] => {
      taken.only(&Spec {
        values: &["--http"],
        switches: &[],
      })?;
      let http = match taken.value("--http") {
        Some(port) => Some(
          u16::try_from(number(port)?).map_err(|_| ParseError::BadValue {
            what: "a port",
            reason: port.to_owned(),
          })?,
        ),
        None => None,
      };
      Ok(Command::Mcp(McpOptions {
        instance: taken.instance(),
        http,
      }))
    }
    ["volume", rest @ ..] => parse_volume(&taken, rest),
    ["attach", id] => {
      taken.only(&Spec {
        values: &["--snapshot", "--oci-source", "--oci-destination"],
        switches: &["--read", "--write"],
      })?;
      let snapshot = taken.value("--snapshot").map(snapshot).transpose()?;
      let intent = if taken.switch("--write") {
        Intent::Write
      } else {
        Intent::Read
      };
      let form = match (
        taken.value("--oci-source"),
        taken.value("--oci-destination"),
      ) {
        (Some(source), Some(destination)) => slates_client::AttachRequest::Oci {
          source: source.to_owned(),
          destination: destination.to_owned(),
        },
        (None, None) => slates_client::AttachRequest::Root,
        (Some(_), None) => return Err(ParseError::Missing("--oci-destination CONTAINER_PATH")),
        (None, Some(_)) => return Err(ParseError::Missing("--oci-source HOST_PATH")),
      };
      Ok(client(
        &taken,
        Verb::Attach {
          volume: volume(id)?,
          snapshot,
          intent,
          form,
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
    ["promote-region", region] => {
      taken.only(&NONE)?;
      let region = region.parse::<u64>().map_err(|e| ParseError::BadValue {
        what: "region",
        reason: e.to_string(),
      })?;
      Ok(client(&taken, Verb::PromoteRegion { region }))
    }
    ["mount", id, path] => {
      taken.only(&NONE)?;
      Ok(client(
        &taken,
        Verb::Mount {
          volume: volume(id)?,
          path: (*path).to_owned(),
        },
      ))
    }
    ["mount", ..] => Err(ParseError::Missing("mount ID PATH")),
    ["unmount", path] => {
      taken.only(&NONE)?;
      Ok(client(
        &taken,
        Verb::Unmount {
          path: (*path).to_owned(),
        },
      ))
    }
    ["unmount"] => Err(ParseError::Missing("unmount PATH")),
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
    ["grant", landing, manifest] => {
      taken.only(&Spec {
        values: &["--term"],
        switches: &["--session"],
      })?;
      let landing = landing.parse::<u64>().map_err(|e| ParseError::BadValue {
        what: "LANDING",
        reason: e.to_string(),
      })?;
      let manifest = manifest_hash(manifest)?;
      let scope = if taken.switch("--session") {
        GrantScope::Session
      } else {
        GrantScope::Once
      };
      let term_ns = match taken.value("--term") {
        Some(seconds) => seconds
          .parse::<u64>()
          .map_err(|e| ParseError::BadValue {
            what: "--term",
            reason: e.to_string(),
          })?
          .saturating_mul(NANOS_PER_SECOND),
        None => GRANT_TERM_NS,
      };
      Ok(client(
        &taken,
        Verb::Grant {
          landing,
          manifest,
          scope,
          term_ns,
        },
      ))
    }
    ["grant", ..] => Err(ParseError::Missing("grant LANDING MANIFEST")),
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
    ["green", name] => parse_green(&taken, name),
    ["advance", attachment, rest @ ..] if rest.len() <= 1 => {
      parse_advance(&taken, attachment, rest.first().copied())
    }
    ["read", id, path] => parse_read(&taken, id, path),
    ["read", ..] => Err(ParseError::Missing("VOLUME PATH")),
    ["versions", id] => {
      taken.only(&NONE)?;
      Ok(client(&taken, Verb::Versions { green: volume(id)? }))
    }
    ["changed-since", id, version] => {
      taken.only(&NONE)?;
      Ok(client(
        &taken,
        Verb::ChangedSince {
          green: volume(id)?,
          version: number(version)?,
        },
      ))
    }
    ["work", green, name] => {
      taken.only(&NONE)?;
      Ok(client(
        &taken,
        Verb::Work {
          green: volume(green)?,
          name: (*name).to_owned(),
        },
      ))
    }
    ["edit", work, path, at, delete, text] => {
      taken.only(&NONE)?;
      Ok(client(
        &taken,
        Verb::Edit {
          work: volume(work)?,
          path: (*path).to_owned(),
          at: number(at)?,
          delete_len: number(delete)?,
          bytes: (*text).as_bytes().to_vec(),
        },
      ))
    }
    ["submit", work] => parse_submit(&taken, work),
    ["rebase", work] => {
      taken.only(&NONE)?;
      Ok(client(
        &taken,
        Verb::Rebase {
          work: volume(work)?,
        },
      ))
    }
    [verb, rest @ ..]
      if matches!(
        *verb,
        "anchor" | "daemon" | "profile" | "mcp" | "attach" | "detach" | "status" | "enroll"
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
    ["destroy-snapshot", id, snap] => {
      taken.only(&NONE)?;
      Ok(client(
        taken,
        Verb::DestroySnapshot {
          volume: volume(id)?,
          snapshot: snapshot(snap)?,
        },
      ))
    }
    ["destroy-snapshot", ..] => Err(ParseError::Missing("ID SNAPSHOT")),
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
    ["digest", id, path] => Ok(client(
      taken,
      Verb::Digest {
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
    ["read" | "digest", ..] => Err(ParseError::Missing("ID PATH")),
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
        form: slates_client::AttachRequest::Root,
      }
    );
    assert_eq!(
      parse(&args("anchor --quick --shards 2 --instance z")),
      Ok(Command::Anchor(ProcessOptions {
        instance: "z".into(),
        quick: true,
        shards: Some(2),
        fleet: None,
      }))
    );
    assert_eq!(
      parse(&args("mcp --instance m")),
      Ok(Command::Mcp(McpOptions {
        instance: "m".into(),
        http: None,
      }))
    );
    assert_eq!(
      parse(&args("mcp --instance m --http 8787")),
      Ok(Command::Mcp(McpOptions {
        instance: "m".into(),
        http: Some(8787),
      }))
    );
  }

  /// The container bind form of `attach` (§4.6 A-9): both paths make the form; one alone is a usage
  /// refusal naming the missing flag.
  #[test]
  fn the_attach_grammar_takes_the_container_bind_flags_together() {
    let Ok(Command::Client(request)) = parse(&args(
      "attach 00000000000000000000000000000001 --write --oci-source /Users/me/mnt --oci-destination /work",
    )) else {
      panic!("client");
    };
    assert_eq!(
      request.verb,
      Verb::Attach {
        volume: parse_volume_id("00000000000000000000000000000001").unwrap(),
        snapshot: None,
        intent: Intent::Write,
        form: slates_client::AttachRequest::Oci {
          source: "/Users/me/mnt".to_owned(),
          destination: "/work".to_owned(),
        },
      }
    );
    assert_eq!(
      parse(&args(
        "attach 00000000000000000000000000000001 --oci-source /Users/me/mnt"
      )),
      Err(ParseError::Missing("--oci-destination CONTAINER_PATH"))
    );
    assert_eq!(
      parse(&args(
        "attach 00000000000000000000000000000001 --oci-destination /work"
      )),
      Err(ParseError::Missing("--oci-source HOST_PATH"))
    );
  }

  /// `base digest ID PATH` parses to the clean-file digest verb (§4.15); the path is required.
  #[test]
  fn the_grammar_parses_base_digest() {
    let Command::Client(request) = parse(&args(
      "base digest 00000000000000000000000000000001 /src/lib.rs --json",
    ))
    .unwrap() else {
      panic!("client");
    };
    assert!(request.json);
    assert_eq!(
      request.verb,
      Verb::Digest {
        volume: parse_volume_id("00000000000000000000000000000001").unwrap(),
        path: "/src/lib.rs".into(),
      }
    );
    assert_eq!(
      parse(&args("base digest 00000000000000000000000000000001")),
      Err(ParseError::Missing("ID PATH"))
    );
  }

  /// `promote-region N` parses to the operator's region-loss promotion (§4.8, D-14 — "the operator issues it
  /// (a CLI verb over this)"); a non-integer region is a usage error naming the bad value.
  #[test]
  fn the_grammar_parses_promote_region() {
    let Command::Client(request) = parse(&args("promote-region 1 --instance a")).unwrap() else {
      panic!("client");
    };
    assert_eq!(request.instance, "a");
    assert_eq!(request.verb, Verb::PromoteRegion { region: 1 });
    assert!(matches!(
      parse(&args("promote-region notaregion")),
      Err(ParseError::BadValue { what: "region", .. })
    ));
  }

  /// A fleet node (§2.6 boot step 6): `--fleet PATH --node NAME` on the daemon or the anchor, both or
  /// neither — one without the other is a usage error naming the missing one.
  #[test]
  fn the_grammar_parses_a_fleet_node() {
    assert_eq!(
      parse(&args(
        "daemon --instance a --fleet /etc/slates/fleet.json --node a"
      )),
      Ok(Command::Daemon(ProcessOptions {
        instance: "a".into(),
        quick: false,
        shards: None,
        fleet: Some(FleetSelection {
          manifest: "/etc/slates/fleet.json".into(),
          node: "a".into(),
        }),
      }))
    );
    assert!(matches!(
      parse(&args("daemon --fleet fleet.json")),
      Err(ParseError::Missing(_))
    ));
    assert!(matches!(
      parse(&args("anchor --node a")),
      Err(ParseError::Missing(_))
    ));
  }

  /// The merge service's grammar (§4.16 as a service): a green over a complete immutable base with
  /// evidence required (one of the base pair alone is a usage refusal), an advance to the head or a
  /// version, a read at the head, a version or an attachment, and a submit carrying evidence.
  #[test]
  fn the_grammar_parses_the_base_reader_and_evidence_forms() {
    let verb = |text: &str| -> Verb {
      let Command::Client(request) = parse(&args(text)).unwrap() else {
        panic!("client");
      };
      request.verb
    };
    let id = "00000000000000000000000000000001";
    let vid = parse_volume_id(id).unwrap();
    assert_eq!(
      verb(&format!(
        "green over --require-evidence --base {id} --snapshot 3"
      )),
      Verb::Green {
        name: "over".into(),
        evidence: true,
        base: Some(GreenBase {
          volume: vid,
          snapshot: slates_client::SnapshotId { value: 3 },
        }),
      }
    );
    assert!(matches!(
      parse(&args(&format!("green half --base {id}"))),
      Err(ParseError::BadValue { .. })
    ));
    assert_eq!(
      verb("advance 7"),
      Verb::Advance {
        attachment: 7,
        version: None,
      }
    );
    assert_eq!(
      verb("advance 7 2"),
      Verb::Advance {
        attachment: 7,
        version: Some(2),
      }
    );
    assert_eq!(
      verb(&format!("read {id} /f --attachment 7")),
      Verb::Read {
        volume: vid,
        path: "/f".into(),
        at: ReadAt::Attachment { attachment: 7 },
      }
    );
    assert_eq!(
      verb(&format!("read {id} f --version 2")),
      Verb::Read {
        volume: vid,
        path: "f".into(),
        at: ReadAt::Version { version: 2 },
      }
    );
    assert_eq!(
      verb(&format!("submit {id} --evidence {}", "ab".repeat(32))),
      Verb::Submit {
        work: vid,
        evidence: vec![[0xab; 32]],
      }
    );
  }

  /// The merge grammar (§4.16): green, work, edit, submit, versions and changed-since parse to
  /// their verbs.
  #[test]
  fn the_grammar_parses_the_merge_verbs() {
    let verb = |text: &str| -> Verb {
      let Command::Client(request) = parse(&args(text)).unwrap() else {
        panic!("client");
      };
      request.verb
    };
    let id = "00000000000000000000000000000001";
    let vid = parse_volume_id(id).unwrap();
    assert_eq!(
      verb("green shared"),
      Verb::Green {
        name: "shared".into(),
        evidence: false,
        base: None,
      }
    );
    assert_eq!(
      verb(&format!("versions {id}")),
      Verb::Versions { green: vid }
    );
    assert_eq!(
      verb(&format!("changed-since {id} 2")),
      Verb::ChangedSince {
        green: vid,
        version: 2,
      }
    );
    assert_eq!(
      verb(&format!("work {id} w")),
      Verb::Work {
        green: vid,
        name: "w".into(),
      }
    );
    assert_eq!(
      verb(&format!("edit {id} f 0 5 hello")),
      Verb::Edit {
        work: vid,
        path: "f".into(),
        at: 0,
        delete_len: 5,
        bytes: b"hello".to_vec(),
      }
    );
    assert_eq!(
      verb(&format!("submit {id}")),
      Verb::Submit {
        work: vid,
        evidence: Vec::new(),
      }
    );
    assert_eq!(verb(&format!("rebase {id}")), Verb::Rebase { work: vid });
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

  /// The verb a client-verb text parses to.
  fn client_verb(text: &str) -> Verb {
    let Command::Client(request) = parse(&args(text)).unwrap() else {
      panic!("client");
    };
    request.verb
  }

  /// The enrollment surface (§4.13): `enroll` with or without an account, `revoke`, and `share` with
  /// a principal in each of its three forms and any rights; a bad principal and a missing consumer
  /// are refused.
  #[test]
  fn the_grammar_parses_the_enrollment_verbs() {
    assert_eq!(client_verb("enroll"), Verb::Enroll { account: None });
    assert_eq!(
      client_verb("enroll --account 501 --json"),
      Verb::Enroll { account: Some(501) }
    );
    assert_eq!(client_verb("revoke 7"), Verb::Revoke { consumer: 7 });
    let id = "00000000000000000000000000000001";
    let vid = parse_volume_id(id).unwrap();
    let share = |principal: SharePrincipal, rights: Rights| Verb::Share {
      volume: vid,
      principal,
      rights,
    };
    assert_eq!(
      client_verb(&format!("share {id} consumer:7 --read")),
      share(
        SharePrincipal::Consumer {
          account: None,
          consumer: 7,
        },
        Rights {
          read: true,
          write: false,
          admin: false,
        },
      )
    );
    assert_eq!(
      client_verb(&format!("share {id} consumer:501/7 --write --admin")),
      share(
        SharePrincipal::Consumer {
          account: Some(501),
          consumer: 7,
        },
        Rights {
          read: false,
          write: true,
          admin: true,
        },
      )
    );
    assert_eq!(
      client_verb(&format!("share {id} uid:501")),
      share(SharePrincipal::Uid(501), Rights::default())
    );
    assert!(matches!(
      parse(&args(&format!("share {id} nobody:1"))),
      Err(ParseError::BadValue {
        what: "PRINCIPAL",
        ..
      })
    ));
    assert_eq!(parse(&args("revoke")), Err(ParseError::Missing("CONSUMER")));
  }

  /// `run` splits at `--` with its switches before it; a missing `--` or an empty command is refused.
  #[test]
  fn run_splits_the_switches_from_the_command() {
    assert_eq!(
      parse(&args("run --instance x --keep --json -- sh -c true")),
      Ok(Command::Run(RunRequest {
        instance: "x".into(),
        keep: true,
        json: true,
        command: vec!["sh".into(), "-c".into(), "true".into()],
      }))
    );
    assert!(matches!(
      parse(&args("run -- ")),
      Err(ParseError::Missing("a command after `--`"))
    ));
    assert!(matches!(
      parse(&args("run sh")),
      Err(ParseError::Missing("`--` then the command"))
    ));
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
