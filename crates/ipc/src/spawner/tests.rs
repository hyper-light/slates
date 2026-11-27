//! A-924's tests: the spawner started as this test binary re-invoked in its role ([`spawner_role`]), a child started
//! through it and its exit answered, its confinement the same as a direct spawn's, a running child's group killed when
//! the daemon's end goes, and the red: a spawn's cost on its calling thread flat in the daemon's open descriptors,
//! measured in a process of its own ([`spawn_cost_role`]) so its descriptors reach no other test.

use std::ffi::OsStr;
use std::io::Read as _;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use super::{
  Answer, FrameKind, Inbound, Outbound, Read, Started, answer_of, frame, spawn_body,
  spawn_descriptors, start,
};
use crate::confined_spawn::{ChildStream, Environment, Spawn};

/// Format: the variable that makes [`spawn_cost_role`] measure.
const MEASURE_VARIABLE: &str = "SLATES_SPAWNER_TEST_MEASURE";
/// Format: the tag on each line [`spawn_cost_role`] prints, looked for anywhere in a line (libtest's banner precedes it).
const COST_TAG: &str = "SPAWN-COST";
/// Format: the tag on a line saying a count could not be held, and how many were (a runner's hard `RLIMIT_NOFILE` of
/// 65,536 holds no 100,000, main's diag run 45666757721); no cost row's tag is in it.
const CAP_TAG: &str = "SPAWN-HELD";
/// Format: the descriptors the measuring role opens of its own past those open at its start: the null device it
/// duplicates and the listing that counts them.
const ROLE_OWN_DESCRIPTORS: usize = 2;
/// Shape: the open descriptors the red measures at: a quiet process, a busy daemon, a daemon serving tens of thousands
/// of pushes (GAPS 8a-hook-exit: 0.2, 1.2 to 4, 8 to 13 ms a direct spawn on macOS).
const DESCRIPTOR_COUNTS: [usize; 3] = [100, 10_000, 100_000];
/// Shape: the spawns measured at each count; the median and the spread of an odd count.
const REPS: usize = 7;
/// Shape: how long a test waits for the spawner's answer: four liveness windows' worth of a quiet machine's spawn.
const PATIENCE: Duration = Duration::from_secs(10);

/// The arguments that run `test` alone in this test binary.
fn role_args(test: &str) -> Vec<String> {
  vec![
    format!("spawner::tests::{test}"),
    "--exact".to_owned(),
    "--nocapture".to_owned(),
  ]
}

/// The spawner's role: serves when this binary was started as one, else does nothing.
#[test]
fn spawner_role() {
  if let Some(served) = super::serve_from_env() {
    std::process::exit(i32::from(served.is_err()));
  }
}

/// A spawner of `ends` ends started as this binary in its role.
fn spawner(ends: usize) -> Started {
  let program = std::env::current_exe().unwrap();
  let args = role_args("spawner_role");
  let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
  start(&program, &args, ends).unwrap()
}

/// `/bin/sh` with `args` (`-c` and its script), its standard output `stdout`, as its group's leader.
fn shell<'a>(args: &'a [&'a OsStr], stdout: ChildStream<'a>) -> Spawn<'a> {
  Spawn {
    program: Path::new("/bin/sh"),
    args,
    environment: Environment::Inherited,
    overrides: &[],
    directory: Path::new("/"),
    own_group: true,
    stdin: ChildStream::Null,
    stdout,
    stderr: ChildStream::Null,
    handed: &[],
  }
}

/// Asks the spawner on `end` for `spawn` as spawn `id`: the time the asking thread spent sending it.
fn ask(end: &OwnedFd, id: u64, spawn: &Spawn<'_>) -> Duration {
  let began = Instant::now();
  let request = frame(FrameKind::Spawn, id, &spawn_body(spawn).unwrap()).unwrap();
  let riding: Vec<OwnedFd> = spawn_descriptors(spawn)
    .into_iter()
    .map(|fd| fd.try_clone_to_owned().unwrap())
    .collect();
  let mut outbound = Outbound::default();
  outbound.owe(&request, riding);
  outbound.flush(end.as_fd()).unwrap();
  let took = began.elapsed();
  while !outbound.is_empty() {
    wait_ready(end, rustix::event::PollFlags::OUT);
    outbound.flush(end.as_fd()).unwrap();
  }
  took
}

