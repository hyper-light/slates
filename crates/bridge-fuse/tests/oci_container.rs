//! T-4.13 over the Linux host mount (§4.6 A-9 "A host OCI runtime passes the established host
//! attachment into the container mount namespace"; AC-4.11; RQ-20) — the CI Linux lane's variant of
//! `crates/cli/tests/cli.rs::an_oci_container_consumes_the_host_attachment_through_the_runtime_bind`.
//! The daemon does not serve `/dev/fuse` yet, so the host attachment here is a real FUSE mount of a
//! scratch volume served in-process by this crate's `fusermount3` launcher and blocking serve loop
//! (no privilege, R10); the daemon-side record and report of the form are proven by
//! `crates/server/tests/attach_forms.rs` and the macOS lane. What this lane proves is the bind leg:
//! a real container, started by the host's OCI runtime with the bind entry slates returns
//! (`type: bind`, `rbind` + `rw`/`ro`), runs the same filesystem workload as the host over a real
//! slates FUSE mount, and the two views agree byte for byte; an edit inside the container is the
//! host's, a delete on the host is the container's, and the read-only bind refuses a write.
//!
//! Gated, skipping loudly: without `fusermount3`, without a reachable `docker`, or where
//! `fusermount3` refuses `allow_other` — the runtime's daemon and the container's processes are other
//! users to the FUSE mount, which admits only the mounting uid unless `/etc/fuse.conf` sets
//! `user_allow_other` (§4.6 "`allow_other` only if `user_allow_other` is set and the operator
//! asked"; this test is the operator asking). The scratch directory is RAM-backed (`/dev/shm`),
//! named with the process id, and removed at the end.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slates_bridge_core::{Attachments, Rights, View};
use slates_bridge_fuse::channel::serve_blocking;
use slates_bridge_fuse::mount::{MountError, mount};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};
mod common;
use common::{store, volume_for_owner};

