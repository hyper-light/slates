# A spinning shard gave its core up on every turn (2026-11-05, §4.3, §4.7, R9; macOS)

## Description

Inside the idle window a client's activity opens, a shard with nothing to run spins before it parks (§4.3
"Loop"). Each turn asks its rings, its pollers and its driver, and the driver is asked by a poll that must not
block (`Shard::harvest_io`, the driver's `wait(Some(0))`). §4.3 says so: "Every non-blocking harvest is one that
never sleeps." On macOS the kqueue driver (`crates/rt/src/kqueue.rs`) made that poll with `kevent` and a
zero-valued timeout, one of the two ways kevent(2) names to poll. On this machine that call sleeps.

## Measurement

All on Apple M5 Max (18 cores), macOS 26.4.1 (25E253), 2026-11-05 20:00–20:30 UTC, load 5–16 with other agents'
builds beside it. Probes kept in `~/.cache/slates-agents-scratch/clients/turnprobe` (not in the tree); every call's
return checked; each form on a fresh queue.

- **One call, 5,000 a round, three rounds** (`checked`):

  | form | ns a call | involuntary switches a call |
  |---|---|---|
  | `kevent`, zero timespec | 14,752 / 14,866 / 14,737 | 1.01 / 1.02 / 1.01 |
  | `kevent64`, zero timespec | 14,598 / 14,757 / 15,524 | 1.00 / 1.00 / 1.07 |
  | `kevent64`, `KEVENT_FLAG_IMMEDIATE` | 186 / 182 / 180 | 0.00 / 0.00 / 0.00 |
  | `getppid` (a plain syscall, `switches`) | 88 / 99 / 90 | 0.00 |

  So the zero-timeout poll blocks the thread for about 15 µs every time; the immediate flag does not.
- **Two threads spinning on a word, each turn carrying the poll,** 10,000 round trips a round, three rounds
  (`hop2`): with a zero timespec, median 15,208 / 15,166 / 15,333 ns (`kevent`) and 15,541 / 15,083 / 15,334 ns
  (`kevent64`), about 2.2 involuntary switches a round trip; with the immediate flag, 416 / 416 / 416 ns and 0.00–0.02;
  a bare spin, 167–250 ns.
- **`rt_bench`'s cross-shard wake round trip,** both shards spinning, main against the fix, interleaved, five rounds:
  main 7,792 / 7,667 / 8,000 / 6,375 / 7,000 ns, the fix 625 / 666 / 625 / 666 / 625 ns. Both shards parking: main
  7,125 / 7,542 / 7,208 / 5,959 / 7,125 ns, the fix 7,166 / 6,458 / 7,041 / 7,583 / 6,833 ns. On main a spinning round
  trip cost what a parked one did: the spin saved nothing.
- `xctrace` (Time Profiler) on `provision_bench`'s single-client form, earlier the same day: `harvest_io` →
  `kevent` held 41–42% of the shard threads' samples.

## Root cause

With a zero-valued timeout the thread is scheduled out and back on every call, about 15 µs and one involuntary
context switch, while `KEVENT_FLAG_IMMEDIATE` returns from the scan without either. That much is measured; why XNU
takes a waiting path for a deadline already past was not read from its source here (the likeliest reading, a timed
wait armed at "now" plus the timer's leeway, is unverified). The idle spin made one such call every turn, so a
message landing during the spin waited out the rest of a 15 µs sleep, which is longer than the wake the spin exists
to save (the shards' measured wake mean, 4–10 µs here).

A second fact, met while fixing it: XNU fixes a queue's record format at its first call. A `kevent64` on a queue that
legacy `kevent` had registered is refused `EINVAL`. The first form of this fix changed only the wait and failed every
wait (`driver_errors` equal to `waits`, 1,091,904 of them, in `rt_bench`), so every call on the queue moved.

## Impact

Every macOS daemon. A spinning shard was a sleeping shard in 15 µs naps:
- every cross-shard hop to a spinning shard paid about one nap: the forwarded create's legs measured 8–16 µs
  each, and 80% of single-client creates are forwarded;
- the single-client create's p50 sat at 60–75 µs (GAPS), against 9–18 µs on CI's Linux and Windows;
- the shards parked more: 94–161 parks a thousand creates on the single-client form, against 18–44 now.

Linux (`epoll_wait` with a zero timeout) and Windows were not affected by this; the io_uring driver's equivalent was
fixed on 2026-09-26 (`docs/bugs/2026-09-26-io-uring-zero-timeout-harvest-sleeps.md`).

## Edits

- `crates/rt/src/kqueue.rs`: every call on the queue is `kevent64` (the kick's registration and trigger through one
  `apply_change`, the read and write filters through `register_once`, the wait), and a zero timeout is
  `KEVENT_FLAG_IMMEDIATE`, never a zero-valued timespec. The unsafe sites go from five to two (`apply_change`, the
  wait). The module doc carries the measurement.
- `crates/rt/src/{lib,driver,registry,runtime}.rs`: the kqueue `cfg`s name macOS alone. FreeBSD was named in six of
  them and built nowhere; slates ships Windows, macOS and Linux, and `kevent64` is Apple's.

## Test

`crates/rt/tests/spin_keeps_core.rs` (`a_spinning_shard_answers_sooner_than_a_parked_one`): 2,000 round trips
between a task on each of two shards, parking, then spinning; the spinning median must be under half the parked one.
On main it failed 3 of 3 (spinning 14,459 / 10,791 / 11,208 ns against parked 17,084 / 10,333 / 10,125 ns); with
the fix it passed 3 of 3 (2,791 / 2,750 / 2,792 against 13,917 / 9,875 / 11,459 ns). The whole `slates-rt` suite
passes with the fix.

A first form of the test counted the process's involuntary switches over a spin and failed main (21,825 in 337 ms),
but this host's own preemption of a pure spinning thread, with no syscall at all, was 1,386–5,218 in 200–350 ms,
so the count could not tell the fix from the bug. It was replaced.

## Siblings found

- `crates/client/src/exit_watch.rs` reads its `EVFILT_PROC` queue with a zero timeout through rustix's `kevent`. It is
  asked on a cold path only (after a reply has stalled past the deadline), so the 15 µs is no cost there. Not
  changed; reported.
- **Exposed, not caused:** with the shard truly spinning, the bench's oversubscribed 64-client form teaches the
  shards a wake mean of 26–55 µs (main: 4.6–14 µs, its "spin" mostly asleep), and the idle window is that mean ×
  `IDLE_WINDOW_RATIO` (100), 3–5 ms of spinning on every shard. The parked form that follows in the same daemon then
  measured a p99 of 7.8 / 12.6 / 15.0 ms against main's 2.0 / 0.50 / 1.1 ms at load 12–16. Run alone on a fresh
  daemon (load 5–6), the parked form is no worse than main: p99 1.48 / 0.43 / 0.95 / 0.85 ms against
  2.33 / 0.66 / 1.22 / 0.70 ms, p50 101–113 µs against 120–129 µs. Recorded in GAPS, open.
