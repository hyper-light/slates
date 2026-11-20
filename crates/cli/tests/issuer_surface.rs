//! The issuer surface across processes, on every platform (A-754; §4.13 "Grants", A-299): what the anchor prints for its
//! user to export lets a process the anchor did not start mint a credential (`slates credential token`), and nothing
//! less does. On macOS and Windows the printed surface is the segment's name; on Linux, since A-754, the anchor's issuer
//! route and the secret it answers to, which a process presents over the route for the segment's descriptor, the anchor
//! checking its user (`SO_PEERCRED`) and the secret. Before A-754 a Linux anchor printed nothing, so no separate process
//! could issue there, and stock git, hg and svn could not authenticate to any edge.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "support/process_wait.rs"]
mod process_wait;

use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Shape: the shards of the test's daemon, as `tests/cli.rs` starts one.
const SHARDS: &str = "2";

/// Shape: the pause between looks at the anchor's tree while a line is awaited.
const POLL: Duration = Duration::from_millis(20);

/// Format: the start of the line a daemon prints once it serves (`crates/api/src/role/daemon.rs`); its instance follows.
const SERVING_LINE: &str = "slates daemon: instance ";

/// Format: the anchor's line naming its issuer surface (`crates/api/src/role/anchor.rs`): `NAME=VALUE` pairs.
const ISSUER_SURFACE_LINE: &str = "slates anchor: issuer surface: export ";

/// Format: the anchor's line when its supervision finds its daemon in a crash loop.
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";

/// Format: the variable holding the Linux issuer secret (`slates_ipc::issuer_route::ENV_ISSUER_SECRET`).
#[cfg(target_os = "linux")]
const ENV_ISSUER_SECRET: &str = "SLATES_ANCHOR_ISSUER_SECRET";

/// The anchor process, killed and reaped when dropped, so a failed assertion leaves no daemon behind.
struct Anchor {
  child: Child,
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// Forwards the anchor's lines, each echoed to this process's standard error, to a channel the test reads.
fn forward_lines(child: &mut Child) -> Receiver<String> {
  let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let (sender, lines) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    for line in stderr.lines().map_while(Result::ok) {
      eprintln!("{line}");
      if sender.send(line).is_err() {
        break;
      }
    }
  });
  lines
}

/// Reads the anchor's lines until its daemon serves and it has printed its issuer surface, in either order: the
/// surface's pairs.
/// Each line is waited for while the anchor's tree gains CPU ([`process_wait::Judged`]; until 2026-11-20 within
/// `PATIENCE`, 120 s of the wall).
fn surface_once_served(
  lines: &Receiver<String>,
  instance: &str,
  judged: &mut process_wait::Judged,
) -> Vec<(String, String)> {
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (mut served, mut surface) = (false, None);
  while !served || surface.is_none() {
    let line = loop {
      match lines.recv_timeout(POLL) {
        Ok(line) => break line,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
          judged.look("the anchor served and printed its issuer surface")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("the anchor's lines ended"),
      }
    };
    assert!(
      !line.starts_with(CRASH_LOOP_LINE),
      "the daemon never served: {line}"
    );
    served |= line.starts_with(&serving);
    if let Some(pairs) = line.trim().strip_prefix(ISSUER_SURFACE_LINE) {
      surface = Some(
        pairs
          .split(' ')
          .filter_map(|pair| pair.split_once('='))
          .map(|(name, value)| (name.to_owned(), value.to_owned()))
          .collect::<Vec<_>>(),
      );
    }
  }
  surface.unwrap()
}

/// Runs `slates --instance INSTANCE ARGS` with `issuer`'s variables in its environment and none of the anchor's own:
/// its exit code and both outputs.
fn slates(instance: &str, issuer: &[(String, String)], args: &[&str]) -> (i32, String, String) {
  let output = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args(["--instance", instance])
    .args(args)
    .env_remove("SLATES_ANCHOR")
    .env_remove("SLATES_ANCHOR_LEN")
    .env_remove("SLATES_ANCHOR_ISSUER_ROUTE")
    .env_remove("SLATES_ANCHOR_ISSUER_SECRET")
    .envs(issuer.iter().map(|(name, value)| (name, value)))
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// A-754 (§4.13, every platform). Do: start an anchor; read the issuer surface it prints; make its consensus group as a
/// first start does; from a separate process given those variables, mint a token (`slates credential token`); then,
/// from one given none, mint again; on Linux, from one given the route with a secret one bit off, and with the route
/// alone, mint again. Expect: the anchor prints a surface on every platform; the first mint succeeds and names a token;
/// every other is refused, nonzero, with no token.
#[test]
#[allow(clippy::disallowed_methods)]
fn a_separate_process_with_the_printed_surface_mints_a_credential_and_none_without_it_does() {
  let instance = format!("issuer-surface-{}", std::process::id());
  let mut child = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args([
      "--instance",
      &instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let lines = forward_lines(&mut child);
  let mut judged = process_wait::Judged::of(child.id());
  let _anchor = Anchor { child };
  let surface = surface_once_served(&lines, &instance, &mut judged);
  assert!(!surface.is_empty(), "the anchor printed an empty surface");
  let mut bootstrapped = false;
  for _ in 0..50 {
    if slates(&instance, &[], &["bootstrap", "root"]).0 == 0 {
      bootstrapped = true;
      break;
    }
    std::thread::sleep(Duration::from_millis(100));
  }
  assert!(bootstrapped, "the daemon's consensus group was made");
  let (code, out, err) = slates(&instance, &surface, &["credential", "token"]);
  assert_eq!(
    code, 0,
    "credential token with the printed surface: {out}{err}"
  );
  assert!(out.lines().any(|line| line.starts_with("token: ")), "{out}");
  let (code, out, err) = slates(&instance, &[], &["credential", "token"]);
  assert!(
    code != 0 && !out.contains("token: "),
    "without the surface: {code} {out}{err}"
  );
  #[cfg(target_os = "linux")]
  {
    let tampered: Vec<(String, String)> = surface
      .iter()
      .map(|(name, value)| {
        if name == ENV_ISSUER_SECRET {
          let mut text = value.clone().into_bytes();
          let last = text.last_mut().unwrap();
          *last = if *last == b'0' { b'1' } else { b'0' };
          (name.clone(), String::from_utf8(text).unwrap())
        } else {
          (name.clone(), value.clone())
        }
      })
      .collect();
    let (code, out, err) = slates(&instance, &tampered, &["credential", "token"]);
    assert!(
      code != 0 && !out.contains("token: "),
      "with a secret one bit off: {code} {out}{err}"
    );
    let route_alone: Vec<(String, String)> = surface
      .iter()
      .filter(|(name, _)| name != ENV_ISSUER_SECRET)
      .cloned()
      .collect();
    let (code, out, err) = slates(&instance, &route_alone, &["credential", "token"]);
    assert!(
      code != 0 && !out.contains("token: "),
      "with the route alone: {code} {out}{err}"
    );
  }
}
