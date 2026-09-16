//! `cargo xtask conformance` — the I/O half of the conformance evidence surface (AC-9.7/T-9.1,
//! GAP-A9-15; the pure half is `crates/conformance`). It drives the real `slates` binary the way
//! `crates/cli/tests/cli.rs` does — an anchor supervising a daemon, a volume provisioned through
//! the CLI, a real kernel mount — runs one named suite inside a bounded scratch directory in the
//! mount, tears everything down, and writes one typed record per (transport × suite) into the
//! records directory `docs/wip/conformance.md` is regenerated from. Every cell it cannot run on
//! this host gets a `SKIPPED` record with the typed reason from the capability table, so the
//! matrix never claims what did not run.
//!
//! Subcommands: `plan` (write the skip records for every cell this host cannot run), `run
//! --suite S` (run one suite over this host's native transport), `all` (plan, then every runnable
//! suite, continuing past failures), `matrix [--write]` (render the matrix from the records;
//! `--write` rewrites the document's generated block), `tally --outputs DIR [--privilege
//! root|unprivileged]` (re-read a kept `pjdfstest-output` directory — a `--keep` scratch here, or
//! the CI lane's uploaded artifact — and print the counts, the judgement, and the failures by
//! shape and by file, so a run that happened elsewhere is reviewed by a command). Scratch lives
//! outside the tree (`--scratch`, else a fresh `mktemp -d`) and is removed at the end unless
//! `--keep`.
//!
//! This is a development tool, not shipped code: it writes its scratch and its records with
//! `std::fs`, each such site allowed in place with the reason, exactly as the ratchet task does.
//! The daemon it drives stays unprivileged (R10); where a suite itself needs root (a Linux NFS
//! client mount, pjdfstest's uid switches, macOS `fs_usage`) the harness asks `sudo -n` and
//! records a privilege skip when it is refused, never a prompt.

mod fetch;
mod hermeticity;
mod slates;
mod suites;
mod workloads;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slates_conformance::capability::{Availability, HostOs, RootNeed, availability};
use slates_conformance::civil::CivilDate;
use slates_conformance::record::{
  Host, ListDigest, Outcome, Privilege, Record, SCHEMA, SkipReason,
};
use slates_conformance::{Suite, Transport, matrix};

use crate::Failure;

/// Shape: the busy timeout the sqlite workload waits out a sibling's lock with, milliseconds; the
/// two inserts race by design and the loser must wait rather than fail.
const SQLITE_BUSY_MS: u64 = 5_000;
/// Shape: how long the watcher workloads wait for their event, seconds.
const WATCH_SECONDS: u64 = 3;
/// Shape: fsx's default operation count — enough to exercise every operation class many times
/// (fsx picks among read/write/mapread/mapwrite/truncate uniformly) inside a minute over loopback.
const FSX_OPERATIONS: u64 = 10_000;
/// Shape: fsx's default seed (its own default is 1; recorded so a run is reproducible).
const FSX_SEED: u64 = 1;
/// Shape: fsx's default file-length bound, its own default (`-l`, 262144 bytes).
const FSX_FILE_LENGTH: u64 = 262_144;
/// Shape: fsstress's default operations per process.
const FSSTRESS_OPERATIONS: u64 = 500;
/// Shape: fsstress's default process count — several writers racing in one tree.
const FSSTRESS_PROCESSES: u32 = 4;
/// Shape: fsstress's default seed.
const FSSTRESS_SEED: u64 = 1;
/// Shape: the bounded size of the volume a suite runs in — room for fsstress's tree, a cargo
/// target directory and pjdfstest's scratch, well under a CI runner's RAM.
const VOLUME_SIZE: &str = "512MiB";
/// Shape: a `sudo -n true` probe's wait (it answers at once or refuses at once).
const SUDO_PROBE: Duration = Duration::from_secs(5);

