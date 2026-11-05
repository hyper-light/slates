# A closed log pipe ended the anchor

Found 2026-11-05 by the release agent: `slates anchor 2>&1 >/dev/null | head -n 1` exits 134 once `head` has its line.
Design: A-213 (the process's own lines), CLAUDE.md §2 item 6 (no panics in non-test code).

## Description

Every log line in slates was an `eprintln!` and every command's output a `println!` or `print!`. Those macros panic when
their write is refused. Rust ignores `SIGPIPE`, so a write into a pipe whose reader has gone is refused `EPIPE`, and the
next line panics; a release build aborts (`panic = "abort"`, exit 134).

## Root cause

The standard print macros were used for lines whose delivery the process cannot promise. The lint wall had no rule
against them, so 266 such sites sat in shipped code.

## Impact

- **The anchor:** its log is its standard error. Once the reader of that pipe went away, its next line (a daemon's
  restart, a refused hold message, a lapse) ended it, and its kill-on-exit tie ended the daemon and every held mount.
- **The daemon:** it inherits the anchor's standard error. Its startup lines panic into the same pipe, so a new daemon
  died at once, in a loop, until the anchor itself went.
- **Every command:** `slates status | head -n 1` panicked after its first line instead of finishing or failing.

## Fix

- `slates_machine::log` (`crates/machine/src/log.rs`): `log_line!` writes one line to standard error and, if that write
  is refused, counts it and drops it; `out_line!` and `out_text!` do the same on standard output. Neither can panic.
- **The counts are shown:**
  - The daemon reports its own as `log_lines_refused` in `slates status`.
  - The anchor writes its own to the supervision block (`SUP_LOG_REFUSALS`, offset 200), shown as
    `anchor_log_lines_refused`.
  - A command whose output was refused exits 4 (failed) with a line saying so (`output_delivered` in
    `crates/cli/src/main.rs`).
- **Every shipped site replaced:**
  - 226 found by clippy on Linux: `cli` 160, `server` 61, `rt` 3, `anchor`, `bridge-fuse` and `cluster` 1 each.
  - 40 compiled only on Windows, macOS or under loom: `bridge-winfsp` 17, `server`'s WinFsp and Windows hold files 17,
    `cli` 3, `mem`'s loom bounds 2, `anchor` and `ipc` 1 each.
  - `slates-cluster` gains a direct `slates-machine` dependency for its one.
- **Every shipped crate's root denies `clippy::print_stdout` and `clippy::print_stderr`**, with `allow-print-in-tests`
  in `clippy.toml`, so a new print macro outside test code fails the lint.

## Failing tests first

- `an_anchor_whose_log_reader_went_away_keeps_supervising` (`crates/cli/tests/cli.rs`): the real `slates anchor` with
  its log piped; the reader is dropped once the daemon serves; the daemon is killed. The anchor must still run and a new
  daemon must answer.
- `a_command_whose_output_reader_went_away_fails_without_a_panic` (same file): `slates --help` into a pipe with no
  reader must exit 4 with a line saying why.

## Evidence

The red and green runs, with the command and its date, are recorded in the GAPS entry and the commit.

## Not changed here (not shipped)

These crates do not ship, so their print sites stay; each is listed with file:line in the commit message.

- `xtask` (148 sites): the developer and CI tool.
- `slates-guest-probe` (11 sites): a VM guest's `/init` for the tests. It says "never runs on a host and never ships".
- `slates-test-seeds` (1 site): linked only into tests.

A broken pipe there still panics that tool.
