# A lone watchdog judged its own freeze a wedge (2026-11-18, §4.14 `daemon.alive`, A-308)

## Description

The watchdog calls a shard wedged after a window with no CPU progress and never seen runnable, and "never seen" is
sampled at each look, a beat apart (A-228). When the watchdog's own thread is held off its CPU for a window or more, as
in a process frozen whole or paging at a few hundred megabytes free, the first look after the gap found every shard
still for the whole gap, never asked whether it waited for a CPU in it, and judged it wedged; a daemon nobody supervises
then ends itself (A-232).

## How it showed

The server `fleet` binary exited 4 on a wedge verdict that came 146 s past its window (g231, a 15-CPU Mac at load 62 to
131 with 0.2 to 1 GB free; one poll measured 160,662 ms): the whole process had gone unscheduled. Reported by the
supervisor on 2026-11-18 with a second item from the same runs, which was a test's bound and not the product:
`a_lone_daemons_spinning_shard_ends_its_process_and_the_call_ends_daemon_gone` drove its call for ten windows of wall
time from the hold, while the exit came 8.49 s after it (a window of the spinning shard's CPU took that long there) and
the call's own bound ran to 11.69 s, so it read a call still inside its bound as never ended.

## Fix (exact edits)

- `crates/server/src/watchdog.rs` (`ShardWatch::judge`): a look a window or more after the previous one counts as
  movement for the shard, so its still time starts again there; the verdict's named lateness still runs from the look
  before the gap, so A-232's `W + B + L` bound holds by its terms.
- `crates/client/tests/lone_daemon.rs` (`drive`, `call_after_exit`): once the child has exited, the loop runs on to the
  call's own bound after the exit; a daemon that never ends itself still fails at ten windows.

## Tests

`a_gap_in_the_watchdogs_own_looks_is_not_counted_as_a_wedge` (a blocked shard, a gap of a window and a half in the
looks: no verdict at the look after it, wedged ten looks later; before, wedged at the look after the gap).

## Open

A shard held in the kernel past a watched window (a page-in at that pressure) is still judged wedged. Linux reports such
a wait as `D`; XNU reports a user page fault's wait as an ordinary abortable wait (`THREAD_ABORTSAFE`), the same state as
a deadlock, so the scheduler state alone cannot tell them apart on macOS. A server suite on the same runner at load 50
to 60 still exited 4 once with the log writer in place: shard 2 wedged 1,073 ms with the watchdog's looks on time
(23 ms late at most), the control shard's beat task 58 s late around it. Owed: a rule that tells a shard held by the
machine's memory from a stuck one (Linux PSI `memory full`, macOS's memory-pressure level), with its evidence.
