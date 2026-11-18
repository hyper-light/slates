//! Instance start by use (§2.6 "Boot order", §4.12 "Embedding", A-278): this test binary is an embedding host. Its
//! `main` serves the roles when its arguments begin with its prefix (`slates-role`), exactly as kiln's does, and the
//! tests start instances from it through the library — no `slates` binary is built, found or run. Each test judges only
//! what a host could observe: the start's answer, the instance's answers, the processes' arguments, and the exit codes.
//!
//! The binary has no libtest harness (`harness = false` in `Cargo.toml`), since a host's `main` must see its own
//! arguments first; the tests run in order, and a failure ends the binary with the failure's text and code 1. Every
//! started anchor is stopped by a guard, so a failed assertion leaves no instance behind.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::print_stdout,
  clippy::print_stderr
)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_api::{Anchor, Error, Instance, StartFault, StartSpec};

/// Format: the word this host's `main` recognises before a role's arguments.
const PREFIX: &str = "slates-role";
/// Shape: one shard per test instance: the start is the subject, not placement.
const TEST_SHARDS: u16 = 1;
/// Shape: a scratch volume's quota: room for the one small file a test writes.
const ROOM: u64 = 4 << 20;
/// Shape: the permission bits of a file a test writes.
const MODE: u32 = 0o644;
/// Shape: how long the deadline test waits on an instance nobody starts.
const NOBODY_WAIT: Duration = Duration::from_millis(300);

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  if args.first().map(String::as_str) == Some(PREFIX) {
    return slates_api::role::main(&args[..1], &args[1..]);
  }
  let tests: [(&str, fn()); 5] = [
    (
      "an_instance_starts_from_the_hosts_own_executable_serves_and_stops",
      an_instance_starts_from_the_hosts_own_executable_serves_and_stops,
    ),
    (
      "a_second_start_of_a_running_instance_is_refused_already_running",
      a_second_start_of_a_running_instance_is_refused_already_running,
    ),
    (
      "a_program_that_cannot_run_is_refused_spawn",
      a_program_that_cannot_run_is_refused_spawn,
    ),
    (
      "an_anchor_whose_host_refuses_the_role_is_refused_anchor_exited",
      an_anchor_whose_host_refuses_the_role_is_refused_anchor_exited,
    ),
    (
      "a_wait_for_an_instance_nobody_starts_ends_unavailable_at_its_deadline",
      a_wait_for_an_instance_nobody_starts_ends_unavailable_at_its_deadline,
    ),
  ];
  let filter = args.iter().find(|arg| !arg.starts_with('-'));
  let mut ran = 0usize;
  for (name, test) in tests {
    if filter.is_some_and(|filter| !name.contains(filter.as_str())) {
      continue;
    }
    println!("test {name} ...");
    test();
    println!("test {name} ... ok");
    ran += 1;
  }
  println!("test result: ok. {ran} passed");
  ExitCode::SUCCESS
}

