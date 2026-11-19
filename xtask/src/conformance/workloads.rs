//! The workload suite (Part 6 "Real workloads"; AC-3.2, AC-4.2): every roster tool present on
//! this host runs its script twice under one fixed environment — in a host directory and in a
//! directory inside the mount — and the two runs must be byte-identical: same exit code, same
//! output with the directory roots normalized, same tree manifest under the roster's reviewed
//! exclusions. The volume is created with the host directory's name-folding policy so git's
//! `core.ignorecase` probe sees the same filesystem on both sides (`EQUIVALENCE.md` §4).
//!
//! On Windows (A-300) the lane runs the roster's version-control workloads over the WinFsp drive, each script run by
//! Git for Windows' `sh` in an NTFS folder of the scratch and in a folder of the drive, in a directory's plain drive
//! form (`D:\a`, never the verbatim `\\?\D:\a` a canonical path has, which a shell's `pwd` would not print); and the
//! drive's runs are judged under its stated differences (`slates_conformance::workload::stated_differences`), which
//! hold only where the drive refuses a hard link: before the workloads run, the harness gives one file of the mount's
//! workload root a second name ([`mount_links`], A-498), so a drive under a driver carrying `docs/wip/winfsp-patches`
//! is held to NTFS's link counts.

use std::path::Path;
use std::process::{Command, Stdio};

use slates_conformance::Suite;
use slates_conformance::capability::HostOs;
use slates_conformance::record::{Counts, WorkloadResult, WorkloadStatus};
use slates_conformance::workload::{
  ENV_SQLITE_BUSY_MS, ENV_WATCH_SECONDS, ENVIRONMENT, Entry, EntryKind, HardLinks, Manifest,
  ROSTER, Workload, compare_with, stated_differences,
};

use super::slates::{Session, folds_names};
use super::{
  Run, SQLITE_BUSY_MS, SuiteResult, WATCH_SECONDS, create_dir, plain_path, tool_on_path,
};
use crate::Failure;

/// Format: git's variable for configuration passed on its own command line (`git -c`), which a workload must not
/// inherit from the process that started the harness ([`workload_environment`]).
const GIT_CONFIG_PARAMETERS: &str = "GIT_CONFIG_PARAMETERS";

/// Format: a file's permission bits in `st_mode`.
const PERMISSION_MASK: u32 = 0o7777;

/// Whether a roster entry is the watcher for another operating system.
fn foreign_watcher(workload: &Workload, os: HostOs) -> bool {
  workload.name == "watcher" && (workload.tool == "fswatch") != (os == HostOs::Macos)
}

/// Whether this host's lane runs a roster entry: the watcher of its own operating system, and on Windows only what the
/// roster marks for it (`Workload::runs_on_windows`, A-300).
fn runs_here(workload: &Workload, os: HostOs) -> bool {
  !foreign_watcher(workload, os) && (os != HostOs::Windows || workload.runs_on_windows)
}

/// The note a Windows record carries about the roster's other workloads (A-300): each is owed a Windows form, named
/// once (both watchers are `watcher`). `None` on any other host, which runs the whole roster.
fn owed_on_windows_note(os: HostOs) -> Option<String> {
  if os != HostOs::Windows {
    return None;
  }
  let mut owed: Vec<&str> = ROSTER
    .iter()
    .filter(|workload| !workload.runs_on_windows)
    .map(|workload| workload.name)
    .collect();
  owed.dedup();
  Some(format!(
    "on Windows the roster runs its version-control workloads (A-300); owed a Windows form: {}",
    owed.join(", ")
  ))
}

