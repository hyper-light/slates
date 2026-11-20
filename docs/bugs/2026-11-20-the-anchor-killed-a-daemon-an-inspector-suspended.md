# The anchor killed a daemon an inspector suspended (not a bug in the anchor; the gotcha recorded)

**Found:** w-hg, r2 on the big Studio (Adas-Mac-Studio.local, 32 cores, load 3-14), 2026-11-20. The anchor killed its
daemon four times in one run, each time "the daemon's watchdog has not looked for ~2,0xx ms, its heartbeat ~2,0xx ms
old; its process made no progress for ~1,0xx ms (its 1000 ms window); killing it", the first at idle about 100 ms after
the daemon served, the second after a push (`~/.cache/w-hg-mem/work/anchor.log` there). r21 on the same box had none.

**The suspects, measured away.**
- *Stretched timers.* Under kiln's launchd job (nice 20) on that box, 40 timed waits each of 100 ms came back at most
  150 ms late: `time.sleep` p50 74.6 ms, a condition variable's timed wait p50 61.6 ms, a kevent timer 150 ms, max
  150 ms each. The kernel's largest coalescing leeway there is 100 ms (`kern.timer_coalesce_bg_ns_max`; every other
  tier's at most 75 ms). The watchdog's wait (sampled from a live daemon there) is `recv_timeout` → `park_timeout` →
  `dispatch_semaphore` timed wait → `semaphore_timedwait_trap`; stretched to 250 ms its 100 ms beat stays far inside the
  1 s window. A 2 s silence with no CPU at all is no timer's.
- *Idle.* An idle daemon's watchdog beats on; the anchor judges a process by its CPU only once the watchdog has
  stopped looking (A-232's `on_lapse`), and the withheld-beat kill needs a looking watchdog.

**Root cause.** w-hg's `clone-memory3.sh` (and `clone-memory2.sh`) ran `vmmap -summary` and `heap` on the daemon at
`idle` and after each phase. Both suspend the whole task while they read it, and this daemon maps ~220 GB of address
space (the 77 GB anchor segment and the 146 GB content object), which `vmmap` walks page by page. The run's RSS file
shows four daemons, each replaced right after a look: 16174 (idle), 16264 (push), 24325 (http-clone), 37784
(http-clone2). Reproduced on Studio-4 at 1cfca89f1: one `vmmap -summary` of an idle anchored daemon took 2.42 s, and the
anchor killed it with the same line ("has not looked for 2020 ms ... no progress for 1015 ms; killing it"); `heap` alone
took 0.00 s and did not.

**Why the anchor cannot tell.** `task_suspend` leaves `p_stat` as it was (only a stop signal sets `SSTOP`), the task's
running count at 0, and each held thread in an ordinary wait (it blocks on its suspend count); the suspend count is read
only through the task port, which the anchor does not hold. So "silent, no CPU, nothing runnable" is one reading for an
inspector's hold and for a daemon whose every thread, its watchdog's too, is wedged, and A-232 kills the second by
design (`a_frozen_daemon_is_killed_at_its_heartbeat_lapse_and_restarted`).

**Impact.** Any inspector that suspends an anchored daemon (`vmmap`, `heap`, `footprint`, a debugger) for longer than
the anchor's window gets it killed and restarted; a run that inspects after each phase loses the daemon each time.

**Ruling (main, 2026-11-20).** Keep A-232. Don't inspect an anchored daemon with a suspending tool past its window; read
the daemon's memory from the daemon. The edits: `slates status` reports the daemon's mapped memory by kind (segment,
content, arenas, regions, heap: mappings, reserved bytes, resident bytes, written bytes where the OS says), read in its
own process with nothing suspended (`slates_mem::heap`, `SharedObject::residency`, `AnchorSegment::residency_of`, the
A-744 sweep extended to the two anchor objects whole and the clients' ring regions and armed to completion by a status
request, `crates/server/src/content_ranges.rs`; `MemoryKindReport` in the protocol); the gotcha in A-232's text. Red
first: `a_status_during_a_push_reports_the_daemons_memory_by_kind_while_the_anchor_keeps_it` in
`crates/cli/tests/git_interop.rs` failed with no `memory` in the status and passes with it. w-hg samples `ps` RSS until
then, the status report after. Passed on: the daemon maps ~220 GB on a 96 GB machine, w-sizing's to examine.