/// Waits until `end` is ready for `flags`, within [`PATIENCE`].
fn wait_ready(end: &OwnedFd, flags: rustix::event::PollFlags) {
  let mut fds = [rustix::event::PollFd::new(end, flags)];
  let patience = rustix::event::Timespec::try_from(PATIENCE).unwrap();
  rustix::event::poll(&mut fds, Some(&patience)).unwrap();
}

/// The next answer the spawner sends on `end` about spawn `id`, within [`PATIENCE`]; none once the end has gone.
fn answered(end: &OwnedFd, inbound: &mut Inbound, id: u64) -> Option<Answer> {
  let until = Instant::now() + PATIENCE;
  loop {
    while let Some(frame) = inbound.next_frame() {
      if frame.id == id {
        return answer_of(&frame);
      }
    }
    assert!(
      Instant::now() < until,
      "the spawner answered spawn {id} within {PATIENCE:?}"
    );
    wait_ready(end, rustix::event::PollFlags::IN);
    if inbound.read(end.as_fd()).unwrap() == Read::Closed {
      return None;
    }
  }
}

/// Lets spawn `id` go.
fn release(end: &OwnedFd, id: u64) {
  let mut outbound = Outbound::default();
  outbound.owe(&frame(FrameKind::Release, id, &[]).unwrap(), Vec::new());
  outbound.flush(end.as_fd()).unwrap();
}

/// A-924. Do: start the spawner; ask it for `/bin/sh -c 'echo spawned; exit 7'` with its output on a pipe of this
/// process's. Expect: the pid, the pipe's `spawned`, then the exit with code 7; the child reaped once released.
#[test]
fn a_child_started_through_the_spawner_runs_and_its_exit_is_answered() {
  // Spawns a process, so holds the descriptor tests' gate across it (`crate::descriptor_test_gate`).
  let _gate = crate::descriptor_test_gate();
  let spawner = spawner(1);
  let end = spawner.ends.first().unwrap();
  let (mut read, write) = std::io::pipe().unwrap();
  let args = [OsStr::new("-c"), OsStr::new("echo spawned; exit 7")];
  ask(end, 1, &shell(&args, ChildStream::Fd(write.as_fd())));
  drop(write);
  let mut inbound = Inbound::default();
  let Some(Answer::Spawned(pid)) = answered(end, &mut inbound, 1) else {
    panic!("not spawned");
  };
  let mut said = String::new();
  read.read_to_string(&mut said).unwrap();
  let exited = answered(end, &mut inbound, 1);
  release(end, 1);
  assert!(pid > 0);
  assert_eq!(said, "spawned\n");
  let Some(Answer::Exited(status)) = exited else {
    panic!("no exit: {exited:?}");
  };
  assert_eq!(status.code(), Some(7));
}

/// A-924, confinement parity (main's ruling of 2026-11-25, addition 1). Do: hold a pipe open in this process; start
/// a child that lists its open descriptors (`ls /dev/fd`, whose own reading of the listing shows in it) directly, as
/// `confined_spawn` starts one, and through the spawner. Expect: the two listings the same: the child through the
/// spawner holds nothing a direct one does not (none of the spawner's ends, its self-pipe, or this process's pipe).
#[test]
fn a_child_started_through_the_spawner_holds_what_a_direct_one_holds() {
  // Spawns a process, so holds the descriptor tests' gate across it (`crate::descriptor_test_gate`).
  let _gate = crate::descriptor_test_gate();
  let spawner = spawner(1);
  let end = spawner.ends.first().unwrap();
  let (_held_read, _held_write) = std::io::pipe().unwrap();
  let args = [OsStr::new("-c"), OsStr::new("ls /dev/fd")];
  let (mut direct_read, direct_write) = std::io::pipe().unwrap();
  let mut child =
    crate::confined_spawn::spawn(&shell(&args, ChildStream::Fd(direct_write.as_fd()))).unwrap();
  drop(direct_write);
  let mut direct = String::new();
  direct_read.read_to_string(&mut direct).unwrap();
  child.wait().unwrap();
  let (mut read, write) = std::io::pipe().unwrap();
  ask(end, 1, &shell(&args, ChildStream::Fd(write.as_fd())));
  drop(write);
  let mut inbound = Inbound::default();
  assert!(matches!(
    answered(end, &mut inbound, 1),
    Some(Answer::Spawned(_))
  ));
  let mut through = String::new();
  read.read_to_string(&mut through).unwrap();
  let _ = answered(end, &mut inbound, 1);
  release(end, 1);
  assert_eq!(
    through, direct,
    "the child through the spawner holds what a direct one holds"
  );
}

