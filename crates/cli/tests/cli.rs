//! The command's tests (Phase 2 task 5; §2.5, §2.6, §4.12): `slates anchor` as a real
//! process supervising a real `slates daemon`, the verbs driven through the binary, the
//! outputs parsed back, the exit codes of the refusal taxonomy, and the daemon leaving when
//! its anchor is killed.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long the anchor and its daemon get to come up (a quick profile plus two
/// starts), and how long the daemon gets to leave after its anchor dies.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: how long a live-mount check waits for the daemon's reaper and the kernel's `UMNT` to show in
/// `status` — ten liveness budgets (`LIVENESS_BUDGET_NS`, 1 s; a client is asked about once silent for
/// one budget, so a death shows within two).
const WAIT_FOR: Duration = Duration::from_secs(10);
/// Shape: the pause between polls of a starting or stopping daemon.
const POLL_MS: u64 = 20;
/// Shape: shards for the test daemon.
const SHARDS: &str = "2";
/// Shape: exit code 3 — the daemon was unavailable (the client never reached it).
const EXIT_UNAVAILABLE: i32 = 3;
/// Shape: how many times a `--json` verb retries when it lands in the daemon's one startup-restart
/// window; a verb that got exit 3 never reached the daemon, so the retry is side-effect-free.
const RESTART_RETRIES: u32 = 50;
/// Shape: consecutive daemon responses `start_anchor` waits for, so the anchor's one startup restart
/// (a heartbeat-lapse recovery) has settled before a test runs its verbs.
const STABLE_STREAK: u32 = 10;

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a client verb; (exit code, stdout, stderr).
fn run(instance: &str, args: &[&str]) -> (i32, String, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn value_of(text: &str, key: &str) -> String {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .to_owned()
}

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

/// The anchor process: killed and reaped when dropped, so a failed assertion leaves nothing
/// behind.
struct AnchorProcess {
  child: Child,
  drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AnchorProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    if let Some(drain) = self.drain.take() {
      let _ = drain.join();
    }
  }
}

/// Starts the anchor and waits until a verb is answered.
fn start_anchor(instance: &str) -> AnchorProcess {
  start_anchor_with_environment(instance, &[])
}

fn start_anchor_with_environment(
  instance: &str,
  environment: &[(String, String)],
) -> AnchorProcess {
  let child = slates()
    .envs(environment.iter().cloned())
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  let anchor = AnchorProcess { child, drain: None };
  let started = Instant::now();
  // The anchor restarts the daemon once at startup if its first heartbeat lapses (a known liveness
  // recovery, logged as "heartbeat lapsed; killing it"). Wait for several *consecutive* successes so
  // that restart has settled before returning — otherwise a test verb lands in the restart window and
  // gets exit 3 (no daemon). A restart resets the streak.
  let mut streak = 0u32;
  loop {
    let (code, _, _) = run(instance, &["volume", "list"]);
    if code == 0 {
      streak += 1;
      if streak >= STABLE_STREAK {
        let (code, _, error) = run(instance, &["bootstrap", "root"]);
        assert_eq!(code, 0, "explicit first-time bootstrap: {error}");
        return anchor;
      }
    } else {
      streak = 0;
    }
    assert!(started.elapsed() < START_WAIT, "the daemon came up: {code}");
    pause();
  }
}

/// create prints the id and the path line; the duplicate is refused with exit 1; list
/// shows the volume.
fn create_and_list(instance: &str) -> String {
  let (code, out, err) = run(
    instance,
    &["volume", "create", "scratch", "--bounded", "4MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  assert_eq!(id.len(), 32);
  assert!(out.contains("path: (none until a bridge exists)"));
  let (code, _, err) = run(
    instance,
    &["volume", "create", "scratch", "--bounded", "4MiB"],
  );
  assert_eq!(code, 1);
  assert!(err.contains("AlreadyExists"), "{err}");
  let (code, out, _) = run(instance, &["volume", "list"]);
  assert_eq!(code, 0);
  assert!(
    out
      .lines()
      .any(|l| l.starts_with(&id) && l.contains(" scratch ")),
    "{out}"
  );
  id
}

/// snapshot, clone and stat.
fn snapshot_clone_stat(instance: &str, id: &str) {
  let (code, out, _) = run(instance, &["volume", "snapshot", id]);
  assert_eq!(code, 0);
  let snapshot = value_of(&out, "snapshot");
  let (code, out, _) = run(instance, &["volume", "clone", id, &snapshot, "copy"]);
  assert_eq!(code, 0);
  assert_ne!(value_of(&out, "id"), id);
  let (code, out, _) = run(instance, &["volume", "stat", id]);
  assert_eq!(code, 0);
  assert_eq!(value_of(&out, "name"), "scratch");
  assert_eq!(value_of(&out, "snapshots"), "1");
}

/// The register at f=0 (task 7): the head is placed on the local append, the host epoch is 1,
/// no mirror; `volume placed` awaits the region, and the mirror is refused.
fn placement_at_f0(instance: &str, id: &str) {
  let (code, out, _) = run(instance, &["volume", "stat", id]);
  assert_eq!(code, 0);
  assert_eq!(value_of(&out, "placed"), "true");
  assert_eq!(value_of(&out, "host_epoch"), "1");
  assert_eq!(value_of(&out, "mirror_age_ns"), "none");
  let (code, out, _) = run(instance, &["volume", "placed", id]);
  assert_eq!(code, 0);
  assert_eq!(value_of(&out, "placed"), "true");
  let (code, _, err) = run(instance, &["volume", "placed", id, "--mirror"]);
  assert_eq!(code, 1, "no mirror on a laptop: {err}");
  assert!(err.contains("Unsupported"), "{err}");
}

/// attach for writing, the volume's status, detach.
fn attach_status_detach(instance: &str, id: &str) {
  let (code, out, _) = run(instance, &["attach", id, "--write"]);
  assert_eq!(code, 0);
  assert_eq!(value_of(&out, "lease_epoch"), "1");
  let attachment = value_of(&out, "attachment");
  let (code, out, _) = run(instance, &["status", id]);
  assert_eq!(code, 0);
  assert_eq!(value_of(&out, "attachments"), "1");
  let (code, out, _) = run(instance, &["status", id, "--drift"]);
  assert_eq!(code, 0);
  assert!(out.is_empty(), "a scratch volume drifts nowhere: {out:?}");
  let (code, _, err) = run(instance, &["detach", &attachment]);
  assert_eq!(code, 0, "detach: {err}");
}

/// The daemon's own status: the daemon's lines and one block per shard.
fn daemon_status(instance: &str) {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "{err}");
  assert_eq!(value_of(&out, "shards"), SHARDS);
  assert!(out.contains("shard 0: clients="), "{out}");
  assert!(out.contains("shard 1 catalog.volumes: "), "{out}");
}

/// resize and destroy; then the usage refusals (exit 2) and a missing volume (exit 1).
fn resize_destroy_and_refusals(instance: &str, id: &str) {
  let (code, _, _) = run(instance, &["volume", "resize", id, "--bounded", "8MiB"]);
  assert_eq!(code, 0);
  let (code, _, _) = run(instance, &["volume", "destroy", id]);
  assert_eq!(code, 0);
  let (code, _, _) = run(instance, &["volume", "create", "nosize"]);
  assert_eq!(code, 2);
  let (code, _, _) = run(instance, &["volume", "stat", "not-an-id"]);
  assert_eq!(code, 2);
  let (code, _, err) = run(instance, &["volume", "stat", &"0".repeat(32)]);
  assert_eq!(code, 1, "{err}");
  assert!(err.contains("NotFound"), "{err}");
}

/// The anchor dies (killed, as a crash would); the daemon notices and leaves, so the
/// instance answers nobody (exit 3).
fn kill_anchor_and_wait_for_the_daemon_to_leave(instance: &str, anchor: AnchorProcess) {
  drop(anchor);
  let started = Instant::now();
  loop {
    let (code, _, _) = run(instance, &["volume", "list"]);
    if code == 3 {
      return;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "the daemon left with its anchor: still answering {code}"
    );
    pause();
  }
}

/// The whole flow through the binary: up, the verbs, the refusals, down with the anchor.
#[test]
fn the_anchor_supervises_a_daemon_the_verbs_answer_and_the_daemon_leaves_with_the_anchor() {
  // This spawns a real anchor and daemon (subprocesses with spinning shards) and a `slates`
  // process per verb. Run in parallel with the rest of the suite on a busy machine, the daemon
  // starves and a verb times out; it is reliable on its own. Like the RAM-disk landing tests
  // and the supervised-child test, it runs in a dedicated CI step (`SLATES_TEST_CLI=1`) and
  // skips loudly elsewhere, so the default `cargo test` stays green.
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the anchor+daemon CLI flow: set SLATES_TEST_CLI=1 to run it (needs the machine        to itself)"
    );
    return;
  }
  let instance = format!("cli-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let id = create_and_list(&instance);
  snapshot_clone_stat(&instance, &id);
  placement_at_f0(&instance, &id);
  attach_status_detach(&instance, &id);
  daemon_status(&instance);
  resize_destroy_and_refusals(&instance, &id);
  kill_anchor_and_wait_for_the_daemon_to_leave(&instance, anchor);
}

/// A live kernel mount point for the duration of a test: force-unmounted and removed when dropped,
/// so a failed assertion never leaves a mount or a temp directory behind (the `AnchorProcess`
/// discipline, applied to the mount). Both teardown steps are best-effort — a still-mounted path or
/// a leftover directory on a panicking test is worse than a swallowed `umount`/`rmdir` error.
struct MountPoint {
  path: String,
}

