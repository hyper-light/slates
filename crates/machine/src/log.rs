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

/// Formats `line` into one contiguous buffer before locking standard error, counting a refusal if
/// formatting fails, and passes it to `deliver`.
fn write_formatted(line: fmt::Arguments<'_>, deliver: impl FnOnce(String)) {
  let mut buffer = String::new();
  if fmt::write(&mut buffer, line).is_err() {
    LOG_LINES_REFUSED.fetch_add(1, Ordering::Relaxed);
    return;
  }
  deliver(buffer);
}

/// Writes `bytes` to `sink` in one `write_all` call, counting one refusal on error and returning
/// whether the write succeeded.
fn write_or_refuse(sink: &mut impl Write, bytes: &[u8]) -> bool {
  if sink.write_all(bytes).is_err() {
    LOG_LINES_REFUSED.fetch_add(1, Ordering::Relaxed);
    return false;
  }
  true
}

/// Writes one already-framed log record `bytes` to `sink` in one `write_all` call, counting a
/// refusal on error.
fn write_bytes_to(sink: &mut impl Write, bytes: &[u8]) {
  let _ = write_or_refuse(sink, bytes);
}

/// [`write_owned_to`] on standard error: the sink a [`LogWriter`] writes with.
fn write_owned(line: String) {
  let mut stderr = std::io::stderr().lock();
  write_owned_to(&mut stderr, line);
}

/// Writes `line` and a newline to standard error in one locked `write_all` call, counting a refusal.
fn write_line(line: fmt::Arguments<'_>) {
  write_formatted(line, write_owned);
}

/// Appends a trailing newline to `line` and writes it to `sink` in one single `write_all` call.
/// One `write_all` under stderr's lock guarantees single-process atomicity and POSIX pipe atomicity
/// up to `PIPE_BUF` (512 B on macOS, 4096 B on Linux); anchor + daemon cross-process lines of
/// arbitrary length are serialized through one writer via [`relay_stderr`]. Counts a refusal on
/// error.
fn write_owned_to(sink: &mut impl Write, mut line: String) {
  line.push('\n');
  write_bytes_to(sink, line.as_bytes());
}

/// Explicit state of the logical line currently being relayed from a child's stderr pipe.
/// Never holds a sink lock across `Read::read` so a stalled child cannot deadlock parent callers
/// of [`log`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelayLine {
  /// At a line boundary: bytes of a normal line buffer up to `cap`.
  Idle,
  /// Streaming an overlong (`>= cap`) logical line in bounded `<= cap` locked chunks until `\n`
  /// (or EOF, where a trailing `\n` is written).
  Streaming,
  /// A write for this logical line already failed (refusal counted once); draining remaining bytes
  /// until `\n` or EOF without re-locking or counting duplicate refusals.
  Refused,
}

/// Bounded per-line relay pairing the line buffer, lock factory, and logical-line state machine.
struct LineRelay<F> {
  cap: usize,
  line: Vec<u8>,
  state: RelayLine,
  acquire: F,
}

impl<W: Write, F: FnMut() -> W> LineRelay<F> {
  fn new(cap: usize, acquire: F) -> Self {
    Self {
      cap,
      line: Vec::new(),
      state: RelayLine::Idle,
      acquire,
    }
  }

  /// Acquires the sink lock once and writes a complete `<= cap` buffered line in one `write_all`.
  fn emit_buffered_line(&mut self, head: &[u8]) {
    let mut sink = (self.acquire)();
    if self.line.is_empty() {
      write_bytes_to(&mut sink, head);
    } else {
      self.line.extend_from_slice(head);
      write_bytes_to(&mut sink, &self.line);
      self.line.clear();
    }
  }

  /// Acquires the sink lock only for the immediate write of the first `cap` bytes of an overlong
  /// line, dropping the lock before returning to `Read::read`.
  fn begin_streaming(&mut self) {
    let ok = {
      let mut sink = (self.acquire)();
      write_or_refuse(&mut sink, &self.line)
    };
    self.line.clear();
    self.state = if ok {
      RelayLine::Streaming
    } else {
      RelayLine::Refused
    };
  }

