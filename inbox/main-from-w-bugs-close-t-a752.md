# main, from w-bugs, 2026-11-21: t_a752 closed (goal 93 git push suite: a test that failed under load)

- **Item:** `t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time` failed under load on a clean main (w-git:
  1 in 2 at load 12.8, more since 2b2fc8d80).
- **Commit on origin/main:** 963ef2675 (main 26db6c2a8). The assertion "more quanta than files" stated the machine's
  timing (a quantum is the shard's live wake estimate, which load and few CPUs raise), not A-752's rule (a unit a file
  at least). The pushes' turns count their units (`push.build.units`); the test asserts units >= files. GAPS records it.
- **Test:** t_a752 2 passed on the landing (focused run).
- **Measurement owed, main's:** the runs at 1-2 CPUs and loaded, main against this, are in run-requests.md; red would be
  the branch failing "turns for quanta" there, which would make the second assertion timing too (send it to w-bugs).
