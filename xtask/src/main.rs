//! Workspace tasks that enforce Part 0 of the design by construction.
//!
//! - `cargo xtask structural` — the structural test (AC-0.1): for every shipped `slates-*` crate,
//!   the resolved dependency set must not contain a forbidden crate (an async runtime, a lock
//!   library, an `Arc`-based concurrency crate), the sources must not name `std::fs`, `std::net`,
//!   `Arc`, `Rc`, `Mutex` or `RwLock` outside the crates allowed to, and no write-capable file
//!   syscall may appear outside the landing crate. Test code (`tests/`, `benches/`, `examples/`,
//!   and `#[cfg(test)]` modules) is exempt: tests write only to RAM-backed directories they name.
//! - `cargo xtask literals` — the literal check (AC-0.2, R3): a numeric literal in shipped code
//!   must carry its derivation. Allowed forms: inside a `derived!(...)` call; on or within four
//!   lines after a doc line marked `Derived:`, `Measured:`, `Format:` or `Shape:`; the values 0, 1
//!   and 2; bit widths in shifts and type contexts; attributes; array indices. Everything else
//!   fails with its file and line.
//! - `cargo xtask unsafe [--tighten]` — the unsafe budget per crate (`unsafe-budget.toml`),
//!   which only tightens.
//! - `cargo xtask version [--write | --expect-tag TAG]` — the one version and every package that
//!   carries it (see `version.rs`): the workspace version is the only hand-edited one; the Python
//!   wheel derives it through maturin; the Node main package, its platform packages and its
//!   `optionalDependencies` pins must equal it and are re-derived by `--write`; `--expect-tag` is
//!   the release guard (the tag must be `v` + the version).
//! - `cargo xtask npm-reserve --out DIR` — the 0.0.0 stub packages a maintainer publishes once to
//!   create each npm name, derived from the same manifests (see `version.rs`).
//! - `cargo xtask check` — structural, literals, the unsafe budget and the version.
//! - `cargo xtask ratchet [--record] [--tighten] [--reset] [--runs N]` — the performance
//!   ratchet over the bench examples, keyed by machine identity (see `ratchet.rs`).
//! - `cargo xtask conformance (plan | run --suite S | all | matrix [--write]) [--records DIR]
//!   [--scratch DIR] [--keep]` — the conformance evidence harness (see `conformance/mod.rs`).
//! - `cargo xtask kind (image | smoke) [--tag TAG] [--keep]` — the KIND fleet lane: the image, one node
//!   of it in Docker (see `kind.rs`; docs/wip/kind-lane.md).
//! - `cargo xtask tsan` — the ThreadSanitizer lane: the race canary must be reported, then the
//!   threaded crates' tests run instrumented and any report fails (see `tsan.rs`; needs nightly
//!   with `rust-src`).
//!
//! This is a development tool, not shipped code. It reads sources and runs cargo, so it is the one
//! place in the workspace where `std::fs` reads and `std::process` are ordinary; it still obeys the
//! no-panic law (typed errors, exit codes) so a broken tree reports rather than crashes.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod callgrind;
mod capabilities;
mod conformance;
mod kind;
mod ratchet;
mod tsan;
mod unsafe_budget;
mod version;

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

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let task = args.first().map(String::as_str).unwrap_or("check");
  let outcome = match task {
    "structural" => structural::run(),
    "literals" => literals::run(),
    "unsafe" => workspace_root()
      .and_then(|root| unsafe_budget::run(&root, args.iter().any(|a| a == "--tighten"))),
    "check" => structural::run()
      .and_then(|()| literals::run())
      .and_then(|()| workspace_root().and_then(|root| unsafe_budget::run(&root, false)))
      .and_then(|()| workspace_root().and_then(|root| version::run(&root, &version::Mode::Check)))
      .and_then(|()| workspace_root().and_then(|root| check_capabilities(&root))),
    "callgrind" => workspace_root().and_then(|root| {
      let iai = args
        .iter()
        .position(|a| a == "--iai")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target").join("iai"));
      callgrind::run(&root, &iai, args.iter().any(|a| a == "--record"))
    }),
    "capabilities" => workspace_root().and_then(|root| {
      if args.iter().any(|a| a == "--write") {
        capabilities::write(&root).map_err(Failure)?;
      }
      check_capabilities(&root)
    }),
    "version" => workspace_root().and_then(|root| {
      let mode = version::mode_from(&args[1..])?;
      version::run(&root, &mode)
    }),
    "npm-reserve" => workspace_root().and_then(|root| {
      let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .ok_or_else(|| {
          Failure("npm-reserve needs `--out DIR` (a directory outside the tree)".to_owned())
        })?;
      version::reserve(&root, &out)
    }),
    "ratchet" => workspace_root().and_then(|root| {
      let runs = args
        .iter()
        .position(|a| a == "--runs")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok());
      let flags = ratchet::Flags {
        record: args.iter().any(|a| a == "--record"),
        tighten: args.iter().any(|a| a == "--tighten"),
        reset: args.iter().any(|a| a == "--reset"),
        runs,
      };
      ratchet::run(&root, flags)
    }),
    "conformance" => workspace_root().and_then(|root| {
      let options = conformance::parse(&root, &args[1..])?;
      conformance::run(&root, &options)
    }),
    "kind" => workspace_root().and_then(|root| {
      let options = kind::parse(&args[1..])?;
      kind::run(&root, &options)
    }),
    "tsan" => workspace_root().and_then(|root| tsan::run(&root)),
    other => Err(Failure(format!(
      "unknown task `{other}`; tasks: structural, literals, unsafe, version, npm-reserve, check, capabilities, callgrind, ratchet, conformance, kind, tsan"
    ))),
  };
  match outcome {
    Ok(()) => ExitCode::SUCCESS,
    Err(failure) => {
      eprintln!("xtask: {failure}");
      ExitCode::FAILURE
    }
  }
}