impl Drop for MountPoint {
  fn drop(&mut self) {
    // A plain unmount first; forced if something (a container runtime's share of the path, measured
    // 2026-09-14) still holds the mount point busy — a dead mount left behind is worse. On Linux every
    // slates mount is FUSE, removed without a privilege by the OS's `fusermount3` (lazily, if busy).
    let (program, plain, forced): (&str, &[&str], &[&str]) = if cfg!(target_os = "linux") {
      ("fusermount3", &["-u"], &["-u", "-z"])
    } else {
      ("umount", &[], &["-f"])
    };
    let unmounted = Command::new(program)
      .args(plain)
      .arg(&self.path)
      .output()
      .map(|o| o.status.success())
      .unwrap_or(false);
    if !unmounted {
      let _ = Command::new(program).args(forced).arg(&self.path).output();
    }
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// A fresh, user-owned directory in the build output (`CARGO_TARGET_TMPDIR`, under `target/`), named with
/// the process id and a per-process counter: a mount point, or the operator's files a test writes (A-50: a
/// test's real host directories live in the build output, never `/tmp`). Resolved to its real path so a
/// mount point matches what the kernel records in the mount table. The directory is made by `mkdir`, not
/// `std::fs::create_dir`; `std::fs::canonicalize` is a read.
fn build_output_dir(prefix: &str) -> String {
  static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let raw = format!(
    "{}/{prefix}-{}-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id(),
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
  );
  let made = Command::new("mkdir").args(["-p", &raw]).output().unwrap();
  assert!(
    made.status.success(),
    "mkdir -p {raw}: {}",
    String::from_utf8_lossy(&made.stderr)
  );
  std::fs::canonicalize(&raw)
    .unwrap()
    .to_string_lossy()
    .into_owned()
}

/// A fresh mount-point directory in the build output.
fn fresh_mount_point() -> String {
  build_output_dir("slates-mount")
}

/// Whether `mount_nfs` — the mechanism `slates mount` drives — is on this host. It is macOS and the
/// BSDs; on Linux the loopback mount is a different tool, so the live-mount flow skips there.
fn mount_nfs_available() -> bool {
  Command::new("sh")
    .args(["-c", "command -v mount_nfs"])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Whether the mount table lists a mount at `path`.
fn is_mounted(path: &str) -> bool {
  let out = Command::new("mount").output().unwrap();
  String::from_utf8_lossy(&out.stdout)
    .lines()
    .any(|line| line.contains(path))
}

/// `slates mount ID DIR` reports the path and the kernel mount table lists a real NFS mount there. A refused
/// mount fails with the daemon's status, whose refusal counts say which rule refused it (a lease refusal is
/// counted `lease.refused.superseded` or `lease.refused.unconfirmed`).
fn mount_and_check(instance: &str, id: &str, path: &str) {
  let (code, out, err) = run(instance, &["mount", id, path]);
  if code != 0 {
    let (_, status, status_err) = run(instance, &["status"]);
    let council = diagnose_fleet_council(instance);
    panic!("slates mount failed: {err}\n--- {instance} status:\n{status}{status_err}{council}");
  }
  assert_eq!(
    value_of(&out, "mounted"),
    path,
    "the command reports the path it mounted"
  );
  assert!(is_mounted(path), "the kernel mount table lists the mount");
  mount_table_hides_the_capability(path);
}

/// Shape: how long a forged `UMNT` is watched for effect: three times the daemon's one-liveness-budget
/// confirmation deadline (`crates/server/src/nfs.rs`, `UNMOUNT_CONFIRM_NS`), so a loaded box's delay
/// cannot hide an ended attachment.
#[cfg(target_os = "macos")]
const FORGED_UNMOUNT_WATCH: Duration = Duration::from_secs(3);

/// The daemon's loopback NFS port for the mount at `path`, as any local user reads it: `nfsstat -m`
/// prints each mount's `port=`. macOS only, as the forged-unmount test that reads it is.
#[cfg(target_os = "macos")]
fn nfs_port_of(path: &str) -> Option<u16> {
  let out = Command::new("nfsstat").arg("-m").output().ok()?;
  let text = String::from_utf8_lossy(&out.stdout);
  let section = text.split(path).nth(1)?;
  let (_, rest) = section.split_once("port=")?;
  rest
    .split(|c: char| !c.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// §4.6 A-34: a `UMNT` proves nothing by itself, since any local process can send one. Do send the
/// daemon a forged `UMNT /<name>` while the volume is mounted, as another user could; expect the mount's
/// attachment to survive past the daemon's confirmation deadline, and the mount to keep serving.
#[cfg(target_os = "macos")]
fn a_forged_unmount_ends_nothing(instance: &str, id: &str, name: &str, path: &str) {
  use slates_bridge_nfs::client::{Credentials, exchange, umnt_call};
  let port = nfs_port_of(path).expect("nfsstat -m names the mount's port");
  let caller = Credentials {
    uid: 0,
    gid: 0,
    gids: Vec::new(),
  };
  exchange(
    port,
    &umnt_call(1, &caller, &format!("/{name}")),
    Duration::from_secs(5),
  )
  .expect("the daemon answers the forged UMNT");
  // Watched past the daemon's confirmation deadline (one liveness budget), with room for a loaded box:
  // the attachment must hold the whole time.
  let watch_until = Instant::now() + FORGED_UNMOUNT_WATCH;
  while Instant::now() < watch_until {
    assert_eq!(
      attachments_of(instance, id),
      "1",
      "a forged UMNT of a live mount ends nothing"
    );
    pause();
  }
  assert!(is_mounted(path), "the mount is still there");
}

/// §4.6 A-34 (NFSv3 hardening): the mount table, readable by every local user, names the volume but
/// never its mount capability: on macOS the source is `slates:/<name>`, with no `@<attachment>.<token>`
/// in it (the token once rode in `mount_nfs`'s export path, visible to `mount` and to `ps`).
fn mount_table_hides_the_capability(path: &str) {
  let out = Command::new("mount").output().unwrap();
  let table = String::from_utf8_lossy(&out.stdout);
  let line = table
    .lines()
    .find(|line| line.contains(path))
    .expect("the mount is listed");
  if cfg!(target_os = "macos") {
    assert!(
      line.starts_with("slates:/"),
      "the source is the volume's name: {line}"
    );
    assert!(
      !line.contains('@'),
      "no capability in the mount table: {line}"
    );
  }
}

/// The mount's root directory is owned by the mounting user, not root:wheel
/// (docs/bugs/2026-09-14-volume-root-owned-by-root-wheel.md): `stat -f %u:%g` of the mount point is
/// this process's uid and effective gid — what git's `safe.directory` check reads, and what the NFS
/// export's POSIX permission checks judge the user's own volume by.
fn mount_root_is_owned_by_the_mounting_user(path: &str) {
  let stat = Command::new("stat")
    .args(["-f", "%u:%g", path])
    .output()
    .unwrap();
  assert!(
    stat.status.success(),
    "{}",
    String::from_utf8_lossy(&stat.stderr)
  );
  let owner = String::from_utf8_lossy(&stat.stdout).trim().to_owned();
  let me = format!(
    "{}:{}",
    rustix::process::getuid().as_raw(),
    rustix::process::getegid().as_raw()
  );
  assert_eq!(owner, me, "the volume root is owned by the mounting user");
}

/// A sparse extension through the mount survives the daemon's barrier
/// (docs/bugs/2026-09-15-recovery-image-materializes-a-sparse-files-holes.md): a file is extended to
/// 999,999,999,999,999 bytes — pjdfstest's `truncate/12.t`, which took the daemon down while the
/// recovery image materialized the hole — the size reads back through the mount, and the daemon
/// still answers `status` after the barrier that publishes the image. `perl`'s `truncate` makes the
/// extension (macOS ships no `truncate` command); a host without perl skips this step loudly.
fn a_sparse_extension_through(instance: &str, path: &str) {
  let file = format!("{path}/sparse.txt");
  let extended = Command::new("sh")
    .arg("-c")
    .arg(format!(
      "command -v perl >/dev/null || {{ echo SKIP; exit 0; }}; printf 'held' > '{file}' && \
       perl -e 'truncate($ARGV[0], 999999999999999) or die \"$!\\n\"' '{file}' && stat -f %z '{file}'"
    ))
    .output()
    .unwrap();
  assert!(
    extended.status.success(),
    "extend through the mount: {}",
    String::from_utf8_lossy(&extended.stderr)
  );
  let reported = String::from_utf8_lossy(&extended.stdout).trim().to_owned();
  if reported == "SKIP" {
    eprintln!("skipping the sparse extension step: perl is not on this host");
    return;
  }
  assert_eq!(reported, "999999999999999", "the sparse size reads back");
  let (code, _, err) = run(instance, &["status"]);
  assert_eq!(
    code, 0,
    "the daemon answers after the barrier that imaged the sparse file: {err}"
  );
}

/// A file written through the mount reads back byte for byte — the bytes travel host write → NFS →
/// the slates volume → NFS → host read. The write side avoids `std::fs` (R1) through the shell; the
/// read side is a separate `cat` process, a fresh READ across the mount rather than a page-cache echo.
fn roundtrip_a_file_through(path: &str) {
  a_hard_link_outlives_its_first_name_through(path);
  let payload = "written through a real slates kernel mount";
  let file = format!("{path}/roundtrip.txt");
  let wrote = Command::new("sh")
    .arg("-c")
    .arg(format!("printf '%s' '{payload}' > '{file}'"))
    .output()
    .unwrap();
  assert!(
    wrote.status.success(),
    "write through the mount: {}",
    String::from_utf8_lossy(&wrote.stderr)
  );
  let readback = Command::new("cat").arg(&file).output().unwrap();
  assert_eq!(
    String::from_utf8_lossy(&readback.stdout),
    payload,
    "the bytes written through the mount read back byte for byte"
  );
}

/// Shape: how many times the hard-link pattern runs through a kernel mount — far past the rate at which a stale
/// name showed through Docker Desktop's share (99 in 100, 2026-10-01), so one stale lookup fails the test.
const HARD_LINK_ROUNDS: usize = 100;

/// The hard-link pattern git finalizes every object with, through the kernel mount at `path`: the temporary
/// held open and written, linked to the final name, the temporary removed, then the final name read at once.
/// Every round must read the bytes back (on this host's mount; the macOS host measured 0 failures in 200).
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn a_hard_link_outlives_its_first_name_through(path: &str) {
  let dir = format!("{path}/links");
  std::fs::create_dir(&dir).unwrap();
  for round in 0..HARD_LINK_ROUNDS {
    let (tmp, obj) = (format!("{dir}/tmp-{round}"), format!("{dir}/obj-{round}"));
    let held = std::fs::File::create(&tmp).unwrap();
    std::io::Write::write_all(&mut &held, b"object bytes").unwrap();
    std::fs::hard_link(&tmp, &obj).unwrap();
    std::fs::remove_file(&tmp).unwrap();
    assert_eq!(
      std::fs::read(&obj).unwrap_or_default(),
      b"object bytes",
      "round {round}: the second name outlives the first"
    );
    drop(held);
  }
  // The directory stays: removing a held-open name leaves an NFS client's silly-renamed `.nfs.*` file, which
  // the client deletes at the last close on its own schedule; the scratch volume goes with the test.
}

/// `slates unmount DIR` removes the mount (a pure `umount`, no daemon needed).
fn unmount_and_check(instance: &str, id: &str, path: &str) {
  let (code, _out, err) = run(instance, &["unmount", path]);
  assert_eq!(code, 0, "slates unmount failed: {err}");
  assert!(
    !is_mounted(path),
    "the kernel mount table no longer lists it"
  );
  // The kernel's own `UMNT` ended the mount's attachment (AUD-01): the volume reports none, and no
  // bound mount point remains.
  assert!(
    wait_for(|| attachments_of(instance, id) == "0"),
    "the kernel's UMNT ended the mount's attachment: {}",
    attachments_of(instance, id)
  );
  let (code, out, err) = run(instance, &["status", id]);
  assert_eq!(code, 0, "{err}");
  assert!(
    !out.lines().any(|line| line.starts_with("mount: ")),
    "no bound mount point remains after the unmount: {out}"
  );
}

/// The volume's attachment count as `slates status ID` reports it.
fn attachments_of(instance: &str, id: &str) -> String {
  let (code, out, err) = run(instance, &["status", id]);
  assert_eq!(code, 0, "{err}");
  value_of(&out, "attachments")
}

/// Polls `holds` at the test's pace until it holds or the deadline passes; whether it held.
fn wait_for(mut holds: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + WAIT_FOR;
  while Instant::now() < deadline {
    if holds() {
      return true;
    }
    pause();
  }
  holds()
}

/// The mount outlives the process that attached it (AUD-01; §4.6): `slates mount` exited the moment it
/// mounted, the daemon reaps that process's client — `clients_reaped` moves, the non-vacuity counter —
/// and the mount keeps serving under the attachment the kernel holds, the volume's one attachment.
fn mount_outlives_the_process_that_attached(instance: &str, id: &str, path: &str) {
  let reaped = |instance: &str| {
    let (code, out, err) = run(instance, &["status"]);
    assert_eq!(code, 0, "{err}");
    value_of(&out, "clients_reaped")
      .parse::<u64>()
      .expect("a count")
  };
  assert!(
    wait_for(|| reaped(instance) > 0),
    "the daemon reaped the exited command's client: clients_reaped = {}",
    reaped(instance)
  );
  let readback = Command::new("cat")
    .arg(format!("{path}/roundtrip.txt"))
    .output()
    .unwrap();
  assert_eq!(
    String::from_utf8_lossy(&readback.stdout),
    "written through a real slates kernel mount",
    "the mount still serves after its command's client was reaped"
  );
  assert_eq!(
    attachments_of(instance, id),
    "1",
    "the mount's attachment survived the reap of the client that made it"
  );
}

/// `slates mount ID DIR` establishes a real kernel NFS mount of a provisioned volume with no
/// privilege (§4.6, R10; `noresvport`), a file written through the mount reads back byte for byte,
/// and `slates unmount DIR` removes it — the whole command flow through the real binary against a
/// real anchor-supervised daemon (R5: drive the CLI, assert observable behaviour). Gated like the
/// anchor+daemon flow above: it performs a real kernel mount (a system-state change), so it runs only
/// under `SLATES_TEST_CLI=1` and skips loudly where `mount_nfs` is absent, keeping the default
/// `cargo test` green and hermetic.
#[test]
fn slates_mount_establishes_a_real_kernel_mount_and_unmount_removes_it() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the live mount flow: set SLATES_TEST_CLI=1 to run it (it performs a real kernel        mount_nfs and needs the machine to itself)"
    );
    return;
  }
  if !mount_nfs_available() {
    eprintln!("skipping the live mount flow: mount_nfs is not on this host (macOS/BSD only)");
    return;
  }
  let instance = format!("cli-mnt-{}", std::process::id());
  let anchor = start_anchor(&instance);

  let (code, out, err) = run(
    &instance,
    &["volume", "create", "mounted", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");

  // The guard tears the mount and the directory down even if a helper below panics.
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  mount_and_check(&instance, &id, &mount_point.path);
  assert_eq!(
    attachments_of(&instance, &id),
    "1",
    "the mount is the volume's one attachment (AUD-01)"
  );
  // The mount point is bound to the attachment (§4.4 `Bound`; GAP-A9-4): the status names it.
  let (code, out, err) = run(&instance, &["status", &id]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out
      .lines()
      .any(|line| line.starts_with(&format!("mount: {} (attachment ", mount_point.path))),
    "the status lists the bound mount point: {out}"
  );
  mount_root_is_owned_by_the_mounting_user(&mount_point.path);
  roundtrip_a_file_through(&mount_point.path);
  a_sparse_extension_through(&instance, &mount_point.path);
  mount_outlives_the_process_that_attached(&instance, &id, &mount_point.path);
  #[cfg(target_os = "macos")]
  a_forged_unmount_ends_nothing(&instance, &id, "mounted", &mount_point.path);
  unmount_and_check(&instance, &id, &mount_point.path);

  drop(mount_point);
  drop(anchor);
}

/// `status ID --json`: one JSON object carrying the volume's real fields (the MCP `status_json` schema).
fn json_status_of(instance: &str, id: &str) {
  let (code, out, err) = run(instance, &["status", id, "--json"]);
  assert_eq!(code, 0, "{err}");
  let out = out.trim();
  assert!(
    out.starts_with('{') && out.ends_with('}'),
    "a JSON object: {out}"
  );
  assert!(out.contains(&format!("\"id\":\"{id}\"")), "the id: {out}");
  assert!(out.contains("\"name\":\"jsonvol\""), "the name: {out}");
  assert!(out.contains("\"nfs_port\":"), "the nfs_port field: {out}");
  assert!(out.contains("\"region\":true"), "placed on a laptop: {out}");
}

/// `volume list --json`: a JSON array carrying the volume.
fn json_list_has(instance: &str, id: &str) {
  let (code, out, err) = run(instance, &["volume", "list", "--json"]);
  assert_eq!(code, 0, "{err}");
  let out = out.trim();
  assert!(
    out.starts_with('[') && out.ends_with(']'),
    "a JSON array: {out}"
  );
  assert!(
    out.contains(&format!("\"id\":\"{id}\"")),
    "the volume: {out}"
  );
}

/// `status --json` (no volume): the daemon's status as one JSON object (the MCP `daemon_json` schema).
fn json_daemon_status(instance: &str) {
  let (code, out, err) = run(instance, &["status", "--json"]);
  assert_eq!(code, 0, "{err}");
  let out = out.trim();
  assert!(
    out.starts_with('{') && out.ends_with('}'),
    "a JSON object: {out}"
  );
  assert!(out.contains("\"pid\":"), "the daemon pid: {out}");
  assert!(out.contains("\"shards\":"), "the shard count: {out}");
}

/// `versions GREEN --json` → a `{ "head": N }` object; `changed-since GREEN 0 --json` → a
/// `{ "paths": [...] }` object (empty on a fresh green).
fn json_merge_queries(instance: &str) {
  let (code, out, err) = run(instance, &["green", "greenjson"]);
  assert_eq!(code, 0, "{err}");
  let green = value_of(&out, "id");
  let (code, out, err) = run(instance, &["versions", &green, "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.trim().starts_with('{') && out.contains("\"head\":"),
    "versions json: {out}"
  );
  let (code, out, err) = run(instance, &["changed-since", &green, "0", "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.trim().starts_with('{') && out.contains("\"paths\":"),
    "changed-since json: {out}"
  );
  // work → edit → submit --json: a clean submit is an `accepted` outcome with the new version.
  let (code, out, err) = run(instance, &["work", &green, "workjson"]);
  assert_eq!(code, 0, "{err}");
  let work = value_of(&out, "id");
  let (code, _out, err) = run(instance, &["edit", &work, "/f.txt", "0", "0", "hi"]);
  assert_eq!(code, 0, "{err}");
  let (code, out, err) = run(instance, &["submit", &work, "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.trim().starts_with('{') && out.contains("\"accepted\":true"),
    "submit json accepted: {out}"
  );
  json_merge_reader(instance, &green);
}

/// A green reader through the CLI (§4.16 "Attachments and versions"; AC-6.13/T-6.15): `attach GREEN
/// --read --json` pins the head version (`"version":1`); a second work lands version 2 with a new file;
/// `read GREEN PATH --attachment A` still refuses the new file (exit 1, the pinned view) while `read
/// GREEN PATH` at the head streams it; `advance A --json` re-pins to 2 naming the invalidated path;
/// then the attached read streams the bytes.
fn json_merge_reader(instance: &str, green: &str) {
  let attachment = json_merge_reader_pins(instance, green);
  json_merge_reader_advances(instance, green, &attachment);
}

/// The attach pins version 1; a second work lands version 2; the attached read refuses the later file
/// while the head streams it. Returns the attachment id.
fn json_merge_reader_pins(instance: &str, green: &str) -> String {
  let (code, out, err) = run(instance, &["attach", green, "--read", "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.contains("\"version\":1"),
    "a green attachment pins the head: {out}"
  );
  let attachment = json_field(&out, "attachment");
  json_land_second_version(instance, green);
  let (code, _, err) = run(
    instance,
    &["read", green, "/g.txt", "--attachment", &attachment],
  );
  assert_eq!(code, 1, "the pinned view lacks the later file: {err}");
  assert!(err.contains("NotFound"), "{err}");
  let (code, out, err) = run(instance, &["read", green, "/g.txt"]);
  assert_eq!(code, 0, "{err}");
  assert_eq!(out, "later", "the head streams the later file's bytes");
  attachment
}

/// A second work over `green` creates `/g.txt` and submits: version 2.
fn json_land_second_version(instance: &str, green: &str) {
  let (code, out, _) = run(instance, &["work", green, "workjson2"]);
  assert_eq!(code, 0);
  let work = value_of(&out, "id");
  let (code, _, err) = run(instance, &["edit", &work, "/g.txt", "0", "0", "later"]);
  assert_eq!(code, 0, "{err}");
  let (code, out, err) = run(instance, &["submit", &work, "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.contains("\"version\":2"),
    "the second work lands version 2: {out}"
  );
}

/// `advance` re-pins to 2 naming the invalidated path; the attached read then streams the bytes.
fn json_merge_reader_advances(instance: &str, green: &str, attachment: &str) {
  let (code, out, err) = run(instance, &["advance", attachment, "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.contains("\"version\":2") && out.contains("\"invalidated\":[\"g.txt\"]"),
    "advance names the invalidated path: {out}"
  );
  let (code, out, err) = run(
    instance,
    &["read", green, "/g.txt", "--attachment", attachment],
  );
  assert_eq!(code, 0, "{err}");
  assert_eq!(out, "later", "the advanced view streams the bytes");
  let (code, _, err) = run(instance, &["detach", attachment]);
  assert_eq!(code, 0, "{err}");
}

/// `green`/`work` under `--json` emit JSON objects (the merge create verbs; `edit` is the same form).
fn json_merge_creates(instance: &str) {
  let (code, out, err) = run(instance, &["green", "greenj", "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.trim().starts_with('{') && out.contains("\"id\":"),
    "green json: {out}"
  );
  // Capture a green id from the text form, then exercise `work --json`.
  let (code, out, _) = run(instance, &["green", "greenj2"]);
  assert_eq!(code, 0);
  let green = value_of(&out, "id");
  let (code, out, err) = run(instance, &["work", &green, "wj", "--json"]);
  assert_eq!(code, 0, "{err}");
  let out = out.trim();
  assert!(
    out.starts_with('{') && out.contains("\"id\":") && out.contains("\"base\":"),
    "work json: {out}"
  );
}

/// A failing verb under `--json` emits a JSON error on stderr with a kind and message (GAP-A9-10
/// "consistent JSON errors"), and the same exit code (1, refused) as the text form.
fn json_error(instance: &str) {
  let (code, _out, err) = run(instance, &["status", &"0".repeat(32), "--json"]);
  assert_eq!(code, 1, "a missing volume is refused: {err}");
  let err = err.trim();
  assert!(
    err.starts_with('{') && err.contains("\"error\"") && err.contains("\"kind\":\"refused\""),
    "a JSON error object on stderr: {err}"
  );
}

/// Reads one compact-JSON field's value (a number, or a quoted string with its quotes stripped), so
/// the lifecycle flow can capture an id or a snapshot number the `--json` verbs print.
fn json_field(text: &str, key: &str) -> String {
  let needle = format!("\"{key}\":");
  let start = text
    .find(&needle)
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    + needle.len();
  let rest = &text[start..];
  let end = rest.find([',', '}', ']']).unwrap_or(rest.len());
  rest[..end].trim().trim_matches('"').to_owned()
}

/// Runs one verb under `--json`, asserts it succeeded (exit 0) and its output carries `needle`, and
/// returns the trimmed JSON so the caller can read an id or a snapshot number from it. A verb that
/// lands in the daemon's one startup-restart window gets exit 3 (no daemon) *without ever reaching
/// the daemon*, so retrying is side-effect-free (safe even for the non-idempotent verbs); a real
/// crash-loop exhausts the retries and fails. One place for the assertions keeps [`json_lifecycle`]
/// a branch-free sequence under the complexity gate.
fn json_ok(instance: &str, args: &[&str], needle: &str) -> String {
  for _ in 0..RESTART_RETRIES {
    let (code, out, err) = run(instance, args);
    if code == EXIT_UNAVAILABLE {
      pause();
      continue;
    }
    assert_eq!(code, 0, "{args:?}: {err}");
    let out = out.trim().to_owned();
    assert!(out.contains(needle), "{args:?} json wants {needle}: {out}");
    return out;
  }
  panic!("{args:?}: daemon unavailable after {RESTART_RETRIES} retries");
}

/// The value-returning lifecycle verbs under `--json` (GAP-A9-10 "consistent JSON" for the lifecycle,
/// Ada's request — a human scripting the CLI wants every verb to speak JSON): `create`/`clone` emit
/// `{ "id" }` (the same key `green`/`work` use), `snapshot` `{ "snapshot" }`, `attach` the MCP
/// attachment schema, the daemon-wide `grants`/`audit` a JSON array, and the acknowledgement verbs
/// (`resize`, `destroy`, `detach`) a uniform `{ "ok": true }`. Each id is captured from its own JSON.
/// (`pin` needs reserved space and `rewitness`/`land` a base/target; their `--json` shapes — success
/// `{ "pinned" }`/`{ "paths" }` and the JSON error object on refusal — are covered elsewhere.)
fn json_lifecycle(instance: &str) {
  let created = json_ok(
    instance,
    &["volume", "create", "lifej", "--bounded", "4MiB", "--json"],
    "\"id\":\"",
  );
  let id = json_field(&created, "id");
  let snapshotted = json_ok(
    instance,
    &["volume", "snapshot", &id, "--json"],
    "\"snapshot\":",
  );
  let snapshot = json_field(&snapshotted, "snapshot");
  json_ok(
    instance,
    &["volume", "clone", &id, &snapshot, "lifeclone", "--json"],
    "\"id\":\"",
  );
  json_ok(
    instance,
    &["volume", "resize", &id, "--bounded", "8MiB", "--json"],
    "\"ok\":true",
  );
  let attached = json_ok(
    instance,
    &["attach", &id, "--read", "--json"],
    "\"attachment\":",
  );
  let attachment = json_field(&attached, "attachment");
  json_ok(instance, &["detach", &attachment, "--json"], "\"ok\":true");
  json_ok(instance, &["grants", "--json"], "[");
  json_ok(instance, &["audit", "--json"], "[");
  json_ok(
    instance,
    &["volume", "destroy", &id, "--json"],
    "\"ok\":true",
  );
}

/// `--json` makes every verb emit the MCP JSON schema (§4.12, GAP-A9-10 "consistent JSON" — the CLI
/// and the MCP surface share one definition): the read verbs (`status ID`, `status`, `volume list`),
/// the merge verbs, and the lifecycle verbs (`create`/`snapshot`/`clone`/`resize`/`attach`/`detach`/
/// `grants`/`audit`/`destroy`), each carrying the volume's real fields, plus a JSON error on refusal.
/// Gated like the anchor+daemon flow (it needs a daemon), skipping loudly without `SLATES_TEST_CLI`.
#[test]
fn the_verbs_emit_json_with_the_json_flag() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the --json flow: set SLATES_TEST_CLI=1 to run it (needs the machine to itself)"
    );
    return;
  }
  let instance = format!("cli-json-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "jsonvol", "--bounded", "4MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  json_status_of(&instance, &id);
  json_list_has(&instance, &id);
  json_daemon_status(&instance);
  json_merge_queries(&instance);
  json_merge_creates(&instance);
  json_lifecycle(&instance);
  json_error(&instance);
  drop(anchor);
}

/// Starts the anchor with its stderr piped and reads the issuer-surface line it prints (the segment
/// handoff a shell exports to run `grant`/`enroll`/`revoke`/`run`); the rest of its stderr is drained
/// by a thread so the anchor never blocks on a full pipe. Waits for the daemon as `start_anchor` does.
fn start_anchor_with_issuer_surface(instance: &str) -> (AnchorProcess, Vec<(String, String)>) {
  use std::io::{BufRead, BufReader};
  let mut child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = BufReader::new(child.stderr.take().unwrap());
  let mut anchor = AnchorProcess { child, drain: None };
  let mut exports = Vec::new();
  let mut line = String::new();
  while exports.is_empty() {
    line.clear();
    assert!(
      stderr.read_line(&mut line).unwrap() > 0,
      "the anchor printed its issuer surface before ending"
    );
    if let Some(rest) = line
      .trim()
      .strip_prefix("slates anchor: issuer surface: export ")
    {
      exports = rest
        .split(' ')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    }
  }
  // Drain what the anchor says from here on (restarts, stops) so it never blocks writing.
  anchor.drain = Some(std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::sink());
  }));
  let started = Instant::now();
  let mut streak = 0u32;
  loop {
    let (code, _, _) = run(instance, &["volume", "list"]);
    streak = if code == 0 { streak + 1 } else { 0 };
    if streak >= STABLE_STREAK {
      let (code, _, error) = run(instance, &["bootstrap", "root"]);
      assert_eq!(code, 0, "explicit first-time bootstrap: {error}");
      return (anchor, exports);
    }
    assert!(started.elapsed() < START_WAIT, "the daemon came up: {code}");
    pause();
  }
}