/// A fresh instance name, unique to this process and test.
fn instance_name(tag: &str) -> String {
  static NEXT: AtomicU64 = AtomicU64::new(0);
  format!(
    "api-start-{tag}-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  )
}

/// The start of `instance` from this binary, as a host makes it: its own executable, its prefix, the quick profile and
/// one shard.
fn spec(instance: &str) -> StartSpec {
  let mut spec = StartSpec::new(instance, std::env::current_exe().unwrap(), &[PREFIX]);
  spec.quick = true;
  spec.shards = Some(TEST_SHARDS);
  spec
}

/// Stops the anchor it holds when dropped, so a failed assertion leaves no instance running.
struct Stopping(Option<Anchor>);

impl Stopping {
  /// Stops the anchor now; its exit code.
  fn stop(mut self) -> Option<i32> {
    self.0.take().unwrap().stop().unwrap()
  }
}

impl Drop for Stopping {
  fn drop(&mut self) {
    if let Some(anchor) = self.0.take() {
      let _stopped = anchor.stop();
    }
  }
}

/// The arguments of every process whose arguments carry `--instance instance`, by process id (Unix: `ps`, which a host
/// reading its machine's processes would use too).
#[cfg(unix)]
fn processes_of(instance: &str) -> Vec<(u32, String)> {
  let listing = std::process::Command::new("ps")
    .args(["-A", "-ww", "-o", "pid=,args="])
    .output()
    .unwrap();
  let needle = format!("--instance {instance} ");
  String::from_utf8_lossy(&listing.stdout)
    .lines()
    .filter_map(|line| {
      let line = line.trim_start();
      let (pid, args) = line.split_once(' ')?;
      let args = format!("{} ", args.trim());
      args.contains(&needle).then(|| (pid.parse().unwrap(), args))
    })
    .collect()
}

/// A-278, AC-5.12 / T-5.14. Do: start an instance from this binary with its prefix (no `slates` binary anywhere), use
/// it — the daemon's status, a scratch volume written and read back — then stop it and attach again. Expect: the start
/// answered with the group made, so a volume is created at once; the anchor and the daemon both this executable, each
/// with the prefix first and `--instance NAME` in its arguments (the daemon started by the anchor from the same program
/// and prefix); the stop answered exit 0 (the graceful stop); and the instance gone after it, `Unavailable`.
fn an_instance_starts_from_the_hosts_own_executable_serves_and_stops() {
  let instance = instance_name("serves");
  let started = Instance::start(&spec(&instance)).unwrap();
  let mut slates = started.instance;
  let anchor = Stopping(Some(started.anchor));
  let report = slates.daemon_status().unwrap();
  assert_eq!(report.shards.len(), usize::from(TEST_SHARDS));
  let volume = slates
    .create(&slates_api::scratch("started", ROOM))
    .unwrap();
  let attached = slates
    .attach_volume(volume, slates_api::Intent::Write)
    .unwrap();
  slates
    .client()
    .fs_write((volume, attached.attachment), "/f", b"from the host", MODE)
    .unwrap();
  slates.detach(attached.attachment).unwrap();
  assert_eq!(
    slates
      .client()
      .read(volume, "/f", slates_ipc::protocol::ReadAt::Head)
      .unwrap(),
    b"from the host"
  );
  #[cfg(unix)]
  {
    let program = std::env::current_exe().unwrap();
    let program = program.to_string_lossy();
    let processes = processes_of(&instance);
    let anchor_pid = anchor.0.as_ref().unwrap().id();
    assert!(
      processes.iter().any(|(pid, args)| *pid == anchor_pid
        && args.starts_with(&format!("{program} {PREFIX} --instance {instance} anchor "))),
      "the anchor runs this executable with the prefix: {processes:?}"
    );
    assert!(
      processes.iter().any(|(pid, args)| *pid != anchor_pid
        && args.ends_with(&format!(
          " {PREFIX} --instance {instance} daemon --quick --shards {TEST_SHARDS} "
        ))),
      "the anchor's daemon runs this executable with the same prefix: {processes:?}"
    );
  }
  drop(slates);
  assert_eq!(anchor.stop(), Some(0), "the graceful stop exits 0");
  let after = Instance::attach(&instance).unwrap_err();
  assert!(after.is_unavailable(), "{after:?}");
}

/// A-278. Do: start an instance, then start it again while it runs. Expect: the second start refused
/// `AlreadyRunning` naming the instance, before it spawns anything; the first instance still answers.
fn a_second_start_of_a_running_instance_is_refused_already_running() {
  let instance = instance_name("twice");
  let started = Instance::start(&spec(&instance)).unwrap();
  let mut slates = started.instance;
  let anchor = Stopping(Some(started.anchor));
  assert_eq!(
    Instance::start(&spec(&instance)).unwrap_err(),
    Error::Start(StartFault::AlreadyRunning {
      instance: instance.clone()
    })
  );
  slates.daemon_status().unwrap();
  drop(slates);
  assert_eq!(anchor.stop(), Some(0));
}

/// A-278. Do: start an instance from a program that does not exist. Expect: `Spawn` naming the program, with the OS's
/// error number; nothing is running under the instance's name.
fn a_program_that_cannot_run_is_refused_spawn() {
  let instance = instance_name("missing");
  let mut missing = spec(&instance);
  missing.program = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-such-program");
  let refused = Instance::start(&missing).unwrap_err();
  assert!(
    matches!(&refused, Error::Start(StartFault::Spawn { program, errno: Some(_) })
      if program.ends_with("no-such-program")),
    "{refused:?}"
  );
  assert!(Instance::attach(&instance).unwrap_err().is_unavailable());
}

/// A-278. Do: start an instance from this binary with a prefix after which its role entry finds a flag no role takes,
/// as a host whose `main` hands the role entry the wrong arguments would. Expect: the start answered `AnchorExited`
/// with the role entry's usage code, at once — the wait ends with the anchor, never a hang.
fn an_anchor_whose_host_refuses_the_role_is_refused_anchor_exited() {
  let instance = instance_name("refused");
  let mut refused = spec(&instance);
  refused.prefix.push("--no-such-flag".to_owned());
  assert_eq!(
    Instance::start(&refused).unwrap_err(),
    Error::Start(StartFault::AnchorExited {
      code: Some(i32::from(slates_api::role::EXIT_USAGE))
    })
  );
}

/// A-278. Do: wait for an instance nobody starts, with no anchor to watch and a deadline. Expect: `Unavailable` naming
/// the instance once the deadline has passed, and not before it.
fn a_wait_for_an_instance_nobody_starts_ends_unavailable_at_its_deadline() {
  let instance = instance_name("nobody");
  let began = Instant::now();
  let refused = Instance::wait_for(&instance, None, Some(began + NOBODY_WAIT)).unwrap_err();
  assert!(
    matches!(&refused, Error::Unavailable { instance: asked, .. } if *asked == instance),
    "{refused:?}"
  );
  assert!(began.elapsed() >= NOBODY_WAIT, "{:?}", began.elapsed());
}
