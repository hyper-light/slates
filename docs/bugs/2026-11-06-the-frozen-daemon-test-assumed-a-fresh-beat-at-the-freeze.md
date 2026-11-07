# The frozen-daemon test assumed a fresh beat at the freeze (2026-11-06, §4.14 `daemon.alive`, A-228)

## Description

`crates/cli/tests/cli.rs` `a_frozen_daemon_is_killed_at_its_heartbeat_lapse_and_restarted` freezes a daemon with
`SIGSTOP` and requires the anchor to replace it no sooner than the liveness window less one beat after the freeze (900
ms). That assumes the daemon's last beat came at most one beat interval before the freeze. Since A-228 the beat is the
watchdog's, and on a loaded machine a beat can come late (0.26 s measured on a starved shard). The last beat can then
precede the freeze by more than a beat, and the anchor, judging the beat's age as it should, kills sooner after the
freeze. The product was right, and the test's bound was its own assumption.

## Measurement

- Main CI run 42189119311 (job 127796487410, macOS, main `76e5b502`): "not killed before its heartbeat lapsed: replaced
  after 869.840709ms" at `cli.rs:607`.
- Diagnostics run `diag-clients-frozen-1` (2026-11-07 UTC, macos-latest, 5 CPUs, two spinning shell loops a CPU), ten
  interleaved rounds each: main's test failed 1 of 10 (replaced after 862.7 ms). The fixed test passed 10 of 10: replaced
  942–1,005 ms after the freeze, with the anchor judging the beat 1,002–1,004 ms old against a bound of 1,001–1,002 ms. On
  ubuntu-latest both passed 10 of 10.

## Root cause

The test bounded the kill from below by the freeze, which it can see, standing in for the last beat, which it cannot.

## Edits

- `crates/cli/tests/cli.rs`: the test keeps the anchor's stderr and reads the anchor's own kill line for the lapse ("N ms
  old, past B ms: the W ms window …"). It asserts the anchor's predicate: the beat it judged was at least its bound and
  at least the window old, and the window is the liveness budget. The upper bound stays: the new daemon answers within
  the suite's start wait past the anchor's bound (the window plus its lateness p99). No fixed slack.

## Tests

- The fixed test, ten runs on macOS arm64 (M5 Max, 18 CPUs) at the machine's own load (62–75, other sessions' VMs and
  builds): 10 of 10 passed. The anchor judged the beat 1.09–1.30 s old against bounds of 1.064–1.29 s, and the new
  daemon answered 1.70–4.88 s after the freeze (`~/.cache/slates-agents-scratch/clients/frozen-load.sh`, no added
  spinners).
- Red check (a scratch build, not committed): an anchor patched to kill at half its window, with its kill line still
  naming the true window, killed at 546 ms against a 542 ms bound. The new assertion failed on it ("the beat was 546ms
  old against a 542ms bound") at `cli.rs:637`.

## Siblings

- At load 150–233 on this Mac the daemon never started: the anchor killed every incarnation, "the daemon never beat
  inside the recovery budget" (104 times in one run), because `RECOVERY_BUDGET_NS` is a fixed second. Sent to the
  liveness agent, which owns "A busy machine never kills a recovering daemon in a loop".
