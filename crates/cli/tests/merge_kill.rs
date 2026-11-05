//! A daemon killed mid-merge (§4.16 failure matrix: "Green owner restart: Masked (chain, deltas and `seen` replay from
//! the log; in-flight submissions retry by identity)"; "Submitter crash after commit: the retry returns the original
//! result"; AUD-11: a client is never told a version a restart can lose). A real anchor supervises a real daemon; a
//! client edits and submits in a loop through the Rust client while another thread `SIGKILL`s the daemon the moment a
//! submit is on its way, again and again. The anchor starts a successor each time, and the client's request, retried
//! by its id, meets the completion record. Afterwards, from what the client was told alone:
//!
//! - every version of the chain was told to exactly one submit (2..=head), so no version committed unheard and none
//!   was told twice;
//! - a submit that crossed no restart reads back at its version with its bytes;
//! - a submit that crossed one reads back with its bytes or without its edit (a work's unsubmitted journal does not
//!   survive a restart, D-18), never with anything else;
//! - every version reads back as the one before it with that version's edit applied.
//!
//! Unix: the kill is `SIGKILL`. Non-vacuity: some kill must land while a submit is in flight.
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn,
  clippy::indexing_slicing
)]

use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, ReadAt, Submitted, VolumeId};

/// Shape: how long the anchor's daemon, or its successor after a kill, may take to answer.
const START_WAIT: Duration = Duration::from_secs(60);
/// Shape: the kills, each landing as a submit leaves the client.
const KILLS: u32 = 6;
/// Shape: the shared files and the blocks of each, which every round overwrites one of.
const FILES: usize = 2;
/// Shape: blocks per file.
const BLOCKS: usize = 4;
/// Shape: one block's bytes.
const BLOCK: usize = 16;
/// Shape: the rounds a successor serves before the next kill, so the chain grows between kills.
const ROUNDS_BETWEEN_KILLS: u64 = 20;
/// Shape: the shards the anchor's daemon runs, two so the client's shard and the green's can differ.
const SHARDS: &str = "2";

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a client verb; (exit code, stdout).
fn run(instance: &str, args: &[&str]) -> (i32, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
  )
}

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(20));
}

/// The anchor: killed and reaped when dropped, so a failed assertion leaves nothing behind.
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

/// Format: the start of the line the daemon prints once it serves (`crates/cli/src/daemon.rs`).
const SERVING_LINE: &str = "slates daemon: instance ";

/// Starts `slates anchor --quick`, waits for its daemon to serve and answer, and bootstraps the consensus group.
fn start_anchor(instance: &str) -> AnchorProcess {
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
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let mut line = String::new();
  loop {
    line.clear();
    assert!(
      stderr.read_line(&mut line).unwrap() > 0,
      "the anchor ended before its daemon served"
    );
    eprint!("{line}");
    if line.starts_with(&serving) {
      break;
    }
  }
  let drain = std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  });
  let anchor = AnchorProcess {
    child,
    drain: Some(drain),
  };
  let started = Instant::now();
  while daemon_pid(instance).is_none() {
    assert!(started.elapsed() < START_WAIT, "the daemon never answered");
    pause();
  }
  let (code, _) = run(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "explicit first-time bootstrap");
  anchor
}

