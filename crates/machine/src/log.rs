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

use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// Log lines this process could not write, over its life.
static LOG_LINES_REFUSED: AtomicU64 = AtomicU64::new(0);
/// Output lines this process could not write, over its life.
static OUT_LINES_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Writes `line` and a newline to standard error, one locked write; a refused write is counted and dropped.
pub fn log(line: fmt::Arguments<'_>) {
  let mut stderr = std::io::stderr().lock();
  if writeln!(stderr, "{line}").is_err() {
    LOG_LINES_REFUSED.fetch_add(1, Ordering::Relaxed);
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

  /// The panic rule. Do: write a log line and an output line in this test process, whose streams libtest captures.
  /// Expect: both return, and neither counts a refusal.
  #[test]
  fn a_written_line_counts_no_refusal() {
    let (log_before, out_before) = (log_lines_refused(), out_lines_refused());
    crate::log_line!("a log line {}", 1);
    crate::out_line!("an output line {}", 2);
    assert_eq!(log_lines_refused(), log_before);
    assert_eq!(out_lines_refused(), out_before);
  }
}
