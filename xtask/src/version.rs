//! `cargo xtask version`: the one version, and every package that carries it (§2.4, §4.12;
//! `docs/publish.md`).
//!
//! The workspace manifest's `[workspace.package] version` is the only hand-edited version in the
//! tree. Every Rust crate inherits it; the Python wheel derives it through maturin (`pyproject.toml`
//! declares `dynamic = ["version"]` and must never carry a static one); the Node package cannot
//! derive it — npm needs a literal `version` in every `package.json` — so this task keeps the copies
//! npm needs equal to it: the main package, its platform packages, and the `optionalDependencies`
//! pins that bind the two. `cargo xtask version` refuses on any drift (it runs inside
//! `cargo xtask check` and in CI), `--write` re-derives the copies, and `--expect-tag vX.Y.Z` is the
//! release guard every publish workflow runs first: the tag must name the workspace version.
//!
//! The platform packages are derived, never hand-edited: their names come from the main package's
//! `name`, their binary file names from `napi.binaryName`, and their `os`/`cpu`/`libc` fields from
//! the Rust target triple by napi's own rule (Node's `process.platform` and `process.arch` names plus
//! the C library), so a target in `napi.targets` without its directory, a stray directory, or a
//! package that would make npm pick the wrong binary is refused with its file and the remedy.
//!
//! `cargo xtask npm-reserve --out DIR` writes the 0.0.0 stub packages a maintainer publishes once to
//! create each name on npm — npm attaches a trusted publisher only to a package that already exists
//! (npm/cli#8544; vorpal's recipe) — derived from the same manifests, so the reserved names are
//! exactly the published names.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde_json::Value;

use crate::Failure;

/// The Python SDK's project file; maturin reads the wheel's version from the crate manifest, which
/// inherits the workspace's, when the file says `dynamic = ["version"]`.
const PYPROJECT: &str = "crates/sdk-python/pyproject.toml";
/// The Node SDK's main package manifest: the source of the package name, the binary name and the
/// target list every platform package is derived from.
const NODE_PACKAGE: &str = "crates/sdk-node/package.json";
/// The per-platform packages, one directory per `napi.targets` entry, named by napi's platform id.
const NODE_PLATFORMS: &str = "crates/sdk-node/npm";
/// Format: npm and napi write `package.json` with two-space indentation and one field per line; the
/// writer edits exactly those lines and verifies every result by parsing it before writing.
const INDENT: &str = "  ";
/// Format: the version of the stub a maintainer publishes once to create a name on npm — below every
/// real release, so the first tagged release supersedes it (npm/cli#8544; vorpal's recipe).
const RESERVED_VERSION: &str = "0.0.0";

/// What `cargo xtask version` is asked to do.
pub(crate) enum Mode {
  /// Refuse on any drift; print the version.
  Check,
  /// Re-derive every npm copy from the workspace version and the main package, then check.
  Write,
  /// The release guard: nothing may drift, and the tag must be `v` + the workspace version.
  ExpectTag(String),
}

/// Parses the task's arguments (`--write`, `--expect-tag TAG`).
pub(crate) fn mode_from(args: &[String]) -> Result<Mode, Failure> {
  if args.iter().any(|a| a == "--write") {
    return Ok(Mode::Write);
  }
  match args.iter().position(|a| a == "--expect-tag") {
    Some(index) => args
      .get(index + 1)
      .map(|tag| Mode::ExpectTag(tag.clone()))
      .ok_or_else(|| Failure("--expect-tag needs the tag (`v0.1.0`)".to_owned())),
    None => Ok(Mode::Check),
  }
}

/// A napi platform: the Rust target triple and the names npm and Node use for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Platform {
  /// The Rust target triple (`aarch64-apple-darwin`).
  pub(crate) triple: String,
  /// napi's platform id: the platform directory and the package suffix (`darwin-arm64`,
  /// `linux-x64-gnu`, `win32-ia32-msvc`).
  pub(crate) id: String,
  /// Node's `process.platform` value (`darwin`, `linux`, `win32`).
  pub(crate) os: &'static str,
  /// Node's `process.arch` value (`arm64`, `x64`, `ia32`).
  pub(crate) cpu: &'static str,
  /// The C library npm's `libc` field names on Linux (`glibc`, `musl`); none elsewhere.
  pub(crate) libc: Option<&'static str>,
}

/// napi's rule for a target triple (Node's platform and arch names plus the C library), restricted
/// to the systems slates ships; anything else is an unknown target, never a guess.
pub(crate) fn platform_of(triple: &str) -> Result<Platform, Finding> {
  let unknown = || Finding::UnknownTarget {
    triple: triple.to_owned(),
  };
  let parts: Vec<&str> = triple.split('-').collect();
  let (cpu_part, system, abi) = match parts.as_slice() {
    [cpu, _vendor, system] => (*cpu, *system, None),
    [cpu, _vendor, system, abi] => (*cpu, *system, Some(*abi)),
    _ => return Err(unknown()),
  };
  let cpu = node_arch(cpu_part).ok_or_else(unknown)?;
  let (os, libc) = node_platform(system, abi).ok_or_else(unknown)?;
  let id = match abi {
    Some(abi) => format!("{os}-{cpu}-{abi}"),
    None => format!("{os}-{cpu}"),
  };
  Ok(Platform {
    triple: triple.to_owned(),
    id,
    os,
    cpu,
    libc,
  })
}

/// Node's `process.arch` name for a Rust target's CPU.
fn node_arch(cpu: &str) -> Option<&'static str> {
  match cpu {
    "x86_64" => Some("x64"),
    "aarch64" => Some("arm64"),
    "i686" => Some("ia32"),
    _ => None,
  }
}

/// Node's `process.platform` name and npm's `libc` value for a Rust target's system and ABI.
fn node_platform(system: &str, abi: Option<&str>) -> Option<(&'static str, Option<&'static str>)> {
  match (system, abi) {
    ("darwin", None) => Some(("darwin", None)),
    ("linux", Some("gnu")) => Some(("linux", Some("glibc"))),
    ("linux", Some("musl")) => Some(("linux", Some("musl"))),
    ("windows", Some("msvc")) => Some(("win32", None)),
    _ => None,
  }
}

