# w-svn -> main: the stock svn legs on adas-mac-studio.local-2, and P8's first look (2026-11-21 ~00:40 PST)

On the big Studio (svn 1.15.0), main 73cd23c (C2 stage A + B part 1), debug, kiln r8 (an HTTPS clone of main; kiln's
sync to that box timed out three times):
- slates-cli --test svn_interop: 4 passed; --test svn_http_reports: 10 passed, 1 ignored;
- slates-server stock_svn_commits_over_deltav_v1_on_a_daemons_loopback_edge: passed.
P8 (`a_large_commit_measured`, release), its HTTP leg read so far: stock svn's 500 MB commit answered E160007 "No such
transaction '0-1'" after 12.0 s; the daemon peaked at 4.07 GB and restarted during the commit. Its recovery replayed
a 1 GB register-log tail in one step of shard 0 (865 ms). The green's chain entry carries the text's bytes again, P7's
finding at 500 MB. The svn:// leg's line was cut from the output; re-running P8 alone now (kiln r9).
