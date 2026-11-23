# w-k90-kvm progress (kiln goal 90: KVM microVM drive, resync end on KVM, A-520 piece 5; big Studio d1323898)
Started 2026-11-20 ~07:50 PST. Read: briefs/common.md, briefs/w-k90-kvm.md, progress/w-k90-microvm.md, relaunch-2026-11-19.md.

## Setup
- State folder ~/fanout-state-w-k90-kvm (HTTPS clone; this Studio has no GitHub push credential -> push via kiln run on
  adas-mac-studio-2.local b24c5c95, bundle route).
- Kiln clone here: ~/kiln-work/w-k90-kvm (origin/main ed8991d29). Scripts: ~/kiln-work/w-k90-kvm-scripts.
- adacachy: own clone ~/.cache/w-k90-kvm/clone, logs ~/.cache/w-k90-kvm/logs, script ~/.cache/w-k90-kvm/kvm-drive.sh
  (bounded: systemd-run --user MemoryMax=30G CPUWeight=1 Nice=19 RuntimeMaxSec=5400; test timeout 3000 s; watchdog
  kills the test 120 s after a runner log says `Unstarted(`). Target: the handed-off ~/.cache/w-k90-microvm-target.

## Part 1: KVM microVM drive (SLATES_E2E=1 VM_E2E=1 slates_e2e microvm::)
- Predecessor's last KVM run (systemd unit run-p2923027, 07:31-07:45 PST, ended by stop): NOT a loop. The drive hung
  in its no-window wait "its first turn". Runner log /tmp/.tmpQE0sv8/slatesvm.log on adacachy: box kiln-z1-0 and then
  kiln-z1-1 failed `box ...'s member 0: agent control: Unstarted(Failed)` (Shards: "its host could not run it"),
  and kiln never ended or failed the session, so the drive waited until it was killed.
- The respawning `kiln agent --name slatesvm` on adacachy is another session's (pid 1925357, from
  ~/.cache/envscope-test/target, orphaned to systemd --user, ~4.5 h). Not ours; not touched; told main.
- r14 lost (adacachy's runner restarted mid-clone; ~/.cache/w-k90-kvm/clone is a broken partial clone -> for main to
  delete). r27/r34 kiln syncs timed out (kiln server not answering). Route that works: the partial sync's .git
  (~/.kiln/reach/d1323898.../w-k90-kvm-0afe6c0693) -> `git worktree add --detach ~/.cache/w-k90-kvm/wt ed8991d29`.
- r36 (adacachy, 08:58 PST, unit run-p3578166): microvm:: at ed8991d29 in ~/.cache/w-k90-kvm/wt, SHARDS_LOG=debug,
  RUST_LOG hex/shards/kiln::sandbox debug. Logs ~/.cache/w-k90-kvm/logs/r2-ed8991d29{,.pack,.build}.
- Reading: Shards' MCP lane answers Unstarted::Failed for "its run is over" (daemon/managed/mcp.rs), so the VM's run
  likely ended right after boot; kiln's isolated.rs bridge() logs and returns, nothing ends the session.
- Two defects to take red-first: (a) the guest member's exec failing on KVM; (b) a session whose microVM member never
  started is never ended/said (the drive's wait has no window).

## Part 2: resync end on KVM -- after part 1's drive passes (g90 test in the same microvm:: set).

## Part 3: A-520 piece 5
- Code is ON kiln main: 5a e73ad6d39, 5b-i a69822973, 5b-ii 4eda3a697 (runner's `container` engine ->
  sandbox::container::launch -> workspace::container). Missing: proof by use.
- adacachy: rootful docker 29.9.0 only, no podman, no user_allow_other in /etc/fuse.conf -> NeedsSharedMount.
  Asked main (08:0x PST): Ada's word on podman install or user_allow_other; overlap with w-k90-containers' r52.

## For main to delete
- adacachy ~/.cache/w-k90-kvm/clone (broken partial clone, mine)

## 2026-11-20 ~10:00-11:10 PST: KVM ROOT CAUSE + fixes (big Studio rebooted 10:35; resumed)
- ROOT CAUSE (r50 diag, adacachy, ed8991d29 + log patch): Hex refused the member: "its command never started (status
  128): the volume at /tmp/.tmpKZErig/ask/project: a volume may not be at or under /tmp, a domain's own scratch".
  The drive's tempdir is /tmp on Linux (/var/folders on macOS, hence HVF passed). The image itself is fine on KVM
  (`shards run` of the stand-in image: python3 + /opt/vm-stub/stub -> WROTE, STUB_DONE, rc 0, 23 s).
