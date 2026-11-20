# The liveness beat tests timed the watchdog by their own starved reading

**Found:** g227 on Studio-4 (clean main eae99ac9, throttled sweep), 2026-11-20: `a_wedged_shard_stops_the_beat`,
`a_shard_blocked_in_a_syscall_stops_the_beat` and `a_shard_waiting_on_a_blocked_thread_stops_the_beat` (each with its
`_role`) failing in `crates/server/tests/liveness.rs`. Reported to w-reaper with A-740 as the first suspect.

**Reproduced** on Studio-2 (14 cores) under 28 `yes` spinners with the test under `taskpolicy -b` (load 41): the same
binary passed quiet and failed loaded, "the beat stopped 1.303759416s after the shard wedged, past its 1s window and
three beats". At the commit before A-740 (1d3d8de0) and at A-740 (4acb1f28), four rounds of the three tests each: 3 of
12 failed before A-740, 2 of 12 at it. A-740 is not the cause.

**Root cause.** The tests held the beat to stopping within a window and three beats of the block, measured by the test
thread's own sampling of the beat word. Under load that thread is scheduled late too, so its reading of when the beat
last advanced ran hundreds of milliseconds late, and a fixed bound ignored the watchdog's own look lateness, which
the design says the stop includes (A-232 step 5: the last beat comes within the window, a beat and the lateness of the
looks the verdict rests on; the watchdog's verdict lines measured it at 420-630 ms in the failing runs). The third test
watched three windows from the observation's sending, not from when the shard began to wait, so a late observation
ended its watch before the verdict.

**Fix.** The watchdog records each withheld episode (`watchdog::Turns::episode`: the last beat before it, on its own
clock, and the lateness of its looks up to the first withheld one; `Daemon::watchdog_episode`). The tests take the
block's start on that same clock (`slates_machine::clock::monotonic_ns`, the watchdog's `HostClock`) and hold the stop
to the window, a beat and that measured lateness; the waiting test watches from the wait's start, within a patience.
The sibling `a_shard_waiting_on_memory_keeps_the_beat_while_the_host_stalls_and_stops_it_a_window_after` had the same
fixed bound on its own sampling and takes the same rule.

**Impact.** Test-only: the watchdog's verdicts were right throughout; no daemon was killed late or early by this.
