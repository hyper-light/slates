//! The process's own lines: a log line on standard error ([`log_line!`](crate::log_line)) and an output line on standard
//! output ([`out_line!`](crate::out_line)), each one write that cannot panic (CLAUDE.md §2, item 6).
//!
//! **Why not `eprintln!` and `println!`.** Both panic when their write is refused, and Rust ignores `SIGPIPE`, so a
//! write to a pipe whose reader has gone is refused `EPIPE`: `slates anchor 2>&1 >/dev/null | head -n 1` aborted the
//! anchor (exit 134) at its first line after `head` had its own, and its daemon, which writes into the same pipe, with
//! it. A daemon serving held mounts cannot end because nobody reads its log.
//!
//! **What a refusal does.** A refused log line is counted ([`log_lines_refused`]) and dropped: the process goes on, and
//! the daemon's status reports the count, as the anchor's own (`slates status`: `log_lines_refused`,
//! `anchor_log_lines_refused`). A refused output line is counted too ([`out_lines_refused`]); the command whose output
//! could not be delivered reports it as its failure at exit (`crates/cli/src/main.rs`), since its caller did not get
//! what it asked for. The workspace lint wall denies `print_stdout` and `print_stderr` outside tests, so every line goes
//! through here.
//!
//! The counters are statistics, read by status and at exit, so `Relaxed`.
//!
//! **A thread that must never wait on the log (A-305).** A daemon's shard threads are judged by its watchdog (A-228,
//! `slates_server::watchdog`): a shard that stands still for a window, never runnable, is wedged, and a daemon nobody
//! supervises ends its own process on that (A-232). A line written straight to standard error takes the lock every line
//! of the process takes and then waits for the write itself, so a shard behind another thread starved while it held that
//! lock, or behind a write the log's reader was slow to take, stood still a window and was ended: test binaries exited 4
//! at a daemon's start under load (merge_post_state, merge_load, nfs_hostile, nfs_mount, nfs_writeback, green_checkpoint,
//! fleet; 2026-11-17, loads 40 to 50), each with a line cut mid-write at the exit and the watchdog's own end line lost
//! behind the same lock. A daemon now owns a [`LogWriter`] and routes each of its shard threads to it ([`LogRoute`]): a
//! routed thread's line goes into the writer's bounded queue and the call returns at once, while the writer's own thread
//! waits on standard error for every routed thread. A full queue refuses the line, counted as any refused line is: a
//! daemon serving held mounts cannot be stopped by a log it cannot write. Every other thread writes as before, so the
//! CLI's and the anchor's lines keep their order with their output.

use std::cell::RefCell;
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};

/// Log lines this process could not write, over its life.
static LOG_LINES_REFUSED: AtomicU64 = AtomicU64::new(0);
/// Output lines this process could not write, over its life.
static OUT_LINES_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Writes `line` and a newline to standard error, one locked write; a refused write is counted and dropped. On a thread
/// routed to a [`LogWriter`] ([`LogRoute::install`]) the line goes into the writer's queue instead, and the call never
/// waits on standard error.
pub fn log(line: fmt::Arguments<'_>) {
  // `try_with`: a line written while the thread's locals are torn down (a value's drop at the thread's end) finds the
  // route gone, and is written directly rather than panicking.
  let routed = ROUTE
    .try_with(|route| {
      route
        .try_borrow()
        .ok()
        .and_then(|route| route.as_ref().map(|route| route.send(line)))
    })
    .ok()
    .flatten();
  match routed {
    Some(true) => {}
    Some(false) => {
      LOG_LINES_REFUSED.fetch_add(1, Ordering::Relaxed);
    }
    None => write_line(line),
  }
}

/// Writes `line` and a newline to standard error as one locked write, counting a refusal.
fn write_line(line: impl fmt::Display) {
  let mut stderr = std::io::stderr().lock();
  if writeln!(stderr, "{line}").is_err() {
    LOG_LINES_REFUSED.fetch_add(1, Ordering::Relaxed);
  }
}

/// Derived: the bytes of lines a [`LogWriter`] holds while standard error does not take them: a pipe's capacity on Linux
/// and macOS, 65,536 bytes (pipe(7): "Since Linux 2.6.11, the pipe capacity is 16 pages"; XNU's `BIG_PIPE_SIZE`), so no
/// more of the log waits in this process than the kernel holds for a reader that has fallen behind. A daemon's start
/// writes a few hundred bytes a shard.
const QUEUE_BYTES: usize = 65_536;