/// A-924, bound to the daemon (main's ruling, addition 2). Do: ask the spawner for a child that sleeps far past the
/// test; once it runs, drop the daemon's end. Expect: the spawner kills the child's group and exits, the child gone
/// within [`PATIENCE`].
#[test]
fn a_running_childs_group_is_killed_when_the_daemons_end_goes() {
  // Spawns a process, so holds the descriptor tests' gate across it (`crate::descriptor_test_gate`).
  let _gate = crate::descriptor_test_gate();
  let mut spawner = spawner(1);
  let end = spawner.ends.pop().unwrap();
  let args = [OsStr::new("-c"), OsStr::new("exec sleep 3600")];
  ask(&end, 1, &shell(&args, ChildStream::Null));
  let mut inbound = Inbound::default();
  let Some(Answer::Spawned(pid)) = answered(&end, &mut inbound, 1) else {
    panic!("not spawned");
  };
  drop(end);
  let status = spawner.child.wait().unwrap();
  let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
  let until = Instant::now() + PATIENCE;
  while rustix::process::test_kill_process(pid).is_ok() && Instant::now() < until {
    #[allow(
      clippy::disallowed_methods,
      reason = "a test waits on another process's death, which no timer of ours observes; D-9 binds shipped code"
    )]
    std::thread::sleep(Duration::from_millis(10));
  }
  assert!(status.success(), "the spawner exited cleanly: {status:?}");
  assert!(
    rustix::process::test_kill_process(pid).is_err(),
    "the child the spawner held was killed when the daemon's end went"
  );
}

/// A-924, hostile input (CLAUDE.md §4: every parser of external bytes is held to its cap before it allocates). Do: on
/// the spawner's only end, send a frame header whose body length is `u32::MAX`, far past [`super::most_body_bytes`].
/// Expect: the spawner ends that conversation at once, without waiting for the body: it exits cleanly, and the end
/// reads closed.
#[test]
fn a_frame_longer_than_any_spawn_ends_its_conversation() {
  // Spawns a process, so holds the descriptor tests' gate across it (`crate::descriptor_test_gate`).
  let _gate = crate::descriptor_test_gate();
  let mut spawner = spawner(1);
  let end = spawner.ends.pop().unwrap();
  let mut header = frame(FrameKind::Spawn, 1, &[]).unwrap();
  if let Some(length) = header.get_mut(super::LENGTH_AT..super::ID_AT) {
    length.copy_from_slice(&u32::MAX.to_le_bytes());
  }
  let mut outbound = Outbound::default();
  outbound.owe(&header, Vec::new());
  outbound.flush(end.as_fd()).unwrap();
  let status = spawner.child.wait().unwrap();
  let mut inbound = Inbound::default();
  wait_ready(&end, rustix::event::PollFlags::IN);
  assert!(status.success(), "the spawner exited cleanly: {status:?}");
  assert_eq!(inbound.read(end.as_fd()).unwrap(), Read::Closed);
}

/// What one count's spawns took: the least, the median and the most, in nanoseconds.
#[derive(Debug, Clone, Copy)]
struct Took {
  least: u128,
  median: u128,
  most: u128,
}

/// The least, median and most of `samples`.
fn took(mut samples: Vec<Duration>) -> Took {
  samples.sort_unstable();
  let at = |index: usize| samples.get(index).map_or(0, Duration::as_nanos);
  Took {
    least: at(0),
    median: at(samples.len() / 2),
    most: at(samples.len().saturating_sub(1)),
  }
}

/// [`REPS`] spawns of `/usr/bin/true` through the spawner on `end`, each waited to its exit and released: each one's
/// time on the asking thread.
fn through_the_spawner(end: &OwnedFd, first_id: u64) -> Vec<Duration> {
  let mut inbound = Inbound::default();
  (0..REPS)
    .map(|rep| {
      let id = first_id + u64::try_from(rep).unwrap();
      let spent = ask(end, id, &true_program());
      assert!(matches!(
        answered(end, &mut inbound, id),
        Some(Answer::Spawned(_))
      ));
      assert!(matches!(
        answered(end, &mut inbound, id),
        Some(Answer::Exited(_))
      ));
      release(end, id);
      spent
    })
    .collect()
}

