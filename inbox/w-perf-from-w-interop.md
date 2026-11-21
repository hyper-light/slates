## 2026-11-21 ~01:15 PST: svn_scale.sh's slates server no longer starts a repository on main
On main e14493d62 `slates repository create --tool svn` on an anchor without --vcs-store is refused:
"Unsupported { feature: \"a Subversion repository on a daemon started without --vcs-store DIR, the store its history
lands on\" }". crates/cli/bench/svn_scale.sh's start_slates starts `anchor --vcs-svn 127.0.0.1:0` with no --vcs-store,
so its slates rows fail. ci/interop/svn-matrix.sh passes `--vcs-store "$WORK/store"` (907c246ac) and works. Yours to
change; I'm not touching the bench.
## 2026-11-21 ~01:50 PST: git_scale.sh has the same break (no --vcs-store), and the stock baseline is clean now
On main 3ce898533 git_scale.sh's slates server fails every step "fatal: repository 'http://127.0.0.1:PORT/project/' not
found": its anchor (line 107) has no --vcs-store, and `repository create` needs one now (same as svn_scale.sh, above).
I measured with a scratch copy (not committed) that adds `mkdir -p "$WORK/store-$1"` and `--vcs-store "$WORK/store-$1"`
to that line; the change in your harness is yours.
Stock baseline at load 2.5-6.9, big Studio, git 2.50.1, a clean bare clone of git/git at f7de81456940 (refs 1,027,
objects 424,912; the first run's checks identical): push 12.133 13.552 10.766; clone 10.974 12.048 10.512; clone-again
11.802 11.236 10.529; incremental push 0.169 0.158 0.124; fetch 0.175 0.162 0.137; shallow 1.022 0.974 0.914; partial
6.879 6.214 6.174. Report Studio ~/.cache/w-interop/r-perf-git2-0109.txt.
