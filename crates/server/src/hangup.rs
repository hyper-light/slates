//! A client's hang-up: the event its death is seen at (§4.7 "Failure matrix", A-475). Each seated client has a watch,
//! a task of its shard that awaits the hang-up through the shard's driver and then reports it (`crate::reap::watch`), so
//! a dead client is retired an event turn after its death. Until A-475 the reaper found the dead by a poll: a sweep each
//! liveness budget asked every client silent for a budget whether its peer was gone, so a client killed with `SIGKILL`
//! kept its seat, its attachments and its id for one to two budgets, and every shard woke each second to ask.
//!
//! What a hang-up is, by platform (paired `#[cfg]` arms of one signature):
//! - **Linux: the control socket's end of stream.** The rendezvous keeps a control socket per client, whose peer end the
//!   kernel closes when the client's process ends, however it ends. The watch awaits it readable (`EPOLLIN`: "The
//!   associated file is available for read(2) operations"), which a stream socket is at its peer's close, raised with
//!   `EPOLLRDHUP` ("Stream socket peer closed connection") and `EPOLLHUP`, which "epoll_wait(2) will always wait for"
//!   [B: epoll_ctl(2)]; then a peek tells the close (zero bytes) from a stray byte. A client sends nothing on that socket
//!   after its opening, so a byte there is a broken channel, not a live sign: ended as a hang-up is, counted apart
//!   ([`Ended::Broke`]), since the watch could not tell its death from its bytes. (The poll it replaces took a waiting
//!   byte for a live client, so a client that wrote one and died was never reaped.)
//! - **macOS: the client's process exit.** No control socket outlives a macOS rendezvous, so the watch is on the process
//!   the claim named: `EVFILT_PROC` with `NOTE_EXIT` ("The process has exited") on a kqueue of the watch's own
//!   (`slates_ipc::exit_watch`), bound to the process, not its pid [B: kqueue(2), macOS]; the shard's kqueue awaits that
//!   kqueue readable, which it is once the exit is pending.
//! - **Windows: the client's process handle**, opened for `SYNCHRONIZE`, signalled at the exit and awaited through the
//!   shard by the system thread pool (`slates_rt::handle_wait`).
//!
//! **Register, then test once.** Each wait registers with the shard first and then asks once whether the client is
//! already gone (a peek; the exit watch read with a zero timeout; the handle asked with a zero timeout), so a client that
//! died before its watch was registered is caught at the registration, not at a later event.
//!
//! **Cancel before close.** The watch holds a descriptor of its own: a duplicate of the control socket (Linux), its exit
//! kqueue (macOS), its process handle (Windows). Its wait borrows it, so the wait is dropped before the descriptor closes,
//! however the watch ends: at the hang-up, or cancelled when its seat ends first (a stranger's slot the session gate
//! ends, `crate::reap::end_unproven`). A closed descriptor takes its kernel registration with it once nothing else
//! refers to its file (kqueue(2): "Calling close() on a file descriptor will remove any kevents that reference the
//! descriptor"; epoll(7) A6: only "after all the file descriptors referring to the underlying open file description
//! have been closed", so a cancelled Linux watch's registration goes with its seat's socket, which a seat's end closes
//! in the same turn). Every seat is bounded (`clients_per_shard`), so the watches and their descriptors are too: one task
//! and one descriptor a client, both counted in the shard's task bound (`config::TASKS_PER_CLIENT`).

use slates_ipc::rendezvous::platform::Control;
use slates_rt::RtError;

/// What ended a client, as its watch saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
  /// The client hung up: its control socket read end of stream or failed (Linux), or its process exited.
  HungUp,
  /// The client wrote on its control socket, which a client never does after its opening (Linux): ended as a hang-up
  /// is, since its watch can no longer tell its death from its bytes.
  #[cfg(target_os = "linux")]
  Broke,
}

/// Why a client's hang-up cannot be watched, so it is not seated: nothing would reclaim it (A-475).
#[derive(Debug)]
pub(crate) enum Unwatchable {
  /// The rendezvous kept no control socket for it (Linux), so nothing closes at its death.
  #[cfg(target_os = "linux")]
  NoControl,
  /// The system refused the watch its own descriptor, or no hang-up is watched on this platform.
  Refused(slates_ipc::IpcError),
}