/// The facts the audit judges, read once from the tree (or built by a test).
pub(crate) struct Truth {
  /// `[workspace.package] version` in the root `Cargo.toml`: the one hand-edited version.
  pub(crate) workspace_version: String,
  /// The Python project file's version declaration.
  pub(crate) pyproject: PyProject,
  /// The Node main package.
  pub(crate) root: RootPackage,
  /// The directories under `crates/sdk-node/npm`, in name order.
  pub(crate) platform_dirs: Vec<String>,
  /// The platform packages found, by directory name.
  pub(crate) platforms: BTreeMap<String, PlatformPackage>,
}

/// How `pyproject.toml` declares the version.
pub(crate) struct PyProject {
  /// The file, for messages.
  pub(crate) path: String,
  /// Whether `[project]` carries a literal `version` (a second hand-edited copy: refused).
  pub(crate) static_version: bool,
  /// Whether `[project] dynamic` names `version` (maturin then reads the crate manifest).
  pub(crate) dynamic_version: bool,
}

/// The Node main package's facts.
pub(crate) struct RootPackage {
  /// The file, for messages.
  pub(crate) path: String,
  /// The package name, the prefix of every platform package name.
  pub(crate) name: String,
  /// The literal version npm publishes.
  pub(crate) version: String,
  /// `napi.binaryName`: the stem of every `<binaryName>.<platform>.node` file.
  pub(crate) binary_name: String,
  /// `napi.targets`: the Rust target triples the lane builds, in order.
  pub(crate) targets: Vec<String>,
  /// `optionalDependencies`: the platform packages pinned by name to a version.
  pub(crate) optional_dependencies: BTreeMap<String, String>,
  /// Whether `publishConfig.access` is `public` (a scoped package is restricted otherwise).
  pub(crate) access_public: bool,
}

/// One platform package's facts.
pub(crate) struct PlatformPackage {
  /// The manifest, for messages.
  pub(crate) path: String,
  /// The package's README, for messages.
  pub(crate) readme_path: String,
  /// The package name; must be `<root name>-<platform id>`.
  pub(crate) name: String,
  /// The version; must equal the workspace version.
  pub(crate) version: String,
  /// `main`; must be the platform's binary file name.
  pub(crate) main: String,
  /// `files`; must be exactly the binary file name.
  pub(crate) files: Vec<String>,
  /// `os`; must be exactly Node's platform name.
  pub(crate) os: Vec<String>,
  /// `cpu`; must be exactly Node's arch name.
  pub(crate) cpu: Vec<String>,
  /// `libc`; must be exactly the C library on Linux and absent elsewhere.
  pub(crate) libc: Option<Vec<String>>,
  /// Whether `publishConfig.access` is `public`.
  pub(crate) access_public: bool,
  /// The README text; must be napi's template for the name and triple.
  pub(crate) readme: String,
}

/// One thing the tree gets wrong, with its file and the remedy in plain English. The set is closed:
/// an uncategorized finding would be a bug in this task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finding {
  /// `pyproject.toml` carries a literal `[project] version`.
  PyprojectStaticVersion { path: String },
  /// `pyproject.toml` does not list `version` under `[project] dynamic`.
  PyprojectVersionNotDynamic { path: String },
  /// A `package.json` version differs from the workspace version.
  VersionDrift {
    path: String,
    found: String,
    expected: String,
  },
  /// A platform package's name is not `<root name>-<platform id>`.
  NameDrift {
    path: String,
    found: String,
    expected: String,
  },
  /// A `napi.targets` entry names a system slates does not ship.
  UnknownTarget { triple: String },
  /// A `napi.targets` entry has no platform directory.
  MissingPlatformDirectory { platform: String, triple: String },
  /// A platform directory names no `napi.targets` entry.
  StrayPlatformDirectory { platform: String },
  /// A platform package's `main`, `files`, `os`, `cpu` or `libc` would make npm pick the wrong
  /// binary, or none.
  FieldDrift {
    path: String,
    field: &'static str,
    found: String,
    expected: String,
  },
  /// A platform README is not napi's template for its name and triple.
  ReadmeDrift { path: String },
  /// A package's `publishConfig.access` is not `public`.
  AccessNotPublic { path: String },
  /// A platform package the main package does not pin.
  OptionalDependencyMissing { name: String },
  /// A pin that names another version than the workspace's.
  OptionalDependencyDrift {
    name: String,
    found: String,
    expected: String,
  },
  /// A pin on a package no target produces.
  OptionalDependencyStray { name: String },
  /// The writer found no line for a field it must stamp.
  LineNotFound { key: String },
  /// The writer found several lines for a field it must stamp.
  LineAmbiguous { key: String },
}

