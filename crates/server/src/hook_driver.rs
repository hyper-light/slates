//! The hook driver (§4.17.1 "Policy: protected branches, access and hooks"; A-622 part 3): one configured hook run as a
//! process of its own over a read-only view of the work, held to A-597's runner (`crate::vcs_hooks::HookRun`).
//!
//! **The launch.** The hook's program, by its absolute path from the repository's policy (A-622 part 1), is started
//! with its point's arguments, environment and standard input in its tool's terms (`crate::vcs_hooks`'s builders), its
//! working directory the root of the read-only attachment it runs over (the tool's server makes that view: git's bare
//! repository with the quarantine, Mercurial's revlog view, Subversion's transaction; Mercurial's `HG_PENDING` names its
//! root), in a process group of its own, so the hook and every process it starts can be ended together. Its words are
//! one pipe: standard output and standard error joined for git and Mercurial, standard error alone for Subversion, whose
//! standard output goes to the null device (A-597). On Linux the hook is meant to run inside the launcher's namespace
//! (`slates exec`, §4.6); that confinement is the launcher's piece, not this driver's.
//!
//! **The run.** One task of the shard drives the runner: it feeds standard input as the pipe takes it (a hook that
//! closes its input early is no failure: git stops feeding at a write error too), reads the words as they arrive, and
//! awaits the hook's exit through the shard's driver (`slates_ipc::exit_watch::ExitWatch`: `NOTE_EXIT` on a kqueue of
//! its own on macOS, a pidfd on Linux), the runner's deadline on the shard's wheel, and its client leaving, whichever
//! comes first, and does what the runner answers: wait, kill the hook's process group, or take the outcome. Nothing
//! is polled: every wait is an event.
//!
//! **The deadline.** The bound is the one the hook's times learned (A-597): none before its first finished run, when
//! its client leaving is its only end; the run's lesson (a finished length, or a cut at the bound) is carried back to
//! the caller, which teaches the hook's times on the shard, since a task cannot hold them across its awaits.
//!
//! **No orphan.** The hook's process is owned until it is reaped: the run returns only after its exit was read and the
//! child waited for; a run whose task is dropped first (its push gone) kills the hook's process group and leaves the
//! child to a reaper task of the shard, which waits on its exit watch until it is reaped. A process the hook starts and
//! moves out of its group escapes the group's kill; holding those too is the launcher's namespace's (§4.6).
//!
//! **Platforms.** Unix (Linux and macOS) here; Windows starts a hook through `slates_ipc::win_spawn` and ends its
//! descendants with a job object, owed as A-622's Windows part, so this module is built on Unix alone.

use std::future::Future;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::pin::{Pin, pin};
use std::process::{Child, Command, Stdio};
use std::task::Poll;

use slates_ipc::exit_watch::ExitWatch;
use slates_rt::futures;

use crate::vcs_hooks::{
  EnvValue, Event, ExitStatus as HookExit, HookInput, HookRun, Lesson, Outcome, StartRefused, Step,
  WordsFrom,
};

/// What a hook is launched with (the module doc).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Launch<'a> {
  /// The program, by the absolute path the repository's policy names.
  pub(crate) program: &'a Path,
  /// Its point's arguments, environment and standard input (A-597's builders).
  pub(crate) input: &'a HookInput,
  /// The root of the read-only view it runs over: its working directory, and the root `EnvValue::ViewRoot` names.
  pub(crate) view_root: &'a Path,
  /// How many bytes of its words are kept, a bound its caller derives (A-597's `Words`).
  pub(crate) words_cap: usize,
}

/// Counter: hooks whose process the run's task was dropped before reaping, killed and left to a reaper task.
const HOOK_REAP_HANDED: &str = "hook.reap_handed";
/// Counter: hooks killed and reaped where no reaper task could be started, waited for on the shard at once.
const HOOK_REAP_WAITED: &str = "hook.reap_waited";