std::thread_local! {
  /// The writer this thread's lines go to, once it is routed ([`LogRoute::install`]); `None` for a thread that writes
  /// its lines itself.
  static ROUTE: RefCell<Option<LogRoute>> = const { RefCell::new(None) };
}

/// The queue's bytes in flight and its bound, shared by a writer and the threads routed to it. One per writer, leaked for
/// the process's life: two words.
#[derive(Debug)]
#[repr(align(128))]
struct Queued {
  bytes: AtomicUsize,
  capacity: usize,
}

/// A thread's way to a [`LogWriter`]: the sending side of its queue. Made by [`LogWriter::route`] and installed on the
/// thread whose lines it carries ([`LogRoute::install`]); dropped with that thread.
#[derive(Debug)]
pub struct LogRoute {
  lines: Sender<String>,
  queued: &'static Queued,
}

impl LogRoute {
  /// Routes the calling thread's log lines to this route's writer from now until the thread ends.
  pub fn install(self) {
    let _installed = ROUTE.try_with(|route| {
      if let Ok(mut route) = route.try_borrow_mut() {
        *route = Some(self);
      }
    });
  }

  /// Queues `line` for the writer if the queue has room for it: whether it was taken.
  fn send(&self, line: fmt::Arguments<'_>) -> bool {
    let text = line.to_string();
    let bytes = text.len();
    let taken = self
      .queued
      .bytes
      .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
        queued
          .checked_add(bytes)
          .filter(|after| *after <= self.queued.capacity)
      })
      .is_ok();
    if !taken {
      return false;
    }
    if self.lines.send(text).is_err() {
      self.queued.bytes.fetch_sub(bytes, Ordering::AcqRel);
      return false;
    }
    true
  }
}

/// The owned thread that writes the lines of the threads routed to it (see the module docs): it waits on standard error
/// so they never do. Started by its owner (the daemon, before its shards), it ends once its owner has stopped it and
/// every routed thread has ended, having written every line queued before.
#[derive(Debug)]
pub struct LogWriter {
  /// The route its owner clones for each thread; dropped at the stop, so the thread ends when the routed threads have.
  route: Option<LogRoute>,
  handle: Option<std::thread::JoinHandle<()>>,
  /// Told when the thread has written its last line.
  ended: Receiver<()>,
}

impl LogWriter {
  /// Starts the writer's thread, its queue bounded at [`QUEUE_BYTES`].
  pub fn start() -> std::io::Result<LogWriter> {
    LogWriter::start_with(QUEUE_BYTES)
  }

  /// Starts the writer's thread with its queue bounded at `capacity` bytes.
  fn start_with(capacity: usize) -> std::io::Result<LogWriter> {
    let (lines, queue) = channel::<String>();
    let (ended_sender, ended) = channel::<()>();
    let queued: &'static Queued = Box::leak(Box::new(Queued {
      bytes: AtomicUsize::new(0),
      capacity,
    }));
    let handle = std::thread::Builder::new()
      .name("slates-log".to_owned())
      .spawn(move || {
        for line in queue {
          queued.bytes.fetch_sub(line.len(), Ordering::AcqRel);
          write_line(line);
        }
        // An owner that stopped waiting (a stop past its wait) is told nothing more.
        let _told = ended_sender.send(());
      })?;
    Ok(LogWriter {
      route: Some(LogRoute { lines, queued }),
      handle: Some(handle),
      ended,
    })
  }

  /// A route to this writer, for one thread to install; `None` once the writer is stopping.
  pub fn route(&self) -> Option<LogRoute> {
    self.route.as_ref().map(|route| LogRoute {
      lines: route.lines.clone(),
      queued: route.queued,
    })
  }

  /// Stops the writer: lets its route go and waits `within` for the thread to write what is queued and end, then joins
  /// it. Whether it ended: a thread still holding a route (a stuck shard's, which nothing joins), or a standard error
  /// that takes nothing, keeps it past the wait, and it is left to the process's end rather than waited for without end.
  pub fn stop(&mut self, within: std::time::Duration) -> bool {
    drop(self.route.take());
    let Some(handle) = self.handle.take() else {
      return true;
    };
    match self.ended.recv_timeout(within) {
      Ok(()) | Err(RecvTimeoutError::Disconnected) => handle.join().is_ok(),
      Err(RecvTimeoutError::Timeout) => false,
    }
  }
}

