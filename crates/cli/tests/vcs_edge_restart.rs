//! A daemon restart keeps every version-control listener's port, on every platform (A-748; A-625 part 3, A-684,
//! A-689; §4.6): the anchor holds the HTTP edge's, the SSH edge's and the svn:// listener and hands each to every daemon
//! it starts, so a client's URL survives the daemon, and a client that connects while no daemon runs waits in the
//! listener's queue for the next. On Unix the anchor hands each over as an inherited descriptor; on Windows, which
//! inherits no socket (A-183), as an explicit duplicate made for each daemon's process (`WSADuplicateSocketW`).
//!
//! Before A-748 a Windows anchor held none of them, so each daemon bound its own and every restart moved all three to new
//! ports (w-interop on jupiter, 2026-11-20: the HTTP edge at 52289, then 53602, 53617, 53629). `tests/cli.rs` holds the
//! Unix twins of this test (`a_daemon_restart_keeps_the_vcs_http_edges_port_and_serves_again`,
//! `an_anchors_vcs_ssh_edge_names_its_host_key_and_a_restart_keeps_port_and_key`); that file is Unix-only, and this one
//! is the same rule where Windows builds it.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

#[path = "support/process_wait.rs"]
mod process_wait;

use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Shape: the shards of the test's daemon, as `tests/cli.rs` starts one.
const SHARDS: &str = "2";

/// Shape: the pause between looks at the anchor's process tree while a line is awaited.
const POLL: Duration = Duration::from_millis(20);

/// Format: the start of the line a daemon prints once it serves (`crates/api/src/role/daemon.rs`); its instance
/// follows.
const SERVING_LINE: &str = "slates daemon: instance ";

/// Format: the anchor's line when its supervision finds its daemon in a crash loop (`crates/api/src/role/anchor.rs`).
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";

/// Format: the words of the anchor's restart line before the successor's pid (`crates/api/src/role/anchor.rs`).
const RESTARTED_AS: &str = "restarted as pid ";

/// Format: the start of each daemon line naming a listener's port, the port following it, and what a client is first
/// answered there: an HTTP request's head (`401` to a request with no credential, A-625 part 3), the SSH edge's
/// version line (RFC 4253 §4.2, A-684), and svnserve's greeting (`( success ( 2 2 `, A-689).
const EDGES: [(&str, &str, Ask); 3] = [
  (
    "--vcs-http",
    "slates daemon: the version-control HTTP edge serves cleartext on 127.0.0.1:",
    Ask::Http,
  ),
  (
    "--vcs-ssh",
    "slates daemon: the version-control SSH edge serves on 127.0.0.1:",
    Ask::Ssh,
  ),
  (
    "--vcs-svn",
    "slates daemon: the svn:// listener serves on 127.0.0.1:",
    Ask::Svn,
  ),
];

/// What a client asks a listener first, and what its answer must begin with.
#[derive(Clone, Copy, Debug)]
enum Ask {
  /// An HTTP request with no credential: `HTTP/1.1 401 `.
  Http,
  /// Nothing: the server's version line, `SSH-2.0-`.
  Ssh,
  /// Nothing: svnserve's greeting, `( success `.
  Svn,
}

impl Ask {
  /// What is sent first.
  fn request(self) -> &'static [u8] {
    match self {
      Ask::Http => {
        b"GET /r.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: localhost\r\n\r\n"
      }
      Ask::Ssh | Ask::Svn => b"",
    }
  }

  /// What the answer begins with.
  fn answer(self) -> &'static [u8] {
    match self {
      Ask::Http => b"HTTP/1.1 401 ",
      Ask::Ssh => b"SSH-2.0-",
      Ask::Svn => b"( success ",
    }
  }
}

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

/// The next line the anchor forwards, waited for while the anchor's process tree gains CPU ([`process_wait::Judged`];
/// until 2026-11-20 within `PATIENCE`, 120 s of the wall); fails the test on the anchor's crash-loop line.
fn next_line(lines: &Receiver<String>, judged: &mut process_wait::Judged) -> String {
  let line = loop {
    match lines.recv_timeout(POLL) {
      Ok(line) => break line,
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        judged.look("the anchor said what its daemon did")
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("the anchor's lines ended"),
    }
  };
  assert!(
    !line.starts_with(CRASH_LOOP_LINE),
    "the daemon never served: {line}"
  );
  line
}