/// Runs one hook to its outcome on this shard (the module doc), held to `bound_ns` (its hook's learned bound, none
/// before its first finished run) and ended when `client_gone` resolves; the hook is reaped before this returns.
/// Answers the outcome and the run's lesson for its hook's times.
pub(crate) async fn run(
  launch: Launch<'_>,
  bound_ns: Option<u64>,
  client_gone: impl Future<Output = ()>,
) -> (Outcome, Lesson) {
  let mut lesson = Lesson::default();
  let mut hook = match Hook::spawn(launch) {
    Ok(hook) => hook,
    Err(cause) => return (HookRun::unstartable(cause), lesson),
  };
  let (mut run, mut step) = HookRun::with_bound(futures::now_ns(), bound_ns, launch.words_cap);
  let mut client_gone = pin!(client_gone);
  // Once the client's future has answered it is never polled again (a finished future must not be).
  let mut client_left = false;
  loop {
    match step {
      Step::Done {
        outcome,
        end_leftovers,
      } => {
        if end_leftovers {
          hook.kill_group();
        }
        hook.reap();
        return (outcome, lesson);
      }
      Step::Kill => {
        hook.kill_group();
        step = Step::Wait { wake_at_ns: None };
      }
      Step::Wait { wake_at_ns } => {
        let happened = hook
          .next(wake_at_ns, client_gone.as_mut(), &mut client_left)
          .await;
        step = match &happened {
          Happened::Output(bytes) => run.on(Event::Output(bytes), &mut lesson),
          Happened::OutputEnded => run.on(Event::OutputEnded, &mut lesson),
          Happened::Exited { status, at_ns } => run.on(
            Event::Exited {
              status: *status,
              at_ns: *at_ns,
            },
            &mut lesson,
          ),
          Happened::Timer { now_ns } => run.on(Event::Timer { now_ns: *now_ns }, &mut lesson),
          Happened::ClientLeft => run.on(Event::ClientLeft, &mut lesson),
        };
      }
    }
  }
}

/// What the run's next event is, owned.
#[derive(Debug)]
enum Happened {
  /// The hook wrote these bytes.
  Output(Vec<u8>),
  /// Its words stream ended.
  OutputEnded,
  /// Its process exited, read and reaped.
  Exited {
    /// How.
    status: HookExit,
    /// When, on the shard's clock.
    at_ns: u64,
  },
  /// The deadline came.
  Timer {
    /// The shard's clock.
    now_ns: u64,
  },
  /// The client left (or the shard refused a wait it needed, which ends the run as a client's leaving does).
  ClientLeft,
}

/// Which wait answered first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Woke {
  /// The words stream is readable.
  Words,
  /// Standard input is writable.
  Input,
  /// The exit is pending.
  Exit,
  /// The deadline came.
  Deadline,
  /// The client left.
  Client,
  /// The shard refused a wait.
  Refused,
}

/// A started hook: its child, owned until reaped, and the ends of its pipes this side holds. Its exit is read without
/// reaping it (`waitid` with `WNOWAIT`), so its process-group id stays reserved by its unreaped leader until the run
/// ends, and killing the group then cannot reach a group another process formed under a reused id.
struct Hook {
  /// The child, `None` once reaped.
  child: Option<Child>,
  /// The watch on its exit.
  exit: ExitWatch,
  /// Its standard input's write end, non-blocking, until all of it is written or the hook closed its end.
  input: Option<OwnedFd>,
  /// What of its standard input is left to write.
  pending: Vec<u8>,
  /// Its words stream's read end, non-blocking, until the stream ends.
  words: Option<OwnedFd>,
  /// Whether its watch has reported its exit (which a watch reports once).
  exit_seen: bool,
  /// Whether its exit status was read (the child still unreaped).
  exited: bool,
  /// One read's room: the machine's page.
  buffer: Vec<u8>,
}