/// The capability table's check (AUD-29-32): the README's table equals the registry's rendering and every
/// cited test exists and runs.
fn check_capabilities(root: &Path) -> Result<(), Failure> {
  let problems = capabilities::problems(root);
  for problem in &problems {
    eprintln!("capabilities: {problem}");
  }
  if problems.is_empty() {
    println!(
      "capabilities: ok ({} rows, each proof a runnable test)",
      capabilities::CAPABILITIES.len()
    );
    Ok(())
  } else {
    Err(Failure(format!("{} capability problem(s)", problems.len())))
  }
}

/// The workspace root: the directory holding the top-level `Cargo.toml`.
fn workspace_root() -> Result<PathBuf, Failure> {
  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  manifest_dir
    .parent()
    .map(Path::to_path_buf)
    .ok_or_else(|| Failure("xtask has no parent directory".to_owned()))
}

/// One shipped crate as `cargo metadata` describes it.
#[derive(Debug, serde::Deserialize)]
struct Package {
  name: String,
  manifest_path: String,
  #[serde(default)]
  dependencies: Vec<Dependency>,
}

#[derive(Debug, serde::Deserialize)]
struct Dependency {
  name: String,
}

#[derive(Debug, serde::Deserialize)]
struct Metadata {
  packages: Vec<Package>,
  workspace_members: Vec<String>,
  resolve: Option<Resolve>,
}

#[derive(Debug, serde::Deserialize)]
struct Resolve {
  nodes: Vec<ResolveNode>,
}

#[derive(Debug, serde::Deserialize)]
struct ResolveNode {
  id: String,
  #[serde(default)]
  dependencies: Vec<String>,
}

fn cargo_metadata(root: &Path) -> Result<Metadata, Failure> {
  let output = Command::new(env!("CARGO"))
    .args(["metadata", "--format-version", "1"])
    .current_dir(root)
    .output()?;
  if !output.status.success() {
    return Err(Failure(format!(
      "cargo metadata failed: {}",
      String::from_utf8_lossy(&output.stderr)
    )));
  }
  Ok(serde_json::from_slice(&output.stdout)?)
}

/// Every `.rs` file under `dir`, recursively, in a stable order.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Failure> {
  let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
    .map(|e| e.map(|e| e.path()))
    .collect::<Result<_, _>>()?;
  entries.sort();
  for path in entries {
    if path.is_dir() {
      rust_sources(&path, out)?;
    } else if path.extension().is_some_and(|x| x == "rs") {
      out.push(path);
    }
  }
  Ok(())
}

/// Whether a source path belongs to test-only code: `tests/`, `benches/`, `examples/` trees.
fn is_test_tree(path: &Path) -> bool {
  path.components().any(|c| {
    let s = c.as_os_str();
    s == "tests" || s == "benches" || s == "examples"
  })
}

/// Splits a source file into lines, marking the ones inside a `#[cfg(test)]` module.
///
/// The module boundary is tracked by brace depth from the `mod` line that follows the
/// attribute; this is a lexical approximation, which is sufficient because the workspace's
/// style puts test modules at the end of files as `#[cfg(test)] mod tests { ... }`.
fn lines_with_test_flag(source: &str) -> Vec<(usize, &str, bool)> {
  let mut out = Vec::new();
  let mut in_test = false;
  let mut depth: i64 = 0;
  let mut armed = false;
  for (index, line) in source.lines().enumerate() {
    let trimmed = line.trim_start();
    if trimmed.starts_with("#[cfg(") && trimmed.contains("test") {
      armed = true;
    } else if armed && trimmed.starts_with("mod ") {
      in_test = true;
      armed = false;
      depth = 0;
    } else if armed
      && !trimmed.is_empty()
      && !trimmed.starts_with("#[")
      && !trimmed.starts_with("//")
    {
      // The attribute sat on another item (a test-only function or impl): it does not reach the next
      // module. Until 2026-10-01 it did, and production modules after a `#[cfg(test)] fn` went unscanned
      // (AUD-29-31; `slates-machine`'s segment module).
      armed = false;
    }
    if in_test {
      depth += i64::from(line.matches('{').count() as u32);
      depth -= i64::from(line.matches('}').count() as u32);
    }
    out.push((index + 1, line, in_test));
    if in_test && depth <= 0 && line.contains('}') {
      in_test = false;
    }
  }
  out
}

/// Strips string literals and comments from a code line so a word inside them cannot match.
fn code_only(line: &str) -> String {
  let mut out = String::with_capacity(line.len());
  let mut chars = line.chars().peekable();
  let mut in_string = false;
  while let Some(c) = chars.next() {
    if in_string {
      if c == '\\' {
        chars.next();
      } else if c == '"' {
        in_string = false;
        out.push('"');
      }
      continue;
    }
    if c == '"' {
      in_string = true;
      out.push('"');
    } else if c == '/' && chars.peek() == Some(&'/') {
      break;
    } else {
      out.push(c);
    }
  }
  out
}

mod structural {
  use super::{
    Failure, Metadata, Package, cargo_metadata, code_only, is_test_tree, lines_with_test_flag,
    rust_sources, workspace_root,
  };
  use std::path::Path;

  /// Crates that may never appear in a shipped crate's resolved dependency set (D-7, D-8, D-9).
  const FORBIDDEN_DEPENDENCIES: &[&str] = &[
    "tokio",
    "tokio-util",
    "async-std",
    "smol",
    "futures-executor",
    "parking_lot",
    "dashmap",
    "rayon",
    "crossbeam-epoch",
    "arc-swap",
    "async-lock",
  ];

  /// Symbols no shipped crate may name in code (comments and strings excluded).
  const FORBIDDEN_SYMBOLS: &[&str] = &[
    "Arc<", "Arc::", "Rc<", "Rc::", "Mutex<", "RwLock<", "Condvar",
  ];

