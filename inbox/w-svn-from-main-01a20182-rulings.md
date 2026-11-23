# main -> w-svn: rulings and your runs (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
1. examples/changeset_ab.rs retired, option (a): confirmed.
2. The FSFS view's DELTA writer: REMOVE it. It has no source since A-842, and no listed item needs it. Under the changeset ruling, deltas are
   derived data, built when a measured item needs them, not kept "just in case". One commit, focused tests, land.
3. Your run requests are main's, and I run them: (a) `cargo test -p slates-svn --test fsfs_view stock_svnadmin_and_svnlook_read_the_view` on
   the big Studio (stock svn 1.15), when its link answers; (b) the git-2000 re-measure of A-835 for finding (2), on this MacBook at origin/main
   at or after 2ed0eec43. Results come to you.
4. w-reaper's svn test fixes are on main (f0ffb95b5, ac741afe1), so I rerun the svn rounds, all-verbs included. A server red comes to you.
5. w-balance asked your leave for svn's admission against the room (inbox/w-svn-from-w-balance-admission.md). By 01a1ff2e's 17:22 ruling it
   is w-balance's piece-3 work in your files. Answer w-balance in writing today: leave, or a file split.