impl Hook {
  /// Starts the hook (the module doc): `StartRefused` by the operating system's cause.
  fn spawn(launch: Launch<'_>) -> Result<Self, StartRefused> {
    let (words_read, words_write) = std::io::pipe().map_err(|error| start_refused(&error))?;
    let mut command = Command::new(launch.program);
    command
      .args(launch.input.args.iter().map(|arg| std::ffi::OsStr::from_bytes(arg)))
      .current_dir(launch.view_root)
      .process_group(0)
      .stdin(Stdio::piped());
    for (name, value) in &launch.input.env {
      match value {
        EnvValue::Bytes(bytes) => command.env(name, std::ffi::OsStr::from_bytes(bytes)),
        EnvValue::ViewRoot => command.env(name, launch.view_root),
        EnvValue::Unset => command.env_remove(name),
      };
    }
    match launch.input.point.words_from() {
      WordsFrom::BothStreams => {
        let joined = words_write
          .try_clone()
          .map_err(|error| start_refused(&error))?;
        command.stdout(Stdio::from(joined));
      }
      WordsFrom::ErrorStream => {
        command.stdout(Stdio::null());
      }
    }
    command.stderr(Stdio::from(words_write));
    let mut child = command.spawn().map_err(|error| start_refused(&error))?;
    // The command holds this side's copies of the words stream's write end: dropped, so the stream ends when the hook
    // and what it started close theirs.
    drop(command);
    let input = child.stdin.take().map(OwnedFd::from);
    let words = OwnedFd::from(words_read);
    let exit = match ExitWatch::on(child.id()) {
      Ok(exit) => exit,
      Err(_) => {
        // Its exit cannot be awaited: killed with its group at once, and waited for (`SIGKILL` makes the wait short).
        kill_group_of(&child);
        let _ = child.wait();
        return Err(StartRefused::NoRoom);
      }
    };
    let ready = [input.as_ref(), Some(&words)]
      .into_iter()
      .flatten()
      .all(|fd| non_blocking(fd).is_ok());
    let mut hook = Self {
      child: Some(child),
      exit,
      input,
      pending: launch.input.stdin.clone(),
      words: Some(words),
      exit_seen: false,
      exited: false,
      buffer: vec![0; rustix::param::page_size()],
    };
    if !ready {
      hook.kill_group();
      hook.reap();
      return Err(StartRefused::NoRoom);
    }
    if hook.pending.is_empty() {
      hook.input = None;
    }
    Ok(hook)
  }

  /// The hook's next event: what is ready now, else the first of its waits to answer; `client_left` once the client's
  /// future has answered, which is then not polled again.
  async fn next(
    &mut self,
    wake_at_ns: Option<u64>,
    client: Pin<&mut impl Future<Output = ()>>,
    client_left: &mut bool,
  ) -> Happened {
    let mut client = client;
    loop {
      self.feed();
      if let Some(happened) = self.read_words().or_else(|| self.read_exit()) {
        return happened;
      }
      let now_ns = futures::now_ns();
      if wake_at_ns.is_some_and(|at| now_ns >= at) {
        return Happened::Timer { now_ns };
      }
      match self.wait(wake_at_ns, client.as_mut(), client_left).await {
        Woke::Client | Woke::Refused => return Happened::ClientLeft,
        Woke::Deadline => {
          return Happened::Timer {
            now_ns: futures::now_ns(),
          };
        }
        Woke::Words | Woke::Input | Woke::Exit => {}
      }
    }
  }

  /// Waits for the first of: the words readable, the input writable, the exit, the deadline, the client leaving (asked
  /// only while it has not answered).
  async fn wait(
    &self,
    wake_at_ns: Option<u64>,
    client: Pin<&mut impl Future<Output = ()>>,
    client_left: &mut bool,
  ) -> Woke {
    let words = self.words.as_ref().map(|fd| fd.as_raw_fd());
    let input = self.input.as_ref().map(|fd| fd.as_raw_fd());
    let exit = (!self.exit_seen)
      .then(|| self.exit.readiness().map(|fd| fd.as_raw_fd()))
      .flatten();
    let mut words = pin!(async move {
      match words {
        Some(fd) => slates_rt::readiness::readable(fd).await,
        None => std::future::pending().await,
      }
    });
    let mut input = pin!(async move {
      match input {
        Some(fd) => slates_rt::readiness::writable(fd).await,
        None => std::future::pending().await,
      }
    });
    let mut exit = pin!(async move {
      match exit {
        Some(fd) => slates_rt::readiness::readable(fd).await,
        None => std::future::pending().await,
      }
    });
    let mut deadline = pin!(async move {
      match wake_at_ns {
        Some(at) => futures::sleep(at.saturating_sub(futures::now_ns())).await,
        None => std::future::pending().await,
      }
    });
    let mut client = client;
    std::future::poll_fn(|cx| {
      let waits: [(Woke, Poll<Result<(), slates_rt::RtError>>); 4] = [
        (Woke::Words, words.as_mut().poll(cx)),
        (Woke::Input, input.as_mut().poll(cx)),
        (Woke::Exit, exit.as_mut().poll(cx)),
        (Woke::Deadline, deadline.as_mut().poll(cx)),
      ];
      for (woke, polled) in waits {
        match polled {
          Poll::Ready(Ok(())) => return Poll::Ready(woke),
          Poll::Ready(Err(_)) => return Poll::Ready(Woke::Refused),
          Poll::Pending => {}
        }
      }
      if !*client_left && client.as_mut().poll(cx).is_ready() {
        *client_left = true;
        return Poll::Ready(Woke::Client);
      }
      Poll::Pending
    })
    .await
  }

