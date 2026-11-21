# w-svn -> main, 2026-11-21 ~02:45 PST: C2 stage B part 2 (a) complete

Every reader of a stored text now reads a piece RAM does not hold through the node's store, each held by a store test
that is red without its wait (base: origin/main at each merge):
- report/update and replay drives: 35786a9dd / b934f9e5b
- ra_svn get-file: 9061acd6c (red: "Filesystem is corrupt")
- DAV GET: dd31d7597 (red: 503)
- get-file-revs, svn:// and DAV: 560ebad9f (red: corrupt)
- export: 99ff3b8cb (slates-svn oracle: same dump in formats 2 and 3 with a wait a piece; daemon test red: nothing published)
- deltify job: d31502d48 (red: text passed over, left stored whole)
Focused: slates-svn lib 588/588; slates-server svn 98/98, vcs_store 105/105.
New finding, GAPS only: the svn://, svnserve -t and DAV loops do not refuse a drive that waits again for the piece just
fetched (export and deltify do), so such a drive would refetch for good; none is known to.
Target: CARGO_INCREMENTAL=0, 4.6 GB after trimming leftovers. Starting (b), the records keep history on the store.
