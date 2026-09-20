# Timer allocation measurements included libtest's other threads

Date: 2026-09-19. Area: `crates/rt/tests/timer_allocations.rs`, AC-0.4.

## Failure and impact

[macOS CI job 105974090609](https://github.com/hyper-light/slates/actions/runs/35471844854/job/105974090609)
failed on `bf0e690`: reserving 65 timers counted 9 allocations, while reserving
1,617,130 timers counted 4. The assertion compared the two counts. This was a
measurement failure; the log did not establish a capacity-dependent wheel cost.

## Root cause and reproduction

The allocator incremented one process-wide atomic. Keeping one test in the binary
did not exclude allocations on libtest's reporting thread. Such work could fall
inside either measured interval, including the allocation-free use interval.

The unchanged test passed 100 isolated invocations on the local macOS host.
The controlled regression then performed one real allocation on another thread
between the measuring thread's two counter reads. Before the fix:

```text
cargo test -p slates-rt --test timer_allocations \
  an_allocation_measurement_excludes_another_threads_work -- --nocapture
another thread cannot change this measurement: left 1, right 0
```

The worker is joined, both channel wait paths are warmed before measurement,
and the worker's own count must be positive. The regression exercises the
contaminating event directly instead of depending on libtest's scheduling.

## Fix

Replace the process-wide atomic with a constant-initialized, destructor-free
thread-local `Cell`. Every allocator call belongs to its allocating thread.
The counter itself allocates nothing. Keep the capacity comparison and the
zero-allocation insert/renew/expire assertion unchanged.

Sibling check: `mem/tests/no_alloc.rs` had the same process-wide counter. The same
controlled regression failed there with one foreign allocation counted locally;
its counter now uses the same thread-local discipline. Its existing zero-allocation
assertion is unchanged. `rt/tests/memory.rs` and `sim_memory.rs` measure process
resident memory intentionally and are not changed by this fix.

Validation on 2026-09-19: the two focused allocation suites each pass both tests after their
controlled foreign-allocation regression failed against the old counter. Timer construction
reports four allocations at both capacities. The subsequent macOS `cargo test --workspace`
passes with 1,490 passed, 14 ignored and zero failed. Linux validation is recorded with the
io_uring repair after its full workspace run.
