//! A suite run inside a real container through the OCI attachment (§4.6 A-9; AUD-29-78: "add §13's by-use
//! cases, including the exact OCI mount entry", "pin tested runtime profiles"). The lane is the macOS host's
//! NFS mount handed to a container the way a harness would hand it:
//! - the session's host mount (the native lane's daemon, volume and `mount_nfs`);
//! - the runtime handshake, `slates oci-runtime docker`, whose profile line the record keeps — a profile
//!   without evidence fails the lane, it is never recorded as a pass;
//! - `attach --oci`, then `slates oci-check` of the returned source immediately before the bind;
//! - one `docker run` of the returned entry as Docker's `--mount` (non-recursive, private, read-only when
//!   the entry says `ro`), as the mounting user, in which the suite is compiled from the pinned source and
//!   run inside the bind.
//!
//! The suite's own verdict judges the run, as on the host lane; the record names the container command and
//! the profile, so a pass is a pass for that profile only.

use std::path::Path;
use std::process::{Command, Stdio};

use slates_conformance::capability::HostOs;
use slates_conformance::exerciser::{judge_fsstress, judge_fsx};
use slates_conformance::record::{Counts, Outcome, WorkloadResult, WorkloadStatus};
use slates_conformance::workload::{
  ENV_SQLITE_BUSY_MS, ENV_WATCH_SECONDS, ROSTER, Workload, compare,
};

use super::fetch;
use super::slates::Session;
use super::suites::this_user;
use super::{Run, SuiteResult};
use crate::Failure;

/// Format: the runtime CLI the lane binds with — the one profile the handshake holds evidence for.
const RUNTIME: &str = "docker";
/// Format: the image the suite is compiled and run in: the toolchain image the Linux lanes already use
/// (`rust:1.98.0`, Debian with `cc`), so the lane installs nothing.
const IMAGE: &str = "rust:1.98.0";
/// Format: where the container sees the volume, and where it sees the pinned sources (read-only).
const DESTINATION: &str = "/work";
const SOURCES: &str = "/src";

/// The binding `attach --oci` returned: the entry's source, its options, and the evidence the source
/// check needs.
struct Binding {
  source: String,
  read_only: bool,
  mount_id: u64,
  device: u64,
}

/// Attaches the session's volume in the container form over its host mount, and checks the source again
/// just before the bind.
fn bind(session: &Session) -> Result<Binding, Failure> {
  let mount = session.mount.path().display().to_string();
  let reply = session
    .binary
    .run(
      &session.instance,
      &[
        "attach",
        &session.volume_id,
        "--oci-source",
        &mount,
        "--oci-destination",
        DESTINATION,
        "--write",
        "--json",
      ],
    )?
    .expect_ok("attach --oci")?;
  let json = reply.json()?;
  let binding = &json["established"]["binding"];
  let entry = &binding["mount"];
  let number = |value: &serde_json::Value, what: &str| {
    value
      .as_u64()
      .ok_or_else(|| Failure(format!("the binding has no {what}: {json}")))
  };
  let bound = Binding {
    source: entry["source"]
      .as_str()
      .ok_or_else(|| Failure(format!("the entry has no source: {json}")))?
      .to_owned(),
    read_only: entry["options"]
      .as_array()
      .is_some_and(|options| options.iter().any(|o| o == "ro")),
    mount_id: number(&binding["evidence"]["mount_id"], "mount id")?,
    device: number(&binding["evidence"]["mount_device"], "mount device")?,
  };
  session
    .binary
    .run(
      &session.instance,
      &[
        "oci-check",
        &bound.source,
        &bound.mount_id.to_string(),
        &bound.device.to_string(),
      ],
    )?
    .expect_ok("oci-check before the bind")?;
  Ok(bound)
}

/// The runtime's profile line, or the handshake's refusal as the lane's failure.
fn handshake(session: &Session) -> Result<String, Failure> {
  let reply = session
    .binary
    .run(&session.instance, &["oci-runtime", RUNTIME])?
    .expect_ok("the runtime handshake")?;
  Ok(reply.stdout.trim().replace('\n', "; "))
}