  fn step_idle<'a>(&mut self, rest: &'a [u8]) -> &'a [u8] {
    let room = self.cap.saturating_sub(self.line.len());
    if let Some(nl) = rest.iter().position(|byte| *byte == b'\n')
      && nl < room
      && let Some((head, tail)) = rest.split_at_checked(nl.saturating_add(1))
    {
      self.emit_buffered_line(head);
      return tail;
    }
    let take = rest.len().min(room);
    let Some((head, tail)) = rest.split_at_checked(take) else {
      return &[];
    };
    self.line.extend_from_slice(head);
    if self.line.len() >= self.cap {
      self.begin_streaming();
    }
    tail
  }

  fn step_streaming<'a>(&mut self, rest: &'a [u8]) -> &'a [u8] {
    let (head, tail, ended) = match rest.iter().position(|byte| *byte == b'\n') {
      Some(nl) => match rest.split_at_checked(nl.saturating_add(1)) {
        Some((head, tail)) => (head, tail, true),
        None => (rest, &[][..], false),
      },
      None => (rest, &[][..], false),
    };
    let ok = {
      let mut sink = (self.acquire)();
      write_or_refuse(&mut sink, head)
    };
    self.state = if ended {
      RelayLine::Idle
    } else if ok {
      RelayLine::Streaming
    } else {
      RelayLine::Refused
    };
    tail
  }

  fn step_refused<'a>(&mut self, rest: &'a [u8]) -> &'a [u8] {
    match rest.iter().position(|byte| *byte == b'\n') {
      Some(nl) => {
        self.state = RelayLine::Idle;
        rest.get(nl.saturating_add(1)..).unwrap_or_default()
      }
      None => &[],
    }
  }

  fn feed(&mut self, mut rest: &[u8]) {
    while !rest.is_empty() {
      rest = match self.state {
        RelayLine::Idle => self.step_idle(rest),
        RelayLine::Streaming => self.step_streaming(rest),
        RelayLine::Refused => self.step_refused(rest),
      };
    }
  }

  /// Flushes any unterminated trailing line at EOF with a trailing `\n`.
  fn finish(mut self) {
    match self.state {
      RelayLine::Idle if !self.line.is_empty() => {
        self.line.push(b'\n');
        let mut sink = (self.acquire)();
        write_bytes_to(&mut sink, &self.line);
      }
      RelayLine::Streaming => {
        let mut sink = (self.acquire)();
        write_bytes_to(&mut sink, b"\n");
      }
      RelayLine::Idle | RelayLine::Refused => {}
    }
  }
}

/// Reads bytes from `reader` in `capacity`-byte chunks and writes to a lock guard returned by
/// `acquire`, dropping the guard before every `Read::read` call. Lines up to `capacity` bytes emit
/// atomically in one locked `write_all`; longer lines stream in bounded `<= capacity` locked
/// chunks until `\n` (or EOF, terminating with `\n`) and count at most one refusal per logical
/// line.
fn relay_stderr_bounded<W: Write>(
  mut reader: impl std::io::Read,
  capacity: usize,
  acquire: impl FnMut() -> W,
) {
  let cap = capacity.max(1);
  let mut chunk = vec![0u8; cap];
  let mut relay = LineRelay::new(cap, acquire);
  loop {
    match reader.read(&mut chunk) {
      Ok(0) => break,
      Ok(count) => relay.feed(chunk.get(..count).unwrap_or_default()),
      Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
      Err(_) => break,
    }
  }
  relay.finish();
}

