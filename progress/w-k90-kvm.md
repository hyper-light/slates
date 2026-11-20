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
