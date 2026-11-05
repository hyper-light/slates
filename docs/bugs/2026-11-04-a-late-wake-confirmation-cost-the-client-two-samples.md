# A late wake confirmation cost the client two samples

Observed on adacachy (AMD Ryzen 9 9950X3D2, 16 cores, 32 threads, Linux 7.2.9-2-cachyos), 2026-11-02 and again on
2026-11-04, in `a_client_learns_its_wake_from_the_parks_a_reply_ended` (`crates/ipc/tests/rings.rs`; §4.7, A-31).
It closes the GAPS entry "Wake-learning fixture's timing assumption still open". The cause was not the fixture's
timing, as recorded on 2026-11-02. It was the product: the daemon's confirmation of a wake could land after the
client had moved on, and then cost the client that sample and the next one.

## Failure

The case drives 32 parked trips (the client spins for zero, the daemon replies a millisecond after the parked flag
rises) and requires that at least 16 teach the estimate. On 2026-11-02 a capped all-ring run with zero idle cores
learned 7 of 32. On 2026-11-04, unchanged `origin/main` (`76b450a1`), in a capped unit with no pinning (load
average about 45 on 32 threads), the case failed 7 of 100 runs, learning 5 to 14 of 32 each time. All 32 trips
parked and delivered their replies every time.

## Cause, from the logs

A temporary diagnostic printed what the client's settle step did with each park
(`~/.cache/slates-agents-scratch/fixtures/diag2.sh`; the eprintln lines are not in the change). In a capped unit
pinned to two CPUs with four busy loops (`AllowedCPUs=0-1`), 7 of 60 runs failed. Each failing run splits its 32
parks the same way:

| run | learned | given up unconfirmed | stamp outside the wait | of which the stamp's time was 0 |
|---|---|---|---|---|
| 39 | 1 | 15 | 15 | 15 |
| 43 | 2 | 15 | 15 | 15 |
| 34 | 10 | 11 | 11 | 11 |
| 2 | 14 | 9 | 9 | 9 |

Every out-of-window stamp read zero nanoseconds with the confirmed bit set, and their count equals the given-up
count in every run. The sequence that produces both:

1. The daemon stamps the reply time, calls the futex wake, and is preempted before it sets the confirmed bit. On
   two busy CPUs the woken client takes the waker's CPU.
2. The client returns, reads the stamp unconfirmed, keeps it unsettled, sends its next request and parks again.
   That park settles first (still unconfirmed), gives the sample up and clears the stamp to zero.
3. The daemon resumes and ORs the confirmed bit into the cleared word: `0 | confirmed`.
4. The next reply stamps only a zero word (`compare_exchange(0, …)`), so it is refused. The client settles
   `0 | confirmed` as a confirmed stamp at time 0, outside its wait, and learns nothing.

So one late confirmation cost two samples. A-31's design text said such a confirmation "is settled at its next
wait"; that holds only if the waker confirms before the client's next park, which preemption does not ensure.

Fixing the bare bit alone (confirming by `compare_exchange(stamp, stamp | confirmed)`) was measured and is not
enough. The out-of-window count fell to 0, but 10 of 100 runs still failed on two busy CPUs. In those runs the
woken client outran its waker on nearly every trip, and 26 to 29 of 32 samples were given up, because one word
cannot hold a park's confirmation while the next park uses it.

## Impact

Under load a client's spin window stopped learning: every park whose waker was preempted between its wake and its
confirmation taught nothing, and neither did the park after it. On two busy CPUs the unchanged code learned 0 to
14 of 32 wakes. The estimate is never taught a wrong value (a stamp outside the wait is never measured); it stays
at its prior for longer. No reply was lost or late.

## Exact edits

- `crates/ipc/src/region.rs`: layout version 3. The reply stamp is two `u64` slots in its cache line
  (`REPLY_STAMP_SLOTS`). The parked flag's value names the park's slot: `PARKED_STAMP_SLOT_ZERO` (1),
  `PARKED_STAMP_SLOT_ONE` (2). Zero still means not parked, and any other armed value names slot 0.
  `reply_stamp(slot)` takes the slot.
- `crates/ipc/src/endpoint.rs`, `ClientEnd::wait`: each park takes the other slot from the last. Unconfirmed
  parks are held per slot and settled at each arming and after each wake. One is given up only when its slot is
  armed again. One daemon serves a ring in order, so by then the reply call that stamped the slot has returned.
- `DaemonEnd::reply`: reads the flag's value inside the park protocol's fence and stamps the slot it names. It
  confirms by an exchange on the exact stamp it wrote, so a confirmation never lands on a word it did not stamp.
- `arm_async` arms slot 0 (the async path times no wake).
- `crates/ipc/tests/rings.rs`: the unanswered-wake case puts its marker in the slot the client's next park names
  (the other one from the flag's), and both armed checks read "not zero" rather than "one". No assertion changed:
  the learning case still requires 16 of 32 parks and an estimate below half its prior.
- §4.7 (data model, wake strategy, status) and amendment A-199; GAPS.

## Verification

Both binaries were built from the same tree state apart from this change. The before binary came from `origin/main`
`76b450a1` in a scratch worktree, the after binary from this branch. Each was copied aside, and they were run
interleaved (A, then B, per round) inside capped units on adacachy on 2026-11-04. The busy loops ran inside the
unit, on its CPUs only. Logs: `~/.cache/slates-agents-scratch/fixtures/wake-ab-*.log`.

```sh
EXTRA="-p AllowedCPUs=<cpus>" capped.sh 2400 \
  "ab.sh 100 <busy loops> rings-before rings-after a_client_learns_its_wake_from_the_parks_a_reply_ended --exact"
```

| load | before: failures | after: failures |
|---|---|---|
| one CPU, two busy loops | 100 of 100 | 0 of 100 |
| two CPUs, four busy loops | 89 of 100 | 0 of 100 |
| unpinned, the whole box (load average 15) | 0 of 100 | 0 of 100 |
| two CPUs, four busy loops, all nine ring cases | 95 of 100 | 0 of 100 |

A separate 100-run sample of the after binary on two CPUs with four busy loops also failed 0 of 100. The whole box
was not saturated during the A/B (load average 15 to 18 of 32 threads), so the unpinned row passes either way. The
pinned rows are the load that reproduces the failure. Every after-fix run learned at least 16 of 32.
