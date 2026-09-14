//! The typed run record: what one harness run of one suite over one transport produced (Part 6
//! "Conformance", "Real workloads", example 8; AC-9.7 "publish capability-specific results").
//! A record is the only input to the evidence matrix, so its shape is the honesty rule made
//! structural: a `RAN` cell carries counts, a date, the host and the exact command or it cannot
//! be built; a `LIMITED` cell names its adapter and what the adapter leaves uncovered; a
//! `SKIPPED` cell carries a typed reason (the lane it belongs to, the tool it lacked, the
//! hardware or privilege it needed, or the product piece still owed).
//!
//! Records are JSON files, one per (transport × suite), named `<transport>.<suite>.json`; the
//! parser refuses a file past its cap before allocating and refuses a foreign schema rather than
//! guessing at it. A record from a later run replaces an earlier one only if it does not
//! downgrade evidence: a skip never overwrites a run ([`Record::may_replace`]), so a laptop's
//! `plan` cannot erase what a CI lane proved.

use serde::{Deserialize, Serialize};

/// Format: the record schema version; a record with another schema is refused, never guessed at.
pub const SCHEMA: u32 = 1;

/// Derived: the most bytes a record file may hold before it is parsed. The widest record (a
/// workload run: every tool of the roster with a detail line capped at [`DETAIL_CHARS`]) is under
/// 8 KiB; the cap is eight times that, for notes and later fields (Part 6 "Hostile input").
pub const MAX_RECORD_BYTES: usize = 64 * 1024;

/// Shape: the longest detail a result keeps (a suite's own message, truncated), so a record
/// stays readable and bounded whatever a suite prints.
pub const DETAIL_CHARS: usize = 512;

/// An offered transport (§4.6): the five the release must have evidence for (AC-9.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
  /// macOS, the daemon's NFSv3 loopback server under a kernel `mount_nfs` (no privilege).
  NativeMacosNfs,
  /// Linux, the `/dev/fuse` bridge.
  NativeLinuxFuse,
  /// Windows, the WinFsp volume host.
  NativeWindowsWinfsp,
  /// A Linux guest over the owned FUSE-over-virtio device.
  VirtioFs,
  /// An OCI container consuming an established host attachment.
  Oci,
}

impl Transport {
  /// Every transport, in the matrix's row order.
  pub const ALL: [Transport; 5] = [
    Transport::NativeMacosNfs,
    Transport::NativeLinuxFuse,
    Transport::NativeWindowsWinfsp,
    Transport::VirtioFs,
    Transport::Oci,
  ];

  /// The file-name and command-line form.
  pub fn slug(self) -> &'static str {
    match self {
      Transport::NativeMacosNfs => "native-macos-nfs",
      Transport::NativeLinuxFuse => "native-linux-fuse",
      Transport::NativeWindowsWinfsp => "native-windows-winfsp",
      Transport::VirtioFs => "virtio-fs",
      Transport::Oci => "oci",
    }
  }

  /// The matrix's row label.
  pub fn label(self) -> &'static str {
    match self {
      Transport::NativeMacosNfs => "native macOS (NFS loopback)",
      Transport::NativeLinuxFuse => "native Linux (FUSE)",
      Transport::NativeWindowsWinfsp => "native Windows (WinFsp)",
      Transport::VirtioFs => "virtio-fs guest",
      Transport::Oci => "OCI container",
    }
  }

  /// The transport a slug names.
  pub fn parse(slug: &str) -> Option<Transport> {
    Transport::ALL.into_iter().find(|t| t.slug() == slug)
  }
}

/// A suite of the matrix's columns (Part 6: the three conformance suites, the workloads, the
/// hermeticity tracer, the pressure and failure suites).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Suite {
  /// POSIX conformance: pjdfstest's TAP suite with the reviewed expected-failure list.
  Pjdfstest,
  /// The file-system exerciser (read/write/truncate/mmap against an in-memory oracle).
  Fsx,
  /// The randomized namespace stress (mkdir/create/rename/link/unlink/… across processes).
  Fsstress,
  /// Real tools inside the mount, byte-identical to the host.
  Workloads,
  /// The filesystem-write tracer: zero writes outside a granted landing target.
  Hermeticity,
  /// Memory-pressure behaviour (Part 6 "Soak and scale").
  Pressure,
  /// Fault injection on real processes (Part 6).
  Failure,
}

impl Suite {
  /// Every suite, in the matrix's column order.
  pub const ALL: [Suite; 7] = [
    Suite::Pjdfstest,
    Suite::Fsx,
    Suite::Fsstress,
    Suite::Workloads,
    Suite::Hermeticity,
    Suite::Pressure,
    Suite::Failure,
  ];

