# The anchor killed live daemons on a busy machine (2026-11-06, §4.14 `daemon.alive`, A-228)

## Description

The anchor kills a daemon whose heartbeat word in the segment is older than its liveness budget, a fixed second
(`slates_server::daemon::LIVENESS_BUDGET_NS`). The word was beaten by `heartbeat_loop`, a task on the control shard's
executor, so it missed whenever that shard's pinned thread did not run, whatever the other shards did. Under the A/B
bar's load, two spinners a logical CPU on CI's 4-CPU Windows runners, the anchor killed daemons that were alive and
serving their mounts. A busy machine is the normal case (Ada's rule), so a starved but live daemon must not be killed.
And the opposite fault: a shard other than the control shard that wedged or spun forever never stopped the beat, so the
anchor could not see it.

## Measurement

- CI (reported in GAPS by the supervisor): diagnostics run 42161838327 killed sep1 (main `28359a75`) and comb1 with "the
  daemon's heartbeat lapsed; killing it"; that run logged 16, 13 and 11 late beats a daemon. The control shard ran 2–4
  steps in 974–1,036 ms; its longest poll was 503–576 ms, all of the gap's polls unattributed; its timer overrun was 42–92
  ms. Runs 42130583401 and 42080252187 lost a daemon the same way. diag-31 run 4 lost main's own receiver twice at two
  daemons a VM.
- `crates/server/tests/liveness.rs` on main (macOS arm64, M5 Max, 2026-11-06 21:05 local, load 54): a starved poll on the
  control shard (one poll of a window and a half, 10 ms on the CPU and 40 ms off it) gapped the beat **1.53 s**; a wedged
  shard and a spinning shard other than the control shard left the beat's largest gap at 110 ms and 125 ms: **never
  stopped**.

## Root cause

The beat was written by one pinned shard's task, so it measured that shard's scheduling, not the daemon's liveness; and
the anchor judged its age against a fixed second, so a thread kept off its core by the machine read as hung.

## Edits

- `crates/server/src/watchdog.rs` (new): the daemon's watchdog, an owned thread outside the runtime, pinned to no core,
  joined after the shards. Each beat it judges every shard by A-65's rule from the pulse's step count and parking flag and
  the shard's thread clock (`slates_rt::registry::shard_cpu`): alive while parked or stepping; with no step, wedged once a
  window passes with no CPU progress, spinning once it has run a window of CPU, starved (and alive) otherwise. It beats
  while every shard is alive and the control loop lives; it logs a stuck shard once and withholds the beat.
- `crates/server/src/daemon.rs`: `Daemon::start` starts the watchdog over its own mapping of the segment and passes the
  control loop a sender that is its life; `heartbeat_loop` holds it and no longer beats; `stop_parts` stops the
  watchdog last, so a draining daemon beats while its shards step.
- `crates/cli/src/anchor.rs`: `Lateness`, the anchor's own lateness over its last 100 timed waits that ran their pause;
  the lapse bound is the window plus its p99, logged at start and with every kill.

## Tests

- `crates/server/tests/liveness.rs`, after the fix (same host, load 54–70): the starved poll's largest gap **0.26 s**
  (red on main, 1.53 s); the wedged shard stops the beat 1.007 s after it wedged (red on main); the spinning shard stops
  it after 1,063 ms of its own CPU, 2.98 s of wall at load 56 (red on main); a half-window block's largest gap 253 ms and
  two spinners a core 249 ms (both under the window on main too).
- `crates/server/src/watchdog.rs` unit tests: the rule on every host with no thread under it (stepping, parked and
  starved alive; wedged a whole window after the last progress, not before; a 0.9-window block alive; spinning at a
  window of CPU).
- `crates/cli/src/anchor.rs`: `the_lapse_bound_is_the_window_plus_the_anchors_own_lateness`.
- `crates/cli/tests/cli.rs` `a_frozen_daemon_is_killed_at_its_heartbeat_lapse_and_restarted` (a whole-process hang):
  killed at 1,045 ms against a 1,044 ms bound (the window plus a 44 ms lateness p99 over 15 waits).

## Siblings

- `LIVENESS_BUDGET_NS` is still a fixed second, the window here and the reply-deadline unit of about 60 files: R3 debt,
  recorded in GAPS as a follow-on.
- `log_late_beat` still logs the control shard's own late turns; it no longer says anything about the beat the anchor
  reads, and it stays as that shard's diagnostic.
