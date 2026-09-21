//! Instruction-count benches for the memory crate under valgrind's callgrind (iai-callgrind):
//! deterministic for a given binary, so a one-percent change is visible where the wall clock's
//! between-run drift hides it (D-20; BENCHMARKS.md "Ratchets"). Runs only where valgrind exists
//! (CI's Linux lane); compile it with `cargo bench -p slates-mem --bench callgrind
//! --features instruction-counts --no-run`.
//! Every operation must succeed and return the advertised result. Teardown checks
//! prevent refused work from looking faster (2026-09-20 corrupt-frame negative control).

// The benchmark macros generate undocumented modules and constants; the doc rule is for our
// own items, which are documented above each function.
#![allow(missing_docs)]

use std::hint::black_box;

use iai_callgrind::{
  Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
use slates_mem::buddy::Buddy;
use slates_mem::error::MemError;
use slates_mem::ring::SpscRing;
use slates_mem::slab::Slab;

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

fn slab() -> Slab<[u8; 64]> {
  let mut slab = Slab::new(256, 1024);
  slab.reserve_segments(4);
  slab
}

fn check_slab(result: Result<([u8; 64], usize), MemError>) {
  let (removed, remaining) = require_success(result, "insert and remove the benchmark value");
  assert_eq!(removed, [1u8; 64]);
  assert_eq!(remaining, 0);
}

#[library_benchmark(teardown = check_slab)]
#[bench::insert_remove(slab())]
fn slab_insert_remove(mut slab: Slab<[u8; 64]>) -> Result<([u8; 64], usize), MemError> {
  count_instructions(move || {
    let handle = slab.insert([1u8; 64])?;
    let removed = black_box(slab.remove(handle))?;
    Ok(black_box((removed, slab.len())))
  })
}

fn buddy() -> Buddy {
  Buddy::new(4096, 12)
}

fn check_buddy(result: Result<(usize, usize, usize), MemError>) {
  let (region, free, largest) = require_success(result, "allocate and free every benchmark block");
  assert_eq!(free, region);
  assert_eq!(largest, region, "freed buddies must coalesce");
}

#[library_benchmark(teardown = check_buddy)]
#[bench::one_page(buddy())]
fn buddy_alloc_free_page(mut buddy: Buddy) -> Result<(usize, usize, usize), MemError> {
  count_instructions(move || {
    let block = buddy.alloc(4096)?;
    black_box(buddy.free(block))?;
    Ok(black_box((
      buddy.region_bytes(),
      buddy.free_bytes(),
      buddy.largest_free(),
    )))
  })
}

#[library_benchmark(teardown = check_buddy)]
#[bench::split_and_coalesce(buddy())]
fn buddy_alloc_free_split(mut buddy: Buddy) -> Result<(usize, usize, usize), MemError> {
  count_instructions(move || {
    let page = buddy.alloc(4096)?;
    let large = buddy.alloc(4096 * 64)?;
    black_box(buddy.free(large))?;
    black_box(buddy.free(page))?;
    Ok(black_box((
      buddy.region_bytes(),
      buddy.free_bytes(),
      buddy.largest_free(),
    )))
  })
}

fn ring() -> SpscRing {
  require_success(SpscRing::new(1024), "construct the benchmark ring")
}

fn check_ring(result: Result<Option<u64>, u64>) {
  assert_eq!(result, Ok(Some(7)), "deliver the word pushed into the ring");
}

#[library_benchmark(teardown = check_ring)]
#[bench::push_pop(ring())]
fn ring_push_pop(ring: SpscRing) -> Result<Option<u64>, u64> {
  count_instructions(move || {
    let (mut producer, mut consumer) = ring.split();
    producer.push(black_box(7))?;
    Ok(black_box(consumer.pop()))
  })
}

library_benchmark_group!(
  name = memory;
  benchmarks = slab_insert_remove, buddy_alloc_free_page, buddy_alloc_free_split, ring_push_pop
);

main!(
  config = LibraryBenchmarkConfig::default().tool(
    Callgrind::with_args(["--collect-atstart=no"]).entry_point(EntryPoint::None)
  );
  library_benchmark_groups = memory
);
