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

/// A scratch directory for the manifest and the DER files (`mktemp -d`, named with the process id),
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
  let out = Command::new("mktemp")
    .args(["-d", "-t", &format!("slates-fleet-{}", std::process::id())])
    .output()
    .unwrap();
  assert!(out.status.success(), "mktemp -d");
  ScratchDir {
    path: String::from_utf8_lossy(&out.stdout).trim().to_owned(),
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

/// A base port whose `count` consecutive UDP loopback ports are all free right now — bound together so
/// the OS confirms the whole block, dropped before the daemons bind them — searched upward from `from`
/// within the attempt bound. Tests may use `std::net` (as the server's fleet tests do); the daemon never
/// links it (R1).
fn free_port_block(from: u16, count: u16) -> u16 {
  let mut base = from;
  for _ in 0..PORT_ATTEMPTS {
    let sockets: Vec<Option<std::net::UdpSocket>> = (0..count)
      .map(|k| {
        base
          .checked_add(k)
          .and_then(|port| std::net::UdpSocket::bind(("127.0.0.1", port)).ok())
      })
      .collect();
    if sockets.iter().all(Option::is_some) {
      return base;
    }
    base = base.checked_add(count).unwrap_or(PORT_FLOOR);
  }
  panic!("no block of {count} free loopback UDP ports found from {from}");
}

/// One free port pair per node, disjoint, from a pid-derived start.
fn port_blocks() -> Vec<u16> {
  let block = PORTS_PER_NODE;
  let offset = std::process::id() % u32::from(PORT_SPAN);
  let start = u16::try_from(u32::from(PORT_FLOOR) + offset).unwrap();
  let mut bases = Vec::with_capacity(FLEET_NODES.len());
  let mut from = start;
  for _ in FLEET_NODES {
    let base = free_port_block(from, block);
    bases.push(base);
    from = base.checked_add(block).unwrap_or(PORT_FLOOR);
  }
  bases
}

/// Starts `slates daemon --fleet MANIFEST --node NODE` alone (no anchor) at `instance`.
fn start_fleet_daemon(instance: &str, manifest: &str, node: &str) -> FleetProcess {
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
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  FleetProcess { child }
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
    // A clean formation: the serve sockets routed every packet to its session, dropped nothing, refused
    // no dialer, replaced no session — and the daemon refused nothing.
    assert_eq!(
      view.dropped, "unknown_id=0 inbox_full=0 refused=0 replaced=0",
      "the serve sockets dropped nothing forming the fleet: {views:?}"
    );
    assert!(
      view.refusals.is_empty(),
      "nothing refused forming the fleet: {views:?}"
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
  let (code, out, err) = run(
    instance,
    &["volume", "create", FLEET_VOLUME, "--bounded", "8MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  if mountable {
    let mount_point = MountPoint {
      path: fresh_mount_point(),
    };
    mount_and_check(instance, &id, &mount_point.path);
    write_payload_through(&mount_point.path);
    unmount_and_check(instance, &mount_point.path);
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
/// its id, named and placed) or the fleet wait passes; returns the successor's instance.
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
    assert!(
      Instant::now() < deadline,
      "a survivor took the volume over and serves it within the fleet wait"
    );
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
  unmount_and_check(instance, &mount_point.path);
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
  let manifest = write_manifest(&scratch.path, &port_blocks());
  let instances: Vec<String> = FLEET_NODES
    .iter()
    .map(|node| format!("cli-fleet-{node}-{pid}"))
    .collect();
  let mut daemons: Vec<FleetProcess> = FLEET_NODES
    .iter()
    .zip(&instances)
    .map(|(node, instance)| start_fleet_daemon(instance, &manifest, node))
    .collect();
  for instance in &instances {
    wait_answers(instance);
  }

  // Formation: one manifest, three processes, one fleet.
  let hosts = assert_formed(&wait_fleet_views(
    &instances,
    FLEET_NODES.len(),
    u32::try_from(FLEET_NODES.len() - 1).unwrap(),
  ));
  let owner_host = fleet_view(&instances[0]).unwrap().host;
  assert!(hosts.contains(&owner_host));

  // Placement across processes, then the owner's death as a crash would deal it.
  let mountable = mount_nfs_available();
  let (id, snapshot) = seal_on_owner(&instances[0], mountable);
  wait_placed(&instances[0], &id, &snapshot);
  let survivors = instances[1..].to_vec();
  assert_not_served(&survivors, &id);
  drop(daemons.remove(0));

  // Retirement, takeover, serve.
  assert_retired(&wait_fleet_views(&survivors, 2, 1), &owner_host);
  let successor = wait_successor(&survivors, &id);
  read_on_successor(&successor, &id, mountable);

  drop(daemons);
  drop(scratch);
}