impl fmt::Display for Finding {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::PyprojectStaticVersion { path } => write!(
        f,
        "{path}: `[project] version` is a second hand-edited copy; remove it and keep `dynamic = [\"version\"]` (maturin reads the crate manifest)"
      ),
      Self::PyprojectVersionNotDynamic { path } => write!(
        f,
        "{path}: `[project] dynamic` must list \"version\" so maturin derives the wheel version from the crate manifest"
      ),
      Self::VersionDrift {
        path,
        found,
        expected,
      } => write!(
        f,
        "{path}: version `{found}` is not the workspace version `{expected}`; run `cargo xtask version --write`"
      ),
      Self::NameDrift {
        path,
        found,
        expected,
      } => write!(
        f,
        "{path}: name `{found}` is not the derived `{expected}`; run `cargo xtask version --write`"
      ),
      Self::UnknownTarget { triple } => write!(
        f,
        "{NODE_PACKAGE}: napi.targets names `{triple}`, which is not a system slates ships (darwin, linux-gnu, linux-musl, windows-msvc on x86_64, aarch64 or i686)"
      ),
      Self::MissingPlatformDirectory { platform, triple } => write!(
        f,
        "{NODE_PLATFORMS}/{platform}: missing, but napi.targets names `{triple}`; create it with `napi create-npm-dirs` in crates/sdk-node, then `cargo xtask version --write`"
      ),
      Self::StrayPlatformDirectory { platform } => write!(
        f,
        "{NODE_PLATFORMS}/{platform}: no napi.targets entry produces it; remove the directory or add the target"
      ),
      Self::FieldDrift {
        path,
        field,
        found,
        expected,
      } => write!(
        f,
        "{path}: `{field}` is {found}, expected {expected} (npm would pick the wrong binary); regenerate the directory with `napi create-npm-dirs`"
      ),
      Self::ReadmeDrift { path } => write!(
        f,
        "{path}: not napi's template for its name and triple; run `cargo xtask version --write`"
      ),
      Self::AccessNotPublic { path } => write!(
        f,
        "{path}: `publishConfig.access` must be \"public\" (a scoped package publishes restricted otherwise)"
      ),
      Self::OptionalDependencyMissing { name } => write!(
        f,
        "{NODE_PACKAGE}: optionalDependencies lacks `{name}`; run `cargo xtask version --write`"
      ),
      Self::OptionalDependencyDrift {
        name,
        found,
        expected,
      } => write!(
        f,
        "{NODE_PACKAGE}: optionalDependencies pins `{name}` at `{found}`, not the workspace version `{expected}`; run `cargo xtask version --write`"
      ),
      Self::OptionalDependencyStray { name } => write!(
        f,
        "{NODE_PACKAGE}: optionalDependencies pins `{name}`, which no napi.targets entry produces; run `cargo xtask version --write`"
      ),
      Self::LineNotFound { key } => write!(
        f,
        "no line `{INDENT}\"{key}\": ...` at the top level to stamp (the file is not in npm's two-space shape)"
      ),
      Self::LineAmbiguous { key } => write!(
        f,
        "several lines `{INDENT}\"{key}\": ...` at the top level; the file is not one JSON object in npm's shape"
      ),
    }
  }
}

/// Judges the facts; an empty result is a consistent tree.
pub(crate) fn audit(truth: &Truth) -> Vec<Finding> {
  let mut findings = Vec::new();
  audit_pyproject(&truth.pyproject, &mut findings);
  audit_root(&truth.root, &truth.workspace_version, &mut findings);
  audit_platforms(truth, &mut findings);
  audit_optional_dependencies(&truth.root, &truth.workspace_version, &mut findings);
  findings
}

fn audit_pyproject(pyproject: &PyProject, findings: &mut Vec<Finding>) {
  if pyproject.static_version {
    findings.push(Finding::PyprojectStaticVersion {
      path: pyproject.path.clone(),
    });
  }
  if !pyproject.dynamic_version {
    findings.push(Finding::PyprojectVersionNotDynamic {
      path: pyproject.path.clone(),
    });
  }
}

fn audit_root(root: &RootPackage, workspace_version: &str, findings: &mut Vec<Finding>) {
  if root.version != workspace_version {
    findings.push(Finding::VersionDrift {
      path: root.path.clone(),
      found: root.version.clone(),
      expected: workspace_version.to_owned(),
    });
  }
  if !root.access_public {
    findings.push(Finding::AccessNotPublic {
      path: root.path.clone(),
    });
  }
}

fn audit_platforms(truth: &Truth, findings: &mut Vec<Finding>) {
  let mut expected_dirs: Vec<String> = Vec::new();
  for triple in &truth.root.targets {
    match platform_of(triple) {
      Err(finding) => findings.push(finding),
      Ok(platform) => {
        expected_dirs.push(platform.id.clone());
        match truth.platforms.get(&platform.id) {
          None => findings.push(Finding::MissingPlatformDirectory {
            platform: platform.id.clone(),
            triple: triple.clone(),
          }),
          Some(package) => audit_platform(package, &platform, truth, findings),
        }
      }
    }
  }
  for dir in &truth.platform_dirs {
    if !expected_dirs.contains(dir) {
      findings.push(Finding::StrayPlatformDirectory {
        platform: dir.clone(),
      });
    }
  }
}

fn audit_platform(
  package: &PlatformPackage,
  platform: &Platform,
  truth: &Truth,
  findings: &mut Vec<Finding>,
) {
  let name = platform_name(&truth.root.name, platform);
  if package.name != name {
    findings.push(Finding::NameDrift {
      path: package.path.clone(),
      found: package.name.clone(),
      expected: name.clone(),
    });
  }
  if package.version != truth.workspace_version {
    findings.push(Finding::VersionDrift {
      path: package.path.clone(),
      found: package.version.clone(),
      expected: truth.workspace_version.clone(),
    });
  }
  audit_platform_fields(package, platform, &truth.root.binary_name, findings);
  if package.readme != platform_readme(&name, &platform.triple, &truth.root.name) {
    findings.push(Finding::ReadmeDrift {
      path: package.readme_path.clone(),
    });
  }
  if !package.access_public {
    findings.push(Finding::AccessNotPublic {
      path: package.path.clone(),
    });
  }
}

