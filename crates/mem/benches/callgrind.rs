//! Instruction-count benches for the memory crate under valgrind's callgrind (iai-callgrind):
//! deterministic for a given binary, so a one-percent change is visible where the wall clock's
//! between-run drift hides it (D-20; BENCHMARKS.md "Ratchets"). Runs only where valgrind exists
//! (CI's Linux lane); `cargo bench --bench callgrind --no-run` compiles it anywhere.

// The benchmark macros generate undocumented modules and constants; the doc rule is for our
// own items, which are documented above each function.
#![allow(missing_docs)]

use std::hint::black_box;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use slates_mem::buddy::Buddy;
use slates_mem::ring::SpscRing;
use slates_mem::slab::Slab;

fn slab() -> Slab<[u8; 64]> {
  let mut slab = Slab::new(256, 1024);
  slab.reserve_segments(4);
  slab
}

#[library_benchmark]
#[bench::insert_remove(slab())]
fn slab_insert_remove(mut slab: Slab<[u8; 64]>) -> usize {
  let handle = slab.insert([1u8; 64]).ok();
  if let Some(h) = handle {
    let _ = black_box(slab.remove(h));
  }
  black_box(slab.len())
}

fn buddy() -> Buddy {
  Buddy::new(4096, 12)
}

#[library_benchmark]
#[bench::one_page(buddy())]
fn buddy_alloc_free_page(mut buddy: Buddy) -> usize {
  if let Ok(block) = buddy.alloc(4096) {
    let _ = black_box(buddy.free(block));
  }
  black_box(buddy.free_bytes())
}

#[library_benchmark]
#[bench::split_and_coalesce(buddy())]
fn buddy_alloc_free_split(mut buddy: Buddy) -> usize {
  if let Ok(a) = buddy.alloc(4096) {
    if let Ok(b) = buddy.alloc(4096 * 64) {
      let _ = black_box(buddy.free(b));
    }
    let _ = black_box(buddy.free(a));
  }
  black_box(buddy.free_bytes())
}

fn ring() -> SpscRing {
  SpscRing::new(1024).unwrap_or_else(|_| SpscRing::new(1).unwrap_or_else(|_| unreachable_ring()))
}

fn unreachable_ring() -> SpscRing {
  // `SpscRing::new(1)` cannot fail (one is a power of two); this arm is never taken.
  loop {
    std::hint::spin_loop();
  }
}

#[library_benchmark]
#[bench::push_pop(ring())]
fn ring_push_pop(ring: SpscRing) -> Option<u64> {
  let (mut producer, mut consumer) = ring.split();
  let _ = producer.push(black_box(7));
  black_box(consumer.pop())
}

library_benchmark_group!(
  name = memory;
  benchmarks = slab_insert_remove, buddy_alloc_free_page, buddy_alloc_free_split, ring_push_pop
);

main!(library_benchmark_groups = memory);