/// The parsed command line.
#[derive(Debug)]
pub(crate) struct Options {
  /// `plan`, `run`, `all`, `matrix` or `tally`.
  command: String,
  /// The records directory.
  records: PathBuf,
  /// The scratch directory, or a fresh one.
  scratch: Option<PathBuf>,
  /// Keep the scratch directory.
  keep: bool,
  /// `run`: the suite.
  suite: Option<Suite>,
  /// `matrix`: rewrite the document.
  write: bool,
  /// `tally`: the kept `pjdfstest-output` directory to re-read.
  outputs: Option<PathBuf>,
  /// `tally`: who ran the outputs (`root` or `unprivileged`); this process's own identity when
  /// absent.
  privilege: Option<Privilege>,
  /// The exerciser bounds.
  bounds: Bounds,
}

/// The recorded bounds of the exerciser runs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
  fsx_operations: u64,
  fsx_seed: u64,
  fsx_file_length: u64,
  fsstress_operations: u64,
  fsstress_processes: u32,
  fsstress_seed: u64,
}

impl Default for Bounds {
  fn default() -> Self {
    Bounds {
      fsx_operations: FSX_OPERATIONS,
      fsx_seed: FSX_SEED,
      fsx_file_length: FSX_FILE_LENGTH,
      fsstress_operations: FSSTRESS_OPERATIONS,
      fsstress_processes: FSSTRESS_PROCESSES,
      fsstress_seed: FSSTRESS_SEED,
    }
  }
}

fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
  args
    .iter()
    .position(|a| a == flag)
    .and_then(|i| args.get(i + 1))
    .map(String::as_str)
}

fn number<T: std::str::FromStr>(args: &[String], flag: &str, default: T) -> Result<T, Failure> {
  match value_after(args, flag) {
    Some(text) => text
      .parse()
      .map_err(|_| Failure(format!("{flag} takes a number, got `{text}`"))),
    None => Ok(default),
  }
}

/// Parses the arguments after `conformance`.
pub(crate) fn parse(root: &Path, args: &[String]) -> Result<Options, Failure> {
  let command = args.first().cloned().ok_or_else(|| {
    Failure("conformance: a subcommand is needed: plan, run, all, matrix, tally".to_owned())
  })?;
  let privilege = match value_after(args, "--privilege") {
    Some("root") => Some(Privilege::Root),
    Some("unprivileged") => Some(Privilege::Unprivileged),
    Some(other) => {
      return Err(Failure(format!(
        "--privilege takes root or unprivileged, got `{other}`"
      )));
    }
    None => None,
  };
  let defaults = Bounds::default();
  let suite = match value_after(args, "--suite") {
    Some(slug) => Some(Suite::parse(slug).ok_or_else(|| {
      Failure(format!(
        "unknown suite `{slug}`; suites: {}",
        Suite::ALL.map(Suite::slug).join(", ")
      ))
    })?),
    None => None,
  };
  Ok(Options {
    command,
    records: value_after(args, "--records")
      .map_or_else(|| root.join("docs/wip/conformance/records"), PathBuf::from),
    scratch: value_after(args, "--scratch").map(PathBuf::from),
    keep: args.iter().any(|a| a == "--keep"),
    suite,
    write: args.iter().any(|a| a == "--write"),
    outputs: value_after(args, "--outputs").map(PathBuf::from),
    privilege,
    bounds: Bounds {
      fsx_operations: number(args, "--fsx-ops", defaults.fsx_operations)?,
      fsx_seed: number(args, "--fsx-seed", defaults.fsx_seed)?,
      fsx_file_length: number(args, "--fsx-length", defaults.fsx_file_length)?,
      fsstress_operations: number(args, "--fsstress-ops", defaults.fsstress_operations)?,
      fsstress_processes: number(args, "--fsstress-procs", defaults.fsstress_processes)?,
      fsstress_seed: number(args, "--fsstress-seed", defaults.fsstress_seed)?,
    },
  })
}