/// Runs a client verb with the issuer surface's variables in its environment; (exit code, stdout,
/// stderr).
fn run_as_issuer(
  instance: &str,
  exports: &[(String, String)],
  args: &[&str],
) -> (i32, String, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .envs(exports.iter().map(|(name, value)| (name, value)))
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// `slates run [--json] -- CMD` (§4.12, §4.13 — the harness verb): the command runs as a consumer
/// enrolled for its lifetime with the capability delivered on an inherited descriptor: `run --json`
/// announces `{"consumer":N}` first and then the workload's own output (here a `slates volume create
/// --json`, itself a client that binds as the consumer); the volume it made is the consumer's, so the
/// account's `status` on it is refused (exit 1, `Forbidden`); the workload's exit code is `run`'s
/// (exit 1 from a refused verb inside passes through); `enroll --json` shows a consumer and its
/// capability once, `share` gives it a right on a volume, `revoke` ends it; and without the anchor's
/// variables the issuer verbs refuse (exit 4) before asking the daemon. Gated like the other process
/// flows (`SLATES_TEST_CLI=1`); the issuer surface is a name on macOS and Windows and a descriptor
/// only the anchor's children hold on Linux, where the flow skips loudly.
#[test]
fn slates_run_spawns_the_command_as_an_ephemeral_consumer() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the run/enroll flow: set SLATES_TEST_CLI=1 to run it (needs the machine to itself)"
    );
    return;
  }
  if cfg!(target_os = "linux") {
    eprintln!(
      "skipping the run/enroll flow: on Linux the anchor's segment is a descriptor only its children hold"
    );
    return;
  }
  let instance = format!("cli-run-{}", std::process::id());
  let (anchor, exports) = start_anchor_with_issuer_surface(&instance);
  let id = run_creates_a_volume_as_the_consumer(&instance, &exports);
  run_passes_the_workloads_exit_through(&instance, &exports, &id);
  enroll_share_and_revoke(&instance, &exports);
  issuer_verbs_refuse_without_the_anchor(&instance);
  drop(anchor);
}

/// `run --json -- slates volume create --json`: the consumer is announced first, the workload (the
/// CLI itself, bound as the consumer) creates a volume, and the account is refused on it. Returns
/// the volume's id.
fn run_creates_a_volume_as_the_consumer(instance: &str, exports: &[(String, String)]) -> String {
  let (code, out, err) = run_as_issuer(
    instance,
    exports,
    &[
      "run",
      "--json",
      "--",
      env!("CARGO_BIN_EXE_slates"),
      "--instance",
      instance,
      "volume",
      "create",
      "mine",
      "--bounded",
      "4MiB",
      "--json",
    ],
  );
  assert_eq!(code, 0, "{err}");
  let mut lines = out.lines();
  let announced = lines.next().unwrap_or_default();
  assert!(
    announced.starts_with("{\"consumer\":"),
    "the consumer is announced first: {out}"
  );
  let created = lines.next().unwrap_or_default();
  let id = json_field(created, "id");
  assert_eq!(id.len(), 32, "the workload created a volume: {out}");
  let (code, _, err) = run(instance, &["status", &id]);
  assert_eq!(
    code, 1,
    "the account is refused on the consumer's volume: {err}"
  );
  assert!(err.contains("Forbidden"), "{err}");
  id
}

/// The workload's exit passes through: a refused verb inside (another consumer's volume, seen from
/// a fresh consumer) exits 1, and so does `run`.
fn run_passes_the_workloads_exit_through(instance: &str, exports: &[(String, String)], id: &str) {
  let (code, _, err) = run_as_issuer(
    instance,
    exports,
    &[
      "run",
      "--",
      env!("CARGO_BIN_EXE_slates"),
      "--instance",
      instance,
      "status",
      id,
    ],
  );
  assert_eq!(code, 1, "the workload's exit code is run's: {err}");
}

/// `enroll --json` shows a consumer and its capability once; `share` gives it a right on the
/// account's volume; `revoke` ends it.
fn enroll_share_and_revoke(instance: &str, exports: &[(String, String)]) {
  let (code, out, err) = run_as_issuer(instance, exports, &["enroll", "--json"]);
  assert_eq!(code, 0, "{err}");
  let consumer = json_field(&out, "consumer");
  assert_eq!(json_field(&out, "capability").len(), 64, "{out}");
  let (code, out, err) = run(
    instance,
    &["volume", "create", "shared", "--bounded", "4MiB", "--json"],
  );
  assert_eq!(code, 0, "{err}");
  let shared = json_field(&out, "id");
  let (code, out, err) = run(
    instance,
    &[
      "share",
      &shared,
      &format!("consumer:{consumer}"),
      "--read",
      "--json",
    ],
  );
  assert_eq!(code, 0, "{err}");
  assert!(out.contains("\"ok\":true"), "{out}");
  let (code, out, err) = run_as_issuer(instance, exports, &["revoke", &consumer, "--json"]);
  assert_eq!(code, 0, "{err}");
  assert!(out.contains("\"ok\":true"), "{out}");
}

/// Without the anchor's variables there is no issuer authority: refused (exit 4) before the daemon
/// is asked.
fn issuer_verbs_refuse_without_the_anchor(instance: &str) {
  let (code, _, err) = run(instance, &["enroll"]);
  assert_eq!(code, 4, "{err}");
  assert!(err.contains("no anchor in this environment"), "{err}");
}

/// `slates profile --quick` prints the derived constants; `slates` alone prints the usage.
#[test]
fn the_profile_and_the_usage_print() {
  let output = slates().args(["profile", "--quick"]).output().unwrap();
  assert!(output.status.success());
  let text = String::from_utf8_lossy(&output.stdout);
  assert_eq!(value_of(&text, "quick"), "true");
  assert!(text.contains("spin_before_park_ns: "));
  let output = slates().output().unwrap();
  assert!(output.status.success());
  assert!(String::from_utf8_lossy(&output.stdout).starts_with("usage: slates"));
  let output = slates()
    .args(["volume", "list", "--bogus"])
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(2));
}

/// Shape: how long a three-process fleet gets for each phase — to form its mesh, to place a sealed
/// snapshot across processes, to retire a killed node, and for the successor to serve. Each is a few
/// protocol periods on loopback (the in-process fleet tests hold them to 15–25 s), widened for three
/// daemons booting one after another on a shared machine.
const FLEET_WAIT: Duration = Duration::from_secs(40);
/// Shape: the fleet's TLS name — every test certificate carries it and every node verifies its peers'
/// sessions under it.
const FLEET_NAME: &str = "slates-fleet";
/// Shape: the nodes of the test fleet, in manifest order (the order fixes the port layout).
const FLEET_NODES: [&str; 3] = ["a", "b", "c"];
/// Shape: the fleet's fault tolerance — three nodes at `f = 1` keep a commit quorum through one death
/// (`2f + 1 = 3` candidates, `f + 1 = 2` acknowledgements).
const FLEET_F: u32 = 1;
/// Shape: the ports a node serves on — one per plane, its base and the next (`slates_server::deploy`).
const PORTS_PER_NODE: u16 = 2;
/// Shape: the bottom of the port range the test searches for a node's free port block — above the
/// well-known and registered ports a shared machine has bound.
const PORT_FLOOR: u16 = 20_000;
/// Shape: the width of that range; the search starts at a pid-derived offset into it so two test
/// processes on one machine start apart.
const PORT_SPAN: u16 = 30_000;
/// Shape: how many candidate bases the search tries before failing (a block of six consecutive free UDP
/// ports is found within a handful on a shared machine).
const PORT_ATTEMPTS: u16 = 2_000;
/// Shape: the bytes written on the owner and read back on its successor.
const FLEET_PAYLOAD: &str =
  "sealed on the owner, replicated to its holders, served by its successor";
/// Shape: the volume's name in the fleet flow — also its NFS export name.
const FLEET_VOLUME: &str = "served";

/// A `slates daemon --fleet` process: killed (as a crash would kill it) and reaped when dropped, so a
/// failed assertion leaves no daemon behind.
struct FleetProcess {
  child: Child,
}

impl Drop for FleetProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// A scratch directory in the build output for the manifest and the DER files (`build_output_dir`),
/// removed when dropped — even when an assertion fails.
struct ScratchDir {
  path: String,
}

impl Drop for ScratchDir {
  fn drop(&mut self) {
    let _ = Command::new("rm").args(["-rf", &self.path]).output();
  }
}

fn scratch_dir() -> ScratchDir {
  ScratchDir {
    path: build_output_dir("slates-fleet"),
  }
}

/// A self-signed identity for `node` carrying the fleet's TLS name, written as DER files into `dir`: the
/// test's stand-in for the operator-provisioned certificate and key (§4.8 "certificates provisioned by the
/// operator"). The test writes the operator's files the daemon reads; test code is outside the R1 wall
/// (it writes only into the scratch directory it removes).
#[allow(clippy::disallowed_methods)]
fn mint_identity(dir: &str, node: &str) {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![FLEET_NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  std::fs::write(format!("{dir}/{node}.crt.der"), cert.der().as_ref()).unwrap();
  std::fs::write(format!("{dir}/{node}.key.der"), key.serialize_der()).unwrap();
}

/// The shared manifest (the documented shape, `crates/cli/src/fleet.rs`): every node with its loopback
/// base port and its DER files, written into `dir`; returns its path.
#[allow(clippy::disallowed_methods)]
fn write_manifest(dir: &str, bases: &[u16]) -> String {
  let nodes: Vec<serde_json::Value> = FLEET_NODES
    .iter()
    .zip(bases)
    .map(|(node, base)| {
      serde_json::json!({
        "node": node,
        "address": format!("127.0.0.1:{base}"),
        "certificate": format!("{node}.crt.der"),
        "key": format!("{node}.key.der"),
      })
    })
    .collect();
  let manifest = serde_json::json!({ "name": FLEET_NAME, "f": FLEET_F, "nodes": nodes });
  let path = format!("{dir}/fleet.json");
  std::fs::write(&path, manifest.to_string()).unwrap();
  path
}

/// A node's port block, held bound for the whole test: its base port and the socket on each of its
/// ports, handed to the node's daemon ([`start_fleet_daemon`]) rather than released for it to rebind — so
/// no other process can take a port between the test learning it and the daemon serving on it
/// (`docs/bugs/2026-09-28-a-released-test-port-was-taken-before-the-daemon-bound-it.md`: three concurrent
/// copies of this test, started with consecutive pids, searched overlapping ranges and one daemon's bind
/// failed).
struct HeldBlock {
  base: u16,
  sockets: Vec<std::net::UdpSocket>,
}

/// A block of `count` consecutive UDP loopback ports, bound together and **kept** bound, searched upward
/// from `from` within the attempt bound. Tests may use `std::net` (as the server's fleet tests do); the
/// daemon never links it (R1).
fn free_port_block(from: u16, count: u16) -> HeldBlock {
  let mut base = from;
  for _ in 0..PORT_ATTEMPTS {
    let sockets: Option<Vec<std::net::UdpSocket>> = (0..count)
      .map(|k| {
        base
          .checked_add(k)
          .and_then(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).ok())
      })
      .collect();
    if let Some(sockets) = sockets {
      return HeldBlock { base, sockets };
    }
    base = base.checked_add(count).unwrap_or(PORT_FLOOR);
  }
  panic!("no block of {count} free loopback UDP ports found from {from}");
}

/// One held port block per node, disjoint, from a pid-derived start.
fn port_blocks() -> Vec<HeldBlock> {
  let block = PORTS_PER_NODE;
  let offset = std::process::id() % u32::from(PORT_SPAN);
  let start = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  let mut blocks = Vec::with_capacity(FLEET_NODES.len());
  let mut from = start;
  for _ in FLEET_NODES {
    let held = free_port_block(from, block);
    from = held.base.checked_add(block).unwrap_or(PORT_FLOOR);
    blocks.push(held);
  }
  blocks
}

/// A duplicate of `socket`'s descriptor that a spawned child inherits (close-on-exec cleared); the
/// test's own socket keeps the port, and the duplicate is closed here once the child has it.
fn inheritable(socket: &std::net::UdpSocket) -> std::os::fd::OwnedFd {
  let duplicate: std::os::fd::OwnedFd = socket.try_clone().unwrap().into();
  rustix::io::fcntl_setfd(&duplicate, rustix::io::FdFlags::empty()).unwrap();
  duplicate
}

/// Starts `slates daemon --fleet MANIFEST --node NODE` alone (no anchor) at `instance`, handing it the
/// node's held serve sockets as a supervisor would ([`slates_anchor::ENV_FLEET_SERVE`]).
fn start_fleet_daemon(
  instance: &str,
  manifest: &str,
  node: &str,
  held: &HeldBlock,
) -> FleetProcess {
  use std::os::fd::AsRawFd;
  let [probe, record] = held.sockets.as_slice() else {
    panic!("a node's block is its probe and record ports");
  };
  let (probe, record) = (inheritable(probe), inheritable(record));
  let child = slates()
    .args([
      "--instance",
      instance,
      "daemon",
      "--quick",
      "--shards",
      SHARDS,
      "--fleet",
      manifest,
      "--node",
      node,
    ])
    .env(
      slates_anchor::ENV_FLEET_SERVE,
      format!("{},{}", probe.as_raw_fd(), record.as_raw_fd()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  FleetProcess { child }
}

/// The daemon's pid, from `status --json`, or `None` while no daemon answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, out, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  let after = out.split("\"pid\":").nth(1)?;
  after
    .trim_start()
    .split(|c: char| !c.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Whether some other socket can bind 127.0.0.1:`port` right now (a probe of who holds it: bound and
/// dropped at once, only ever on a port the test expects to be held).
fn port_is_free(port: u16) -> bool {
  std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok()
}

/// AC-8.1 (§4.8 "Deployment", §4.6's anchor-held listener applied to the fleet;
/// docs/bugs/2026-09-28-a-released-test-port-was-taken-before-the-daemon-bound-it.md): an anchor that
/// supervises a fleet node holds the node's two manifest ports across a daemon's death and restart —
/// no other socket can take either port while the daemon is down — and the restarted daemon serves on
/// them. The test hands the anchor its held block, then lets go of its own copies, so from there only the
/// anchor holds the ports.
#[test]
fn a_fleet_node_under_its_anchor_keeps_its_serve_ports_across_a_daemon_restart() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the anchored fleet restart flow: set SLATES_TEST_CLI=1 to run it (an anchor and its daemon)"
    );
    return;
  }
  let scratch = scratch_dir();
  let instance = format!("cli-fleet-anchor-{}", std::process::id());
  let (_anchor, ports) = start_anchored_fleet_node(&scratch.path, &instance);
  let first = await_daemon(&instance, None, &[]);
  assert!(
    ports.iter().all(|port| !port_is_free(*port)),
    "the anchored node holds both manifest ports"
  );
  let killed = Command::new("kill")
    .args(["-9", &first.to_string()])
    .status()
    .unwrap();
  assert!(killed.success(), "the daemon was killed");
  // Until a new daemon answers, the anchor alone holds the ports: nothing can take them in the gap.
  let second = await_daemon(&instance, Some(first), &ports);
  assert_ne!(second, first, "a new daemon serves");
  let (code, out, err) = run(&instance, &["status"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    out.contains("fleet"),
    "the restarted daemon runs as the fleet node on the held sockets: {out}"
  );
}

/// Starts `slates anchor --fleet` for node `a` of a fresh three-node manifest in `dir`, handing the anchor
/// the node's held port block, then lets go of the test's own copies — so from here only the anchor holds
/// the node's two ports, which are returned.
fn start_anchored_fleet_node(dir: &str, instance: &str) -> (AnchorProcess, [u16; 2]) {
  use std::os::fd::AsRawFd;
  for node in FLEET_NODES {
    mint_identity(dir, node);
  }
  let blocks = port_blocks();
  let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
  let manifest = write_manifest(dir, &bases);
  let [probe, record] = blocks[0].sockets.as_slice() else {
    panic!("a node's block is its probe and record ports");
  };
  let (probe, record) = (inheritable(probe), inheritable(record));
  let child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
      "--fleet",
      &manifest,
      "--node",
      FLEET_NODES[0],
    ])
    .env(
      slates_anchor::ENV_FLEET_SERVE,
      format!("{},{}", probe.as_raw_fd(), record.as_raw_fd()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  (
    AnchorProcess { child, drain: None },
    [bases[0], bases[0] + 1],
  )
}

/// A client certificate and key, DER, as a node's `tlshd` holds them.
type ClientIdentity = (
  rustls::pki_types::CertificateDer<'static>,
  rustls::pki_types::PrivateKeyDer<'static>,
);

/// Reads one record-marked message from `stream`; `None` when the peer closed first. A read a signal
/// interrupts is retried, as `read_exact` does.
fn read_one_record(stream: &mut dyn std::io::Read) -> Option<Vec<u8>> {
  let mut bytes = Vec::new();
  let mut chunk = [0u8; 4096];
  loop {
    if let Ok((message, _)) = slates_bridge_nfs::rpc::read_record(&bytes) {
      return Some(message);
    }
    match stream.read(&mut chunk) {
      Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
      Ok(0) | Err(_) => return None,
      Ok(count) => bytes.extend_from_slice(&chunk[..count]),
    }
  }
}

/// An RPC-with-TLS session on the export at `port`, as a node's NFS client opens one (RFC 9289): the `AUTH_TLS`
/// probe (its reply returned), then a mutual TLS 1.3 handshake with `client` (issued by the fleet's authority,
/// trusted as `authority`) offering ALPN `sunrpc`.
#[allow(clippy::disallowed_types)] // rustls's client takes `Arc` by signature (D-8 exception 3, a test harness).
fn export_session(
  port: u16,
  authority: &rustls::pki_types::CertificateDer<'static>,
  client: ClientIdentity,
) -> (
  Option<Vec<u8>>,
  rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
) {
  use slates_bridge_nfs::rpc::write_record;
  use slates_bridge_nfs::rpc_tls::{ALPN_SUNRPC, AUTH_TLS};
  use std::io::Write;
  let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  socket.set_read_timeout(Some(START_WAIT)).unwrap();
  let mut probe = Vec::new();
  for word in [1u32, 0, 2, 100_003, 4, 0, AUTH_TLS, 0, 0, 0] {
    probe.extend_from_slice(&word.to_be_bytes());
  }
  socket.write_all(&write_record(&probe)).unwrap();
  let reply = read_one_record(&mut socket);
  let mut roots = rustls::RootCertStore::empty();
  roots.add(authority.clone()).unwrap();
  let mut config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
    rustls::crypto::ring::default_provider(),
  ))
  .with_protocol_versions(&[&rustls::version::TLS13])
  .unwrap()
  .with_root_certificates(roots)
  .with_client_auth_cert(vec![client.0], client.1)
  .unwrap();
  config.alpn_protocols = vec![ALPN_SUNRPC.to_vec()];
  let connection = rustls::ClientConnection::new(
    std::sync::Arc::new(config),
    rustls::pki_types::ServerName::try_from(FLEET_NAME).unwrap(),
  )
  .unwrap();
  (reply, rustls::StreamOwned::new(connection, socket))
}

/// Sends one record-marked call inside `session` and returns its reply message.
fn session_call(
  session: &mut rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
  record: &[u8],
) -> Option<Vec<u8>> {
  use std::io::Write;
  session.write_all(record).ok()?;
  read_one_record(session)
}

/// NFSv3 `NULL` as a record-marked call with transaction id `xid`.
fn nfs_null(xid: u32) -> Vec<u8> {
  let mut message = Vec::new();
  for word in [xid, 0, 2, 100_003, 3, 0, 0, 0, 0, 0] {
    message.extend_from_slice(&word.to_be_bytes());
  }
  slates_bridge_nfs::rpc::write_record(&message)
}

/// A fleet enrolled under one operator authority, written into a scratch directory: the authority, the manifest
/// (every node in its own failure domain, `f = 0` so node a alone holds a quorum), and each node's held ports.
struct EnrolledFleet {
  authority: rcgen::Certificate,
  authority_key: rcgen::KeyPair,
  manifest: String,
  blocks: Vec<HeldBlock>,
}

