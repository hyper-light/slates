//! Driving the real `slates` binary: build it, run an anchor that supervises a daemon (the
//! `AnchorProcess` discipline of `crates/cli/tests/cli.rs`: killed and reaped on drop), provision
//! a volume through the CLI, establish a real kernel mount (`slates mount` on macOS; on Linux a
//! root `mount -t nfs` by the OS client of the daemon's loopback export, the lane's recorded
//! adapter), and issue a grant the way the anchor's own children can — with the anchor handoff
//! taken from the daemon's environment (a shared-memory name on macOS; on Linux a memfd reopened
//! through `/proc/<pid>/fd` and inherited by the `slates grant` child). The daemon stays
//! unprivileged throughout (R10); only the OS mount client and the suite's own processes ever run
//! under `sudo`, and only where `sudo -n` already works.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slates_conformance::capability::HostOs;

use super::{Run, create_dir, pause, stdout_of, tool_on_path};
use crate::Failure;

/// Shape: how long the anchor and its daemon get to come up — the CLI test's twenty seconds,
/// tripled for a loaded shared runner.
const START_WAIT: Duration = Duration::from_secs(60);
/// Shape: consecutive `volume list` successes before the daemon counts as settled (the CLI test's
/// streak, which outlasts the anchor's one startup restart).
const STABLE_STREAK: u32 = 10;
/// Shape: shards for the harness's daemon, as the CLI test.
const SHARDS: &str = "2";
/// Shape: how long a mount gets to appear in the mount table.
const MOUNT_WAIT: Duration = Duration::from_secs(20);
/// Format: the anchor's stderr line naming the daemon it started.
const DAEMON_PID_MARK: &str = "daemon pid ";
/// Format: the environment variables carrying the anchor handoff (`slates_anchor::segment`).
const ENV_HANDOFF: &str = "SLATES_ANCHOR";
/// Format: the handoff object's length variable.
const ENV_HANDOFF_LEN: &str = "SLATES_ANCHOR_LEN";
/// Format: the Linux mount options for the daemon's NFSv3 loopback export: version 3 over TCP, the
/// explicit port and mount port (no rpcbind), no NLM (`nolock`; the daemon serves no lock manager),
/// a high source port, soft with a short retry so a wedged export is escapable, and the same
/// one-second attribute cache `slates mount` asks for (`crates/cli/src/mount.rs`).
const LINUX_NFS_OPTIONS: &str = "vers=3,tcp,nolock,noresvport,soft,timeo=10,retrans=2,actimeo=1";

/// The built `slates` binary.
#[derive(Clone)]
pub(crate) struct SlatesBinary {
  path: PathBuf,
}

/// A CLI reply: exit code, stdout, stderr.
pub(crate) struct Reply {
  pub(crate) code: i32,
  pub(crate) stdout: String,
  pub(crate) stderr: String,
}

impl Reply {
  /// The value of a `key: value` line.
  pub(crate) fn value_of(&self, key: &str) -> Result<String, Failure> {
    self
      .stdout
      .lines()
      .find_map(|line| line.strip_prefix(&format!("{key}: ")))
      .map(str::to_owned)
      .ok_or_else(|| {
        Failure(format!(
          "no `{key}` in the reply: {:?} / {:?}",
          self.stdout, self.stderr
        ))
      })
  }

  /// The reply if it succeeded, else a failure naming the verb and the stderr.
  pub(crate) fn expect_ok(self, what: &str) -> Result<Reply, Failure> {
    if self.code == 0 {
      Ok(self)
    } else {
      Err(Failure(format!(
        "{what} exited {}: {}",
        self.code,
        self.stderr.trim()
      )))
    }
  }

  /// The reply's stdout as JSON.
  pub(crate) fn json(&self) -> Result<serde_json::Value, Failure> {
    serde_json::from_str(self.stdout.trim())
      .map_err(|e| Failure(format!("not JSON: {e}: {}", self.stdout)))
  }
}

