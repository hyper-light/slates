//! The Phase 0 runtime baseline: task spawn-to-completion, a local wake (yield), a timer's
//! lateness, and a cross-shard wake round trip, each with the machine harness's interval.
//!
//! `cargo run --release -p slates-rt --example bench`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use slates_machine::bench::{Measurement, measure};
use slates_machine::stats::{Interval, Percentile, Sample, Xorshift, bootstrap_interval};
use slates_rt::futures::{sleep, yield_now};
use slates_rt::runtime::{LocalRuntime, Runtime, RuntimeConfig};

fn report(name: &str, m: Measurement) {
  println!(
    "ratchet\t{}\t{}\t{}\t{}",
    key(name),
    m.interval.lower,
    m.median_ns(),
    m.interval.upper
  );
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
    cores: Vec::new(),
    page_bytes: 16384,
    spin_ns: 0,
  }
}

static PINGS: AtomicU64 = AtomicU64::new(0);

/// The ratchet key of a row: `rt.` plus the row's name in snake case, whole, so two rows
/// that share their first words stay distinct.
fn key(name: &str) -> String {
  let words: Vec<&str> = name
    .split(|c: char| !c.is_ascii_alphanumeric())
    .filter(|w| !w.is_empty())
    .collect();
  format!("rt.{}", words.join("_").to_ascii_lowercase())
}

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
    "ratchet\trt.timer_lateness_p99\t{}\t{}\t{}",
    lateness.median().unwrap_or(0),
    lateness.percentile(Percentile::P99).unwrap_or(0),
    lateness.values().last().copied().unwrap_or(0)
  );
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

  // The same round trip with the shard active: it spins for the profile's window before
  // parking, so a reply that lands within the window skips the kernel wake.
  let profile = slates_machine::MachineProfile::measure(slates_machine::ProfileOptions {
    budget_per_probe: Duration::from_millis(50),
    codecs: false,
    core_matrix: false,
  });
  let mut spinning = config(2);
  spinning.spin_ns = profile.derived().spin_before_park_ns.get();
  let two = Runtime::start(&spinning)?;
  let target = two.shard_ids()[1];
  two.set_active(target, true);
  let (tx, rx) = channel::<u64>();
  report(
    "foreign spawn and reply with the shard spinning before it parks",
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
  println!(
    "spin window {} ns; shard 1 spin hits {} misses {}",
    spinning.spin_ns, counters[1].spin_hits, counters[1].spin_misses
  );

  // What the spin is for: two shards waking each other through the pair rings. Each round trip
  // is two ring words and two kicks when the shards park, or two ring words alone when they spin.
  for (label, spin_ns) in [("parking", 0), ("spinning", spinning.spin_ns)] {
    let mut cfg = config(2);
    cfg.spin_ns = spin_ns;
    let (sample, hits, misses) = ping_pong(&cfg, spin_ns > 0);
    let interval =
      bootstrap_interval(&sample, &mut Xorshift::new(Xorshift::SEED)).unwrap_or(Interval {
        median: 0,
        lower: 0,
        upper: 0,
      });
    let m = Measurement {
      interval,
      p99_ns: sample.percentile(Percentile::P99).unwrap_or(0),
      min_ns: sample.min().unwrap_or(0),
      samples: u32::try_from(sample.len()).unwrap_or(u32::MAX),
      batch: 1,
      quick: false,
    };
    report(
      &format!("cross-shard wake round trip with both shards {label}"),
      m,
    );
    println!("  spin hits {hits} misses {misses}");
  }
  Ok(())
}

static TURN: AtomicU64 = AtomicU64::new(0);
static WAKERS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static REGISTERED: AtomicU64 = AtomicU64::new(0);

/// Waits for `TURN` to equal `side`, registering this task's waker word for the other side.
struct AwaitTurn {
  side: u64,
}

impl std::future::Future for AwaitTurn {
  type Output = ();
  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
    if let Some(word) = slates_rt::waker::word_of(cx.waker()) {
      WAKERS[usize::try_from(self.side).unwrap_or(0)].store(word.word(), Ordering::Release);
      REGISTERED.fetch_or(1 << self.side, Ordering::AcqRel);
    }
    if TURN.load(Ordering::Acquire) == self.side {
      std::task::Poll::Ready(())
    } else {
      std::task::Poll::Pending
    }
  }
}

fn wake_side(side: u64) {
  let word = WAKERS[usize::try_from(side).unwrap_or(0)].load(Ordering::Acquire);
  slates_rt::registry::wake(slates_mem::Encoded::from_word(word));
}

/// Runs `rounds` ping-pongs between a task on shard 0 and a task on shard 1; returns the
/// per-round-trip sample measured on side 0 and shard 1's spin counters.
fn ping_pong(cfg: &RuntimeConfig, active: bool) -> (Sample, u64, u64) {
  /// Shape: enough rounds for the bootstrap interval to mean something; a few milliseconds.
  const ROUNDS: u64 = 2000;
  TURN.store(0, Ordering::Release);
  REGISTERED.store(0, Ordering::Release);
  let rt = match Runtime::start(cfg) {
    Ok(rt) => rt,
    Err(_) => return (Sample::new(Vec::new()), 0, 0),
  };
  let ids = rt.shard_ids().to_vec();
  if active {
    rt.set_active(ids[0], true);
    rt.set_active(ids[1], true);
  }
  let (tx, rx) = channel::<Vec<u64>>();
  rt.spawn_on(ids[1], async move {
    // Side 1 echoes: wait for its turn, hand the turn back, and wake side 0.
    loop {
      AwaitTurn { side: 1 }.await;
      if TURN.load(Ordering::Acquire) == u64::MAX {
        break;
      }
      TURN.store(0, Ordering::Release);
      wake_side(0);
    }
  });
  rt.spawn_on(ids[0], async move {
    // Side 0 drives: register, wait until side 1 registered, then time each round trip.
    AwaitTurn { side: 0 }.await;
    while REGISTERED.load(Ordering::Acquire) & 2 == 0 {
      yield_now().await;
    }
    let mut times = Vec::with_capacity(usize::try_from(ROUNDS).unwrap_or(0));
    for _ in 0..ROUNDS {
      let started = std::time::Instant::now();
      TURN.store(1, Ordering::Release);
      wake_side(1);
      AwaitTurn { side: 0 }.await;
      times.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    let _ = tx.send(times);
  });
  let times = rx.recv().unwrap_or_default();
  TURN.store(u64::MAX, Ordering::Release);
  wake_side(1);
  let counters = rt.shutdown();
  (
    Sample::new(times),
    counters[1].spin_hits,
    counters[1].spin_misses,
  )
}