/// The BLAKE3 of a file's bytes, hexadecimal.
fn digest_of(path: &Path) -> Result<String, Failure> {
  let bytes = std::fs::read(path).map_err(|e| Failure(format!("{}: {e}", path.display())))?;
  Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// A file's POSIX permission bits, as the mount reports them.
#[cfg(unix)]
fn permission_bits(metadata: &std::fs::Metadata) -> u32 {
  use std::os::unix::fs::PermissionsExt;
  metadata.permissions().mode()
}

/// Format: the bits a Windows file without the read-only attribute is compared as (`rwxrwxrwx`): Windows keeps no
/// POSIX mode, and its one permission attribute is read-only.
#[cfg(windows)]
const WINDOWS_WRITABLE_BITS: u32 = 0o777;
/// Format: the bits a Windows file with the read-only attribute is compared as (`r-xr-xr-x`).
#[cfg(windows)]
const WINDOWS_READ_ONLY_BITS: u32 = 0o555;

/// A file's permission bits on Windows: its read-only attribute, the one permission Windows keeps.
#[cfg(windows)]
fn permission_bits(metadata: &std::fs::Metadata) -> u32 {
  if metadata.permissions().readonly() {
    WINDOWS_READ_ONLY_BITS
  } else {
    WINDOWS_WRITABLE_BITS
  }
}

/// One manifest entry for a path under `root`.
fn entry_of(root: &Path, path: &Path) -> Result<Entry, Failure> {
  let metadata =
    std::fs::symlink_metadata(path).map_err(|e| Failure(format!("{}: {e}", path.display())))?;
  let relative = path
    .strip_prefix(root)
    .unwrap_or(path)
    .to_string_lossy()
    .replace('\\', "/");
  let file_type = metadata.file_type();
  let (kind, size, digest) = if file_type.is_symlink() {
    let target = std::fs::read_link(path)
      .map(|t| t.to_string_lossy().into_owned())
      .map_err(|error| Failure(format!("reading link {}: {error}", path.display())))?;
    (EntryKind::Symlink { target }, 0, String::new())
  } else if file_type.is_dir() {
    (EntryKind::Directory, 0, String::new())
  } else if file_type.is_file() {
    (EntryKind::File, metadata.len(), digest_of(path)?)
  } else {
    (EntryKind::Other, 0, String::new())
  };
  // A symlink's permission bits are not meaningful (POSIX ignores them — access is through the target)
  // and are not portable: Linux always reports 0o777, macOS reports the creation mode masked by the
  // umask (so 0o755 for a default umask), and the NFS-loopback mount reports 0o777. Normalize a
  // symlink's mode to the 0o777 convention so the comparison judges the tree, not a non-behavioural,
  // fs-specific value; a regular file's or directory's mode is compared as measured.
  let mode = if file_type.is_symlink() {
    0o777
  } else {
    permission_bits(&metadata) & PERMISSION_MASK
  };
  Ok(Entry {
    path: relative,
    kind,
    mode,
    size,
    digest,
  })
}

/// The manifest of a tree, sorted by path.
pub(super) fn manifest_of(root: &Path) -> Result<Manifest, Failure> {
  let mut entries = Vec::new();
  let mut stack = vec![root.to_path_buf()];
  while let Some(dir) = stack.pop() {
    for entry in std::fs::read_dir(&dir).map_err(|e| Failure(format!("{}: {e}", dir.display())))? {
      let path = entry?.path();
      let item = entry_of(root, &path)?;
      if matches!(item.kind, EntryKind::Directory) {
        stack.push(path);
      }
      entries.push(item);
    }
  }
  entries.sort_by(|a, b| a.path.cmp(&b.path));
  Ok(Manifest { entries })
}

/// Sets `command`'s environment for a workload: the roster's shared environment (the fixed identities and dates, no
/// configuration a parent passes; `slates_conformance::workload::ENVIRONMENT`), the tools' caches in `scratch`, the
/// watch and busy bounds, and none of the parent's git configuration.
fn workload_environment<'a>(command: &'a mut Command, scratch: &Path) -> &'a mut Command {
  command
    .envs(ENVIRONMENT.iter().copied())
    .env_remove(GIT_CONFIG_PARAMETERS)
    .env("npm_config_cache", scratch.join("npm-cache"))
    .env(ENV_SQLITE_BUSY_MS, SQLITE_BUSY_MS.to_string())
    .env(ENV_WATCH_SECONDS, WATCH_SECONDS.to_string())
}

/// A workload's script as `sh -c` runs it: its error output joined to its output, and stopped at its first failing
/// command.
fn shell_script(workload: &Workload) -> String {
  format!("exec 2>&1\nset -e\n{}", workload.script)
}