  /// Writes what standard input takes now; at its end, or at a write error (the hook closed its end), closes it.
  fn feed(&mut self) {
    while let Some(fd) = self.input.as_ref() {
      if self.pending.is_empty() {
        self.input = None;
        return;
      }
      match rustix::io::write(fd, &self.pending) {
        Ok(written) => {
          self.pending.drain(..written.min(self.pending.len()));
        }
        Err(rustix::io::Errno::AGAIN) => return,
        Err(rustix::io::Errno::INTR) => {}
        Err(_) => {
          self.pending.clear();
          self.input = None;
        }
      }
    }
  }

  /// Bytes the words stream holds now, or its end (or a read error, which ends it too); none when it would wait.
  fn read_words(&mut self) -> Option<Happened> {
    let fd = self.words.as_ref()?;
    loop {
      match rustix::io::read(fd, self.buffer.as_mut_slice()) {
        Ok(0) => break,
        Ok(read) => return Some(Happened::Output(self.buffer.get(..read)?.to_vec())),
        Err(rustix::io::Errno::INTR) => {}
        Err(rustix::io::Errno::AGAIN) => return None,
        Err(_) => break,
      }
    }
    self.words = None;
    Some(Happened::OutputEnded)
  }

  /// The hook's exit, once its watch has it: its status read without reaping it (the struct's doc); none before, and
  /// none twice. A status not yet readable when the watch fired is read again at the run's next wake (its words' end,
  /// which an exit brings, or its deadline or its client).
  fn read_exit(&mut self) -> Option<Happened> {
    if self.exited {
      return None;
    }
    if !self.exit_seen {
      self.exit_seen = self.exit.exited();
    }
    if !self.exit_seen {
      return None;
    }
    let pid = self
      .child
      .as_ref()
      .and_then(|child| i32::try_from(child.id()).ok())
      .and_then(rustix::process::Pid::from_raw)?;
    let options = rustix::process::WaitIdOptions::EXITED
      | rustix::process::WaitIdOptions::NOWAIT
      | rustix::process::WaitIdOptions::NOHANG;
    let status = rustix::process::waitid(rustix::process::WaitId::Pid(pid), options).ok()??;
    self.exited = true;
    let status = match (status.exit_status(), status.terminating_signal()) {
      (_, Some(signal)) => HookExit::Signal(signal),
      (Some(code), None) => HookExit::Code(code),
      (None, None) => HookExit::Code(status.raw_signo()),
    };
    Some(Happened::Exited {
      status,
      at_ns: futures::now_ns(),
    })
  }

  /// Kills the hook's process group: the hook and what it started that stayed in its group.
  fn kill_group(&self) {
    if let Some(child) = self.child.as_ref() {
      kill_group_of(child);
    }
  }

  /// Reaps the child when its exit was read; otherwise leaves it to `Drop`, which kills and reaps it.
  fn reap(&mut self) {
    if let Some(child) = self.child.as_mut() {
      if let Ok(Some(_)) = child.try_wait() {
        self.child = None;
      }
    }
  }
}

impl Drop for Hook {
  /// A hook not yet reaped (its run's task dropped first, or ended with leftovers): its group killed, and its child
  /// reaped by a reaper task of the shard, or waited for here when none can be started.
  fn drop(&mut self) {
    let Some(mut child) = self.child.take() else {
      return;
    };
    if let Ok(Some(_)) = child.try_wait() {
      return;
    }
    kill_group_of(&child);
    let exit = ExitWatch::on(child.id()).ok();
    let handed = futures::spawn(reap_killed(child, exit)).and_then(futures::detach);
    match handed {
      Ok(()) => count(HOOK_REAP_HANDED),
      Err(_) => count(HOOK_REAP_WAITED),
    }
  }
}

