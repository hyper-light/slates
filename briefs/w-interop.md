# w-interop: the worker for UI goal 93's part "Interop with stock git, hg and svn clients proven" (main, 2026-11-19 ~20:15 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-interop.md (create it; keep it current: piece, branch, runs, next step).
Work ONE piece at a time, in this order; each piece's handoff details are in the brief and progress file named beside it -- read them when you reach that piece. Each piece lands as one commit plus the merge, failing test first, then a 3-line SendMessage to "slates-7a", then the next piece. You are the only worker on this goal; the other five goals have their own workers (w-shared, w-git, w-hg, w-svn, w-interop, w-perf) -- coordinate with them by SendMessage only where a piece touches theirs.

## Queue
You prove interop with STOCK clients (git 2.50.1, hg 7.2, svn 1.15, OpenSSH, git-lfs on the Studio) against real
anchors/daemons. You don't build server features; failures you find by use go to main and the owning goal worker
(w-git, w-hg, w-svn) in one line with the exact reproduction. Fix only the interop tests themselves.
1. git over SSH: un-ignore and pass crates/cli/tests/git_interop.rs's whole stock_git_clones_pushes_and_fetches_over_
   the_ssh_edge (waits on nothing; enrolled key issued as the anchor's issuer; known_hosts from the daemon's printed
   fingerprint): briefs/g299.md item 1; progress/g299-http-edge.md.
2. hg clone over ssh:// beside g083's stock-hg tests in crates/cli/tests/cli.rs (its push stays ignored until w-hg
   lands A-696).
3. Watch main (`git log origin/main`) and un-ignore each stock-client test the moment its last wait lands: hg HTTP
   round trip (A-680 2b), hg SSH (A-696), svn_interop.rs's svn:// leg (A-695 + ra_svn commits + A-692), HTTP legs (DAV
   update-report + writes), svn+ssh (daemon svn+ssh). Run each with the stock client on the Studio.
4. Then widen: large real repositories per tool (git/git, an hg repo like hg's own, an svn repo), every common client
   command a user runs (log, blame, diff, branch, tag, merge, rebase), and record pass/fail per command per tool.