/// Runs the subcommand.
pub(crate) fn run(root: &Path, options: &Options) -> Result<(), Failure> {
  match options.command.as_str() {
    "plan" => plan(options),
    "run" => {
      let suite = options
        .suite
        .ok_or_else(|| Failure("conformance run: --suite is needed".to_owned()))?;
      run_suite(root, options, suite)
    }
    "all" => all(root, options),
    "matrix" => render_matrix(root, options),
    "tally" => tally(root, options),
    other => Err(Failure(format!(
      "conformance: unknown subcommand `{other}`; plan, run, all, matrix, tally"
    ))),
  }
}

/// `tally --outputs DIR [--privilege root|unprivileged]`: review kept pjdfstest outputs.
fn tally(root: &Path, options: &Options) -> Result<(), Failure> {
  let outputs = options
    .outputs
    .as_deref()
    .ok_or_else(|| Failure("conformance tally: --outputs DIR is needed".to_owned()))?;
  let runner = match options.privilege {
    Some(Privilege::Root) => suites::Runner::Root,
    Some(Privilege::Unprivileged) => match suites::this_user() {
      suites::Runner::Root => {
        return Err(Failure(
          "conformance tally: --privilege unprivileged, but this process is root; the classifier \
           needs the uid and groups the run had"
            .to_owned(),
        ));
      }
      user => user,
    },
    None => suites::this_user(),
  };
  suites::tally_outputs(root, native_transport(host_os()?), outputs, &runner)
}

// --- the host ---------------------------------------------------------------------------------

/// This host's operating system.
fn host_os() -> Result<HostOs, Failure> {
  if cfg!(target_os = "macos") {
    Ok(HostOs::Macos)
  } else if cfg!(target_os = "linux") {
    Ok(HostOs::Linux)
  } else if cfg!(target_os = "windows") {
    Ok(HostOs::Windows)
  } else {
    Err(Failure(
      "conformance: this operating system has no lane".to_owned(),
    ))
  }
}

/// The native transport this host offers.
fn native_transport(os: HostOs) -> Transport {
  match os {
    HostOs::Macos => Transport::NativeMacosNfs,
    HostOs::Linux => Transport::NativeLinuxFuse,
    HostOs::Windows => Transport::NativeWindowsWinfsp,
  }
}

/// The trimmed stdout of a command, or empty when it cannot run.
pub(crate) fn stdout_of(program: &str, args: &[&str]) -> String {
  Command::new(program)
    .args(args)
    .output()
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    .unwrap_or_default()
}

/// The host facts a record carries.
fn host_facts(privilege: Privilege) -> Host {
  let os = match host_os() {
    Ok(HostOs::Macos) => format!(
      "macOS {} ({})",
      stdout_of("sw_vers", &["-productVersion"]),
      stdout_of("sw_vers", &["-buildVersion"])
    ),
    Ok(HostOs::Linux) => std::fs::read_to_string("/etc/os-release")
      .ok()
      .and_then(|text| {
        text.lines().find_map(|l| {
          l.strip_prefix("PRETTY_NAME=")
            .map(|v| v.trim_matches('"').to_owned())
        })
      })
      .unwrap_or_else(|| "Linux".to_owned()),
    _ => std::env::consts::OS.to_owned(),
  };
  Host {
    os,
    kernel: stdout_of("uname", &["-sr"]),
    arch: stdout_of("uname", &["-m"]),
    privilege,
  }
}

/// Whether `name` is an executable on the `PATH`.
pub(crate) fn tool_on_path(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Whether this process is root.
fn is_root() -> bool {
  stdout_of("id", &["-u"]) == "0"
}

/// Whether `sudo -n` works without a prompt (the CI runners; never this laptop).
pub(crate) fn sudo_without_prompt() -> bool {
  let Ok(mut child) = Command::new("sudo")
    .args(["-n", "true"])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
  else {
    return false;
  };
  let started = std::time::Instant::now();
  loop {
    match child.try_wait() {
      Ok(Some(status)) => return status.success(),
      Ok(None) if started.elapsed() < SUDO_PROBE => pause(),
      _ => {
        let _ = child.kill();
        let _ = child.wait();
        return false;
      }
    }
  }
}

/// Shape: the pause between polls of a child process.
const POLL: Duration = Duration::from_millis(20);

/// Paces the harness's polls of its child processes; shipped code parks on its driver (D-9).
pub(crate) fn pause() {
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

/// Today's civil date (UTC), for the record.
fn today() -> String {
  let seconds = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    .unwrap_or(0);
  CivilDate::from_unix(seconds).iso()
}

// --- files (the development tool's own writes) -----------------------------------------------

/// Writes a file in the scratch or records directory. xtask is a development tool writing its
/// own outputs outside the shipped code paths, like the ratchet task; shipped code never reaches this.
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, bytes).map_err(|e| Failure(format!("writing {}: {e}", path.display())))
}