/// Counts `counter` on this shard, when there is one.
fn count(counter: &'static str) {
  let _ = crate::state::with_state_counted(|state| state.count(counter, 1));
}

/// Waits for a killed hook's exit and reaps it.
async fn reap_killed(mut child: Child, exit: Option<ExitWatch>) {
  loop {
    if let Ok(Some(_)) = child.try_wait() {
      return;
    }
    let ready = exit
      .as_ref()
      .and_then(|exit| exit.readiness().map(|fd| fd.as_raw_fd()));
    match ready {
      Some(fd) if slates_rt::readiness::readable(fd).await.is_ok() => {}
      // No watch to wait on: the group was sent `SIGKILL`, so the child's wait is short.
      _ => {
        let _ = child.wait();
        return;
      }
    }
  }
}

/// Kills `child`'s process group, which its unreaped leader keeps reserved; refused only when the group has gone, which
/// is what the kill was for.
fn kill_group_of(child: &Child) {
  if let Some(pid) = i32::try_from(child.id())
    .ok()
    .and_then(rustix::process::Pid::from_raw)
  {
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
  }
}

/// Makes `fd` non-blocking.
fn non_blocking(fd: &OwnedFd) -> rustix::io::Result<()> {
  let flags = rustix::fs::fcntl_getfl(fd.as_fd())?;
  rustix::fs::fcntl_setfl(fd.as_fd(), flags | rustix::fs::OFlags::NONBLOCK)
}

/// Why a hook could not be started, by the operating system's error.
fn start_refused(error: &std::io::Error) -> StartRefused {
  match error.kind() {
    std::io::ErrorKind::NotFound => StartRefused::NotFound,
    std::io::ErrorKind::PermissionDenied => StartRefused::NotPermitted,
    std::io::ErrorKind::OutOfMemory => StartRefused::NoRoom,
    _ => match error.raw_os_error().map(rustix::io::Errno::from_raw_os_error) {
      Some(rustix::io::Errno::AGAIN | rustix::io::Errno::MFILE | rustix::io::Errno::NFILE) => {
        StartRefused::NoRoom
      }
      other => StartRefused::Other(other.map_or(0, |errno| errno.raw_os_error())),
    },
  }
}

#[cfg(test)]
mod tests {
  use std::io::Read;
  use std::path::PathBuf;
  use std::sync::mpsc::channel;

  use slates_rt::runtime::{LocalRuntime, RuntimeConfig};

  use super::{Launch, run};
  use crate::daemon::LIVENESS_BUDGET_NS;
  use crate::vcs_hooks::{
    EnvValue, ExitStatus, HookInput, HookPoint, HookTimes, Learns, Lesson, Outcome, PendingView,
    StartRefused, View,
  };

  /// Format: the variable that makes this test binary, re-invoked, a hook of the role it names.
  const ROLE: &str = "SLATES_HOOK_ROLE";
  /// Format: the libtest name of the role test the binary is re-invoked with.
  const ROLE_TEST: &str = "hook_driver::tests::a_hook_role";
  /// Format: the tag before each line a role writes, since libtest's banner shares its standard output.
  const TAG: &str = "slates-hook:";
  /// Shape: how long a sleeping role sleeps: far past any bound the tests learn, so only a kill ends it.
  const SLEEP_SECONDS: u64 = 30;
  /// Shape: how many bytes of a hook's words the tests keep.
  const CAP: usize = 1 << 16;
  /// Shape: a hook run as a quick check takes it, 30 ms.
  const QUICK_NS: u64 = 30_000_000;
  /// Shape: how long a test lets a run take before giving up on it: ten liveness windows.
  const TEST_BOUND_NS: u64 = 10 * LIVENESS_BUDGET_NS;
  /// Shape: when a test's client leaves: a fifth of a liveness window.
  const LEAVES_AFTER_NS: u64 = LIVENESS_BUDGET_NS / 5;
  /// Shape: a hook's standard input larger than any pipe holds, a mebibyte.
  const LARGE_INPUT: usize = 1 << 20;
  /// Format: the exit code the refusing role exits with.
  const REFUSED: i32 = 3;

