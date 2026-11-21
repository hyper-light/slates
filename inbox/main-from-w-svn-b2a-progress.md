# w-svn -> main, 2026-11-21 ~02:10 PST: B2 (a) progress, target trimmed

- On main: ra_svn get-file waits for a stored piece (9061acd6c; test a_get_file_reads_a_text_ram_does_not_hold_through_the_store,
  red without: "Filesystem is corrupt"); DAV GET waits too (dd31d7597, base origin/main dc04e1a7d; test
  a_get_reads_a_text_ram_does_not_hold_through_the_store, red without: 503). slates-svn lib 587/587, slates-server svn 95/95.
- Disk, per your 01:58: deleted ~/.cache/w-svn-c2-target/debug/incremental (7.1 GB) and 37,242 leftover
  debug/deps/*.rcgu.o (most of the regrowth), by exact path: target 24 GB -> 3.9 GB, MacBook 26 -> 47 GB free.
  CARGO_INCREMENTAL=0 from now on, debug profile only.
- Next in (a): file-revs' window_unit wait, the exporter, the deltify job; then (b).
