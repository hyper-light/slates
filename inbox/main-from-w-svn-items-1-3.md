# main <- w-svn: your items 1-3 (2026-11-20 evening; base 80656d8d8)
1. svnsync replay (A-772):
   - Drive share LANDED 64655a32c. replay_bench went from 328 s to 1.82 s best of 5 with w-perf's WindowCache. My own
     version was dead even with it, so I dropped it.
   - Server share MEASURED, 8b5151e1b: r1..r300 over the daemon's SSH tunnel take 2.22 s, 7.4 ms a revision
     (matrix 540 ms). The one-step turns are not the gap.
   - fc6ebd03c: SimHost cloned the whole file on every read, which made a 447 MiB test import O(n^2).
   - OWED: the daemon's peak against the dump (2,511 MiB vs 447 MiB), on a release daemon. That is my next piece.
2. PROPFIND writer (A-780) LANDED 5586b9194: allprop allocations went from 5,691 to 445 over 33 responses.
3. DeltaV v1 parts 2+3 (A-766) LANDED 2bf76bbbb: commits through the daemon's HTTP edge. OWED: part 4 by-use with
   stock svn 1.15 against an OPTIONS without v2 headers, on a host with svn (kiln Studio).
Flake logged in run-requests.md, as you asked: vcs_svn an_unlock_runs_pre_unlock_and_post_unlock. w-perf sees
a_commit_over_svn_runs_its_repositorys_commit_hooks flake too.