  /// Host-path symbols: only the crates in `HOST_PATH_ALLOWED` may name them.
  const HOST_PATH_SYMBOLS: &[&str] = &[
    "std::fs::",
    "std::net::",
    "std::os::unix::net",
    "std::os::windows::fs",
  ];

  /// Crates that may name host paths, with the reason (R1's table of allowed sites).
  const HOST_PATH_ALLOWED: &[(&str, &str)] = &[
    (
      "slates-machine",
      "reads the kernel's pseudo-files (/proc, /sys) as queries; never writes",
    ),
    (
      "slates-base",
      "read-only access to the directories overlay volumes sit on (§4.15)",
    ),
    (
      "slates-land",
      "the only writer of host paths, under a grant (§4.15)",
    ),
    (
      "slates-ipc",
      "the per-OS rendezvous, which creates no filesystem entry (§4.7)",
    ),
    (
      "slates-mcp",
      "the loopback MCP Streamable HTTP transport (§4.12); authorized by Ada 2026-09-06",
    ),
    ("slates-bridge-fuse", "the mount and its device file (§4.6)"),
    (
      "slates-bridge-nfs",
      "the mount point and loopback socket (§4.6)",
    ),
    ("slates-bridge-fskit", "the mount (§4.6)"),
    ("slates-bridge-winfsp", "the volume (§4.6)"),
    (
      "slates-cli",
      "the launcher's namespace setup and mount install (§4.12); its reads of the operator's fleet manifest, certificate, key and recovery-key files are defects against A-50's zero disk access, to be removed",
    ),
  ];

  /// Write-capable file syscalls: only the landing crate may link them (R1, D-26).
  const WRITE_SYSCALLS: &[&str] = &[
    "libc::rename",
    "libc::renameat",
    "libc::renameat2",
    "libc::renamex_np",
    "libc::unlink",
    "libc::unlinkat",
    "libc::mkdir",
    "libc::mkdirat",
    "libc::rmdir",
    "libc::link(",
    "libc::linkat",
    "libc::symlink",
    "libc::truncate",
    "libc::ftruncate",
    "libc::fsync",
    "libc::fdatasync",
    "libc::utimensat",
    "libc::futimens",
    "libc::chmod",
    "libc::fchmod",
    "libc::chown",
    "libc::clonefile",
    "MoveFileExW",
    "DeleteFileW",
    "CreateDirectoryW",
    "RemoveDirectoryW",
    "SetFileInformationByHandle",
    "FlushFileBuffers",
    "ReplaceFileW",
    // The NT native calls under them (AUD-29-62): `NtCreateFile` creates as well as opens, by disposition.
    "NtCreateFile",
    "NtWriteFile",
    "NtSetInformationFile",
    "NtDeleteFile",
    // rustix's write-capable file calls and open flags, for the same crates.
    "rustix::fs::unlink",
    "rustix::fs::rename",
    "rustix::fs::mkdir",
    "rustix::fs::rmdir",
    "rustix::fs::link",
    "rustix::fs::symlink",
    "rustix::fs::ftruncate",
    "rustix::fs::fsync",
    "rustix::fs::fdatasync",
    "rustix::fs::utimensat",
    "rustix::fs::futimens",
    "rustix::fs::chmod",
    "rustix::fs::fchmod",
    "rustix::fs::chown",
    "rustix::fs::fallocate",
    "rustix::fs::copy_file_range",
    // `rustix::io::write` is not listed: the runtime writes its eventfd and pipe kicks with
    // it, which touches no file; `pwrite` and every path-creating call are.
    "rustix::io::pwrite",
    "OFlags::WRONLY",
    "OFlags::RDWR",
    "OFlags::CREATE",
    "OFlags::TRUNC",
    "OFlags::APPEND",
    "OFlags::TMPFILE",
    // The standard library's write-capable file functions, for crates allowed to read host paths.
    "std::fs::write",
    "std::fs::File::create",
    "std::fs::File::create_new",
    "std::fs::OpenOptions",
    "std::fs::create_dir",
    "std::fs::remove_file",
    "std::fs::remove_dir",
    "std::fs::rename",
    "std::fs::hard_link",
    "std::fs::copy",
    "std::fs::set_permissions",
    "std::fs::File::set_len",
    // R1/D-3: slates never creates a symlink (the clippy list cannot name a unix-only path on Windows).
    "std::os::unix::fs::symlink",
    "std::os::windows::fs::symlink_file",
    "std::os::windows::fs::symlink_dir",
  ];

  const WRITE_ALLOWED: &[&str] = &["slates-land"];

  /// The shipped crates the no-panic sweep has not reached yet (CLAUDE.md banned item 6; GAPS 2026-09-29).
  /// Every other shipped crate's root carries [`NO_PANIC_ATTRIBUTE`]. The list only shrinks: a crate on it
  /// that already carries the attribute fails too, so a crate leaves the list in the change that cleans it.
  const NO_PANIC_PENDING: &[&str] = &[];

  /// Format: the crate-root attribute of the no-panic law's last ratcheted part, whitespace removed: outside
  /// test builds, deny arithmetic that can overflow or divide by zero. Out-of-bounds indexing and slicing and
  /// string slicing off a character boundary are denied workspace-wide (`[workspace.lints.clippy]`, since
  /// 2026-10-06, when the last site was gone).
  const NO_PANIC_ATTRIBUTE: &str = "#![cfg_attr(not(test),deny(clippy::arithmetic_side_effects))]";

