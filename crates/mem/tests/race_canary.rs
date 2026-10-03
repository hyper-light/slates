//! The TSan lane's non-vacuity canary (Part 6 "Concurrency", AUD-29-32). A race detector that reports
//! nothing proves nothing unless it is shown to report a race it must see, so the lane runs this test first
//! and requires ThreadSanitizer's data-race report from it before it trusts a clean run of the suites
//! (`cargo xtask tsan`, `xtask/src/tsan.rs`).
//!
//! The test is a deliberate data race, which is undefined behaviour: it is `#[ignore]`d, never run by a
//! plain `cargo test` or by Miri's lane (which names its targets), and is run only by the TSan task, under
//! the instrumentation that turns the race into a report. Two sibling threads write one word with no
//! synchronization between them. Each thread's spawn orders it after the parent, but nothing orders the two
//! writes against each other, so ThreadSanitizer reports them on every run whatever the interleaving (its
//! shadow memory keeps the earlier write's thread and clock; Serebryany and Iskhodzhanov, "ThreadSanitizer:
//! data race detection in practice", WBIA 2009, §2).

/// Format: the two values the racing writers store, distinct so either order is visible.
const WRITES: [u64; 2] = [1, 2];

/// T-0.3 (non-vacuity of the TSan lane): run two unsynchronized writers on one word; ThreadSanitizer must
/// report a data race. Run only by `cargo xtask tsan`.
#[test]
#[ignore = "a deliberate data race, run only under ThreadSanitizer by `cargo xtask tsan`"]
fn two_unsynchronized_writers_on_one_word_are_reported_as_a_race() {
  let word = std::cell::UnsafeCell::new(0_u64);
  let address = word.get().expose_provenance();
  std::thread::scope(|scope| {
    for value in WRITES {
      scope.spawn(move || {
        let pointer = std::ptr::with_exposed_provenance_mut::<u64>(address);
        // SAFETY: none — this write races the sibling's on purpose. It is the canary that proves the
        // detector sees a race, and runs only under ThreadSanitizer (the module doc).
        unsafe { pointer.write_volatile(value) };
      });
    }
  });
  let stored = word.into_inner();
  if !WRITES.contains(&stored) {
    eprintln!("race canary: the word holds {stored}, neither writer's value");
  }
}
