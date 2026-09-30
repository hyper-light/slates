# Runtime start and drop orphaned workers (2026-09-30, AUD-29-12)

Contracts: §4.3 (the runtime owns its shards), banned item 9 ("a spawned thread without an owner that
joins or cancels it"; "a lost or swallowed error"). Reported by the 2026-09-29 audit.

## Description

`slates_rt::runtime::Runtime` owned its workers' `JoinHandle`s but did not own their termination:

- **Dropping it detached every worker.** There was no `Drop`, so without `shutdown` the shard threads ran
  on and their registry slots stayed claimed.
- **A failure part-way through `start` leaked.** A driver or registration refused at shard k, or a
  refused thread spawn, returned through `?`. That detached the threads already spawned and kept every
  slot already registered.
- **A failed context was advertised.** A worker whose `ShardContext::build` failed returned default
  counters, and `start` returned success with a dead shard in `shard_ids()`.
- **A panic was hidden.** `shutdown` turned a worker's panic into default counters.

## Root cause

Startup was not transactional: nothing waited for the workers to be ready, and nothing rolled back. The
value's destruction had no owner of the workers' termination.

## Impact

- A dropped runtime (every error path in a caller that did not reach `shutdown`) left live threads and
  claimed slots for the process lifetime.
- A daemon could report started with a shard that serves nothing.
- A worker's failure was indistinguishable from an idle shard's counters.

## Exact edits (`crates/rt/src/runtime.rs`, `error.rs`)

- **Transactional start.** `Runtime::start` = `start_with(config, prepare)` over the OS driver.
  - `prepare` is a per-shard driver seam, the same form `LocalRuntime::with_driver` takes.
  - A refused preparation, registration or pairing gives every slot claimed so far back.
  - Each worker acknowledges its context's build, or its refusal, on a bounded channel, then drops its
    sender. A worker that ends unacknowledged is seen, not awaited.
  - Any refusal, including a refused thread spawn, stops and joins the workers already started, gives
    every slot back, and returns that refusal. A second failure found while rolling back is reported on
    the error stream.
- **Owned termination.** `shutdown(self) -> Result<Vec<Counters>, RtError>`: every worker is stopped
  and joined and every slot given back, then the first failure is returned typed. A panicked worker is
  the new `RtError::WorkerFailed { shard }`. `Drop` runs the same stop when `shutdown` was not called
  and reports a failure on the error stream.
- **Callers.** The daemon's `stop` and `Drop` share one `stop_parts` that reports a shard failure on the
  daemon's error stream, which its anchor keeps. Every test and example takes the typed result. The
  `rt_bench` example had been hiding a refused start as an empty sample; it now propagates it.

## Evidence

- **Red.** `crates/rt/tests/lifecycle.rs::a_runtime_dropped_without_shutdown_stops_and_joins_its_workers`
  on `4c6febb`: the parked tasks were never dropped ("Timeout" after 10 s). The workers were detached.
- **Green** (`crates/rt/tests/lifecycle.rs`):
  - that test;
  - a driver refused at the third shard: the start is refused with it, and a new runtime gets the
    baseline slots;
  - the last shard's context refused on its own thread while its siblings run: the start is refused,
    the siblings are stopped and joined, and the baseline slots are intact;
  - a worker that panics: `shutdown` returns `WorkerFailed { shard }`, and every slot is given back.
- **Suites.**
  - rt passes on macOS and Linux (io_uring).
  - server daemon: 17 on macOS, 18 on Linux.
  - server fleet: 59/59.
  - bridge-nfs async loopback and bridge-virtiofs serve pass.
  - Clippy is clean on macOS, Linux and the Windows cross-lint.

## Open

- A panicked worker's context is not freed: it panicked inside its loop, before `reclaim_context`.
  Release builds abort on panic (`panic = "abort"`), so this is reachable only in test builds.
- A refused thread spawn is covered by the shared rollback path, not by its own injected test. No safe
  seam makes `std::thread::Builder::spawn` fail on demand.