impl EnrolledFleet {
  #[allow(clippy::disallowed_methods)] // the test's own scratch files: the manifest and its DER files
  fn write(dir: &str) -> EnrolledFleet {
    let authority_key = rcgen::KeyPair::generate().unwrap();
    let mut authority_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    authority_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let authority = authority_params.self_signed(&authority_key).unwrap();
    std::fs::write(format!("{dir}/authority.crt.der"), authority.der().as_ref()).unwrap();
    let fleet = EnrolledFleet {
      authority,
      authority_key,
      manifest: format!("{dir}/fleet.json"),
      blocks: port_blocks(),
    };
    for (domain, node) in FLEET_NODES.iter().enumerate() {
      let (cert, key) = fleet.issue(vec![
        FLEET_NAME.to_owned(),
        format!("r0.d{domain}.{FLEET_NAME}"),
      ]);
      std::fs::write(format!("{dir}/{node}.crt.der"), cert.as_ref()).unwrap();
      std::fs::write(format!("{dir}/{node}.key.der"), key.secret_der()).unwrap();
    }
    let nodes: Vec<serde_json::Value> = FLEET_NODES
      .iter()
      .zip(&fleet.blocks)
      .enumerate()
      .map(|(domain, (node, held))| {
        serde_json::json!({
          "node": node,
          "address": format!("127.0.0.1:{}", held.base),
          "certificate": format!("{node}.crt.der"),
          "key": format!("{node}.key.der"),
          "domain": domain,
        })
      })
      .collect();
    let document = serde_json::json!({
      "name": FLEET_NAME,
      "f": 0,
      "nodes": nodes,
      "enrollment_roots": ["authority.crt.der"],
    });
    std::fs::write(&fleet.manifest, document.to_string()).unwrap();
    fleet
  }

  /// A leaf for `names` the authority signs.
  fn issue(&self, names: Vec<String>) -> ClientIdentity {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(names)
      .unwrap()
      .signed_by(&key, &self.authority, &self.authority_key)
      .unwrap();
    (
      cert.der().clone(),
      rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
  }

  /// A node's NFS client identity, as its `tlshd` holds one.
  fn client(&self) -> ClientIdentity {
    self.issue(vec!["node-k8s-1".to_owned()])
  }

  /// Starts `slates anchor --fleet` for node a, handing it the node's probe and record sockets and its export's
  /// TCP listener on the base port (the test lets go of its own copies), and returns the anchor and the port.
  fn start_anchor(&self, instance: &str) -> (AnchorProcess, u16) {
    use std::os::fd::AsRawFd;
    let [probe, record] = self.blocks[0].sockets.as_slice() else {
      panic!("a node's block is its probe and record ports");
    };
    let port = self.blocks[0].base;
    let export = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let export_fd: std::os::fd::OwnedFd = export.try_clone().unwrap().into();
    rustix::io::fcntl_setfd(&export_fd, rustix::io::FdFlags::empty()).unwrap();
    let (probe, record) = (inheritable(probe), inheritable(record));
    let child = slates()
      .args([
        "--instance",
        instance,
        "anchor",
        "--quick",
        "--shards",
        SHARDS,
        "--fleet",
        &self.manifest,
        "--node",
        FLEET_NODES[0],
      ])
      .env(
        slates_anchor::ENV_FLEET_SERVE,
        format!(
          "{},{},{}",
          probe.as_raw_fd(),
          record.as_raw_fd(),
          export_fd.as_raw_fd()
        ),
      )
      .stdout(Stdio::null())
      .stderr(Stdio::inherit())
      .spawn()
      .unwrap();
    (AnchorProcess { child, drain: None }, port)
  }

  /// `STARTTLS` then `NULL` answered `SUCCESS` inside a session on the export at `port`.
  fn assert_serves_null(&self, port: u16, which: &str) {
    let (probe, mut session) = export_session(port, &self.authority.der().clone(), self.client());
    let reply = session_call(&mut session, &nfs_null(2));
    assert_eq!(
      probe,
      Some(slates_bridge_nfs::rpc_tls::starttls_reply(1)),
      "{which} answers the probe"
    );
    assert_eq!(
      reply,
      Some(slates_bridge_nfs::rpc::reply_bytes(
        2,
        slates_bridge_nfs::rpc::AcceptStatus::Success,
        &[]
      )),
      "{which} serves the call in the session"
    );
  }

  /// `MNT path` inside a fresh session on the export at `port`: the root handle or the refusal.
  fn mount(&self, port: u16, path: &str, xid: u32) -> Option<Result<Vec<u8>, String>> {
    let caller = slates_bridge_nfs::client::Credentials {
      uid: 0,
      gid: 0,
      gids: Vec::new(),
    };
    let (_, mut session) = export_session(port, &self.authority.der().clone(), self.client());
    let reply = session_call(
      &mut session,
      &slates_bridge_nfs::client::mnt_call(xid, &caller, path),
    )?;
    Some(
      slates_bridge_nfs::client::parse_mnt_reply(&reply, xid)
        .map(|handle| handle.0.to_vec())
        .map_err(|refusal| format!("{refusal:?}")),
    )
  }
}

/// As an operator publishing a volume for a PersistentVolume: bootstrap the group, create a volume, `slates
/// export` it, `MNT` the printed path over the export, `slates detach` the export, `MNT` again. The first `MNT`
/// is admitted; the second refused.
fn assert_an_export_is_published_and_detached(fleet: &EnrolledFleet, instance: &str, port: u16) {
  let (code, _, err) = run(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap: {err}");
  let started = Instant::now();
  let id = loop {
    let (code, out, err) = run(
      instance,
      &["volume", "create", "published", "--bounded", "4MiB"],
    );
    if code == 0 {
      break value_of(&out, "id");
    }
    assert!(
      err.contains("ConsensusNotInitialized") && started.elapsed() < START_WAIT,
      "create: {err}"
    );
    pause();
  };
  let (code, out, err) = run(instance, &["export", &id, "--json"]);
  assert_eq!(code, 0, "export: {err}");
  let path = json_field(&out, "path");
  let admitted = fleet.mount(port, &path, 3);
  let (code, _, err) = run(instance, &["detach", &json_field(&out, "attachment")]);
  assert_eq!(code, 0, "detach: {err}");
  let after_detach = fleet.mount(port, &path, 4);
  assert!(
    matches!(admitted, Some(Ok(_))),
    "the exported path is admitted over the TLS session: {admitted:?}"
  );
  assert!(
    matches!(after_detach, Some(Err(_))),
    "a detached export admits nothing: {after_detach:?}"
  );
}

/// §4.6 "Kubernetes publication without privilege" (AUD-29-75) and §4.8's anchor-held ports. Do: start
/// `slates anchor --fleet` for node a of a manifest with an operator authority (every node enrolled under it, in
/// its own failure domain), handing the anchor the node's probe and record sockets and its export's TCP listener
/// on the base port; call `NULL` over an RPC-with-TLS session as a node's NFS client would; `kill -9` the daemon
/// and call again; then publish a volume (`slates export`), `MNT` its path over the export, detach it, and `MNT`
/// again. Expect: `STARTTLS` and `NULL` answered on both daemons (the export is the anchor's, so its port never
/// closes); the exported path admitted; and refused once the export is detached.
#[test]
fn an_anchored_node_serves_its_export_over_rpc_with_tls_across_a_daemon_restart() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the anchored export flow: set SLATES_TEST_CLI=1 to run it (an anchor and its daemon)"
    );
    return;
  }
  let scratch = scratch_dir();
  let fleet = EnrolledFleet::write(&scratch.path);
  let instance = format!("cli-fleet-export-{}", std::process::id());
  let (_anchor, port) = fleet.start_anchor(&instance);
  let first = await_daemon(&instance, None, &[]);
  fleet.assert_serves_null(port, "the first daemon");
  let killed = Command::new("kill")
    .args(["-9", &first.to_string()])
    .status()
    .unwrap();
  assert!(killed.success(), "the daemon was killed");
  let second = await_daemon(&instance, Some(first), &[]);
  assert_ne!(second, first, "a new daemon serves");
  fleet.assert_serves_null(port, "the restarted daemon, on the same port");
  assert_an_export_is_published_and_detached(&fleet, &instance, port);
}

/// Waits (bounded by the start wait) for a daemon other than `replaced` to answer at `instance`, and
/// returns its pid; on every look before it answers, each of `held` must still be taken.
fn await_daemon(instance: &str, replaced: Option<u32>, held: &[u16]) -> u32 {
  let started = Instant::now();
  loop {
    assert!(
      held.iter().all(|port| !port_is_free(*port)),
      "a manifest port was free while the daemon was down"
    );
    if let Some(pid) = daemon_pid(instance).filter(|pid| Some(*pid) != replaced) {
      return pid;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "a fleet daemon answered at {instance}"
    );
    pause();
  }
}

/// AC-8.1 (§4.8 "Deployment"; typed refusals): a daemon handed serve sockets that are not what its plan
/// says does not start, and says why — a socket bound at another port is refused naming both addresses,
/// and a descriptor list that is not two numbers is refused naming the variable.
#[test]
fn a_daemon_refuses_inherited_serve_sockets_bound_elsewhere_or_malformed() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the inherited-serve refusals: set SLATES_TEST_CLI=1 to run them (starts daemons)"
    );
    return;
  }
  use std::os::fd::AsRawFd;
  let scratch = scratch_dir();
  for node in FLEET_NODES {
    mint_identity(&scratch.path, node);
  }
  let blocks = port_blocks();
  let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
  let manifest = write_manifest(&scratch.path, &bases);
  let refused = |value: String| {
    slates()
      .args([
        "--instance",
        &format!("cli-fleet-refuse-{}", std::process::id()),
        "daemon",
        "--quick",
        "--shards",
        SHARDS,
        "--fleet",
        &manifest,
        "--node",
        FLEET_NODES[0],
      ])
      .env(slates_anchor::ENV_FLEET_SERVE, value)
      .output()
      .unwrap()
  };
  // Node b's sockets handed to node a: bound, datagram, but at b's ports.
  let [elsewhere_probe, elsewhere_record] = blocks[1].sockets.as_slice() else {
    panic!("a node's block is its probe and record ports");
  };
  let (probe, record) = (inheritable(elsewhere_probe), inheritable(elsewhere_record));
  let output = refused(format!("{},{}", probe.as_raw_fd(), record.as_raw_fd()));
  drop((probe, record));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    !output.status.success(),
    "the daemon did not start: {stderr}"
  );
  assert!(
    stderr.contains(&format!("bound at 127.0.0.1:{}", bases[1]))
      && stderr.contains(&format!("not at the planned 127.0.0.1:{}", bases[0])),
    "the refusal names where the socket is and where it should be: {stderr}"
  );
  let output = refused("not-descriptors".to_owned());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    !output.status.success(),
    "the daemon did not start: {stderr}"
  );
  assert!(
    stderr.contains(slates_anchor::ENV_FLEET_SERVE)
      && stderr.contains("not two descriptor numbers"),
    "the refusal names the malformed variable: {stderr}"
  );
}

/// Waits until the daemon at `instance` answers a verb, bounded by the start wait.
fn wait_answers(instance: &str) {
  let started = Instant::now();
  loop {
    let (code, _, _) = run(instance, &["volume", "list"]);
    if code == 0 {
      return;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "the fleet daemon at {instance} came up: still {code}"
    );
    pause();
  }
}

/// The fleet lines of `slates status`: this node's member id, the members it holds alive, and the peers
/// it has a formed probe session to.
#[derive(Debug)]
struct FleetView {
  host: String,
  members: Vec<String>,
  peers_probed: u32,
  /// The serve sockets' drop counters and the refusal lines, so a failed assertion shows what the
  /// daemon refused or dropped.
  dropped: String,
  refusals: String,
}

fn fleet_view(instance: &str) -> Option<FleetView> {
  let (code, out, _) = run(instance, &["status"]);
  if code != 0 {
    return None;
  }
  let mut members: Vec<String> = value_of(&out, "fleet_members")
    .split_whitespace()
    .map(str::to_owned)
    .collect();
  members.sort();
  let dropped = format!(
    "unknown_id={} inbox_full={} refused={} replaced={}",
    value_of(&out, "fleet_unknown_id"),
    value_of(&out, "fleet_inbox_full"),
    value_of(&out, "fleet_sessions_refused"),
    value_of(&out, "fleet_replaced")
  );
  let refusals: Vec<&str> = out.lines().filter(|l| l.contains(" refused ")).collect();
  Some(FleetView {
    host: value_of(&out, "fleet_host"),
    members,
    peers_probed: value_of(&out, "fleet_peers_probed").parse().unwrap(),
    dropped,
    refusals: refusals.join("; "),
  })
}

/// Polls every node's fleet view until each holds `members` members alive with `probed` peers probed, or
/// the fleet wait passes; returns the views then (the caller asserts on them, so a failure shows every
/// node's view).
fn wait_fleet_views(instances: &[String], members: usize, probed: u32) -> Vec<Option<FleetView>> {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let views: Vec<Option<FleetView>> = instances.iter().map(|i| fleet_view(i)).collect();
    let settled = views.iter().all(|view| {
      view
        .as_ref()
        .is_some_and(|v| v.members.len() == members && v.peers_probed == probed)
    });
    if settled || Instant::now() >= deadline {
      return views;
    }
    pause();
  }
}

/// Every process derived the same fleet from the one manifest: the same three member ids (its own among
/// them, distinct per node) and both peers probed.
fn assert_formed(views: &[Option<FleetView>]) -> Vec<String> {
  let formed: Vec<&FleetView> = views
    .iter()
    .map(|v| {
      v.as_ref()
        .unwrap_or_else(|| panic!("every node answers status: {views:?}"))
    })
    .collect();
  let members = &formed[0].members;
  assert_eq!(members.len(), FLEET_NODES.len(), "three members: {views:?}");
  for view in &formed {
    assert_eq!(
      &view.members, members,
      "every process holds the same members: {views:?}"
    );
    assert!(
      members.contains(&view.host),
      "a node is among its own members: {views:?}"
    );
    assert_eq!(
      view.peers_probed,
      u32::try_from(FLEET_NODES.len() - 1).unwrap(),
      "both peers probed: {views:?}"
    );
    // Fresh identities replace the manifest's seed links. A replacement is a successful lifecycle
    // transition, not a dropped packet; queue/capacity loss and unexpected refusals still fail here. The
    // counter map also carries events: the configuration fan's deliveries (`fleet.fan.sent`) and the
    // periods that owed a shard nothing (`fleet.fan.unchanged`) are formation's normal work (AUD-29-29); a
    // fan a shard's channel refused (`fleet.fan.refused`) is not, and still fails here.
    let (drops, _) = view.dropped.rsplit_once(' ').unwrap();
    assert_eq!(
      drops, "unknown_id=0 inbox_full=0 refused=0",
      "the serve sockets dropped nothing forming the fleet: {views:?}"
    );
    assert!(
      view
        .refusals
        .split(';')
        .filter(|line| !line.trim().is_empty())
        .all(|line| {
          [
            "fleet.discovery.invalidated:",
            "fleet.link.stale_return:",
            "fleet.accept.replaced:",
            "fleet.fan.sent:",
            "fleet.fan.unchanged:",
          ]
          .iter()
          .any(|counter| line.split_whitespace().nth(3) == Some(counter))
        }),
      "only superseded link work may end during formation: {views:?}"
    );
  }
  let mut hosts: Vec<String> = formed.iter().map(|v| v.host.clone()).collect();
  hosts.sort();
  hosts.dedup();
  assert_eq!(
    hosts.len(),
    FLEET_NODES.len(),
    "distinct member ids: {views:?}"
  );
  hosts
}

/// Writes the payload as `hello.txt` through a kernel mount at `path` (the shell writes; R1).
fn write_payload_through(path: &str) {
  let wrote = Command::new("sh")
    .arg("-c")
    .arg(format!(
      "printf '%s' '{FLEET_PAYLOAD}' > '{path}/hello.txt'"
    ))
    .output()
    .unwrap();
  assert!(
    wrote.status.success(),
    "write through the owner's mount: {}",
    String::from_utf8_lossy(&wrote.stderr)
  );
}

/// Provisions the volume on the owner, writes the payload into it through a real kernel mount where
/// `mount_nfs` exists (the mount is released before the owner dies, so no kernel client is left talking to
/// a dead server), and seals it; returns the id and the snapshot.
fn seal_on_owner(instance: &str, mountable: bool) -> (String, String) {
  // The mesh forms before a fresh learner imports the bootstrapped groups. The explicit refusal
  // precedes any create effect, so only that refusal is retried, within the existing formation bound.
  let deadline = Instant::now() + FLEET_WAIT;
  let out = loop {
    let (code, out, err) = run(
      instance,
      &["volume", "create", FLEET_VOLUME, "--bounded", "8MiB"],
    );
    if code == 1 && err.trim() == "slates: refused: ConsensusNotInitialized" {
      assert!(
        Instant::now() < deadline,
        "the owner imported the bootstrapped groups: {err}"
      );
      pause();
      continue;
    }
    assert_eq!(code, 0, "{err}");
    break out;
  };
  let id = value_of(&out, "id");
  if mountable {
    let mount_point = MountPoint {
      path: fresh_mount_point(),
    };
    mount_and_check(instance, &id, &mount_point.path);
    write_payload_through(&mount_point.path);
    unmount_and_check(instance, &id, &mount_point.path);
  } else {
    eprintln!(
      "mount_nfs is not on this host: the fleet flow seals an empty volume (the content read-back runs on macOS/BSD)"
    );
  }
  let (code, out, err) = run(instance, &["volume", "snapshot", &id]);
  assert_eq!(code, 0, "{err}");
  (id, value_of(&out, "snapshot"))
}

/// Polls the owner's `volume placed ID --snapshot N` until it answers placed (at `f = 1`, a peer process
/// acknowledged the content and the head) or the fleet wait passes.
fn wait_placed(instance: &str, id: &str, snapshot: &str) {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    let (code, out, err) = run(instance, &["volume", "placed", id, "--snapshot", snapshot]);
    assert_eq!(code, 0, "{err}");
    if value_of(&out, "placed") == "true" {
      return;
    }
    assert!(
      Instant::now() < deadline,
      "the snapshot placed across processes within the fleet wait"
    );
    pause();
  }
}

/// Before the owner dies its peers hold the head as candidates but serve no such volume: `volume stat`
/// refuses on both — the transition the takeover makes is then observable, not vacuous.
fn assert_not_served(instances: &[String], id: &str) {
  for instance in instances {
    let (code, _, err) = run(instance, &["volume", "stat", id]);
    assert_eq!(code, 1, "a holder does not serve the owner's volume: {err}");
    assert!(err.contains("NotFound"), "{err}");
  }
}

/// The survivors retired the dead node: two members, one peer probed, the dead id gone.
fn assert_retired(views: &[Option<FleetView>], dead: &str) {
  for view in views {
    let view = view
      .as_ref()
      .unwrap_or_else(|| panic!("every survivor answers status: {views:?}"));
    assert_eq!(
      view.members.len(),
      2,
      "two members after the death: {views:?}"
    );
    assert!(
      !view.members.contains(&dead.to_owned()),
      "the dead node is retired: {views:?}"
    );
    assert_eq!(view.peers_probed, 1, "one peer left in the mesh: {views:?}");
  }
}

/// Polls the survivors' `volume stat ID` until one serves the volume (the successor materialized it under
/// its id, named and placed) or the fleet wait passes; returns the successor's instance. A wait that runs out
/// fails with every survivor's status and its last answer for the volume, so the failure names its state.
fn wait_successor(instances: &[String], id: &str) -> String {
  let deadline = Instant::now() + FLEET_WAIT;
  loop {
    for instance in instances {
      let (code, out, _) = run(instance, &["volume", "stat", id]);
      if code == 0 {
        assert_eq!(value_of(&out, "name"), FLEET_VOLUME);
        assert_eq!(value_of(&out, "placed"), "true");
        return instance.clone();
      }
    }
    if Instant::now() >= deadline {
      let states: Vec<String> = instances
        .iter()
        .map(|instance| {
          let (status_code, status, status_err) = run(instance, &["status"]);
          let (stat_code, _, stat_err) = run(instance, &["volume", "stat", id]);
          format!(
            "--- {instance}: volume stat exit {stat_code}: {}\n--- {instance}: status exit {status_code}:\n{status}{status_err}",
            stat_err.trim()
          )
        })
        .collect();
      panic!(
        "a survivor took the volume over and serves it within the fleet wait:\n{}",
        states.join("\n")
      );
    }
    pause();
  }
}