/// The mounting user as `UID:GID`, the identity the container runs as (the tested profile's).
#[cfg(unix)]
fn mounting_user() -> Result<String, Failure> {
  Ok(format!(
    "{}:{}",
    rustix::process::getuid().as_raw(),
    rustix::process::getgid().as_raw()
  ))
}

/// The container leg runs on the macOS lane only; no other host reaches it.
#[cfg(not(unix))]
fn mounting_user() -> Result<String, Failure> {
  Err(Failure(
    "the container leg runs on the macOS lane".to_owned(),
  ))
}

/// The binding as Docker's `--mount`: non-recursive, private, read-only when the entry says `ro`.
fn binding_mount(binding: &Binding) -> String {
  let mut volume = format!(
    "type=bind,source={},destination={DESTINATION},bind-recursive=disabled,bind-propagation=private",
    binding.source
  );
  if binding.read_only {
    volume.push_str(",readonly");
  }
  volume
}

/// A host directory bound read-only (`readonly`) or writable at `destination`.
fn host_mount(source: &Path, destination: &str, readonly: bool) -> String {
  let mut mount = format!(
    "type=bind,source={},destination={destination}",
    source.display()
  );
  if readonly {
    mount.push_str(",readonly");
  }
  mount
}

/// One `docker run` as the mounting user with `mounts` and `env`; `script` runs under `sh -c`. The exit code,
/// stdout with stderr after it, and the command line.
fn docker_run(
  mounts: &[String],
  env: &[(String, String)],
  script: &str,
) -> Result<(i32, String, String), Failure> {
  let mut args = vec![
    "run".to_owned(),
    "--rm".to_owned(),
    "--user".to_owned(),
    mounting_user()?,
  ];
  for mount in mounts {
    args.extend(["--mount".to_owned(), mount.clone()]);
  }
  for (key, value) in env {
    args.extend(["-e".to_owned(), format!("{key}={value}")]);
  }
  args.extend([
    IMAGE.to_owned(),
    "sh".to_owned(),
    "-c".to_owned(),
    script.to_owned(),
  ]);
  let output = Command::new(RUNTIME)
    .args(&args)
    .stdin(Stdio::null())
    .output()
    .map_err(|e| Failure(format!("running {RUNTIME}: {e}")))?;
  let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
  text.push_str(&String::from_utf8_lossy(&output.stderr));
  let command = format!("{RUNTIME} {}", args.join(" "));
  Ok((output.status.code().unwrap_or(-1), text, command))
}

/// `docker run` of the binding with the pinned sources read-only at [`SOURCES`]: success, output, command.
fn run_in_container(
  binding: &Binding,
  sources: &Path,
  script: &str,
) -> Result<(bool, String, String), Failure> {
  let mounts = [binding_mount(binding), host_mount(sources, SOURCES, true)];
  let (code, text, command) = docker_run(&mounts, &[], script)?;
  Ok((code == 0, text, command))
}

/// `text` with the home directory written `~`, so a record names no account's home.
fn home_as_tilde(text: &str) -> String {
  match std::env::var("HOME") {
    Ok(home) if !home.is_empty() => text.replace(&home, "~"),
    _ => text.to_owned(),
  }
}

/// A container leg's setup and what its record carries: the session (host mount), the handshake's profile, the
/// checked binding.
struct Leg {
  session: Session,
  profile: String,
  binding: Binding,
}

impl Leg {
  /// Opens the session for `suite`, asks the runtime for its profile, makes the suite's working directory
  /// inside the mount, and binds the mount in the container form, checked again just before the bind. The
  /// volume folds names when `fold` (a workload compared against a name-folding reference).
  fn open(run: &Run<'_>, suite: &str, fold: bool) -> Result<Leg, Failure> {
    let session = Session::open(run, &format!("oci-{suite}"), super::VOLUME_SIZE, fold, None)?;
    let profile = handshake(&session)?;
    session.workdir(suite)?;
    let binding = bind(&session)?;
    Ok(Leg {
      session,
      profile,
      binding,
    })
  }