/// Writes `text` to standard output, then a newline when `newline`, and flushes, so a refusal is seen here rather than
/// at a later flush; a refused write is counted and dropped.
pub fn out(text: fmt::Arguments<'_>, newline: bool) {
  let mut stdout = std::io::stdout().lock();
  let written = if newline {
    writeln!(stdout, "{text}")
  } else {
    write!(stdout, "{text}")
  };
  if written.and_then(|()| stdout.flush()).is_err() {
    OUT_LINES_REFUSED.fetch_add(1, Ordering::Relaxed);
  }
}

/// Log lines this process could not write so far.
pub fn log_lines_refused() -> u64 {
  LOG_LINES_REFUSED.load(Ordering::Relaxed)
}

/// Output lines this process could not write so far.
pub fn out_lines_refused() -> u64 {
  OUT_LINES_REFUSED.load(Ordering::Relaxed)
}

/// A log line on standard error, formatted as `eprintln!` formats it, that cannot panic ([`log`]).
#[macro_export]
macro_rules! log_line {
  ($($argument:tt)*) => {
    $crate::log::log(::std::format_args!($($argument)*))
  };
}

/// An output line on standard output, formatted as `println!` formats it, that cannot panic ([`out`]).
#[macro_export]
macro_rules! out_line {
  () => {
    $crate::log::out(::std::format_args!(""), true)
  };
  ($($argument:tt)*) => {
    $crate::log::out(::std::format_args!($($argument)*), true)
  };
}

/// Output on standard output without a newline, formatted as `print!` formats it, that cannot panic ([`out`]).
#[macro_export]
macro_rules! out_text {
  ($($argument:tt)*) => {
    $crate::log::out(::std::format_args!($($argument)*), false)
  };
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Serializes the tests that read the process's refusal counters against the one that moves them on purpose.
  #[allow(clippy::disallowed_types)]
  fn counter_gate() -> std::sync::MutexGuard<'static, ()> {
    // structural: allow — D-8 exception 3: a test harness; the gate's owners are this module's counter tests.
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
  }

  /// The panic rule. Do: write a log line and an output line in this test process, whose streams libtest captures.
  /// Expect: both return, and neither counts a refusal.
  #[test]
  fn a_written_line_counts_no_refusal() {
    let _gate = counter_gate();
    let (log_before, out_before) = (log_lines_refused(), out_lines_refused());
    crate::log_line!("a log line {}", 1);
    crate::out_line!("an output line {}", 2);
    assert_eq!(log_lines_refused(), log_before);
    assert_eq!(out_lines_refused(), out_before);
  }

  /// Shape: how long a test waits for a routed thread's lines while standard error is held: far past any line's queueing,
  /// so only a thread that waits on standard error misses it.
  const ROUTED_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
  /// Shape: the test writer's queue, in bytes. The first line is 42 bytes and each after it 39; the writer takes the
  /// first off the queue before or after some of the others and then waits on the held lock, so three or two of the
  /// eight after it fit and five or six are refused.
  const TEST_QUEUE_BYTES: usize = 128;

  /// A-305. Do: hold standard error's lock on this thread, as a thread starved while writing a line holds it; on a thread
  /// routed to a log writer, write a line, then lines past the queue's bytes; let the lock go and stop the writer.
  /// Expect: the routed thread's calls all return while the lock is held, the lines past the queue counted refused; once
  /// it is let go the writer writes what it queued and ends. Unrouted, the first line waited for the lock: a shard so
  /// held for a window was wedged, and a lone daemon ended its process.
  #[test]
  fn a_routed_thread_never_waits_on_standard_error_and_a_full_queue_refuses() {
    let _gate = counter_gate();
    let mut writer = LogWriter::start_with(TEST_QUEUE_BYTES).unwrap();
    let route = writer.route().unwrap();
    let refused_before = log_lines_refused();
    let held = std::io::stderr().lock();
    let (done, finished) = std::sync::mpsc::channel();
    let routed = std::thread::spawn(move || {
      route.install();
      crate::log_line!("a routed line while standard error is held");
      for count in 0..8 {
        crate::log_line!("a routed line past the queue's bytes, {count}");
      }
      let _ = done.send(());
    });
    let returned = finished.recv_timeout(ROUTED_WAIT);
    let refused = log_lines_refused().saturating_sub(refused_before);
    drop(held);
    routed.join().unwrap();
    assert!(
      returned.is_ok(),
      "a routed thread's lines returned while standard error was held"
    );
    assert!(
      (5..=6).contains(&refused),
      "the lines past the queue's {TEST_QUEUE_BYTES} bytes were refused: {refused}"
    );
    assert!(
      writer.stop(ROUTED_WAIT),
      "the writer wrote what it queued and ended"
    );
  }
}
