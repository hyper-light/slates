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
  let (code, _, _) = run(instance, &["detach", &attachment]);
  assert_eq!(code, 0);
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
  let instance = format!("cli-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let id = create_and_list(&instance);
  snapshot_clone_stat(&instance, &id);
  attach_status_detach(&instance, &id);
  daemon_status(&instance);
  resize_destroy_and_refusals(&instance, &id);
  kill_anchor_and_wait_for_the_daemon_to_leave(&instance, anchor);
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
