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
//! - `cargo xtask check` — structural, literals and the unsafe budget.
//! - `cargo xtask ratchet [--record] [--tighten] [--reset] [--runs N]` — the performance
//!   ratchet over the bench examples, keyed by machine identity (see `ratchet.rs`).
//!
//! This is a development tool, not shipped code. It reads sources and runs cargo, so it is the one
//! place in the workspace where `std::fs` reads and `std::process` are ordinary; it still obeys the
//! no-panic law (typed errors, exit codes) so a broken tree reports rather than crashes.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod ratchet;
mod unsafe_budget;

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
      .and_then(|()| workspace_root().and_then(|root| unsafe_budget::run(&root, false))),
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
    other => Err(Failure(format!(
      "unknown task `{other}`; tasks: structural, literals, unsafe, check, ratchet"
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
      "the launcher's namespace setup and mount install (§4.12)",
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

  /// Runs the structural test over every shipped crate.
  pub(super) fn run() -> Result<(), Failure> {
    let root = workspace_root()?;
    let metadata = cargo_metadata(&root)?;
    let mut violations: Vec<String> = Vec::new();
    for package in shipped_crates(&metadata) {
      check_dependencies(&metadata, package, &mut violations);
      check_sources(package, &mut violations)?;
    }
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
      let mut allow_next = false;
      for (line_no, line, in_test) in lines_with_test_flag(&source) {
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
        report(
          &code,
          FORBIDDEN_SYMBOLS,
          &file,
          line_no,
          "forbidden symbol (R2/D-7)",
          violations,
        );
        if !host_paths_allowed {
          report(
            &code,
            HOST_PATH_SYMBOLS,
            &file,
            line_no,
            "host path outside an allowed crate (R1)",
            violations,
          );
        }
        if !writes_allowed {
          report(
            &code,
            WRITE_SYSCALLS,
            &file,
            line_no,
            "write-capable syscall outside slates-land (R1/D-26)",
            violations,
          );
        }
      }
    }
    Ok(())
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