/// The payload written on the dead owner reads back through a real kernel mount of the successor's
/// volume — where `mount_nfs` exists; elsewhere the read-back skips loudly.
fn read_on_successor(instance: &str, id: &str, mountable: bool) {
  if !mountable {
    eprintln!(
      "mount_nfs is not on this host: skipping the read-back through the successor's mount"
    );
    return;
  }
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  mount_and_check(instance, id, &mount_point.path);
  let readback = Command::new("cat")
    .arg(format!("{}/hello.txt", mount_point.path))
    .output()
    .unwrap();
  assert_eq!(
    String::from_utf8_lossy(&readback.stdout),
    FLEET_PAYLOAD,
    "the file written on the dead owner reads back byte for byte through the successor's mount"
  );
  unmount_and_check(instance, id, &mount_point.path);
}

/// AC (§2.6 boot step 6 "multi-process deployment"; §4.8 "Membership", "Promotion and takeover"; §4.10;
/// R8): three real `slates daemon --fleet` **processes**, each started from the **same** manifest with its
/// own `--node`, form one `f = 1` fleet over loopback UDP — every process derives the same three member
/// ids from the certificates (each node's `status` lists the same members, its own among them) and probes
/// both peers. A volume sealed on one node **places across processes** (`volume placed` at `f = 1` needs
/// a peer process's acknowledgement) while its peers serve no such volume. Killed with SIGKILL, the owner
/// is retired by both survivors (their `status` drops it), and the survivor rendezvous ranks first takes
/// the volume over and serves it under its id — the file written on the dead owner reading back through a
/// real kernel mount of the successor where `mount_nfs` exists (elsewhere that step skips loudly; the
/// deployment proof stands). Gated like the other process flows (`SLATES_TEST_CLI=1`): three daemons with
/// spinning shards need the machine to themselves.
#[test]
fn three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the fleet deployment flow: set SLATES_TEST_CLI=1 to run it (three daemons; needs the machine to itself)"
    );
    return;
  }
  let pid = std::process::id();
  let scratch = scratch_dir();
  for node in FLEET_NODES {
    mint_identity(&scratch.path, node);
  }
  let blocks = port_blocks();
  let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
  let manifest = write_manifest(&scratch.path, &bases);
  let instances: Vec<String> = FLEET_NODES
    .iter()
    .map(|node| format!("cli-fleet-{node}-{pid}"))
    .collect();
  let mut daemons: Vec<FleetProcess> = FLEET_NODES
    .iter()
    .zip(&instances)
    .zip(&blocks)
    .map(|((node, instance), held)| start_fleet_daemon(instance, &manifest, node, held))
    .collect();
  for instance in &instances {
    wait_answers(instance);
  }

  // Formation: one manifest, three processes, one fleet.
  let views = wait_fleet_views(
    &instances,
    FLEET_NODES.len(),
    u32::try_from(FLEET_NODES.len() - 1).unwrap(),
  );
  let hosts = assert_formed(&views);
  // Bootstrap once on the eventual root representative and keep it alive. The owner's crash then
  // exercises regional takeover through a surviving quorum, not loss of the singleton root group.
  let bootstrap = views
    .iter()
    .enumerate()
    .min_by_key(|(_, view)| view.as_ref().unwrap().host.parse::<u64>().unwrap())
    .map(|(index, _)| index)
    .unwrap();
  let (code, _, error) = run(&instances[bootstrap], &["bootstrap", "root"]);
  assert_eq!(
    code, 0,
    "one explicit bootstrap of the fresh fleet: {error}"
  );
  let owner = (0..instances.len())
    .find(|index| *index != bootstrap)
    .unwrap();
  let owner_host = views[owner].as_ref().unwrap().host.clone();
  assert!(hosts.contains(&owner_host));

  // Placement across processes, then the owner's death as a crash would deal it.
  let mountable = mount_nfs_available();
  let (id, snapshot) = seal_on_owner(&instances[owner], mountable);
  wait_placed(&instances[owner], &id, &snapshot);
  let survivors: Vec<String> = instances
    .iter()
    .enumerate()
    .filter(|(index, _)| *index != owner)
    .map(|(_, instance)| instance.clone())
    .collect();
  assert_not_served(&survivors, &id);
  drop(daemons.remove(owner));

  // Retirement, takeover, serve.
  assert_retired(&wait_fleet_views(&survivors, 2, 1), &owner_host);
  let successor = wait_successor(&survivors, &id);
  read_on_successor(&successor, &id, mountable);

  drop(daemons);
  drop(scratch);
}

// --- T-4.13: the OCI namespace handoff (§4.6 A-9; AC-4.11; RQ-20). ---