/// Shape: how long one container run may take on a loaded runner.
const CONTAINER_WAIT: Duration = Duration::from_secs(300);
/// Shape: how long `docker info` may take before the runtime is called unreachable.
const DOCKER_INFO_WAIT: Duration = Duration::from_secs(60);
/// Shape: the pause between polls of a running command.
const POLL_MS: u64 = 20;
/// Format: the image the container workload runs in — one small image.
const CONTAINER_IMAGE: &str = "alpine:3.20";
/// Format: the RAM-backed scratch root every Linux lane uses.
const RAM_ROOT: &str = "/dev/shm";
/// Format: the bind entry's vocabulary as `crates/bridge-oci/src/binding.rs` builds it (the OCI
/// runtime specification's bind mount): the entry a daemon returns for a write and a read attachment.
const OPTIONS_RW: &str = "rw";
const OPTIONS_RO: &str = "ro";
/// Format: the workload every side runs (the twin of the macOS lane's): `$1` the root of the view,
/// `$2` the side's tag — create, write, read back, mkdir, rename, delete, list, read the other side's
/// file, print every size.
const WORKLOAD: &str = r#"
set -e
R="$1"; T="$2"
printf 'hello from %s' "$T" > "$R/$T.txt"
cat "$R/$T.txt"; echo
mkdir "$R/dir-$T"
printf 'inner' > "$R/dir-$T/inner.txt"
mv "$R/dir-$T/inner.txt" "$R/dir-$T/renamed.txt"
cat "$R/dir-$T/renamed.txt"; echo
rm "$R/dir-$T/renamed.txt"
echo "--- dir-after-delete"
ls -1A "$R/dir-$T" || true
rmdir "$R/dir-$T" 2>&1 && echo "rmdir ok"
echo "--- listing"
ls -1 "$R" || true
echo "--- other"
for f in "$R"/*.txt; do
  case "$f" in *"/$T.txt") ;; *) printf '%s:' "$(basename "$f")"; cat "$f"; echo ;; esac
done
echo "--- sizes"
for f in "$R"/*.txt; do printf '%s %s\n' "$(basename "$f")" "$(wc -c < "$f" | tr -d ' ')"; done
"#;
/// Format: the read-only side's probe: list, then try a write the runtime's `ro` bind refuses.
const READ_ONLY_PROBE: &str = r#"
R="$1"
echo "--- listing"
ls -1 "$R" || true
echo "--- write"
( printf 'x' > "$R/from-ro.txt" ) 2>&1 || echo "write refused"
"#;

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

/// Runs a command to completion within `wait`, killing it past the bound.
fn bounded(command: &mut Command, wait: Duration) -> Result<(i32, String, String), String> {
  let mut child = command
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("spawn: {e}"))?;
  let started = Instant::now();
  loop {
    match child.try_wait() {
      Ok(Some(_)) => break,
      Ok(None) if started.elapsed() < wait => pause(),
      Ok(None) => {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("timed out after {wait:?}"));
      }
      Err(e) => return Err(format!("wait: {e}")),
    }
  }
  let output = child
    .wait_with_output()
    .map_err(|e| format!("output: {e}"))?;
  Ok((
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  ))
}

fn command_available(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

fn docker_server() -> Result<String, String> {
  let (code, out, err) = bounded(
    Command::new("docker").args([
      "info",
      "--format",
      "{{.ServerVersion}} {{.OperatingSystem}} runtime={{.DefaultRuntime}}",
    ]),
    DOCKER_INFO_WAIT,
  )?;
  if code == 0 {
    Ok(out.trim().to_owned())
  } else {
    Err(format!("docker info exited {code}: {}", err.trim()))
  }
}

/// A RAM-backed scratch directory named with the pid, unmounted (lazily, in case the loop is still
/// draining) and removed on drop so a failed assertion leaves nothing behind.
struct Scratch {
  root: String,
  mount_point: String,
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = Command::new("fusermount3")
      .args(["-u", "-z", &self.mount_point])
      .output();
    let _ = Command::new("rm").args(["-rf", &self.root]).output();
  }
}

fn scratch() -> Scratch {
  let root = format!("{RAM_ROOT}/slates-oci-{}", std::process::id());
  let mount_point = format!("{root}/mnt");
  let made = Command::new("mkdir")
    .args(["-p", &mount_point])
    .status()
    .unwrap();
  assert!(made.success(), "mkdir -p {mount_point}");
  Scratch { root, mount_point }
}

/// Runs `script` in a container over the bind `source:/work:<access>` as the mounting user (the
/// consumer, §4.13) — the entry as the daemon returns it, handed to the runtime.
fn run_in_container(
  source: &str,
  access: &str,
  script: &str,
  tag: &str,
) -> Result<(i32, String, String), String> {
  let user = format!(
    "{}:{}",
    rustix::process::getuid().as_raw(),
    rustix::process::getgid().as_raw()
  );
  let name = format!("slates-oci-fuse-{}-{tag}", std::process::id());
  let bind = format!("{source}:/work:{access}");
  let mut command = Command::new("docker");
  command.args([
    "run",
    "--rm",
    "--name",
    &name,
    "--user",
    &user,
    "-v",
    &bind,
    CONTAINER_IMAGE,
    "sh",
    "-c",
    script,
    "sh",
    "/work",
    tag,
  ]);
  let result = bounded(&mut command, CONTAINER_WAIT);
  if result.is_err() {
    let _ = Command::new("docker").args(["rm", "-f", &name]).output();
  }
  result
}

/// The lines of one `--- section` of a workload's output.
fn section<'a>(output: &'a str, name: &str) -> Vec<&'a str> {
  let header = format!("--- {name}");
  output
    .lines()
    .skip_while(|line| *line != header)
    .skip(1)
    .take_while(|line| !line.starts_with("--- "))
    .collect()
}

/// The two views agree: the container read the host's file byte for byte, both listings hold both
/// files and both sizes, and the container's file reads back on the host byte for byte — one copy.
fn assert_views_agree(mount_point: &str, host_out: &str, container_out: &str) {
  assert!(
    host_out.contains("hello from host") && host_out.contains("inner"),
    "the host workload: {host_out}"
  );
  assert!(
    container_out.contains("hello from container") && container_out.contains("inner"),
    "the container workload: {container_out}"
  );
  // The FUSE client unlinks at once: the directory is empty after the delete and goes away, on
  // both sides (the bind adds no open-handle cache of its own on Linux).
  for (side, out) in [("host", host_out), ("container", container_out)] {
    assert_eq!(
      section(out, "dir-after-delete"),
      vec!["rmdir ok"],
      "{side}: an open-free delete leaves nothing behind"
    );
  }
  assert_eq!(
    section(container_out, "other"),
    vec!["host.txt:hello from host"]
  );
  assert_eq!(
    section(container_out, "listing"),
    vec!["container.txt", "host.txt"]
  );
  assert_eq!(
    section(container_out, "sizes"),
    vec!["container.txt 20", "host.txt 15"]
  );
  let (code, host_view, _) = bounded(
    Command::new("sh").args([
      "-c",
      "ls -1 \"$1\"; printf '%s' \"$(cat \"$1/container.txt\")\"; echo; wc -c < \"$1/container.txt\" | tr -d ' '",
      "sh",
      mount_point,
    ]),
    CONTAINER_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0);
  assert_eq!(
    host_view.lines().collect::<Vec<_>>(),
    vec!["container.txt", "host.txt", "hello from container", "20"]
  );
}

/// T-4.13's bind leg over a real slates FUSE mount: the same workload on the host and in a container
/// started by the runtime with the daemon's bind entry, views agreeing; the host's delete seen in the
/// container; the read-only bind refusing a write.
#[test]
fn an_oci_container_consumes_a_fuse_host_mount_through_the_runtime_bind() {
  if !command_available("fusermount3") {
    eprintln!("skipping T-4.13's FUSE container leg: fusermount3 is not on this host");
    return;
  }
  let server = match docker_server() {
    Ok(server) => server,
    Err(why) => {
      eprintln!(
        "skipping T-4.13's FUSE container leg: the container runtime is unreachable: {why}"
      );
      return;
    }
  };
  eprintln!("T-4.13 (FUSE) over {server}");
  let scratch = scratch();
  let mounted = match mount(&scratch.mount_point, &["allow_other"], CONTAINER_WAIT) {
    Ok(mounted) => mounted,
    Err(MountError::Helper { exit } | MountError::NoDevice { exit }) => {
      // fusermount3 refuses `allow_other` before it opens /dev/fuse, so it hands back no descriptor
      // (`NoDevice`), or after (`Helper`); either is the environment's refusal, printed by the helper.
      eprintln!(
        "skipping T-4.13's FUSE container leg: fusermount3 refused `allow_other` or found no /dev/fuse (exit {exit:?}); the runtime's daemon and the container are other users to the mount, so /etc/fuse.conf needs `user_allow_other`"
      );
      return;
    }
    Err(other) => panic!("the FUSE mount did not come up: {other:?}"),
  };
  let uid = rustix::process::getuid().as_raw();
  let gid = rustix::process::getgid().as_raw();
  // The serve loop owns the mount and the volume (a volume is not `Send`, so it is made here); it
  // ends when the mount point is unmounted below.
  let server_thread = std::thread::spawn(move || {
    let mut mounted = mounted;
    let mut store = store();
    let mut volume = volume_for_owner(&mut store, uid, gid);
    let volume_id = VolumeId { bytes: [7; 16] };
    let mut attachments = Attachments::new();
    let attachment = attachments
      .attach(
        volume_id,
        View::Current,
        Principal::Uid { uid },
        Rights {
          read: true,
          write: true,
        },
      )
      .unwrap();
    let mut bridge = VolumeBridge::new(volume_id, &mut volume, &mut store);
    serve_blocking(
      mounted.channel(),
      &mut bridge,
      &mut attachments,
      attachment,
      None,
    )
  });

  let (code, host_out, host_err) = bounded(
    Command::new("sh").args(["-c", WORKLOAD, "sh", &scratch.mount_point, "host"]),
    CONTAINER_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0, "the host workload: {host_err}");
  let (code, container_out, container_err) =
    run_in_container(&scratch.mount_point, OPTIONS_RW, WORKLOAD, "container").unwrap();
  assert_eq!(code, 0, "the container workload: {container_err}");
  assert_views_agree(&scratch.mount_point, &host_out, &container_out);

  let (code, _, err) = bounded(
    Command::new("rm").arg(format!("{}/host.txt", scratch.mount_point)),
    CONTAINER_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0, "{err}");
  let (code, probe_out, probe_err) =
    run_in_container(&scratch.mount_point, OPTIONS_RO, READ_ONLY_PROBE, "reader").unwrap();
  assert_eq!(code, 0, "the read-only probe: {probe_err}");
  assert_eq!(section(&probe_out, "listing"), vec!["container.txt"]);
  let write = section(&probe_out, "write").join("\n");
  assert!(
    write.contains("Read-only file system") && write.contains("write refused"),
    "the runtime enforces the read-only bind: {write}"
  );

  let unmounted = Command::new("fusermount3")
    .args(["-u", &scratch.mount_point])
    .status()
    .unwrap();
  assert!(unmounted.success(), "fusermount3 -u");
  server_thread
    .join()
    .unwrap()
    .expect("the serve loop ended cleanly at unmount");
  drop(scratch);
}
