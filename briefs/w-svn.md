# w-svn: the worker for UI goal 93's part "The Subversion server built" (main, 2026-11-19 ~20:15 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-svn.md (create it; keep it current: piece, branch, runs, next step).
Work ONE piece at a time, in this order; each piece's handoff details are in the brief and progress file named beside it -- read them when you reach that piece. Each piece lands as one commit plus the merge, failing test first, then a 3-line SendMessage to "slates-7a", then the next piece. You are the only worker on this goal; the other five goals have their own workers (w-shared, w-git, w-hg, w-svn, w-interop, w-perf) -- coordinate with them by SendMessage only where a piece touches theirs.

## Queue
(main 2026-11-19 ~20:55: A-695 LANDED a3705be03; A-692 moved ahead of the ra_svn commits.)
1. A-695 svn:// lookup (find_repos, owner-shard connection move, callable from the tunnel), sources through the landed
   lender vcs_store::svn_view::ServedRecords: briefs/g234.md; progress/g234-windows-wake-estimate.md.
2. ra_svn commits through repository::svn_commit::commit (svn:// and svn+ssh), the by-use exit difference, svn+ssh in
   the daemon's SSH site: briefs/g229.md; progress/g229-chaos-across-real.md.
3. A-692 CRAM-MD5 against enrolled credentials, A-693 ra_svn lock verbs: briefs/g307.md; progress/g307-rebase-links.md.
4. DAV: GET runs capped, the svn site in ToolSites + update-report (stock svn checkout/update), then HTTPv2 writes:
   briefs/g297.md; progress/g297-svn-reads.md.
5. A-702 revprop changes, then ra_svn replay/replay-range (svnsync, svnrdump): briefs/g302.md; progress/g302-derived-data.md.
6. A-652 export half, then the lock-expiry case by use: briefs/g268.md; progress/g268-slates-delivers-everything.md.
7. Then: DeltaV v1 for old clients, review piece 14.
