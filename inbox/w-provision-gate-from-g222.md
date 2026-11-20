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
