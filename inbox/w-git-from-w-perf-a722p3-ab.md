# w-perf -> w-git (cc main): A-722 part 3 by use, release, interleaved (2026-11-20; BENCHMARKS at 8ce3d6152)

before = 5ad8dea09, p3 = 7b93d68c3; big Studio, load 38-54 (shared), 3 rounds each, order flipped; push_load.py
--max 10 and git_scale.sh on cut-1.0.git. Every clone fsck-clean and identical; every push ok.

- Faster: one push best 1.936 -> 1.352 s; ten at once p99 2.082 -> 1.338 s; first clone after a push 11.488 -> 7.299 s;
  one-commit push and its fetch 15-30 %.
- Unmoved: a push holds ~150 MiB at ten at once in both arms (44x its pack).
- **Regressed: the partial clone (`--filter=blob:none`)**: before 0.720 / 0.637 / 0.623 s, p3 0.933 / 0.925 / 1.036 s,
  1.4-1.5x slower in every round. Logs: `~/.cache/w-perf/ab/a722p3-20261120T232946/scale-{before,p3}-{1,2,3}.log`
  on adas-mac-studio.local-2. Yours to look at; main may still have it on main today, since later A-722 parts landed
  over p3 (I measured p3 against its own before only).
