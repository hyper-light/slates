# Starts longer than the anchor's restart window were restarted at once, forever (2026-11-11)

## Description

B10's Linux gate on adacachy2 (be4985bb, load 67-73, none of 32 cores idle) ran
`crates/cli/tests/cli.rs` `a_start_whose_shards_sum_past_a_window_comes_up` for 27 minutes. Every second the anchor
killed the 407-shard start ("the starting daemon ... ran 1009-1037 ms of CPU on one thread without beating") and
restarted it at once, "1 in the window", and the test read the anchor's lines for good, since it ended only at a serving
line or a crash-loop line and neither came. Main's macOS CI met the same with a 367-shard start.

## Root cause

`Supervisor::step` (`crates/anchor/src/supervise.rs`) counted a daemon's failed starts (exits before it served) in a
window of the recovery budget, `slates_db::replay::RECOVERY_BUDGET_NS`, a second, against the starts that budget held at
the longest start measured (`RestartPolicy::derive`, at least one), and called a crash loop only past that count. A start
killed after a second of one thread's CPU had lived at least a second, so each exit fell in a window of its own: one in
the window, never past the bound of one, never backed off.

The rule's evidence was the liveness agent's red test of 2026-11-11 (`2a5570e4d`, carried here as `7e53d6444`): a child
whose every start ran 400 ms and failed, under a policy whose window was 200 ms, was restarted at once 4 times of 4 on
main 29ef4521, backed off None.

## Impact

Any start that a busy machine stretched past a second and that then failed, by the anchor's start judgment or its own
exit, was restarted at full speed for as long as it kept failing: a 407-shard start burned its CPU every second for 27
minutes on a machine already at load 70, and its clients' hard mounts waited the whole time.

## Exact edits (A-311)

- `crates/anchor/src/supervise.rs`: the window and its count are gone. A failed start is restarted at once; the second in
  a row is a crash loop, backed off at least the failed daemon's own life (spawn to exit), then twice the last backoff,
  doubled no further than the cap and never shorter than the failed start took (`crash_loop_backoff`). A daemon that
  serves resets the count. `RestartPolicy` is the cap alone, derived from the operator's failover SLO
  (`RestartPolicy::derive(failover_slo_ns)`); `RETRY_CAP_NS` (60 s, a hard Linux NFS mount's `timeo`) and
  `Supervisor::set_policy` are removed. `Step::Restarted` names the failed starts in a row (`failed_in_a_row`).
- `crates/api/src/role/anchor.rs`: the policy is derived once from the daemon configuration's `failover_slo_ns` and logged
  with its derivation; a measured start no longer re-derives it.
- `crates/cli/tests/cli.rs`: the many-shard start's watch ends at the anchor's first start kill and fails with its line,
  rather than reading on.

## Tests

- `crates/anchor/tests/anchor.rs` `a_crash_loop_of_starts_longer_than_the_window_is_backed_off`: red on main 29ef4521
  (restarted at once 4 times of 4, backed off None); passes (a crash loop at the second exit, backed off at least the
  start's own length).
- `crates/anchor/src/supervise.rs` `backoff_tests`: the backoff starts at the failed start's life, doubles to the cap,
  and is never shorter than the failed start's life.
- The anchor's own fixtures under the rule: one restart then a crash loop, served exits never counted, the doubling
  backoff on the test's clock, the device hold, the torn publication, and the device-refusal fixture (whose garbler now
  waits to be stopped, since its exit would be restarted at once).

## Siblings

- The serve-then-crash loop: a daemon that serves, then dies before it has served as long as its start took, was still
  restarted at once every time (`a_daemon_that_dies_soon_after_it_serves_is_backed_off`). Closed the same day by
  A-311's second half: such an exit is a failed start, the anchor publishes its verdict that a daemon has recovered
  (`recovered` in `slates status`), and the end-to-end tests that kill daemons in a row wait for it.
- The cause of the 407-shard kills themselves: the start's main thread made no beat until the watchdog ran, and its
  work grows with the shards. Closed the same day by A-312: a start beats at every unit of its work.
- `docs/wip/bench/kill_storm.sh` recorded A-119's numbers under the window's rule ("ten restarts, then a crash loop"); a
  storm now meets a crash loop at the second kill that lands before the daemon serves.
