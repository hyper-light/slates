//! The TSan lane's non-vacuity canary (Part 6 "Concurrency", AUD-29-32). A race detector that reports
//! nothing proves nothing unless it is shown to report a race it must see, so the lane runs this test first
//! and requires ThreadSanitizer's data-race report from it before it trusts a clean run of the suites
//! (`cargo xtask tsan`, `xtask/src/tsan.rs`).
//!
//! The test is a deliberate data race, which is undefined behaviour: it is `#[ignore]`d, never run by a
//! plain `cargo test` or by Miri's lane (which names its targets), and is run only by the TSan task, under
//! the instrumentation that turns the race into a report. Two sibling threads write one word with nothing
//! ordering the two writes (ThreadSanitizer's shadow memory keeps the earlier write's thread and clock;
//! Serebryany and Iskhodzhanov, "ThreadSanitizer: data race detection in practice", WBIA 2009, §2).
//!
//! The two writes happen one after the other in time with nothing ordering them, and both writers stay alive
//! across both. The first writer writes, then raises a *relaxed* flag; the second waits for that flag, then
//! writes. ThreadSanitizer models a relaxed atomic as no synchronization, so the second write is not ordered
//! after the first, and since they never overlap, neither does the detector's own update of its shadow memory.
//! Each writer then waits for the other's acquire/release "done" before it exits, so neither thread's slot is
//! reused while the other writes. Two earlier versions were not reliable (2026-10-03): writers that could
//! finish before their sibling began missed the race in one of three CI runs (run 37143592977, "ran: true,
//! reported: false"); writers handshaking so that they wrote at the same instant missed it in 1 of 20 runs
//! here.

/// Format: the two values the racing writers store, distinct so either order is visible.
const WRITES: [u64; 2] = [1, 2];

/// T-0.3 (non-vacuity of the TSan lane): run two unsynchronized writers on one word; ThreadSanitizer must
/// report a data race. Run only by `cargo xtask tsan`.
#[test]
#[ignore = "a deliberate data race, run only under ThreadSanitizer by `cargo xtask tsan`"]
fn two_unsynchronized_writers_on_one_word_are_reported_as_a_race() {
  use std::sync::atomic::{AtomicBool, Ordering};
  let word = std::cell::UnsafeCell::new(0_u64);
  let address = word.get().expose_provenance();
  let first_wrote = AtomicBool::new(false);
  let done = [AtomicBool::new(false), AtomicBool::new(false)];
  let write = |value: u64| {
    let pointer = std::ptr::with_exposed_provenance_mut::<u64>(address);
    // SAFETY: none — this write races the sibling's on purpose. It is the canary that proves the detector
    // sees a race, and runs only under ThreadSanitizer (the module doc).
    unsafe { pointer.write_volatile(value) };
  };
  let wait = |flag: &AtomicBool, order: Ordering| {
    while !flag.load(order) {
      std::hint::spin_loop();
    }
  };
  let [first, second] = WRITES;
  std::thread::scope(|scope| {
    scope.spawn(|| {
      write(first);
      // Relaxed: the second writer learns the time, not an ordering.
      first_wrote.store(true, Ordering::Relaxed);
      let [mine, theirs] = &done;
      mine.store(true, Ordering::Release);
      wait(theirs, Ordering::Acquire);
    });
    scope.spawn(|| {
      wait(&first_wrote, Ordering::Relaxed);
      write(second);
      let [theirs, mine] = &done;
      mine.store(true, Ordering::Release);
      wait(theirs, Ordering::Acquire);
    });
  });
  let stored = word.into_inner();
  if !WRITES.contains(&stored) {
    eprintln!("race canary: the word holds {stored}, neither writer's value");
  }
}