/// Runs a workload's script in a fresh directory and captures everything. The script starts in the directory's plain
/// form, which is the one its tools print it in (on Windows the drive form, not a canonical path's verbatim one).
fn execute(
  workload: &Workload,
  dir: &Path,
  scratch: &Path,
) -> Result<slates_conformance::workload::Run, Failure> {
  create_dir(dir)?;
  let directory = plain_path(dir);
  let mut command = Command::new("sh");
  command
    .arg("-c")
    .arg(shell_script(workload))
    .current_dir(&directory);
  let output = workload_environment(&mut command, scratch)
    .stdin(Stdio::null())
    .output()
    .map_err(|e| Failure(format!("running the {} workload: {e}", workload.name)))?;
  Ok(slates_conformance::workload::Run {
    directory,
    exit_code: output.status.code().unwrap_or(-1),
    output: String::from_utf8_lossy(&output.stdout).into_owned(),
    manifest: manifest_of(dir)?,
  })
}

/// One roster entry run on the host and over the mount and judged under the route's stated differences (A-300), or
/// skipped naming its absent tool. A difference, a failure or a stated difference is printed and noted for the record.
fn run_one(
  run: &Run<'_>,
  workload: &Workload,
  (host_root, mount_root): (&Path, &Path),
  links: HardLinks,
  notes: &mut Vec<String>,
) -> Result<WorkloadResult, Failure> {
  if !tool_on_path(workload.tool) {
    return Ok(WorkloadResult {
      name: workload.name.to_owned(),
      status: WorkloadStatus::Skipped {
        tool: workload.tool.to_owned(),
      },
    });
  }
  let host = execute(workload, &host_root.join(workload.name), run.scratch.path())?;
  let mount = execute(
    workload,
    &mount_root.join(workload.name),
    run.scratch.path(),
  )?;
  let status = compare_with(
    workload,
    &host,
    &mount,
    stated_differences(run.transport, links),
  );
  match status
    .failure_note(workload.name)
    .or_else(|| status.stated_note(workload.name))
  {
    Some(note) => {
      println!("workloads: {note}");
      notes.push(note);
    }
    None => println!("workloads: {}: {status:?}", workload.name),
  }
  Ok(WorkloadResult {
    name: workload.name.to_owned(),
    status,
  })
}

/// Format: the file the harness gives a second name in the mount's workload root, and that name.
const LINK_PROBE: (&str, &str) = (".hard-link-probe", ".hard-link-probe-second-name");

/// Whether the mount makes a hard link (A-498): a file made in the mount's workload root (`root`) given a second name,
/// both names removed after; a refusal is printed with its error. This is the route's own answer, never assumed of it:
/// one WinFsp transport covers a drive whose driver links (`docs/wip/winfsp-patches`, A-360) and one whose driver
/// refuses, as WinFsp's as released does (`ERROR_NOT_SUPPORTED`).
#[allow(
  clippy::disallowed_methods,
  reason = "the probe writes only inside the workload root it is handed, where the git and Mercurial workloads themselves write: the mount under test or the harness's own scratch reference, never a host path of its choosing"
)]
fn mount_links(root: &Path) -> Result<HardLinks, Failure> {
  let (first, second) = (root.join(LINK_PROBE.0), root.join(LINK_PROBE.1));
  let remove = |path: &Path| {
    std::fs::remove_file(path)
      .map_err(|error| Failure(format!("remove {}: {error}", path.display())))
  };
  std::fs::write(&first, b"one file, two names\n")
    .map_err(|error| Failure(format!("make {}: {error}", first.display())))?;
  let links = match std::fs::hard_link(&first, &second) {
    Ok(()) => {
      remove(second.as_path())?;
      HardLinks::Made
    }
    Err(error) => {
      println!("workloads: the mount refused a hard link: {error}");
      HardLinks::Refused
    }
  };
  remove(first.as_path())?;
  Ok(links)
}

