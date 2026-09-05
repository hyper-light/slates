//! The Phase 0 memory baseline: slab insert+remove, buddy alloc+free, ring push+pop, each with
//! the bootstrap interval the machine harness reports (Phase 0 task 7; BENCHMARKS.md).
//!
//! `cargo run --release -p slates-mem --example bench`

use std::time::Duration;

use slates_machine::bench::{Measurement, measure};
use slates_mem::buddy::Buddy;
use slates_mem::ring::SpscRing;
use slates_mem::slab::Slab;

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

/// The ratchet key of a row: `mem.` plus the row's name in snake case, whole, so two rows
/// that share their first words stay distinct.
fn key(name: &str) -> String {
  let words: Vec<&str> = name
    .split(|c: char| !c.is_ascii_alphanumeric())
    .filter(|w| !w.is_empty())
    .collect();
  format!("mem.{}", words.join("_").to_ascii_lowercase())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let budget = Duration::from_millis(500);
  let facts = slates_machine::facts::Facts::query();
  let page = usize::try_from(facts.page.base)?;

  let mut slab: Slab<[u8; 64]> = Slab::new(1024, 1 << 20);
  slab.reserve_segments(8);
  report(
    "slab insert+remove (64-byte slot)",
    measure(
      || {
        if let Ok(h) = slab.insert([1u8; 64]) {
          std::hint::black_box(slab.remove(std::hint::black_box(h)).ok());
        }
      },
      budget,
    ),
  );

  let mut buddy = Buddy::new(page, 12);
  report(
    "buddy alloc+free (one page)",
    measure(
      || {
        if let Ok(block) = buddy.alloc(page) {
          let _ = buddy.free(block);
        }
      },
      budget,
    ),
  );
  report(
    "buddy alloc+free (64 pages, split and coalesce)",
    measure(
      || {
        if let Ok(a) = buddy.alloc(page) {
          if let Ok(b) = buddy.alloc(page * 64) {
            let _ = buddy.free(b);
          }
          let _ = buddy.free(a);
        }
      },
      budget,
    ),
  );

  // The ring round trip that matters: two rings between two threads, the peer echoing each
  // word back; one round trip is a push, a cross-core handoff, an echo push and a pop.
  let to_peer = SpscRing::<u64>::new(1024)?;
  let from_peer = SpscRing::<u64>::new(1024)?;
  let stop = std::sync::atomic::AtomicBool::new(false);
  let mut round_trip = None;
  std::thread::scope(|scope| {
    let (mut peer_in_producer, mut peer_in_consumer) = to_peer.split();
    let (mut peer_out_producer, mut peer_out_consumer) = from_peer.split();
    let stop = &stop;
    scope.spawn(move || {
      while !stop.load(std::sync::atomic::Ordering::Acquire) {
        match peer_in_consumer.pop() {
          Some(word) => {
            while peer_out_producer.push(word).is_err() {
              std::hint::spin_loop();
            }
          }
          None => std::hint::spin_loop(),
        }
      }
    });
    let m = measure(
      || {
        while peer_in_producer.push(7).is_err() {
          std::hint::spin_loop();
        }
        while peer_out_consumer.pop().is_none() {
          std::hint::spin_loop();
        }
      },
      budget,
    );
    stop.store(true, std::sync::atomic::Ordering::Release);
    round_trip = Some(m);
  });
  if let Some(m) = round_trip {
    report("spsc ring round trip between two threads", m);
  }
  Ok(())
}
