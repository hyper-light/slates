//! Instruction-count benches for the runtime under callgrind (iai-callgrind): one loop step
//! with nothing to do, a task's whole life, and a local wake, all on the simulation driver so no
//! OS call is counted (D-20; BENCHMARKS.md "Ratchets").

// The benchmark macros generate undocumented modules and constants; the doc rule is for our
// own items, which are documented above each function.
#![allow(missing_docs)]

use std::hint::black_box;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use slates_rt::futures::yield_now;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;

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
  SimRuntime::new(&config(), 1).unwrap_or_else(|_| never())
}

fn never() -> SimRuntime {
  // A simulation with one shard cannot fail to build; this arm is never taken.
  loop {
    std::hint::spin_loop();
  }
}

#[library_benchmark]
#[bench::idle(sim())]
fn step_idle(mut sim: SimRuntime) -> u64 {
  black_box(sim.run_until_idle())
}

#[library_benchmark]
#[bench::trivial(sim())]
fn spawn_and_run(mut sim: SimRuntime) -> u64 {
  let shard = sim.shard_ids()[0];
  let _ = sim.spawn_on(shard, async {});
  black_box(sim.run_until_idle())
}

#[library_benchmark]
#[bench::one_yield(sim())]
fn local_wake(mut sim: SimRuntime) -> u64 {
  let shard = sim.shard_ids()[0];
  let _ = sim.spawn_on(shard, async {
    yield_now().await;
  });
  black_box(sim.run_until_idle())
}

library_benchmark_group!(
  name = runtime;
  benchmarks = step_idle, spawn_and_run, local_wake
);

main!(library_benchmark_groups = runtime);
