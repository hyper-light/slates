# A Linux client learned a wake from a park that never slept (2026-11-23)

## Description
`crates/ipc/tests/rings.rs`, `a_reply_sent_before_the_client_blocked_teaches_its_wake_nothing`, failed on Linux. A
plain debug build on adacachy2 at main db3ddff02 reported: "racing: 16384 parks in 16426 trips, 5341 parked and never
switched out, 4 of them taught a wake [9286, 645, 669, 560] ns". CI's ThreadSanitizer lane failed it in two of five
runs on 2026-11-23 (runs 45203217471 and 45228216366: 1 of 22 and 2 of 40 such parks taught a wake). CI's `gates` job
stops at fmt or clippy before its tests, so no plain Linux lane had run it.

## Root cause
A Linux client folds a park's wake into its estimate when the daemon confirms that its wake found a sleeper:
`futex_wake` reported a waiter woken (`ClientEnd::settle_wakes`). `wait_may_teach` let every such park go on to be
judged and ignored the client's own switch count, which `park_once` already reads before and after every wait on every
platform. But `futex_wake` counts a waiter that has queued itself and not yet blocked. When the wake lands between the
waiter's queueing and its `schedule()`, the wake sets it running again, it never switches out, and its voluntary switch
count (`ru_nvcsw`) does not move. The client then timed an on-CPU handoff (560 to 669 ns) or a preemption (9,286 ns) as a
sleeper's wake (4,216 ns was the sleepers' estimate on the same run). This is the race A-223 closed on Windows, where
the client already judged by its own count.

## Impact
- A Linux client's wake estimate, which sets how long it spins before it parks, took in a few samples that were not
  wakes: 4 in 16,384 parks against a daemon that replies the instant the flag rises. The estimate is a moving mean of
  about 2^shift samples, so each such sample pulled it toward the handoff's time.
- macOS has no per-thread switch count here (`slates_machine::switches::switches_now` is `None`), so it keeps the
  waker's confirmation alone; the same race there is unmeasured.

## Edits
- `crates/ipc/src/endpoint.rs`: `wait_may_teach` judges by the client's own switch count wherever the OS gives one:
  a wait goes on to teach only if the count rose across it. Where the OS gives none, the waker's confirmation alone
  decides, as before. On Linux a park now teaches only if it switched out and the daemon confirmed a sleeper found
  (`settle_wakes` still waits for the confirmation). Windows is unchanged (no waker confirms there), and so is macOS
  (no count).
- No call added: `park_once` already read the switch count (`getrusage(RUSAGE_THREAD)` on Linux) before and after
  every wait. Before this change Linux paid for both reads and used neither. They cost about 200 ns a park
  (BENCHMARKS, "What a Linux park's two switch-count reads cost").
- `ClientEnd::wakes_held` counts the parks whose wake was held to be learned. Every learned sample passes that gate.
  The rings test now judges each racing trip by that count, not by the estimate's samples. A Linux park's sample is
  folded when its stamp's confirmation lands, which can be at the next park's arming, so a sample read across one trip
  can belong to the trip before. With the rule fixed, the samples-based judgement still failed 2 runs of 3 on adacachy2
  (1 and 2 samples, each a switched park's, settled late), while no unswitched park was held.
- Tests, red first: the unit test `only_a_wait_that_switched_out_teaches_where_the_client_judges_itself` now expects a
  confirmed wait whose count did not move to teach nothing, and the rings test as above.
- `crates/ipc/examples/ipc_bench.rs`, found while measuring the reads: the spinning row names its spin. A sync wait
  parks at once unless its caller names one (A-799, 85b88770e), so the row had parked 17,753 to 18,726 of its 20,000
  trips and aborted on its zero-wakes assertion in every run since.

## Evidence
- Red on adacachy2 (plain debug, db3ddff02): the rings test failed with 4 of 5,341 unswitched parks taught. The unit
  test failed on the old rule: "the daemon's confirmation with the client's count, Some(7) → Some(7)".
- Red check of the held count, with the old rule put back in a copy of the tree: red in 2 runs of 2, "3604 of 3604
  parks that never switched out had their wake held to be learned", and 5,146 of 5,146.
- Green: the unit test, and the rings test in 3 runs of 3 (7,110, 7,375 and 5,474 parks never switched out, 0 held;
  9,274, 9,009 and 10,910 wakes held in all).
- macOS: the rings test skips loudly as before (no per-thread switch count).
