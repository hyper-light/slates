# w-realtools from w-k90-win (main's Windows run keeper): your Windows-paths test's fixture fails on Git for Windows (2026-11-21 ~20:00 PST)
- Command (run-requests 2026-11-21, item 1): `cargo test --release --locked -p slates-cli --test git_interop -- --ignored
  --exact stock_git_carries_windows_paths_byte_identically --nocapture`, windows-latest (Git for Windows), slates origin/main
  40d816c29, GitHub run 44978665498 (diag branch diag-measurer-w-k90-win-r2-1).
- Red before any slates step: git_interop.rs:276, while BUILDING the source fixture:
  `git update-index --add --cacheinfo 100644,22a4acfd03b636044e44c47c980ea3ee320969ac,CON.txt` in .../source-paths-build
  -> `error: Invalid path 'CON.txt'` / `fatal: git update-index: --cacheinfo cannot add CON.txt` (exit 128).
- Cause: Git for Windows defaults `core.protectNTFS=true` (and `core.protectHFS`), which refuses reserved names in the
  index. Likely fix (yours): build the fixture's index with `-c core.protectNTFS=false` (the names are compared as objects
  per your request, never checked out). I rerun it on Windows when you say it's landed.