  /// The suite's working directory as the container sees it.
  fn workdir(suite: &str) -> String {
    format!("{DESTINATION}/conformance-{}/{suite}", std::process::id())
  }

  /// The notes every container record carries: the pinned source's note, the profile, the source check.
  fn notes(&self, source_note: String) -> Vec<String> {
    vec![
      source_note,
      format!(
        "the runtime's profile (`slates oci-runtime {RUNTIME}`): {}",
        home_as_tilde(&self.profile)
      ),
      format!(
        "the source checked again (`slates oci-check`, mount {} on device {}) just before the bind",
        self.binding.mount_id, self.binding.device
      ),
    ]
  }

  /// The record's command, reading the same on every host: the bind's source is the session's mount, the
  /// user the mounting user, the work directory the process's; the sources' path is the scratch the
  /// harness names `<scratch>` itself.
  fn recorded(&self, command: &str) -> Result<String, Failure> {
    Ok(
      command
        .replace(&self.binding.source, "<mount>")
        .replace(&mounting_user()?, "<uid>:<gid>")
        .replace(
          &format!("conformance-{}/", std::process::id()),
          "conformance-<pid>/",
        ),
    )
  }
}

/// fsx inside a container over the OCI bind.
pub(crate) fn run_fsx(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let tools = run.scratch.shared("tools")?;
  // The host build fetches and verifies the pinned source; the container compiles the same file.
  let built = fetch::build_fsx(&tools)?;
  let leg = Leg::open(run, "fsx", false)?;
  let bounds = run.bounds();
  let script = format!(
    "cc -O2 -w -include time.h -include stdint.h -o /tmp/fsx {SOURCES}/{} && cd {} \
     && /tmp/fsx -N {} -S {} -l {} -q -P /tmp fsx.bin",
    fetch::FSX_C.name,
    Leg::workdir("fsx"),
    bounds.fsx_operations,
    bounds.fsx_seed,
    bounds.fsx_file_length
  );
  let (success, output, command) = run_in_container(&leg.binding, &tools, &script)?;
  let verdict = judge_fsx(success, &output);
  let mut notes = leg.notes(built.note);
  notes.push(
    "fsx compiled inside the container from the same pinned source; its .fsxlog/.fsxgood files kept in the container's /tmp".to_owned(),
  );
  if !verdict.ok {
    notes.push(format!("fsx output tail: {}", verdict.detail));
  }
  notes.extend(leg.session.size_note.clone());
  let command = leg.recorded(&command)?;
  drop(leg);
  Ok(SuiteResult {
    privilege: this_user().privilege(),
    outcome: Outcome::Ran {
      counts: Counts::Fsx {
        operations: bounds.fsx_operations,
        seed: bounds.fsx_seed,
        file_length: bounds.fsx_file_length,
        ok: verdict.ok,
      },
    },
    command,
    bound: format!(
      "{} operations, seed {}, file length {} bytes",
      bounds.fsx_operations, bounds.fsx_seed, bounds.fsx_file_length
    ),
    expected_failure_list: None,
    notes,
    ok: verdict.ok,
  })
}