/// Relays a supervised child's stderr pipe line-by-line into this process's standard error under
/// its single stderr lock, bounding per-line memory to [`QUEUE_BYTES`] and never holding the lock
/// across `Read::read`. Lines up to [`QUEUE_BYTES`] write atomically; longer lines stream
/// losslessly in bounded chunks with a trailing `\n` on unterminated EOF tails. If standard error
/// refuses writes (e.g. `EPIPE`), each failed logical line is counted once in
/// [`log_lines_refused`] while draining continues until EOF so the child never stalls on a full
/// pipe.
pub fn relay_stderr(reader: impl std::io::Read) {
  relay_stderr_bounded(reader, QUEUE_BYTES, || std::io::stderr().lock());
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

/// The queue's bytes in flight and its bound, shared by a writer and the threads routed to it, and the lines the writer
/// has written, which its stop reads as its progress (A-552). One per writer, leaked for the process's life: three words.
#[derive(Debug)]
#[repr(align(128))]
struct Queued {
  bytes: AtomicUsize,
  capacity: usize,
  written: AtomicU64,
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
      .try_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
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
  /// The queue the route sends into, kept past the route's drop for the stop to read the writer's progress.
  queued: &'static Queued,
  handle: Option<std::thread::JoinHandle<()>>,
  /// Told when the thread has written its last line.
  ended: Receiver<()>,
}

impl LogWriter {
  /// Starts the writer's thread, its queue bounded at [`QUEUE_BYTES`].
  pub fn start() -> std::io::Result<LogWriter> {
    LogWriter::start_with(QUEUE_BYTES)
  }

  /// Starts the writer's thread with its queue bounded at `capacity` bytes, writing to standard error.
  fn start_with(capacity: usize) -> std::io::Result<LogWriter> {
    LogWriter::start_with_sink(capacity, write_owned)
  }

  /// Starts the writer's thread with its queue bounded at `capacity` bytes, each line written by `sink`: standard error's
  /// writer, or a test's.
  fn start_with_sink(capacity: usize, sink: fn(String)) -> std::io::Result<LogWriter> {
    let (lines, queue) = channel::<String>();
    let (ended_sender, ended) = channel::<()>();
    let queued: &'static Queued = Box::leak(Box::new(Queued {
      bytes: AtomicUsize::new(0),
      capacity,
      written: AtomicU64::new(0),
    }));
    let handle = std::thread::Builder::new()
      .name("slates-log".to_owned())
      .spawn(move || {
        for line in queue {
          queued.bytes.fetch_sub(line.len(), Ordering::AcqRel);
          sink(line);
          // After the write: a writer held inside one shows no progress.
          queued.written.fetch_add(1, Ordering::Release);
        }
        // An owner that stopped waiting (a stop past its wait) is told nothing more.
        let _told = ended_sender.send(());
      })?;
    Ok(LogWriter {
      route: Some(LogRoute { lines, queued }),
      queued,
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

  /// Stops the writer: lets its route go, waits for the thread to write what is queued and end, and joins it. It waits
  /// while the writer goes on writing lines, its owner's liveness window `window` at a time, and gives it up at the first
  /// look a window after the last that finds no line written since (A-552): a standard error that takes lines slowly is
  /// waited for until it has them all, while one that takes nothing, or a thread still holding a route (a stuck
  /// shard's, which nothing joins) with nothing left to send, keeps the writer still, and it is left to the process's end
  /// rather than waited for without end. Whether it ended. Until A-552 the stop waited one window in all, so it gave up a
  /// slow, live writer with lines unwritten.
  pub fn stop(&mut self, window: std::time::Duration) -> bool {
    drop(self.route.take());
    let Some(handle) = self.handle.take() else {
      return true;
    };
    let mut written = self.queued.written.load(Ordering::Acquire);
    loop {
      match self.ended.recv_timeout(window) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => return handle.join().is_ok(),
        Err(RecvTimeoutError::Timeout) => {
          let now = self.queued.written.load(Ordering::Acquire);
          if now == written {
            return false;
          }
          written = now;
        }
      }
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

  /// Shape: the window the stop test's writers are stopped with. A std timed wait under a launchd job's timer policy
  /// (kiln's agents, CI's macOS runner) comes back about eight times late, at most about 150 ms late (measured
  /// 2026-10-26), so the window is long enough for a slow line taken that late to land inside it.
  const STOP_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);
  /// Shape: how long the stop test's slow sink takes a line: a sixth of the window, so even taken late it takes one well
  /// inside each window.
  const SLOW_LINE: std::time::Duration = std::time::Duration::from_millis(100);
  /// Shape: the lines the stop test queues: the slow sink takes at least two windows over them.
  const SLOW_LINES: usize = 12;

  /// A standard error that takes a line every [`SLOW_LINE`]: slow, never stuck.
  fn slow_sink(line: String) {
    // The point of the fixture: a reader that takes each line late.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(SLOW_LINE);
    drop(line);
  }

  /// A-552 (R3's class D: the daemon's log writer's drain wait). Do: route a thread to a writer whose standard error takes
  /// a line every sixth of a window, queue more lines than it takes in two windows, let the thread end, and stop the
  /// writer with a window; then stop a second writer while a route to it is still held, as a stuck shard's thread holds
  /// its own, with nothing queued. Expect the first stop to wait while the writer goes on taking lines and find it ended,
  /// every line written; the second to give its writer up once a window passes with no line taken. Before, a stop waited
  /// one window in all, so it gave a slow, live writer up with lines unwritten.
  #[test]
  fn a_stop_waits_while_its_writer_takes_lines_and_gives_up_a_still_one() {
    let _gate = counter_gate();
    let mut writer = LogWriter::start_with_sink(TEST_QUEUE_BYTES * SLOW_LINES, slow_sink).unwrap();
    let route = writer.route().unwrap();
    std::thread::spawn(move || {
      route.install();
      for count in 0..SLOW_LINES {
        crate::log_line!("a line for a slow standard error, {count}");
      }
    })
    .join()
    .unwrap();
    let began = std::time::Instant::now();
    let ended = writer.stop(STOP_WINDOW);
    let took = began.elapsed();
    assert!(
      ended,
      "a writer that went on taking lines was waited for: gave up after {took:?}"
    );
    assert!(
      took > STOP_WINDOW,
      "its lines took longer than a window to take: {took:?}"
    );
    let mut still = LogWriter::start_with_sink(TEST_QUEUE_BYTES, slow_sink).unwrap();
    let held = still.route().unwrap();
    let began = std::time::Instant::now();
    let ended = still.stop(STOP_WINDOW);
    let took = began.elapsed();
    drop(held);
    assert!(
      !ended,
      "a writer a held route keeps, with nothing to take, is given up"
    );
    assert!(took >= STOP_WINDOW, "after a still window: {took:?}");
  }

  /// Sink that records each individual `Write::write` slice (and can fail on demand) to prove a
  /// log line is emitted in one single `write` call with its trailing newline.
  struct RecordingSink {
    writes: Vec<Vec<u8>>,
    fail: bool,
  }

  impl Write for RecordingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      if self.fail {
        return Err(std::io::Error::new(
          std::io::ErrorKind::BrokenPipe,
          "peer closed",
        ));
      }
      self.writes.push(buf.to_vec());
      Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  /// SEAM-3: Do: write a formatted multi-argument log line and an owned log line to a recording
  /// sink, then write to a failing sink.
  /// Expect: each line arrives in ONE single `write` call including its trailing `\n` (never split
  /// across fragments that could interleave between anchor and daemon), and each failed write
  /// increments `log_lines_refused` without panicking.
  #[test]
  fn a_log_line_is_written_atomically_in_one_write_call_and_refusals_are_counted() {
    let _gate = counter_gate();
    let refused_before = log_lines_refused();

    let mut formatted = RecordingSink {
      writes: Vec::new(),
      fail: false,
    };
    write_formatted(
      format_args!(
        "slates anchor: issuer surface: export {} inode {}",
        "SLATES_ISSUER=token", 737_198_504_u64
      ),
      |buffer| write_owned_to(&mut formatted, buffer),
    );
    assert_eq!(
      formatted.writes,
      vec![b"slates anchor: issuer surface: export SLATES_ISSUER=token inode 737198504\n".to_vec()],
      "formatted multi-arg line must arrive in a single write call"
    );

    let mut owned = RecordingSink {
      writes: Vec::new(),
      fail: false,
    };
    write_owned_to(&mut owned, "slates-server: partition 0: ready".to_owned());
    assert_eq!(
      owned.writes,
      vec![b"slates-server: partition 0: ready\n".to_vec()],
      "owned line must arrive in a single write call with its trailing newline"
    );
    assert_eq!(log_lines_refused(), refused_before);

    let mut broken = RecordingSink {
      writes: Vec::new(),
      fail: true,
    };
    write_formatted(format_args!("line {}", 1), |buffer| {
      write_owned_to(&mut broken, buffer);
    });
    write_owned_to(&mut broken, "line 2".to_owned());
    assert_eq!(log_lines_refused(), refused_before + 2);
  }

  /// Checks whether one recorded 32 KiB+ line is intact and non-interleaved.
  fn is_intact_large_line(record: &[u8], expected_len: usize) -> bool {
    let Some(body) = record.strip_suffix(b"\n") else {
      return false;
    };
    if body.len() != expected_len {
      return false;
    }
    if let Some(fill) = body.strip_prefix(b"CHILD:") {
      fill.iter().all(|byte| *byte == b'C')
    } else if let Some(fill) = body.strip_prefix(b"ANCHOR:") {
      fill.iter().all(|byte| *byte == b'A')
    } else {
      false
    }
  }

  /// Shared test sink for concurrent relay and `log_line!` serialization checks.
  #[allow(clippy::disallowed_types)]
  fn shared_recording_sink() -> &'static std::sync::Mutex<RecordingSink> {
    // structural: allow — D-8 exception 3: test harness sink shared by the relay and anchor threads.
    Box::leak(Box::new(std::sync::Mutex::new(RecordingSink {
      writes: Vec::new(),
      fail: false,
    })))
  }

  /// Adapter implementing `Write` over a locked test sink so `relay_stderr_bounded` can hold the
  /// mutex guard across an entire logical line just as `std::io::stderr().lock()` does.
  #[allow(clippy::disallowed_types)]
  struct LockedSink<T: 'static>(std::sync::MutexGuard<'static, T>);

  impl<T: Write + 'static> Write for LockedSink<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
      self.0.flush()
    }
  }

  /// Single-thread `RefCell` guard implementing `Write` and counting lock acquisitions.
  struct BorrowedSink<'a, T> {
    guard: std::cell::RefMut<'a, T>,
  }

  impl<T: Write> Write for BorrowedSink<'_, T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      self.guard.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
      self.guard.flush()
    }
  }

  /// Blocker 4: Do: concurrently relay 16 child lines of 32 KiB+ (fed in 512-byte pipe writes
  /// matching macOS `PIPE_BUF`) through `relay_stderr_bounded` while an anchor thread writes 16
  /// 32 KiB+ lines through `write_formatted`/`write_owned_to`, both locking one shared sink.
  /// Expect: all 32 lines arrive intact with zero interleaving.
  #[test]
  fn relay_stderr_serializes_32kib_lines_with_log_line_without_interleaving() {
    let _gate = counter_gate();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let sink = shared_recording_sink();
    let payload_len = 32_768usize;
    let child_line = format!("CHILD:{}\n", "C".repeat(payload_len - 6));
    let anchor_line = format!("ANCHOR:{}", "A".repeat(payload_len - 7));
    let producer = std::thread::spawn(move || {
      for _ in 0..16 {
        for piece in child_line.as_bytes().chunks(512) {
          writer.write_all(piece).unwrap();
        }
      }
    });
    let relay = std::thread::spawn(move || {
      relay_stderr_bounded(reader, QUEUE_BYTES, || {
        LockedSink(
          sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
      });
    });
    for _ in 0..16 {
      write_formatted(format_args!("{anchor_line}"), |buf| {
        let mut guard = sink
          .lock()
          .unwrap_or_else(std::sync::PoisonError::into_inner);
        write_owned_to(&mut *guard, buf);
      });
    }
    producer.join().unwrap();
    relay.join().unwrap();
    let writes = sink
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .writes
      .clone();
    assert_eq!(writes.len(), 32);
    assert!(
      writes
        .iter()
        .all(|record| is_intact_large_line(record, payload_len)),
      "every relayed and local 32 KiB+ line must arrive intact with zero interleaving"
    );
  }

  /// Blocker 4: Do: feed an overlong newline-less stream (256 bytes under a 100-byte bound), a
  /// normal `\n`-terminated line, and an unterminated EOF tail into `relay_stderr_bounded`.
  /// Expect: every emitted chunk is bounded at <= 100 bytes, the sink lock is acquired only for
  /// each immediate write (never held across `Read::read`: 4 acquisitions for 4 emitted chunks),
  /// and the unterminated EOF remainder flushes with a trailing `\n`.
  #[test]
  fn relay_stderr_bounds_overlong_input_and_flushes_eof_remainder() {
    let mut input = vec![b'A'; 256];
    input.extend_from_slice(b"short\ntail");
    let sink = RefCell::new(RecordingSink {
      writes: Vec::new(),
      fail: false,
    });
    let mut acquisitions = 0usize;
    relay_stderr_bounded(std::io::Cursor::new(input), 100, || {
      acquisitions += 1;
      BorrowedSink {
        guard: sink.borrow_mut(),
      }
    });
    let emitted = sink.into_inner().writes;
    assert_eq!(
      acquisitions, 4,
      "one immediate lock acquisition per emitted chunk/line"
    );
    assert_eq!(
      emitted.iter().map(Vec::len).collect::<Vec<_>>(),
      vec![100, 100, 62, 5]
    );
    assert_eq!(emitted[2], [vec![b'A'; 56], b"short\n".to_vec()].concat());
    assert_eq!(emitted[3], b"tail\n");
  }

  /// Blocker 4: Do: relay lines over a real pipe while the destination sink refuses every write
  /// with `BrokenPipe`.
  /// Expect: the relay keeps draining the child pipe until EOF (so the producer never stalls) and
  /// counts every refused line in `log_lines_refused`.
  #[test]
  fn relay_stderr_keeps_draining_and_counts_refusals_when_sink_fails() {
    let _gate = counter_gate();
    let refused_before = log_lines_refused();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let producer = std::thread::spawn(move || {
      for idx in 0..4 {
        let line = format!("daemon line {idx}: {}\n", "X".repeat(1_024));
        writer.write_all(line.as_bytes()).unwrap();
      }
    });
    let broken = RefCell::new(RecordingSink {
      writes: Vec::new(),
      fail: true,
    });
    relay_stderr_bounded(reader, QUEUE_BYTES, || BorrowedSink {
      guard: broken.borrow_mut(),
    });
    producer.join().unwrap();
    assert_eq!(log_lines_refused().saturating_sub(refused_before), 4);
  }

  /// Sink that appends all written bytes into one byte stream to detect any mid-line interleaving
  /// or missing newline terminators across arbitrary line lengths.
  struct StreamSink {
    bytes: Vec<u8>,
    acquisitions: usize,
  }

  impl Write for StreamSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      self.bytes.extend_from_slice(buf);
      Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  #[allow(clippy::disallowed_types)]
  fn shared_stream_sink() -> &'static std::sync::Mutex<StreamSink> {
    // structural: allow — D-8 exception 3: test harness sink shared by the relay and anchor threads.
    Box::leak(Box::new(std::sync::Mutex::new(StreamSink {
      bytes: Vec::new(),
      acquisitions: 0,
    })))
  }

  /// Classifies one newline-delimited logical line from the concurrent overlong-line relay test.
  fn classify_overlong_line(line: &[u8]) -> Option<&'static str> {
    if let Some(fill) = line.strip_prefix(b"ANCHOR:")
      && fill.len() == 1_024
      && fill.iter().all(|byte| *byte == b'A')
    {
      return Some("anchor");
    }
    if let Some(fill) = line.strip_prefix(b"CHILD:")
      && fill.iter().all(|byte| *byte == b'C')
    {
      return match fill.len() {
        65_532 => Some("child_65539"),
        99_993 => Some("child_100000"),
        _ => None,
      };
    }
    if let Some(fill) = line.strip_prefix(b"EOF:")
      && fill.len() == QUEUE_BYTES - 4
      && fill.iter().all(|byte| *byte == b'E')
    {
      return Some("eof_exact");
    }
    None
  }

  /// Feeds 8×65,539-byte, 8×100,000-byte, and 1×exact-65,536-byte unterminated lines into `writer`
  /// in 512-byte pieces to exercise lock continuity across `QUEUE_BYTES` boundaries.
  fn write_overlong_child_lines(mut writer: std::io::PipeWriter) {
    let line_65539 = format!("CHILD:{}\n", "C".repeat(65_532));
    assert_eq!(line_65539.len(), 65_539);
    let line_100000 = format!("CHILD:{}\n", "C".repeat(99_993));
    assert_eq!(line_100000.len(), 100_000);
    let eof_exact = format!("EOF:{}", "E".repeat(QUEUE_BYTES - 4));
    assert_eq!(eof_exact.len(), QUEUE_BYTES);
    for _ in 0..8 {
      for piece in line_65539.as_bytes().chunks(512) {
        writer.write_all(piece).unwrap();
      }
      for piece in line_100000.as_bytes().chunks(512) {
        writer.write_all(piece).unwrap();
      }
    }
    for piece in eof_exact.as_bytes().chunks(512) {
      writer.write_all(piece).unwrap();
    }
  }

  /// Verifies every newline-delimited logical line in `stream` matches expected counts with zero
  /// interleaving and a trailing newline.
  fn assert_overlong_stream_intact(stream: &[u8], expected: (usize, usize, usize, usize)) {
    assert!(
      stream.ends_with(b"\n"),
      "exact-capacity unterminated EOF line must end with a newline"
    );
    let mut counts = (0usize, 0usize, 0usize, 0usize);
    for raw in stream[..stream.len() - 1].split(|byte| *byte == b'\n') {
      match classify_overlong_line(raw) {
        Some("anchor") => counts.0 += 1,
        Some("child_65539") => counts.1 += 1,
        Some("child_100000") => counts.2 += 1,
        Some("eof_exact") => counts.3 += 1,
        other => panic!(
          "interleaved or malformed line (len={}, kind={other:?}, prefix={:?})",
          raw.len(),
          &raw[..raw.len().min(32)]
        ),
      }
    }
    assert_eq!(counts, expected);
  }

  /// Round-2 Blocker 3 / Round-4 Deadlock Fix: Do: relay 65,539-byte and 100,000-byte lines (both
  /// past `QUEUE_BYTES`) plus an exact-65,536-byte unterminated EOF line over a real pipe in
  /// 512-byte writes.
  /// Expect: zero lost or merged bytes, every logical line arrives intact with an exact trailing
  /// `\n` on the exact-capacity EOF line, and streaming releases the sink lock between chunks.
  #[test]
  fn relay_stderr_streams_overlong_lines_losslessly_and_terminates_exact_cap_eof() {
    let _gate = counter_gate();
    let (reader, writer) = std::io::pipe().unwrap();
    let sink = shared_stream_sink();
    let producer = std::thread::spawn(move || write_overlong_child_lines(writer));
    let relay = std::thread::spawn(move || {
      relay_stderr_bounded(reader, QUEUE_BYTES, || {
        let mut guard = sink
          .lock()
          .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.acquisitions += 1;
        LockedSink(guard)
      });
    });
    producer.join().unwrap();
    relay.join().unwrap();

    let (stream, acquisitions) = {
      let guard = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
      (guard.bytes.clone(), guard.acquisitions)
    };
    assert!(
      acquisitions > 17,
      "overlong lines must release and reacquire the sink lock per chunk rather than holding across reads (got {acquisitions})"
    );
    assert_overlong_stream_intact(&stream, (0, 8, 8, 1));
  }

  /// Round-2 Blocker 3 / Nit 1: Do: relay two 150,000-byte lines (3 `QUEUE_BYTES` chunks each)
  /// into a failing `BrokenPipe` sink.
  /// Expect: `log_lines_refused` increases by 2 (one per logical line), not 6 (per chunk).
  #[test]
  fn relay_stderr_counts_one_refusal_per_overlong_logical_line() {
    let _gate = counter_gate();
    let refused_before = log_lines_refused();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let producer = std::thread::spawn(move || {
      let line = format!("{}\n", "X".repeat(149_999));
      assert_eq!(line.len(), 150_000);
      writer.write_all(line.as_bytes()).unwrap();
      writer.write_all(line.as_bytes()).unwrap();
    });
    let broken = RefCell::new(RecordingSink {
      writes: Vec::new(),
      fail: true,
    });
    relay_stderr_bounded(reader, QUEUE_BYTES, || BorrowedSink {
      guard: broken.borrow_mut(),
    });
    producer.join().unwrap();
    assert_eq!(log_lines_refused().saturating_sub(refused_before), 2);
  }

  /// Sink wrapper that signals `on_first_write` immediately after the first write completes while
  /// keeping the underlying guard alive until the wrapper is dropped.
  struct NotifyingSink<W> {
    inner: W,
    on_first_write: Option<std::sync::mpsc::Sender<()>>,
  }

  impl<W: Write> Write for NotifyingSink<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      let written = self.inner.write(buf)?;
      if let Some(tx) = self.on_first_write.take() {
        let _ = tx.send(());
      }
      Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
      self.inner.flush()
    }
  }

  /// Round-4 Deadlock Regression: Do: have a child pipe writer emit `QUEUE_BYTES` without `\n` and
  /// pause with the pipe open; once the relay writes that first chunk, have another thread acquire
  /// the same sink lock and write a line before unblocking the writer to finish with `TAIL\n`.
  /// Expect: the second thread acquires the sink lock immediately without deadlocking, and every
  /// byte of both the anchor line and the overlong child line arrives intact.
  #[test]
  fn relay_stderr_releases_sink_lock_while_paused_mid_overlong_line() {
    let _gate = counter_gate();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let sink = shared_stream_sink();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let (anchor_tx, anchor_rx) = std::sync::mpsc::channel();

    let producer = std::thread::spawn(move || {
      writer.write_all(&vec![b'X'; QUEUE_BYTES]).unwrap();
      let _ = resume_rx.recv();
      writer.write_all(b"TAIL\n").unwrap();
    });
    let mut notify = Some(first_tx);
    let relay = std::thread::spawn(move || {
      relay_stderr_bounded(reader, QUEUE_BYTES, move || NotifyingSink {
        inner: LockedSink(
          sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        ),
        on_first_write: notify.take(),
      });
    });

    first_rx
      .recv_timeout(ROUTED_WAIT)
      .expect("relay must emit the first QUEUE_BYTES chunk");
    let anchor = std::thread::spawn(move || {
      let mut guard = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
      write_owned_to(&mut *guard, "ANCHOR:unblocked".to_owned());
      let _ = anchor_tx.send(());
    });
    let acquired = anchor_rx.recv_timeout(ROUTED_WAIT);
    let _ = resume_tx.send(());
    producer.join().unwrap();
    relay.join().unwrap();
    anchor.join().unwrap();

    assert!(
      acquired.is_ok(),
      "anchor thread deadlocked on sink lock while child paused mid-overlong line"
    );
    let stream = sink
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .bytes
      .clone();
    let expected = [
      vec![b'X'; QUEUE_BYTES],
      b"ANCHOR:unblocked\n".to_vec(),
      b"TAIL\n".to_vec(),
    ]
    .concat();
    assert_eq!(stream, expected);
  }
}
