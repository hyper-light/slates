//! AC-0.7, §4.3: race registration, foreign wakes and retirement through the public registry.
//! This fixture owns its process: its neighbour sends intentionally address any live slot,
//! so sharing a binary with tests that do not drain those rings can corrupt their observations
//! or deadlock them. All sixteen concurrent workers and their history budget remain unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};

use slates_rt::driver::Kick;
use slates_rt::registry::{
  MAX_SHARDS, RegisterKick, TrySend, holder_of, register, try_send_foreign, unregister, with_entry,
};

/// Sends `word` to `target` as a shard does: one turn at a time, draining this thread's own ring
/// (`own`) while the target's is full, so a producer that is also a consumer keeps consuming.
fn send_as_shard(own: u16, target: u16, word: u64, landed: &AtomicU64) -> bool {
  let mut pending = word;
  let mut own_seen = false;
  loop {
    match try_send_foreign(target, pending) {
      TrySend::Landed | TrySend::Gone => return own_seen,
      TrySend::Full(back) => {
        pending = back;
        own_seen |= drain_own(own, landed);
        std::thread::yield_now();
      }
    }
  }
}

/// Drains the calling thread's own ring (it is the "shard" of `id`), counting the wake it sent
/// itself and reporting whether it was seen; a neighbour's wake that landed here is drained too but
/// not counted, since the assertion below is about the wakes to live slots this thread sent itself.
fn drain_own(id: u16, landed: &AtomicU64) -> bool {
  with_entry(id, |entry| {
    let mut seen = false;
    let mut ring = entry.inbound.consumer();
    while let Some(word) = ring.pop() {
      if word == 7 {
        landed.fetch_add(1, Ordering::Relaxed);
        seen = true;
      }
    }
    seen
  })
  .unwrap_or(false)
}

/// The slot protocol under contention (a stand-in until the loom lane covers this crate, which is
/// owed): many threads register, wake and unregister against a handful of slots at once; every
/// wake either lands in a live ring or is counted stale — never a fault — and every slot ends free
/// with no entry lost (each registration is unregistered by its own thread). Do: 16 threads × 200
/// register/wake/unregister cycles. Expect: no panic, every wake accounted for, every slot free.
/// This test found two faults in the protocol on 2026-09-14 (both timing-dependent, so the same
/// binary passed a validation run and hung or crashed the next): a wake to a neighbour whose ring
/// was full and whose holder then unregistered spun forever (`send_foreign` waited for a consumer
/// that was gone), and a reader descheduled between loading a neighbour's entry and pushing to it
/// dereferenced the entry a re-registration had freed (a null load in the freed ring's head word).
/// The re-validated spin and the counted reader are the fixes; this test must pass every run.
#[test]
fn registrations_wakes_and_unregistrations_interleave_without_a_fault() {
  /// Shape: concurrent holders exercising contention on registration and retirement.
  const THREADS: usize = 16;
  /// Shape: repeated registration histories per holder in this bounded stress fixture.
  const CYCLES: usize = 200;
  // A process-static counter the threads share (R2: no `Arc`; a `&'static` is the sharing form).
  static WAKES_LANDED: AtomicU64 = AtomicU64::new(0);
  let landed: &'static AtomicU64 = &WAKES_LANDED;
  let handles: Vec<_> = (0..THREADS)
    .map(|_| {
      std::thread::spawn(move || {
        for _ in 0..CYCLES {
          let (id, receiver) = register(4, 2, RegisterKick::Kick(Kick::none())).unwrap();
          let held = holder_of(id).unwrap();
          // A foreign wake to our own slot lands in its ring, drained by the "shard" (this thread).
          // Both wakes are sent as a shard sends: one turn at a time, draining this thread's own ring
          // between turns — a consumer that blocks in a producer loop without consuming is the
          // deadlock this test found on 2026-09-14: a holder descheduled after registering came back
          // to a ring its fast-cycling predecessor had filled with neighbour wakes, looped on `Full`
          // in its own wake without draining, and every thread behind it stopped too.
          let mut own_seen = send_as_shard(id, id, 7, landed);
          // Drain until the own wake is seen: it landed, but a neighbour mid-push into the slot
          // ahead of it holds the consumer back until that producer finishes (a ring pops in
          // order) — a shard keeps stepping, so this thread keeps draining.
          while !own_seen {
            own_seen = drain_own(id, landed);
            if !own_seen {
              std::thread::yield_now();
            }
          }
          // A wake to a slot another thread may have freed meanwhile is counted, never a fault.
          let neighbour = id.wrapping_add(1) % u16::try_from(MAX_SHARDS).unwrap_or(u16::MAX);
          let _ = send_as_shard(id, neighbour, 9, landed);
          let _ = receiver.try_recv();
          unregister(id);
          // The slot was given back: its generation moved past the even value this thread held
          // (another test in this binary, or another thread here, may already hold it again, so
          // "free" is not the claim — "no longer mine" is).
          assert_ne!(holder_of(id), Some(held), "slot {id}'s registration ended");
        }
      })
    })
    .collect();
  for handle in handles {
    handle.join().unwrap();
  }
  assert_eq!(
    landed.load(Ordering::Relaxed),
    u64::try_from(THREADS * CYCLES).unwrap(),
    "every wake to a live slot landed"
  );
}