/// fsstress inside a container over the OCI bind: LTP's pinned sources staged with the Linux shim and compiled
/// inside, run with the lane's bounds; the daemon must still answer afterwards.
pub(crate) fn run_fsstress(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let sources = run.scratch.shared("tools")?.join("ltp-linux");
  fetch::stage_fsstress(&sources, HostOs::Linux)?;
  let leg = Leg::open(run, "fsstress", false)?;
  let bounds = run.bounds();
  let script = format!(
    "cp -R {SOURCES} /tmp/ltp && cd /tmp/ltp && cc {} -o /tmp/fsstress {} && /tmp/fsstress -d {} -n {} -p {} -s {} -v",
    fetch::FSSTRESS_CFLAGS.join(" "),
    fetch::FSSTRESS_C.name,
    Leg::workdir("fsstress"),
    bounds.fsstress_operations,
    bounds.fsstress_processes,
    bounds.fsstress_seed
  );
  let (success, output, command) = run_in_container(&leg.binding, &sources, &script)?;
  let verdict = judge_fsstress(success, &output);
  let alive = leg.session.daemon_alive();
  let ok = verdict.ok && alive;
  let mut notes = leg.notes(format!(
    "fsstress: {} ({}), sha256 {}, compiled inside the container `cc {}` over the harness's Linux shim config.h",
    fetch::FSSTRESS_C.upstream,
    fetch::FSSTRESS_C.license,
    fetch::FSSTRESS_C.sha256,
    fetch::FSSTRESS_CFLAGS.join(" ")
  ));
  notes.push(format!(
    "the daemon answered `volume list` after the run: {alive}"
  ));
  if !verdict.ok {
    notes.push(format!("fsstress output tail: {}", verdict.detail));
  }
  if !alive {
    notes.push(format!(
      "the daemon stopped answering during the run; anchor log tail:\n{}",
      leg.session.anchor_log_tail()
    ));
  }
  notes.extend(leg.session.size_note.clone());
  let command = leg.recorded(&command)?;
  drop(leg);
  Ok(SuiteResult {
    privilege: this_user().privilege(),
    outcome: Outcome::Ran {
      counts: Counts::Fsstress {
        operations: bounds.fsstress_operations,
        processes: bounds.fsstress_processes,
        seed: bounds.fsstress_seed,
        logged_operations: verdict.logged_operations,
        disabled_operations: Vec::new(),
        ok,
      },
    },
    command,
    bound: format!(
      "{} operations per process × {} processes, seed {}",
      bounds.fsstress_operations, bounds.fsstress_processes, bounds.fsstress_seed
    ),
    expected_failure_list: None,
    notes,
    ok,
  })
}

/// Format: where the container sees the host scratch directory the workloads' reference runs use.
const HOST_SIDE: &str = "/host";

/// The roster's tools present in the image, from one probe run (`command -v` of each).
fn tools_in_image(workloads: &[&Workload]) -> Result<Vec<&'static str>, Failure> {
  let probe: Vec<String> = workloads
    .iter()
    .map(|w| format!("command -v {} >/dev/null 2>&1 && echo {}", w.tool, w.tool))
    .collect();
  let (_, output, _) = docker_run(&[], &[], &probe.join("; "))?;
  Ok(
    workloads
      .iter()
      .map(|w| w.tool)
      .filter(|tool| output.lines().any(|line| line.trim() == *tool))
      .collect(),
  )
}

/// One workload side in a container: the script in `dir` (a path inside the container, made fresh), with the
/// suite's fixed environment; the run as the suite compares it, its manifest read on the host at `on_host`.
fn execute_side(
  mounts: &[String],
  workload: &Workload,
  dir: &str,
  on_host: &Path,
) -> Result<slates_conformance::workload::Run, Failure> {
  let mut env: Vec<(String, String)> = super::workloads::GIT_IDENTITY
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
  env.extend([
    ("npm_config_cache".to_owned(), "/tmp/npm-cache".to_owned()),
    (
      ENV_SQLITE_BUSY_MS.to_owned(),
      super::SQLITE_BUSY_MS.to_string(),
    ),
    (
      ENV_WATCH_SECONDS.to_owned(),
      super::WATCH_SECONDS.to_string(),
    ),
    ("HOME".to_owned(), "/tmp".to_owned()),
    // The profile's hard-link rule (`slates oci-runtime`: other names go stale once a linked file's first
    // name is removed, until the guest revalidates), followed by the harness on both sides: git renames its
    // objects into place instead of linking them.
    ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
    (
      "GIT_CONFIG_KEY_0".to_owned(),
      "core.createObject".to_owned(),
    ),
    ("GIT_CONFIG_VALUE_0".to_owned(), "rename".to_owned()),
  ]);
  let script = format!(
    "mkdir -p {dir} && cd {dir} && exec 2>&1 && set -e\n{}",
    workload.script
  );
  let (code, output, _) = docker_run(mounts, &env, &script)?;
  Ok(slates_conformance::workload::Run {
    directory: dir.to_owned(),
    exit_code: code,
    output,
    manifest: super::workloads::manifest_of(on_host)?,
  })
}

