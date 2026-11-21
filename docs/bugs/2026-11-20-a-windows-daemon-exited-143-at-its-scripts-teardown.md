# A Windows daemon "exited (Some(143))" at its script's teardown (not slates; the cause now said)

**Found:** w-interop's git matrix on jupiter (debug, base 9d4c5e5), 2026-11-20: each run's anchor log ended "slates
anchor: daemon exited (Some(143)); restarted as pid ..." with no kill or spin line before it, after "daemon pid ...
recovered" and a near-miss beat line (`C:\Users\corph\.cache\w-interop\matrix-git\anchor.log`).

**Root cause.** Not the anchor's kill path and not the daemon: nothing in slates exits 143 (the daemon's failure exit is
4, `FAILED_EXIT_CODE`, and an anchor kill is logged first). The 143 came at 12:42:43.98, about 1.4 s after the
matrix's last row (`ssh-fetch`, 12:42:42.57), when the script's EXIT trap ran `kill $ANCHOR` from Git Bash. The MSYS2
runtime ends a native Windows process's tree by `TerminateProcess` with 128 + the signal's number, children first: the
daemon went first with 143, the anchor, still alive, logged the restart, and then went itself, so nothing followed.
Reproduced on jupiter with the run's own binary (`w-interop-target/debug/slates.exe`): an anchor started from Git Bash
with `&` and ended with `kill $!` logged exactly "daemon exited (Some(143)); restarted as pid 472988 (0 failed starts in
a row)" as its last line, and no slates process was left. The near-miss beat line (501 ms after a turn, against a
1000 ms window) was the control shard's beat task, judged a near miss and nothing more. The 11 failed git rows were the
matrix script's (a default branch the push never fills, w-git), not this.

**Impact.** A misleading line: a restart that reads like a liveness failure at every Git Bash teardown.

**Edits.** `crates/api/src/role/anchor.rs`: `exit_cause` names the exit in the anchor's restart and crash-loop lines:
the anchor's own kill, a clean exit, the watchdog's failure exit, 143 as a termination from outside (Git Bash's `kill`
on Windows named), 137 as a kill from outside, no code as a signal's, and any other code as the daemon's own. Red first:
`a_daemons_exit_is_named_in_words_143_as_a_termination_from_outside` failed on the bare code and passes.
