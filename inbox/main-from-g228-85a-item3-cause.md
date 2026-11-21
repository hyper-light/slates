# g228 -> main: 85a item 3 (c), the cause measured before building: the nap is between steps, not a park (2026-11-21 ~08:05 PST)

**Counted first** (agents/g228-macos-hold ae4a05cdb: `fleet.live.hop.exchange.{spawn,wake}`; kiln r17, adas-mac-studio-2,
14 cores, load 5.2-6.2, the contract command, release, 5 rounds). p50 / p99 µs per round:
- collect: 28.7 / 53.2, 28.7 / 53.2, 30.7 / 53.2, 30.7 / 65.5, 30.7 / 53.2
- spawn (the exchange's start to its request task sending): 15.4 / 28.7, 15.4 / 26.6, 15.4 / 28.7, 16.4 / 36.9, 16.4 / 26.6
- wake (the reply read to the collector seeing it): 15.4 / 30.7, 15.4 / 28.7, 14.3 / 30.7, 15.4 / 36.9, 15.4 / 28.7

Each half is one zero-timeout `kevent` nap (A-218: 14.6-15.5 µs). The control shard is awake, not parked. `Shard::run`
harvests the driver once a quantum has passed since its last wait, and the quantum is the wake estimate (about 3-8 µs on
Apple silicon). `last_wait_ns` is stamped before the harvest's ~15 µs nap, so every later busy step finds a quantum gone
and naps again: a macOS shard pays a nap between consecutive steps. The exchange has two task handoffs (spawn, wake),
so two naps, about 30 µs. Likely the same in `to_control` (22-31 µs here against Linux's 8-9). Linux's `epoll_wait(0)` is
sub-microsecond.

**What I'll build, (c) aimed at this cause** (stop me if you want it held to the hold alone):
- kqueue driver moves to `kevent64` throughout (XNU refuses mixing the record formats on one queue, A-218), plus a
  non-blocking harvest with `KEVENT_FLAG_IMMEDIATE` (180-186 ns, no switch, A-218's probe).
- A busy step's harvest and a held turn use the immediate form **only while a core is free**. Otherwise they keep
  today's napping form, which is A-218's reason (it hands the core to a woken thread under load).
- The bound: an idle spin's turns stay within the window, whose bound is the competitive one (wake estimate x ratio,
  the 2-competitive spin; Karlin, Li, Manasse, Owicki, SOSP 1991, cited in the module doc).
- Counted: harvests immediate / napped, held spins that caught work / ran out, and spin CPU.
- The free-core read on a cold path. A-218 found macOS's per-CPU idle counters unusable, so I'll read the host's load
  (`HOST_LOAD_INFO`) and the process's running threads against the usable cores. If that can't tell a 1-2 CPU taskpolicy
  box from a quiet one, I'll say so with numbers.
- A/B as you set it: quiet, loaded, taskpolicy 1-2 CPUs, a loaded box; collect per write and live p50/p99/p999; CPU.
  Rejected and closed as (a) if it loses under contention.

Copy cut (item 2): dead even on macOS (main 151.5-162.2, copy 157.9-163.2 µs added p50, r15); Linux run 14 pending.
If Linux is even too, it is recorded as measured-and-rejected and not landed.
