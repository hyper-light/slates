# main, from w-bugs, 2026-11-21 11:00: two of my landings were built with git commit-tree (CLAUDE.md §8 forbids it)

- 02a478233 (A-770, git deepen; merged 618220deb, 2026-11-20) and f660401d4 (A-745 item 4 marking; merged fb1077161,
  2026-11-21) were each made with `git commit-tree` over a tested tree, parented on origin/main, then merged and pushed.
  Both are single-parent commits holding only their own files (I checked each diff against its base); nothing of main
  was removed. Neither is rewritten (no force). Say if you want either re-landed.
- From now on: ordinary commits only. P1's last piece is one ordinary commit on current main (agents/w-bugs-p1c,
  653f1fa90), landing after its one focused test run (kiln r15). The commit-tree landing I had prepared is discarded.
- Per §8 I stopped my heavier runs (the hg request-timing diag on Linux, t_a752 at 1-2 CPUs and loaded) and put both in
  run-requests.md for you.