impl std::fmt::Display for Unwatchable {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      #[cfg(target_os = "linux")]
      Unwatchable::NoControl => f.write_str("no control socket was kept for it"),
      Unwatchable::Refused(refusal) => write!(f, "the watch's descriptor was refused: {refusal}"),
    }
  }
}

/// The wait on one client's hang-up, holding the descriptor it waits on (the module doc).
pub(crate) struct Hangup {
  inner: platform::Hangup,
}

impl Hangup {
  /// The watch on a client the rendezvous admitted with `control` and process `pid`.
  pub(crate) fn of_client(control: Option<&Control>, pid: u32) -> Result<Hangup, Unwatchable> {
    platform::of_client(control, pid).map(|inner| Hangup { inner })
  }

  /// The watch on the client whose control socket's daemon end is `socket` (Linux): a test's seat, which keeps no
  /// rendezvous control.
  #[cfg(all(test, target_os = "linux"))]
  pub(crate) fn of_socket(socket: std::os::fd::BorrowedFd<'_>) -> Result<Hangup, Unwatchable> {
    platform::of_socket(socket).map(|inner| Hangup { inner })
  }

  /// The watch on the client whose process is `pid` (macOS, Windows): a test's seat.
  #[cfg(all(test, any(target_os = "macos", windows)))]
  pub(crate) fn of_process(pid: u32) -> Result<Hangup, Unwatchable> {
    platform::of_process(pid).map(|inner| Hangup { inner })
  }

  /// Waits through the shard until the client hangs up: registers, then tests once. Refused when the shard refuses the
  /// wait (a driver that cannot register it, or a wait polled off a shard).
  pub(crate) async fn wait(&self) -> Result<Ended, RtError> {
    self.inner.wait().await
  }
}

/// Polls `wait` once with the polling task's own context, so a wait through the shard registers, and answers what that
/// poll answered: the registration's moment, after which a test once catches what happened before it. A wait that
/// answered here is not polled again.
#[cfg(unix)]
async fn poll_once<F: std::future::Future>(
  mut wait: std::pin::Pin<&mut F>,
) -> std::task::Poll<F::Output> {
  std::future::poll_fn(move |cx| std::task::Poll::Ready(wait.as_mut().poll(cx))).await
}

#[cfg(target_os = "linux")]
mod platform {
  use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
  use std::task::Poll;

  use rustix::net::RecvFlags;
  use slates_ipc::rendezvous::platform::Control;
  use slates_rt::RtError;

  use super::{Ended, Unwatchable};

  /// A duplicate of the client's control socket, the watch's own: the peer's close reaches every descriptor of the
  /// socket, and a seat's socket and its watch's end apart (the module doc).
  pub(super) struct Hangup {
    socket: OwnedFd,
  }

  pub(super) fn of_client(control: Option<&Control>, _pid: u32) -> Result<Hangup, Unwatchable> {
    let control = control.ok_or(Unwatchable::NoControl)?;
    of_socket(control.socket.as_fd())
  }

  pub(super) fn of_socket(socket: BorrowedFd<'_>) -> Result<Hangup, Unwatchable> {
    rustix::io::fcntl_dupfd_cloexec(socket, 0)
      .map(|socket| Hangup { socket })
      .map_err(|errno| {
        Unwatchable::Refused(slates_ipc::IpcError::OsRefused {
          call: "fcntl(F_DUPFD_CLOEXEC) of a client's control socket",
          code: Some(errno.raw_os_error()),
        })
      })
  }

  impl Hangup {
    pub(super) async fn wait(&self) -> Result<Ended, RtError> {
      let raw = self.socket.as_raw_fd();
      loop {
        let mut ready = std::pin::pin!(slates_rt::readiness::readable(raw));
        let first = super::poll_once(ready.as_mut()).await;
        if let Some(ended) = probe(&self.socket) {
          return Ok(ended);
        }
        match first {
          Poll::Ready(registered) => registered?,
          Poll::Pending => ready.await?,
        }
        if let Some(ended) = probe(&self.socket) {
          return Ok(ended);
        }
      }
    }
  }

  /// What the socket says without waiting: end of stream or a failed socket is a hang-up, a byte a broken channel, and
  /// nothing to read a live client.
  fn probe(socket: &OwnedFd) -> Option<Ended> {
    let mut byte = [0u8; 1];
    match rustix::net::recv(socket, &mut byte, RecvFlags::PEEK | RecvFlags::DONTWAIT) {
      Ok((0, _)) => Some(Ended::HungUp),
      Ok(_) => Some(Ended::Broke),
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => None,
      // Reset, or an unusable descriptor: the channel is gone either way.
      Err(_) => Some(Ended::HungUp),
    }
  }
}

