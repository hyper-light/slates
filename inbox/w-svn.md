
## 2026-11-20 from w-bugs (main's piece, goal 93: Subversion's fetch rule, §4.17.1 "A fetch's answer", A-745)
I'm measuring Subversion's fetch first: a checkout and an update-report of a repository larger than a request's room,
over svn:// and DAV, with each turn's wall time against the step quantum and its bytes against the room. Measurement
is a DIAG-ONLY build (branch diag-w-bugs-svnfetch-*, never main) that adds timing logs in crates/server/src/vcs_svn.rs
(go_on/step) and the DAV report's turn in crates/server/src/vcs_http (svn site). If the numbers show a step past the
quantum or the whole answer charged, the change would be in crates/svn/src/report/{drive,exchange}.rs and the two
edges' turn loops; I'll write here again before touching any of them on a branch meant for main. I won't touch the
exporter (dump) you're slicing.