/// [`REPS`] direct spawns of `/usr/bin/true`, each reaped: each spawn call's time.
fn direct() -> Vec<Duration> {
  (0..REPS)
    .map(|_| {
      let began = Instant::now();
      let mut child = crate::confined_spawn::spawn(&true_program()).unwrap();
      let spent = began.elapsed();
      child.wait().unwrap();
      spent
    })
    .collect()
}

/// `/usr/bin/true`, its streams the null device.
fn true_program() -> Spawn<'static> {
  Spawn {
    program: Path::new("/usr/bin/true"),
    args: &[],
    environment: Environment::Inherited,
    overrides: &[],
    directory: Path::new("/"),
    own_group: true,
    stdin: ChildStream::Null,
    stdout: ChildStream::Null,
    stderr: ChildStream::Null,
    handed: &[],
  }
}

/// The red's measurement, in a process of its own: the spawner started first, while this process holds few
/// descriptors; then at each of [`DESCRIPTOR_COUNTS`] open descriptors, [`REPS`] spawns through it and as many direct,
/// each count's figures printed on a [`COST_TAG`] line.
#[test]
fn spawn_cost_role() {
  if std::env::var_os(MEASURE_VARIABLE).is_none() {
    return;
  }
  let spawner = spawner(1);
  let end = spawner.ends.first().unwrap();
  let room = descriptor_room();
  let null = std::fs::File::open("/dev/null").unwrap();
  let mut held: Vec<OwnedFd> = Vec::new();
  for (round, wanted) in DESCRIPTOR_COUNTS.iter().enumerate() {
    let count = (*wanted).min(room);
    if count < *wanted {
      println!("{CAP_TAG} {wanted} {count}");
    }
    while held.len() < count {
      held.push(null.as_fd().try_clone_to_owned().unwrap());
    }
    let first_id = u64::try_from(round * REPS).unwrap() + 1;
    let spawner_took = took(through_the_spawner(end, first_id));
    let direct_took = took(direct());
    println!(
      "{COST_TAG} {count} {} {} {} {} {} {}",
      spawner_took.least,
      spawner_took.median,
      spawner_took.most,
      direct_took.least,
      direct_took.median,
      direct_took.most
    );
  }
  std::process::exit(0);
}

/// How many more descriptors this process may hold: its soft `RLIMIT_NOFILE` raised as far as the system lets it toward
/// the largest of [`DESCRIPTOR_COUNTS`], less those open now and the role's own ([`ROLE_OWN_DESCRIPTORS`]).
fn descriptor_room() -> usize {
  use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
  let open_now = std::fs::read_dir("/dev/fd").map_or(0, Iterator::count);
  let wanted =
    DESCRIPTOR_COUNTS.iter().max().copied().unwrap_or(0) + open_now + ROLE_OWN_DESCRIPTORS;
  let limit = getrlimit(Resource::Nofile);
  let raised = u64::try_from(wanted)
    .unwrap()
    .min(limit.maximum.unwrap_or(u64::MAX));
  let _ = setrlimit(
    Resource::Nofile,
    Rlimit {
      current: Some(raised),
      maximum: limit.maximum,
    },
  );
  let current = getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX);
  usize::try_from(current)
    .unwrap_or(usize::MAX)
    .saturating_sub(open_now + ROLE_OWN_DESCRIPTORS)
}