#[cfg(target_os = "macos")]
mod platform {
  use std::os::fd::AsRawFd;
  use std::task::Poll;

  use slates_ipc::exit_watch::ExitWatch;
  use slates_ipc::rendezvous::platform::Control;
  use slates_rt::RtError;

  use super::{Ended, Unwatchable};

  /// The exit watch on the client's process: a kqueue of the watch's own holding `NOTE_EXIT` for it.
  pub(super) struct Hangup {
    exit: ExitWatch,
  }

  pub(super) fn of_client(_control: Option<&Control>, pid: u32) -> Result<Hangup, Unwatchable> {
    of_process(pid)
  }

  pub(super) fn of_process(pid: u32) -> Result<Hangup, Unwatchable> {
    ExitWatch::on(pid)
      .map(|exit| Hangup { exit })
      .map_err(Unwatchable::Refused)
  }

  impl Hangup {
    pub(super) async fn wait(&self) -> Result<Ended, RtError> {
      loop {
        // No descriptor once the exit was read, or when the process was gone at the watch.
        let Some(queue) = self.exit.readiness() else {
          return Ok(Ended::HungUp);
        };
        let mut ready = std::pin::pin!(slates_rt::readiness::readable(queue.as_raw_fd()));
        let first = super::poll_once(ready.as_mut()).await;
        if self.exit.exited() {
          return Ok(Ended::HungUp);
        }
        match first {
          Poll::Ready(registered) => registered?,
          Poll::Pending => ready.await?,
        }
        if self.exit.exited() {
          return Ok(Ended::HungUp);
        }
      }
    }
  }
}

#[cfg(windows)]
mod platform {
  use slates_ipc::exit_watch::ExitWatch;
  use slates_ipc::rendezvous::platform::Control;
  use slates_rt::RtError;

  use super::{Ended, Unwatchable};

  /// The exit watch on the client's process: its handle, opened for `SYNCHRONIZE`.
  pub(super) struct Hangup {
    exit: ExitWatch,
  }

  pub(super) fn of_client(_control: Option<&Control>, pid: u32) -> Result<Hangup, Unwatchable> {
    of_process(pid)
  }

  pub(super) fn of_process(pid: u32) -> Result<Hangup, Unwatchable> {
    ExitWatch::on(pid)
      .map(|exit| Hangup { exit })
      .map_err(Unwatchable::Refused)
  }

  impl Hangup {
    pub(super) async fn wait(&self) -> Result<Ended, RtError> {
      // No handle when the process was gone at the watch.
      let Some(process) = self.exit.handle() else {
        return Ok(Ended::HungUp);
      };
      slates_rt::handle_wait::signalled(process)
        .await
        .map(|()| Ended::HungUp)
    }
  }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
  use slates_ipc::rendezvous::platform::Control;
  use slates_rt::RtError;

  use super::{Ended, Unwatchable};

  /// No hang-up is watched on a platform slates does not ship.
  pub(super) struct Hangup;

  pub(super) fn of_client(_control: Option<&Control>, _pid: u32) -> Result<Hangup, Unwatchable> {
    Err(Unwatchable::Refused(slates_ipc::IpcError::Unsupported {
      feature: "a client's hang-up on this platform",
    }))
  }

  impl Hangup {
    pub(super) async fn wait(&self) -> Result<Ended, RtError> {
      Ok(Ended::HungUp)
    }
  }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;
  use std::future::Future;
  use std::pin::Pin;
  use std::sync::mpsc::Receiver;
  use std::task::{Context, Poll};
  use std::time::Instant;

  use slates_rt::RtError;
  use slates_rt::runtime::{LocalRuntime, RuntimeConfig};

  use super::{Ended, Hangup};

  std::thread_local! {
    /// The polls of the watch under test: the test's thread runs its shard, so the count is the thread's own.
    static POLLS: Cell<u32> = const { Cell::new(0) };
  }

  /// Shape: how long a watch is let wait before the test gives up on it: a liveness budget, the earliest the sweep the
  /// watch replaced asked about a client, and far past a kill's delivery.
  const BOUND_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
  /// Shape: how long the shard runs after a cancelled watch's client ends, for a stray completion of its wait to arrive:
  /// a tenth of a liveness budget, far past a kill's or a close's delivery.
  const SETTLE_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 10;

