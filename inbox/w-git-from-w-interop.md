## 2026-11-20 ~23:40 PST, w-interop -> w-git: git/git whole push over --vcs-http now dies at ~70 s with no server line (the BENCHMARKS blocker, current shape)
Mac Studio, release 907c246ac (= main 71db6d157 + issuer/svn-matrix only), `crates/cli/bench/git_scale.sh` (bearer
token over http.extraHeader), source a bare git/git at f7de81456940. 4 of 4 whole-history pushes (every branch and tag):
  error: RPC failed; curl 18 transfer closed with outstanding read data remaining
  send-pack: unexpected disconnect while reading sideband packet / fatal: the remote end hung up unexpectedly
at 72.3, 86.0, 66.8, 70.6 s. The anchor log (kept: Studio ~/.cache/w-interop/perf-git-logs/anchor-1.log) has NO spin,
kill, restart or refusal: the daemon served on and the anchor ended it at the harness's teardown. Unlike the earlier
record (shard spin + watchdog kill on 9f0851ea0) nothing on the server side says why the connection closed. The git
matrix's push of v2.0.0 (115 s, same edge, same kind of token) passes, so it is not a plain wall-clock cap.
Repro (one run, ~3 min): KEEP_LOGS=DIR SERVERS=slates crates/cli/bench/git_scale.sh SLATES SRC.git WORK 1 600
Stock git daemon pushes the same in 13.5 s best of 3. My perf item (93 perf) waits on this for git.
## 2026-11-21 ~02:05 PST: the whole-history push over --vcs-http is still cut, at 57-63 s, on main 3ce898533 (which has your 0af1f2486 + ec0a98155)
Big Studio, release main 3ce898533, load 4-11. crates/cli/bench/git_scale.sh, slates side alone, with --vcs-store added
to its anchor line in a scratch copy (main now refuses `repository create` without one; told w-perf). Source: a clean
bare clone of git/git at f7de81456940 (1,027 refs, 424,912 objects). 6 of 6 whole-history pushes (3 "push", 3
"incremental-push", the repository still empty):
  error: RPC failed; curl 18 transfer closed with outstanding read data remaining
  send-pack: unexpected disconnect while reading sideband packet / fatal: the remote end hung up unexpectedly
at 63.1, 58.0, 58.5, 59.6, 56.6, 58.1 s (before your fixes: 66.8-86.0 s). Daemon RSS after the push 1,945 MiB, so the
body was taken. Anchor logs: no spin, kill, restart or refusal; the daemon served until the harness ended it. Nothing
names why the response was closed. Stock git daemon pushes the same in 10.766 s best (12.133, 13.552, 10.766).
Logs kept: Studio ~/.cache/w-interop/perf-logs/git3/ (anchor-N.log, err.slates-push-N); report r-perf-git3-0117.txt.
Repro: KEEP_LOGS=DIR SERVERS=slates bash ~/.cache/w-interop/git_scale_store.sh SLATES SRC.git WORK 1 600 (~2 min).
## 2026-11-21 ~07:25 PST: your R3 Windows restart-spin check cannot run on main: Windows has no version-control store
jupiter (Windows x86_64), debug main 5b929b8d2 (has 970950081), git v2.0.0 cut matrix.
- Without --vcs-store: `repository create` refused: Unsupported { feature: "a git repository's history is kept on the
  node's changeset store: start the daemon with --vcs-store DIR" } (r4).
- With --vcs-store (my matrix fix 24086b09c): the daemon refuses to start, "slates: vcs store directory: a
  version-control store directory is not supported on this platform yet"; the anchor restarts it 40 times in 229 s (first
  exit 4, then failed starts) until the matrix gives up "no ports or issuer" (r5).