  /// Whether `package`'s crate root (`src/lib.rs`, else `src/main.rs`) carries [`NO_PANIC_ATTRIBUTE`], and
  /// a violation when that disagrees with [`NO_PANIC_PENDING`].
  fn check_no_panic_ratchet(
    package: &Package,
    violations: &mut Vec<String>,
  ) -> Result<(), Failure> {
    let crate_dir = Path::new(&package.manifest_path)
      .parent()
      .ok_or_else(|| Failure(format!("{}: manifest has no directory", package.name)))?;
    let root = [crate_dir.join("src/lib.rs"), crate_dir.join("src/main.rs")]
      .into_iter()
      .find(|path| path.is_file());
    let Some(root) = root else {
      return Ok(());
    };
    let compact: String = std::fs::read_to_string(&root)?
      .chars()
      .filter(|c| !c.is_whitespace())
      .collect();
    let carries = compact.contains(NO_PANIC_ATTRIBUTE);
    let pending = NO_PANIC_PENDING.contains(&package.name.as_str());
    if pending && carries {
      violations.push(format!(
        "{}: its root carries the no-panic attribute; remove it from NO_PANIC_PENDING",
        package.name
      ));
    } else if !pending && !carries {
      violations.push(format!(
        "{}: its root ({}) lacks the no-panic attribute `{NO_PANIC_ATTRIBUTE}` (CLAUDE.md banned item 6)",
        package.name,
        root.display()
      ));
    }
    Ok(())
  }

  /// Runs the structural test over every shipped crate.
  pub(super) fn run() -> Result<(), Failure> {
    let root = workspace_root()?;
    let metadata = cargo_metadata(&root)?;
    let mut violations: Vec<String> = Vec::new();
    for package in shipped_crates(&metadata) {
      check_dependencies(&metadata, package, &mut violations);
      check_sources(package, &mut violations)?;
      check_no_panic_ratchet(package, &mut violations)?;
      check_property_persistence(package, &mut violations)?;
    }
    check_design_table(&root, &mut violations)?;
    if violations.is_empty() {
      println!(
        "structural: ok ({} shipped crates)",
        shipped_crates(&metadata).count()
      );
      Ok(())
    } else {
      for v in &violations {
        eprintln!("structural: {v}");
      }
      Err(Failure(format!(
        "{} structural violation(s)",
        violations.len()
      )))
    }
  }

  fn shipped_crates(metadata: &Metadata) -> impl Iterator<Item = &Package> {
    metadata
      .packages
      .iter()
      .filter(|p| p.name.starts_with("slates-"))
      .filter(|p| {
        metadata.workspace_members.iter().any(|m| {
          m.contains(&format!("{}#", p.name))
            || m.ends_with(&p.name)
            || m.contains(&format!("/{}", crate_dir(p)))
        })
      })
  }

  fn crate_dir(package: &Package) -> String {
    Path::new(&package.manifest_path)
      .parent()
      .and_then(Path::file_name)
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_default()
  }

  fn check_dependencies(metadata: &Metadata, package: &Package, violations: &mut Vec<String>) {
    let mut reachable: Vec<String> = Vec::new();
    if let Some(resolve) = &metadata.resolve {
      let mut stack: Vec<&str> = resolve
        .nodes
        .iter()
        .filter(|n| id_names(&n.id) == package.name)
        .map(|n| n.id.as_str())
        .collect();
      while let Some(id) = stack.pop() {
        if let Some(node) = resolve.nodes.iter().find(|n| n.id == id) {
          for dep in &node.dependencies {
            let name = id_names(dep);
            if !reachable.iter().any(|r| r == &name) {
              reachable.push(name);
              stack.push(dep);
            }
          }
        }
      }
    } else {
      reachable.extend(package.dependencies.iter().map(|d| d.name.clone()));
    }
    for forbidden in FORBIDDEN_DEPENDENCIES {
      if reachable.iter().any(|r| r == forbidden) {
        violations.push(format!(
          "{}: depends (transitively) on forbidden crate `{forbidden}` (D-7/D-8/D-9)",
          package.name
        ));
      }
    }
  }

  /// The crate name inside a `cargo metadata` package id (`name version (source)` or `...#name@ver`).
  fn id_names(id: &str) -> String {
    if let Some((_, tail)) = id.rsplit_once('#') {
      return tail.split('@').next().unwrap_or(tail).to_owned();
    }
    id.split(' ').next().unwrap_or(id).to_owned()
  }

  fn check_sources(package: &Package, violations: &mut Vec<String>) -> Result<(), Failure> {
    let crate_root = Path::new(&package.manifest_path)
      .parent()
      .ok_or_else(|| Failure(format!("{}: manifest has no directory", package.name)))?;
    let src = crate_root.join("src");
    if !src.is_dir() {
      return Ok(());
    }
    let mut files = Vec::new();
    rust_sources(&src, &mut files)?;
    let host_paths_allowed = HOST_PATH_ALLOWED.iter().any(|(n, _)| *n == package.name);
    let writes_allowed = WRITE_ALLOWED.contains(&package.name.as_str());
    for file in files {
      if is_test_tree(&file) {
        continue;
      }
      let source = std::fs::read_to_string(&file)?;
      violations.extend(source_violations(
        &file.display().to_string(),
        &source,
        host_paths_allowed,
        writes_allowed,
      ));
    }
    Ok(())
  }