  /// What a watch answered within the bound: its end, its refusal, or nothing.
  type Answer = Result<Option<Result<Ended, RtError>>, RtError>;

  /// The watch under test, its polls counted in [`POLLS`].
  struct Counted<F>(Pin<Box<F>>);

  impl<F: Future> Future for Counted<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
      POLLS.with(|polls| polls.set(polls.get().saturating_add(1)));
      self.0.as_mut().poll(cx)
    }
  }

  /// Shape: a runtime for one watch: one shard, a few task slots.
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

  /// Spawns the watch of `hangup` on `runtime` and runs its first poll (its registration and its test): the task, and
  /// where its answer arrives.
  fn start(runtime: &LocalRuntime, hangup: Hangup) -> (slates_rt::TaskId, Receiver<Answer>) {
    POLLS.with(|polls| polls.set(0));
    let (sent, answer) = std::sync::mpsc::channel();
    let task = runtime
      .spawn(async move {
        let watched = Counted(Box::pin(async move { hangup.wait().await }));
        let waited = slates_rt::futures::within(BOUND_NS, watched).await;
        let _ = sent.send(waited);
      })
      .expect("the watch is spawned");
    let _ = runtime.context().step();
    assert_eq!(POLLS.with(Cell::get), 1, "the watch's first poll ran");
    (task, answer)
  }

  /// A client standing in for a real one: a child process, and the watch on it.
  struct Client {
    child: std::process::Child,
    hangup: Hangup,
  }

  /// A client whose control socket's other end its child process holds as its standard input (Linux): the watch is on
  /// the daemon's end, which closes here, so the watch's own is the daemon's only one.
  #[cfg(target_os = "linux")]
  fn watched_client() -> Client {
    use std::os::fd::AsFd;
    let (seat_end, client_end) = rustix::net::socketpair(
      rustix::net::AddressFamily::UNIX,
      rustix::net::SocketType::STREAM,
      rustix::net::SocketFlags::CLOEXEC,
      None,
    )
    .expect("a socket pair");
    let child = std::process::Command::new("sleep")
      .arg("30")
      .stdin(std::process::Stdio::from(client_end))
      .spawn()
      .expect("a client process");
    let hangup = Hangup::of_socket(seat_end.as_fd()).expect("a watch on the socket");
    Client { child, hangup }
  }

  /// A client process watched by its process (macOS).
  #[cfg(target_os = "macos")]
  fn watched_client() -> Client {
    let child = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .expect("a client process");
    let hangup = Hangup::of_process(child.id()).expect("a watch on the process");
    Client { child, hangup }
  }

  /// A client process watched by its process (Windows).
  #[cfg(windows)]
  fn watched_client() -> Client {
    let child = std::process::Command::new("cmd")
      .args(["/C", "ping", "-n", "30", "127.0.0.1"])
      .spawn()
      .expect("a client process");
    let hangup = Hangup::of_process(child.id()).expect("a watch on the process");
    Client { child, hangup }
  }

  /// A-475 ("a client killed with SIGKILL is retired within one event turn, not a liveness budget"). Do: watch a client
  /// (Linux: its control socket, the other end held by a child process; macOS and Windows: the child process), let the
  /// watch register, kill the child with `SIGKILL`, and run the shard. Expect: the watch answers `HungUp` at its second
  /// poll, the one the kill's event woke, far inside a liveness budget.
  #[test]
  fn a_client_killed_with_sigkill_is_seen_at_the_next_event_turn() {
    let runtime = runtime();
    let Client { mut child, hangup } = watched_client();
    let (_task, answer) = start(&runtime, hangup);
    let killed = Instant::now();
    child.kill().expect("the client is killed");
    runtime.run_until_idle();
    let seen = killed.elapsed();
    child.wait().expect("the client is reaped");
    let answer = answer.try_recv().expect("the watch answered");
    let polls = POLLS.with(Cell::get);
    println!("a client killed with SIGKILL: seen {seen:?} after the kill, at poll {polls}");
    assert!(
      matches!(answer, Ok(Some(Ok(Ended::HungUp)))),
      "the watch answered {answer:?}"
    );
    assert_eq!(polls, 2, "seen at the one poll the kill's event woke");
  }

  /// A-475 ("one that died before registration is caught at registration"). Do: make a client's watch while it lives,
  /// then kill the child and reap it before the watch's task first runs. Expect: the watch answers `HungUp` at that
  /// first poll: it registers, then tests once, so a death before the registration is caught there and not at a later
  /// event.
  #[test]
  fn a_client_dead_before_its_watch_registered_is_caught_at_the_registration() {
    let runtime = runtime();
    let Client { mut child, hangup } = watched_client();
    child.kill().expect("the client is killed");
    child.wait().expect("the client is reaped");
    let (_task, answer) = start(&runtime, hangup);
    let answer = answer
      .try_recv()
      .expect("the watch answered at its first poll");
    assert!(
      matches!(answer, Ok(Some(Ok(Ended::HungUp)))),
      "the watch answered {answer:?}"
    );
  }

  /// A-475 ("a cancelled wait closes cleanly"), Linux. Do: watch a socket's end while its peer lives, cancel the watch's
  /// task after its registration (as a seat that ends first does), run the shard, close the seat's own end, then read the
  /// peer's end and close it with the shard running. Expect: the watch never answers (its future was dropped); the peer
  /// reads the daemon's close, so the watch's own descriptor closed with its wait; and no completion of the cancelled
  /// wait reaches the shard (`interests_unclaimed` unchanged).
  #[cfg(target_os = "linux")]
  #[test]
  fn a_cancelled_watch_leaves_no_wait_and_no_descriptor_behind() {
    use std::os::fd::AsFd;
    let runtime = runtime();
    let (seat_end, client_end) = rustix::net::socketpair(
      rustix::net::AddressFamily::UNIX,
      rustix::net::SocketType::STREAM,
      rustix::net::SocketFlags::CLOEXEC,
      None,
    )
    .expect("a socket pair");
    let hangup = Hangup::of_socket(seat_end.as_fd()).expect("a watch on the socket");
    let (task, answer) = start(&runtime, hangup);
    runtime
      .context()
      .cancel(task)
      .expect("the watch is cancelled");
    runtime.run_until_idle();
    drop(seat_end);
    let mut byte = [0u8; 1];
    let read = rustix::net::recv(
      &client_end,
      &mut byte,
      rustix::net::RecvFlags::PEEK | rustix::net::RecvFlags::DONTWAIT,
    );
    let unclaimed = runtime.context().counters().interests_unclaimed;
    drop(client_end);
    settle(&runtime);
    assert!(
      matches!(read, Ok((0, _))),
      "the peer read {read:?}, not the daemon's close"
    );
    assert!(answer.try_recv().is_err(), "a cancelled watch answered");
    assert_eq!(
      runtime.context().counters().interests_unclaimed,
      unclaimed,
      "a completion of the cancelled wait reached the shard"
    );
  }

  /// A-475 ("a cancelled wait closes cleanly"), macOS and Windows. Do: watch a live client process, cancel the watch's
  /// task after its registration (as a seat that ends first does), run the shard, then kill the child with the shard
  /// running. Expect: the watch never answers (its future was dropped), and no completion of the cancelled wait reaches
  /// the shard: no readiness completion went unclaimed, and no task but the shard's settling one was polled.
  #[cfg(any(target_os = "macos", windows))]
  #[test]
  fn a_cancelled_watch_leaves_no_wait_and_no_descriptor_behind() {
    let runtime = runtime();
    let Client { mut child, hangup } = watched_client();
    let (task, answer) = start(&runtime, hangup);
    runtime
      .context()
      .cancel(task)
      .expect("the watch is cancelled");
    runtime.run_until_idle();
    let before = runtime.context().counters();
    child.kill().expect("the client is killed");
    settle(&runtime);
    child.wait().expect("the client is reaped");
    let after = runtime.context().counters();
    assert!(answer.try_recv().is_err(), "a cancelled watch answered");
    assert_eq!(
      after.interests_unclaimed, before.interests_unclaimed,
      "a completion of the cancelled wait reached the shard"
    );
    assert_eq!(
      after.polls.saturating_sub(before.polls),
      2,
      "a task beside the settling one was polled"
    );
  }

  /// Runs the shard for [`SETTLE_NS`], parked in its driver, so a completion that arrives meanwhile is taken: a task that
  /// sleeps that long, polled twice (its timer armed, then fired).
  fn settle(runtime: &LocalRuntime) {
    runtime
      .spawn(async {
        let _ = slates_rt::futures::sleep(SETTLE_NS).await;
      })
      .expect("the settling task is spawned");
    runtime.run_until_idle();
  }
}