  /// The file-name and command-line form.
  pub fn slug(self) -> &'static str {
    match self {
      Suite::Pjdfstest => "pjdfstest",
      Suite::Fsx => "fsx",
      Suite::Fsstress => "fsstress",
      Suite::Workloads => "workloads",
      Suite::Hermeticity => "hermeticity",
      Suite::Pressure => "pressure",
      Suite::Failure => "failure",
    }
  }

  /// The matrix's column label.
  pub fn label(self) -> &'static str {
    match self {
      Suite::Pjdfstest => "POSIX conformance (pjdfstest)",
      Suite::Fsx => "fsx",
      Suite::Fsstress => "fsstress",
      Suite::Workloads => "workloads",
      Suite::Hermeticity => "hermeticity",
      Suite::Pressure => "pressure",
      Suite::Failure => "failure suites",
    }
  }

  /// The suite a slug names.
  pub fn parse(slug: &str) -> Option<Suite> {
    Suite::ALL.into_iter().find(|s| s.slug() == slug)
  }
}

/// Whether the run had root (some suites need it; the daemon never does, R10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Privilege {
  /// The suite ran as root (the daemon still ran unprivileged).
  Root,
  /// The suite ran as an ordinary user.
  Unprivileged,
}

/// The host a record was made on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
  /// The operating system and version (`sw_vers`, `/etc/os-release`, `ver`).
  pub os: String,
  /// The kernel (`uname -sr`).
  pub kernel: String,
  /// The architecture (`uname -m`).
  pub arch: String,
  /// The privilege the suite ran with.
  pub privilege: Privilege,
}

/// Why a cell was not run, typed (AC-9.7: "report skipped lanes and limited adapters honestly").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reason", rename_all = "kebab-case")]
pub enum SkipReason {
  /// The cell runs in another CI lane (another operating system).
  Lane(String),
  /// A tool the suite needs is absent on this host.
  Tool(String),
  /// The hardware or environment the cell needs is absent (a guest, a container runtime).
  Hardware(String),
  /// A privilege the suite (never the daemon) needs is absent.
  Privilege(String),
  /// The product piece the cell would exercise does not exist yet.
  Owed(String),
  /// The suite does not apply to the transport (a POSIX C suite on Windows).
  NotApplicable(String),
}

impl SkipReason {
  /// The reason's class, as the matrix prints it.
  pub fn class(&self) -> &'static str {
    match self {
      SkipReason::Lane(_) => "lane",
      SkipReason::Tool(_) => "tool",
      SkipReason::Hardware(_) => "hardware",
      SkipReason::Privilege(_) => "privilege",
      SkipReason::Owed(_) => "owed",
      SkipReason::NotApplicable(_) => "not applicable",
    }
  }

  /// The reason's text.
  pub fn text(&self) -> &str {
    match self {
      SkipReason::Lane(t)
      | SkipReason::Tool(t)
      | SkipReason::Hardware(t)
      | SkipReason::Privilege(t)
      | SkipReason::Owed(t)
      | SkipReason::NotApplicable(t) => t,
    }
  }
}

/// One tool of the workload roster and how its mounted run compared with its host run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadResult {
  /// The workload's name (`git`, `cargo`, …).
  pub name: String,
  /// The comparison.
  pub status: WorkloadStatus,
}

/// The byte-identity verdict of one workload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WorkloadStatus {
  /// Outputs and the resulting tree were identical, byte for byte, under the reviewed exclusions.
  Identical,
  /// Something differed; `detail` names the first difference.
  Differs {
    /// The first difference found.
    detail: String,
  },
  /// The tool is absent on this host.
  Skipped {
    /// The tool that was absent.
    tool: String,
  },
}

