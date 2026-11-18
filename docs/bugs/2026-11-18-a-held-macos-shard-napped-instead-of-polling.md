# A held macOS shard napped instead of polling, so A-326's hold slowed a single client at many shards

**Date:** 2026-11-18. **Area:** `crates/server/src/config.rs` (the closed-loop hold), design §4.7, A-326; A-387.
**Found by:** the measurer's quiet-gated `provision_bench` A/B on the Mac Studio d1323898 (Apple M3 Ultra, 32 CPUs,
its derived 23 shards; each invocation started once `top` read at least 85% idle), `3d7a36ab8` against `0db7ea28a`,
which landed A-223's window rule and A-326 together (logs `~/.cache/measurer-logs/g234q-*.log` there).

## Description

A spinning single client's creates got slower on macOS at many shards:

```
3d7a36ab8 (before), 23 shards: provision.spinning_1_p50 48708 / 86875 / 48541 ns, p99 200250 / 251291 / 226667 ns
0db7ea28a (after),  23 shards: provision.spinning_1_p50 97625 / 105792 / 101958 ns, p99 262084 / 298125 / 296208 ns
```

and the process's CPU went from 2.4–3.8 to 2.0–5.9 cores. The R9 gate (`ac-2.1: 1 client spinning`, 50 µs) failed in
both arms; the regression is the difference between them.

## Root cause

A-326 holds the shards a closed loop goes round open for the loop's return, on the premise that a held shard polls and
sees the next request as it lands. A macOS shard's idle spin does not poll: each turn harvests the driver with a `kevent`
whose timeout is zero, and on macOS that call sleeps 12–15 µs with an involuntary switch (§4.3 "Loop"; A-218 measured
14.6–15.5 µs on macOS 26.4.1, where `KEVENT_FLAG_IMMEDIATE` takes 180 ns). Only a parked shard is kicked, so a held one
reads its rings only when a nap ends. A hold therefore kept every shard the loop went round napping and switching.

Measured on a quiet M4 Max (Adas-Mac-Studio-4, 14 CPUs, 10 performance; its derived nine shards; release
`provision_bench`, four arms built on main `cef0f520e`, three rounds interleaved, nine runs a cell; BENCHMARKS "The
closed-loop hold on macOS" holds every number, the noise floor and the load):

| nine shards, quiet | main (held) | hold none |
|---|---|---|
| spinning_1 p50 | 51.4–60.0 µs | 41.3–43.3 µs |
| spinning_1 p99, median | 92.0 µs | 92.3 µs |
| CPU, client included | 2,859–3,046 thousandths of a core | 1,645–1,696 |
| involuntary switches per 1,000 creates | 63,395–63,678 | 16,561–16,762 |
| owner parks per 1,000 creates, per shard | 1–4 | 152–237 |

Beside two spinners a logical CPU, the nine-shard p99 was 1.87 ms held against 1.34 ms unheld at the median. An unheld
owner parks between the loop's returns and wakes on a kick in 3.1–6.0 µs. Main with A-223's window rule undone and the
hold kept read as main, so the window rule is not the cause. On the 32-CPU Studio at its 23 shards (the same binaries,
three rounds interleaved on a shared box), every unheld run beat every held one: p50 44.7–64.5 against 94.4–115.2 µs,
p99 147.6–249.3 against 256.0–376.0 µs, at 1.9–2.2 against 3.4–5.8 cores.

## Impact

Every macOS daemon serving a closed-loop client over several shards: a slower median create and about twice the CPU,
growing with the shards held (none in the p50 at three shards, 15 µs at nine, 57 µs at 23). Linux and Windows are
not affected: epoll and a completion port harvest without sleeping, so their held shards do poll.

## Exact edits

- `crates/server/src/config.rs`: the loop's return at R9's percentile moves to `loop_return_hold` (cfg-free, the unit
  test `a_closed_loops_hold_covers_its_return_to_a_shard_at_r9s_percentile` now tests it); `closed_loop_hold` becomes a
  paired `#[cfg]` function, the return on Linux and Windows and none on macOS (`derived!(0, …)`, anchored on A-218 and
  A-387). The daemon's `closed_loop_hold_ns` is that value, so on macOS `verbs::loop_hold` returns 0 and a forward asks
  no hold.
- `crates/server/tests/closed_loop.rs`: one scenario (`run_scenario`), its client spinning for `loop_return_hold` on
  every platform so its loop is R9's closed loop whatever the daemon holds; the Linux and Windows verdict as before; on
  macOS `on_macos_a_closed_loop_holds_no_shard_and_its_owners_park_between_its_returns`. On the Mac Studio (debug, load
  25–31) it failed 2 of 2 with macOS's hold put back to the loop's return (the loop's forwards asked 261 and 164 holds;
  branch `diag-g234-a387-red`) and passed 3 of 3 as built (no hold; the owners parked 416–419 times over the loop's 198
  returns later than 2F). The Linux and Windows verdict passed 3 of 3 on ubuntu-latest (CI run 44367421244).
- Design §4.3, §4.7 (derived constants, a status), A-326's last line and the amendment A-387; GAPS (A-223's entry);
  BENCHMARKS ("The closed-loop hold on macOS").

## Not changed, and why

- R9's own window on macOS (A-223's F) still naps 12–15 µs a turn on the client's own shard, kept by A-218's
  measurement of the tails under load. Whether that window should park sooner on macOS was not measured here.
- The true spin (`KEVENT_FLAG_IMMEDIATE`) would make a hold pay: p50 8–17 µs in this A/B, but at 3.5–10 cores quiet,
  and A-218 rejected it for its loaded tails. It stays rejected.
