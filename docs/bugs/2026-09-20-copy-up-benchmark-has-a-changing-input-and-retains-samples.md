# Copy-up benchmark input changes with the build and retains old samples

Date: 2026-09-20 (local). Design: D-25, §4.5, R5 and Part 5 measurement discipline.

## Failure

The macOS ratchet at `46ca485`'s source reports small-file copy-up medians of
96,625 / 97,042 / 104,958 ns against the unchanged 30,667 ns ceiling. Command:
`cargo xtask ratchet`; log: `/private/tmp/slates-macos-current.log`.

The September 5 baseline describes an overlay over `target/debug`, with about twelve
directory entries. The same directory now has 69. Each sample creates a fresh overlay,
resolves a name through its uncached parent listing, and writes one byte. The selected
file is whichever nonempty build artifact is smallest, up to 4 KiB. Neither the directory
width nor the selected file is fixed by the benchmark.

## Controlled comparison

On the same Apple M5 Max, macOS 26.4.1, Rust 1.98.0, extract `9344472` with `git archive`
into `/private/tmp/slates-performance-baseline-9344472`. That commit recorded zero ratchet
regressions. Change only its benchmark source paths to read the current tree's real
`target/debug` and `target/debug/deps`; build into a separate target directory. Run the old
and current binaries serially, alternating them three times. No operation or timing rule
is changed. Command: `python3.14 /private/tmp/slates-compare-base-mem.py`; log:
`/private/tmp/slates-base-mem-controlled-comparison.log`. Wall time: 23.12 s, including
7.75 s compilation; load averages at the trials: 3.07–3.27 on 18 logical CPUs.

| Copy-up median, ns | Trial 1 | Trial 2 | Trial 3 |
| --- | ---: | ---: | ---: |
| September 5 source, current input | 94,667 | 98,584 | 96,583 |
| Current source, current input | 99,709 | 100,500 | 98,000 |

All six intervals overlap. The older source also fails the original ceiling on the new
input. This rules out attributing the whole increase to the current production changes;
it does not quantify the directory width's contribution separately from file selection
and machine state.

The same comparison measured the one-page buddy operation at 72 / 32 / 34 ns on old code
and 31 / 33 / 31 ns on current code (ceiling 56 ns). The allocator, its benchmark and its
timing harness have no source diff between these commits. This does not establish the
cause of the first slow sample, or close the other performance failures.

## Retained sample resources

A separate diagnostic counts successful writes, refusals and store occupancy outside the
timed sample. Sixteen small-file iterations all succeeded, with zero refusals, while live
inodes grew **1 → 33** and directories **1 → 17**. The large-class row also ran sixteen
successful writes and added 32 inodes and 16 directories. Command:
`python3.14 /private/tmp/slates-diagnose-base-samples.py`; log:
`/private/tmp/slates-base-sample-diagnostic.log`.

`vol = fresh(...)` drops the Rust volume object without destroying its objects in the
shared store. In addition, failed resolve, write and replacement-create operations are
ignored. No refusal occurred in this short run; it would be wrong to claim refusal was
the cause of the measured slowdown. The resource growth itself is confirmed.

The diagnostic restored the original source in a `finally` block. Its built executable
is diagnostic; rebuild the ordinary benchmark before using that executable for a gate.

## Required correction

Define a checked-in, read-only input with a fixed directory width and file contents,
preserving the baseline's stated small-directory workload. Keep the real `OsHost` path.
Make every attempted operation report failure, and reclaim each sample's volume and any
owned host descriptors. Prove repeated successful copy-ups in a store that cannot retain
the whole history. Measure the complete stated operation under the existing ceiling;
do not reset the ceiling or substitute a simulated host to obtain a pass.

The fixture correction and resource reclamation are not yet implemented. The separate
database, landing, VFS and intermittent destroy findings remain in `TBD_FIXES.md`.
