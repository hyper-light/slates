//! The two exercisers' verdicts (Part 6 "Conformance": fsx and fsstress). fsx verifies every
//! operation against its own in-memory copy of the file and prints one line when all of them
//! held (`All operations completed A-OK!`, `fsx.c` at the pinned FreeBSD commit); anything else
//! — a mismatch dump, a signal, a non-zero exit — is a failure of the transport. fsstress checks
//! nothing itself: it is a randomized namespace stress whose pass is "every process finished
//! without error and the daemon still answers", so its verdict is the exit status plus the count
//! of operations its `-v` log shows completed, and the harness adds the daemon's liveness.

use crate::record::detail;

/// Format: fsx's success line (`fsx.c`, printed at the end of `main` when every op verified).
pub const FSX_SUCCESS: &str = "All operations completed A-OK!";

/// Shape: the trailing lines of an exerciser's output kept as the detail of a failure.
const TAIL_LINES: usize = 12;

/// A verdict with the output's tail as its detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExerciserVerdict {
  /// Whether the exerciser passed.
  pub ok: bool,
  /// The output's tail, bounded.
  pub detail: String,
}

/// The last lines of an output, joined and bounded.
pub fn tail(output: &str) -> String {
  let lines: Vec<&str> = output.lines().collect();
  let start = lines.len().saturating_sub(TAIL_LINES);
  detail(&lines[start..].join("\n"))
}

/// fsx passed when it exited 0 and printed its success line.
pub fn judge_fsx(exit_success: bool, output: &str) -> ExerciserVerdict {
  ExerciserVerdict {
    ok: exit_success && output.contains(FSX_SUCCESS),
    detail: tail(output),
  }
}

/// fsstress's verdict: the exit status, and the operations its log shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsstressVerdict {
  /// Every process exited 0.
  pub ok: bool,
  /// `proc/op:` lines in the `-v` log.
  pub logged_operations: u64,
  /// The output's tail, bounded.
  pub detail: String,
}

/// Whether a line is an fsstress `-v` operation line: `<proc>/<op>: <name> ...`.
fn is_operation_line(line: &str) -> bool {
  let Some((prefix, _)) = line.split_once(": ") else {
    return false;
  };
  match prefix.split_once('/') {
    Some((process, op)) => {
      !process.is_empty()
        && !op.is_empty()
        && process.chars().all(|c| c.is_ascii_digit())
        && op.chars().all(|c| c.is_ascii_digit())
    }
    None => false,
  }
}

/// fsstress passed when it exited 0; the logged operations are counted from its `-v` lines.
pub fn judge_fsstress(exit_success: bool, output: &str) -> FsstressVerdict {
  let logged = output.lines().filter(|l| is_operation_line(l)).count();
  FsstressVerdict {
    ok: exit_success,
    logged_operations: u64::try_from(logged).unwrap_or(u64::MAX),
    detail: tail(output),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// fsx's real output (a 300-operation run on this host, 2026-09-14) passes; a mismatch dump or a
  /// signal exit fails even if the line appears.
  #[test]
  fn fsx_needs_both_the_exit_and_the_line() {
    let good =
      "Seed set to 1\ntruncating to largest ever: 0x3c2c2\nAll operations completed A-OK!\n";
    assert!(judge_fsx(true, good).ok);
    assert!(!judge_fsx(false, good).ok, "a failed exit is a failure");
    let bad = "Seed set to 1\nREAD BAD DATA: offset = 0x1000, size = 0x2000\nLOG DUMP (10 total operations):\n";
    let verdict = judge_fsx(true, bad);
    assert!(!verdict.ok);
    assert!(verdict.detail.contains("READ BAD DATA"));
  }

  /// fsstress's real `-v` lines (a 40-operation, two-process run on this host, 2026-09-14) are
  /// counted; other lines are not; the verdict is the exit.
  #[test]
  fn fsstress_counts_its_operation_lines() {
    let log = "1/0: write - no filename\n1/1: truncate - no filename\n1/2: mkdir d0 0\n\
      1/3: creat d0/f1 x:0 0 0\nseed = 7\n2/0: symlink d0/l2 0\nnot an op\n";
    let verdict = judge_fsstress(true, log);
    assert!(verdict.ok);
    assert_eq!(verdict.logged_operations, 5);
    assert!(!judge_fsstress(false, log).ok);
  }

  /// The tail is bounded whatever the exerciser prints.
  #[test]
  fn the_tail_is_bounded() {
    let noisy: String = (0..1000).map(|i| format!("line {i}\n")).collect();
    let kept = tail(&noisy);
    assert!(kept.starts_with("line 988"), "{kept}");
    assert!(kept.ends_with("line 999"));
    let wide = "x".repeat(100_000);
    assert!(tail(&wide).chars().count() <= crate::record::DETAIL_CHARS + 1);
  }
}