/// The fields npm reads to pick one platform package for the running machine, and the binary the
/// loader then requires from it.
fn audit_platform_fields(
  package: &PlatformPackage,
  platform: &Platform,
  binary_name: &str,
  findings: &mut Vec<Finding>,
) {
  let binary = binary_file(binary_name, platform);
  let libc: Option<Vec<String>> = platform.libc.map(|l| vec![l.to_owned()]);
  let checks: [(&'static str, String, String); 5] = [
    ("main", package.main.clone(), binary.clone()),
    (
      "files",
      format!("{:?}", package.files),
      format!("{:?}", [binary]),
    ),
    (
      "os",
      format!("{:?}", package.os),
      format!("{:?}", [platform.os]),
    ),
    (
      "cpu",
      format!("{:?}", package.cpu),
      format!("{:?}", [platform.cpu]),
    ),
    ("libc", format!("{:?}", package.libc), format!("{:?}", libc)),
  ];
  for (field, found, expected) in checks {
    if found != expected {
      findings.push(Finding::FieldDrift {
        path: package.path.clone(),
        field,
        found,
        expected,
      });
    }
  }
}

fn audit_optional_dependencies(
  root: &RootPackage,
  workspace_version: &str,
  findings: &mut Vec<Finding>,
) {
  let expected: Vec<String> = root
    .targets
    .iter()
    .filter_map(|triple| platform_of(triple).ok())
    .map(|platform| platform_name(&root.name, &platform))
    .collect();
  for name in &expected {
    match root.optional_dependencies.get(name) {
      None => findings.push(Finding::OptionalDependencyMissing { name: name.clone() }),
      Some(found) if found != workspace_version => {
        findings.push(Finding::OptionalDependencyDrift {
          name: name.clone(),
          found: found.clone(),
          expected: workspace_version.to_owned(),
        });
      }
      Some(_) => {}
    }
  }
  for name in root.optional_dependencies.keys() {
    if !expected.contains(name) {
      findings.push(Finding::OptionalDependencyStray { name: name.clone() });
    }
  }
}

/// `<root name>-<platform id>`: napi's platform package name.
fn platform_name(root_name: &str, platform: &Platform) -> String {
  format!("{root_name}-{}", platform.id)
}

/// `<binaryName>.<platform id>.node`: the file napi builds and the loader requires.
fn binary_file(binary_name: &str, platform: &Platform) -> String {
  format!("{binary_name}.{}.node", platform.id)
}

/// napi's README for a platform package (`napi create-npm-dirs` writes exactly this).
fn platform_readme(name: &str, triple: &str, root_name: &str) -> String {
  format!("# `{name}`\n\nThis is the **{triple}** binary for `{root_name}`\n")
}

/// Reads the facts from the tree.
pub(crate) fn read_truth(root: &Path) -> Result<Truth, Failure> {
  let platform_dirs = read_platform_dirs(root)?;
  let mut platforms = BTreeMap::new();
  for dir in &platform_dirs {
    platforms.insert(dir.clone(), read_platform_package(root, dir)?);
  }
  Ok(Truth {
    workspace_version: read_workspace_version(root)?,
    pyproject: read_pyproject(root)?,
    root: read_root_package(root)?,
    platform_dirs,
    platforms,
  })
}

fn read_toml(path: &Path) -> Result<toml::Value, Failure> {
  let text = std::fs::read_to_string(path)?;
  toml::from_str(&text).map_err(|e| Failure(format!("{}: toml: {e}", path.display())))
}

fn read_workspace_version(root: &Path) -> Result<String, Failure> {
  let manifest = read_toml(&root.join("Cargo.toml"))?;
  manifest
    .get("workspace")
    .and_then(|w| w.get("package"))
    .and_then(|p| p.get("version"))
    .and_then(toml::Value::as_str)
    .map(str::to_owned)
    .ok_or_else(|| Failure("Cargo.toml: no `[workspace.package] version`".to_owned()))
}

fn read_pyproject(root: &Path) -> Result<PyProject, Failure> {
  let value = read_toml(&root.join(PYPROJECT))?;
  let project = value
    .get("project")
    .ok_or_else(|| Failure(format!("{PYPROJECT}: no `[project]` table")))?;
  let dynamic_version = project
    .get("dynamic")
    .and_then(toml::Value::as_array)
    .is_some_and(|names| names.iter().any(|n| n.as_str() == Some("version")));
  Ok(PyProject {
    path: PYPROJECT.to_owned(),
    static_version: project.get("version").is_some(),
    dynamic_version,
  })
}

fn read_json(path: &Path) -> Result<Value, Failure> {
  let text = std::fs::read_to_string(path)?;
  serde_json::from_str(&text).map_err(|e| Failure(format!("{}: json: {e}", path.display())))
}

/// A required string field of a JSON object, named by a dotted path for the message.
fn json_string(value: &Value, file: &str, key: &str) -> Result<String, Failure> {
  value
    .get(key)
    .and_then(Value::as_str)
    .map(str::to_owned)
    .ok_or_else(|| Failure(format!("{file}: no string field `{key}`")))
}

/// A string-array field, or empty when absent.
fn json_strings(value: &Value, key: &str) -> Option<Vec<String>> {
  value.get(key).and_then(Value::as_array).map(|items| {
    items
      .iter()
      .filter_map(Value::as_str)
      .map(str::to_owned)
      .collect()
  })
}

fn access_is_public(value: &Value) -> bool {
  value
    .get("publishConfig")
    .and_then(|p| p.get("access"))
    .and_then(Value::as_str)
    == Some("public")
}

fn read_root_package(root: &Path) -> Result<RootPackage, Failure> {
  let value = read_json(&root.join(NODE_PACKAGE))?;
  let napi = value
    .get("napi")
    .ok_or_else(|| Failure(format!("{NODE_PACKAGE}: no `napi` object")))?;
  let optional_dependencies = value
    .get("optionalDependencies")
    .and_then(Value::as_object)
    .map(|pins| {
      pins
        .iter()
        .filter_map(|(name, version)| Some((name.clone(), version.as_str()?.to_owned())))
        .collect()
    })
    .unwrap_or_default();
  Ok(RootPackage {
    path: NODE_PACKAGE.to_owned(),
    name: json_string(&value, NODE_PACKAGE, "name")?,
    version: json_string(&value, NODE_PACKAGE, "version")?,
    binary_name: json_string(napi, NODE_PACKAGE, "binaryName")?,
    targets: json_strings(napi, "targets")
      .ok_or_else(|| Failure(format!("{NODE_PACKAGE}: no `napi.targets` list")))?,
    optional_dependencies,
    access_public: access_is_public(&value),
  })
}

fn read_platform_dirs(root: &Path) -> Result<Vec<String>, Failure> {
  let mut dirs: Vec<String> = std::fs::read_dir(root.join(NODE_PLATFORMS))?
    .filter_map(Result::ok)
    .filter(|entry| entry.path().is_dir())
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .collect();
  dirs.sort();
  Ok(dirs)
}

fn read_platform_package(root: &Path, dir: &str) -> Result<PlatformPackage, Failure> {
  let path = format!("{NODE_PLATFORMS}/{dir}/package.json");
  let readme_path = format!("{NODE_PLATFORMS}/{dir}/README.md");
  let value = read_json(&root.join(&path))?;
  Ok(PlatformPackage {
    name: json_string(&value, &path, "name")?,
    version: json_string(&value, &path, "version")?,
    main: json_string(&value, &path, "main")?,
    files: json_strings(&value, "files").unwrap_or_default(),
    os: json_strings(&value, "os").unwrap_or_default(),
    cpu: json_strings(&value, "cpu").unwrap_or_default(),
    libc: json_strings(&value, "libc"),
    access_public: access_is_public(&value),
    readme: std::fs::read_to_string(root.join(&readme_path)).unwrap_or_default(),
    path,
    readme_path,
  })
}

/// Runs the task: `--write` first re-derives the npm copies; then the audit must be clean; then the
/// release guard, if asked, compares the tag.
pub(crate) fn run(root: &Path, mode: &Mode) -> Result<(), Failure> {
  if matches!(mode, Mode::Write) {
    write(root, &read_truth(root)?)?;
  }
  let truth = read_truth(root)?;
  let findings = audit(&truth);
  if !findings.is_empty() {
    for finding in &findings {
      eprintln!("version: {finding}");
    }
    return Err(Failure(format!(
      "{} version/packaging finding(s)",
      findings.len()
    )));
  }
  if let Mode::ExpectTag(tag) = mode {
    let expected = format!("v{}", truth.workspace_version);
    if *tag != expected {
      return Err(Failure(format!(
        "tag `{tag}` does not name the workspace version (expected `{expected}`); bump `[workspace.package] version`, run `cargo xtask version --write`, commit, then tag"
      )));
    }
  }
  println!(
    "version: ok ({} in Cargo.toml; the Python wheel derives it through maturin; {} and its {} platform packages pinned to it)",
    truth.workspace_version,
    truth.root.name,
    truth.root.targets.len()
  );
  Ok(())
}

/// Re-derives the npm copies: the main package's version and pins, and each platform package's
/// name, version and README. Shape facts (`main`, `files`, `os`, `cpu`, `libc`) are not stamped —
/// a wrong one means a wrong directory, which `napi create-npm-dirs` regenerates — so the audit
/// that follows still judges them.
fn write(root: &Path, truth: &Truth) -> Result<(), Failure> {
  let platforms: Vec<Platform> = truth
    .root
    .targets
    .iter()
    .map(|triple| platform_of(triple).map_err(|f| Failure(f.to_string())))
    .collect::<Result<_, _>>()?;
  let pins: Vec<(String, String)> = platforms
    .iter()
    .map(|p| {
      (
        platform_name(&truth.root.name, p),
        truth.workspace_version.clone(),
      )
    })
    .collect();
  let path = root.join(NODE_PACKAGE);
  let text = std::fs::read_to_string(&path)?;
  let stamped = restamp_field(&text, "version", &truth.workspace_version)
    .and_then(|t| restamp_optional_dependencies(&t, &pins))
    .map_err(|f| Failure(format!("{NODE_PACKAGE}: {f}")))?;
  write_json_verified(&path, &stamped)?;
  for platform in &platforms {
    write_platform(root, truth, platform)?;
  }
  Ok(())
}

fn write_platform(root: &Path, truth: &Truth, platform: &Platform) -> Result<(), Failure> {
  let dir = root.join(NODE_PLATFORMS).join(&platform.id);
  let path = dir.join("package.json");
  let text = std::fs::read_to_string(&path).map_err(|e| {
    Failure(format!(
      "{}: {e}; create the directory with `napi create-npm-dirs` in crates/sdk-node",
      path.display()
    ))
  })?;
  let name = platform_name(&truth.root.name, platform);
  let stamped = restamp_field(&text, "name", &name)
    .and_then(|t| restamp_field(&t, "version", &truth.workspace_version))
    .map_err(|f| Failure(format!("{}: {f}", path.display())))?;
  write_json_verified(&path, &stamped)?;
  write_text(
    &dir.join("README.md"),
    &platform_readme(&name, &platform.triple, &truth.root.name),
  )
}

/// Writes a JSON document only after it parses: a stamp that broke the file is refused, never
/// written.
fn write_json_verified(path: &Path, text: &str) -> Result<(), Failure> {
  serde_json::from_str::<Value>(text).map_err(|e| {
    Failure(format!(
      "{}: the stamped text is not JSON: {e}",
      path.display()
    ))
  })?;
  write_text(path, text)
}

fn write_text(path: &Path, text: &str) -> Result<(), Failure> {
  // The development tool rewriting the tree's own manifests (not a host path of the product).
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, text)?;
  Ok(())
}

