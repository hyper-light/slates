# w-svn → main 01a20182: the revprop flake was the hook runner; a ruling is needed for its sibling (2026-11-22 ~03:00 PST)

## What I found

Reproduced in the parallel daemon_edge set: 3 of 16 runs at load 40, with hg skipped too (2 of 8), so hg wasn't the
holder. Once the DAV refusal answered its chain's best message, the words were:

> Revprop change blocked by pre-revprop-change hook (exit code 0) with no output.

The runner's deadline timer fired before the hook's exit was seen, so it sent a kill. The hook had already exited 0, so
the kill found nothing. The run was then timed out with the hook's own exit code 0, which reads as a refusal. Nothing
the tests share was at fault: the bug is in the runner.

## Fixed

8db529eec, merged as 059578d78.
- A reaped status that is an exit code, not the kill's signal, is the hook's own exit, so the run is judged by it. The
  cut stays taught, so the next bound still backs off.
- Red first: `a_hook_that_exited_on_its_own_before_its_kill_is_judged_by_its_own_exit` (TimedOut Code(0), now
  Accepted).
- Also: the DAV site's hook refusal now answers its chain's best message instead of an empty one.
- Tests: hook tests 56/56. In 12 parallel daemon_edge runs the revprop test never failed.

## Still seen, needs your ruling

2 of 12 runs: `a_dav_posts_start_commit_hook...` and `a_dav_lock_runs_pre_lock...`, each "the answer ended inside a
line (WouldBlock)" at the client's 10 s read.

- Both are a refusing hook's first run, and a first run has no learned bound.
- No hook process was alive past 3 s (a ps watcher), so the hook itself exits. What's left waiting is the words
  stream's EOF.
- The likely holder: a process spawned at the same instant elsewhere in the process inherited that stream's write
  end. macOS has no `pipe2`, so std's pipe is marked close-on-exec a call after it is made.
- This can bite production too: two shards spawning hooks at once.

Options:
- **(a)** Spawn hooks with no inherited descriptors but their three: `posix_spawn` with `POSIX_SPAWN_CLOEXEC_DEFAULT` on
  macOS, `close_range` in `pre_exec` on Linux. This stops our own hooks from holding each other's pipes. It's unsafe
  code (a budget edit), and std's `Command` can't do it.
- **(b)** After the hook has exited, end its drain once its process group is empty. A foreign holder is never the
  hook's, and the hook's own children stay in its group, so svnserve's wait-for-EOF behaviour holds for them.
- **(c)** Both.

My lean is (b): no unsafe, and it covers holders we didn't spawn, the tests' helpers included. Tell me which, and whose
it is. The hook driver is A-622's.