/// Creates a directory (and its parents) in the scratch or records tree; same reason as [`write_file`].
pub(crate) fn create_dir(path: &Path) -> Result<(), Failure> {
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir_all(path).map_err(|e| Failure(format!("creating {}: {e}", path.display())))
}

/// Removes a scratch tree; same reason as [`write_file`]. Best effort: a leftover scratch is reported.
pub(crate) fn remove_tree(path: &Path) {
  #[allow(clippy::disallowed_methods)]
  if let Err(e) = std::fs::remove_dir_all(path) {
    eprintln!("conformance: scratch {} not removed: {e}", path.display());
  }
}

/// A fresh scratch directory outside the tree (`--scratch DIR`, else `mktemp -d`), removed on drop
/// unless kept.
pub(crate) struct Scratch {
  path: PathBuf,
  keep: bool,
}

impl Scratch {
  fn open(options: &Options) -> Result<Scratch, Failure> {
    let path = match &options.scratch {
      Some(dir) => {
        create_dir(dir)?;
        dir.clone()
      }
      None => {
        let made = stdout_of("mktemp", &["-d", "-t", "slates-conformance"]);
        if made.is_empty() {
          return Err(Failure("mktemp -d failed; pass --scratch DIR".to_owned()));
        }
        PathBuf::from(made)
      }
    };
    let path = std::fs::canonicalize(&path)
      .map_err(|e| Failure(format!("scratch {}: {e}", path.display())))?;
    Ok(Scratch {
      path,
      keep: options.keep,
    })
  }

  /// The scratch directory.
  pub(crate) fn path(&self) -> &Path {
    &self.path
  }

  /// A fresh subdirectory of the scratch.
  pub(crate) fn subdir(&self, name: &str) -> Result<PathBuf, Failure> {
    let path = self.path.join(name);
    create_dir(&path)?;
    Ok(path)
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    if self.keep {
      eprintln!("conformance: scratch kept at {}", self.path.display());
    } else {
      remove_tree(&self.path);
    }
  }
}

// --- records ----------------------------------------------------------------------------------

/// Every record in the directory (a file that is not a record is reported and skipped).
pub(crate) fn load_records(dir: &Path) -> Result<Vec<Record>, Failure> {
  let mut records = Vec::new();
  let Ok(entries) = std::fs::read_dir(dir) else {
    return Ok(records);
  };
  let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
  paths.sort();
  for path in paths {
    if path.extension().is_none_or(|x| x != "json") {
      continue;
    }
    let bytes =
      std::fs::read(&path).map_err(|e| Failure(format!("reading {}: {e}", path.display())))?;
    match Record::parse(&bytes) {
      Ok(record) => records.push(record),
      Err(e) => eprintln!("conformance: {} skipped: {e}", path.display()),
    }
  }
  Ok(records)
}

/// Stores a record unless it would downgrade evidence already recorded for its cell.
fn store_record(dir: &Path, record: &Record) -> Result<(), Failure> {
  create_dir(dir)?;
  let path = dir.join(record.file_name());
  if let Ok(bytes) = std::fs::read(&path)
    && let Ok(existing) = Record::parse(&bytes)
    && let Err(reason) = record.may_replace(&existing)
  {
    println!("conformance: kept {}: {reason}", record.file_name());
    return Ok(());
  }
  let json = record.to_json().map_err(|e| Failure(e.to_string()))?;
  write_file(&path, json.as_bytes())?;
  println!("conformance: wrote {}", path.display());
  Ok(())
}