/// The counts a run produced, per suite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "suite", rename_all = "kebab-case")]
pub enum Counts {
  /// pjdfstest: TAP files and cases, and how the reviewed list judged the failures.
  Pjdfstest {
    /// TAP files run.
    files: u32,
    /// Cases seen.
    cases: u32,
    /// Cases that passed.
    passed: u32,
    /// Cases that failed for a reason other than privilege.
    failed: u32,
    /// Cases that failed only because the run was not root.
    needs_root: u32,
    /// Cases the suite itself marks TODO.
    todo: u32,
    /// Failures the reviewed list expects.
    expected_failures: u32,
    /// Failures the reviewed list does not expect (a regression, or an unreviewed case).
    unexpected_failures: u32,
    /// Listed expectations that now pass (the list must shrink).
    listed_now_passing: u32,
  },
  /// fsx: the bound and whether the exerciser's own verification passed.
  Fsx {
    /// `-N`, the operation count.
    operations: u64,
    /// `-S`, the seed.
    seed: u64,
    /// `-l`, the file-length bound in bytes.
    file_length: u64,
    /// The exerciser reported every operation verified.
    ok: bool,
  },
  /// fsstress: the bound and whether every process finished cleanly.
  Fsstress {
    /// `-n`, operations per process.
    operations: u64,
    /// `-p`, processes.
    processes: u32,
    /// `-s`, the seed.
    seed: u64,
    /// Operations the log shows completed (the `-v` lines).
    logged_operations: u64,
    /// Operations disabled on this host (`-f op=0`), with the reason recorded in the notes.
    disabled_operations: Vec<String>,
    /// Every process exited 0 and the daemon answered afterwards.
    ok: bool,
  },
  /// The workloads, one verdict per roster tool.
  Workloads {
    /// The roster's results.
    tools: Vec<WorkloadResult>,
  },
  /// The tracer's classification of every write-capable call the traced processes made.
  Hermeticity {
    /// Write-capable calls seen.
    write_calls: u64,
    /// Calls on paths inside the granted target.
    inside_target: u64,
    /// Calls on RAM-only kernel objects (memfd, shm, sockets, pipes, devices).
    ram_only: u64,
    /// Writes to the processes' own standard streams.
    standard_streams: u64,
    /// Calls the tracer could not attribute to any path (fs_usage prints none for some fds).
    unresolved: u64,
    /// Calls on a path outside every allowed class — violations; must be zero.
    outside: u64,
    /// Paths written inside the target that the landing report lists as `Written`.
    written_matched: u32,
    /// Paths written inside the target that the landing report does not list.
    written_unmatched: u32,
  },
}

/// What a run produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Outcome {
  /// The suite ran over the transport as offered.
  Ran {
    /// The counts.
    counts: Counts,
  },
  /// The suite ran over an adapter that is not the offered transport, or with a reduced scope.
  Limited {
    /// The adapter that stood in (a root NFS mount by the OS client, a non-root subset).
    adapter: String,
    /// What the adapter does not cover.
    not_covered: String,
    /// The counts.
    counts: Counts,
  },
  /// The suite did not run.
  Skipped {
    /// Why.
    reason: SkipReason,
  },
}

/// The digest of the reviewed expected-failure list a run was judged against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDigest {
  /// The list's path in the repository.
  pub path: String,
  /// The BLAKE3 of its canonical form, hexadecimal.
  pub blake3: String,
  /// How many expectations it holds.
  pub entries: u32,
}

/// One run of one suite over one transport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
  /// [`SCHEMA`].
  pub schema: u32,
  /// The suite.
  pub suite: Suite,
  /// The transport.
  pub transport: Transport,
  /// The host.
  pub host: Host,
  /// The civil date of the run, `YYYY-MM-DD` (UTC).
  pub date: String,
  /// The exact command the harness ran for the suite (the contract of the number).
  pub command: String,
  /// The bound the run was held to (operation counts, seeds, file counts), in plain words.
  pub bound: String,
  /// What happened.
  pub outcome: Outcome,
  /// The reviewed list the failures were judged against, when the suite has one.
  pub expected_failure_list: Option<ListDigest>,
  /// Wall time of the suite, milliseconds.
  pub duration_ms: u64,
  /// Facts worth the reader's eye that the counts do not carry.
  pub notes: Vec<String>,
}

/// Why a record could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
  /// The file is past [`MAX_RECORD_BYTES`]; refused before parsing.
  TooLarge {
    /// The file's length.
    len: usize,
    /// The cap.
    cap: usize,
  },
  /// The bytes are not a record (the JSON error, in words).
  Json(String),
  /// The record's schema is not this build's.
  Schema {
    /// The schema found.
    found: u32,
    /// The schema expected.
    expected: u32,
  },
}

impl std::fmt::Display for RecordError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RecordError::TooLarge { len, cap } => {
        write!(f, "record of {len} bytes exceeds the cap of {cap}")
      }
      RecordError::Json(text) => write!(f, "record is not valid: {text}"),
      RecordError::Schema { found, expected } => {
        write!(f, "record schema {found} is not this build's {expected}")
      }
    }
  }
}

impl Record {
  /// The file name a record is kept under: `<transport>.<suite>.json`.
  pub fn file_name(&self) -> String {
    format!("{}.{}.json", self.transport.slug(), self.suite.slug())
  }

  /// The record as pretty JSON, with a trailing newline.
  pub fn to_json(&self) -> Result<String, RecordError> {
    serde_json::to_string_pretty(self)
      .map(|text| format!("{text}\n"))
      .map_err(|e| RecordError::Json(e.to_string()))
  }

  /// Parses a record, refusing an oversized file before allocation and a foreign schema after.
  pub fn parse(bytes: &[u8]) -> Result<Record, RecordError> {
    if bytes.len() > MAX_RECORD_BYTES {
      return Err(RecordError::TooLarge {
        len: bytes.len(),
        cap: MAX_RECORD_BYTES,
      });
    }
    let record: Record =
      serde_json::from_slice(bytes).map_err(|e| RecordError::Json(e.to_string()))?;
    if record.schema != SCHEMA {
      return Err(RecordError::Schema {
        found: record.schema,
        expected: SCHEMA,
      });
    }
    Ok(record)
  }

