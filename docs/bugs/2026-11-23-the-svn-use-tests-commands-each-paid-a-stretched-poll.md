# The svn use test's every stock command paid a stretched poll, and a round cut at its bound said nothing (2026-11-23)

## Description
Main's r546 (the big Studio, c40775ea2): the use test over HTTP with git-2000.dump ran from 18:53 and was killed at its
5,400 s bound at 20:23:57 (rc 142). Its log held nothing past the daemon's start, because the test prints only at its
end. At 19:25 the test thread sat in the harness's poll (`nanosleep`, `POLL` 20 ms) between short-lived `svn`
children, and the daemon had used 32 s of CPU in 28 minutes.

## Root cause
The harness's two waits for a stock command polled every `POLL`: `exit_within` (behind `Svn::run`) and `tool_with`'s
loop. Each is a timed sleep, and on that runner launchd's timer policy stretches a 10 ms sleep to about 76 ms. So every
command paid at least one stretched sleep. The `blame` verb runs a blame of every file of a working copy at every
sampled revision on both legs: with git-2000, 4,872 files × 16 samples × 2 legs is 155,904 commands. The test also
printed nothing until it ended, so a cut round could not say where it was.

## Impact
The use test over git-2000 could not finish within its bound on that runner, and its log could not show how far it
got. Nothing in slates was wrong, and no verb was shown slow.

## Edits
- `crates/cli/tests/svn_interop.rs`:
  - `exited_within(child, bound, what)`: the one wait for a stock command. A thread owns the child and waits on its exit;
    this thread waits on that thread's word within the bound, killing the still-unreaped child past it. `Svn::run` and
    `tool_with` both use it, and `exit_within`'s poll is gone.
  - `Svn::spent` and `Svn::charged`: each session's commands' wall time, `svnrdump` and `svnsync` charged to their leg.
  - `timed_verb`: each verb prints as it starts, then its wall time, each leg's share and the differences so far.
  - The red test, `a_commands_wait_takes_no_timed_pause` (adda1f16e): `pause`s counted on this thread across a 0.1 s
    command through `tool`.

## Evidence
- Before (the MacBook): the red test failed, `(0, 3)` against `(0, 0)`: three pauses for a 0.1 s command.
- After: it and the comparison's other tests pass, 7 of 7.
- Owed: main's rerun of git-2000 over HTTP with the progress lines, to size its bound by verb.