/// A skip record for a cell.
fn skip_record(transport: Transport, suite: Suite, reason: SkipReason) -> Record {
  Record {
    schema: SCHEMA,
    suite,
    transport,
    host: host_facts(if is_root() {
      Privilege::Root
    } else {
      Privilege::Unprivileged
    }),
    date: today(),
    command: String::new(),
    bound: String::new(),
    outcome: Outcome::Skipped { reason },
    expected_failure_list: None,
    duration_ms: 0,
    notes: Vec::new(),
  }
}

/// What this host can do with a cell right now.
enum Readiness {
  /// Runnable here; root available as stated.
  Ready { root: bool },
  /// Not runnable here, for this typed reason.
  Skip(SkipReason),
}

/// Decides a cell for this host from the capability table and the host's tools and privilege.
fn readiness(os: HostOs, transport: Transport, suite: Suite) -> Readiness {
  match availability(transport, suite) {
    Availability::Owed(reason) => Readiness::Skip(SkipReason::Owed(reason.to_owned())),
    Availability::NotApplicable(reason) => {
      Readiness::Skip(SkipReason::NotApplicable(reason.to_owned()))
    }
    Availability::Runnable { on, .. } if on != os => Readiness::Skip(SkipReason::Lane(format!(
      "runs in the {} lane, not on {}",
      on.lane(),
      os.lane()
    ))),
    Availability::Runnable { tools, root, .. } => {
      if let Some(missing) = tools.iter().find(|t| !tool_on_path(t)) {
        return Readiness::Skip(SkipReason::Tool(format!(
          "`{missing}` is not on the PATH of this host"
        )));
      }
      let root_available = is_root() || sudo_without_prompt();
      match root {
        RootNeed::Required(reason) if !root_available => Readiness::Skip(SkipReason::Privilege(
          format!("{reason}; this host has no passwordless sudo (`sudo -n true` was refused)"),
        )),
        _ => Readiness::Ready {
          root: root_available,
        },
      }
    }
  }
}

/// `plan`: a skip record for every cell this host cannot run; the runnable ones are listed.
fn plan(options: &Options) -> Result<(), Failure> {
  let os = host_os()?;
  let mut runnable = Vec::new();
  for transport in Transport::ALL {
    for suite in Suite::ALL {
      match readiness(os, transport, suite) {
        Readiness::Ready { root } => runnable.push(format!(
          "{} × {}{}",
          transport.slug(),
          suite.slug(),
          if root {
            " (root available)"
          } else {
            " (no root)"
          }
        )),
        Readiness::Skip(reason) => {
          store_record(&options.records, &skip_record(transport, suite, reason))?
        }
      }
    }
  }
  println!(
    "conformance: runnable here: {}",
    if runnable.is_empty() {
      "nothing".to_owned()
    } else {
      runnable.join("; ")
    }
  );
  Ok(())
}

/// The context one suite runs in.
pub(crate) struct Run<'a> {
  pub(crate) root: &'a Path,
  pub(crate) options: &'a Options,
  pub(crate) os: HostOs,
  pub(crate) transport: Transport,
  pub(crate) root_available: bool,
  pub(crate) scratch: Scratch,
}

impl Run<'_> {
  /// The recorded bounds.
  pub(crate) fn bounds(&self) -> Bounds {
    self.options.bounds
  }

  /// The privilege the suite's own processes run with.
  pub(crate) fn privilege(&self) -> Privilege {
    if self.root_available {
      Privilege::Root
    } else {
      Privilege::Unprivileged
    }
  }

  /// The adapter this lane must use, if any (recorded `LIMITED`).
  pub(crate) fn adapter(&self, suite: Suite) -> Option<(String, String)> {
    match availability(self.transport, suite) {
      Availability::Runnable {
        adapter: Some(adapter),
        ..
      } => Some((adapter.name.to_owned(), adapter.not_covered.to_owned())),
      _ => None,
    }
  }
}

