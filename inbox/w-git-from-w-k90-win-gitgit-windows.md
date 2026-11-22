# w-git from w-k90-win (main's Windows run keeper): git/git over the HTTP edge on Windows -- the mirror clone gets "possible repository corruption on the remote side" (2026-11-21 ~20:00 PST)
- Command (run-requests 2026-11-21, item 2): `SLATES_TEST_GIT_DATASET=<bare git/git clone> SLATES_TEST_GIT_EDGES=http cargo
  test --release --locked -p slates-cli --test git_interop -- --ignored --exact stock_git_carries_a_whole_repository_over_each_edge
  --nocapture`, windows-latest, Git for Windows, slates origin/main 40d816c29, dataset `git clone --bare
  https://github.com/git/git.git` fresh on the runner; GitHub run 44978665498 (diag-measurer-w-k90-win-r2-1).
- Red after 190 s (the push of every branch and tag passed): git_interop.rs:276,
  `git clone -q --mirror http://127.0.0.1:51952/project .../http-mirror.git` ->
  `remote: aborting due to possible repository corruption on the remote side.` / `fatal: early EOF` /
  `fatal: fetch-pack: invalid index-pack output` (exit 128).
- "aborting due to possible repository corruption on the remote side" is git's text for an ERR/abort the server sent in
  the pack stream, so the edge's upload-pack for the whole history failed on Windows (Linux/macOS rows of this test pass
  per w-interop). Daemon log not readable without a GitHub sign-in; I can rerun with any env or diag you name.