  /// The violations in one source file of a crate that may (`host_paths_allowed`) or may not name host
  /// paths, and may (`writes_allowed`) or may not name write-capable calls (AUD-29-31). Two passes over the
  /// non-test lines: every line's code against the spelled symbols, and every `use` statement's expanded
  /// tree — braces, nesting, `self` and aliases resolved — against the same paths, so `use std::fs;` then
  /// `fs::write(..)`, `use libc::{mkdirat}` or `use rustix::fs::mkdirat as make` cannot hide a call the
  /// spelled check would see. A glob import of a write-capable module, and an alias of a crate root that
  /// holds write calls (`use libc as c;`, `extern crate std as s;`), are refused outright, since later calls
  /// through them cannot be checked. This is a source check: the resolved-path lints (`clippy.toml`
  /// `disallowed-methods`) see through macro expansion, and the hermeticity tracer observes the syscalls
  /// themselves; none of the three is a linker proof.
  fn source_violations(
    file: &str,
    source: &str,
    host_paths_allowed: bool,
    writes_allowed: bool,
  ) -> Vec<String> {
    let mut violations = Vec::new();
    let mut allow_next = false;
    let mut statement = String::new();
    let mut statement_line = 0usize;
    for (line_no, line, in_test) in lines_with_test_flag(source) {
      if in_test {
        continue;
      }
      // `// structural: allow` on the line, or on the comment line above it, exempts one
      // line; the comment must say why (an FFI edge, or a syscall on a memory object).
      let allowed = line.contains("structural: allow") || allow_next;
      allow_next = line.trim_start().starts_with("//") && line.contains("structural: allow");
      if allowed {
        continue;
      }
      let code = code_only(line);
      let file = Path::new(file);
      report(
        &code,
        FORBIDDEN_SYMBOLS,
        file,
        line_no,
        "forbidden symbol (R2/D-7)",
        &mut violations,
      );
      if !host_paths_allowed {
        report(
          &code,
          HOST_PATH_SYMBOLS,
          file,
          line_no,
          "host path outside an allowed crate (R1)",
          &mut violations,
        );
      }
      if !writes_allowed {
        report(
          &code,
          WRITE_SYSCALLS,
          file,
          line_no,
          "write-capable syscall outside slates-land (R1/D-26)",
          &mut violations,
        );
      }
      if let Some(alias) = aliased_root(&code) {
        violations.push(format!(
          "{}:{line_no}: alias of a crate root that holds write calls: `{alias}` (AUD-29-31)",
          file.display()
        ));
      }
      // Gather a `use` statement across the lines rustfmt wraps it over, then check its expanded paths.
      let trimmed = code.trim();
      if statement.is_empty() && is_use_statement(trimmed) {
        statement_line = line_no;
      }
      if !statement.is_empty() || is_use_statement(trimmed) {
        statement.push_str(trimmed);
        if trimmed.ends_with(';') {
          check_use_paths(
            file,
            statement_line,
            &statement,
            host_paths_allowed,
            writes_allowed,
            &mut violations,
          );
          statement.clear();
        }
      }
    }
    violations
  }

  /// Whether a code line opens a `use` statement (any visibility).
  fn is_use_statement(code: &str) -> bool {
    let rest = code
      .strip_prefix("pub(crate) ")
      .or_else(|| code.strip_prefix("pub(super) "))
      .or_else(|| code.strip_prefix("pub "))
      .unwrap_or(code);
    rest.starts_with("use ")
  }

  /// Crate roots and modules whose aliasing would hide write calls from the spelled check.
  const ALIAS_GUARDED: &[&str] = &[
    "std",
    "libc",
    "rustix",
    "rustix::fs",
    "rustix::io",
    "std::fs",
  ];

  /// The guarded root a line aliases (`use libc as c;`, `extern crate std as s;`), if any.
  fn aliased_root(code: &str) -> Option<String> {
    let trimmed = code.trim();
    let body = trimmed
      .strip_prefix("extern crate ")
      .or_else(|| trimmed.strip_prefix("use "))?;
    let (path, _) = body.trim_end_matches(';').split_once(" as ")?;
    let path = path.trim().trim_start_matches("::");
    ALIAS_GUARDED.contains(&path).then(|| path.to_owned())
  }

  /// Checks every path a `use` statement imports against the host-path and write rules.
  fn check_use_paths(
    file: &Path,
    line_no: usize,
    statement: &str,
    host_paths_allowed: bool,
    writes_allowed: bool,
    violations: &mut Vec<String>,
  ) {
    let Some((_, tree)) = statement.split_once("use ") else {
      return;
    };
    let tree = tree.trim().trim_end_matches(';').trim_start_matches("::");
    for path in expand_use_tree("", tree) {
      if !host_paths_allowed
        && HOST_PATH_SYMBOLS
          .iter()
          .map(|symbol| symbol.trim_end_matches("::"))
          .any(|root| path == root || path.starts_with(&format!("{root}::")))
      {
        violations.push(format!(
          "{}:{line_no}: host path outside an allowed crate (R1): imports `{path}`",
          file.display()
        ));
      }
      if !writes_allowed {
        for pattern in WRITE_SYSCALLS {
          // A prefix, as the spelled check matches (`rustix::fs::mkdir` covers `mkdirat`); a pattern that
          // ends in `(` names exactly that function.
          let matched = match pattern.strip_suffix('(') {
            Some(exact) => path == exact || path.starts_with(&format!("{exact}::")),
            None => path.starts_with(pattern),
          };
          if matched {
            violations.push(format!(
              "{}:{line_no}: write-capable syscall outside slates-land (R1/D-26): imports `{path}`",
              file.display()
            ));
          }
        }
        if let Some(module) = path.strip_suffix("::*")
          && GLOB_GUARDED.contains(&module)
        {
          violations.push(format!(
            "{}:{line_no}: glob import of a module with write calls: `{path}` (AUD-29-31)",
            file.display()
          ));
        }
      }
    }
  }

  /// Modules a glob import of which would bring write calls in unnamed.
  const GLOB_GUARDED: &[&str] = &[
    "libc",
    "rustix::fs",
    "rustix::io",
    "std::fs",
    "std::os::unix::fs",
    "windows_sys::Win32::Storage::FileSystem",
  ];

