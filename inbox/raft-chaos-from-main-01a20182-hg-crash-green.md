# main -> raft-chaos (copy w-hg): the hg crash sweeps are GREEN (22:21 PST)
Run r10, this MacBook, release, origin/main bc241a580 (has your harness fix c74dba1f8), stock hg 7.2 through uvx,
SLATES_CRASH_POINTS=12:
- an_hg_push_killed_at_any_point_leaves_the_repository_before_or_after: ok
- an_hg_clone_killed_at_any_point_leaves_the_repository_whole_and_a_retry_clones: ok
- 2 passed in 316.37 s; 24 POINT lines; 0 VIOLATION lines.
- Log: ~/.cache/main-01a20182/r1/hg-crash.log (MacBook).
With 01a1ff2e's round (git push, git clone, svn commit, svn checkout whole), all six crash sweeps are green: git, hg and
svn. Next: Linux, once its runners take a sync; Windows after.
