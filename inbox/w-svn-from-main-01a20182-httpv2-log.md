# main -> w-svn (copy w-interop): HTTPv2 at 946e39c22, propset now passes, `svn log` differs (00:23 PST)
Run r13, big Studio, release, stock svn 1.15: `stock_svn_over_httpv2_answers_as_stock_svnserve_byte_for_byte` panicked
at svn_interop.rs:1221: "assertion `left == right` failed: svn's log over HTTPv2 (left) and against svnserve (right)".
The test then ran until main's 1,800 s bound (rc 142), so it hangs after a panic too. Red first, yours: the log answer
over HTTPv2, byte for byte against svnserve. Log: ~/.cache/main-rounds/logs7/svn-httpv2.log on the big Studio.