/// The daemon's pid, from `slates status --json`, when it answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, out) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  out
    .split("\"pid\":")
    .nth(1)?
    .trim_start()
    .split(|c: char| !c.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Waits until a daemon other than `killed` answers.
fn await_successor(instance: &str, killed: u32) {
  let started = Instant::now();
  while daemon_pid(instance).is_none_or(|pid| pid == killed) {
    assert!(
      started.elapsed() < START_WAIT,
      "no successor of daemon {killed}"
    );
    pause();
  }
}

/// `SIGKILL` to `pid` as one syscall, so the kill lands inside the submit it was aimed at.
fn signal_kill(pid: u32) {
  let pid = i32::try_from(pid)
    .ok()
    .and_then(rustix::process::Pid::from_raw)
    .expect("the daemon's pid");
  rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("SIGKILL to the daemon");
}

fn connect(instance: &str) -> Client {
  let deadlines = Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        pause();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

fn path_of(file: usize) -> String {
  format!("kill/f{file}")
}

/// One round as the client saw it.
#[derive(Debug)]
struct Told {
  file: usize,
  block: usize,
  bytes: Vec<u8>,
  /// The verdict, or the typed error the submit answered.
  answer: Result<Submitted, String>,
  /// Whether the client reconnected between its edit and its submit's reply (a restart crossed the round).
  crossed: bool,
}

/// The rounds' shared state with the killer: the submits under way and the kills made.
struct Shared {
  /// Set by the client as a submit leaves it; cleared by the killer as it kills.
  aim: AtomicBool,
  /// The rounds completed, so kills are spaced by rounds.
  rounds: AtomicU64,
  /// Set when the killer is done.
  done: AtomicBool,
}

/// The killer: each time the client is about to submit, after enough rounds since the last kill, `SIGKILL` the daemon
/// at once, then wait for the anchor's successor.
fn killer(instance: &str, shared: &Shared) {
  for _ in 0..KILLS {
    let pid = daemon_pid(instance).expect("the daemon answers");
    let after = shared.rounds.load(Ordering::SeqCst) + ROUNDS_BETWEEN_KILLS;
    let started = Instant::now();
    while !(shared.aim.load(Ordering::SeqCst) && shared.rounds.load(Ordering::SeqCst) >= after) {
      assert!(
        started.elapsed() < START_WAIT,
        "the client stopped submitting"
      );
      std::hint::spin_loop();
    }
    signal_kill(pid);
    shared.aim.store(false, Ordering::SeqCst);
    await_successor(instance, pid);
  }
  shared.done.store(true, Ordering::SeqCst);
}

/// The client: rounds of an overwrite of one block, then a submit, until the killer is done; a work is replaced after
/// a conflict or a restart (a rebuilt work is based on the head the client did not see).
fn client_rounds(client: &mut Client, green: VolumeId, shared: &Shared) -> Vec<Told> {
  let mut told = Vec::new();
  let mut made = 0usize;
  let mut work = client.create_work(green, "w0").unwrap().0;
  let mut word = 0x9E37_79B9_7F4A_7C15u64;
  while !shared.done.load(Ordering::SeqCst) {
    word ^= word << 13;
    word ^= word >> 7;
    word ^= word << 17;
    let (file, block) = (
      usize::try_from(word % FILES as u64).unwrap(),
      usize::try_from((word >> 8) % BLOCKS as u64).unwrap(),
    );
    let tag = u32::try_from(told.len() + 1).unwrap().to_le_bytes();
    let bytes: Vec<u8> = tag.iter().copied().cycle().take(BLOCK).collect();
    let before = client.reconnects();
    let edited = client.edit(
      work,
      &path_of(file),
      u64::try_from(block * BLOCK).unwrap(),
      BLOCK as u64,
      &bytes,
    );
    shared.aim.store(true, Ordering::SeqCst);
    let answer = edited.and_then(|()| client.submit(work));
    shared.aim.store(false, Ordering::SeqCst);
    let crossed = client.reconnects() != before;
    let replace = crossed || !matches!(answer, Ok(Submitted::Accepted(_)));
    told.push(Told {
      file,
      block,
      bytes,
      answer: answer.map_err(|e| e.to_string()),
      crossed,
    });
    shared.rounds.fetch_add(1, Ordering::SeqCst);
    if replace {
      let _ = client.destroy(work);
      made += 1;
      work = client.create_work(green, &format!("w{made}")).unwrap().0;
    }
  }
  told
}

/// T-6.x (§4.16 failure matrix "Green owner restart: Masked"; AUD-11): do submit in a loop while the daemon is
/// `SIGKILL`ed as submits leave the client, `KILLS` times; expect every version told to exactly one submit, every
/// version to read back as the one before with its round's edit applied (or, for a round a restart crossed, without
/// it), and at least one kill to land with a submit in flight.
#[test]
fn a_daemon_killed_mid_merge_loses_no_told_version_and_tells_none_twice() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!("skipping the merge kill test: set SLATES_TEST_CLI=1 to run it (starts daemons)");
    return;
  }
  let instance = format!("merge-kill-{}", std::process::id());
  let _anchor = start_anchor(&instance);
  let mut client = connect(&instance);
  let green = client.create_green("kill-g", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  let seed_bytes = vec![b'.'; BLOCKS * BLOCK];
  for file in 0..FILES {
    client
      .edit(seed, &path_of(file), 0, 0, &seed_bytes)
      .unwrap();
  }
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let shared = Shared {
    aim: AtomicBool::new(false),
    rounds: AtomicU64::new(0),
    done: AtomicBool::new(false),
  };
  let told = std::thread::scope(|scope| {
    let killing = scope.spawn(|| killer(&instance, &shared));
    let told = client_rounds(&mut client, green, &shared);
    killing.join().unwrap();
    told
  });
  judge(&mut client, green, &told, &seed_bytes);
}

/// The invariants of the module doc, over what the client was told.
fn judge(client: &mut Client, green: VolumeId, told: &[Told], seed: &[u8]) {
  let head = client.versions(green).unwrap();
  let mut versions: Vec<(u64, &Told)> = told
    .iter()
    .filter_map(|t| match t.answer {
      Ok(Submitted::Accepted(version)) => Some((version, t)),
      _ => None,
    })
    .collect();
  versions.sort_by_key(|(version, _)| *version);
  let numbers: Vec<u64> = versions.iter().map(|(version, _)| *version).collect();
  assert_eq!(
    numbers,
    (2..=head).collect::<Vec<_>>(),
    "every version told to exactly one submit; errors: {:?}",
    told
      .iter()
      .filter(|t| t.answer.is_err())
      .collect::<Vec<_>>()
  );
  let in_flight = told.iter().filter(|t| t.crossed).count();
  assert!(in_flight > 0, "no kill landed with a round in flight");
  let mut model: Vec<Vec<u8>> = vec![seed.to_vec(); FILES];
  for (version, round) in &versions {
    let read = client
      .read(
        green,
        &path_of(round.file),
        ReadAt::Version { version: *version },
      )
      .unwrap();
    let range = round.block * BLOCK..(round.block + 1) * BLOCK;
    let applied = read[range.clone()] == round.bytes[..];
    assert!(
      applied || (round.crossed && read == model[round.file]),
      "version {version}: neither the round's edit nor, for a crossed round, the version before"
    );
    if applied {
      model[round.file][range].copy_from_slice(&round.bytes);
    }
    for (file, expected) in model.iter().enumerate() {
      assert_eq!(
        &client
          .read(green, &path_of(file), ReadAt::Version { version: *version })
          .unwrap(),
        expected,
        "{} at version {version}",
        path_of(file)
      );
    }
  }
  eprintln!(
    "merge kill: {} rounds, {} versions, {in_flight} rounds crossed a restart, {} answered an error",
    told.len(),
    head,
    told.iter().filter(|t| t.answer.is_err()).count()
  );
}
