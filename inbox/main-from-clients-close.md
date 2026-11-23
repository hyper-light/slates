# main <- clients: A-830 piece written and landed (2026-11-21)

- Landed on origin/main 2e56759d2 (commits 0de166f06 svn, 77a01bcc6 git), checked with merge-base --is-ancestor.
- git RSS red test: the_daemons_resident_memory_after_a_push_and_a_clone_follows_the_pack_not_the_history
  (crates/cli/tests/git_interop.rs). Bound: warm-up end + 2N + W (W the warm-up round's resident range), peak (sampled)
  and settled after push and clone. Made history (2,000 files, 200 one-line commits, N 258,770 B) PASSES on current main
  (this Mac, debug: peak 103.6 MiB vs bound 112.6 after the merge; 68.2 vs 90.5 before): r2's 7,421 MiB (base
  26c98c76d) is not reproduced at that size. A 400-commit single-file history (100 MiB expanded) also stayed flat.
  The real cut runs by SLATES_TEST_GIT_SOURCE/CUT: run-requests.md has the command. If it is red, send it back to me
  for the attribution (slates status --memory, not vmmap: suspending an anchored daemon gets it killed) and the fix.
- svn HTTPv2 differential test: stock_svn_over_httpv2_answers_as_stock_svnserve_byte_for_byte
  (crates/cli/tests/svn_interop.rs); skips here (no svn). Run request filed; the revprop PROPPATCH red goes to w-svn.
- Found by use, one line, not chased: in debug, cloning a 400-commit history of one 256 KiB file ran past 120 s.
- A-830 row in the ledger: left open until the real cut's run says red or green.
- Worktree ~/slates-work/clients-svn-httpv2 kept until the run answers; my target ~/.cache/clients-local-target.noindex
  (Mac) and ~/.cache/clients-target.noindex (big Studio, partial build, for main to delete if wanted).
