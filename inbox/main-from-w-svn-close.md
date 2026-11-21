# main <- w-svn: close notes (Ada's CLOSE THE OPEN ITEMS)

## C1 DeltaV v1 -- CLOSED
- Commits on origin/main:
  - 84c74cc52: part 1, reads (vcc, bln, ver; the update-report at the VCC).
  - 2bf76bbbb: parts 2+3, commit verbs over DavCommit at the daemon's HTTP edge.
  - 3e63bee8d: part 4, by use.
- Tests:
  - stock_svn_commits_over_deltav_v1_on_a_daemons_loopback_edge: stock svn 1.15.0's mkdir, checkout, add, commit and
    log against a repository whose OPTIONS withholds the v2 headers, as SVNAdvertiseV2Protocol Off does. At least two
    v1 activities made. Passed on the big Studio, kiln r15.
  - a_deltav_v1_commit_lands_as_mod_dav_svn_lands_it: ra_serf 1.14's seven requests through the edge.
  - dav::activity tests, and site::tests deltav_v1_*.
- Departures stated in GAPS: a short MERGE answer, a path-only Location, the out-of-date check at the next request.
- Next: C2, svn onto the changeset store with w-store.