/// A-924's red (main's ruling of 2026-11-25). Do: run [`spawn_cost_role`] in a process of its own. Expect: through the
/// spawner, the least spawn's time on the asking thread at the most descriptors the process may hold, up to 100,000,
/// within the spread measured at 100 (the most at 100 plus its own spread); and, so the measure is not vacuous, on macOS
/// and x86_64 Linux the direct spawn's least there past the same bound drawn from its own figures at 100. The least,
/// not the median: a copy of the table raises every spawn, the least too, while a busy machine lifts a median past a
/// tight bound (1 of 4 runs on the MacBook at load 30 judged by the median). Red before: a
/// hook's spawn was the direct one, 8 to 13 ms at 100,000 against 0.2 at 100 on macOS, 532 µs at 10,000 against 193 at
/// 100 on x86_64 Linux. Where the process may not hold 100,000 (a runner's hard limit of 65,536) the role holds what it
/// may and says so; red there before was the role refused its descriptors (main's diag run 45666757721).
#[test]
fn a_spawns_cost_on_its_calling_thread_is_flat_in_the_daemons_descriptors() {
  // Spawns a process, so holds the descriptor tests' gate across it (`crate::descriptor_test_gate`).
  let _gate = crate::descriptor_test_gate();
  let program = std::env::current_exe().unwrap();
  let output = std::process::Command::new(program)
    .args(role_args("spawn_cost_role"))
    .env(MEASURE_VARIABLE, "1")
    .output()
    .unwrap();
  let printed = String::from_utf8_lossy(&output.stdout);
  let rows: Vec<Vec<u128>> = printed
    .lines()
    .filter_map(|line| line.split(COST_TAG).nth(1))
    .map(|row| {
      row
        .split_whitespace()
        .filter_map(|word| word.parse().ok())
        .collect()
    })
    .collect();
  eprintln!("{printed}");
  assert_eq!(
    rows.len(),
    DESCRIPTOR_COUNTS.len(),
    "a row a count: {printed}"
  );
  let figure = |row: usize, at: usize| {
    rows
      .get(row)
      .and_then(|row| row.get(at))
      .copied()
      .unwrap_or(0)
  };
  let bound =
    |least: usize, most: usize| figure(0, most) + figure(0, most).saturating_sub(figure(0, least));
  let last = DESCRIPTOR_COUNTS.len() - 1;
  assert!(
    figure(last, 1) <= bound(1, 3),
    "through the spawner, {} ns at least at {} descriptors against {} ns at {}: {printed}",
    figure(last, 1),
    figure(last, 0),
    bound(1, 3),
    figure(0, 0)
  );
  // Not on arm Linux, whose direct spawn grows too gently for the bound (284 to 333 µs a spawn from 100 to 10,000
  // descriptors on ubuntu-24.04-arm, main's diag run 45666757721), where x86_64's grew 2.75 times.
  // Its bound is drawn from the median at 100 and its own spread below it, which no one slow spawn widens: drawn from the
  // most, one direct spawn of 6.79 ms at 100 (its median 1.54) lifted it past a least of 4.9 ms at 100,000 (MacBook,
  // 2026-11-25, beside the crate's other tests).
  #[cfg(any(target_os = "macos", all(target_os = "linux", target_arch = "x86_64")))]
  let steady = |least: usize, median: usize| {
    figure(0, median) + figure(0, median).saturating_sub(figure(0, least))
  };
  #[cfg(any(target_os = "macos", all(target_os = "linux", target_arch = "x86_64")))]
  assert!(
    figure(last, 4) > steady(4, 5),
    "the direct spawn's cost grows with the descriptors, so the measure can see growth: {printed}"
  );
}

/// Main 01a20182's diag run 45672799755 (both Linux lanes, 2026-11-25): a child started through the spawner held a
/// descriptor a direct child did not (its listing "0 1 142 145 2 3 6" against "0 1 142 145 2 3"); macOS passed, its
/// spawn closing by default what its file actions do not name. Do: send a descriptor over a stream pair by `SCM_RIGHTS`
/// as a spawn request carries a child's stream, and read it with the spawner's [`Inbound`]. Expect: the received
/// descriptor close-on-exec, so no child the spawner starts on Linux inherits it. Red before: received without the flag,
/// on every platform (Linux's spawn then handed it to the child).
#[test]
fn a_descriptor_riding_a_request_is_received_close_on_exec() {
  let (sender, receiver) = crate::stream::pair().expect("a pair");
  let (_read, write) = std::io::pipe().expect("a pipe");
  let request = frame(FrameKind::Spawn, 1, &[]).expect("a frame");
  super::send(sender.as_fd(), &request, &[write.as_fd()]).expect("sent with the descriptor");
  let mut inbound = Inbound::default();
  wait_ready(&receiver, rustix::event::PollFlags::IN);
  assert_eq!(inbound.read(receiver.as_fd()).expect("read"), Read::Bytes);
  let received = inbound.take_descriptors(1);
  let flags = received
    .first()
    .map(|fd| rustix::io::fcntl_getfd(fd).expect("its flags"));
  assert_eq!(
    flags.map(|flags| flags.contains(rustix::io::FdFlags::CLOEXEC)),
    Some(true),
    "the received descriptor is close-on-exec"
  );
}
