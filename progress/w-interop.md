# w-interop -- progress (UI goal 93: interop with stock git, hg and svn clients proven)

Role: implementer (w-interop), supervisor "slates-7a". Rules: relaunch-2026-11-19.md + briefs/common.md + briefs/w-interop.md.
Studio = kiln runner adas-mac-studio.local-2 (`kiln run -q --on adas-mac-studio.local-2 --no-sync -- "<script>"`),
CARGO_TARGET_DIR ~/.cache/w-interop-target, clone ~/.cache/w-interop/src (delete at END). Scratch ~/slates-work/w-interop-scratch.

## Piece 1: git over SSH (git_interop.rs stock_git_clones_pushes_and_fetches_over_the_ssh_edge un-ignored) -- 2026-11-20 ~04:05Z
- Took over predecessor's uncommitted draft in ~/slates-work/g299-sshinterop (branch agents/g299-ssh-interop, at main
  b63f8a05d): ssh_edge_of (port + fingerprint from the daemon line), known_hosts via ssh-keyscan checked by ssh-keygen -l,
  StrictHostKeyChecking=yes. Committed 43c86824e, pushed as agents/w-interop-git-ssh.
- Run 1 (Studio, focused git_interop + fmt): cut off by the 04:06Z shutdown (signal 15) after fmt clean.
- Run 2 (relaunched agent, 04:10Z): kiln MCP r1 on d1323898 (same script as run1.sh), running.
- Run 2 r1 green: git_interop 5 passed 0 ignored; SSH leg alone 1/1 in 7.4 s, no skip; fmt clean.
- LANDED a0175d10b on main (2026-11-20 04:20Z): test + GAPS lines 433/708. Worktree g299-sshinterop removed.

## Piece 2: hg clone over ssh:// beside g083's stock-hg tests in crates/cli/tests/cli.rs -- started 04:20Z
- Worktree ~/slates-work/w-interop-git-ssh-land (reused for piece 2 on a new branch).
- No CLI import verb exists (ImportRepository has no client), and pushes wait on A-696, so the clone is of an empty
  repository: new test stock_hg_clones_over_the_ssh_edge (clone, empty, paths default, pull "no changes found"), strict
  host key. Commit 1900f5aca on branch agents/w-interop-hg-ssh-clone (over main a0175d10b).
- Studio: persistent clone ~/.cache/w-interop/src, worktree ~/.cache/w-interop/wt (re-added per piece), target
  ~/.cache/w-interop-target. Run r3 (fmt + `--test cli stock_hg_`) running.
- LANDED 630eb8d76 (merge dd614c59f) 2026-11-20 ~04:55Z: r3 stock_hg_ 2 passed 2 ignored, fmt clean. Reported.

## Piece 3: watch and un-ignore -- started ~04:55Z
- A-680 2b HTTP hg round trip already un-ignored on main by g303 (7c71c8048). A-692, A-695 on main.
- r4: svn:// leg run --ignored on main (Studio worktree ~/.cache/w-interop/wt) to see where it stops.
- Worktree now ~/slates-work/w-interop-hg-land (branch agents/w-interop-hg-ssh-land).
- svn:// leg on main dd614c59f (r4): checkout refused `E170001: No access allowed to this repository` (lookup now
  works). Reported to main (w-svn unreachable by SendMessage). Stays ignored.

## Piece 4: widen -- started ~05:10Z
- ci/interop/git-matrix.sh (branch agents/w-interop-matrix, bae36a13b): anchor with HTTP+SSH edges, push a real repo,
  ~26 rows (clone, log, blame, diff, branch, tag, merge, rebase, fetch/prune, shallow, partial, ssh). Prints PASS/FAIL.
- r5: git/git mirror (~/.cache/w-interop/git-git.git) through the debug slates binary, STEP_LIMIT 1800. Running.
- NEXT: record results (GAPS line), report failures to main/w-git; then hg (hg's own repo) and svn matrices.
- r10 (main 9f51d75, own Studio target): full git/git push over HTTP: shard 0 spins 3559 ms, daemon killed, git hangs
  (rewind rpc post data); over SSH closed after 225 s. r11 with PUSH_REFS=v1.0.0 cut (3.4 MB): push PASS both edges;
  clone/partial/ssh clone FAIL "upload-pack: the request needs more room than the server admits"; shallow PASS;
  deepen FAIL "possible repository corruption". 4/27. All reported to main (w-git unreachable).
- Landing: script (same-heads fixed, PUSH_REFS documented) from w-interop-scratch/git-matrix.sh into worktree
  ~/slates-work/w-interop-matrix-land (branch agents/w-interop-matrix-land), one GAPS line, push.
- Rules now: local runs only for landing, own target ~/.cache/w-interop-local-target JOBS=6; svn not on this Mac
  (not installing without Ada); svn legs on the Studio.
