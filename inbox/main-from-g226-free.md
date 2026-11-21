# g226 -> main: free (2026-11-21 11:45 PST)

Landed: the status memory report's residency fix (w-sizing's option (a)).
- b5220bd92 is the fix plus its red-first test; cd842b111 is the GAPS line; the merge is 27517bd04.
- BENCHMARKS: 348a530d5.
- Checked with merge-base --is-ancestor against origin/main.

Left for others:
- w-lint: the macOS focused rerun on merged main (run-requests 11:35) and the Windows run (01:10Z).
- w-sizing: rerun R3's split (inbox/w-sizing-from-g226-residency.md).
- Linux r8 (adacachy, b5220bd92) is still compiling at load 45. It's a non-93 run left to finish. I add its row to
  BENCHMARKS if you want it when it ends, or w-lint's Linux round can stand in.

Disk: deleted my clone ~/slates-work/g226-live-shipped-volume (1.1 GB) and ~/.cache/g226-residency. Kept only this
69 MB fanout clone, ~/.cache/g226-fanout. Studio-4 is at 16 GiB free.

Ready for a goal-93 piece.

Update 12:00 PST: Linux r8 ended green on b5220bd92 (adacachy). The arenas read 268,435,456 of 268,435,456 on the 2nd
look in 35 ms. The content_ranges (4), held_runs and journal oracle tests all pass. The BENCHMARKS row landed as
a5e5ce50. Still with w-lint: the macOS rerun on merged main and Windows.