/// The port `line` names for edge `index` of [`EDGES`], when it is that edge's line.
fn port_on(line: &str, index: usize) -> Option<u16> {
  line
    .strip_prefix(EDGES[index].1)?
    .split(|character: char| !character.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Reads the anchor's lines until its daemon serves: the port each edge named, in [`EDGES`]' order.
fn ports_until_served(
  lines: &Receiver<String>,
  judged: &mut process_wait::Judged,
  serving: &str,
) -> [Option<u16>; 3] {
  let mut ports = [None; 3];
  loop {
    let line = next_line(lines, judged);
    for (index, port) in ports.iter_mut().enumerate() {
      *port = port_on(&line, index).or(*port);
    }
    if line.starts_with(serving) {
      return ports;
    }
  }
}

/// Connects to loopback `port`, sends `ask`'s request, and reads until the answer is as long as `ask`'s expected start
/// or the server closes: the bytes read, or why there were none. Read without a timeout: the daemon answers while it
/// lives, and across a restart the anchor holds the listener for its successor (A-232, A-468); until 2026-11-20 each
/// read waited `PATIENCE`.
fn first_answer(port: u16, ask: Ask) -> std::io::Result<Vec<u8>> {
  let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
  stream.write_all(ask.request())?;
  let mut answer = Vec::new();
  let mut chunk = [0u8; 256];
  while answer.len() < ask.answer().len() {
    let read = stream.read(&mut chunk)?;
    if read == 0 {
      break;
    }
    answer.extend_from_slice(&chunk[..read]);
  }
  Ok(answer)
}

/// The daemon's pid as `slates status --json` names it.
fn daemon_pid(instance: &str) -> Option<u32> {
  let output = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args(["--instance", instance, "status", "--json"])
    .output()
    .ok()?;
  let text = String::from_utf8_lossy(&output.stdout).into_owned();
  text
    .split("\"pid\":")
    .nth(1)?
    .trim_start()
    .split(|character: char| !character.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Ends process `pid` at once, as a crash would: `kill -9` on Unix, `taskkill /F` on Windows.
fn kill(pid: u32) {
  let pid = pid.to_string();
  #[cfg(unix)]
  let status = Command::new("kill").args(["-9", &pid]).status();
  #[cfg(windows)]
  let status = Command::new("taskkill").args(["/F", "/PID", &pid]).status();
  assert!(status.unwrap().success(), "the daemon {pid} was killed");
}

/// Each listener at `ports` (in [`EDGES`]' order) answers as a client is first answered there.
fn assert_each_listener_answers(ports: &[u16; 3]) {
  for (index, port) in ports.iter().enumerate() {
    let ask = EDGES[index].2;
    let answer = first_answer(*port, ask).expect("the listener answered");
    assert!(
      answer.starts_with(ask.answer()),
      "{ask:?} at {port}: {}",
      String::from_utf8_lossy(&answer)
    );
  }
}

/// Each port asked while no daemon ran was taken and answered as its listener answers.
fn assert_answered_while_restarting(answers: Vec<(Ask, u16, std::io::Result<Vec<u8>>)>) {
  for (ask, port, answer) in answers {
    let answer = answer.unwrap_or_else(|error| {
      panic!("{ask:?}'s port {port} was dropped while its daemon restarted: {error}")
    });
    assert!(
      answer.starts_with(ask.answer()),
      "{ask:?} at {port} while the daemon restarted: {}",
      String::from_utf8_lossy(&answer)
    );
  }
}

/// A-748 with A-625 part 3, A-684 and A-689 (§4.6, every platform). Do: start an anchor with the HTTP edge, the SSH edge
/// and the svn:// listener on loopback, each on a port the system picks (`127.0.0.1:0`); read the ports its daemon
/// names and ask each; kill the daemon; once the anchor says it restarted it, ask each port again, before the successor
/// serves; read the ports the successor names. Expect: each answer as a client is first answered there (`401`, the SSH
/// version line, svnserve's greeting), the ones asked while no daemon ran taken and answered by the successor, and the
/// successor naming the same three ports. Red on Windows before A-748: the successor bound three new ports.
#[test]
fn a_daemon_restart_keeps_every_version_control_listeners_port() {
  let instance = format!("vcs-edge-restart-{}", std::process::id());
  let mut command = Command::new(env!("CARGO_BIN_EXE_slates"));
  command.args([
    "--instance",
    &instance,
    "anchor",
    "--quick",
    "--shards",
    SHARDS,
  ]);
  for (flag, _, _) in EDGES {
    command.args([flag, "127.0.0.1:0"]);
  }
  let mut child = command
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let lines = forward_lines(&mut child);
  let mut judged = process_wait::Judged::of(child.id());
  let _anchor = Anchor { child };
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let ports = ports_until_served(&lines, &mut judged, &serving)
    .map(|port| port.expect("each listener named its port"));
  assert_each_listener_answers(&ports);
  let killed = daemon_pid(&instance).expect("the daemon's pid");
  kill(killed);
  // The anchor's restart line and the successor's own lines come from two processes, in whichever order they arrive.
  let mut successor = None;
  let mut while_restarting = Vec::new();
  let mut again = [None; 3];
  let mut served = false;
  while successor.is_none() || !served {
    let line = next_line(&lines, &mut judged);
    if successor.is_none()
      && let Some(after) = line.split(RESTARTED_AS).nth(1)
    {
      successor = after
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|pid| pid.parse::<u32>().ok());
      // The killed daemon is gone (the anchor saw it exit): only a held listener still takes a connection at its port.
      while_restarting = ports
        .iter()
        .enumerate()
        .map(|(index, port)| (EDGES[index].2, *port, first_answer(*port, EDGES[index].2)))
        .collect();
    }
    for (index, port) in again.iter_mut().enumerate() {
      *port = port_on(&line, index).or(*port);
    }
    served |= line.starts_with(&serving);
  }
  assert_ne!(successor, Some(killed), "the anchor started a new daemon");
  assert_answered_while_restarting(while_restarting);
  assert_eq!(
    again,
    ports.map(Some),
    "the restarted daemon serves every listener on the port its anchor holds"
  );
}
