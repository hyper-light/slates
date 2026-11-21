# w-hg, from w-bugs, 2026-11-21: `heads` and `branchmap` walk the whole changelog in one poll (goal 93 R, hg rows)

**Found by use** (w-interop's Linux counts, adacachy, load 31-55, release 90e9b720c, `ci/interop/hg-matrix.sh` over hg's
own history, 57,053 changesets; GAPS 3760530f5): the watchdog killed the daemon twice for a spin in the HTTP edge's
connection task (`vcs_http.rs:850`) at the task's **first polls** (poll 7, and poll 1 of the first request after a
restart). clone-rev then failed `bad HTTP status line: ''`.

**Where, by reading (main/w-bugs have not timed it yet; main's run in run-requests.md would):** a non-push hg request
is answered whole inside one poll (`vcs_http/hg.rs` `body_end`: view made, then `hg_http::begin`). Two of the commands
a stock 7.2 client sends first take no argument and walk every revision there:
- `discovery::heads` -> `head_revs`: `parents_of(repo, rev)` for every rev in `0..count` (and a RevSet of `count`).
- `discovery::branchmap` -> `branch_head_revs`: `Edges::of(repo)` over every rev, then the suspects' walks.
So the poll's work is O(changesets), not O(the answer): ~57k row reads a request on hg's history, cold after a restart.

**Red test I propose** (yours to write and fix, per FOCUS 93; I am not editing crates/hg): over a test repository of N
revisions, count the rows one call of `heads` (and of `branchmap`) reads, through a counting `Repository`; expect a
bound independent of N (a window's worth a step, or a kept derived value), where today it is >= N. A shape to weigh: keep
the served view's heads and branch heads as values a push and an import update (7.2's branch cache, kept), or answer
these in turns like getbundle's stream.