/// What a suite run hands back for its record.
pub(crate) struct SuiteResult {
  pub(crate) outcome: Outcome,
  pub(crate) command: String,
  pub(crate) bound: String,
  pub(crate) expected_failure_list: Option<ListDigest>,
  pub(crate) notes: Vec<String>,
  /// Whether the run met its verdict (the process exit code).
  pub(crate) ok: bool,
}

/// `run --suite S`: one suite over this host's native transport.
fn run_suite(root: &Path, options: &Options, suite: Suite) -> Result<(), Failure> {
  let os = host_os()?;
  let transport = native_transport(os);
  let root_available = match readiness(os, transport, suite) {
    Readiness::Ready { root } => root,
    Readiness::Skip(reason) => {
      println!(
        "conformance: {} × {} skipped: {}: {}",
        transport.slug(),
        suite.slug(),
        reason.class(),
        reason.text()
      );
      return store_record(&options.records, &skip_record(transport, suite, reason));
    }
  };
  let run = Run {
    root,
    options,
    os,
    transport,
    root_available,
    scratch: Scratch::open(options)?,
  };
  let started = std::time::Instant::now();
  let result = match suite {
    Suite::Fsx => suites::run_fsx(&run)?,
    Suite::Fsstress => suites::run_fsstress(&run)?,
    Suite::Pjdfstest => suites::run_pjdfstest(&run)?,
    Suite::Workloads => workloads::run_workloads(&run)?,
    Suite::Hermeticity => hermeticity::run_hermeticity(&run)?,
    Suite::Pressure | Suite::Failure => {
      return Err(Failure(format!(
        "{} has no runnable form yet",
        suite.slug()
      )));
    }
  };
  // The scratch directory is a temporary path; the recorded command names it as `<scratch>` so the
  // contract of the number reads the same on every host.
  let scratch_text = run.scratch.path().display().to_string();
  let record = Record {
    schema: SCHEMA,
    suite,
    transport,
    host: host_facts(run.privilege()),
    date: today(),
    command: result.command.replace(&scratch_text, "<scratch>"),
    bound: result.bound,
    outcome: result.outcome,
    expected_failure_list: result.expected_failure_list,
    duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    notes: result.notes,
  };
  store_record(&options.records, &record)?;
  println!(
    "conformance: {} × {}: {}",
    transport.slug(),
    suite.slug(),
    matrix::cell(Some(&record))
  );
  if result.ok {
    Ok(())
  } else {
    Err(Failure(format!(
      "{} over {} did not meet its verdict (the record was written)",
      suite.slug(),
      transport.slug()
    )))
  }
}

/// `all`: plan, then every runnable suite, continuing past failures.
fn all(root: &Path, options: &Options) -> Result<(), Failure> {
  plan(options)?;
  let mut failures = Vec::new();
  for suite in [
    Suite::Fsx,
    Suite::Fsstress,
    Suite::Pjdfstest,
    Suite::Workloads,
    Suite::Hermeticity,
  ] {
    if let Err(e) = run_suite(root, options, suite) {
      eprintln!("conformance: {}: {e}", suite.slug());
      failures.push(suite.slug());
    }
  }
  if failures.is_empty() {
    Ok(())
  } else {
    Err(Failure(format!(
      "suites that did not meet their verdict: {}",
      failures.join(", ")
    )))
  }
}

/// `matrix [--write]`: render from the records; rewrite the document's block on request.
fn render_matrix(root: &Path, options: &Options) -> Result<(), Failure> {
  let records = load_records(&options.records)?;
  let rendered = matrix::render(&records);
  if options.write {
    let document_path = root.join("docs/wip/conformance.md");
    let document = std::fs::read_to_string(&document_path)
      .map_err(|e| Failure(format!("reading {}: {e}", document_path.display())))?;
    let rewritten = matrix::rewrite(&document, &rendered).map_err(|e| Failure(e.to_owned()))?;
    write_file(&document_path, rewritten.as_bytes())?;
    println!(
      "conformance: wrote the matrix into {}",
      document_path.display()
    );
  } else {
    print!("{rendered}");
  }
  Ok(())
}