/// Replaces the value of the top-level string field `key` in a `package.json` in npm's shape,
/// keeping every other byte. Exactly one such line must exist.
pub(crate) fn restamp_field(text: &str, key: &str, value: &str) -> Result<String, Finding> {
  let prefix = format!("{INDENT}\"{key}\": \"");
  let hits = text
    .lines()
    .filter(|line| line.starts_with(&prefix))
    .count();
  match hits {
    0 => {
      return Err(Finding::LineNotFound {
        key: key.to_owned(),
      });
    }
    1 => {}
    _ => {
      return Err(Finding::LineAmbiguous {
        key: key.to_owned(),
      });
    }
  }
  let lines: Vec<String> = text
    .lines()
    .map(|line| {
      if line.starts_with(&prefix) {
        let rest = &line[prefix.len()..];
        let tail = rest.find('"').map_or("\"", |end| &rest[end..]);
        format!("{prefix}{value}{tail}")
      } else {
        line.to_owned()
      }
    })
    .collect();
  Ok(join_lines(&lines, text.ends_with('\n')))
}

/// Replaces the body of the top-level `optionalDependencies` object with the given pins, in order,
/// keeping every other byte.
pub(crate) fn restamp_optional_dependencies(
  text: &str,
  pins: &[(String, String)],
) -> Result<String, Finding> {
  let open = format!("{INDENT}\"optionalDependencies\": {{");
  let close = format!("{INDENT}}}");
  let not_found = || Finding::LineNotFound {
    key: "optionalDependencies".to_owned(),
  };
  let lines: Vec<&str> = text.lines().collect();
  let start = lines
    .iter()
    .position(|line| *line == open)
    .ok_or_else(not_found)?;
  let end = lines[start..]
    .iter()
    .position(|line| line.starts_with(&close))
    .map(|offset| start + offset)
    .ok_or_else(not_found)?;
  let mut out: Vec<String> = lines[..=start].iter().map(|s| (*s).to_owned()).collect();
  for (index, (name, version)) in pins.iter().enumerate() {
    let comma = if index + 1 < pins.len() { "," } else { "" };
    out.push(format!("{INDENT}{INDENT}\"{name}\": \"{version}\"{comma}"));
  }
  out.extend(lines[end..].iter().map(|s| (*s).to_owned()));
  Ok(join_lines(&out, text.ends_with('\n')))
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
  let mut out = lines.join("\n");
  if trailing_newline {
    out.push('\n');
  }
  out
}