/// Shape: how long one container run may take on a loaded box (the first emulated `linux/amd64`
/// alpine run measured 2026-09-14 exceeded 90 s with the box at the memory wall).
const CONTAINER_WAIT: Duration = Duration::from_secs(300);
/// Shape: how long `docker info` may take to answer before the runtime is called unreachable.
const DOCKER_INFO_WAIT: Duration = Duration::from_secs(60);
/// Shape: how many polls `slates unmount` is retried while the runtime's share holds the mount
/// point busy after the containers exited (measured 2026-09-14: the share holds every file a
/// container touched open beyond the container's lifetime — not released within 150 s — so the
/// plain unmount answers "Resource busy" and the guard's forced unmount ends the test).
const UNMOUNT_TRIES: u32 = 100;
/// Format: the image the container workload runs in — one small image (alpine 3.20, 12 MB).
const CONTAINER_IMAGE: &str = "alpine:3.20";
/// Format: the workload every side runs, as POSIX shell: `$1` is the root of the volume's view, `$2`
/// the side's tag. Create, write, read back, make a directory, rename, delete, show the directory
/// after the delete and remove it, list (without the AppleDouble `._*` sidecars the macOS NFS client
/// writes for host files' extended attributes, which the runtime's share refuses to show), then
/// read every other side's file and print every file's size.
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
ls -1 "$R" | grep -v '^\._' || true
echo "--- other"
for f in "$R"/*.txt; do
  case "$f" in *"/$T.txt") ;; *) printf '%s:' "$(basename "$f")"; cat "$f"; echo ;; esac
done
echo "--- sizes"
for f in "$R"/*.txt; do printf '%s %s\n' "$(basename "$f")" "$(wc -c < "$f" | tr -d ' ')"; done
"#;
/// Format: what the container sees at its destination before the workload: the filesystem type the
/// runtime mounted there (from the container's own mount table), recorded for the transport's record.
const CONTAINER_VIEW: &str = r#"
echo "--- view"
awk -v m="$1" '$5 == m { for (i = 7; i <= NF; i++) if ($i == "-") { print $(i+1) " " $(i+2); break } }' /proc/self/mountinfo || true
"#;
/// Format: the read-only side's probe: list, then try a write, which the runtime's `ro` bind refuses.
const READ_ONLY_PROBE: &str = r#"
R="$1"
echo "--- listing"
ls -1 "$R" | grep -v '^\._' || true
echo "--- write"
( printf 'x' > "$R/from-ro.txt" ) 2>&1 || echo "write refused"
"#;

/// Runs a command to completion within `wait`, killing it past the bound: (exit code, stdout,
/// stderr), or why it did not run.
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

/// The container runtime's server, as `docker info` states it, or why it is unreachable.
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

/// The gate: the live-mount flow's own (`SLATES_TEST_CLI=1`, `mount_nfs`) plus a reachable
/// runtime; the runtime's server description when the test may run, else the loud skip was printed.
fn container_leg_gate() -> Option<String> {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping T-4.13's container leg: set SLATES_TEST_CLI=1 to run it (a real kernel mount and a real container)"
    );
    return None;
  }
  if !mount_nfs_available() {
    eprintln!("skipping T-4.13's container leg: mount_nfs is not on this host (macOS/BSD only)");
    return None;
  }
  match docker_server() {
    Ok(server) => Some(server),
    Err(why) => {
      eprintln!("skipping T-4.13's container leg: the container runtime is unreachable: {why}");
      None
    }
  }
}

/// Runs `script` in a container over the runtime entry the daemon returned: the entry's source bound
/// at its destination with the entry's options, as the mounting user (the consumer, §4.13). The entry is
/// passed as the harness would pass it to its runtime: Docker's `--mount` form of the recipe — a
/// non-recursive bind (`bind-recursive=disabled`) with private propagation, read-only when the entry says
/// `ro` — never `-v`, which binds recursively and creates a missing source on the host (AUD-29-65/66);
/// `--mount` refuses a source that does not exist.
fn run_in_container(
  entry: &serde_json::Value,
  script: &str,
  tag: &str,
) -> Result<(i32, String, String), String> {
  let user = format!(
    "{}:{}",
    rustix::process::getuid().as_raw(),
    rustix::process::getgid().as_raw()
  );
  run_in_container_as(entry, script, tag, &user, &[])
}

/// [`run_in_container`] as the container identity `user` (`UID:GID`) with the supplementary groups `groups`.
fn run_in_container_as(
  entry: &serde_json::Value,
  script: &str,
  tag: &str,
  user: &str,
  groups: &[&str],
) -> Result<(i32, String, String), String> {
  let source = entry["source"].as_str().unwrap();
  let destination = entry["destination"].as_str().unwrap();
  let read_only = entry["options"]
    .as_array()
    .unwrap()
    .iter()
    .any(|o| o == "ro");
  let name = format!("slates-oci-{}-{tag}", std::process::id());
  let mut bind = format!(
    "type=bind,source={source},destination={destination},bind-recursive=disabled,bind-propagation=private"
  );
  if read_only {
    bind.push_str(",readonly");
  }
  let mut command = Command::new("docker");
  command.args(["run", "--rm", "--name", &name, "--user", user]);
  for group in groups {
    command.args(["--group-add", group]);
  }
  command.args([
    "--mount",
    &bind,
    CONTAINER_IMAGE,
    "sh",
    "-c",
    script,
    "sh",
    destination,
    tag,
  ]);
  let result = bounded(&mut command, CONTAINER_WAIT);
  if result.is_err() {
    let _ = Command::new("docker").args(["rm", "-f", &name]).output();
  }
  result
}

/// The workload on the host path, through the shell (no `std::fs`, R1).
fn run_on_host(root: &str, tag: &str) -> (i32, String, String) {
  bounded(
    Command::new("sh").args(["-c", WORKLOAD, "sh", root, tag]),
    CONTAINER_WAIT,
  )
  .unwrap()
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

/// `attach ID --oci-source MOUNT --oci-destination /work [--read|--write] --json`: the parsed reply.
fn attach_oci(instance: &str, id: &str, mount: &str, access: &str) -> serde_json::Value {
  let (code, out, err) = run(
    instance,
    &[
      "attach",
      id,
      access,
      "--oci-source",
      mount,
      "--oci-destination",
      "/work",
      "--json",
    ],
  );
  assert_eq!(code, 0, "attach in the OCI form: {err}");
  serde_json::from_str(out.trim()).unwrap()
}

/// The verified binding of a write attachment: a recursive read-write bind of the mount point, whose
/// evidence names the volume's export.
fn assert_verified_binding(binding: &serde_json::Value, mount: &str, volume_name: &str) {
  assert_eq!(binding["source"], mount, "the verified mount point");
  assert_eq!(binding["destination"], "/work");
  assert_eq!(binding["read_only"], false);
  assert_eq!(binding["evidence"]["fstype"], "nfs");
  assert_eq!(
    binding["evidence"]["mount_source"],
    format!("slates:/{volume_name}"),
    "the mount's source names the volume and carries no capability (§4.6 A-34)"
  );
  assert_eq!(binding["evidence"]["names_volume"], true);
  let entry = &binding["mount"];
  assert_eq!(entry["type"], "bind");
  assert_eq!(
    entry["options"],
    serde_json::json!(["bind", "rw", "private"])
  );
}

/// The transport's report on the reply: the bind offered read-write, with the host mount's
/// delete-while-open rule (the macOS NFS client silly-renames) that the container then meets.
fn assert_bind_capability(capability: &serde_json::Value) {
  assert_eq!(capability["transport"], "oci");
  assert_eq!(capability["supported"], true);
  assert_eq!(capability["read_write"], "read_write");
  assert_eq!(
    capability["sharing"]["delete_while_open"], "silly_renamed",
    "the report states the rule the container then meets"
  );
}

/// The write attachment's reply: an established container bind, verified, with its report.
fn assert_write_binding(attached: &serde_json::Value, mount: &str, volume_name: &str) {
  assert_eq!(attached["established"]["form"], "oci_bind");
  assert_verified_binding(&attached["established"]["binding"], mount, volume_name);
  assert_bind_capability(&attached["capability"]);
}

/// The two workload outputs agree: the container read the host's file byte for byte, its top-level
/// listing and sizes hold both sides' files, the host's own delete leaves nothing and its directory
/// goes, and the container's delete meets the reported rule — the runtime's share still holds the
/// file open, so the NFS client silly-renamed it to `.nfs.*` and the directory cannot be removed.
fn assert_workload_outputs_agree(host_out: &str, container_out: &str) {
  assert!(
    host_out.contains("hello from host") && host_out.contains("inner"),
    "the host workload: {host_out}"
  );
  assert!(
    container_out.contains("hello from container") && container_out.contains("inner"),
    "the container workload: {container_out}"
  );
  assert_eq!(
    section(host_out, "dir-after-delete"),
    vec!["rmdir ok"],
    "a delete by the host alone leaves nothing behind"
  );
  let after_delete = section(container_out, "dir-after-delete");
  assert!(
    after_delete.len() == 2
      && after_delete[0].starts_with(".nfs.")
      && after_delete[1].contains("Directory not empty"),
    "the container's delete met the silly-rename rule: {after_delete:?}"
  );
  assert_eq!(
    section(container_out, "other"),
    vec!["host.txt:hello from host"],
    "the container read the host's bytes"
  );
  assert_eq!(
    section(container_out, "listing"),
    vec!["container.txt", "dir-container", "host.txt"]
  );
  assert_eq!(
    section(container_out, "sizes"),
    vec!["container.txt 20", "host.txt 15"]
  );
}

/// The host lists the same names as the container did and reads the container's bytes — one copy.
fn assert_host_view_agrees(mount: &str) {
  let (code, host_view, _) = bounded(
    Command::new("sh").args([
      "-c",
      "ls -1 \"$1\" | grep -v '^\\._'; printf '%s' \"$(cat \"$1/container.txt\")\"; echo; wc -c < \"$1/container.txt\" | tr -d ' '",
      "sh",
      mount,
    ]),
    CONTAINER_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0);
  assert_eq!(
    host_view.lines().collect::<Vec<_>>(),
    vec![
      "container.txt",
      "dir-container",
      "host.txt",
      "hello from container",
      "20"
    ],
    "the host lists the same names and reads the container's bytes"
  );
}

/// `slates oci-check` of a binding's source against the identity its evidence named (AUD-29-66): the check a
/// harness runs immediately before its runtime binds the path. Its exit code and its output.
fn oci_check(instance: &str, binding: &serde_json::Value) -> (i32, String, String) {
  let evidence = &binding["established"]["binding"]["evidence"];
  run(
    instance,
    &[
      "oci-check",
      binding["established"]["binding"]["source"]
        .as_str()
        .unwrap(),
      &evidence["mount_id"].as_u64().unwrap().to_string(),
      &evidence["mount_device"].as_u64().unwrap().to_string(),
    ],
  )
}

/// The write attachment in the OCI form, the workload on the host path, then the same workload in
/// a real container over the returned entry; the writer's reply, or `None` when the runtime's file
/// sharing refused the mount point (the loud skip was printed).
fn bind_and_run_workloads(instance: &str, id: &str, path: &str) -> Option<serde_json::Value> {
  let writer = attach_oci(instance, id, path, "--write");
  assert_write_binding(&writer, path, "oci");
  let entry = writer["established"]["binding"]["mount"].clone();
  let (code, host_out, host_err) = run_on_host(path, "host");
  assert_eq!(code, 0, "the host workload: {host_err}");
  let script = format!("{CONTAINER_VIEW}{WORKLOAD}");
  // The harness checks the verified mount is still the one at the path just before its runtime binds it.
  let (code, _, err) = oci_check(instance, &writer);
  assert_eq!(
    code, 0,
    "the verified source is unchanged before the bind: {err}"
  );
  // And it asks the runtime that will bind it for its profile (AUD-29-67): this run is the evidence the
  // handshake names, so the profile it binds through must be the one named.
  let (code, profile, err) = run(instance, &["oci-runtime", "docker"]);
  assert_eq!(code, 0, "the runtime's profile holds evidence: {err}");
  assert!(profile.contains("evidence: T-4.13"), "{profile}");
  let (code, container_out, container_err) = run_in_container(&entry, &script, "container")
    .unwrap_or_else(|why| panic!("the container did not run: {why}"));
  if code != 0 && (container_err.contains("Mounts denied") || container_err.contains("not shared"))
  {
    eprintln!(
      "skipping T-4.13's container leg: the runtime's file sharing refused the mount point {path}: {}",
      container_err.trim()
    );
    return None;
  }
  assert_eq!(code, 0, "the container workload: {container_err}");
  eprintln!(
    "the container's view of /work: {:?}",
    section(&container_out, "view")
  );
  assert_workload_outputs_agree(&host_out, &container_out);
  assert_host_view_agrees(path);
  Some(writer)
}

/// A delete on the host is the container's view too (the name goes; the share's open handle keeps
/// the bytes as a `.nfs.*` entry `ls -1` does not show), and a read attachment is a read-only bind the
/// runtime enforces; the reader's reply.
fn assert_read_only_bind(instance: &str, id: &str, path: &str) -> serde_json::Value {
  let (code, _, err) = bounded(
    Command::new("rm").arg(format!("{path}/host.txt")),
    CONTAINER_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0, "{err}");
  let reader = attach_oci(instance, id, path, "--read");
  let read_only_entry = &reader["established"]["binding"]["mount"];
  assert_eq!(
    read_only_entry["options"],
    serde_json::json!(["bind", "ro", "private"])
  );
  assert_eq!(reader["established"]["binding"]["read_only"], true);
  let (code, _, err) = oci_check(instance, &reader);
  assert_eq!(
    code, 0,
    "the verified source is unchanged before the bind: {err}"
  );
  let (code, probe_out, probe_err) =
    run_in_container(read_only_entry, READ_ONLY_PROBE, "reader").unwrap();
  assert_eq!(code, 0, "the read-only probe: {probe_err}");
  assert_eq!(
    section(&probe_out, "listing"),
    vec!["container.txt", "dir-container"],
    "the host's delete is the container's view"
  );
  let write = section(&probe_out, "write").join("\n");
  assert!(
    write.contains("Read-only file system") && write.contains("write refused"),
    "the runtime enforces the read-only bind: {write}"
  );
  reader
}

/// A host path that is not the volume's mount point is refused typed, before any effect.
fn assert_not_a_mount_point_refused(instance: &str, id: &str) {
  let (code, _, err) = run(
    instance,
    &[
      "attach",
      id,
      "--oci-source",
      "/private",
      "--oci-destination",
      "/work",
    ],
  );
  assert_eq!(code, 1, "a refusal: {err}");
  assert!(
    err.contains("ChosenPathUnavailable") && err.contains("NotAMountPoint"),
    "{err}"
  );
}

/// Explicitly detach every binding created by the CLI workload. A missing binding is a failure:
/// it means the attachment did not survive for the consumer that received it.
fn detach_all(instance: &str, attachments: &[u64]) {
  for attachment in attachments {
    let (code, _, err) = run(instance, &["detach", &attachment.to_string()]);
    assert_eq!(code, 0, "detach: {err}");
  }
}

/// Wait for every completed CLI command to be retired. This SDK connection only observes;
/// both bindings were created by the real CLI, whose processes have already exited.
fn retire_cli_commands(observer: &mut slates_client::Client) {
  assert!(
    wait_for(|| {
      observer
        .daemon_status()
        .unwrap()
        .shards
        .iter()
        .map(|shard| u64::from(shard.clients))
        .sum::<u64>()
        == 1
    }),
    "only the observing connection remains after the commands exit"
  );
}

/// Restart this fixture's daemon under its live anchor, retaining the source mount and
/// its borrowers. The persistent observer must reconnect and see the replacement process.
fn restart_binding_daemon(observer: &mut slates_client::Client) {
  let previous = observer.daemon_status().unwrap().pid;
  let killed = Command::new("kill")
    .args(["-KILL", &previous.to_string()])
    .status()
    .unwrap();
  assert!(killed.success(), "kill the fixture's daemon");
  assert!(
    wait_for(|| observer.daemon_status().unwrap().pid != previous),
    "the anchor replaced the daemon"
  );
  assert!(
    observer.reconnects() > 0,
    "the observer crossed the daemon restart"
  );
}

/// A read-only source cannot grant a writable bind even to the volume owner.
fn assert_read_only_source_refuses_write_binding(instance: &str, id: &str, path: &str) {
  let (code, _, err) = run(instance, &["mount", id, path, "--read-only"]);
  assert_eq!(code, 0, "{err}");
  let (code, _, err) = run(
    instance,
    &[
      "attach",
      id,
      "--write",
      "--oci-source",
      path,
      "--oci-destination",
      "/work",
      "--json",
    ],
  );
  assert_eq!(code, 1, "a write binding of a read-only source must refuse");
  let failure: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
  assert_eq!(failure["error"]["kind"], "refused");
  assert!(
    failure["error"]["message"]
      .as_str()
      .unwrap()
      .contains("Forbidden"),
    "{err}"
  );
  assert_eq!(
    attachments_of(instance, id),
    "1",
    "the refused binding has no attachment"
  );
  let _reader = attach_oci(instance, id, path, "--read");
  assert_eq!(
    attachments_of(instance, id),
    "2",
    "a read binding remains usable"
  );
  unmount_and_check(instance, id, path);
}

/// AC-4.11 / T-4.13: the CLI hands off two bindings, exits, and is reaped. Both bindings
/// must remain until explicitly detached; an additional binding must end with its source mount.
#[test]
fn oci_bindings_outlive_the_cli_and_end_with_detach_or_their_source_mount() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() || !mount_nfs_available() {
    eprintln!("skipping OCI lifecycle: needs SLATES_TEST_CLI=1 and mount_nfs");
    return;
  }
  let instance = format!("cli-oci-life-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let mut observer = slates_client::Client::connect(&instance, deadlines).unwrap();
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "oci-life", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  mount_and_check(&instance, &id, &mount_point.path);
  let writer = attach_oci(&instance, &id, &mount_point.path, "--write");
  let reader = attach_oci(&instance, &id, &mount_point.path, "--read");
  retire_cli_commands(&mut observer);
  assert_eq!(
    attachments_of(&instance, &id),
    "3",
    "both bindings survive CLI retirement"
  );
  restart_binding_daemon(&mut observer);
  assert_eq!(
    attachments_of(&instance, &id),
    "3",
    "recovery keeps the source and both bindings"
  );
  detach_all(
    &instance,
    &[
      writer["attachment"].as_u64().unwrap(),
      reader["attachment"].as_u64().unwrap(),
    ],
  );
  assert_eq!(
    attachments_of(&instance, &id),
    "1",
    "explicit detach preserves the source mount"
  );
  roundtrip_a_file_through(&mount_point.path);
  let _dependent = attach_oci(&instance, &id, &mount_point.path, "--read");
  retire_cli_commands(&mut observer);
  assert_eq!(attachments_of(&instance, &id), "2");
  unmount_and_check(&instance, &id, &mount_point.path);
  assert_read_only_source_refuses_write_binding(&instance, &id, &mount_point.path);
  drop(observer);
  drop(mount_point);
  drop(anchor);
}

/// `slates unmount` after the containers ran, retried while the runtime's share holds the point
/// busy; whether the plain unmount succeeded within the bound (the guard forces it otherwise).
fn unmount_after_container(instance: &str, path: &str) -> bool {
  for _ in 0..UNMOUNT_TRIES {
    let (code, _, _) = run(instance, &["unmount", path]);
    if code == 0 {
      return true;
    }
    pause();
  }
  false
}

/// T-4.13 (AC-4.11, §4.6 A-9): a volume attached on the host — a real `slates mount` — is handed
/// to a real container by the host's OCI runtime as the bind the daemon's `attach --oci-source`
/// returned; the same filesystem workload runs on the host path and inside the container (create,
/// write, read back, rename, delete, list, a byte-identical read of the other side's file); the two
/// views agree byte for byte and in names and sizes; an edit inside the container is the host's edit
/// and a delete on the host is the container's (one copy, no third); a read attachment yields a
/// read-only bind the runtime enforces (`Read-only file system`); a host path that is not the
/// volume's mount point is refused typed (`ChosenPathUnavailable{NotAMountPoint}`); and the
/// container meets exactly the sharing rule the report states — the runtime's share holds files open
/// past the container's life, so a delete inside it silly-renames (`.nfs.*`) and blocks `rmdir` and
/// the plain unmount. Gated like the live mount flow (`SLATES_TEST_CLI=1`, `mount_nfs`), skipping
/// loudly where the runtime is unreachable or its file sharing refuses the mount point — saying
/// exactly what it said.
#[test]
fn an_oci_container_consumes_the_host_attachment_through_the_runtime_bind() {
  let Some(server) = container_leg_gate() else {
    return;
  };
  eprintln!("T-4.13 over {server}");
  let instance = format!("cli-oci-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(&instance, &["volume", "create", "oci", "--bounded", "8MiB"]);
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  let path = mount_point.path.clone();
  mount_and_check(&instance, &id, &path);
  #[cfg(target_os = "macos")]
  a_forged_unmount_ends_nothing(&instance, &id, "mounted", &path);

  let attachments = bind_and_run_workloads(&instance, &id, &path).map(|writer| {
    let reader = assert_read_only_bind(&instance, &id, &path);
    assert_not_a_mount_point_refused(&instance, &id);
    [
      writer["attachment"].as_u64().unwrap(),
      reader["attachment"].as_u64().unwrap(),
    ]
  });
  if let Some(attachments) = attachments {
    detach_all(&instance, &attachments);
    assert_eq!(
      attachments_of(&instance, &id),
      "1",
      "only the kernel mount's attachment remains after cleanup"
    );
  }
  let unmounted = unmount_after_container(&instance, &path);
  eprintln!(
    "plain unmount after the containers: {}",
    if unmounted {
      "succeeded"
    } else {
      "busy (the runtime's share holds the mount point); forcing"
    }
  );
  drop(mount_point);
  assert!(!is_mounted(&path), "the mount table no longer lists it");
  drop(anchor);
}

/// AUD-29-66. Do: mount a volume, attach the container form over it, check its source with `slates
/// oci-check` against the identity the attach's evidence named, unmount, and check again. Expect: the first
/// check passes; after the unmount the check refuses `SourceMissing` (exit 1), so a harness never hands its
/// runtime a bare directory where the verified mount was (`docker -v` would bind it, or create one). Gated
/// like the live mount flow (`SLATES_TEST_CLI=1`, `mount_nfs`); needs no container runtime.
#[test]
fn a_verified_container_source_is_checked_again_before_it_is_bound() {
  if std::env::var("SLATES_TEST_CLI").is_err() || !std::path::Path::new("/sbin/mount_nfs").exists()
  {
    eprintln!("SKIP: set SLATES_TEST_CLI=1 on a host with mount_nfs to run the source check");
    return;
  }
  let instance = format!("cli-oci-check-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "oci-check", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  let path = mount_point.path.clone();
  mount_and_check(&instance, &id, &path);
  let binding = attach_oci(&instance, &id, &path, "--write");
  let (code, before, err) = oci_check(&instance, &binding);
  assert_eq!(code, 0, "the source is the mount verified: {before}{err}");
  detach_all(&instance, &[binding["attachment"].as_u64().unwrap()]);
  let _ = unmount_after_container(&instance, &path);
  let (code, _, missing) = oci_check(&instance, &binding);
  drop(mount_point);
  drop(anchor);
  assert_eq!(code, 1, "a missing source is refused: {missing}");
  assert!(missing.contains("SourceMissing"), "{missing}");
}

/// T-2.14 / AUD-07: review a recovery plan through the real CLI, refuse missing authority,
/// then approve that exact plan through the anchor issuer and continue serving the retained catalog.
#[test]
fn recovery_approval_is_bound_to_the_reviewed_plan_and_anchor_issuer() {
  if cfg!(target_os = "linux") {
    eprintln!(
      "skipping named anchor issuer surface on Linux: exercised by the provisioned recovery key test"
    );
    return;
  }
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!("skipping recovery CLI flow: set SLATES_TEST_CLI=1");
    return;
  }
  let instance = format!("cli-recover-{}", std::process::id());
  let (_anchor, exports) = start_anchor_with_issuer_surface(&instance);
  assert_eq!(run(&instance, &["bootstrap", "root"]).0, 0);
  let (code, stdout, stderr) = run(&instance, &["recovery-plan", "root", "--json"]);
  assert_eq!(code, 0, "{stderr}");
  let plan: serde_json::Value = serde_json::from_str(&stdout).unwrap();
  let digest = plan["plan"].as_str().unwrap();
  let args = [
    "recover",
    "root",
    "--confirm",
    digest,
    "--fenced",
    "--accept-loss",
    "--json",
  ];
  assert_ne!(run(&instance, &args).0, 0, "no ambient authority");
  let (code, stdout, stderr) = run_as_issuer(&instance, &exports, &args);
  assert_eq!(code, 0, "{stderr}");
  let recovered: serde_json::Value = serde_json::from_str(&stdout).unwrap();
  assert_ne!(recovered["group"], plan["previous_group"]);
  assert_eq!(
    run_as_issuer(&instance, &exports, &args).0,
    0,
    "idempotent approval"
  );
  assert_eq!(run(&instance, &["volume", "list"]).0, 0);
}

/// T-2.14 / AUD-07: a separately invoked CLI can approve recovery on every platform using
/// a node-specific operator key; absence, a wrong key and malformed key bytes all refuse.
#[test]
#[allow(clippy::disallowed_methods)] // The operator's key file, in the test's build-output directory.
fn recovery_approval_uses_the_provisioned_node_key_across_processes() {
  let scratch = scratch_dir();
  let path = std::path::PathBuf::from(&scratch.path);
  let key_path = path.join("recovery.key");
  std::fs::write(&key_path, [91u8; 32]).unwrap();
  let environment = vec![(
    "SLATES_RECOVERY_KEY".to_owned(),
    key_path.to_string_lossy().into_owned(),
  )];
  let instance = format!("cli-node-recover-{}", std::process::id());
  let _anchor = start_anchor_with_environment(&instance, &environment);
  let (code, stdout, stderr) = run(&instance, &["recovery-plan", "root", "--json"]);
  assert_eq!(code, 0, "{stderr}");
  let plan: serde_json::Value = serde_json::from_str(&stdout).unwrap();
  let args = [
    "recover",
    "root",
    "--confirm",
    plan["plan"].as_str().unwrap(),
    "--fenced",
    "--accept-loss",
    "--json",
  ];
  assert_ne!(run(&instance, &args).0, 0, "no ambient recovery authority");
  std::fs::write(&key_path, [92u8; 32]).unwrap();
  assert_ne!(
    run_as_issuer(&instance, &environment, &args).0,
    0,
    "another node's key refuses"
  );
  std::fs::write(&key_path, [91u8; 33]).unwrap();
  assert_ne!(
    run_as_issuer(&instance, &environment, &args).0,
    0,
    "oversized key refuses"
  );
  std::fs::write(&key_path, [91u8; 32]).unwrap();
  let (code, stdout, stderr) = run_as_issuer(&instance, &environment, &args);
  assert_eq!(code, 0, "{stderr}");
  let recovered: serde_json::Value = serde_json::from_str(&stdout).unwrap();
  assert_ne!(recovered["group"], plan["previous_group"]);
  assert_eq!(
    run_as_issuer(&instance, &environment, &args).0,
    0,
    "idempotent approval"
  );
  assert_eq!(run(&instance, &["volume", "list"]).0, 0);
}

/// How many voters `instance`'s regional council has committed (`recovery-plan region`'s `voters`), zero
/// while it does not answer.
fn council_voters(instance: &str) -> usize {
  let (code, out, _) = run(instance, &["recovery-plan", "region", "--json"]);
  if code != 0 {
    return 0;
  }
  out
    .split("\"voters\":[")
    .nth(1)
    .and_then(|rest| rest.split(']').next())
    .map_or(0, |list| {
      list
        .split(',')
        .filter(|item| !item.trim().is_empty())
        .count()
    })
}

/// Which of `instances` report leading the regional council right now (`fleet_council_leads: true`).
fn council_leaders(instances: &[String]) -> Vec<usize> {
  instances
    .iter()
    .enumerate()
    .filter(|(_, instance)| {
      let (code, out, _) = run(instance, &["status"]);
      code == 0 && value_of(&out, "fleet_council_leads") == "true"
    })
    .map(|(index, _)| index)
    .collect()
}

/// Shape: the poll interval while timing a council handoff — a tenth of the daemon's 100 ms heartbeat period,
/// so the measurement resolves a single period.
const HANDOFF_POLL_MS: u64 = 10;

fn pause_for_handoff() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(HANDOFF_POLL_MS));
}

/// Starts three `slates daemon --fleet` processes named `cli-{tag}-{node}-{pid}` from one manifest, waits for
/// their mesh, and bootstraps the fresh fleet once on the lowest member: the scratch directory (kept alive by
/// the caller), the instances and the processes.
fn start_formed_fleet(tag: &str) -> (ScratchDir, Vec<String>, Vec<FleetProcess>) {
  let pid = std::process::id();
  let scratch = scratch_dir();
  for node in FLEET_NODES {
    mint_identity(&scratch.path, node);
  }
  let blocks = port_blocks();
  let bases: Vec<u16> = blocks.iter().map(|held| held.base).collect();
  let manifest = write_manifest(&scratch.path, &bases);
  let instances: Vec<String> = FLEET_NODES
    .iter()
    .map(|node| format!("cli-{tag}-{node}-{pid}"))
    .collect();
  let daemons: Vec<FleetProcess> = FLEET_NODES
    .iter()
    .zip(&instances)
    .zip(&blocks)
    .map(|((node, instance), held)| start_fleet_daemon(instance, &manifest, node, held))
    .collect();
  for instance in &instances {
    wait_answers(instance);
  }
  let views = wait_fleet_views(
    &instances,
    FLEET_NODES.len(),
    u32::try_from(FLEET_NODES.len() - 1).unwrap(),
  );
  assert_formed(&views);
  let bootstrap = views
    .iter()
    .enumerate()
    .min_by_key(|(_, view)| view.as_ref().unwrap().host.parse::<u64>().unwrap())
    .map(|(index, _)| index)
    .unwrap();
  let (code, _, error) = run(&instances[bootstrap], &["bootstrap", "root"]);
  assert_eq!(
    code, 0,
    "one explicit bootstrap of the fresh fleet: {error}"
  );
  (scratch, instances, daemons)
}

/// Waits for one council leader with every node a committed voter, and returns the leader's index. A fresh
/// fleet's council starts as the bootstrapper alone and promotes the others as they are admitted, so a stop
/// before that finds nobody to hand to (the drain reports `NoTarget`) and leaves learners that cannot elect.
fn wait_for_full_council(instances: &[String]) -> usize {
  let deadline = Instant::now() + FLEET_WAIT;
  let mut leaders = council_leaders(instances);
  while !(leaders.len() == 1 && council_voters(&instances[leaders[0]]) == FLEET_NODES.len())
    && Instant::now() < deadline
  {
    pause();
    leaders = council_leaders(instances);
  }
  assert_eq!(leaders.len(), 1, "one council leader: {leaders:?}");
  assert_eq!(
    council_voters(&instances[leaders[0]]),
    FLEET_NODES.len(),
    "every node votes in the council"
  );
  leaders[0]
}

/// When `instance` is a fleet node of this process, the evidence a refused mount on a forming fleet needs
/// (three CI failures on 2026-09-30 showed a council stuck at one voter of three members, with no view of the
/// leader): every node's status, then whether the council widens within the fleet wait and when. The test
/// still fails; this only says whether the refusal met a formation that was slow or one that stalled.
fn diagnose_fleet_council(instance: &str) -> String {
  let pid = std::process::id();
  let instances: Vec<String> = FLEET_NODES
    .iter()
    .map(|node| format!("cli-fleet-{node}-{pid}"))
    .collect();
  if !instances.iter().any(|fleet| fleet == instance) {
    return String::new();
  }
  let mut evidence = String::new();
  for node in &instances {
    let (_, status, status_err) = run(node, &["status"]);
    evidence.push_str(&format!(
      "\n--- {node} status at the refusal:\n{status}{status_err}"
    ));
  }
  let started = Instant::now();
  let widened = loop {
    let leaders = council_leaders(&instances);
    if let [leader] = leaders[..]
      && council_voters(&instances[leader]) == FLEET_NODES.len()
    {
      break Some(started.elapsed());
    }
    if started.elapsed() >= FLEET_WAIT {
      break None;
    }
    pause();
  };
  evidence.push_str(&match widened {
    Some(after) => format!("\n--- the council widened to every node {after:?} after the refusal"),
    None => format!(
      "\n--- the council did not widen to every node within {FLEET_WAIT:?} after the refusal"
    ),
  });
  evidence
}

/// Polls `survivors` from `asked` until exactly one leads the council (or the fleet wait passes): the leaders
/// then, and how long after `asked`.
fn time_the_successor(survivors: &[String], asked: Instant) -> (Vec<usize>, Duration) {
  let mut handed = council_leaders(survivors);
  while handed.len() != 1 && asked.elapsed() < FLEET_WAIT {
    pause_for_handoff();
    handed = council_leaders(survivors);
  }
  (handed, asked.elapsed())
}

/// Thesis §3.10 and the graceful drain (`docs/wip/research/consensus-enhancements.md` §3.2), by real
/// processes: three `slates daemon --fleet` processes form a fleet; the council leader's process is sent
/// `SIGTERM` (what an orchestrator sends a pod it deletes). It hands its leadership off before it goes: a
/// survivor leads within the election timeout a leader loss would wait out before anyone campaigns, and the
/// terminated daemon exits by itself, cleanly. Gated like the other process flows (`SLATES_TEST_CLI=1`).
#[test]
fn a_terminated_council_leader_process_hands_off_before_it_exits() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the fleet drain flow: set SLATES_TEST_CLI=1 to run it (three daemons; needs the machine to itself)"
    );
    return;
  }
  let (_scratch, instances, mut daemons) = start_formed_fleet("drain");
  let leader = wait_for_full_council(&instances);
  let (_, out, _) = run(&instances[leader], &["status"]);
  let base_periods: u64 = value_of(&out, "fleet_council_base_periods")
    .parse()
    .unwrap();
  let timeout = Duration::from_nanos(slates_server::daemon::HEARTBEAT_NS * base_periods);

  // SIGTERM the leader's daemon, as an orchestrator deleting its pod would.
  let mut terminated = daemons.remove(leader);
  let asked = Instant::now();
  let sent = Command::new("kill")
    .args(["-TERM", &terminated.child.id().to_string()])
    .status()
    .unwrap();
  assert!(sent.success(), "SIGTERM sent");
  let survivors: Vec<String> = instances
    .iter()
    .enumerate()
    .filter(|(index, _)| *index != leader)
    .map(|(_, instance)| instance.clone())
    .collect();
  let (handed, handoff) = time_the_successor(&survivors, asked);
  let exit = wait_exit(&mut terminated.child, FLEET_WAIT);
  eprintln!(
    "drain by SIGTERM: a survivor led after {handoff:?} (election timeout {timeout:?}); exit {exit:?}"
  );
  if handed.len() != 1 {
    // The evidence a stalled succession needs (CI run 36669682141 had none): each survivor's own view of
    // the council — term, lease, pre-elections and elections begun, pre-votes by refusal reason, voters —
    // and its links, printed before the assertion fails.
    for survivor in &survivors {
      let (code, status, stderr) = run(survivor, &["status"]);
      eprintln!("survivor {survivor} status (exit {code}):\n{status}{stderr}");
    }
  }
  assert_eq!(handed.len(), 1, "a survivor leads after the drain");
  assert!(
    handoff < timeout,
    "the handoff ({handoff:?}) beat the election timeout ({timeout:?}) a leader loss waits out"
  );
  assert_eq!(
    exit.map(|status| status.success()),
    Some(true),
    "the terminated daemon exited by itself, cleanly"
  );
}

/// Waits for `child` to exit, polling, up to `bound`; its status, or `None` if it had not exited by then.
fn wait_exit(child: &mut Child, bound: Duration) -> Option<std::process::ExitStatus> {
  let started = Instant::now();
  while started.elapsed() < bound {
    if let Ok(Some(status)) = child.try_wait() {
      return Some(status);
    }
    pause();
  }
  None
}

/// The pids of the live processes whose parent is `parent`, from `/proc/<pid>/stat` (its fourth field).
#[cfg(target_os = "linux")]
fn children_of(parent: u32) -> Vec<u32> {
  let mut children = Vec::new();
  for entry in std::fs::read_dir("/proc").unwrap().flatten() {
    let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
      continue;
    };
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
      continue;
    };
    // `pid (comm) state ppid ...`: the command may hold spaces, so split after its closing parenthesis.
    let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest).unwrap_or("");
    let ppid = after_comm
      .split_whitespace()
      .nth(1)
      .and_then(|f| f.parse::<u32>().ok());
    if ppid == Some(parent) {
      children.push(pid);
    }
  }
  children
}

/// What another process of the same user can learn about `pid`'s core dumps, from outside it: its core
/// size limits (soft, hard) and its core filter.
#[cfg(target_os = "linux")]
fn dump_exposure(pid: u32) -> (String, u32) {
  let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
  let core = limits
    .lines()
    .find(|line| line.starts_with("Max core file size"))
    .map(|line| {
      let fields: Vec<&str> = line.split_whitespace().collect();
      // `Max core file size <soft> <hard> bytes`.
      format!("{} {}", fields[4], fields[5])
    })
    .unwrap();
  let filter = std::fs::read_to_string(format!("/proc/{pid}/coredump_filter")).unwrap();
  (core, u32::from_str_radix(filter.trim(), 16).unwrap())
}

/// AUD-29-41 (dump exclusion, the real processes). Do: start the anchor, which spawns its daemon, and look
/// at both from outside, as another process of the same user. Expect: each has a core size limit of 0 soft
/// and hard and a core filter selecting no mapping class — so no core file is written, and a collector's
/// dump carries none of the segment's or a volume's memory. Before (Linux container, 2026-10-01), the anchor
/// ran with the session's limits (`0 unlimited`: the hard limit raisable) and the default filter (`0x23`:
/// anonymous private and shared memory included).
#[cfg(target_os = "linux")]
#[test]
fn the_anchor_and_its_daemon_exclude_themselves_from_core_dumps() {
  let instance = format!("cli-dumps-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let anchor_pid = anchor.child.id();
  let daemons = children_of(anchor_pid);
  assert_eq!(
    daemons.len(),
    1,
    "the anchor supervises one daemon: {daemons:?}"
  );
  for (role, pid) in [("anchor", anchor_pid), ("daemon", daemons[0])] {
    let (core, filter) = dump_exposure(pid);
    eprintln!("{role} {pid}: core limit {core}, core filter {filter:#x}");
    assert_eq!(
      core, "0 0",
      "{role}: no core file, and the limit cannot be raised"
    );
    assert_eq!(filter, 0, "{role}: a collector's dump carries no memory");
  }
  drop(anchor);
}

/// The kernel's mount table entry at `path` (Linux `mountinfo`): its filesystem type and source.
#[cfg(target_os = "linux")]
fn mountinfo_at(path: &str) -> Option<(String, String)> {
  let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
  table.lines().find_map(|line| {
    let (head, tail) = line.split_once(" - ")?;
    (head.split(' ').nth(4)? == path).then(|| {
      let mut fields = tail.split(' ');
      (
        fields.next().unwrap_or("").to_owned(),
        fields.next().unwrap_or("").to_owned(),
      )
    })
  })
}

/// Whether the Linux FUSE mount flow runs here: asked for (`SLATES_TEST_CLI=1`, a real kernel mount) and
/// possible (`fusermount3` and `/dev/fuse`); a loud skip otherwise.
#[cfg(target_os = "linux")]
fn linux_fuse_mount_runs() -> bool {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the Linux FUSE mount flow: set SLATES_TEST_CLI=1 to run it (a real kernel mount)"
    );
    return false;
  }
  let fuse = Command::new("sh")
    .args(["-c", "command -v fusermount3"])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false);
  if !fuse || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the Linux mount is FUSE");
    return false;
  }
  true
}

/// `slates mount ID DIR` on Linux reports the mount point, and the kernel lists a `fuse.slates` mount there
/// whose source names the volume's one attachment.
#[cfg(target_os = "linux")]
fn fuse_mount_and_check(instance: &str, id: &str, path: &str) {
  let (code, out, err) = run(instance, &["mount", id, path]);
  assert_eq!(code, 0, "slates mount: {err}");
  assert_eq!(value_of(&out, "mounted"), path);
  let (fstype, source) = mountinfo_at(path).expect("the kernel lists the mount");
  assert_eq!(fstype, "fuse.slates");
  assert!(
    source.starts_with("slates:"),
    "the source names the attachment: {source}"
  );
  assert_eq!(attachments_of(instance, id), "1");
}

/// AUD-29-64 (`slates mount` on Linux). Do: through the real binary against a real anchor-supervised
/// daemon, create a volume, `slates mount ID DIR`, write and read a file through the mount, then
/// `slates unmount DIR`. Expect: the command reports the mount point; the kernel lists a `fuse.slates`
/// mount there whose source names the volume's one attachment; the file reads back byte for byte; the
/// unmount (`fusermount3 -u`, no privilege) removes the mount and the daemon ends the attachment. Gated
/// like the macOS live mount (`SLATES_TEST_CLI=1`; a real kernel mount); skips loudly without FUSE.
#[cfg(target_os = "linux")]
#[test]
fn slates_mount_on_linux_serves_a_fuse_mount_and_unmount_ends_it() {
  if !linux_fuse_mount_runs() {
    return;
  }
  let instance = format!("cli-fuse-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "fused", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  fuse_mount_and_check(&instance, &id, &mount_point.path);
  roundtrip_a_file_through(&mount_point.path);
  let (code, _, err) = run(&instance, &["unmount", &mount_point.path]);
  assert_eq!(code, 0, "slates unmount: {err}");
  assert!(
    mountinfo_at(&mount_point.path).is_none(),
    "the mount is gone"
  );
  assert!(
    wait_for(|| attachments_of(&instance, &id) == "0"),
    "the daemon ended the mount's attachment"
  );
  drop(anchor);
}

/// AUD-29-64 (a FUSE mount outlives its daemon's crash). Do: `slates mount` a volume over FUSE, then `SIGKILL`
/// the daemon and wait for the anchor's restarted one. Expect: the restarted daemon has ended the dead mount's
/// attachment (its device died with the killed process) and unmounted the dead mount, whose source the kernel
/// table names as that attachment; before 2026-10-01 the record outlived the process and the mount stayed,
/// answering `ENOTCONN`. Gated like the Linux mount (`SLATES_TEST_CLI=1`, FUSE).
#[cfg(target_os = "linux")]
#[test]
fn a_fuse_mount_whose_daemon_was_killed_is_ended_by_the_restarted_daemon() {
  if !linux_fuse_mount_runs() {
    return;
  }
  // On a kernel that can resend what a dead daemon read, the anchor's held device keeps the mount (A-61), which
  // `a_fuse_mount_survives_its_daemons_kill_with_its_open_files_usable` proves; this is the older kernel's outcome.
  if kernel_resends_fuse_requests() {
    eprintln!(
      "SKIP: this kernel resends FUSE requests (Linux 6.9+), so the mount survives the kill (A-61)"
    );
    return;
  }
  let instance = format!("cli-fuse-crash-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "fusecrash", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  fuse_mount_and_check(&instance, &id, &mount_point.path);
  let killed = daemon_pid(&instance).expect("the daemon answers");
  let (code, _, err) = bounded(
    Command::new("kill").args(["-9", &killed.to_string()]),
    START_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0, "{err}");
  await_daemon(&instance, Some(killed), &[]);
  assert!(
    wait_for(|| attachments_of(&instance, &id) == "0"),
    "the restarted daemon ended the dead mount's attachment"
  );
  assert!(
    wait_for(|| mountinfo_at(&mount_point.path).is_none()),
    "the restarted daemon unmounted the dead mount"
  );
  drop(anchor);
}

/// Format: the first Linux release whose FUSE can resend the requests a dead daemon read (`FUSE_NOTIFY_RESEND`,
/// 7.40, Linux 6.9), as (major, minor).
#[cfg(target_os = "linux")]
const FIRST_RESENDING_KERNEL: (u32, u32) = (6, 9);

/// Whether this kernel's FUSE can resend what a dead daemon read: its release at or past
/// [`FIRST_RESENDING_KERNEL`], read from `uname -r` — the version is the observable stand-in for the `INIT` flag
/// the daemon sees.
#[cfg(target_os = "linux")]
fn kernel_resends_fuse_requests() -> bool {
  let release = Command::new("uname")
    .arg("-r")
    .output()
    .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    .unwrap_or_default();
  let mut parts = release
    .trim()
    .split(|c: char| !c.is_ascii_digit())
    .filter_map(|part| part.parse::<u32>().ok());
  match (parts.next(), parts.next()) {
    (Some(major), Some(minor)) => (major, minor) >= FIRST_RESENDING_KERNEL,
    _ => false,
  }
}

/// AC-3.4 / T-3.5 (A-61: the anchor holds a FUSE mount's device across a daemon restart). Do: through the real
/// binary against an anchor-supervised daemon, mount a volume over FUSE; open a file and write to it, and open a
/// second file, write it and unlink it (both descriptors kept); `SIGKILL` the daemon; write to the first file
/// through the same descriptor while the anchor restarts the daemon. Expect: the write waits through the window
/// and succeeds — no `ENOTCONN` — within the recovery budget; the first file holds both writes and the unlinked
/// one its bytes, read through the descriptors opened before the kill; the mount is still the same mount, its
/// attachment still recorded; the new daemon is a different process. Gated like the Linux mount, and to a
/// kernel that can resend (6.9+); skips loudly elsewhere.
#[cfg(target_os = "linux")]
#[test]
fn a_fuse_mount_survives_its_daemons_kill_with_its_open_files_usable() {
  use std::io::Write;
  let Some(held) = HeldMount::start("held") else {
    return;
  };
  let mut kept = held.open("kept");
  kept.write_all(b"before-").unwrap();
  let mut unlinked = held.open("unlinked");
  unlinked.write_all(b"orphan bytes").unwrap();
  let removed = Command::new("rm")
    .arg(format!("{}/unlinked", held.mount_point.path))
    .output()
    .unwrap();
  assert!(removed.status.success(), "unlink through the mount");
  let killed = kill_the_daemon(&held.instance);
  let killed_at = Instant::now();
  kept
    .write_all(b"after")
    .expect("the write waits for the restarted daemon and succeeds (no ENOTCONN)");
  let window = killed_at.elapsed();
  let restarted = await_daemon(&held.instance, Some(killed), &[]);
  assert_ne!(restarted, killed, "a new daemon serves the mount");
  assert!(
    window < Duration::from_nanos(slates_db::replay::RECOVERY_BUDGET_NS),
    "the window ({window:?}) is inside the recovery budget"
  );
  eprintln!("the held mount's window: {window:?}");
  assert_eq!(
    read_whole(&mut kept),
    b"before-after",
    "both writes, through the same descriptor"
  );
  assert_eq!(
    read_whole(&mut unlinked),
    b"orphan bytes",
    "the unlinked file still reads through its descriptor"
  );
  kept.sync_all().expect(
    "the unlink's publication captured the write before the kill, so nothing was lost to report",
  );
  held.assert_still_mounted_and_taken_over();
  drop(kept);
  drop(unlinked);
  held.unmount();
}

/// A FUSE mount of a fresh volume behind an anchor, for the takeover tests (A-61); `None` (a loud skip) where the
/// Linux mount does not run or the kernel cannot resend.
#[cfg(target_os = "linux")]
struct HeldMount {
  instance: String,
  id: String,
  mount_point: MountPoint,
  anchor: Option<AnchorProcess>,
}

#[cfg(target_os = "linux")]
impl HeldMount {
  fn start(tag: &str) -> Option<HeldMount> {
    if !linux_fuse_mount_runs() {
      return None;
    }
    if !kernel_resends_fuse_requests() {
      eprintln!(
        "SKIP: this kernel cannot resend FUSE requests (before Linux 6.9); the mount ends with its daemon"
      );
      return None;
    }
    let instance = format!("cli-fuse-{tag}-{}", std::process::id());
    let anchor = start_anchor(&instance);
    let (code, out, err) = run(&instance, &["volume", "create", tag, "--bounded", "64MiB"]);
    assert_eq!(code, 0, "{err}");
    let id = value_of(&out, "id");
    let mount_point = MountPoint {
      path: fresh_mount_point(),
    };
    fuse_mount_and_check(&instance, &id, &mount_point.path);
    Some(HeldMount {
      instance,
      id,
      mount_point,
      anchor: Some(anchor),
    })
  }

  /// `name` in the mount, created empty, opened for reading and writing.
  fn open(&self, name: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(true)
      .open(format!("{}/{name}", self.mount_point.path))
      .unwrap()
  }

  fn assert_still_mounted_and_taken_over(&self) {
    let (fstype, _) = mountinfo_at(&self.mount_point.path).expect("the mount is still there");
    assert_eq!(fstype, "fuse.slates");
    assert_eq!(
      attachments_of(&self.instance, &self.id),
      "1",
      "the mount's attachment is still recorded"
    );
    assert!(
      counter_of(&self.instance, "fuse.adopted") >= 1,
      "the restarted daemon reports the mount it took over"
    );
  }

  fn unmount(mut self) {
    let (code, _, err) = run(&self.instance, &["unmount", &self.mount_point.path]);
    assert_eq!(code, 0, "slates unmount: {err}");
    assert!(
      wait_for(|| attachments_of(&self.instance, &self.id) == "0"),
      "the unmount ends the attachment"
    );
    drop(self.anchor.take());
  }
}

/// `SIGKILL`s the instance's daemon; its pid.
#[cfg(target_os = "linux")]
fn kill_the_daemon(instance: &str) -> u32 {
  let killed = daemon_pid(instance).expect("the daemon answers");
  let (code, _, err) = bounded(
    Command::new("kill").args(["-9", &killed.to_string()]),
    START_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0, "{err}");
  killed
}

/// Every byte of `file`, read from its start through the same descriptor.
#[cfg(target_os = "linux")]
fn read_whole(file: &mut std::fs::File) -> Vec<u8> {
  use std::io::{Read, Seek, SeekFrom};
  file.seek(SeekFrom::Start(0)).unwrap();
  let mut bytes = Vec::new();
  file.read_to_end(&mut bytes).unwrap();
  bytes
}

/// The sum over shards of the daemon counter `name` in `slates status` (`shard N refused NAME: COUNT`).
#[cfg(target_os = "linux")]
fn counter_of(instance: &str, name: &str) -> u64 {
  let (_, out, _) = run(instance, &["status"]);
  let suffix = format!(" refused {name}: ");
  out
    .lines()
    .filter_map(|line| line.split_once(&suffix).map(|(_, count)| count))
    .filter_map(|count| count.trim().parse::<u64>().ok())
    .sum()
}

/// Shape: the kill cycles the in-flight test may take to catch a request the daemon read and had not answered;
/// inside the anchor's restart policy, which allows a restart per daemon start the recovery budget holds.
#[cfg(target_os = "linux")]
const IN_FLIGHT_ATTEMPTS: usize = 3;
/// Shape: the bytes of each write the in-flight test's writer makes (one page).
#[cfg(target_os = "linux")]
const IN_FLIGHT_WRITE: usize = 4096;
/// Shape: the writes the writer makes before the kill, so it is in its stride when the daemon dies.
#[cfg(target_os = "linux")]
const WRITES_BEFORE: usize = 64;
/// Shape: the writes the writer makes after the restarted daemon answers, so the file spans the takeover.
#[cfg(target_os = "linux")]
const WRITES_AFTER: usize = 64;
/// Format: the page values cycle through this many, a prime, so a page out of place shows.
#[cfg(target_os = "linux")]
const PAGE_VALUES: usize = 251;

/// What one kill cycle of the in-flight test saw: the pages it wrote, every one acknowledged, and whether its
/// descriptor's `fsync` after the takeover reported lost writes (`EIO`).
#[cfg(target_os = "linux")]
struct Cycle {
  pages: usize,
  loss_reported: bool,
}

/// One kill cycle of the in-flight test: a writer thread appends numbered pages through one descriptor while the
/// daemon is killed, stops after [`WRITES_AFTER`] writes past the restart, then `fsync`s that descriptor.
/// `first` numbers the cycle's first page. A write that fails is the test's failure.
#[cfg(target_os = "linux")]
fn write_through_a_kill(instance: &str, path: &str, first: usize) -> Cycle {
  use std::io::Write;
  let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
  let restarted = std::sync::atomic::AtomicBool::new(false);
  let progress = std::sync::atomic::AtomicUsize::new(0);
  let pages = std::thread::scope(|scope| {
    let writer = scope.spawn(|| {
      let mut written = 0usize;
      let mut after = 0usize;
      while after < WRITES_AFTER {
        let page = [u8::try_from((first + written) % PAGE_VALUES).unwrap_or(0); IN_FLIGHT_WRITE];
        file
          .write_all(&page)
          .expect("every write is acknowledged across the kill (none fails or hangs)");
        written += 1;
        progress.store(written, std::sync::atomic::Ordering::Release);
        if restarted.load(std::sync::atomic::Ordering::Acquire) {
          after += 1;
        }
      }
      written
    });
    // Kill only once the writer is in its stride, so a write is likely between the daemon's read and its reply.
    assert!(
      wait_for(|| progress.load(std::sync::atomic::Ordering::Acquire) >= WRITES_BEFORE),
      "the writer started"
    );
    let killed = kill_the_daemon(instance);
    await_daemon(instance, Some(killed), &[]);
    restarted.store(true, std::sync::atomic::Ordering::Release);
    writer.join().unwrap()
  });
  let loss_reported = match file.sync_all() {
    Ok(()) => false,
    Err(e) if e.raw_os_error() == Some(EIO) => true,
    Err(e) => panic!("fsync after the takeover: {e}"),
  };
  Cycle {
    pages,
    loss_reported,
  }
}

/// Format: `EIO`, the errno a lost write's `fsync` reports.
#[cfg(target_os = "linux")]
const EIO: i32 = 5;

/// AC-3.4 / T-3.5 with a request in flight (A-61: `FUSE_NOTIFY_RESEND`, the dirty log). Do: mount a volume over
/// FUSE; a writer thread appends numbered pages through one descriptor while the daemon is `SIGKILL`ed and on
/// past the restart, then `fsync`s; repeat until the restarted daemon reports a request the kernel resent (one the
/// dead daemon read and never answered), within [`IN_FLIGHT_ATTEMPTS`]. Expect: every write is acknowledged —
/// none fails or hangs, so the resent request was served (without the resend its writer would wait forever); the
/// file has every page's length; and no loss is silent: a cycle whose `fsync` succeeded wrote every page exactly,
/// and pages the dead daemon acknowledged but never published read as zero only in a cycle whose `fsync` answered
/// `EIO`, as Linux reports a writeback error. Gated like the takeover test.
#[cfg(target_os = "linux")]
#[test]
fn a_write_in_flight_at_the_daemons_kill_is_resent_and_no_loss_is_silent() {
  let Some(held) = HeldMount::start("resend") else {
    return;
  };
  let path = format!("{}/pages", held.mount_point.path);
  drop(held.open("pages"));
  let mut cycles = Vec::new();
  let mut pages = 0usize;
  while cycles.len() < IN_FLIGHT_ATTEMPTS && counter_of(&held.instance, "fuse.resent") == 0 {
    let cycle = write_through_a_kill(&held.instance, &path, pages);
    pages += cycle.pages;
    cycles.push(cycle);
  }
  let resent = counter_of(&held.instance, "fuse.resent");
  let mut reader = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
  let bytes = read_whole(&mut reader);
  drop(reader);
  let reported = cycles.iter().filter(|cycle| cycle.loss_reported).count();
  eprintln!(
    "{} cycles, {resent} resent, {reported} reported lost writes",
    cycles.len()
  );
  assert!(
    resent >= 1,
    "a kill caught a request in flight in {} attempts",
    cycles.len()
  );
  assert_eq!(
    bytes.len(),
    pages * IN_FLIGHT_WRITE,
    "every acknowledged page's length"
  );
  assert_no_silent_loss(&bytes, &cycles);
  held.unmount();
}

/// Every page is exact, except that a cycle whose `fsync` answered `EIO` may hold zero pages — the writes the dead
/// daemon acknowledged and never published, which it reported. A wrong page anywhere else is a silent loss.
#[cfg(target_os = "linux")]
fn assert_no_silent_loss(bytes: &[u8], cycles: &[Cycle]) {
  let mut number = 0usize;
  let mut pages = bytes.chunks(IN_FLIGHT_WRITE);
  for cycle in cycles {
    for _ in 0..cycle.pages {
      let page = pages.next().unwrap_or(&[]);
      let value = u8::try_from(number % PAGE_VALUES).unwrap_or(0);
      let exact = page.iter().all(|byte| *byte == value);
      let reported_hole = cycle.loss_reported && page.iter().all(|byte| *byte == 0);
      assert!(
        exact || reported_hole,
        "page {number} is wrong and no fsync reported a loss"
      );
      number += 1;
    }
  }
}

/// Shape: container identities other than the mounting user's — root, an ordinary Linux first user, and the
/// mounting user with a supplementary group no host account holds (AUD-29-74).
const OTHER_IDENTITIES: [(&str, &str, &[&str]); 3] = [
  ("root", "0:0", &[]),
  ("first-user", "1000:1000", &[]),
  ("extra-group", "", &["12345"]),
];
/// Format: what each identity does in the container: name itself, make a file and a private directory, and
/// print the owner it sees and the directory's mode.
const IDENTITY_SCRIPT: &str = r#"R="$1"; T="$2"
printf x > "$R/by-$T"
mkdir "$R/d-$T" && chmod 700 "$R/d-$T"
echo "--- seen"
stat -c '%u:%g' "$R/by-$T"
echo "--- mode"
stat -c '%a' "$R/d-$T"
"#;

/// The host's view of `path`: `owner:group`, through the host mount.
fn host_owner(path: &str) -> String {
  let (code, out, err) = bounded(
    Command::new("stat").args(["-f", "%u:%g", path]),
    CONTAINER_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0, "{err}");
  out.trim().to_owned()
}

/// One identity under `host_user_through_share`: the container as `user` with `groups` writes over the bind,
/// sees its own ids on what it made and keeps a 0700 directory 0700, and the host sees both as `me`.
fn assert_host_user_through_share(
  entry: &serde_json::Value,
  path: &str,
  me: &str,
  tag: &str,
  user: &str,
  groups: &[&str],
) {
  let (code, out, err) = run_in_container_as(entry, IDENTITY_SCRIPT, tag, user, groups)
    .unwrap_or_else(|why| panic!("the container did not run: {why}"));
  assert_eq!(code, 0, "{tag} wrote through the bind: {err}");
  assert_eq!(section(&out, "seen"), [user], "{tag} sees its own ids");
  assert_eq!(section(&out, "mode"), ["700"], "{tag}'s private directory");
  for object in [format!("{path}/by-{tag}"), format!("{path}/d-{tag}")] {
    assert_eq!(host_owner(&object), me, "{object} on the host");
  }
}

/// AUD-29-74. Do: through Docker Desktop (the profile with evidence), ask the handshake for the profile's
/// identity rule, then run containers as root, as 1000:1000, and as the mounting user with an extra group;
/// each makes a file and a 0700 directory over the bind. Expect: the handshake states
/// `host_user_through_share`, and the rule holds as stated — every identity writes, each container sees its
/// own ids on what it made, the 0700 directory keeps 0700, and the host sees every object as the mounting
/// user: container ids are not forwarded, so the attachment's capability is the authority. Gated like
/// T-4.13 (`SLATES_TEST_CLI=1`, `mount_nfs`, a reachable runtime).
#[test]
fn a_containers_identity_reaches_the_export_as_its_profile_states() {
  let Some(server) = container_leg_gate() else {
    return;
  };
  eprintln!("AUD-29-74 over {server}");
  let instance = format!("cli-oci-id-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "ocid", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let mount_point = MountPoint {
    path: fresh_mount_point(),
  };
  let path = mount_point.path.clone();
  mount_and_check(&instance, &id, &path);
  let writer = attach_oci(&instance, &id, &path, "--write");
  let entry = writer["established"]["binding"]["mount"].clone();
  let (code, profile, err) = run(&instance, &["oci-runtime", "docker"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    profile.contains("identity: host_user_through_share"),
    "{profile}"
  );
  let me = format!(
    "{}:{}",
    rustix::process::getuid().as_raw(),
    rustix::process::getgid().as_raw()
  );
  for (tag, user, groups) in OTHER_IDENTITIES {
    let user = if user.is_empty() { me.as_str() } else { user };
    assert_host_user_through_share(&entry, &path, &me, tag, user, groups);
  }

  detach_all(&instance, &[writer["attachment"].as_u64().unwrap()]);
  let _ = unmount_after_container(&instance, &path);
  drop(mount_point);
  drop(anchor);
}

/// Whether a live kernel mount runs here: macOS's NFS-loopback mount or Linux's FUSE mount, each gated as its
/// own flow is (`SLATES_TEST_CLI=1`, the host's mount mechanism present); a loud skip otherwise.
fn live_mount_runs() -> bool {
  #[cfg(target_os = "linux")]
  {
    linux_fuse_mount_runs()
  }
  #[cfg(not(target_os = "linux"))]
  {
    if std::env::var_os("SLATES_TEST_CLI").is_none() || !mount_nfs_available() {
      eprintln!("SKIP: a live mount needs SLATES_TEST_CLI=1 and mount_nfs");
      return false;
    }
    true
  }
}

/// Runs `script` under `sh` and returns its standard output, failing the test if it fails.
fn shell(script: &str) -> String {
  let out = Command::new("sh").args(["-c", script]).output().unwrap();
  assert!(
    out.status.success(),
    "{script}: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  String::from_utf8_lossy(&out.stdout).into_owned()
}

/// AUD-29-76 (`slates mount --subtree`). Do: through the real binary against a real anchor-supervised daemon,
/// mount a volume whole and make `shared/g` and `private/secret` through it; `slates mount ID DIR --subtree
/// /shared`; list and read the scoped mount, write `h` through it, then unmount both. Expect: the scoped mount
/// lists `g` alone and serves its bytes, the write lands in `shared` as the whole mount sees it, `private` is
/// nowhere beneath it; `--subtree` naming a file is refused, nothing mounted. Runs over macOS's NFS mount
/// and Linux's FUSE mount alike; before 2026-10-01 no mount could present less than the whole volume.
#[test]
fn slates_mount_subtree_presents_one_directory_of_the_volume() {
  if !live_mount_runs() {
    return;
  }
  let instance = format!("cli-subtree-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "scoped", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let whole = MountPoint {
    path: fresh_mount_point(),
  };
  let (code, _, err) = run(&instance, &["mount", &id, &whole.path]);
  assert_eq!(code, 0, "slates mount: {err}");
  let root = &whole.path;
  shell(&format!(
    "mkdir {root}/shared {root}/private && printf inside > {root}/shared/g && printf hidden > {root}/private/secret"
  ));
  let scoped = MountPoint {
    path: fresh_mount_point(),
  };
  the_subtree_mount_presents_shared(&instance, &id, &scoped.path, root);
  let refused = MountPoint {
    path: fresh_mount_point(),
  };
  let (code, _, _) = run(
    &instance,
    &["mount", &id, &refused.path, "--subtree", "/private/secret"],
  );
  assert_ne!(code, 0, "a file is no subtree");
  assert!(!is_mounted(&refused.path), "nothing was mounted");
  for point in [&scoped.path, &whole.path] {
    let (code, _, err) = run(&instance, &["unmount", point]);
    assert_eq!(code, 0, "slates unmount: {err}");
  }
  drop(refused);
  drop(scoped);
  drop(whole);
  drop(anchor);
}

/// `slates mount ID INNER --subtree /shared` mounts `shared` alone: it lists `g`, serves its bytes, and a write
/// through it lands in `shared` as the whole mount at `root` sees it.
fn the_subtree_mount_presents_shared(instance: &str, id: &str, inner: &str, root: &str) {
  let (code, out, err) = run(instance, &["mount", id, inner, "--subtree", "/shared"]);
  assert_eq!(code, 0, "slates mount --subtree: {err}");
  assert_eq!(value_of(&out, "mounted"), inner);
  assert_eq!(shell(&format!("ls -A {inner}")), "g\n");
  assert_eq!(shell(&format!("cat {inner}/g")), "inside");
  shell(&format!("printf landed > {inner}/h"));
  assert_eq!(shell(&format!("cat {root}/shared/h")), "landed");
}

/// Format: lists the bind (`$1`) and writes one file through it, from inside the container.
const SUBTREE_SCRIPT: &str =
  "ls -A \"$1\" && cat \"$1/g\" && echo && printf from-container > \"$1/c\"";

/// AUD-29-76 (a subtree through the container bind). Do: through Docker Desktop, mount a volume whole and make
/// `shared/g` and `private/secret`; mount `/shared` alone (`--subtree`); bind that mount into a container (the OCI
/// form) and, inside it, list the bind, read `g`, write `c`. Expect: the bind is admitted on the scoped mount;
/// the container sees `g` alone and its bytes, never `private`; its write lands in `shared` as the whole mount
/// sees it. Before 2026-10-01 a container could be given only the whole volume (a bind of a directory inside a
/// mount is refused `NotAMountPoint`). Gated like T-4.13.
#[test]
fn a_container_bound_to_a_subtree_mount_sees_only_that_directory() {
  let Some(server) = container_leg_gate() else {
    return;
  };
  eprintln!("AUD-29-76 subtree bind over {server}");
  let instance = format!("cli-oci-sub-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "ocisub", "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let whole = MountPoint {
    path: fresh_mount_point(),
  };
  mount_and_check(&instance, &id, &whole.path);
  let root = &whole.path;
  shell(&format!(
    "mkdir {root}/shared {root}/private && printf inside > {root}/shared/g && printf hidden > {root}/private/secret"
  ));
  let scoped = MountPoint {
    path: fresh_mount_point(),
  };
  let (code, _, err) = run(
    &instance,
    &["mount", &id, &scoped.path, "--subtree", "/shared"],
  );
  assert_eq!(code, 0, "slates mount --subtree: {err}");
  let binding = attach_oci(&instance, &id, &scoped.path, "--write");
  let entry = binding["established"]["binding"]["mount"].clone();
  let (code, out, err) = run_in_container(&entry, SUBTREE_SCRIPT, "subtree")
    .unwrap_or_else(|why| panic!("the container did not run: {why}"));
  assert_eq!(code, 0, "the container worked through the bind: {err}");
  assert_eq!(out, "g\ninside\n", "the container sees the subtree alone");
  assert_eq!(shell(&format!("cat {root}/shared/c")), "from-container");
  detach_all(&instance, &[binding["attachment"].as_u64().unwrap()]);
  let _ = unmount_after_container(&instance, &scoped.path);
  let _ = unmount_after_container(&instance, &whole.path);
  drop(scoped);
  drop(whole);
  drop(anchor);
}

/// The Linux container leg's gate: a live FUSE mount (`SLATES_TEST_CLI=1`, `fusermount3`, `/dev/fuse`) and a
/// reachable runtime; the runtime's server description, else the loud skip was printed.
#[cfg(target_os = "linux")]
fn linux_container_gate() -> Option<String> {
  if !linux_fuse_mount_runs() {
    return None;
  }
  // The operator's grant a shared mount needs; read, never written (the CI lane sets it, Ada 2026-10-01).
  #[allow(clippy::disallowed_methods)] // a host configuration file, read only (R1)
  let granted = std::fs::read_to_string("/etc/fuse.conf")
    .is_ok_and(|conf| conf.lines().any(|line| line.trim() == "user_allow_other"));
  if !granted {
    eprintln!(
      "SKIP: the Linux container leg needs user_allow_other in /etc/fuse.conf (an operator's grant)"
    );
    return None;
  }
  match docker_server() {
    Ok(server) => Some(server),
    Err(why) => {
      eprintln!("SKIP: the Linux container leg needs a reachable runtime: {why}");
      None
    }
  }
}

/// `owner:group` of `path` as the Linux host sees it.
#[cfg(target_os = "linux")]
fn linux_owner(path: &str) -> String {
  let (code, out, err) = bounded(
    Command::new("stat").args(["-c", "%u:%g", path]),
    CONTAINER_WAIT,
  )
  .unwrap();
  assert_eq!(code, 0, "{err}");
  out.trim().to_owned()
}

/// Format: a foreign identity's attempt: make a file in the bind's root, then read a 0700 directory it does not
/// own; each prints what the kernel answered.
#[cfg(target_os = "linux")]
const FOREIGN_SCRIPT: &str = r#"R="$1"
( printf x > "$R/by-foreign" ) 2>/dev/null && echo "--- create ok" || echo "--- create refused"
cat "$R/owners-only/s" 2>/dev/null && echo "--- read ok" || echo "--- read refused"
"#;

/// Format: a hard link's second name, after the first is removed, read at once, a hundred times.
#[cfg(target_os = "linux")]
const HARD_LINK_SCRIPT: &str = r#"R="$1"; fails=0
for i in $(seq 1 100); do
  echo x > "$R/a$i"; ln "$R/a$i" "$R/b$i"; rm "$R/a$i"
  cat "$R/b$i" >/dev/null 2>&1 || fails=$((fails+1)); rm -f "$R/b$i"
done
echo "--- failed"; echo "$fails"
"#;

/// The bind is refused `MountNotShared` over a FUSE mount the daemon made for its user alone.
#[cfg(target_os = "linux")]
fn an_unshared_mount_is_no_bind_source(instance: &str, id: &str) {
  let point = MountPoint {
    path: fresh_mount_point(),
  };
  let (code, _, err) = run(instance, &["mount", id, &point.path]);
  assert_eq!(code, 0, "slates mount: {err}");
  let (code, out, err) = run(
    instance,
    &[
      "attach",
      id,
      "--write",
      "--oci-source",
      &point.path,
      "--oci-destination",
      "/work",
    ],
  );
  assert_ne!(code, 0, "an unshared mount is no bind source: {out}");
  assert!(err.contains("MountNotShared"), "{err}");
  let (code, _, err) = run(instance, &["unmount", &point.path]);
  assert_eq!(code, 0, "{err}");
}

/// AUD-29-67/74 (the Linux profile, T-4.13 on Linux). Do: on a Linux host whose operator grants
/// `user_allow_other`, mount a volume with `slates mount --shared`; ask the runtime handshake for its profile;
/// bind the mount into containers (the OCI form, the exact entry `attach --oci` returns); as root make a file and
/// read the mounting user's 0700 directory; as a foreign 2000:2000 try both; as the mounting user make its own;
/// link a file, remove its first name and read the second, a hundred times; bind a mount made without
/// `--shared`. Expect: the handshake states `container_ids_as_host_ids`; root's file is 0:0 on the host and it
/// reads the private directory; the foreign id is refused both; the mounting user's file is its own; every second
/// name is served at once; the unshared mount is refused `MountNotShared`. Before 2026-10-01 the Linux bind was
/// refused `ContainerWorkloadUnproven`. Gated on a live FUSE mount and a reachable runtime; skips loudly.
#[cfg(target_os = "linux")]
#[test]
fn a_linux_container_reaches_the_shared_mount_as_its_own_ids() {
  let Some(server) = linux_container_gate() else {
    return;
  };
  eprintln!("the Linux profile over {server}");
  let instance = format!("cli-linux-oci-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "linuxoci", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let point = MountPoint {
    path: fresh_mount_point(),
  };
  let path = point.path.clone();
  let (code, _, err) = run(&instance, &["mount", &id, &path, "--shared"]);
  assert_eq!(code, 0, "slates mount --shared: {err}");
  shell(&format!(
    "mkdir -m 700 {path}/owners-only && printf secret > {path}/owners-only/s"
  ));
  let (code, profile, err) = run(&instance, &["oci-runtime", "docker"]);
  assert_eq!(code, 0, "{err}");
  assert!(
    profile.contains("identity: container_ids_as_host_ids"),
    "{profile}"
  );
  let writer = attach_oci(&instance, &id, &path, "--write");
  let entry = writer["established"]["binding"]["mount"].clone();
  assert_linux_identities(&entry, &path);
  let (code, out, err) = run_in_container(&entry, HARD_LINK_SCRIPT, "links").unwrap();
  assert_eq!(code, 0, "{err}");
  assert_eq!(
    section(&out, "failed"),
    ["0"],
    "every second name served at once"
  );
  detach_all(&instance, &[writer["attachment"].as_u64().unwrap()]);
  let _ = unmount_after_container(&instance, &path);
  an_unshared_mount_is_no_bind_source(&instance, &id);
  drop(point);
  drop(anchor);
}

/// The measured identity rule, by use: root bypasses the bits, a foreign id meets them, the mounting user owns
/// what it makes.
#[cfg(target_os = "linux")]
fn assert_linux_identities(entry: &serde_json::Value, path: &str) {
  let (code, out, err) = run_in_container_as(entry, IDENTITY_SCRIPT, "root", "0:0", &[]).unwrap();
  assert_eq!(code, 0, "root wrote through the bind: {err}");
  assert_eq!(section(&out, "seen"), ["0:0"]);
  assert_eq!(
    linux_owner(&format!("{path}/by-root")),
    "0:0",
    "root's file on the host"
  );
  let (_, out, _) = run_in_container_as(
    entry,
    "cat \"$1/owners-only/s\"; echo",
    "root-reads",
    "0:0",
    &[],
  )
  .unwrap();
  assert!(
    out.contains("secret"),
    "root reads the 0700 directory: {out}"
  );
  let (_, out, _) =
    run_in_container_as(entry, FOREIGN_SCRIPT, "foreign", "2000:2000", &[]).unwrap();
  assert!(out.contains("--- create refused"), "{out}");
  assert!(out.contains("--- read refused"), "{out}");
  let me = format!(
    "{}:{}",
    rustix::process::getuid().as_raw(),
    rustix::process::getgid().as_raw()
  );
  let (code, out, err) = run_in_container_as(entry, IDENTITY_SCRIPT, "me", &me, &[]).unwrap();
  assert_eq!(code, 0, "{err}");
  assert_eq!(section(&out, "seen"), [me.as_str()]);
  assert_eq!(linux_owner(&format!("{path}/by-me")), me);
}
