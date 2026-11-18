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
  let tests: [(&str, fn()); 9] = [
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
    (
      "an_instance_started_with_a_launcher_secret_takes_it_and_serves",
      an_instance_started_with_a_launcher_secret_takes_it_and_serves,
    ),
    (
      "an_anchor_handed_a_bad_launcher_record_refuses_to_start",
      an_anchor_handed_a_bad_launcher_record_refuses_to_start,
    ),
    (
      "the_launchers_secret_proves_its_policy_across_a_daemon_restart",
      the_launchers_secret_proves_its_policy_across_a_daemon_restart,
    ),
    (
      "an_anchor_started_under_an_exec_wrapper_keeps_the_launchers_secret",
      an_anchor_started_under_an_exec_wrapper_keeps_the_launchers_secret,
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
    match self.0.take().map(Anchor::stop) {
      Some(Ok(code)) => code,
      other => panic!("the held anchor stops: {other:?}"),
    }
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
    .provision(&slates_api::VolumeSpec::empty("started", ROOM))
    .unwrap()
    .volume();
  let attached = slates
    .attach_volume(volume, slates_api::Intent::Write)
    .unwrap();
  slates
    .client()
    .fs_write((volume, attached.attachment), "/f", b"from the host", MODE)
    .unwrap();
  slates.detach(attached.attachment).unwrap();
  assert_eq!(
    slates.read(volume, "/f", slates_api::ReadAt::Head).unwrap(),
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

/// A-299. Do: start an instance from this binary with a launcher secret, which the start hands the anchor on its
/// standard input, and use it. Expect: the anchor takes the record (an anchor that could not would refuse to start) and
/// the instance serves; the secret's issuer refuses only what the launcher's authority cannot prove, a consensus
/// recovery.
fn an_instance_started_with_a_launcher_secret_takes_it_and_serves() {
  let instance = instance_name("launcher");
  let secret = slates_api::LauncherSecret::generate().unwrap();
  let started = Instance::start(&spec(&instance).with_launcher(secret.clone())).unwrap();
  let mut slates = started.instance;
  let anchor = Stopping(Some(started.anchor));
  assert_eq!(
    slates.daemon_status().unwrap().shards.len(),
    usize::from(TEST_SHARDS)
  );
  assert_eq!(
    slates_api::Issuer::from_launcher_secret(&secret).recovery_proof(&[0; 32]),
    Err(Error::Denied(slates_api::Denied::NoAuthority(
      slates_api::IssuerFault::LauncherCannotRecover
    )))
  );
  assert_eq!(anchor.stop(), Some(0));
}

/// A-299. Do: launch this binary's anchor with the launcher's marker set and a standard input holding bytes that are not
/// a launcher's record, then with one cut short. Expect: each anchor refuses to start with the refused-configuration
/// code before the instance answers — an anchor whose starter meant to give it authority and failed never runs.
fn an_anchor_handed_a_bad_launcher_record_refuses_to_start() {
  use std::io::Write;
  for (tag, bytes) in [
    ("garbage", vec![7u8; slates_api::launcher::RECORD_BYTES]),
    ("short", vec![7u8; 3]),
  ] {
    let instance = instance_name(tag);
    let mut command = spec(&instance).anchor_command().unwrap();
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(&bytes).unwrap();
    drop(writer);
    command
      .env(
        slates_api::launcher::ENV_LAUNCHER,
        slates_api::launcher::LAUNCHER_ON_STDIN,
      )
      .stdin(std::process::Stdio::from(reader));
    let mut child = command.spawn().unwrap();
    assert_eq!(
      Instance::wait_for(&instance, Some(&mut child), None).unwrap_err(),
      Error::Start(StartFault::AnchorExited {
        code: Some(i32::from(slates_api::role::EXIT_REFUSED))
      }),
      "{tag}"
    );
  }
}

/// A-299, A-285. Do: start an instance from this binary with a launcher secret, set a volume's write policy under the
/// secret's authority and under a stranger's, then kill the daemon (the anchor restarts it, as after a crash) and set
/// the policy again under the same secret. Expect: the launcher's setting holds and reads back; the stranger's is
/// refused `Unproven`; after the restart the new daemon, which minted an issuer secret of its own, still verifies the
/// launcher's proof — the authority a host keeps outlives the daemon — and the volume keeps its policy and takes the new
/// one.
fn the_launchers_secret_proves_its_policy_across_a_daemon_restart() {
  use slates_api::{Denied, PolicySubject, WriteMode, WritePolicy};
  let instance = instance_name("restart");
  let secret = slates_api::LauncherSecret::generate().unwrap();
  let started = Instance::start(&spec(&instance).with_launcher(secret.clone())).unwrap();
  let mut slates = started.instance;
  let anchor = Stopping(Some(started.anchor));
  let launcher = slates_api::Issuer::from_launcher_secret(&secret);
  let volume = slates
    .provision(&slates_api::VolumeSpec::empty("governed", ROOM))
    .unwrap()
    .volume();
  let subject = PolicySubject::Volume(volume);
  let bypass = WritePolicy {
    merge: WriteMode::Bypass,
    land: WriteMode::Bypass,
    require_signed_merges: false,
  };
  slates
    .set_write_policy(subject, &bypass, &launcher)
    .unwrap();
  assert_eq!(slates.write_policy(subject).unwrap(), Some(bypass));
  let stranger =
    slates_api::Issuer::from_launcher_secret(&slates_api::LauncherSecret::generate().unwrap());
  assert_eq!(
    slates.set_write_policy(subject, &WritePolicy::UNSET, &stranger),
    Err(Error::Denied(Denied::Unproven))
  );
  #[cfg(unix)]
  {
    let anchor_pid = anchor.0.as_ref().unwrap().id();
    let (daemon_pid, _) = processes_of(&instance)
      .into_iter()
      .find(|(pid, args)| *pid != anchor_pid && args.contains(" daemon "))
      .unwrap();
    let daemon = rustix::process::Pid::from_raw(i32::try_from(daemon_pid).unwrap()).unwrap();
    rustix::process::kill_process(daemon, rustix::process::Signal::KILL).unwrap();
    let ask = WritePolicy {
      land: WriteMode::Ask,
      ..bypass
    };
    // The client reconnects to the daemon the anchor restarted, under its own id, and the setting is sent to it.
    slates.set_write_policy(subject, &ask, &launcher).unwrap();
    assert_eq!(slates.write_policy(subject).unwrap(), Some(ask));
    assert!(
      processes_of(&instance)
        .iter()
        .all(|(pid, _)| *pid != daemon_pid),
      "the daemon that answered is a new one"
    );
  }
  assert_eq!(anchor.stop(), Some(0));
}

/// A-299 (g263's ask: kiln starts its anchor under `systemd-run --user --scope -- ...` so the instance outlives the
/// runner). Do: start an anchor under an exec wrapper (`/usr/bin/env`, which execs what follows it in the same process,
/// as `systemd-run --scope` does) with a launcher secret, wait for it, and set a volume's write policy under the secret.
/// Expect: the instance answers, the launcher's proof verifies (so the record reached the anchor through the wrapper on
/// its standard input), and the wrapped process, now the anchor, stops gracefully at `SIGTERM`, exit 0.
fn an_anchor_started_under_an_exec_wrapper_keeps_the_launchers_secret() {
  #[cfg(unix)]
  {
    use slates_api::{PolicySubject, WriteMode, WritePolicy};
    /// Stops the wrapped anchor when dropped (`SIGTERM`, then its exit), so a failed assertion leaves no instance.
    struct Wrapped(Option<std::process::Child>);
    impl Wrapped {
      fn stop(mut self) -> Option<i32> {
        let Some(mut child) = self.0.take() else {
          panic!("the wrapped anchor was stopped already");
        };
        let Ok(raw) = i32::try_from(child.id()) else {
          panic!("the wrapped anchor's process id does not fit an i32");
        };
        let Some(pid) = rustix::process::Pid::from_raw(raw) else {
          panic!("the wrapped anchor's process id is zero");
        };
        if let Err(error) = rustix::process::kill_process(pid, rustix::process::Signal::TERM) {
          panic!("asking the wrapped anchor to stop: {error}");
        }
        match child.wait() {
          Ok(status) => status.code(),
          Err(error) => panic!("waiting for the wrapped anchor: {error}"),
        }
      }
    }
    impl Drop for Wrapped {
      fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
          let _killed = child.kill();
          let _waited = child.wait();
        }
      }
    }
    let instance = instance_name("wrapped");
    let secret = slates_api::LauncherSecret::generate().unwrap();
    let mut command = spec(&instance)
      .with_launcher(secret.clone())
      .anchor_command_under(&[std::ffi::OsStr::new("/usr/bin/env")])
      .unwrap();
    let mut wrapped = Wrapped(Some(command.spawn().unwrap()));
    let mut slates = Instance::wait_for(&instance, wrapped.0.as_mut(), None).unwrap();
    slates.ensure_group().unwrap();
    let volume = slates
      .provision(&slates_api::VolumeSpec::empty("wrapped", ROOM))
      .unwrap()
      .volume();
    let bypass = WritePolicy {
      merge: WriteMode::Bypass,
      land: WriteMode::Bypass,
      require_signed_merges: false,
    };
    slates
      .set_write_policy(
        PolicySubject::Volume(volume),
        &bypass,
        &slates_api::Issuer::from_launcher_secret(&secret),
      )
      .unwrap();
    drop(slates);
    assert_eq!(
      wrapped.stop(),
      Some(0),
      "the wrapped anchor stops gracefully"
    );
  }
}
