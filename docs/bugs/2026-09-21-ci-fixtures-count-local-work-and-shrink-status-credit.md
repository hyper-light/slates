# CI fixtures count their own allocation and shrink status credit

Date: 2026-09-21. Design: §4.2, §4.7, §4.14; AC-0.4, AC-2.6.

## Failures and evidence

Run `35604717581` at `2179cc2` fails two independent workspace tests:
[Linux allocation isolation](https://github.com/hyper-light/slates/actions/runs/35604717581/job/106348895387)
counts one local allocation, and [macOS status paging](https://github.com/hyper-light/slates/actions/runs/35604717581/job/106348895490)
receives `BudgetExceeded { available: 1024 }`.

The unchanged allocation test passes 100 isolated native macOS trials but fails Linux
trial 31 in the existing four-CPU, 4 GiB ARM64 container, Rust 1.98.0. Diagnostic counters
around the second handshake reproduce the failure at trial 95: local send 0 allocations,
local receive 1, worker 1. The measured local receive itself allocates. One preliminary
handshake does not ensure every later channel wait has initialized its thread-local state.
The thread-local allocator counter correctly reports this local work.

Command: repeat `cargo test --offline -p slates-mem --test no_alloc
an_allocation_measurement_excludes_another_threads_work -- --exact --nocapture`, bounded
to 100 trials. Logs: `/private/tmp/slates-ci-35604717581-no-alloc-linux-red.log` and
`/private/tmp/slates-ci-35604717581-no-alloc-linux-probe.log`.

The paging fixture changes the whole bulk allocation to `slots × 2 × 128`, so its retained
report credit becomes `slots × 128`. Eight slots leave 1024 bytes. Pinning that geometry
locally reproduces the exact refusal in 0.77 s; this host normally derives 64 slots and
524288 bulk bytes. §4.14 explicitly requires refusing a report larger than its admitted
credit. A fixture intended to force small pages must preserve that credit.

Command: `cargo test --offline -p slates-server --test daemon
status_pages_preserve_a_capture_and_refuse_foreign_or_cancelled_cursors -- --exact
--nocapture`. Log: `/private/tmp/slates-ci-35604717581-status-red.log`.

## Edits

- Synchronize the allocation-isolation history with stack-owned atomic signals and a joined,
  scoped worker. Keep thread creation and join outside the measurement. Require a real worker
  allocation and exactly zero local allocations; bound either stalled handshake.
- Pin CI's original eight-slot byte allowance and split it into 128-byte slots instead of shrinking the credit.
  Keep every existing immutable-capture, cross-client, cancellation and typed-client assertion.
- Remove diagnostic logging. No production accounting, limit, or refusal changes.

## Validation and sibling review

The corrected memory allocation binary passes 100 Linux repetitions (200 test executions,
including the unchanged hot-path test). The sibling fixture in
`crates/rt/tests/timer_allocations.rs` independently reproduces at Linux trial 61 and receives
the same correction. Both allocation binaries and all 14 Linux daemon integration tests
pass on the isolated source snapshot under io_uring. The native status regression, with
CI's original eight-slot allowance pinned, passes in 0.75 s.

Full Linux workspace validation is running. The separate macOS conformance job also fails pjdfstest and starts
`fs_usage` with `ktrace_start: No such process`; those are separate investigations. The
remaining allocation-counting hot-path tests contain no channel wait in their measured interval.
No other fixture shrinks the bulk allocation to force status fragmentation.
