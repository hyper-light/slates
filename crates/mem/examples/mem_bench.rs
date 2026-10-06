//! The Phase 0 memory baseline: slab insert+remove, buddy alloc+free, ring push+pop, each with
//! the bootstrap interval the machine harness reports (Phase 0 task 7; BENCHMARKS.md).
//!
//! `cargo run --release -p slates-mem --example mem_bench`

use std::time::Duration;

use slates_machine::bench::{Measurement, measure};
use slates_mem::buddy::Buddy;
use slates_mem::ring::SpscRing;
use slates_mem::slab::Slab;

fn report(name: &str, m: Measurement) {
  report_line("ratchet", name, m);
}

/// A row whose cost depends on which cores the OS placed two threads on: gated where the OS
/// pins threads (Linux, Windows), informational where it only hints or refuses (macOS), because a
/// cross-cluster placement is not a regression of the code.
fn report_placed(name: &str, m: Measurement) {
  let pinned = matches!(
    slates_machine::probes::pin_current_thread(0),
    slates_machine::probes::Pinning::Pinned
  );
  report_line(if pinned { "ratchet" } else { "ratchet-info" }, name, m);
}

fn report_line(tag: &str, name: &str, m: Measurement) {
  println!(
    "{tag}	{}	{}	{}	{}",
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

  let mut buddy = Buddy::new(page, 12)?;
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

  // A-105: a released block zeroed in place (every page stays resident) against its pages given back to the OS (a
  // syscall now, and a fault per page when the block is reused): the measurement that makes the give-back an idle
  // purge rather than a step of every free. Each iteration allocates a block, touches every page (as a write does),
  // frees it and, in the second row, purges, so that row pays the refault it causes.
  /// Shape: the block the rows release: one content chunk of sixteen pages.
  const CHUNK_PAGES: usize = 16;
  let block = page * CHUNK_PAGES;
  for (name, discard_from) in [
    (
      "arena alloc+touch+free (16 pages, zeroed in place)",
      usize::MAX,
    ),
    ("arena alloc+touch+free (16 pages, pages given back)", page),
  ] {
    let mut arena = slates_mem::arena::ChunkArena::new(page).discarding_from(discard_from);
    arena.add_region(slates_mem::region::Region::map(block * 64, page, false)?)?;
    report(
      name,
      measure(
        || {
          if let Ok(extent) = arena.alloc(block) {
            if let Some(bytes) = arena.bytes_mut(extent) {
              for at in (0..bytes.len()).step_by(page) {
                if let Some(byte) = bytes.get_mut(at) {
                  *byte = 1;
                }
              }
            }
            let _ = arena.free(extent);
            std::hint::black_box(arena.purge(usize::MAX));
          }
        },
        budget,
      ),
    );
  }

  // The ring round trip that matters: two rings between two threads, the peer echoing each
  // word back; one round trip is a push, a cross-core handoff, an echo push and a pop.
  let to_peer = SpscRing::new(1024)?;
  let from_peer = SpscRing::new(1024)?;
  let stop = std::sync::atomic::AtomicBool::new(false);
  let mut round_trip = None;
  let (peer_in_producer, peer_in_consumer) = to_peer.split().ok_or("a fresh ring splits")?;
  let (peer_out_producer, peer_out_consumer) = from_peer.split().ok_or("a fresh ring splits")?;
  std::thread::scope(|scope| {
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
    report_placed("spsc ring round trip between two threads", m);
  }
  Ok(())
}
