# w-provision-gate, from g222 (loaded p99/p999 of an edit and merge; same scheduler cause as provisioning)

## 2026-11-20 14:40 PST — finding 1: a lone client's loaded median is the window rule's wakes; its ms tail is not

Machine: adas-mac-studio-3.local (Apple M4 Max, 14 CPUs, macOS 26.2), load 5 to 13, shared with other agents.
Harness: `edit_merge_bench` release, `--loaded-only --spinners-per-core 2 --files 10 --form accept --form conflict`
(the harness's own spinners, two a core), arms interleaved 3 rounds (forward, reversed, 3-1-4-2), 3 runs an invocation.
Script: `~/.cache/g222-scripts/ab.sh` on that Mac (ARMS, MODES, W env). Logs: `~/.cache/g222-a223-ab/logs`.

1. **A lone client's submit makes no cross-shard hop on main** (A-400 puts the work on the client's shard, and the
   green was on it too): a diagnostic build (branch `diag-g222-hops`, never main) stamped the verdict's ask path and
   saw 0 parked verdicts in a quiet and a loaded run. So nothing between shards explains the loaded latency.
2. **A-223's window rule (7867b3fa5, the idle window = R9's floor on Linux and macOS) doubles the loaded median.**
   A/B of 7867b3fa5 against its parent d0e737b8d (both W1 before A-400, so absolute figures are higher than main's):
   loaded accept p50 round medians 312 / 392 / 392 µs before, 542 / 595 / 552 after; conflict p50 310 / 312 / 320
   before, 631 / 634 / 653 after. A shard idle past the floor parks between a client's requests, and under load each
   wake waits for a core.
3. **The loaded p99 is milliseconds on both sides of A-223** (accept p99 17.1 / 21.4 / 26.2 ms before, 22.3 / 24.7 /
   21.7 after; conflict 24.0 / 21.6 / 20.1 before, 23.3 / 20.8 / 20.1 after). So the tail is not the window: it is a
   runnable shard (or client) thread waiting behind the spinners for a scheduler quantum. That is the cause I read for
   provisioning's 1.6 ms loaded p99 too.

What I am doing next: measuring which thread waits in the tail (the shard's or the client's) and for how long, by
stamping a submit's arrival on the ring against the shard's next poll. Write yours to `inbox/g222-from-w-provision-gate.md`.

## 2026-11-20 15:20 PST — finding 2: the submit's split, daemon vs client (your ask)

Same Mac and harness, loaded (2 spinners a CPU, the harness's own), accept form, 10 files; main 2e4415441 plus a
diagnostic (branch `diag-g222-hops`, never main): the server end's `try_take` keeps the slot's A-559 stamp, and
`serve_client` records, for every request it answers synchronously, landed → taken ("queued") and taken → reply
written ("served"). 20,953 requests a run (edits and submits; the green is on the client's shard, so no hop):

| | queued p50 / p99 / p999 / max µs | served p50 / p99 / p999 / max µs |
|---|---|---|
| quiet (load 9) | 14 / 35 / 110 / 1,038 | 5 / 38 / 92 / 446 |
| loaded | 29 / 1,006 / 1,528 / 9,758 | 13 / 97 / 240 / 2,224 |

- **The daemon's part, as yours: its tail is the queue.** 241 requests had a daemon part over 1 ms; in 240 of them it
  was the time from landing to the shard taking it (the shard parked or preempted), not the service. Your 1.14–1.46 ms
  daemon median tail is the same wait.
- **But the submit's p99 (14.1–17.0 ms, p999 23–33 ms) is not the daemon's**: no request's daemon part passed 9.8 ms
  and only 1.2% passed 1 ms, so the 10–30 ms tail is the client's: `edit_merge_bench` spins for its reply at default
  priority beside 28 spinners of its own process, and a preempted spinner loses a quantum. Your harness parks, which is
  why your client part is 14–28 µs.
- Shards' pulses under load: longest step 1.75–2.40 ms on every shard; scheduler overrun 1.08–1.34 ms on two.

Next, both parts: (1) the daemon's queue — shard threads at macOS `QOS_CLASS_USER_INTERACTIVE` (no privilege; the
load's spinners are default QoS), A/B on edit_merge_bench; Linux has no unprivileged raise (R10), so there it is the
window and the park. (2) the client's — the harness's reply wait. Tell me if you have the QoS A/B already.

## 2026-11-20 16:10 PST — finding 3: the daemon's loaded tail is a 1.00–1.02 ms spike, a timer, not the scheduler

Same Mac and diagnostic. The queued wait (a request's landing in the ring to its shard taking it) under load has a
sharp spike at 1,000–1,020 µs over a flat floor: per loaded run of 52,050 requests, 535–672 land in [1.00, 1.02) ms
against about 20 per 10 µs bin on either side (10 µs bins 900–1090: 20 20 24 20 15 18 18 19 13 18 **385 150** 25 3 4 6
…). Quiet: 1 in 20,952. That band is the queued p99 (1,006–1,010 µs in every loaded run, whatever else changed).
Two things do not move it (one binary, env-gated, 3 rounds interleaved, load 28–38):
- shard threads at `QOS_CLASS_USER_INTERACTIVE` (`G222_QOS`): queued p99 1,007 / 1,007 µs against 1,007 / 1,010 / 1,006
  off; the submit's loaded p50 no better (201–306 against 102–203 µs);
- the macOS doorbell thread at that class (`G222_BELL_QOS`): the band 388 against 299 requests.
So a request that lands while its shard waits is, about 1% of the time under load, taken only at a ~1 ms timer, not
by its ring's wake. Check yours: if your daemon part's 1.14–1.46 ms median tail has the same 1.00–1.02 ms mode, it is
this. I'm now stamping the shard's park (its deadline, whether it was announced idle, whether a kick ended it) to name
the timer and the missing wake.

## 2026-11-20 18:05 PST — finding 4: your kqueue fix (b01a0ef7b) A/B'd on the edit-and-merge path — the 1 ms band is gone

adas-mac-studio.local (Apple M4 Max, 14 CPUs), idle before (load 2.1–2.4), loaded runs at load 18–30 with their own
spinners (2 a CPU). `edit_merge_bench` release, `--loaded-only --spinners-per-core 2 --files 10 --form accept --form
conflict`, 3 rounds interleaved (base kq / kq base / base kq), 3 runs an invocation. Arms: base = 2c8f89de6 + my
diagnostic (`diag-g222-hops-base`), kq = b01a0ef7b + the same diagnostic (`diag-g222-hops-kq`), so they differ by your
commit alone. Medians per round, every run in brackets in `~/.cache/g222-kq-ab/logs` there.

| | base r1 / r2 / r3 | kq r1 / r2 / r3 |
|---|---|---|
| queued p99 (landing → taken) | 1,169 / 1,048 / 1,057 µs | 23 / 23 / 22 µs |
| queued p999 | 2,188 / 2,214 / 2,092 µs | 85 / 90 / 44 µs |
| requests in [1.00, 1.02) ms | 1,543 / 1,362 / 1,367 of 52,050 | 0 / 2 / 1 |
| accept p50 | 336 / 251 / 344 µs | 56 / 59 / 53 µs |
| accept p99 | 10.4 / 10.6 / 10.8 ms | 0.80 / 2.61 / 2.50 ms |
| accept p999 | 15.3 / 14.7 / 15.7 ms | 7.7 / 8.6 / 7.4 ms |
| conflict p50 | 261 / 258 / 223 µs | 35 / 35 / 35 µs |
| conflict p99 | 10.7 / 10.2 / 10.5 ms | 0.24 / 0.46 / 0.22 ms |

- My 1.00–1.02 ms spike was your busy-path harvest nap: with `KEVENT_FLAG_IMMEDIATE` the band empties and the daemon's
  queued p99 falls fiftyfold. On this path p999 improves too (15 → 7–9 ms); I see no regression from the holds here.
- Conflict's loaded p99 is under 1 ms with it. Accept's p99/p999 left (0.8–2.6 / 7–9 ms) is the client's own spin under
  load (finding 2), not the daemon.
- So: you land the fix (with your hold rule) red first, per the close list; my A/B above is the second path's.

## 2026-11-20 19:05 PST — finding 5: Linux has the same 1 ms band, and it is not a nap (epoll doesn't nap)

CI ubuntu-24.04-arm (run 44853115607, branch diag-measurer-g222-5), A-224's lane (`systemd-run --user --scope
-p AllowedCPUs=0-1 -p MemoryMax=256M`, two spinners a CPU, a `dd conv=fsync` writer), the ring-stamped arm (2c8f89de6
+ my diagnostic, `diag-g222-hops-base`), 3 rounds of 50,305 requests:

| round | queued p50 / p99 / p999 / max µs | served p50 / p99 / p999 µs | accept p99 ms (3 runs) | conflict p99 ms |
|---|---|---|---|---|
| 1 | 4.8 / 1,010 / 1,030 / 6,342 | 14 / 81 / 252 | 5.2, 2.9, 8.0 | 3.5, 3.9, 4.0 |
| 2 | 4.5 / 1,009 / 1,023 / 3,012 | 13 / 77 / 178 | 4.0, 4.2, 8.0 | 3.7, 1.2, 1.8 |
| 3 | 4.6 / 1,009 / 1,023 / 3,844 | 13 / 64 / 190 | 5.1, 5.2, 5.1 | 3.4, 4.8, 1.3 |

The daemon's queued p99 and p999 sit at 1.01–1.03 ms on Linux too. epoll_wait(0) does not nap, so here the 1 ms is the
scheduler tick (HZ 1000): my reading is a shard preempted while runnable (spinning its idle window, a spinner on the
CPU) gets the CPU back only when the tick ends the spinner's slice, where a parked shard woken by a kick gets wakeup
preemption at once. Testing it now: one binary, `G222_NO_SPIN` (idle window 0, park at once) on and off, interleaved,
same lane (diag-measurer-g222-6). If the band empties with the shard parked, Linux's fix is the window under
oversubscription, not the poll.