  /// The full paths a `use` tree imports, aliases dropped: `std::{fs, io::{self, Write as W}}` gives
  /// `std::fs`, `std::io`, `std::io::Write`.
  fn expand_use_tree(prefix: &str, tree: &str) -> Vec<String> {
    let tree = tree.trim();
    let join = |head: &str| -> String {
      match (prefix.is_empty(), head.is_empty()) {
        (true, _) => head.to_owned(),
        (false, true) => prefix.to_owned(),
        (false, false) => format!("{prefix}::{head}"),
      }
    };
    if let Some(open) = tree.find('{') {
      let head = tree.get(..open).unwrap_or("").trim().trim_end_matches("::");
      let close = tree.rfind('}').unwrap_or(tree.len());
      let inner = tree.get(open.saturating_add(1)..close).unwrap_or("");
      let base = join(head);
      return split_top_level(inner)
        .into_iter()
        .flat_map(|item| expand_use_tree(&base, item))
        .collect();
    }
    let item = tree.split(" as ").next().unwrap_or(tree).trim();
    if item.is_empty() {
      return Vec::new();
    }
    if item == "self" {
      return vec![prefix.to_owned()];
    }
    vec![join(item)]
  }

  /// Splits a brace group's items at its top-level commas.
  fn split_top_level(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (at, c) in inner.char_indices() {
      match c {
        '{' => depth += 1,
        '}' => depth -= 1,
        ',' if depth == 0 => {
          items.push(inner.get(start..at).unwrap_or(""));
          start = at.saturating_add(1);
        }
        _ => {}
      }
    }
    items.push(inner.get(start..).unwrap_or(""));
    items
      .into_iter()
      .filter(|item| !item.trim().is_empty())
      .collect()
  }

  fn report(
    code: &str,
    patterns: &[&str],
    file: &Path,
    line_no: usize,
    what: &str,
    violations: &mut Vec<String>,
  ) {
    for pattern in patterns {
      if code.contains(pattern) {
        violations.push(format!("{}:{line_no}: {what}: `{pattern}`", file.display()));
      }
    }
  }

  #[cfg(test)]
  mod fixtures {
    use super::source_violations;

    /// The violations a fixture raises in a crate that may neither name host paths nor write.
    fn refused(source: &str) -> Vec<String> {
      source_violations("fixture.rs", source, false, false)
    }

    /// AUD-29-31 (R1). Do: scan fixtures that reach a write-capable call through each spelling the old
    /// string list missed — a module import then a short call, a brace group, a nested group with `self`, an
    /// alias of the call, an alias of a crate root, `extern crate` renaming, a glob import, a wrapped multi-line
    /// import, and a call spelled inside a macro body. Expect: every fixture refused.
    #[test]
    fn every_spelling_of_a_write_call_is_refused() {
      let fixtures = [
        "use std::fs;\nfn f() { let _ = fs::write(\"x\", b\"\"); }\n",
        "use std::{fs, io};\n",
        "use libc::{mkdirat, openat};\n",
        "use rustix::{fs::{self, mkdirat}, io};\n",
        "use rustix::fs::mkdirat as make;\n",
        "use libc as c;\n",
        "extern crate std as s;\n",
        "use libc::*;\n",
        "use rustix::fs::{\n  OFlags,\n  unlinkat,\n};\n",
        "macro_rules! m { () => { std::fs::remove_file(\"x\") }; }\n",
      ];
      for fixture in fixtures {
        assert!(!refused(fixture).is_empty(), "not refused: {fixture:?}");
      }
    }

    /// AUD-29-31. Do: scan a production module that follows a test-only function. Expect: its write call
    /// refused — the `#[cfg(test)]` on the function does not make the next module a test module (before
    /// 2026-10-01 it did, and `slates-machine`'s segment module went unscanned).
    #[test]
    fn a_test_only_function_does_not_hide_the_next_module() {
      let source = "#[cfg(test)]\nfn helper() {}\n\nmod platform {\n  use libc::mkdirat;\n}\n";
      assert!(!refused(source).is_empty());
      let tests = "#[cfg(test)]\nmod tests {\n  use libc::mkdirat;\n}\n";
      assert!(refused(tests).is_empty(), "a real test module stays exempt");
    }

    /// AUD-29-31. Do: scan sources that only read, or name a write call where it is allowed. Expect: nothing
    /// refused for read-only imports in a crate without host-path rights, nothing for a write call in the
    /// landing crate, and nothing for a line carrying a reasoned `structural: allow`.
    #[test]
    fn read_only_and_allowed_sites_pass() {
      assert!(refused("use rustix::io::{read, write};\nuse std::io::Write;\n").is_empty());
      assert!(
        source_violations(
          "land.rs",
          "use rustix::fs::{mkdirat, unlinkat};\n",
          true,
          true
        )
        .is_empty()
      );
      assert!(
        refused(
          "// structural: allow — sizing a memory object, not a file.\nuse rustix::fs::ftruncate;\n"
        )
        .is_empty()
      );
    }
  }

  /// Format: the crate that owns property-test failure persistence (A-50; AUD-29-63).
  const SEEDS_CRATE: &str = "slates-test-seeds";
  /// Format: the path through which every property suite takes its configuration.
  const SEEDS_PATH: &str = "slates_test_seeds::";

  /// A property suite writes no failure file (A-50; AUD-29-63): every `proptest!` block and every
  /// hand-built `TestRunner` in `package` takes its configuration through `slates_test_seeds` (one use per
  /// suite, so a second block in a file cannot inherit proptest's writing default), and nothing but that
  /// crate sets `failure_persistence`. Sources, tests, benches and examples are all checked: a failing test
  /// is exactly where the default would write.
  fn check_property_persistence(
    package: &Package,
    violations: &mut Vec<String>,
  ) -> Result<(), Failure> {
    if package.name == SEEDS_CRATE {
      return Ok(());
    }
    let crate_root = Path::new(&package.manifest_path)
      .parent()
      .ok_or_else(|| Failure(format!("{}: manifest has no directory", package.name)))?;
    let mut files = Vec::new();
    for tree in ["src", "tests", "benches", "examples"] {
      let dir = crate_root.join(tree);
      if dir.is_dir() {
        rust_sources(&dir, &mut files)?;
      }
    }
    for file in files {
      let source = std::fs::read_to_string(&file)?;
      violations.extend(property_persistence_violations(
        &file.display().to_string(),
        &source,
      ));
    }
    Ok(())
  }