/// The workload suite over the mount.
pub(crate) fn run_workloads(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let host_root = run.scratch.fresh("workloads-host")?;
  let fold = folds_names(&host_root);
  let session = Session::open(run, "workloads", super::VOLUME_SIZE, fold, None)?;
  let mount_root = session.workdir("workloads")?;
  let links = mount_links(&mount_root)?;
  let mut tools = Vec::new();
  let mut notes = vec![format!(
    "the volume was created with --fold={fold} to match the host scratch's name policy; git identity and dates fixed; \
     sqlite busy timeout {SQLITE_BUSY_MS} ms; watcher wait {WATCH_SECONDS} s"
  )];
  notes.push(format!(
    "the mount {} a hard link (one file given a second name in its workload root before the workloads ran)",
    match links {
      HardLinks::Made => "made",
      HardLinks::Refused => "refused",
    }
  ));
  notes.extend(owed_on_windows_note(run.os));
  for workload in ROSTER.iter().filter(|workload| runs_here(workload, run.os)) {
    tools.push(run_one(
      run,
      workload,
      (&host_root, &mount_root),
      links,
      &mut notes,
    )?);
  }
  notes.extend(session.size_note.clone());
  drop(session);
  let ok = !tools.iter().any(|t| t.status.fails_the_suite());
  let run_count = tools
    .iter()
    .filter(|t| !matches!(t.status, WorkloadStatus::Skipped { .. }))
    .count();
  let counts = Counts::Workloads { tools };
  Ok(SuiteResult {
    privilege: super::suites::this_user().privilege(),
    outcome: match run.adapter(Suite::Workloads) {
      Some((adapter, not_covered)) => slates_conformance::Outcome::Limited {
        adapter,
        not_covered,
        counts,
      },
      None => slates_conformance::Outcome::Ran { counts },
    },
    command: "sh -c '<roster script>' in <host scratch>/<tool> and <mount>/<tool>, per tool of crates/conformance/src/workload.rs ROSTER; trees compared by manifest".to_owned(),
    bound: format!("{run_count} tools run, one script each"),
    expected_failure_list: None,
    notes,
    ok,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_conformance::workload::compare;

  /// AC-3.2 / T-3.3 (2026-11-07). Do: commit through the workload environment from a parent environment that configures
  /// signing, as kiln's agent sessions do (`GIT_CONFIG_COUNT` with `commit.gpgsign` and a signing program, here one that
  /// always fails, and `GIT_CONFIG_PARAMETERS`). Expect the commit made, unsigned. Before, the parent's configuration
  /// reached the workload's git: on this Mac its commits were x509-signed by kiln's signer, each signature carrying its
  /// signing time, so two runs a second apart differed in every commit's hash over one tree, and
  /// `a_host_reference_run_over_a_kept_scratch_starts_from_an_empty_tree` failed so at load ~14 (bug-sweep).
  #[test]
  fn the_workload_environment_keeps_a_parents_git_configuration_out() {
    if !tool_on_path("git") {
      eprintln!("skipping the inherited git configuration: git is absent");
      return;
    }
    let path =
      crate::conformance::build_output(&format!("slates-git-env-{}", std::process::id())).unwrap();
    let scratch = super::super::Scratch { path, keep: false };
    let mut command = Command::new("sh");
    command
      .args([
        "-ec",
        "git init -q . && printf x > f && git add f && git commit -q -m one && git cat-file -p HEAD",
      ])
      .current_dir(scratch.path())
      // What the parent passes, set before the workload's environment as an inherited variable is.
      .env("GIT_CONFIG_COUNT", "2")
      .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
      .env("GIT_CONFIG_VALUE_0", "true")
      .env("GIT_CONFIG_KEY_1", "gpg.program")
      .env("GIT_CONFIG_VALUE_1", "false")
      .env(GIT_CONFIG_PARAMETERS, "'commit.gpgsign'='true'");
    let output = workload_environment(&mut command, scratch.path())
      .stdin(Stdio::null())
      .output()
      .unwrap();
    let said = format!(
      "{}{}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
    assert!(
      output.status.success() && said.contains("author slates") && !said.contains("gpgsig"),
      "the workload's commit was made unsigned: {said}"
    );
  }

  /// AC-3.2 / T-3.3 (A-289's sweep; CI run 44247840584, macOS, 2026-11-18). Do: run the roster's cargo workload through
  /// the workload environment from a parent environment that colours cargo's output (`CARGO_TERM_COLOR=always`, which
  /// every CI job sets) and builds elsewhere (`CARGO_TARGET_DIR` outside the tree, which kiln's runs set). Expect: exit
  /// 0, the built binary run, and the second build counted `Fresh` once. Before, CI's colour put an escape sequence
  /// before `Fresh`, so the script's count read 0 and it exited 1 on both sides, which was recorded `Identical` until
  /// A-289 failed a workload failing alike: the reuse AC-3.2 names had never been checked in CI. A parent's target
  /// directory left `./target/debug/mini` unbuilt.
  #[test]
  fn the_workload_environment_keeps_a_parents_cargo_settings_out() {
    if !tool_on_path("cargo") {
      eprintln!("skipping the inherited cargo settings: cargo is absent");
      return;
    }
    let path =
      crate::conformance::build_output(&format!("slates-cargo-env-{}", std::process::id()))
        .unwrap();
    let scratch = super::super::Scratch { path, keep: false };
    let workload = ROSTER
      .iter()
      .find(|workload| workload.name == "cargo")
      .unwrap();
    let mut command = Command::new("sh");
    command
      .arg("-c")
      .arg(shell_script(workload))
      .current_dir(scratch.path())
      // What the parent passes, set before the workload's environment as an inherited variable is.
      .env("CARGO_TERM_COLOR", "always")
      .env("CARGO_TARGET_DIR", scratch.path().join("elsewhere"));
    let output = workload_environment(&mut command, scratch.path())
      .stdin(Stdio::null())
      .output()
      .unwrap();
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(
      output.status.success() && said.ends_with("mini says hello with 1 args\n1\n"),
      "the cargo workload built its own target, ran it, and found its second build fresh: exit {:?}, output {said:?}",
      output.status.code()
    );
  }

  /// AC-3.2 / T-3.3: do the host reference run of the real git workload twice over one kept scratch (what
  /// CI does when its cache restores the previous run's `--scratch` tree); expect the second run to be the
  /// first run again — exit 0, and identical by the suite's own comparison. On 2026-09-30 the second run found the first's
  /// repository, exited 1, and the git and rsync workloads differed from the mount on both hosts.
  #[test]
  fn a_host_reference_run_over_a_kept_scratch_starts_from_an_empty_tree() {
    if !tool_on_path("git") {
      eprintln!("skipping the kept-scratch rerun: git is absent");
      return;
    }
    let path =
      crate::conformance::build_output(&format!("slates-rerun-{}", std::process::id())).unwrap();
    let scratch = super::super::Scratch { path, keep: false };
    let workload = ROSTER
      .iter()
      .find(|workload| workload.name == "git")
      .unwrap();
    let mut runs = Vec::new();
    for _ in 0..2 {
      let host_root = scratch.fresh("workloads-host").unwrap();
      runs.push(execute(workload, &host_root.join(workload.name), scratch.path()).unwrap());
    }
    let (first, second) = (&runs[0], &runs[1]);
    assert_eq!(first.exit_code, 0, "first run: {}", first.output);
    assert_eq!(second.exit_code, 0, "rerun: {}", second.output);
    // Judged as the suite judges host against mount (the git index records stat times, so the reviewed
    // policy leaves it out).
    assert_eq!(compare(workload, first, second), WorkloadStatus::Identical);
  }

  /// The goal of 2026-11-18 (git, Mercurial and Subversion byte-identical on every route), its determinism gate. Do: run
  /// each version-control workload of the roster twice on the host, in two directories of different names (as the host
  /// run and the mounted run are) and a clock second or more apart. Expect: identical by the suite's own comparison —
  /// same exit code 0, same output once each directory is the root token, same tree under the roster's exclusions — so
  /// a difference over a mount is the mount's, never the tool's own nondeterminism or an absolute path the comparison
  /// missed. Each absent tool skips loudly. Measured on this rule's first run (2026-11-17, macOS 26, git 2.50.1,
  /// Mercurial 7.2): two runs of the Mercurial workload wrote its store's file list in two orders until the shared
  /// environment fixed Python's hash seed, and a local clone's `undo.desc` and reflog name its source by absolute
  /// path, which the roster now excludes with the reason.
  #[test]
  fn the_version_control_workloads_run_byte_identically_twice_on_the_host() {
    let path =
      crate::conformance::build_output(&format!("slates-vcs-twice-{}", std::process::id()))
        .unwrap();
    let scratch = super::super::Scratch { path, keep: false };
    for name in ["git", "git-history", "mercurial", "subversion"] {
      let workload = ROSTER
        .iter()
        .find(|workload| workload.name == name)
        .unwrap();
      if !tool_on_path(workload.tool) {
        eprintln!(
          "SKIP: the {name} workload twice on the host: {} is absent",
          workload.tool
        );
        continue;
      }
      let first_root = scratch.fresh(&format!("{name}-first")).unwrap();
      let first = execute(workload, &first_root.join(name), scratch.path()).unwrap();
      // A clock second between the runs, so anything a workload stamps with the time differs between them.
      #[allow(clippy::disallowed_methods)]
      // the test thread waits out a whole second, no runtime involved
      std::thread::sleep(std::time::Duration::from_millis(1_100));
      let second_root = scratch.fresh(&format!("{name}-second-run")).unwrap();
      let second = execute(workload, &second_root.join(name), scratch.path()).unwrap();
      assert_eq!(first.exit_code, 0, "{name}, first run: {}", first.output);
      assert_eq!(
        compare(workload, &first, &second),
        WorkloadStatus::Identical,
        "{name}"
      );
      eprintln!(
        "the {name} workload ran twice identically: {} entries, {} output lines",
        first.manifest.entries.len(),
        first.output.lines().count()
      );
    }
  }

  /// A run's output as a clone that copies prints it: each line of the workload's stated link counts with its count of
  /// files with two names as 0, the rest of the line kept (`5 of 5` becomes `0 of 5`, as on the WinFsp drive).
  fn as_a_copying_clone_prints(output: &str, workload: &str) -> String {
    let starts: Vec<&str> = stated_differences(
      slates_conformance::Transport::NativeWindowsWinfsp,
      HardLinks::Refused,
    )
    .iter()
    .filter(|entry| entry.workload == workload)
    .map(|entry| entry.line_start)
    .collect();
    output
      .lines()
      .map(|line| {
        match starts
          .iter()
          .find_map(|start| line.strip_prefix(start).map(|rest| (start, rest)))
        {
          Some((start, rest)) => format!(
            "{start}0{}",
            rest.trim_start_matches(|c: char| c.is_ascii_digit())
          ),
          None => line.to_owned(),
        }
      })
      .map(|line| line + "\n")
      .collect()
  }

  /// A-300 (the WinFsp drive's stated difference). Do: run the real git history and Mercurial workloads on this host,
  /// whose clones link, and judge each against the same run as a clone that copies prints it (the drive's: every
  /// count of files with two names 0), under the drive's stated differences; then against itself. Expect the first
  /// identical but for exactly the stated lines (one of git's, two of Mercurial's, each with the host's nonzero count),
  /// a difference judged on a route that states nothing, and the second refused because no stated line differed, so
  /// each stated start names a line these real scripts print and nothing else. Each absent tool skips loudly.
  #[test]
  fn the_drives_stated_differences_name_the_lines_the_real_clones_print() {
    let path =
      crate::conformance::build_output(&format!("slates-vcs-stated-{}", std::process::id()))
        .unwrap();
    let scratch = super::super::Scratch { path, keep: false };
    let stated = stated_differences(
      slates_conformance::Transport::NativeWindowsWinfsp,
      HardLinks::Refused,
    );
    for (name, stated_lines) in [("git-history", 1), ("mercurial", 2)] {
      let workload = ROSTER
        .iter()
        .find(|workload| workload.name == name)
        .unwrap();
      if !tool_on_path(workload.tool) {
        eprintln!(
          "SKIP: the {name} workload's stated difference: {} is absent",
          workload.tool
        );
        continue;
      }
      let root = scratch.fresh(&format!("{name}-stated")).unwrap();
      let host = execute(workload, &root.join(name), scratch.path()).unwrap();
      assert_eq!(host.exit_code, 0, "{name}: {}", host.output);
      let mut drive = host.clone();
      drive.output = as_a_copying_clone_prints(&host.output, name);
      match compare_with(workload, &host, &drive, stated) {
        WorkloadStatus::Stated { lines } => {
          assert_eq!(lines.len(), stated_lines, "{name}: {lines:?}");
          eprintln!("{name} on the drive's route: {lines:?}");
        }
        other => panic!("{name}: expected the drive's stated difference, got {other:?}"),
      }
      assert!(compare_with(workload, &host, &drive, &[]).fails_the_suite());
      match compare_with(workload, &host, &host, stated) {
        WorkloadStatus::Differs { detail } => assert!(
          detail.starts_with("a stated difference no longer differs"),
          "{name}: {detail}"
        ),
        other => panic!("{name}: expected the unmet stated line refused, got {other:?}"),
      }
    }
  }

  /// A-300. Do: ask which roster entries each lane runs. Expect the Windows lane to run git, git's history, Mercurial
  /// and Subversion and to name every other workload once as owed a Windows form; macOS and Linux to run the whole
  /// roster, each with its own watcher, and to carry no such note.
  #[test]
  fn the_windows_lane_runs_the_version_control_workloads() {
    let lane = |os| -> Vec<(&'static str, &'static str)> {
      ROSTER
        .iter()
        .filter(|workload| runs_here(workload, os))
        .map(|workload| (workload.name, workload.tool))
        .collect()
    };
    assert_eq!(
      lane(HostOs::Windows),
      [
        ("git", "git"),
        ("git-history", "git"),
        ("mercurial", "hg"),
        ("subversion", "svn")
      ]
    );
    let note = owed_on_windows_note(HostOs::Windows).unwrap();
    for owed in ["cargo", "npm", "python", "rg", "rsync", "sqlite", "editor"] {
      assert!(note.contains(owed), "{note}");
    }
    assert_eq!(note.matches("watcher").count(), 1, "{note}");
    for (os, own, foreign) in [
      (HostOs::Macos, "fswatch", "inotifywait"),
      (HostOs::Linux, "inotifywait", "fswatch"),
    ] {
      let runs = lane(os);
      assert_eq!(runs.len(), ROSTER.len() - 1, "{os:?}");
      assert!(runs.contains(&("watcher", own)) && !runs.contains(&("watcher", foreign)));
      assert!(owed_on_windows_note(os).is_none());
    }
  }

  /// AC-3.2 / T-3.3: run the real editor save workload in ordinary and TMPDIR-matching paths.
  /// Both must retain the original bytes in the backup and write the edited bytes to the new file.
  /// The scratch is in the build output (A-50); no mount, installation or privilege is performed here.
  #[test]
  fn an_editor_save_preserves_its_backup_even_inside_tmpdir() {
    if !tool_on_path("vim") {
      eprintln!("skipping real Vim save history: vim is absent");
      return;
    }
    let path =
      crate::conformance::build_output(&format!("slates-editor-{}", std::process::id())).unwrap();
    let scratch = super::super::Scratch { path, keep: false };
    let ordinary = scratch.path().join("ordinary");
    let temporary = scratch.path().join("temporary");
    let workload = ROSTER
      .iter()
      .find(|workload| workload.name == "editor")
      .unwrap();
    for directory in [&ordinary, &temporary] {
      create_dir(directory).unwrap();
      let output = Command::new("sh")
        .args(["-ec", workload.script])
        .current_dir(directory)
        .env("TMPDIR", &temporary)
        .env_remove("TMP")
        .env_remove("TEMP")
        .stdin(Stdio::null())
        .output()
        .unwrap();
      assert!(
        output.status.success(),
        "editor failed: {}",
        String::from_utf8_lossy(&output.stderr)
      );
      let edited = std::fs::read(directory.join("note.txt")).unwrap();
      let backup = std::fs::read(directory.join("note.txt~"));
      eprintln!(
        "editor save in {}: output={:?}, backup={backup:?}",
        directory.display(),
        String::from_utf8_lossy(&output.stdout)
      );
      assert_eq!(edited, b"hello draft\n");
      assert_eq!(
        backup.unwrap(),
        b"draft\n",
        "the rename-save preserves the previous bytes"
      );
    }
  }
}
