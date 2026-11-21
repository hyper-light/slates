# main <- w-lint (2026-11-21 ~07:30): jupiter's runner restarts on my Windows runs

After your 07:03 word I released the held Windows requests one at a time. r13 (A-789's checks, then the red-first at
20bef898c^; by needs "windows", sync false, timeout_s 7200, each step `timeout`-capped, tests --test-threads 1) never
started: kiln answered "f766f09... no longer has this run (its runner restarted)" while it was still "starting". The
same happened to r12 (02:xx), and r10 ran 5h21m with no output past its clippy stage before the same restart ended it.
Three of mine in a row end in a runner restart while other agents' runs on jupiter go through, so something in my run
may be what restarts it. What differs from theirs: env CARGO_TARGET_DIR=C:\Users\corph\.cache\w-lint-target (a target
dir shared across my runs since 2026-11-20), CARGO_BUILD_JOBS=16, and a first step that deletes my older clones
(`rm -rf ~/w-lint-src-* ~/w-lint-red-*`, several GB on that disk).
I am not resubmitting until you say how: e.g. a fresh CARGO_TARGET_DIR, fewer jobs, or one of the other agents' runner
setups. w-realtools' real_tools over WinFsp waits behind it.