/// Whether `name` (a path's last component) is the macOS NFS client's silly-rename of a removed, still-open
/// file: `.nfs.` then eight hexadecimal digits, a dot, four (measured: `.nfs.200511ad.307f`).
fn is_silly_rename(name: &str) -> bool {
  /// Format: the silly-rename name's prefix and its two hexadecimal fields' widths.
  const PREFIX: &str = ".nfs.";
  const FIELDS: [usize; 2] = [8, 4];
  let Some(rest) = name.strip_prefix(PREFIX) else {
    return false;
  };
  let fields: Vec<&str> = rest.split('.').collect();
  fields.len() == FIELDS.len()
    && fields
      .iter()
      .zip(FIELDS)
      .all(|(field, width)| field.len() == width && field.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The run with the NFS client's silly-renamed names left out of its manifest, and how many there were.
fn without_silly_renames(
  mut run: slates_conformance::workload::Run,
) -> (slates_conformance::workload::Run, usize) {
  let before = run.manifest.entries.len();
  run
    .manifest
    .entries
    .retain(|entry| !is_silly_rename(entry.path.rsplit('/').next().unwrap_or(&entry.path)));
  let removed = before.saturating_sub(run.manifest.entries.len());
  (run, removed)
}

/// The workloads inside containers over the OCI bind: each roster tool the image holds runs its script once in
/// a bound host scratch directory and once in the bind, both through the runtime's file sharing, and the two
/// must be byte-identical by the suite's own comparison.
pub(crate) fn run_workloads(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let host_root = run.scratch.fresh("oci-workloads-host")?;
  let fold = super::slates::folds_names(&host_root);
  let leg = Leg::open(run, "workloads", fold)?;
  let mount_root = leg.session.workdir("workloads-run")?;
  let roster: Vec<&Workload> = ROSTER
    .iter()
    .filter(|w| !(w.name == "watcher" && w.tool == "fswatch"))
    .collect();
  let present = tools_in_image(&roster)?;
  let mounts = [
    binding_mount(&leg.binding),
    host_mount(&host_root, HOST_SIDE, false),
  ];
  let mut tools = Vec::new();
  let mut notes = leg.notes(format!(
    "the roster ran in `{IMAGE}` as the mounting user; the reference side is a host scratch directory bound \
     at {HOST_SIDE}, so both sides cross the runtime's file sharing; the volume was created with --fold={fold}; \
     git ran with core.createObject=rename on both sides, as the profile's hard-link rule asks"
  ));
  for workload in roster {
    if !present.contains(&workload.tool) {
      tools.push(WorkloadResult {
        name: workload.name.to_owned(),
        status: WorkloadStatus::Skipped {
          tool: workload.tool.to_owned(),
        },
      });
      continue;
    }
    let host = execute_side(
      &mounts,
      workload,
      &format!("{HOST_SIDE}/{}", workload.name),
      &host_root.join(workload.name),
    )?;
    let mount = execute_side(
      &mounts,
      workload,
      &format!("{}/{}", Leg::workdir("workloads-run"), workload.name),
      &mount_root.join(workload.name),
    )?;
    let (mount, silly) = without_silly_renames(mount);
    if silly > 0 {
      notes.push(format!(
        "{}: {silly} `.nfs.*` names set aside on the mount side — the NFS client's silly-renames of names removed \
         while the runtime's share held them open (the transport's declared delete-while-open rule, `SillyRenamed`)",
        workload.name
      ));
    }
    let status = compare(workload, &host, &mount);
    if let WorkloadStatus::Differs { detail } = &status {
      notes.push(format!("{}: {detail}", workload.name));
    }
    println!("workloads (container): {}: {status:?}", workload.name);
    tools.push(WorkloadResult {
      name: workload.name.to_owned(),
      status,
    });
  }
  notes.extend(leg.session.size_note.clone());
  drop(leg);
  let ok = !tools
    .iter()
    .any(|t| matches!(t.status, WorkloadStatus::Differs { .. }));
  let run_count = tools
    .iter()
    .filter(|t| !matches!(t.status, WorkloadStatus::Skipped { .. }))
    .count();
  Ok(SuiteResult {
    privilege: this_user().privilege(),
    outcome: Outcome::Ran {
      counts: Counts::Workloads { tools },
    },
    command: format!(
      "{RUNTIME} run --rm --user <uid>:<gid> --mount <the binding> --mount <host scratch>:{HOST_SIDE} {IMAGE} sh -c '<roster script>' in {HOST_SIDE}/<tool> and {DESTINATION}/conformance-<pid>/workloads-run/<tool>, per tool of crates/conformance/src/workload.rs ROSTER the image holds; trees compared by manifest"
    ),
    bound: format!("{run_count} tools run, one script each"),
    expected_failure_list: None,
    notes,
    ok,
  })
}

/// Shape: how many lines of a failed container run's output a failure carries — enough for a compiler's error.
const FAILURE_TAIL_LINES: usize = 40;

/// Format: the line the container prints before each test file's output, then the file's path.
const FILE_MARK: &str = "@@@slates-pjdfstest ";

/// Writes `configure.ac`'s checks as probe files into `dir/probes` (each `N.c`, its `config.h` line in `N.define`,
/// and `-c` in `N.flags` when it need only compile), so the container answers them with its own compiler.
fn write_probes(dir: &Path) -> Result<(), Failure> {
  let probes = dir.join("probes");
  super::create_dir(&probes)?;
  super::write_file(
    &probes.join("head.h"),
    fetch::pjdfstest_config_head().as_bytes(),
  )?;
  for (n, probe) in fetch::pjdfstest_probes().iter().enumerate() {
    super::write_file(&probes.join(format!("{n:03}.c")), probe.source.as_bytes())?;
    super::write_file(
      &probes.join(format!("{n:03}.define")),
      probe.define.as_bytes(),
    )?;
    let flags: &[u8] = if probe.compile_only { b"-c" } else { b"" };
    super::write_file(&probes.join(format!("{n:03}.flags")), flags)?;
  }
  Ok(())
}

/// The outputs of every test file, split at [`FILE_MARK`]: (the file's path under the tree, its output).
fn split_outputs(output: &str) -> Vec<(String, String)> {
  let mut files: Vec<(String, String)> = Vec::new();
  for line in output.lines() {
    if let Some(path) = line.strip_prefix(FILE_MARK) {
      files.push((path.trim().to_owned(), String::new()));
    } else if let Some((_, text)) = files.last_mut() {
      text.push_str(line);
      text.push('\n');
    }
  }
  files
}

/// pjdfstest inside a container over the OCI bind: the pinned tree, `config.h` answered by the container's own
/// compiler from the harness's probes, every test file run in a directory inside the bind as the mounting
/// user under the per-file bound, and the outputs judged as the host lane judges them.
pub(crate) fn run_pjdfstest(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let sources = run.scratch.shared("tools")?.join("pjd-linux");
  super::create_dir(&sources)?;
  let root = fetch::stage_pjdfstest(&sources)?;
  write_probes(&sources)?;
  let tree = root
    .file_name()
    .map(|name| name.to_string_lossy().into_owned())
    .ok_or_else(|| Failure("the staged pjdfstest tree has no name".to_owned()))?;
  let leg = Leg::open(run, "pjd", false)?;
  let bound = super::suites::PJDFSTEST_FILE_BOUND.as_secs();
  // A probe that fails is an answer (the check is absent), so the probe loop ends with `;`: chained with `&&`, a
  // last probe Linux lacks skipped the build and every file, silently.
  let script = format!(
    "cp -R {SOURCES}/{tree} /tmp/p && cd /tmp/p && cat {SOURCES}/probes/head.h > config.h && \
     for c in {SOURCES}/probes/*.c; do b=${{c%.c}}; cc -std=gnu17 -w $(cat $b.flags) -o /tmp/probe.out $c >/dev/null 2>&1 \
     && cat $b.define >> config.h; done; cc -O2 -w -I. -o pjdfstest pjdfstest.c && cd {} && \
     for f in $(cd /tmp/p && find tests -name '*.t' | sort); do echo '{FILE_MARK}'$f; timeout {bound} sh /tmp/p/$f 2>&1; \
     [ $? -eq 124 ] && echo 'TIMED OUT'; done",
    Leg::workdir("pjd")
  );
  let (succeeded, output, command) = run_in_container(&leg.binding, &sources, &script)?;
  let runner = this_user();
  let outputs = split_outputs(&output);
  // A run in which no file ran (the build failed inside the container) is a failure, never an empty pass.
  if outputs.is_empty() {
    let tail: Vec<&str> = output.lines().rev().take(FAILURE_TAIL_LINES).collect();
    return Err(Failure(format!(
      "no pjdfstest file ran inside the container (it {}; `{command}`); its output's tail:\n{}",
      if succeeded { "exited 0" } else { "failed" },
      tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    )));
  }
  // Every file's raw output is kept in the scratch, as the host lane keeps it, so a failure can be reviewed by
  // its own lines (`cargo xtask conformance tally --outputs`).
  let kept = run.scratch.fresh("pjdfstest-output")?;
  for (path, text) in &outputs {
    super::write_file(
      &kept.join(format!("{}.txt", path.replace('/', "__"))),
      text.as_bytes(),
    )?;
  }
  let timed_out: Vec<String> = outputs
    .iter()
    .filter(|(_, text)| text.lines().any(|line| line == "TIMED OUT"))
    .map(|(path, _)| path.clone())
    .collect();
  let parsed: Vec<_> = outputs
    .iter()
    .map(|(path, text)| slates_conformance::tap::parse_file(path, text, &runner))
    .collect();
  let alive = leg.session.daemon_alive();
  let mut notes = leg.notes(format!(
    "pjdfstest: {} at {}, compiled inside the container with config.h answered by its own compiler from the \
     harness's {} probes",
    fetch::PJDFSTEST_TARBALL.upstream,
    fetch::PJDFSTEST_COMMIT,
    fetch::pjdfstest_probes().len()
  ));
  if !alive {
    notes.push(format!(
      "the daemon stopped answering during the run; anchor log tail:\n{}",
      leg.session.anchor_log_tail()
    ));
  }
  notes.extend(leg.session.size_note.clone());
  let command = leg.recorded(&command)?;
  let files = outputs.len();
  drop(leg);
  super::suites::judged_pjdfstest(
    run,
    &parsed,
    super::suites::Ran {
      runner,
      files,
      timed_out,
      alive,
      command,
      notes,
    },
  )
}

#[cfg(test)]
mod tests {
  use super::is_silly_rename;

  /// Do: classify the silly-rename names measured through the macOS NFS client and near misses. Expect: only
  /// the exact `.nfs.<8 hex>.<4 hex>` shape is set aside.
  #[test]
  fn only_the_nfs_clients_silly_rename_shape_is_set_aside() {
    for silly in [".nfs.200511ad.307f", ".nfs.20051921.307f"] {
      assert!(is_silly_rename(silly), "{silly}");
    }
    for kept in [
      ".nfs",
      ".nfs.",
      ".nfs.200511ad",
      ".nfs.200511ad.307",
      ".nfs.20051g21.307f",
      "nfs.200511ad.307f",
      ".nfs.200511ad.307f.x",
    ] {
      assert!(!is_silly_rename(kept), "{kept}");
    }
  }
}
