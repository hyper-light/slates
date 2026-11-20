//! The completion bridge's memory (§4.7 "signal the completion fd", D-19; CLAUDE.md §2 item 8): a client that turns
//! its async completion on and off again and again, as an SDK or a host's `AsyncInstance` handing its client back
//! does, keeps nothing for it. The binary runs on an allocator that marks each block with whether its thread was being
//! measured when it was made, so a block the measuring thread makes is counted until it is freed, on whichever thread
//! frees it (a bridge's thread frees what its closure carried). Its own binary, as every allocation test here is
//! (`crates/rt/tests/timer_allocations.rs`, `crates/mem/tests/no_alloc.rs`): a counting allocator owns its process.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use slates_ipc::ClientEnd;
use slates_ipc::region::{ClientRegion, RegionGeometry};

/// The system allocator with a header before every block saying whether its thread was measured when it was made.
struct Measuring;

thread_local! {
  // Const initialization and no destructor: reading it never allocates, and it reads during a thread's teardown.
  static MEASURED: Cell<bool> = const { Cell::new(false) };
}

/// Blocks a measured thread made, and those of them freed since, on any thread.
static MADE: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);

/// The block `layout` asks for with the header before it, and the header's length: at least a word, and a whole
/// multiple of the alignment asked for, so the caller's bytes keep their alignment.
fn with_header(layout: Layout) -> Option<(Layout, usize)> {
  let header = layout.align().max(size_of::<usize>());
  let whole = Layout::from_size_align(
    layout.size().checked_add(header)?,
    layout.align().max(align_of::<usize>()),
  )
  .ok()?;
  Some((whole, header))
}

// SAFETY: every block is the system allocator's, made and freed with one layout: the caller's with the header before
// it, which `with_header` derives from the caller's layout the same way at both ends.
unsafe impl GlobalAlloc for Measuring {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let Some((whole, header)) = with_header(layout) else {
      return std::ptr::null_mut();
    };
    // SAFETY: `whole` is at least a word long, never zero-sized.
    let block = unsafe { System.alloc(whole) };
    if block.is_null() {
      return block;
    }
    let measured = MEASURED.try_with(Cell::get).unwrap_or(false);
    if measured {
      MADE.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: the block is aligned for a word and its header holds at least one.
    unsafe { block.cast::<usize>().write(usize::from(measured)) };
    // SAFETY: the caller's bytes start `header` bytes in, inside the block.
    unsafe { block.add(header) }
  }

  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    let Some((whole, header)) = with_header(layout) else {
      return;
    };
    // SAFETY: `pointer` is one `alloc` returned for `layout`, `header` bytes into its block.
    let block = unsafe { pointer.sub(header) };
    // SAFETY: the word `alloc` wrote at the block's start.
    if unsafe { block.cast::<usize>().read() } == 1 {
      FREED.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: the block the system allocator made for `whole`.
    unsafe { System.dealloc(block, whole) }
  }
}

#[global_allocator]
static ALLOCATOR: Measuring = Measuring;

/// The blocks `run` made on this thread that nobody had freed when it returned.
fn kept_by(run: impl FnOnce()) -> usize {
  let (made, freed) = (MADE.load(Ordering::Relaxed), FREED.load(Ordering::Relaxed));
  MEASURED.set(true);
  run();
  MEASURED.set(false);
  let made = MADE.load(Ordering::Relaxed).saturating_sub(made);
  made.saturating_sub(FREED.load(Ordering::Relaxed).saturating_sub(freed))
}

/// Shape: the turns measured. A turn that kept even one block keeps at least this many.
const TURNS: usize = 64;
/// Shape: the turns before measuring, which take what a process makes once (a lazy table, a thread-local's first use).
const WARM_TURNS: usize = 4;

/// Shape: a small region; the bridge reads only its words.
fn geometry() -> RegionGeometry {
  RegionGeometry {
    slots: 8,
    spin_ns: 200_000,
    spin_shift: 4,
    bulk_bytes: 4096,
    page: 4096,
  }
}

/// One turn: the async completion on, its bridge started, then off, its bridge stopped and joined.
fn turn(client: &mut ClientEnd) {
  #[cfg(not(target_os = "linux"))]
  client.enable_async_completion().unwrap();
  // Linux starts its bridge only for an SDK that adopts the descriptor, on a dup it hands over to close.
  #[cfg(target_os = "linux")]
  {
    use std::os::fd::{FromRawFd, OwnedFd};
    let adopted = client.enable_async_completion_dup().unwrap();
    // SAFETY: the dup is the caller's to close, as `enable_async_completion_dup` says, and nothing else holds it.
    drop(unsafe { OwnedFd::from_raw_fd(adopted) });
  }
  client.disable_async_completion().unwrap();
}

/// CLAUDE.md §2 item 8 (no unbounded growth), §4.7 (D-19): do turn a client's async completion on and off
/// [`TURNS`] times, as an SDK or a host's `AsyncInstance` handing its client back does, counting the blocks the
/// turning thread makes that nobody frees. Expect fewer kept than turns, so no turn keeps one. Before 2026-11-18 each
/// start of the macOS and Windows bridge leaked two (`Box::leak`): its stop flag and the mark a synchronous park
/// raised for it.
#[test]
fn a_client_turning_its_async_completion_on_and_off_keeps_no_memory() {
  let name = format!("slates-completion-memory-{}", std::process::id());
  let region = ClientRegion::create(&name, 7, 0, geometry()).unwrap();
  let (handoff, len) = region.handoff().unwrap();
  let mut client = ClientEnd::new(ClientRegion::open(&handoff, len).unwrap());
  #[cfg(target_os = "linux")]
  client.set_completion(
    rustix::event::eventfd(
      0,
      rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
    )
    .unwrap(),
  );
  for _ in 0..WARM_TURNS {
    turn(&mut client);
  }
  let kept = kept_by(|| {
    for _ in 0..TURNS {
      turn(&mut client);
    }
  });
  assert!(
    kept < TURNS,
    "{kept} blocks kept over {TURNS} turns of the async completion: a turn keeps memory"
  );
  drop(client);
  drop(region);
}