  /// The role this binary plays when re-invoked as a hook (`ROLE`); a test that does nothing otherwise.
  #[test]
  fn a_hook_role() {
    let Ok(role) = std::env::var(ROLE) else {
      return;
    };
    match role.as_str() {
      "accept" => {
        let mut input = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut input);
        let args: Vec<String> = std::env::args().skip_while(|arg| arg != "--").skip(1).collect();
        println!("{TAG}stdin={}", input.len());
        println!("{TAG}args={}", args.join("|"));
        println!("{TAG}node={}", std::env::var("HG_NODE").unwrap_or_default());
        println!("{TAG}pending={}", std::env::var("HG_PENDING").unwrap_or_default());
        println!("{TAG}count={}", std::env::var("GIT_PUSH_OPTION_COUNT").is_ok());
        println!("{TAG}cwd={}", std::env::current_dir().unwrap().display());
        std::process::exit(0);
      }
      "refuse" => {
        println!("{TAG}to standard output");
        eprintln!("{TAG}refused by the hook");
        std::process::exit(REFUSED);
      }
      "sleep" => {
        std::thread::sleep(std::time::Duration::from_secs(SLEEP_SECONDS));
        std::process::exit(0);
      }
      "linger" => {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
          .args([ROLE_TEST, "--exact", "--nocapture"])
          .env(ROLE, "sleep")
          .spawn()
          .unwrap();
        println!("{TAG}lingering={}", child.id());
        std::process::exit(0);
      }
      _ => std::process::exit(0),
    }
  }

  /// A one-shard runtime for a driven run.
  fn runtime() -> LocalRuntime {
    LocalRuntime::new(&RuntimeConfig {
      shards: 1,
      tasks_per_shard: 8,
      timers_per_shard: 8,
      ring_entries: 8,
      step_budget_ns: 1_000_000,
      timer_tick_ns: 100_000,
      batch: 8,
      pin: false,
      cores: Vec::new(),
      page_bytes: 4096,
      spin_ns: 0,
      wake_tracking: None,
    })
    .expect("a local runtime")
  }

  /// A hook's input playing `role` at `point`, its own arguments `args` after libtest's, standard input `stdin`.
  fn input(point: HookPoint, role: &str, args: &[&str], stdin: Vec<u8>) -> HookInput {
    let mut all: Vec<Vec<u8>> = [ROLE_TEST, "--exact", "--nocapture", "--"]
      .iter()
      .map(|arg| arg.as_bytes().to_vec())
      .collect();
    all.extend(args.iter().map(|arg| arg.as_bytes().to_vec()));
    HookInput {
      point,
      args: all,
      env: vec![
        (ROLE.to_owned(), EnvValue::Bytes(role.as_bytes().to_vec())),
        ("HG_NODE".to_owned(), EnvValue::Bytes(b"aabb".to_vec())),
        ("HG_PENDING".to_owned(), EnvValue::ViewRoot),
        ("GIT_PUSH_OPTION_COUNT".to_owned(), EnvValue::Unset),
      ],
      stdin,
      view: View::Pending(PendingView(0)),
    }
  }

  /// The test binary, and the directory it sits in, which stands for a hook's read-only view.
  fn program_and_view() -> (PathBuf, PathBuf) {
    let program = std::env::current_exe().unwrap();
    let view = program.parent().unwrap().to_path_buf();
    (program, view)
  }

  /// What a run answered: its outcome and lesson, and how long it took; `None` past the test's bound.
  type Ran = Option<(Outcome, Lesson, u64)>;

  /// Runs `hook` (by `program`, `/` standing for the test binary) to its outcome, held to `bound_ns`, its client
  /// leaving after `leaves_after_ns` when given.
  fn drive(program: Option<&str>, hook: HookInput, bound_ns: Option<u64>, leaves_after_ns: Option<u64>) -> Ran {
    let runtime = runtime();
    let (sent, got) = channel();
    let (binary, view) = program_and_view();
    let program = program.map_or(binary, PathBuf::from);
    runtime
      .spawn(async move {
        let launch = Launch {
          program: &program,
          input: &hook,
          view_root: &view,
          words_cap: CAP,
        };
        let client = async move {
          match leaves_after_ns {
            Some(ns) => {
              let _ = slates_rt::futures::sleep(ns).await;
            }
            None => std::future::pending().await,
          }
        };
        let began = slates_rt::futures::now_ns();
        let ran = slates_rt::futures::within(TEST_BOUND_NS, run(launch, bound_ns, client)).await;
        let took = slates_rt::futures::now_ns().saturating_sub(began);
        let _ = sent.send(ran.ok().flatten().map(|(outcome, lesson)| (outcome, lesson, took)));
      })
      .expect("the run's task");
    runtime.run_until_idle();
    got.recv().expect("the run answered")
  }

  /// A hook's times after eight quick runs finished: a bound a little past a liveness window.
  fn learned() -> HookTimes {
    let mut times = HookTimes::default();
    for _ in 0..8 {
      times.finished(QUICK_NS);
    }
    times
  }

  /// A hook's words, as text.
  fn text(outcome: &Outcome) -> String {
    match outcome {
      Outcome::Accepted { words } | Outcome::Refused { words, .. } | Outcome::TimedOut { words, .. } => {
        String::from_utf8_lossy(&words.kept).into_owned()
      }
      _ => String::new(),
    }
  }

  /// Whether process `pid` is gone, asked until a liveness window has passed.
  fn gone(pid: i32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_nanos(LIVENESS_BUDGET_NS);
    while std::time::Instant::now() < deadline {
      let alive = rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok());
      if !alive {
        return true;
      }
      std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
  }

  /// A-622 part 3 (the launch). Do: run a hook that reads its standard input and reports its arguments, environment and
  /// working directory. Expect: accepted, having read every input byte, with its point's arguments, `HG_NODE` as given,
  /// `HG_PENDING` and its working directory the view's root, `GIT_PUSH_OPTION_COUNT` removed; and a run that finished,
  /// so a sample for its hook's times.
  #[test]
  fn a_hook_runs_in_its_view_with_its_points_input() {
    let (_, view) = program_and_view();
    let stdin = b"0000 1111 refs/heads/main\n".to_vec();
    let length = stdin.len();
    let hook = input(HookPoint::GitPreReceive, "accept", &["refs/heads/main", "two"], stdin);
    let (outcome, lesson, _) = drive(None, hook, None, None).expect("the run ended");
    let words = text(&outcome);
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
    for line in [
      format!("{TAG}stdin={length}"),
      format!("{TAG}args=refs/heads/main|two"),
      format!("{TAG}node=aabb"),
      format!("{TAG}pending={}", view.display()),
      format!("{TAG}count=false"),
      format!("{TAG}cwd={}", view.display()),
    ] {
      assert!(words.contains(&line), "{line} in {words}");
    }
    let mut times = HookTimes::default();
    lesson.teach(&mut times);
    assert_eq!(times.finished_runs(), 1);
  }

  /// A-622 part 3 (refusals and words). Do: run a hook that writes a line to each stream and exits 3, at a git point
  /// (both streams its words) and at a Subversion point (standard error alone). Expect: refused with exit 3 both times,
  /// git's words holding both lines, Subversion's the standard error line alone.
  #[test]
  fn a_refusing_hook_is_refused_with_its_exit_and_its_points_words() {
    let both = drive(None, input(HookPoint::GitPreReceive, "refuse", &[], Vec::new()), None, None);
    let (outcome, _, _) = both.expect("the run ended");
    assert!(
      matches!(outcome, Outcome::Refused { status: ExitStatus::Code(REFUSED), .. }),
      "{outcome:?}"
    );
    let words = text(&outcome);
    assert!(words.contains("refused by the hook") && words.contains("to standard output"), "{words}");
    let error = drive(None, input(HookPoint::SvnPreCommit, "refuse", &[], Vec::new()), None, None);
    let (outcome, _, _) = error.expect("the run ended");
    let words = text(&outcome);
    assert!(words.contains("refused by the hook") && !words.contains("to standard output"), "{words}");
  }

  /// A-622 part 3 (the learned deadline, the group's kill, the reap). Do: with a learned bound, run a hook that sleeps.
  /// Expect: timed out at that bound, within a liveness window past it; the run cut, which backs its hook's bound off;
  /// and the hook's process gone.
  #[test]
  fn a_hook_past_its_learned_bound_is_killed_with_its_group_and_reaped() {
    let times = learned();
    let bound = times.bound_ns().expect("learned");
    let (outcome, lesson, took) =
      drive(None, input(HookPoint::GitUpdate, "sleep", &[], Vec::new()), Some(bound), None).expect("the run ended");
    assert!(
      matches!(outcome, Outcome::TimedOut { bound_ns, .. } if bound_ns == bound),
      "{outcome:?}"
    );
    assert!(took >= bound && took < bound + LIVENESS_BUDGET_NS, "ended {took} ns in, the bound {bound} ns");
    let mut taught = learned();
    lesson.teach(&mut taught);
    assert_eq!(taught.cut_runs(), 1);
    assert_eq!(taught.bound_ns(), Some(2 * bound));
  }

  /// A-622 part 3 (§4.17.1: "Before a hook has run, its deadline is its push's connection"). Do: with nothing learned,
  /// run a hook that sleeps while its client leaves after a fifth of a liveness window. Expect: abandoned, soon after the
  /// client left, and nothing learned.
  #[test]
  fn a_hooks_first_run_ends_when_its_client_leaves_and_teaches_nothing() {
    let (outcome, lesson, took) = drive(
      None,
      input(HookPoint::HgPretxnchangegroup, "sleep", &[], Vec::new()),
      None,
      Some(LEAVES_AFTER_NS),
    )
    .expect("the run ended");
    assert_eq!(outcome, Outcome::Abandoned);
    assert!(took >= LEAVES_AFTER_NS && took < LIVENESS_BUDGET_NS, "ended {took} ns in");
    assert_eq!(lesson, Lesson::default());
  }

  /// A-622 part 3 (no orphan). Do: with a learned bound, run a hook that starts a sleeping child holding its words stream
  /// and exits 0. Expect: accepted by its own exit, the run held until the bound by the child's stream and then ended,
  /// and the child gone with its group.
  #[test]
  fn a_hook_whose_child_holds_its_output_is_judged_by_its_exit_and_its_child_ended() {
    let bound = learned().bound_ns().expect("learned");
    let (outcome, lesson, took) =
      drive(None, input(HookPoint::GitPostReceive, "linger", &[], Vec::new()), Some(bound), None).expect("the run ended");
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
    assert!(took >= bound, "held until the bound: {took} ns");
    let words = text(&outcome);
    let pid: i32 = words
      .split(&format!("{TAG}lingering="))
      .nth(1)
      .and_then(|rest| rest.lines().next())
      .and_then(|pid| pid.trim().parse().ok())
      .expect("the child's pid");
    assert!(gone(pid), "the lingering child {pid} was ended with its group");
    let mut taught = HookTimes::default();
    lesson.teach(&mut taught);
    assert_eq!(taught.finished_runs(), 1, "its exit was a sample");
  }

  /// A-622 part 3 (input that does not fit a pipe). Do: give a hook that reads nothing a mebibyte of standard input.
  /// Expect: accepted when it exits, the driver having stopped feeding at its closed input.
  #[test]
  fn a_hook_that_reads_none_of_a_large_input_ends_its_run_by_its_exit() {
    let (outcome, _, _) =
      drive(None, input(HookPoint::GitPreReceive, "early", &[], vec![b'x'; LARGE_INPUT]), None, None)
        .expect("the run ended");
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
  }

  /// A-622 part 3. Do: launch a program that does not exist. Expect: unstartable, not found.
  #[test]
  fn a_hook_that_cannot_start_is_unstartable_by_its_cause() {
    let (outcome, lesson, _) =
      drive(Some("/nonexistent/slates-hook"), input(HookPoint::GitUpdate, "accept", &[], Vec::new()), None, None)
        .expect("the run ended");
    assert_eq!(
      outcome,
      Outcome::Unstartable {
        cause: StartRefused::NotFound
      }
    );
    assert_eq!(lesson, Lesson::default());
  }
}
