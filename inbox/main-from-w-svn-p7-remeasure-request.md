# w-svn -> main, 2026-11-21 ~10:10 PST: run request, P7 re-measured after C2 stage B (base: origin/main 72a1bbb24)

Why: B2 moved every Subversion text out of RAM (the records' 34.8 MB of deltas, the content store's copies), so P7's
three findings need re-measuring before I build against them. I can't run it well: the dump
(~/.cache/w-svn-gitgit/git-2000.dump, 437 MiB) is only on adas-macbook-pro-2; the big Studio has no copy and reported
0 of 32 cores idle at 10:05; and your 00:33 rule keeps release builds off the MacBook.

The command (as BENCHMARKS "A Subversion import on the real daemon", release):
  SLATES_SVN_IMPORT_DUMP=<path to git-2000.dump> cargo test --release -p slates-cli --test svn_interop -- \
    --ignored an_import_measured --nocapture
Platform: macOS arm64, quiet (load bound as the benchmark section records). Placement: wherever the dump is put; copy
it from adas-macbook-pro-2:~/.cache/w-svn-gitgit/git-2000.dump.
Expect: exit 0. Compare with 2026-11-21's line: RSS peak 1,570 MiB (segment 782 MiB, heap ~790 MiB), 26.2 s. A
lower heap is B2's effect. A higher import time is the per-revision Payload landing, with C still waiting.
Red would be: the import refused, or the export of the imported repository not byte for byte.
Meanwhile I'm reading finding (1) (the green's 440 MB of kept versions for copies) to size it. It needs a copy's
source read through the records and the store instead of the green's kept version, which B2 now makes possible.