/// Writes the 0.0.0 stub packages under `out`: `main/` for the main package and one directory per
/// platform. Refuses on a drifted tree, so the reserved names are the published names.
pub(crate) fn reserve(root: &Path, out: &Path) -> Result<(), Failure> {
  let truth = read_truth(root)?;
  let findings = audit(&truth);
  if let Some(first) = findings.first() {
    return Err(Failure(format!(
      "the tree has {} version/packaging finding(s) (first: {first}); fix them before reserving names",
      findings.len()
    )));
  }
  let source = read_json(&root.join(NODE_PACKAGE))?;
  let mut count = 0usize;
  write_stub(&out.join("main"), &stub_main(&truth.root, &source))?;
  count += 1;
  for triple in &truth.root.targets {
    let platform = platform_of(triple).map_err(|f| Failure(f.to_string()))?;
    write_stub(
      &out.join(&platform.id),
      &stub_platform(&truth.root, &source, &platform),
    )?;
    count += 1;
  }
  println!(
    "npm-reserve: {count} stub packages at {RESERVED_VERSION} written under {}",
    out.display()
  );
  Ok(())
}

/// The fields every stub carries, copied from the main package so the stub is attributed the same.
fn stub_common(source: &Value, name: &str, description: String) -> serde_json::Map<String, Value> {
  let mut object = serde_json::Map::new();
  object.insert("name".to_owned(), Value::String(name.to_owned()));
  object.insert(
    "version".to_owned(),
    Value::String(RESERVED_VERSION.to_owned()),
  );
  object.insert("description".to_owned(), Value::String(description));
  for key in [
    "license",
    "author",
    "repository",
    "homepage",
    "publishConfig",
  ] {
    if let Some(value) = source.get(key) {
      object.insert(key.to_owned(), value.clone());
    }
  }
  object.insert(
    "files".to_owned(),
    Value::Array(vec![Value::String("README.md".to_owned())]),
  );
  object
}

fn stub_main(root: &RootPackage, source: &Value) -> (Value, String) {
  let description = format!(
    "Reserved name of the slates Node SDK ({}). This {RESERVED_VERSION} stub exists so npm can attach a trusted publisher to the package (npm/cli#8544); the real package arrives with the first tagged release from the slates repository.",
    root.name
  );
  let readme = format!(
    "# `{}`\n\nReserved name. This {RESERVED_VERSION} stub exists so npm can attach a trusted publisher to the package (npm/cli#8544); the slates Node SDK arrives with the first tagged release from https://github.com/hyper-light/slates.\n",
    root.name
  );
  (
    Value::Object(stub_common(source, &root.name, description)),
    readme,
  )
}

fn stub_platform(root: &RootPackage, source: &Value, platform: &Platform) -> (Value, String) {
  let name = platform_name(&root.name, platform);
  let description = format!(
    "Reserved name of the {} binary package of {}. This {RESERVED_VERSION} stub exists so npm can attach a trusted publisher to the package (npm/cli#8544); the binary arrives with the first tagged release.",
    platform.triple, root.name
  );
  let mut object = stub_common(source, &name, description);
  object.insert(
    "os".to_owned(),
    Value::Array(vec![Value::String(platform.os.to_owned())]),
  );
  object.insert(
    "cpu".to_owned(),
    Value::Array(vec![Value::String(platform.cpu.to_owned())]),
  );
  if let Some(libc) = platform.libc {
    object.insert(
      "libc".to_owned(),
      Value::Array(vec![Value::String(libc.to_owned())]),
    );
  }
  let readme = format!(
    "# `{name}`\n\nReserved name. This {RESERVED_VERSION} stub exists so npm can attach a trusted publisher to the package (npm/cli#8544); the **{}** binary for `{}` arrives with the first tagged release from https://github.com/hyper-light/slates.\n",
    platform.triple, root.name
  );
  (Value::Object(object), readme)
}

