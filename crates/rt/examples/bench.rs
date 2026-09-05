//! The Phase 0 runtime baseline: task spawn-to-completion, a local wake (yield), a timer's
//! lateness, and a cross-shard wake round trip, each with the machine harness's interval.
//!
//! `cargo run --release -p slates-rt --example bench`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use slates_machine::bench::{Measurement, measure};
use slates_machine::stats::{Percentile, Sample};
use slates_rt::futures::{sleep, yield_now};
use slates_rt::runtime::{LocalRuntime, Runtime, RuntimeConfig};

fn report(name: &str, m: Measurement) {
  println!(
    "{name}: median {} ns [{}, {}] p99 {} ns, {} samples × batch {}{}",
    m.median_ns(),
    m.interval.lower,
    m.interval.upper,
    m.p99_ns,
    m.samples,
    m.batch,
    if m.quick { " (quick)" } else { "" }
  );
}

fn config(shards: u16) -> RuntimeConfig {
  RuntimeConfig {
    shards,
    tasks_per_shard: 1 << 16,
    timers_per_shard: 1 << 16,
    ring_entries: 1024,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 1024,
    pin: false,
    page_bytes: 16384,
  }
}

static PINGS: AtomicU64 = AtomicU64::new(0);

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let budget = Duration::from_millis(500);
  let rt = LocalRuntime::new(&config(1))?;
  println!("driver notes: {:?}", rt.notes());

  report(
    "run_until_idle with nothing to do",
    measure(|| rt.run_until_idle(), budget),
  );
  report(
    "one step with nothing to do",
    measure(
      || {
        std::hint::black_box(rt.context().step());
      },
      budget,
    ),
  );
  report(
    "one zero-timeout park in the OS driver",
    measure(|| rt.context().park(Some(rt.context().now_ns())), budget),
  );
  report(
    "spawn a trivial task and detach it without running (admission only)",
    measure(
      || {
        if let Ok(id) = rt.spawn(async {}) {
          let _ = rt.context().cancel(id);
          let _ = rt.context().detach(id);
        }
      },
      budget,
    ),
  );
  rt.run_until_idle();
  report(
    "spawn one trivial task and run it to completion",
    measure(
      || {
        if let Ok(id) = rt.spawn(async {}) {
          rt.run_until_idle();
          let _ = rt.context().detach(id);
        }
      },
      budget,
    ),
  );

  report(
    "one local wake (a task yields once and resumes)",
    measure(
      || {
        if let Ok(id) = rt.spawn(async {
          yield_now().await;
        }) {
          rt.run_until_idle();
          let _ = rt.context().detach(id);
        }
      },
      budget,
    ),
  );

  // Timer lateness: arm a sleep of one tick and measure how long after the deadline the task ran.
  let mut lateness = Sample::new(Vec::new());
  for _ in 0..200 {
    let tick = config(1).timer_tick_ns;
    let started = std::time::Instant::now();
    if let Ok(id) = rt.spawn(async move {
      sleep(tick).await;
    }) {
      rt.run_until_idle();
      let _ = rt.context().detach(id);
    }
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    lateness.push(elapsed.saturating_sub(tick));
  }
  println!(
    "timer lateness after a one-tick sleep ({} ns tick): p50 {} ns, p99 {} ns, max {} ns",
    config(1).timer_tick_ns,
    lateness.median().unwrap_or(0),
    lateness.percentile(Percentile::P99).unwrap_or(0),
    lateness.values().last().copied().unwrap_or(0)
  );

  // Cross-shard round trip: a foreign spawn onto shard 1 whose task reports back over a channel.
  let two = Runtime::start(&config(2))?;
  let target = two.shard_ids()[1];
  let (tx, rx) = channel::<u64>();
  report(
    "foreign spawn onto a shard thread and a reply over a channel (ring, kick, poll, send)",
    measure(
      || {
        let tx = tx.clone();
        two.spawn_on(target, async move {
          let _ = tx.send(PINGS.fetch_add(1, Ordering::Relaxed));
        });
        let _ = rx.recv();
      },
      budget,
    ),
  );
  let counters = two.shutdown();
  println!("shard counters after the round trips: {counters:?}");
  Ok(())
}
