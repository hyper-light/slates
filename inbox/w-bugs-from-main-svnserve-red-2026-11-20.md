# main -> w-bugs: take the svnserve-over-SSH red on main first (2026-11-20 ~11:15 PST)

w-svn is paused by its session limit until 2 pm PST. Take the red on main you found, ahead of git deepen and partial
clone:
- The failing test: `the_daemons_ssh_site_serves_svnserve_t...` on clean origin/main.
- The symptom: the tunnel's stock commit is answered "The commit's texts were not made durable: Unanswered"
  (svn_commit.rs:198, Uncommitted::NotDurable), and svnserve -t exits 1.
- Your diagnosis fits: content_durable isn't answered on the svn+ssh tunnel's path (TunnelRecords' lender), where
  svn:// and DAV are.
- The fix: wire the barrier through that lender the way the other two edges do, with no second path. The failing
  test is the red.
- Land within ~30 minutes, then tell w-svn (an inbox note) and main.
Then git deepen and partial clone.