fn write_stub(dir: &Path, (manifest, readme): &(Value, String)) -> Result<(), Failure> {
  // The development tool's own scratch output, outside the tree, named by the maintainer.
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir_all(dir)?;
  let mut text = serde_json::to_string_pretty(manifest)?;
  text.push('\n');
  write_text(&dir.join("package.json"), &text)?;
  write_text(&dir.join("README.md"), readme)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The nine triples slates ships, with napi's platform id and npm's selection fields for each.
  const SHIPPED: [(&str, &str, &str, &str, Option<&str>); 9] = [
    (
      "aarch64-apple-darwin",
      "darwin-arm64",
      "darwin",
      "arm64",
      None,
    ),
    ("x86_64-apple-darwin", "darwin-x64", "darwin", "x64", None),
    (
      "x86_64-unknown-linux-gnu",
      "linux-x64-gnu",
      "linux",
      "x64",
      Some("glibc"),
    ),
    (
      "aarch64-unknown-linux-gnu",
      "linux-arm64-gnu",
      "linux",
      "arm64",
      Some("glibc"),
    ),
    (
      "x86_64-unknown-linux-musl",
      "linux-x64-musl",
      "linux",
      "x64",
      Some("musl"),
    ),
    (
      "aarch64-unknown-linux-musl",
      "linux-arm64-musl",
      "linux",
      "arm64",
      Some("musl"),
    ),
    (
      "x86_64-pc-windows-msvc",
      "win32-x64-msvc",
      "win32",
      "x64",
      None,
    ),
    (
      "aarch64-pc-windows-msvc",
      "win32-arm64-msvc",
      "win32",
      "arm64",
      None,
    ),
    (
      "i686-pc-windows-msvc",
      "win32-ia32-msvc",
      "win32",
      "ia32",
      None,
    ),
  ];

  /// A consistent tree for the root name and version given: the platform packages and pins derived
  /// exactly as the writer derives them.
  fn consistent(root_name: &str, version: &str) -> Truth {
    let targets: Vec<String> = SHIPPED.iter().map(|s| s.0.to_owned()).collect();
    let mut platforms = BTreeMap::new();
    let mut pins = BTreeMap::new();
    for triple in &targets {
      let platform = platform_of(triple).unwrap();
      let name = platform_name(root_name, &platform);
      pins.insert(name.clone(), version.to_owned());
      platforms.insert(
        platform.id.clone(),
        PlatformPackage {
          path: format!("npm/{}/package.json", platform.id),
          readme_path: format!("npm/{}/README.md", platform.id),
          name: name.clone(),
          version: version.to_owned(),
          main: binary_file("slates", &platform),
          files: vec![binary_file("slates", &platform)],
          os: vec![platform.os.to_owned()],
          cpu: vec![platform.cpu.to_owned()],
          libc: platform.libc.map(|l| vec![l.to_owned()]),
          access_public: true,
          readme: platform_readme(&name, triple, root_name),
        },
      );
    }
    Truth {
      workspace_version: version.to_owned(),
      pyproject: PyProject {
        path: "pyproject.toml".to_owned(),
        static_version: false,
        dynamic_version: true,
      },
      root: RootPackage {
        path: "package.json".to_owned(),
        name: root_name.to_owned(),
        version: version.to_owned(),
        binary_name: "slates".to_owned(),
        targets,
        optional_dependencies: pins,
        access_public: true,
      },
      platform_dirs: platforms.keys().cloned().collect(),
      platforms,
    }
  }

  #[test]
  fn napi_platform_rule_names_every_shipped_triple_and_refuses_the_rest() {
    for (triple, id, os, cpu, libc) in SHIPPED {
      let platform = platform_of(triple).unwrap();
      assert_eq!(platform.id, id, "{triple}");
      assert_eq!(platform.os, os, "{triple}");
      assert_eq!(platform.cpu, cpu, "{triple}");
      assert_eq!(platform.libc, libc, "{triple}");
    }
    for stranger in [
      "armv7-unknown-linux-gnueabihf",
      "x86_64-unknown-freebsd",
      "wasm32-wasi",
      "aarch64-linux-android",
      "x86_64-pc-windows-gnu",
      "nonsense",
    ] {
      assert_eq!(
        platform_of(stranger),
        Err(Finding::UnknownTarget {
          triple: stranger.to_owned()
        }),
        "{stranger}"
      );
    }
  }

  #[test]
  fn a_consistent_tree_has_no_findings() {
    assert_eq!(audit(&consistent("@hyper-light/slates", "0.1.0")), vec![]);
  }

  #[test]
  fn every_hand_copy_that_drifts_is_named_with_its_file() {
    let mut truth = consistent("@hyper-light/slates", "0.1.0");
    truth.root.version = "0.2.0".to_owned();
    let package = truth.platforms.get_mut("linux-x64-musl").unwrap();
    package.version = "0.0.9".to_owned();
    package.name = "slates-linux-x64-musl".to_owned();
    truth.root.optional_dependencies.insert(
      "@hyper-light/slates-win32-ia32-msvc".to_owned(),
      "0.0.1".to_owned(),
    );
    truth
      .root
      .optional_dependencies
      .remove("@hyper-light/slates-darwin-x64");
    truth.root.optional_dependencies.insert(
      "@hyper-light/slates-freebsd-x64".to_owned(),
      "0.1.0".to_owned(),
    );
    let findings = audit(&truth);
    assert!(findings.contains(&Finding::VersionDrift {
      path: "package.json".to_owned(),
      found: "0.2.0".to_owned(),
      expected: "0.1.0".to_owned(),
    }));
    assert!(findings.contains(&Finding::VersionDrift {
      path: "npm/linux-x64-musl/package.json".to_owned(),
      found: "0.0.9".to_owned(),
      expected: "0.1.0".to_owned(),
    }));
    assert!(findings.contains(&Finding::NameDrift {
      path: "npm/linux-x64-musl/package.json".to_owned(),
      found: "slates-linux-x64-musl".to_owned(),
      expected: "@hyper-light/slates-linux-x64-musl".to_owned(),
    }));
    assert!(findings.contains(&Finding::OptionalDependencyDrift {
      name: "@hyper-light/slates-win32-ia32-msvc".to_owned(),
      found: "0.0.1".to_owned(),
      expected: "0.1.0".to_owned(),
    }));
    assert!(findings.contains(&Finding::OptionalDependencyMissing {
      name: "@hyper-light/slates-darwin-x64".to_owned(),
    }));
    assert!(findings.contains(&Finding::OptionalDependencyStray {
      name: "@hyper-light/slates-freebsd-x64".to_owned(),
    }));
    assert_eq!(findings.len(), 6, "{findings:?}");
  }

  #[test]
  fn a_platform_package_that_would_misdirect_npm_is_refused_field_by_field() {
    let mut truth = consistent("@hyper-light/slates", "0.1.0");
    let package = truth.platforms.get_mut("linux-arm64-gnu").unwrap();
    package.cpu = vec!["x64".to_owned()];
    package.libc = Some(vec!["musl".to_owned()]);
    package.main = "slates.linux-arm64-gnu.so".to_owned();
    package.access_public = false;
    package.readme = "# stale\n".to_owned();
    let findings = audit(&truth);
    let fields: Vec<&str> = findings
      .iter()
      .filter_map(|f| match f {
        Finding::FieldDrift { field, .. } => Some(*field),
        _ => None,
      })
      .collect();
    assert_eq!(fields, vec!["main", "cpu", "libc"]);
    assert!(findings.contains(&Finding::AccessNotPublic {
      path: "npm/linux-arm64-gnu/package.json".to_owned(),
    }));
    assert!(findings.contains(&Finding::ReadmeDrift {
      path: "npm/linux-arm64-gnu/README.md".to_owned(),
    }));
  }

  #[test]
  fn a_target_without_its_directory_and_a_stray_directory_are_both_refused() {
    let mut truth = consistent("@hyper-light/slates", "0.1.0");
    truth.platforms.remove("win32-x64-msvc");
    truth.platform_dirs.retain(|d| d != "win32-x64-msvc");
    truth.platform_dirs.push("android-arm64".to_owned());
    truth.root.targets.push("x86_64-unknown-freebsd".to_owned());
    let findings = audit(&truth);
    assert!(findings.contains(&Finding::MissingPlatformDirectory {
      platform: "win32-x64-msvc".to_owned(),
      triple: "x86_64-pc-windows-msvc".to_owned(),
    }));
    assert!(findings.contains(&Finding::StrayPlatformDirectory {
      platform: "android-arm64".to_owned(),
    }));
    assert!(findings.contains(&Finding::UnknownTarget {
      triple: "x86_64-unknown-freebsd".to_owned(),
    }));
  }

  #[test]
  fn a_static_python_version_is_a_second_copy_and_is_refused() {
    let mut truth = consistent("@hyper-light/slates", "0.1.0");
    truth.pyproject.static_version = true;
    truth.pyproject.dynamic_version = false;
    let findings = audit(&truth);
    assert!(findings.contains(&Finding::PyprojectStaticVersion {
      path: "pyproject.toml".to_owned(),
    }));
    assert!(findings.contains(&Finding::PyprojectVersionNotDynamic {
      path: "pyproject.toml".to_owned(),
    }));
  }

  const PACKAGE: &str = r#"{
  "name": "@hyper-light/slates",
  "version": "0.1.0",
  "description": "x",
  "napi": {
    "binaryName": "slates",
    "targets": [
      "aarch64-apple-darwin"
    ]
  },
  "optionalDependencies": {
    "@hyper-light/slates-darwin-arm64": "0.1.0",
    "@hyper-light/slates-darwin-x64": "0.1.0"
  },
  "engines": {
    "node": ">= 18"
  }
}
"#;

  #[test]
  fn stamping_a_field_changes_that_value_and_nothing_else() {
    let stamped = restamp_field(PACKAGE, "version", "0.2.0").unwrap();
    let parsed: Value = serde_json::from_str(&stamped).unwrap();
    assert_eq!(parsed["version"], "0.2.0");
    assert_eq!(parsed["name"], "@hyper-light/slates");
    // Every other byte is kept: stamping the old value back restores the original exactly.
    assert_eq!(
      restamp_field(&stamped, "version", "0.1.0").unwrap(),
      PACKAGE
    );
    // The nested `binaryName` is not a top-level field: no line to stamp.
    assert_eq!(
      restamp_field(PACKAGE, "binaryName", "other"),
      Err(Finding::LineNotFound {
        key: "binaryName".to_owned()
      })
    );
  }

  #[test]
  fn stamping_the_pins_replaces_the_block_in_order_and_keeps_the_rest() {
    let pins = vec![
      (
        "@hyper-light/slates-darwin-arm64".to_owned(),
        "0.2.0".to_owned(),
      ),
      (
        "@hyper-light/slates-linux-x64-gnu".to_owned(),
        "0.2.0".to_owned(),
      ),
      (
        "@hyper-light/slates-win32-x64-msvc".to_owned(),
        "0.2.0".to_owned(),
      ),
    ];
    let stamped = restamp_optional_dependencies(PACKAGE, &pins).unwrap();
    let parsed: Value = serde_json::from_str(&stamped).unwrap();
    let block = parsed["optionalDependencies"].as_object().unwrap();
    assert_eq!(block.len(), 3);
    assert_eq!(block["@hyper-light/slates-linux-x64-gnu"], "0.2.0");
    assert!(!block.contains_key("@hyper-light/slates-darwin-x64"));
    assert_eq!(parsed["engines"]["node"], ">= 18");
    assert!(stamped.ends_with("}\n"));
    // The block's text is in the same shape npm writes: four-space entries, no trailing comma.
    assert!(stamped.contains("    \"@hyper-light/slates-win32-x64-msvc\": \"0.2.0\"\n  },"));
    // A file without the block is refused, not silently extended.
    assert_eq!(
      restamp_optional_dependencies("{\n  \"name\": \"x\"\n}\n", &pins),
      Err(Finding::LineNotFound {
        key: "optionalDependencies".to_owned()
      })
    );
  }

  #[test]
  fn the_stubs_carry_the_derived_names_and_npm_selection_fields() {
    let truth = consistent("@hyper-light/slates", "0.1.0");
    let source: Value = serde_json::from_str(PACKAGE).unwrap();
    let (main, readme) = stub_main(&truth.root, &source);
    assert_eq!(main["name"], "@hyper-light/slates");
    assert_eq!(main["version"], RESERVED_VERSION);
    assert!(readme.contains("Reserved name"));
    let platform = platform_of("aarch64-unknown-linux-musl").unwrap();
    let (stub, _) = stub_platform(&truth.root, &source, &platform);
    assert_eq!(stub["name"], "@hyper-light/slates-linux-arm64-musl");
    assert_eq!(stub["os"][0], "linux");
    assert_eq!(stub["cpu"][0], "arm64");
    assert_eq!(stub["libc"][0], "musl");
    let (darwin, _) = stub_platform(
      &truth.root,
      &source,
      &platform_of("x86_64-apple-darwin").unwrap(),
    );
    assert!(darwin.get("libc").is_none());
  }
}
