//! Audit step 7 (2026-11-06): a receive worker's batch makes no system allocation once the worker has started. The
//! per-phase profile of the WinFsp receiver against the DLL dispatcher (diagnostics runs 42178889880 to 42190796283)
//! found one cost on the request path that crash safety does not need: each received batch was parsed into two fresh
//! vectors and sorted. A counting global allocator proves zero calls across a round of receive, take, save, acknowledge,
//! over the journal's own API. Counts belong to the allocating thread, as in `crates/mem/tests/no_alloc.rs`, so libtest's
//! reporting cannot contaminate them.

// Test harness code: an unwrap or a fixed frame offset here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_bridge_winfsp::inflight::capture::{BYTES, Capture};
use slates_bridge_winfsp::inflight::{CLOSE_KIND, REQUEST_BASE, Scratch};

struct Counting;

thread_local! {
  // Const initialization and no destructor keep the counter free of allocations.
  static CALLS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every method forwards to the system allocator unchanged; only a counter is added.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded with the same layout.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: forwarded with the pointer and layout the caller received from `alloc`.
    unsafe { System.dealloc(ptr, layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Format: a Close request's frame at its smallest size, as WinFsp's driver writes it (`FSP_FSCTL_TRANSACT_REQ`): the
/// version, the size, the kind and the hint, little-endian, the rest zero.
fn close_frame(into: &mut [u8], hint: u64) {
  into.fill(0);
  let size = u16::try_from(REQUEST_BASE).unwrap();
  into[2..4].copy_from_slice(&size.to_le_bytes());
  into[4..8].copy_from_slice(&CLOSE_KIND.to_le_bytes());
  into[8..16].copy_from_slice(&hint.to_le_bytes());
}

/// One worker round over the journal: receive one Close into the buffer the next receive fills, take it, save its
/// response, and acknowledge it, which ends the batch. `scratch` is the worker's index, kept across its batches.
fn round(region: &mut [u8], scratch: &mut Scratch, hint: u64) {
  let capture = Capture::new(region).unwrap();
  // Each round fills one frame, so the next receive clears only that much.
  let mut receiving = capture.begin(Some(REQUEST_BASE)).unwrap();
  close_frame(&mut receiving.buffer_mut()[..REQUEST_BASE], hint);
  let mut batch = receiving.received(REQUEST_BASE, scratch).unwrap();
  let request = batch.next_request().unwrap().unwrap();
  let response = request.cancelled_response().unwrap();
  batch.record_response(&request, &response).unwrap();
  batch.acknowledge(request.hint()).unwrap();
}

/// Audit step 7. Do: run a hundred receive rounds over one journal after a first round. Expect: no system allocation in
/// any of them. Before, each batch made two (its requests and their hints, for the duplicate check).
#[test]
fn a_received_batch_is_served_without_a_system_allocation() {
  let mut region = vec![0u8; BYTES];
  let mut scratch = Scratch::new().unwrap();
  round(&mut region, &mut scratch, 1);
  let before = CALLS.get();
  for hint in 2..=101 {
    round(&mut region, &mut scratch, hint);
  }
  let calls = CALLS.get() - before;
  assert_eq!(calls, 0, "100 batches made {calls} system allocations");
}