- Defect (b): kiln said exit -1 with no words (hexvm end_of drops Hex's refusal), and the drive's Watch::ended waited
  for a word newer than its first look though kiln had already said "so kiln stopped starting it again" -> 35 min hang.
- Fix on branch agents/w-k90-kvm-g90 (kiln a8156f377 = wip 182960104 + merge of main 715063153):
  slates_e2e microvm drives' folders under CARGO_TARGET_TMPDIR; hexvm `refusal_said` (a refused end says why on the
  member's stderr -> session error); session::STOPPED_STARTING shared const; vm_e2e Watch::ended refuses at once on it.
  Tests: vm_harnesses s35_a_wait_refuses_at_once... (new case), hexvm g90_a_refused_members_end_says_why_on_its_stderr.
- Red: local (this Studio) vm_harnesses with the rule hunk held out (running). Green: KVM run r3 (adacachy,
  ~/.cache/w-k90-kvm/wt3 at a8156f377, logs ~/.cache/w-k90-kvm/logs/r5-a8156f377*).
- Slip: I ran `git switch` once (to a new branch at the same commit, no file change). No more.
- for main to delete (adacachy): ~/.cache/w-k90-kvm/wt (r2/r3 scratch, diag patch applied), ~/.cache/w-k90-kvm/clone.

## 2026-11-20 ~12:10 PST: LANDED kiln 22bb3fb9e (main merge 448985fef; is-ancestor checked)
- Red (macOS arm64, this Studio, local, base 715063153): vm_harnesses waits::s35_a_wait_refuses_at_once_where_its_harness_is_gone_for_good
  FAILED (left Wait, right Ended{-1, halt words}) with the rule hunk held out; green 1/1 with it;
  sandbox::hexvm::tests::g90_a_refused_members_end_says_why_on_its_stderr 1/1. agentic.md W8: one status sentence.
- Landing route that works here: bundle -> studio-3 (2768d0d5) ~/.cache/w-k90-kvm-kiln-push + worktree
  ~/.cache/w-k90-kvm-kiln-land, a fetch/merge/push loop there (main moves every few minutes).
- Ada (via the turn): don't block on CI runs; focused tests locally, then merge.
- KVM proof by use: run r3 (adacachy, unit run-p3140219, wt ~/.cache/w-k90-kvm/wt3 at a8156f377 = same code on
  715063153), microvm:: (g266 + g90 resync). Building at CPUWeight=1 (load 60-120). Logs ~/.cache/w-k90-kvm/logs/r5-a8156f377*.
- NEXT: read r3; if green -> parts 1+2 evidence to main (plan 01a1fa36 steps 5, 8). Part 3: main's slates_e2e now has
  container::a520_in_a_container_on_its_volume_... (someone else's); my podman/user_allow_other question to main stands.
- for main to delete: kiln branch agents/w-k90-kvm-g90 (superseded by 22bb3fb9e); local ~/kiln-work/w-k90-kvm-land once r3 is read.

## 2026-11-20 ~14:50 PST: PARTS 1 AND 2 PROVEN ON KVM (reported to main)
- Part 2: r28 microvm::g90 PASSED (adacachy, a8156f377 = 22bb3fb9e's code on 715063153): daemon 3185184 SIGKILLed, successor 135631 served the next write.
- Part 1: r39 microvm::g266 PASSED 1/1 in 793 s (same tree): Ask held and approved, Bypass LAND_WRITTEN 3, folders exact, no host mount.
- Lesson: the drive's server and runner time out their link after 10 s (net::limits idle_timeout). Build at low CPU weight, but run the
  drive at the default weight (~/.cache/w-k90-kvm/kvm-test.sh on adacachy). r28's g266 and r9 failed starved, not on code.
- Part 3 (A-520 p5): code on main; proof held by w-k90-a520 (running on adacachy). Not duplicated; told main.
- Load finding (reported): an agent's act is refused at once while its runner's link is down (deliberate: reach/tests.rs).
- Landing worktree ~/kiln-work/w-k90-kvm-land removed.
- for main to delete: kiln branch agents/w-k90-kvm-g90; adacachy ~/.cache/w-k90-kvm/{wt,wt3,clone}; here ~/kiln-work/w-k90-kvm,
  ~/.cache/w-k90-kvm-local-target.noindex.

## 2026-11-20 ~17:05 PST: C1 (90d) and C2 (90f) CLOSED; inbox/main-from-w-k90-kvm-close.md written
- At kiln main c0f576eb3 on adacachy KVM: r51 g266 PASSED (721 s), r42 g90 PASSED (daemon 2738760 -> 3689757). Steps 5 and 8 met.
- FINDINGS (recorded, not worked, per the close order):
  (1) an agent's act is refused at once while its runner's link is down (reach/node.rs on_local; deliberate per
      reach/tests.rs). Under load, a link blip fails an agent's landing.
  (2) a session start sent just before a link drop can be lost; the server then marks it crashed ("the runner doesn't
      hold it"), and only a person's resume takes it up.
  (3) under I/O pressure (adacachy, io some avg10 ~10%) a runner's sqlite stores took 48-96 s to open.

## 2026-11-20 ~21:30 PST onward: goal 92, the six Shards-owed items (main's ask; "vendor once": judged in kiln's tree)
- 1 HOLDS: vendor/patches/shards/0002 == Shards D288 b9006561 (numstat, docs aside); KVM r42/r28, HVF 81174ac49.
- 3 HOLDS: slates_e2e microvm::g266 + ::g90 on KVM (r51, r42 at c0f576eb3) and HVF (81174ac49).
- 4 HOLDS: one Slates in kiln (vendor/slates); vendored Shards has no Slates dep; SLATES_REV only in Shards' .github.
- 6 BUILT, red/green on macOS HVF (this Studio): commit 3ba7efe6c (merge 2666498ea on main 865d8bed5), vendor patch
  0003 "a restored microVM is given its served shares' streams". Red: restore refused --slates-fs-fd; green 1/1.
  Windows: vm-process cross-checks clean; shards crate's Windows (cli served_pipes) unchecked here (aws-lc C), owed on jupiter.
  LANDING: push via adacachy run r3 (synced from ~/kiln-work/w-k90-kvm branch w-k90-kvm-item6). Two earlier tries failed
  (kiln server timeouts; one pasted bundle corrupt).
- 5 IN FLIGHT: PM M620 scratch branch agents/w-k90-kvm-m620 (e1b6b45bd tests only = red, c60ed4c9b + fix = green);
  jupiter run r2 (openssl-backend shallow clone; schannel kept dropping). Then land as vendor patch 0004.
- 6 LANDED (2666498ea). 2 LANDED kiln 110f4115f (vendor patch 0004, fs/acl.rs + server.rs): red unit 3 failed / VM
  "mode reads 600 after its ACL, not 640"; green fs 122, boot a_direct_boots_share_enforces_its_guests_posix_acls 1/1 (HVF).
- 5: GitHub clones on jupiter kept failing (curl 92 HTTP/2 reset), so I sent the commits as a bundle (carry repo m620.bundle) into jupiter
  ~/.cache/w-k90-kvm/kiln.git; worktrees red/green there; run r9. Landing tree ~/kiln-work/w-k90-kvm-m620land (diff applied, win cross-check clean).
- Slips: r61's retry loop ran `rm -rf` on its own partial clone dir (green.part) on jupiter; my git grep/diff on main's
  ~/.local/state/shards-audit.git likely lazily fetched objects into it (told main).
- for main to delete (this Studio): ~/.cache/w-k90-kvm-shards-stray-target{,-2} (test artifacts moved out of vendor/shards),
  ~/.cache/w-k90-kvm-shards-stray-target-3, ~/kiln-work/w-k90-kvm-acl (worktree, landed), ~/.cache/w-k90-kvm-shards-target.noindex, ~/.cache/w-k90-kvm-slates-target.noindex; jupiter ~/.cache/w-k90-kvm/{kiln.git,green.try1}.

## 2026-11-21 ~02:10 PST: GOAL 92 DONE (close table in inbox/main-from-w-k90-kvm-close.md)
- 5 LANDED kiln 64624e059 (vendor patch 0005, PM M620). Jupiter WHP fs::window::guest at --test-threads 1: red 4 passed 2 failed,
  green 6 passed; landed tree devices::virtio::fs 116 passed. At --test-threads 4 two WHP tests collide on host resources (finding).
- 6 Windows: `cargo check -p shards --bins` on jupiter rc 0 (AWS_LC_SYS_PREBUILT_NASM=1 from env; no NASM/CMake there, nothing installed).
  The shards bin test target can't compile in kiln's vendored tree (missing testdata/scripts/docs files; finding).
- Slip: one jupiter command carried a stray `rm -f /dev/null 2>/dev/null` (typo; no effect seen). Noted.
- for main to delete: this Studio ~/kiln-work/{w-k90-kvm-acl,w-k90-kvm-m620,w-k90-kvm-m620land} (worktrees, landed; git worktree remove),
  ~/kiln-work/{w-k90-kvm-m620tree,w-k90-kvm-carry,w-k90-kvm-shards.git}, ~/.cache/w-k90-kvm-shards-stray-target{,-2,-3},
  ~/.cache/w-k90-kvm-shards-target.noindex, ~/.cache/w-k90-kvm-slates-target.noindex; kiln branch agents/w-k90-kvm-m620;
  jupiter ~/.cache/w-k90-kvm (kiln.git, m620/, target/, green.try3, logs).

## 2026-11-21 ~10:00 PST: own scratch deleted, by exact path (main's last step)
- kiln branch agents/w-k90-kvm-m620 deleted on GitHub (ls-remote: 0 refs). Its two scratch commits are not ancestors of main
  (ported as a fresh commit), but all three files they changed are byte-identical on main (a82d027c0, in 64624e059).
- big Studio (d1323898): worktrees removed with `git worktree remove`: ~/kiln-work/w-k90-kvm-acl 550M, ~/kiln-work/w-k90-kvm-m620 550M,
  ~/kiln-work/w-k90-kvm-m620land 550M. Deleted: ~/kiln-work/w-k90-kvm 729M (clone; its one unlanded branch w-k90-kvm-g90 had an empty
  diff to main, superseded by 22bb3fb9e), ~/kiln-work/w-k90-kvm-m620tree 70M, ~/kiln-work/w-k90-kvm-shards.git 13M,
  ~/kiln-work/w-k90-kvm-scripts 8.7M, ~/.cache/w-k90-kvm-shards-stray-target 275M, -2 275M, -3 275M,
  ~/.cache/w-k90-kvm-shards-target.noindex 4.3G, ~/.cache/w-k90-kvm-slates-target.noindex 586M, ~/.cache/w-k90-kvm-local-target.noindex 15G.
  ~/kiln-work/w-k90-kvm-carry 352K deleted right after this push. Kept: this state folder ~/fanout-state-w-k90-kvm (the record).
- jupiter: ~/.cache/w-k90-kvm 8.5G deleted (no process of mine on it, checked by command line).
- adacachy: ~/.cache/w-k90-kvm 1.7G deleted (clone, wt, wt3, wt-main-c0f576eb3, logs, state.git, scripts); `git worktree prune` in
  kiln's reach dir for this session removed those three stale entries. Left for kiln/main: the reach dirs
  ~/.kiln/reach/d1323898…/w-k90-kvm-0afe6c0693 and w-k90-kvm-carry-58e13b5d95 (kiln's sync copies), and
  ~/.cache/w-k90-microvm-target (handed over by w-k90-microvm, not made by me).
- Total freed: about 33 GB. Done; ending.
