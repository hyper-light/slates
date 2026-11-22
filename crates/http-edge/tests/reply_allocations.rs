//! Ada's MEMORY SIZING ruling ("If we don't need to allocate ... we do not allocate"; A-830): the edge writes a reply's
//! runs into the connection's own output buffer, which the caller writes out and hands back, so a long reply (a clone's
//! pack, a run at a time) makes no allocation a run once its first run has sized the buffer. Measured before (A-830's
//! attribution of a git/git v2.0.0 clone over HTTP, 2026-11-22): every run made a fresh buffer the size of the run,
//! framed and copied it, then the output was taken whole and dropped, 4,145 MiB in 33,170 blocks over one clone.
//! Each thread counts its own allocator calls (as `crates/rt/tests/timer_allocations.rs` does), since libtest's
//! reporting thread allocates on its own.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_http::head::Head;
use slates_http_edge::connection::{Connection, Decision, Fields, Next, StatusMode};

struct Counting;
thread_local! {
  // Const initialization and no destructor: counting never allocates recursively.
  static CALLS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards the allocator contract unchanged; the counter observes calls only.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(pointer, layout) }
  }
  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(pointer, layout, size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Shape: the connection's room, enough for the request and a run.
const ROOM: u64 = 1 << 24;

/// Shape: one run's bytes, the size a pack's stream hands the edge a reply run (128 KiB).
const RUN_BYTES: usize = 128 * 1024;

/// Shape: the runs measured after the first, which sizes the buffer.
const MEASURED_RUNS: usize = 64;

/// Format: a GET for a git advertisement, HTTP/1.1.
const GET: &[u8] = b"GET /r.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: h\r\n\r\n";

/// The site's decision: serve, streamed.
fn serve(_: &Head<'_>) -> Decision {
  Decision::Serve {
    fields: Fields::new(),
    head_only: false,
    status: StatusMode::Streamed,
  }
}

/// A-830. Do: on a cleartext connection, decide a GET and answer it with one run of [`RUN_BYTES`], writing the output
/// out as the server's listener does (lend it, write it, hand it back); then [`MEASURED_RUNS`] runs more of the same
/// size, each written out, counting this thread's allocator calls across them. Expect: every run's bytes on the wire,
/// chunked, and no allocation across the measured runs.
#[test]
fn a_long_replys_runs_make_no_allocation_once_the_first_has_sized_the_buffer() {
  let mut connection = Connection::clear(ROOM);
  let run = vec![b'p'; RUN_BYTES];
  let mut wire = 0usize;
  let mut runs_sent = 0usize;
  let mut measured = None;
  loop {
    match connection.next_step() {
      Next::Read => connection.receive(GET),
      Next::Decide => {
        let decision = serve(&connection.head().unwrap());
        connection.decide(decision);
      }
      Next::Body(_) => connection.body_taken(),
      Next::BodyEnd => {}
      Next::Write => {
        wire += connection.output().len();
        connection.written();
      }
      Next::Reply if runs_sent <= MEASURED_RUNS => {
        if runs_sent == 1 {
          measured = Some(CALLS.get());
        }
        connection.reply(&run);
        runs_sent += 1;
      }
      Next::Reply => {
        let made = CALLS.get() - measured.expect("the first run was sent");
        assert_eq!(
          made, 0,
          "{MEASURED_RUNS} runs of {RUN_BYTES} bytes, each written out, made {made} allocations"
        );
        assert!(
          wire >= (MEASURED_RUNS + 1) * RUN_BYTES,
          "every run reached the wire: {wire} bytes"
        );
        return;
      }
      other => panic!("unexpected step {other:?}"),
    }
  }
}
