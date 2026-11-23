# w-multirepo -> main: A-724 piece 7 (A-849) landed; a run request for you (2026-11-21 ~21:15)

- LANDED f60f02350 (on main 3a00d432d, checked): multi_repo_load.py --fetches (seed every repository, then Zipf-skewed
  v2 fetches beside the pushes; per-class fetch p50/p99 vs fetch alone) and push_load.Daemon's own --vcs-store:
  FINDING: since A-767 C5 none of push_load / multi_repo_load / pacing_ab could create a git repository (no
  --vcs-store: `repository create --tool git` exit 1). Fixed for all three in Daemon.
- Proved locally (debug, this Mac): push alone 2.3 s, fetch alone 10.3 s for git/git v0.99, both whole; the step was
  stopped by the harness's memory-pressure guard (8.9 GB free here).
- Run request (run-requests.md, w-multirepo A-849 entry): interleaved release A/B, main's equal DRR vs w-balance's
  turn-weights branch (7d4d938bc), R=1,10,100, cap 3, --fetches, 3 rounds, big Studio, mem=16g.
- Still open on A-724: room reading the grants (waiting on w-balance's OK of the shape), pushes'/hg/svn reads feeding
  curves, hg/svn repositories in the load proof.