  /// The persistence violations in one file's source (comments and string contents ignored).
  fn property_persistence_violations(label: &str, source: &str) -> Vec<String> {
    let code: Vec<String> = source.lines().map(code_only).collect();
    let count = |needle: &str| {
      code
        .iter()
        .map(|line| line.matches(needle).count())
        .fold(0usize, usize::saturating_add)
    };
    let suites = count("proptest!")
      .saturating_add(count("TestRunner::new("))
      .saturating_add(count("TestRunner::new_with_rng("));
    let through_seeds = count(SEEDS_PATH);
    let mut found = Vec::new();
    if through_seeds < suites {
      found.push(format!(
        "{label}: {suites} property suite(s) but {through_seeds} configured through `{SEEDS_PATH}` — \
         proptest's default writes a failure file (A-50; AUD-29-63)"
      ));
    }
    if count("failure_persistence") > 0 {
      found.push(format!(
        "{label}: sets `failure_persistence`; use `{SEEDS_PATH}seeded` or `unseeded` (A-50; AUD-29-63)"
      ));
    }
    found
  }

  /// Format: the markers around the design's generated table of host-path sites (§0.2, A-50).
  const TABLE_BEGIN: &str = "<!-- host-path-sites:begin -->\n";
  const TABLE_END: &str = "<!-- host-path-sites:end -->";

  /// The design's table of the crates that may name a host path (a file, a socket or a mount point),
  /// rendered from [`HOST_PATH_ALLOWED`], the table this check enforces.
  fn host_path_table() -> String {
    let mut table = String::from("| Crate | Why it may name a host path |\n|---|---|\n");
    for (name, why) in HOST_PATH_ALLOWED {
      table.push_str(&format!("| `{name}` | {why} |\n"));
    }
    table
  }

  /// The design's table of host-path sites must be the one this check enforces (A-50): a crate given leave
  /// to name host paths, or a changed reason, is in the design in the same change.
  fn check_design_table(root: &Path, violations: &mut Vec<String>) -> Result<(), Failure> {
    let design = std::fs::read_to_string(root.join("docs/wip/SLATES_DESIGN.md"))?;
    if design_table(&design) != Some(host_path_table().as_str()) {
      violations.push(
        "docs/wip/SLATES_DESIGN.md's host-path table differs from HOST_PATH_ALLOWED; run `cargo test -p xtask -- --ignored regenerate_the_host_path_table`"
          .to_owned(),
      );
    }
    Ok(())
  }

  /// The design's generated block, between its markers.
  fn design_table(design: &str) -> Option<&str> {
    let start = design.find(TABLE_BEGIN)?.checked_add(TABLE_BEGIN.len())?;
    let end = design.get(start..)?.find(TABLE_END)?.checked_add(start)?;
    design.get(start..end)
  }

  #[cfg(test)]
  mod tests {
    use super::{
      TABLE_BEGIN, TABLE_END, design_table, host_path_table, property_persistence_violations,
      workspace_root,
    };

    /// AUD-29-63. Do: judge a file whose two property blocks both go through the seeds crate, one whose
    /// second block does not, one with a hand-built runner that does not, and one that sets
    /// `failure_persistence` itself. Expect: the first passes; each other is refused, naming the reason; a
    /// mention in a comment counts for nothing.
    #[test]
    fn a_property_suite_that_could_write_a_failure_file_is_refused() {
      let both = "proptest! {\n #![proptest_config(slates_test_seeds::unseeded(c))]\n}\nproptest! {\n #![proptest_config(slates_test_seeds::unseeded(c))]\n}\n";
      assert!(property_persistence_violations("a.rs", both).is_empty());
      let second_bare = "proptest! {\n #![proptest_config(slates_test_seeds::unseeded(c))]\n}\nproptest! {\n}\n// slates_test_seeds:: in a comment\n";
      let refused = property_persistence_violations("b.rs", second_bare);
      assert_eq!(refused.len(), 1, "{refused:?}");
      assert!(
        refused[0].contains("2 property suite(s) but 1"),
        "{refused:?}"
      );
      let runner = "let r = TestRunner::new(Config::default());\n";
      assert_eq!(property_persistence_violations("c.rs", runner).len(), 1);
      let own = "let r = TestRunner::new(slates_test_seeds::unseeded(Config { failure_persistence: None, ..c }));\n";
      let refused = property_persistence_violations("d.rs", own);
      assert_eq!(refused.len(), 1, "{refused:?}");
      assert!(
        refused[0].contains("sets `failure_persistence`"),
        "{refused:?}"
      );
    }

    fn design_path() -> std::path::PathBuf {
      workspace_root()
        .expect("the workspace root")
        .join("docs/wip/SLATES_DESIGN.md")
    }

    /// A-50 (doc truth): the design lists exactly the crates `cargo xtask check` lets name a host path,
    /// each with the reason the check records. Do: read the design's generated block. Expect: it equals
    /// the table rendered from the check's own list.
    #[test]
    fn the_design_lists_exactly_the_crates_allowed_to_name_host_paths() {
      let design = std::fs::read_to_string(design_path()).expect("the design is readable");
      assert_eq!(
        design_table(&design),
        Some(host_path_table().as_str()),
        "run `cargo test -p xtask -- --ignored regenerate_the_host_path_table`"
      );
    }

