# w-k90-vendor -> main: closes (2026-11-20)

## C1 goal 90a: vendoring. The export_engine re-vendor is DONE; the other platforms' builds are queued
- Item: Slates re-vendored into kiln with `container_engine::export_engine`, which w-k90-containers waits on.
- Commit: kiln main `877ab71ea`, Slates at `8ce3d61523e1`, contract 57.
- Tests on macOS arm64:
  - `cargo test -p kiln --lib -- workspace::slates workspace::container`: 68 passed, including
    `w1_kiln_speaks_the_contract_it_was_written_against`;
  - `--test slates_roles`: 1 passed;
  - `--test vendored`: 11 passed;
  - `python3.12 scripts/vendor.py check slates`: 4,050 files, 1 recorded change, 0 differences.
- Kiln changes: 0002-0004 were taken upstream and dropped. kiln's `CONTRACT` is now 57. `EngineFault::Untested`'s profile
  is boxed.
- w-k90-containers told (inbox/w-k90-containers-from-w-k90-vendor-revendor.md).
- Every platform's build: Linux (r26) and Windows (r27) runs of `cargo check -p kiln --all-targets` plus the Slates tests
  are queued for room on the Linux runner and jupiter. I'll close that part with their results.
- Upstream finding: Slates main carries `crates/cli/bench/__pycache__/push_load.cpython-314.pyc`, a build artefact
  someone committed. It's vendored as is, so the copy matches upstream.

## R2 goal 93 git: the encode spin at git_encode.rs:345. CLOSED
- Item: the encode task's poll spin, at git_encode.rs:345.
- Commit: slates main `67dd4ef40` (landed via `982ea33d5`). Every unit is at most one window's work: one row, one window
  read, one search step, one level sample, one window deflated, one window put.
- Test: `vcs_store::git_encode::tests::a718_no_unit_of_a_56_mb_packs_encoding_outgrows_the_step_quantum`, on git/git
  v2.0.0's 55.9 MB pack.
  - Red, twice: the watchdog killed the process after 3,508 ms and 3,541 ms in one unit.
  - Green: the daemon lives through all 164,487 objects; no unit exceeds 262,144 bytes or one row.
- r110 (base 944588f6a) predates the fix.

## Also closed today (A-745)
- Item 2, begin's walks in turns: `96cce3522`.
- Item 3, v0 in turns: `1625570cf`.
- Item 4, scattered plans: `7ffddbdf0`.
- The deterministic level: `7c37b6fee`. Its release A/B goes into BENCHMARKS next: a dead heat on CPU, and B writes the
  same bytes every run.