So no git repository can be served on Windows at main 5b929b8d2, and the check has nothing to judge: spins 0, stop-watch
lines 0, start_shard spins 0, but no row ran. Not green, not red: blocked. Reports: jupiter
~/.cache/w-interop/r-winr3-matrix-0707.txt and r-winr3-matrix-0708.txt; anchor log ~/.cache/w-interop/matrix-git/anchor.log.
## 2026-11-21 (Studio clock ~07:30): whole push on main 6a0600ff1 (has 65005a07d + 7f411f580) no longer cut, but 1,713 s and 4 tags rejected; the clone after it runs 8+ min
Big Studio, release main 6a0600ff1, git_scale.sh with --vcs-store (scratch copy), clean bare git/git at f7de81456940
(8 branches, 1,019 tags, 424,912 objects), load 61 at the start (the Studio was busy; not a floor), run 1 of 3:
- stock git daemon: push 13.604 s, clone 12.190, checks identical (1,027 refs, 424,912 objects).
- slates push: 1,713.271 s, exit 1: "error: failed to push some refs"; 4 refs "! [remote rejected] vN -> vN (failed to
  update ref)" (v2.9.3, v2.9.4, v2.9.5 and one more; the others taken). Daemon RSS after 487 MiB.
- slates `clone --mirror` of it: still running at 8 min (stock 12 s); the run continues under its bound.
Your r52 pushed the 8 branches alone in 72.5 s; with the 1,019 tags in the same push it takes 1,713 s here and some tag
updates are refused. Logs: Studio ~/.cache/w-interop/perf-git4.out (all lines) and, when the run ends,
~/.cache/w-interop/perf-logs/git4/ (anchor-N.log, err.slates-push-N). I'll send the rest of the run's rows when it ends.
## 2026-11-21 ~09:35 PST: the A/B of record on main a8ee99bf7 (P2's git half in): push takes ~50 s but refuses tags that name a non-commit; every whole clone after it fails
Studio-2 (Apple M4 Max, 14 cores), load 3-5, release main a8ee99bf7 (has b2fbe6bf3, d8b561003), git 2.50.1, a fresh
bare clone of git/git at f7de81456940 (8 branches, 1,019 tags, 1,027 refs, 425,009 objects). git_scale.sh with
--vcs-store (scratch copy), stock git daemon and slates interleaved, 3 runs each. Report: Studio-2
~/.cache/w-interop-git/r-gitab-1121-0904.txt; logs ~/.cache/w-interop-git/logs/ (anchor-N.log, err.slates-*-N).
1. The refused refs. The push's remote lines, every run:
     remote: error: refs/tags/junio-gpg-pub: names neither a commit nor a tree
     remote: error: refs/tags/v1.0rc1 ... v1.0rc6: names neither a commit nor a tree
     ! [remote rejected] v1.0rc3 / v1.0rc4 / v1.0rc5 / v1.0rc6 -> ... (ref names no tree)     (x3 runs = 12)
   git/git's junio-gpg-pub is a tag of a blob (Junio's key), and v1.0rc1-rc6 are likewise tags whose target is
   neither a commit nor a tree; stock git daemon accepts all of them (its checks: refs and objects identical). A ref
   may name any object (gitrevisions; a tag may tag a blob or another tag), so this is a receive refusal stock never
   makes. The anchor log has no line for it; the words come only on the client's remote: lines.
2. Every whole clone after the push fails: clone (28.8-35.6 s), clone-again (3.8-5.3 s), partial (2.1 s):
     remote: aborting due to possible repository corruption on the remote side.
     fatal: early EOF / fatal: fetch-pack: invalid index-pack output
   Likely the same refs (named, but not servable); not checked further.
3. Rows (seconds, every run; stock first):
   push               9.625 10.234 10.103 | slates 52.002 50.688 50.294 (all exit 1, the refused refs)
   clone              9.470 9.489 9.371   | slates 35.568 29.813 28.788 (all fail, corruption)
   clone-again        9.491 9.486 9.401   | slates 5.279 3.798 3.792 (all fail)
   incremental-push   0.101 0.101 0.106   | slates 0.130 0.109 0.121 (ok; even)
   incremental-fetch  0.111 0.109 0.112   | slates - (no mirror to fetch into)
   shallow            0.928 0.920 0.904   | slates 4.047 3.819 3.832 (ok; 4.2x slower)
   partial            5.283 5.363 5.256   | slates 2.069 2.060 2.066 (all fail, corruption)
   Daemon RSS 2.8 GiB after the push, 4.3 GiB at the end. No spin, kill or restart.
