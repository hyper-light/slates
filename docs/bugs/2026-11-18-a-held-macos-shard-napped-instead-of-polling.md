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

First form (3c872fa74, on main as d8874af10): `closed_loop_hold` a paired `#[cfg]` function, none on macOS. Replaced the
same day, since a value fixed per platform is not a decision derived from what the machine does (CLAUDE.md §3; Ada,
2026-11-18):

- `crates/rt/src/held_turn.rs` (new): `HeldTurn`, the idle turn a request meets, E[T²]/E[T] over the turns learned
  (a plain mean over the first `2^shift`, the wake estimate's own count, then exponentially weighted means), and
  `hold_pays(met_turn, wake)`: true while the turn is shorter than the wake, or before any turn is measured.
- `crates/rt/src/shard.rs`: every turn of every idle spin is timed and folded in (`learn_turn`, a real clock and a
  tracked wake estimate only); `hold_idle_window` keeps a hold only while `hold_pays`, and counts a decline
  (`Counters::idle_holds_declined`); `Counters::held_turn_ns` and `held_turns` report the estimate; the pulse mirrors it
  at each spin's end (`Pulse::held_turn_ns`, `ShardPulse::held_turn_ns`), and `provision_bench` prints it beside each
  shard's wake.
- `crates/server/src/config.rs`: `closed_loop_hold` is the loop's return at R9's percentile on every platform again;
  the owner decides.
- `crates/server/tests/closed_loop.rs`: one cfg-free verdict. Each owner decided by what it measured (an owner whose
  learned turn stayed at least its wake through the loop declined holds, one whose turn stayed shorter kept them); with
  none kept the owners parked for every return later than 2F, R9's tail aside; with holds kept, A-326's verdict. The
  first form's runs on the Mac Studio (debug, load 25–31): with every hold kept the loop's owners kept 261 and 164 holds
  and parked 173 and 315 times over 198 returns later than 2F; with no hold asked, 416–419 parks over the same 198; on
  ubuntu-latest every owner kept holds (CI run 44367421244).
- Design §4.3, §4.7 (derived constants, a status), A-326's last line and the amendment A-387; GAPS (A-223's entry);
  BENCHMARKS ("The closed-loop hold on macOS"). Owed: the learned rule's A/B against main, filed for main to run.

## Not changed, and why

- R9's own window on macOS (A-223's F) still naps 12–15 µs a turn on the client's own shard, kept by A-218's
  measurement of the tails under load. Whether that window should park sooner on macOS was not measured here.
- The true spin (`KEVENT_FLAG_IMMEDIATE`) would make a hold pay: p50 8–17 µs in this A/B, but at 3.5–10 cores quiet,
  and A-218 rejected it for its loaded tails. It stays rejected.
