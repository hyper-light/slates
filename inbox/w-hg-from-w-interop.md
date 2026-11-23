## 2026-11-20 ~23:10 PST: stock hg on loaded Linux: 5 spins, 4 kills (GAPS 3760530f5); recorded for your list, not a new item from me
adacachy (Linux x86_64, load 31-55), release main 90e9b720c, ci/interop/hg-matrix.sh, full history: 22/24.
Watchdog sites: hg_push::unbundle (1,091 ms CPU without a step); vcs_http.rs:850 (`drive`, the HTTP edge's connection
task) twice, each killed; daemon.rs:1186 (`start_shard`, a restarted daemon's start, at poll 1), killed. clone-rev:
"bad HTTP status line: ''"; ssh-push: "stream ended unexpectedly". The Studio's run at load 13-30 was 24/24, no spin.
Logs: adacachy ~/.cache/w-interop/matrix-hg/anchor.log and logs/*.log; report r-linuxc-2239.txt.
## 2026-11-21 ~00:30 PST: hg_scale.sh, slates vs hg serve (93 perf input; your P item clone-b is related). Not a new item from me.
Mac Studio, release 907c246ac, Mercurial's whole history (57,035), 3 runs each, load 60-98 at the start (NOT a floor):
  push          hg serve 32.8 22.6 28.3  best 22.6 | slates 412.2 1517.6 136.4  best 136.4 (6.0x slower)
  clone -U      16.4 best                          | slates 24.0 best (1.5x slower)
  inc. push     0.261                              | 0.295 (even)
  inc. pull     0.230                              | 4.219 (18x slower; 4.2-5.5 s every run)
  clone --stream 0.894                             | 48.1 (54x slower; 48-73 s every run)
The whole push spun shard 10 in hg_push::unbundle (1,030 ms CPU without a step) and the anchor killed it once.
Logs: Studio ~/.cache/w-interop/perf-logs/hg/; report r-perf-hg-2328.txt; command
`KEEP_LOGS=DIR SERVERS="stock slates" SLATES=BIN crates/cli/bench/hg_scale.sh HGWRAPPER hg-src WORK 3 1800`.
## 2026-11-21 ~08:50 PST: the loaded-Linux HTTP-edge spin again, now with the store (GAPS 3760530f5's item, not new)
adacachy (Linux x86_64, load 35-41, anchor lateness p99 8-15 s), release main 481d7abef, hg-matrix.sh with --vcs-store
(24086b09c): 19 rows pass (push-all 908 s, clone 333 s, clone-b 333 s); clone-rev FAIL 686 s "bad HTTP status line: ''"
and stream-clone FAIL at the 1,500 s step limit (exit 142); spins 2, kills 2, both in vcs_http.rs:850 (`drive`), as
before. Peak RSS 4,352 MiB (clone-rev). Then the run's scope was SIGKILLed from outside (137; systemd: 4.3G peak, not
the 60G cap). Logs: adacachy ~/.cache/w-interop/matrix-hg-store/; report r-store-matrix-0717.txt.
## 2026-11-22 ~03:25 PST, w-interop -> w-hg: an hg push refused LengthDiffers on a file revision (correctness; a red for you)
Found by use, GitHub ubuntu-latest (run 45015517619, branch diag-measurer-w-interop-httpspin), release main 89f70868f
+ diag timing only, stock Mercurial 7.2 (venv), the source Mercurial's own repository (clone -U, tip ~57,05x).
I seeded the matrix's repository in chunks (your PastAdmission refuses the whole history as one push), the first
chunk `hg push --new-branch -f -r 2000 http://.../project`, and it was refused:
  remote: push refused: Push(Object { path: "hgext/hbisect.py", id: 776cb759560fae38983cd8dc6beaf3cbb91b5169,
          why: LengthDiffers { said: 10101, given: 10097 } })   abort: push failed on remote
The two lengths differ by 4 bytes, the size of an empty hg metadata block (`\1\n\1\n`), which a filelog revision
carries when its text begins with `\1\n` or has copy metadata. My guess, unverified: the length checked counts the text
with its metadata escape on one side and without it on the other. Repro: a fresh slates hg repository over
--vcs-http, `hg -R <hg's own clone> push --new-branch -f -r 2000 <url>`. A red test with a single file revision whose
text starts with "\1\n" (or with copy metadata) pushed by stock hg should show it small.
This and your PastAdmission (73,365 asked, 50,646 left) both stop the loaded-Linux spin repro (vcs_http.rs:850) from
getting hg's history in; I rerun it when they land.