impl SlatesBinary {
  /// Builds `slates-cli` and locates the executable cargo reports.
  pub(crate) fn build(root: &Path) -> Result<SlatesBinary, Failure> {
    let output = Command::new(env!("CARGO"))
      .args(["build", "-p", "slates-cli", "--message-format=json"])
      .current_dir(root)
      .output()?;
    if !output.status.success() {
      return Err(Failure(format!(
        "cargo build -p slates-cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
      )));
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
      let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
        continue;
      };
      if message["reason"] == "compiler-artifact"
        && message["target"]["name"] == "slates"
        && let Some(exe) = message["executable"].as_str()
      {
        return Ok(SlatesBinary {
          path: PathBuf::from(exe),
        });
      }
    }
    Err(Failure(
      "cargo did not report the slates executable".to_owned(),
    ))
  }

  /// The executable.
  pub(crate) fn path(&self) -> &Path {
    &self.path
  }

  /// Runs a client verb against an instance.
  pub(crate) fn run(&self, instance: &str, args: &[&str]) -> Result<Reply, Failure> {
    self.run_with_env(instance, args, &[])
  }

  /// Runs a client verb with extra environment variables.
  pub(crate) fn run_with_env(
    &self,
    instance: &str,
    args: &[&str],
    env: &[(String, String)],
  ) -> Result<Reply, Failure> {
    let output = Command::new(&self.path)
      .arg("--instance")
      .arg(instance)
      .args(args)
      .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
      .output()?;
    Ok(Reply {
      code: output.status.code().unwrap_or(-1),
      stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
      stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
  }
}

/// The anchor process: killed and reaped when dropped, so a failed suite leaves no daemon behind.
pub(crate) struct Anchor {
  child: Child,
  log: PathBuf,
  instance: String,
}

/// Opens the anchor's log file for writing: the development tool's own scratch output.
fn open_log(path: &Path) -> Result<std::fs::File, Failure> {
  #[allow(clippy::disallowed_methods)]
  std::fs::File::create(path).map_err(|e| Failure(format!("creating {}: {e}", path.display())))
}

impl Anchor {
  /// Starts `slates anchor` — under `tracer` (a command prefix such as strace) when given — with
  /// its stderr in a log file under `scratch`, and waits until verbs answer.
  pub(crate) fn start(
    binary: &SlatesBinary,
    instance: &str,
    scratch: &Path,
    tracer: Option<&[String]>,
  ) -> Result<Anchor, Failure> {
    let log = scratch.join(format!("anchor-{instance}.log"));
    let log_file = open_log(&log)?;
    let mut command = match tracer {
      Some(prefix) if !prefix.is_empty() => {
        let mut command = Command::new(&prefix[0]);
        command.args(&prefix[1..]).arg(binary.path());
        command
      }
      _ => Command::new(binary.path()),
    };
    let child = command
      .args([
        "--instance",
        instance,
        "anchor",
        "--quick",
        "--shards",
        SHARDS,
      ])
      .current_dir(scratch)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::from(log_file))
      .spawn()
      .map_err(|e| Failure(format!("spawning the anchor: {e}")))?;
    let anchor = Anchor {
      child,
      log,
      instance: instance.to_owned(),
    };
    anchor.wait_ready(binary)?;
    Ok(anchor)
  }

  fn wait_ready(&self, binary: &SlatesBinary) -> Result<(), Failure> {
    let started = Instant::now();
    let mut streak = 0u32;
    loop {
      let reply = binary.run(&self.instance, &["volume", "list"])?;
      if reply.code == 0 {
        streak += 1;
        if streak >= STABLE_STREAK {
          return Ok(());
        }
      } else {
        streak = 0;
      }
      if started.elapsed() > START_WAIT {
        return Err(Failure(format!(
          "the daemon did not come up within {START_WAIT:?} (last exit {}); anchor log:\n{}",
          reply.code,
          std::fs::read_to_string(&self.log).unwrap_or_default()
        )));
      }
      pause();
    }
  }

  /// The daemon's pid: the process running `slates --instance <instance> daemon`.
  pub(crate) fn daemon_pid(&self) -> Result<u32, Failure> {
    let pattern = format!("slates --instance {} daemon", self.instance);
    let listed = stdout_of("pgrep", &["-f", &pattern]);
    listed
      .lines()
      .filter_map(|l| l.trim().parse::<u32>().ok())
      .next()
      .ok_or_else(|| {
        Failure(format!(
          "no daemon process found for `{pattern}`; anchor log names: {:?}",
          std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .find(|l| l.contains(DAEMON_PID_MARK))
        ))
      })
  }

  /// Kills and reaps the anchor (the daemon leaves with it).
  pub(crate) fn stop(mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// Provisions a bounded volume through the CLI and returns its id.
pub(crate) fn create_volume(
  binary: &SlatesBinary,
  instance: &str,
  name: &str,
  size: &str,
  fold: bool,
) -> Result<String, Failure> {
  let mut args = vec!["volume", "create", name, "--bounded", size];
  if fold {
    args.push("--fold");
  }
  binary
    .run(instance, &args)?
    .expect_ok("volume create")?
    .value_of("id")
}

/// The daemon's NFS loopback port, from `status ID --json`.
fn nfs_port(binary: &SlatesBinary, instance: &str, id: &str) -> Result<u16, Failure> {
  let status = binary
    .run(instance, &["status", id, "--json"])?
    .expect_ok("status")?
    .json()?;
  status["nfs_port"]
    .as_u64()
    .and_then(|p| u16::try_from(p).ok())
    .ok_or_else(|| Failure(format!("the daemon reports no nfs_port: {status}")))
}

/// How a mount was made, which decides how it is unmade.
enum MountMethod {
  /// `slates mount`; unmade with `umount` (as the CLI test's guard).
  Slates,
  /// A root `mount -t nfs`; unmade with `sudo umount -l`.
  LinuxRoot,
}

/// A live kernel mount: force-unmounted and its directory removed on drop.
pub(crate) struct Mount {
  path: PathBuf,
  method: MountMethod,
}

impl Mount {
  /// The mount point.
  pub(crate) fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for Mount {
  fn drop(&mut self) {
    match self.method {
      MountMethod::Slates => {
        let _ = Command::new("umount").arg(&self.path).output();
      }
      MountMethod::LinuxRoot => {
        let _ = Command::new("sudo")
          .args(["-n", "umount", "-l"])
          .arg(&self.path)
          .output();
      }
    }
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// Whether the mount table lists `path`.
fn is_mounted(path: &Path) -> bool {
  let listed = stdout_of("mount", &[]);
  listed
    .lines()
    .any(|line| line.contains(&path.display().to_string()))
}

/// A fresh user-owned mount-point directory (`mktemp -d`), canonical.
fn fresh_mount_point() -> Result<PathBuf, Failure> {
  let made = stdout_of("mktemp", &["-d", "-t", "slates-mount"]);
  if made.is_empty() {
    return Err(Failure("mktemp -d failed for the mount point".to_owned()));
  }
  std::fs::canonicalize(&made).map_err(|e| Failure(format!("mount point {made}: {e}")))
}

/// Mounts the volume: `slates mount` on macOS; the root NFS client on Linux.
pub(crate) fn mount_volume(
  run: &Run<'_>,
  binary: &SlatesBinary,
  instance: &str,
  id: &str,
  name: &str,
) -> Result<Mount, Failure> {
  let point = fresh_mount_point()?;
  let mount = match run.os {
    HostOs::Macos => {
      binary
        .run(instance, &["mount", id, &point.display().to_string()])?
        .expect_ok("slates mount")?;
      Mount {
        path: point,
        method: MountMethod::Slates,
      }
    }
    HostOs::Linux => {
      let port = nfs_port(binary, instance, id)?;
      let options = format!("{LINUX_NFS_OPTIONS},port={port},mountport={port}");
      let output = Command::new("sudo")
        .args(["-n", "mount", "-t", "nfs", "-o", &options])
        .arg(format!("localhost:/{name}"))
        .arg(&point)
        .output()?;
      if !output.status.success() {
        return Err(Failure(format!(
          "sudo mount -t nfs failed: {}",
          String::from_utf8_lossy(&output.stderr)
        )));
      }
      Mount {
        path: point,
        method: MountMethod::LinuxRoot,
      }
    }
    HostOs::Windows => return Err(Failure("the harness has no WinFsp mount step".to_owned())),
  };
  let started = Instant::now();
  while !is_mounted(&mount.path) {
    if started.elapsed() > MOUNT_WAIT {
      return Err(Failure(format!(
        "{} is not in the mount table after {MOUNT_WAIT:?}",
        mount.path.display()
      )));
    }
    pause();
  }
  Ok(mount)
}

/// Whether a directory's filesystem folds names (`probe-a` and `probe-A` are one entry), so the
/// volume can be created with the same policy (`EQUIVALENCE.md` §4).
pub(crate) fn folds_names(dir: &Path) -> bool {
  let lower = dir.join("probe-a");
  let upper = dir.join("probe-A");
  if create_dir(&lower).is_err() {
    return false;
  }
  #[allow(clippy::disallowed_methods)] // the probe directory in the harness's own scratch
  let folded = std::fs::create_dir(&upper).is_err();
  let _ = Command::new("rmdir").arg(&lower).output();
  let _ = Command::new("rmdir").arg(&upper).output();
  folded
}

/// The daemon's environment: the handoff variables the anchor gave it.
fn daemon_environment(os: HostOs, pid: u32) -> Result<Vec<(String, String)>, Failure> {
  let text = match os {
    HostOs::Macos => stdout_of("ps", &["-Eww", "-o", "command=", "-p", &pid.to_string()]),
    HostOs::Linux => std::fs::read_to_string(format!("/proc/{pid}/environ"))
      .map_err(|e| Failure(format!("/proc/{pid}/environ: {e}")))?
      .replace('\0', " "),
    HostOs::Windows => return Err(Failure("no Windows daemon environment".to_owned())),
  };
  let handoff: Vec<(String, String)> = text
    .split_whitespace()
    .filter_map(|token| token.split_once('='))
    .filter(|(key, _)| key.starts_with(ENV_HANDOFF))
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
  if handoff.iter().any(|(k, _)| k == ENV_HANDOFF)
    && handoff.iter().any(|(k, _)| k == ENV_HANDOFF_LEN)
  {
    Ok(handoff)
  } else {
    Err(Failure(format!(
      "the daemon's environment carries no anchor handoff: {text}"
    )))
  }
}

/// Issues a grant for a presented landing as the anchor's user would, and returns the grant id.
pub(crate) fn grant(
  run: &Run<'_>,
  binary: &SlatesBinary,
  instance: &str,
  daemon_pid: u32,
  landing: u64,
  manifest: &str,
) -> Result<u64, Failure> {
  let mut env = daemon_environment(run.os, daemon_pid)?;
  let _reopened = reopen_linux_handoff(run.os, daemon_pid, &mut env)?;
  let reply = binary
    .run_with_env(
      instance,
      &["grant", &landing.to_string(), manifest, "--json"],
      &env,
    )?
    .expect_ok("slates grant")?;
  reply.json()?["grant"]
    .as_u64()
    .ok_or_else(|| Failure(format!("grant reply carries no id: {}", reply.stdout)))
}

/// On Linux the handoff is a descriptor number in the daemon: reopen the same memfd through
/// `/proc/<pid>/fd/<n>`, make it inheritable, and point the child at the new number. The returned
/// file keeps the descriptor open until the grant child has been spawned.
#[cfg(target_os = "linux")]
fn reopen_linux_handoff(
  os: HostOs,
  pid: u32,
  env: &mut [(String, String)],
) -> Result<Option<std::fs::File>, Failure> {
  use std::os::fd::AsRawFd;
  if os != HostOs::Linux {
    return Ok(None);
  }
  let Some(slot) = env.iter_mut().find(|(k, _)| k == ENV_HANDOFF) else {
    return Ok(None);
  };
  let number: u32 = slot.1.parse().map_err(|_| {
    Failure(format!(
      "the Linux handoff is not a descriptor number: {}",
      slot.1
    ))
  })?;
  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(format!("/proc/{pid}/fd/{number}"))
    .map_err(|e| Failure(format!("reopening the anchor handoff of pid {pid}: {e}")))?;
  rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())
    .map_err(|e| Failure(format!("making the handoff inheritable: {e}")))?;
  slot.1 = file.as_raw_fd().to_string();
  Ok(Some(file))
}

/// On macOS the handoff is a name any same-user process can attach; nothing to reopen.
#[cfg(not(target_os = "linux"))]
fn reopen_linux_handoff(
  _os: HostOs,
  _pid: u32,
  _env: &mut [(String, String)],
) -> Result<Option<std::fs::File>, Failure> {
  Ok(None)
}

/// A suite's session: the daemon, a volume and its live mount, torn down in the right order
/// (the mount first, then the anchor — the field order).
pub(crate) struct Session {
  pub(crate) binary: SlatesBinary,
  pub(crate) instance: String,
  pub(crate) volume_id: String,
  pub(crate) mount: Mount,
  pub(crate) anchor: Anchor,
  base: PathBuf,
}

impl Session {
  /// Builds the binary, starts the anchor (under `tracer` when given), provisions a volume named
  /// after the suite and mounts it.
  pub(crate) fn open(
    run: &Run<'_>,
    suite: &str,
    fold: bool,
    tracer: Option<&[String]>,
  ) -> Result<Session, Failure> {
    if !tool_on_path("mktemp") {
      return Err(Failure("mktemp is needed".to_owned()));
    }
    let binary = SlatesBinary::build(run.root)?;
    let instance = format!("conf-{suite}-{}", std::process::id());
    let anchor = Anchor::start(&binary, &instance, run.scratch.path(), tracer)?;
    let volume_name = format!("{suite}-vol");
    let volume_id = create_volume(&binary, &instance, &volume_name, super::VOLUME_SIZE, fold)?;
    let mount = mount_volume(run, &binary, &instance, &volume_id, &volume_name)?;
    let base = mount
      .path()
      .join(format!("conformance-{}", std::process::id()));
    create_dir(&base)?;
    Ok(Session {
      binary,
      instance,
      volume_id,
      mount,
      anchor,
      base,
    })
  }

  /// A fresh working directory inside the mount.
  pub(crate) fn workdir(&self, name: &str) -> Result<PathBuf, Failure> {
    let path = self.base.join(name);
    create_dir(&path)?;
    Ok(path)
  }

  /// Whether the daemon still answers.
  pub(crate) fn daemon_alive(&self) -> bool {
    self
      .binary
      .run(&self.instance, &["volume", "list"])
      .is_ok_and(|r| r.code == 0)
  }
}
