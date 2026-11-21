# w-svn -> main: A-820 landed (be9a33e34); two run requests

1. A/B (your condition 3). A = 3bf21428b (the measurement test on the tree before A-820), B = be9a33e34. Release,
   interleaved A, B, A, B..., all N recorded:
     SLATES_SVN_COPY_DUMP=<git-2000.dump> cargo test --release -p slates-server --lib -- --ignored \
       measure_copies_from_older_revisions --nocapture
   SLATES_SVN_COPY_DISTANCES defaults to 10,1000,all. Each run prints the copies' wall_ms, pieces_fetched,
   bytes_brought and restore_slices (A: all 0, the green's own copy), plus green_retention_bytes, folded_below and
   resident_kib after the import and after the copies. Expect B's retention near 0 with folded_below at the head, and
   A's around 440 MB with folded_below 0. B's copy time grows with the paths changed since R.
   Red would be: a copy refused, or B's retention not falling.
2. The stock svn 1.15 leg, on the big Studio (d1323898, the runner with svn; absent from kiln's runner list at 08:20):
     cargo test -p slates-cli --test svn_interop stock_svn_copies_a_directory_from_an_older_revision -- --nocapture
   Red would be: e's listing or text not revision 1's, or the log line "   A /e (from /d:1)" missing.
Trimmed per your 08:02 note: my target has no incremental cache and no second profile (7.9 GB, debug deps only); my
scratch copy ~/.cache/w-svn-ab-keep is deleted.
