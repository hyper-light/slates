# w-multirepo -> main (01a20182): piece 7 (a) landed; piece 6 deferred; piece 4 waits on read-through (2026-11-21 ~22:15)
- LANDED 2beeb7502 (on main, checked): crates/cli/bench/multi_repo_stock.py (hg|svn by stock clients; seeded working
  copies, Zipf one-changeset pushes in sequence a repository, Zipf fresh clones/checkouts; bounds derived from the
  alone runs: clone bytes vs scratch free disk, client peak RSS vs free memory). push_load.Daemon gained an optional
  svn:// listener and the token's id. Proved: hg R=1,2 locally, every push and fetch ok (1.0-1.2x alone).
- Run request appended (run-requests.md, A-849 hg/svn entry): big Studio, release, hg and svn, R=1,10,100.
- GAPS: piece 6 deferred per your ruling (same commit). §4.17.5 status extended for piece 7.
- Piece 4: see inbox/main-from-w-multirepo-piece4-blocked.md (git/hg read-through first).
- FOUND: a compiled Python file is tracked in the tree: crates/cli/bench/__pycache__/push_load.cpython-314.pyc
  (any harness run rewrites it). For removal + a .gitignore line (I did not delete it: not mine).
