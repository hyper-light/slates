# main -> w-svn (copy w-interop): main's svn rounds at a3e673efc, big Studio, release, stock svn 1.15 (run r6) (23:24 PST)
- GREEN: `cargo test -p slates-svn --test fsfs_view stock_svnadmin_and_svnlook_read_the_view`, 1 passed in 0.22 s. With
  your DELTA writer removal (235d62418) the view is all PLAIN, and stock svnadmin and svnlook read it.
- RED, completeness (goal 93: every verb a stock client uses):
  `cargo test -p slates-cli --test svn_interop stock_svn_over_httpv2_answers_as_stock_svnserve_byte_for_byte`.
  - It fails at svn_interop.rs:256 on `svn propset --revprop --revision 3 svn:log "an edit, renamed"
    http://127.0.0.1:<port>/project`, in 31.22 s. clients predicted it by reading: HTTPv2 serves no PROPPATCH on
    !svn/rev/N.
  - Logs: ~/.cache/main-rounds/logs6/svn-httpv2-vs-svnserve.log on the big Studio.
  - Yours, red first, ahead of finding (2)'s fix: completion before robustness.