    /// The deliberate writer: rewrites the design's generated block. Ignored, so a normal run never
    /// mutates the tree.
    #[test]
    #[ignore = "rewrites docs/wip/SLATES_DESIGN.md's host-path table; run deliberately with --ignored"]
    fn regenerate_the_host_path_table() {
      let design = std::fs::read_to_string(design_path()).expect("the design is readable");
      let start = design.find(TABLE_BEGIN).expect("the begin marker") + TABLE_BEGIN.len();
      let end = start + design[start..].find(TABLE_END).expect("the end marker");
      let rewritten = format!(
        "{}{}{}",
        &design[..start],
        host_path_table(),
        &design[end..]
      );
      // The design's `--ignored regenerate` writer (CLAUDE §4 "Doc-truth tests").
      #[allow(clippy::disallowed_methods)]
      std::fs::write(design_path(), rewritten).expect("the design is writable");
    }
  }
}

mod literals {
  use super::{
    Failure, cargo_metadata, code_only, is_test_tree, lines_with_test_flag, rust_sources,
    workspace_root,
  };
  use std::path::Path;

  /// Doc or comment markers that justify a literal on the following lines.
  const MARKERS: &[&str] = &["Derived:", "Measured:", "Format:", "Shape:"];

  /// How many lines a marker covers below itself (a doc comment above a `const` item).
  const MARKER_REACH: usize = 4;

  /// Runs the literal check over every shipped crate.
  pub(super) fn run() -> Result<(), Failure> {
    let root = workspace_root()?;
    let metadata = cargo_metadata(&root)?;
    let mut violations: Vec<String> = Vec::new();
    for package in metadata
      .packages
      .iter()
      .filter(|p| p.name.starts_with("slates-"))
    {
      let crate_root = Path::new(&package.manifest_path)
        .parent()
        .ok_or_else(|| Failure(format!("{}: manifest has no directory", package.name)))?;
      let src = crate_root.join("src");
      if !src.is_dir() {
        continue;
      }
      let mut files = Vec::new();
      rust_sources(&src, &mut files)?;
      for file in files {
        if is_test_tree(&file) {
          continue;
        }
        check_file(&file, &mut violations)?;
      }
    }
    if violations.is_empty() {
      println!("literals: ok");
      Ok(())
    } else {
      for v in &violations {
        eprintln!("literals: {v}");
      }
      Err(Failure(format!(
        "{} numeric literal(s) without a derivation (R3); mark them with `derived!`, or a `Derived:`, `Measured:`, `Format:` or `Shape:` doc line",
        violations.len()
      )))
    }
  }

  fn check_file(file: &Path, violations: &mut Vec<String>) -> Result<(), Failure> {
    let source = std::fs::read_to_string(file)?;
    let lines = lines_with_test_flag(&source);
    let mut cover_until: usize = 0;
    let mut derived_depth: i64 = 0;
    for (line_no, line, in_test) in &lines {
      let trimmed = line.trim_start();
      if MARKERS.iter().any(|m| trimmed.contains(m)) {
        cover_until = line_no + MARKER_REACH;
      }
      let code = code_only(line);
      // A `derived!(` call covers every line until its parentheses close.
      let in_derived = derived_depth > 0 || code.contains("derived!(");
      if in_derived {
        derived_depth += i64::from(code.matches('(').count() as u32);
        derived_depth -= i64::from(code.matches(')').count() as u32);
        derived_depth = derived_depth.max(0);
      }
      if *in_test || *line_no <= cover_until || in_derived || is_exempt_line(trimmed) {
        continue;
      }
      for literal in numeric_literals(&code) {
        if !is_trivial(&literal) && !is_bit_width_context(&code, &literal) {
          violations.push(format!(
            "{}:{line_no}: `{literal}` in `{}`",
            file.display(),
            trimmed.trim_end()
          ));
        }
      }
    }
    Ok(())
  }

  /// Lines that never carry a tuning number: attributes, derived! calls, plain comments.
  fn is_exempt_line(trimmed: &str) -> bool {
    trimmed.starts_with("#[")
      || trimmed.starts_with("#![")
      || trimmed.starts_with("//")
      || trimmed.contains("derived!(")
      || trimmed.starts_with("use ")
      || trimmed.starts_with("mod ")
  }

  /// Integer and float literals in a code line (suffixes stripped), left to right.
  fn numeric_literals(code: &str) -> Vec<String> {
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
      let c = bytes[i];
      let prev_is_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
      if c.is_ascii_digit() && !prev_is_ident {
        let start = i;
        let radix_prefix =
          i + 1 < bytes.len() && bytes[i] == b'0' && matches!(bytes[i + 1], b'x' | b'b' | b'o');
        i += if radix_prefix { 2 } else { 0 };
        while i < bytes.len()
          && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'_' || bytes[i] == b'.')
        {
          if bytes[i] == b'.' && i + 1 < bytes.len() && !bytes[i + 1].is_ascii_digit() {
            break;
          }
          i += 1;
        }
        // exponent
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') && !radix_prefix {
          i += 1;
          if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
          }
          while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
          }
        }
        // type suffix
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
          j += 1;
        }
        out.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
        i = j;
      } else {
        i += 1;
      }
    }
    out
  }

  /// 0, 1 and 2 are arithmetic, not tuning: halves, doubles, increments, sentinels.
  fn is_trivial(literal: &str) -> bool {
    matches!(
      literal.trim_end_matches('.'),
      "0" | "1" | "2" | "0.0" | "1.0" | "2.0"
    )
  }

  /// A literal next to a shift, a `size_of`, or an array index is a bit-width or layout fact.
  fn is_bit_width_context(code: &str, literal: &str) -> bool {
    let Some(pos) = code.find(literal) else {
      return false;
    };
    let before = code[..pos].trim_end();
    let after = code[pos + literal.len()..].trim_start();
    before.ends_with("<<")
      || before.ends_with(">>")
      || before.ends_with('[')
      || before.ends_with("align(")
      || after.starts_with(']')
      || before.ends_with("u8")
  }
}
