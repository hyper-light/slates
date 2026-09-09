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
/// Shape: the pause between polls of a starting or stopping daemon.
const POLL_MS: u64 = 20;
/// Shape: shards for the test daemon.
const SHARDS: &str = "2";

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
}

impl Drop for AnchorProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// Starts the anchor and waits until a verb is answered.
fn start_anchor(instance: &str) -> AnchorProcess {
  let child = slates()
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
  let anchor = AnchorProcess { child };
  let started = Instant::now();
  loop {
    let (code, _, _) = run(instance, &["volume", "list"]);
    if code == 0 {
      return anchor;
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
    let _ = Command::new("umount").arg(&self.path).output();
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// A fresh, user-owned mount-point directory (`mktemp -d`), resolved to its real path so it matches
/// what the kernel records in the mount table (`/var/folders/...` is a symlink to `/private/var/...`
/// on macOS). `std::fs::canonicalize` is a read, not a write, so it is outside the R1 wall; the
/// directory itself is made by `mktemp`, not `std::fs::create_dir`.
fn fresh_mount_point() -> String {
  let out = Command::new("mktemp").arg("-d").output().unwrap();
  assert!(out.status.success(), "mktemp -d");
  let raw = String::from_utf8_lossy(&out.stdout).trim().to_owned();
  std::fs::canonicalize(&raw)
    .unwrap()
    .to_string_lossy()
    .into_owned()
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

/// `slates mount ID DIR` reports the path and the kernel mount table lists a real NFS mount there.
fn mount_and_check(instance: &str, id: &str, path: &str) {
  let (code, out, err) = run(instance, &["mount", id, path]);
  assert_eq!(code, 0, "slates mount failed: {err}");
  assert_eq!(
    value_of(&out, "mounted"),
    path,
    "the command reports the path it mounted"
  );
  assert!(is_mounted(path), "the kernel mount table lists the mount");
}

/// A file written through the mount reads back byte for byte — the bytes travel host write → NFS →
/// the slates volume → NFS → host read. The write side avoids `std::fs` (R1) through the shell; the
/// read side is a separate `cat` process, a fresh READ across the mount rather than a page-cache echo.
fn roundtrip_a_file_through(path: &str) {
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

/// `slates unmount DIR` removes the mount (a pure `umount`, no daemon needed).
fn unmount_and_check(instance: &str, path: &str) {
  let (code, _out, err) = run(instance, &["unmount", path]);
  assert_eq!(code, 0, "slates unmount failed: {err}");
  assert!(
    !is_mounted(path),
    "the kernel mount table no longer lists it"
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
  roundtrip_a_file_through(&mount_point.path);
  unmount_and_check(&instance, &mount_point.path);

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

/// `--json` makes the read verbs emit the MCP JSON schema (§4.12, GAP-A9-10 "consistent JSON" — the
/// CLI and the MCP surface share one definition): `status ID --json` and `status --json` return one
/// JSON object, `volume list --json` a JSON array, each carrying the volume's real fields. Gated like
/// the anchor+daemon flow (it needs a daemon), skipping loudly without `SLATES_TEST_CLI`.
#[test]
fn the_read_verbs_emit_json_with_the_json_flag() {
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
  json_error(&instance);
  drop(anchor);
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
