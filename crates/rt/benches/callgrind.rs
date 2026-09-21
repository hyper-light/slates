//! Instruction-count benches for the runtime under callgrind (iai-callgrind): one loop step
//! with nothing to do, a task's whole life, and a local wake, all on the simulation driver so no
//! OS call is counted (D-20; BENCHMARKS.md "Ratchets").
//! Teardown requires admitted tasks to complete, so refused spawns cannot produce
//! a faster successful benchmark. Setup reports its refusal instead of spinning forever.

// The benchmark macros generate undocumented modules and constants; the doc rule is for our
// own items, which are documented above each function.
#![allow(missing_docs)]

use std::hint::black_box;

use iai_callgrind::{
  Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
use slates_rt::error::RtError;
use slates_rt::futures::yield_now;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::shard::Counters;
use slates_rt::sim::SimRuntime;

// The default function-return boundary included teardown instructions on Linux ARM64
// (2026-09-20: the CRC oracle added 119,143 instructions). Explicit requests bracket
// the advertised operation, including its owned input drops, before verification begins.
// Keep a real call boundary: Valgrind 3.24 on ARM64 lost the entire header
// encode from totals when both collection requests were inlined (20 summary, 0 total).
#[cfg(target_os = "linux")]
#[inline(never)]
fn toggle_collection() {
  iai_callgrind::client_requests::callgrind::toggle_collect();
}

// Valgrind is only run in the Linux lane; other hosts still run the same result checks.
#[cfg(not(target_os = "linux"))]
fn toggle_collection() {}

// The closure owns each input, so its destruction finishes before collection stops.
// Both boundaries stay inside this call; teardown size cannot alter the measured return path.
fn count_instructions<Value>(operation: impl FnOnce() -> Value) -> Value {
  toggle_collection();
  let result = operation();
  toggle_collection();
  result
}

// The runner owns main and requires infallible setup/teardown functions. Report the typed
// refusal and fail this benchmark process at that boundary; never substitute a fixture.
fn require_success<Value>(result: Result<Value, impl std::fmt::Debug>, operation: &str) -> Value {
  match result {
    Ok(value) => value,
    Err(error) => {
      eprintln!("{operation}: {error:?}");
      std::process::exit(1);
    }
  }
}

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 256,
    timers_per_shard: 256,
    ring_entries: 256,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 1_000,
    batch: 256,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
  }
}

fn sim() -> SimRuntime {
  require_success(
    SimRuntime::new(&config(), 1),
    "construct the benchmark runtime",
  )
}

fn check_idle(steps: u64) {
  assert!(steps > 0, "the idle benchmark must step the runtime");
}

fn check_task(result: Result<Counters, RtError>) {
  verify_task(require_success(
    result,
    "admit and observe the benchmark task",
  ));
}

fn verify_task(counters: Counters) {
  assert_eq!(counters.completed, 1, "the admitted task must return");
  assert_eq!(counters.cancelled, 0);
  assert_eq!(counters.admission_refused, 0);
}

fn check_wake(result: Result<Counters, RtError>) {
  let counters = require_success(result, "admit and observe the yielding task");
  verify_task(counters);
  assert!(
    counters.polls >= 2,
    "the yielding task must be polled again to complete"
  );
}

#[library_benchmark(teardown = check_idle)]
#[bench::idle(sim())]
fn step_idle(mut sim: SimRuntime) -> u64 {
  count_instructions(move || black_box(sim.run_until_idle()))
}

#[library_benchmark(teardown = check_task)]
#[bench::trivial(sim())]
fn spawn_and_run(mut sim: SimRuntime) -> Result<Counters, RtError> {
  count_instructions(move || {
    let shard = sim.shard_ids()[0];
    sim.spawn_on(shard, async {})?;
    black_box(sim.run_until_idle());
    Ok(black_box(sim.context(shard)?.counters()))
  })
}

#[library_benchmark(teardown = check_wake)]
#[bench::one_yield(sim())]
fn local_wake(mut sim: SimRuntime) -> Result<Counters, RtError> {
  count_instructions(move || {
    let shard = sim.shard_ids()[0];
    sim.spawn_on(shard, async {
      yield_now().await;
    })?;
    black_box(sim.run_until_idle());
    Ok(black_box(sim.context(shard)?.counters()))
  })
}

library_benchmark_group!(
  name = runtime;
  benchmarks = step_idle, spawn_and_run, local_wake
);

main!(
  config = LibraryBenchmarkConfig::default().tool(
    Callgrind::with_args(["--collect-atstart=no"]).entry_point(EntryPoint::None)
  );
  library_benchmark_groups = runtime
);