  /// Whether the record is evidence of a run (`RAN` or `LIMITED`) rather than a skip.
  pub fn is_evidence(&self) -> bool {
    !matches!(self.outcome, Outcome::Skipped { .. })
  }

  /// Whether this record may replace `existing` in the record set: a skip never overwrites
  /// evidence (the reason names what would be lost); anything else replaces.
  pub fn may_replace(&self, existing: &Record) -> Result<(), String> {
    if existing.is_evidence() && !self.is_evidence() {
      return Err(format!(
        "a skip does not replace evidence: {} already holds a run from {} on {}",
        existing.file_name(),
        existing.date,
        existing.host.os
      ));
    }
    Ok(())
  }
}

/// Truncates a suite's message to [`DETAIL_CHARS`] characters, marking the cut.
pub fn detail(text: &str) -> String {
  let mut out: String = text.chars().take(DETAIL_CHARS).collect();
  if text.chars().count() > DETAIL_CHARS {
    out.push('…');
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn host() -> Host {
    Host {
      os: "macOS 26.4.1".to_owned(),
      kernel: "Darwin 25.4.0".to_owned(),
      arch: "arm64".to_owned(),
      privilege: Privilege::Unprivileged,
    }
  }

  fn run(suite: Suite, outcome: Outcome) -> Record {
    Record {
      schema: SCHEMA,
      suite,
      transport: Transport::NativeMacosNfs,
      host: host(),
      date: "2026-09-14".to_owned(),
      command: "fsx -N 1000 -S 1 f".to_owned(),
      bound: "1000 operations, seed 1".to_owned(),
      outcome,
      expected_failure_list: None,
      duration_ms: 12,
      notes: vec![],
    }
  }

  /// A record round-trips through its JSON and is named by its transport and suite.
  #[test]
  fn a_record_round_trips_and_names_its_file() {
    let record = run(
      Suite::Fsx,
      Outcome::Ran {
        counts: Counts::Fsx {
          operations: 1000,
          seed: 1,
          file_length: 262_144,
          ok: true,
        },
      },
    );
    let json = record.to_json().unwrap();
    assert_eq!(Record::parse(json.as_bytes()).unwrap(), record);
    assert_eq!(record.file_name(), "native-macos-nfs.fsx.json");
    assert!(record.is_evidence());
  }

  /// Hostile input: garbage, an oversized file and a foreign schema are typed refusals.
  #[test]
  fn hostile_input_is_refused_typed() {
    assert!(matches!(
      Record::parse(b"not json"),
      Err(RecordError::Json(_))
    ));
    let big = vec![b' '; MAX_RECORD_BYTES + 1];
    assert!(matches!(
      Record::parse(&big),
      Err(RecordError::TooLarge { .. })
    ));
    let mut record = run(
      Suite::Fsx,
      Outcome::Skipped {
        reason: SkipReason::Tool("cc".to_owned()),
      },
    );
    record.schema = SCHEMA + 1;
    let json = record.to_json().unwrap();
    assert!(matches!(
      Record::parse(json.as_bytes()),
      Err(RecordError::Schema { .. })
    ));
  }

  /// A skip never replaces evidence; evidence replaces anything; a skip replaces a skip.
  #[test]
  fn a_skip_never_replaces_evidence() {
    let ran = run(
      Suite::Fsx,
      Outcome::Ran {
        counts: Counts::Fsx {
          operations: 1,
          seed: 1,
          file_length: 1,
          ok: true,
        },
      },
    );
    let skip = run(
      Suite::Fsx,
      Outcome::Skipped {
        reason: SkipReason::Lane("macOS".to_owned()),
      },
    );
    assert!(skip.may_replace(&ran).is_err());
    assert!(ran.may_replace(&skip).is_ok());
    assert!(skip.may_replace(&skip).is_ok());
    assert!(ran.may_replace(&ran).is_ok());
  }

  /// Every transport and suite slug parses back to itself, so file names are unambiguous.
  #[test]
  fn slugs_round_trip() {
    for transport in Transport::ALL {
      assert_eq!(Transport::parse(transport.slug()), Some(transport));
    }
    for suite in Suite::ALL {
      assert_eq!(Suite::parse(suite.slug()), Some(suite));
    }
    assert_eq!(Transport::parse("nope"), None);
  }

  /// A detail is cut at the cap and marked.
  #[test]
  fn details_are_bounded() {
    let long = "x".repeat(DETAIL_CHARS * 2);
    let cut = detail(&long);
    assert_eq!(cut.chars().count(), DETAIL_CHARS + 1);
    assert!(cut.ends_with('…'));
    assert_eq!(detail("short"), "short");
  }
}
