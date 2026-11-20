# w-git: the worker for UI goal 93's part "The git server built" (main, 2026-11-19 ~20:15 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-git.md (create it; keep it current: piece, branch, runs, next step).
Work ONE piece at a time, in this order; each piece's handoff details are in the brief and progress file named beside it -- read them when you reach that piece. Each piece lands as one commit plus the merge, failing test first, then a 3-line SendMessage to "slates-7a", then the next piece. You are the only worker on this goal; the other five goals have their own workers (w-shared, w-git, w-hg, w-svn, w-interop, w-perf) -- coordinate with them by SendMessage only where a piece touches theirs.

## Queue
(main 2026-11-19 ~21:10: item 1 DONE -- A-704 landed c34de85e1, main f6308ccdc; w-perf runs its by-use proof. Owed from it, add as item 1b after hooks: unpack into the quarantine a slice at a time. Start at item 2. Read progress/w-git.md first.)
1. THE LARGE-PUSH BUG (found by use: stock git can't push git/git, 324 MiB, HTTP 413; 4,508 objects -> "unpack eof"):
   the fix with failing tests first, as briefs/g309.md item 2 says (slice-bounded body + unpack, derived per-repository
   limit, git's maxInputSize words). Harness state: progress/g309-interop.md. (w-perf owns the A-699 baseline record.)
2. A-665 part 4 hooks (PresentView), then client-left signal, learned hook limits, SSH hooks: briefs/g293.md;
   progress/g293-kiln-oci-engine.md.
3. Stock git-lfs end to end, then non-owner locks: briefs/g230.md; progress/g230-now-scale-out.md.
4. A-640 part 4 slices 4e, 4f, then bundle-uri, packfile-uris: briefs/g296.md; progress/g296-git-v2.md.
5. ECDSA P-256 (+ RSA if handed) SSH host keys so fleet nodes start their SSH edge: briefs/g299.md item 2.
6. The git inventory (briefs/g294.md; progress/g294-kiln-k8s-engine.md), then the missing pieces it finds, by user impact.
