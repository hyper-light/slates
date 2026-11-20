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
