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
//! across both. The first writer writes, raises a *relaxed* flag, and parks; the second waits for that flag,
//! writes, then raises an acquire/release flag and unparks the first. ThreadSanitizer models a relaxed atomic
//! as no synchronization, so the second write is not ordered after the first, and since they never overlap,
//! neither does the detector's own update of its shadow memory.
//!
//! The first writer parks rather than polls because ThreadSanitizer reports a race only once it has found the
//! earlier access again in that thread's trace, and drops the race as spurious when it cannot
//! (`compiler-rt/lib/tsan/rtl/tsan_rtl_report.cpp`, after `RestoreStack`). The trace keeps three parts of
//! 32,763 events by default, and every instrumented access, function entry and function exit adds an event.
//! A probe whose first writer made K logged loads after its write, about 14 events each, was reported up to
//! K = 6,912 and lost from 6,976; with `history_size=7` (ten parts), up to 23,168 and from 23,232, the same
//! 10:3. The shipped canary's first writer polled after its write, so the race went unreported whenever the
//! second writer came late: in 2 of 99 CI runs (38704730770, 39925420604), in 120 of 120 runs on the
//! measuring Mac at load average 18 to 31, and in none of 30 there at background priority, where the polling
//! ran slower. Parked, it was reported in all 90 runs of both kinds, and in every probe run with up to 200 ms
//! between the writes (2026-10-23, aarch64 macOS, nightly 2026-09-04;
//! `docs/bugs/2026-10-23-the-tsan-canary-went-unreported-when-its-first-writer-polled.md`).
//!
//! Three earlier shapes missed the race: writers that could finish before their sibling began (one of three
//! CI runs, 2026-10-03, run 37143592977, "ran: true, reported: false"), writers handshaking so that they
//! wrote at the same instant (1 of 20 runs, 2026-10-03), and the polling first writer above.

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
  let first_wrote = &AtomicBool::new(false);
  let second_wrote = &AtomicBool::new(false);
  let write = |value: u64| {
    let pointer = std::ptr::with_exposed_provenance_mut::<u64>(address);
    // SAFETY: none — this write races the sibling's on purpose. It is the canary that proves the detector
    // sees a race, and runs only under ThreadSanitizer (the module doc).
    unsafe { pointer.write_volatile(value) };
  };
  let [first, second] = WRITES;
  std::thread::scope(|scope| {
    let first_writer = scope.spawn(move || {
      write(first);
      // Relaxed: the second writer learns the time, not an ordering.
      first_wrote.store(true, Ordering::Relaxed);
      // Parked, not polling: a poll adds to this thread's trace, which must still hold the write when the
      // second one lands (the module doc).
      while !second_wrote.load(Ordering::Acquire) {
        std::thread::park();
      }
    });
    let first_thread = first_writer.thread().clone();
    scope.spawn(move || {
      while !first_wrote.load(Ordering::Relaxed) {
        std::hint::spin_loop();
      }
      write(second);
      second_wrote.store(true, Ordering::Release);
      first_thread.unpark();
    });
  });
  let stored = word.into_inner();
  if !WRITES.contains(&stored) {
    eprintln!("race canary: the word holds {stored}, neither writer's value");
  }
}
