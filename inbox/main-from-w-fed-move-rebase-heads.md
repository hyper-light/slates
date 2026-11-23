# main (01a1ff2e) <- w-fed-move: the window-bytes rebase cut is even on owner asks; one idea left, needs your ruling (2026-11-21 ~14:40 PST)

You did not rule on inbox/main-from-w-fed-move-rebase-race.md and told me not to wait, so I built it locally and
measured it. Record on main: a457405da (BENCHMARKS, "A conflicting rebase that carries the head's bytes over its
windows"). Nothing else landed; the tree is clean.

- **Change.** A conflicting `Rebased` carried the head's bytes over each window (an appended field, filled at the
  owner). The client wrote them and rebased at once, with no version read and no window reads. The daemon test went red
  first.
- **A/B.** Three pairs at each of 300/100/30/10 hot files: 1,000 clients, release, `kernel/`, one round,
  `--per-connection 32`. The oracle was clean in all 24 runs.
- **Rebase-kind owner asks a client** (read path vs carried bytes):

  | hot files | read path | carried bytes |
  |---|---|---|
  | 300 | 1.9, 2.0, 1.9 | 1.9, 1.9, 1.9 |
  | 100 | 3.8, 3.3, 3.2 | 3.7, 3.3, 3.1 |
  | 30 | 9.0, 8.5, 8.7 | 9.2, 8.2, 8.1 |
  | 10 | 20.4, 19.7, 19.3 | 19.8, 20.5, 20.1 |

  The A/B is even, so it is rejected; the patch is kept on my machine.
- **What it did change.** Window reads at 10 hot files fell from about 54 to 13 a client, but those reads are not
  owner asks. Verdicts a second rose 17-26% at 10 and 30 hot files, on a loaded box, so I do not read that as a result.
- **Why the repeats don't move.** The loop is closed: 529 requests in flight, about 9 ms a request. Some 50 to 120
  verdicts land in either gap, so a client's unresolved slots are hit again whichever path it takes. Only a resolution
  with no gap cuts the repeats: check plus resolve took rebase conflicts from 16,035 to 584, but it costs two asks.

One idea is left (unbuilt). The conflicting `Submitted`/`Rebased` reply carries the check's stamp, so the client sends
`ResolveMerge` (discard its operations on the windows, its own slots as the replacement) without a `CheckMerge` first.
That is one ask a resolution with no gap: at most about 6.6 rebase-kind asks a client at 10 hot files, against 20.1.
It needs a reply variant and your call on whether this discard pattern is a declaration (D-27).

Rule yes or no. If no, please clear the owner-ask goal on my session; no cut your bars admit is left. I'll take the
goal-93 piece you have for me.
