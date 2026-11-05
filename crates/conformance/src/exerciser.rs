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
  detail(&lines.get(start..).unwrap_or_default().join("\n"))
}

/// Shape: fsx's operation count — enough to exercise every operation class many times (fsx picks among
/// read/write/mapread/mapwrite/truncate uniformly) inside a minute over loopback. Every leg runs fsx under
/// these bounds: the harness's and the live guest's (`crates/server/tests/virtiofs.rs`).
pub const FSX_OPERATIONS: u64 = 10_000;
/// Shape: fsx's seed (its own default is 1; recorded so a run is reproducible).
pub const FSX_SEED: u64 = 1;
/// Shape: fsx's file-length bound, its own default (`-l`, 262144 bytes).
pub const FSX_FILE_LENGTH: u64 = 262_144;
/// Shape: fsstress's operations per process.
pub const FSSTRESS_OPERATIONS: u64 = 500;
/// Shape: fsstress's process count — several writers racing in one tree.
pub const FSSTRESS_PROCESSES: u32 = 4;
/// Shape: fsstress's seed.
pub const FSSTRESS_SEED: u64 = 1;

/// fsx passed when it exited 0 and printed its success line.
pub fn judge_fsx(exit_success: bool, output: &str) -> ExerciserVerdict {
  ExerciserVerdict {
    ok: exit_success && output.contains(FSX_SUCCESS),
    detail: tail(output),
  }
}

/// The WinFsp fsx port's success line for a run of `operations` (Apple's fsx as `secfs.test` carries it: `prt("All
/// operations - %lu - completed A-OK!\n", testcalls)` at the end of `main`, `fstools/src/fsx/fsx.c` at 6ac65cd).
pub fn winfsp_fsx_success(operations: u64) -> String {
  format!("All operations - {operations} - completed A-OK!")
}

/// The WinFsp fsx port passed when it exited 0 and printed its success line naming exactly the operations asked for
/// (`-N`), so a run that stopped short cannot pass.
pub fn judge_winfsp_fsx(exit_success: bool, output: &str, operations: u64) -> ExerciserVerdict {
  let line = winfsp_fsx_success(operations);
  ExerciserVerdict {
    ok: exit_success && output.lines().any(|candidate| candidate.trim() == line),
    detail: tail(output),
  }
}

/// The number of the last operation an fsx `-v` trace shows done: the largest `N` of its `N: OPERATION …` lines
/// (`fsx.c`'s `logdump` and `-v` lines both start so). `None` when it shows none.
pub fn fsx_last_operation(output: &str) -> Option<u64> {
  output
    .lines()
    .filter_map(|line| {
      let (number, rest) = line.trim_start().split_once(": ")?;
      let named = rest.split_whitespace().next()?;
      let is_operation = !named.is_empty() && named.chars().all(|c| c.is_ascii_uppercase());
      (is_operation && number.chars().all(|c| c.is_ascii_digit()))
        .then(|| number.parse::<u64>().ok())
        .flatten()
    })
    .max()
}

/// The reviewed-list id of an fsx run that stopped short: `fsx-seed-<seed>:<the operation it stopped at>`, the operation
/// after the last one its trace shows done. Ids have the `file:number` form the reviewed lists take.
pub fn fsx_stop_id(seed: u64, output: &str) -> String {
  let stopped_at = fsx_last_operation(output).unwrap_or(0).saturating_add(1);
  format!("fsx-seed-{seed}:{stopped_at}")
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

  /// The WinFsp port's line passes only with the operation count asked for and a clean exit; FreeBSD's line, a short
  /// count or a failed exit fail it.
  #[test]
  fn the_winfsp_port_needs_its_own_line_with_the_count_asked_for() {
    let good = "Seed set to 1\r\nAll operations - 10000 - completed A-OK!\r\n";
    assert!(judge_winfsp_fsx(true, good, 10_000).ok);
    assert!(
      !judge_winfsp_fsx(false, good, 10_000).ok,
      "a failed exit is a failure"
    );
    assert!(
      !judge_winfsp_fsx(true, good, 20_000).ok,
      "a run of other length is not this run"
    );
    assert!(!judge_winfsp_fsx(true, "All operations completed A-OK!\n", 10_000).ok);
    let bad =
      "READ BAD DATA: offset = 0x1000, size = 0x2000\nAll operations - 10000 - completed A-OK!x\n";
    assert!(!judge_winfsp_fsx(true, bad, 10_000).ok);
  }

  /// A-214: a `-v` trace's last operation is its largest numbered operation line; the port's other lines (its seed,
  /// its size changes, `testcalls = 163`) are not operations; a run with none stopped at operation 1.
  #[test]
  fn an_fsx_trace_names_the_operation_it_stopped_at() {
    let trace = "Seed set to 1\nextend file size from 0x0 (0) to 0x40000 (262144)\n\
      162: WRITE           0x32464 (205924) thru 0x39303 (234243)\t(0x6ea0 (28320) bytes) HOLE\n\
      163: TRUNCATE DOWN   from 0x39304 (234244) to 0x269b4 (158132)\nsignal 2\ntestcalls = 163\n";
    assert_eq!(fsx_last_operation(trace), Some(163));
    assert_eq!(fsx_stop_id(1, trace), "fsx-seed-1:164");
    assert_eq!(fsx_stop_id(7, "Seed set to 7\n"), "fsx-seed-7:1");
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
