## CLOSED: 93 git / 93 interop, R: r110's rerun (2026-11-20 ~21:25 PST)
Run r5, Mac Studio (macOS arm64, 32 CPUs), release binary at 907c246ac (= origin/main 71db6d157 + branch
agents/w-interop-wake: the issuer wake fix and the svn matrix's --vcs-store; neither touches git or hg paths).
Report: Studio ~/.cache/w-interop/r-r110b-2059.txt.
- git v2.0.0 cut: 26/27 (r110 was 7/27 with the encode spin). Spins 0, kills 0, restarts 0. Peak RSS 5,254 MiB.
  clone 228 s, clone-b 54 s, partial-clone 90 s, ssh-clone 88 s. The one red row is deepen (22 s): "remote: aborting
  due to possible repository corruption on the remote side. fatal: protocol error: bad pack header": w-bugs' row
  (93 git P: deepen), sent to w-bugs with this text.
- hg full history: 24/24. Spins 0, kills 0, restarts 0. Peak RSS 6,414 MiB (load 13-30 during the run).
  clone 46 s, clone-b 104 s, stream-clone 158 s, ssh-clone 93 s.
## CLOSED: 93 interop, C: the Linux counts (2026-11-20 ~23:10 PST)
Prerequisites landed: the issuer-route EINTR retry 2e7756638 (main b86f0a4a2) and the issuer wake 2b25dc78c (main
90e9b720c; the anchor's wait polls the route's listener, so a claim is answered as it arrives; before it, loaded Linux
refused every mint ETIMEDOUT). Tests: Linux slates-ipc issuer_route 4/4, slates-api role::anchor 15/15.
Run r5, adacachy (Linux x86_64, 32 cores, load 31-55, shared with CI), release main 90e9b720c, git 2.56.0, Mercurial
7.2. Report: adacachy ~/.cache/w-interop/r-linuxc-2239.txt.
- git v1.0.0 cut: 27/27 (earlier today, release f95edf68e).
- git v2.0.0 cut: 26/27; deepen's bad pack is w-bugs' (same as macOS). Spins 2 (git_encode.rs:359, w-k90-vendor's
  encode spin), kills 0. Peak 3,298 MiB. clone 158 s, clone-b 167 s, ssh-clone 95 s.
- hg full history (57,053 changesets): 22/24. clone-rev and ssh-push red, each a client whose daemon the anchor killed
  (spins 5, kills 4, restarts 4: hg_push::unbundle, vcs_http.rs:850's connection task, a restart's start_shard). New
  finding, recorded not worked: GAPS 3760530f5, owner w-hg (inbox/w-hg-from-w-interop.md). Peak 5,434 MiB.
- svn: not countable on Linux: adacachy has no svn, svnadmin, svnrdump or svnsync. An install needs Ada's go, as svn on
  Windows does.
## CLOSED: 93 svn / 93 interop, C: the HTTP v2 rows (2026-11-21 ~00:50 PST)
Landed for it: 907c246ac (svn-matrix --vcs-store + view unmount; main 90e9b720c) and w-reaper's ef3247236 via my merge
f09d464ec (main a51023f57; presenter 2/2, store 3/3, svn_interop revprop x2 + view mount 3/3 on svn 1.15).
Run r7, big Studio (macOS arm64), release main a51023f57, stock svn 1.15. Report: ~/.cache/w-interop/r-svnv2b-2339.txt.
- HTTP route, every HTTP v2 row (checkout, log, blame, diff, export, update, commits, propset, mkdir, copies, switch,
  merge, lock/unlock, delete, revert, svnrdump, svnsync over HTTP): reads.dump 32/32; git-2000.dump all HTTP rows
  pass (svnsync 341 s, checkout 5 s). Spins 0, kills 0, restarts 0, mounts left 0.
- svn:// route: reads.dump 32/32.
- Still red, NOT an HTTP row and not mine: revprop-svn and revprop-ssh (svn:// and SSH) on git-2000.dump, "its owner
  shard did not answer in time": repos_for_hook bounds the first view by one LIVENESS_BUDGET_NS of the owner shard's
  time. w-reaper's revprop item (inbox/w-reaper-from-w-interop.md); its runner Studio-2 is down, so you may need to
  route it.
## CLOSED (addendum to the HTTP v2 rows): the svn revprop rows on a 2,000-revision repository (2026-11-21 ~01:15 PST)
Landed: bdf99f92d (repos_for_hook drives the hook's first view as progress-judged turns on the owner shard, each a
step quantum; refused only when a turn goes unanswered in the owner's liveness window or moves nothing) with its test,
via main e14493d62. Test: svn_interop a_hooks_first_view_of_a_long_history_is_waited_for_while_it_is_laid_out (2,000
revisions over a 4,872-file trunk): red with main's presenter (hook unrun, POST 404), green with the fix, 4/4 with the
two revprop tests and the view-mount test; presenter lib 2/2 (big Studio, debug, svn 1.15).
Run r11, big Studio, release main e14493d62, load 5-14: svn matrix git-2000.dump 32/32 on the svn route AND 32/32 on the
http route (revprop-svn 6 s / 5 s, revprop-ssh 0-1 s; svnsync 288 / 280 s). Spins 0, kills 0, restarts 0, mounts 0.
Report: ~/.cache/w-interop/r-hv-matrix-0038.txt. w-reaper told (inbox/w-reaper-from-w-interop-hook-view.md).
Every svn matrix row is now green on macOS for both dumps and both routes.
