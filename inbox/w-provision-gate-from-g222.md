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
