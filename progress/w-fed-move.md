# w-fed-move — progress (relaunched on the Mac Studio 2026-11-19 ~21:35 PST; supervisor slates-7a via kiln)

## 2026-11-20 11:10 PST (after the Studio's 10:35 reboot)
- Blocker 2 was NOT a Slates stall: current kiln leaves a moved agent whose turn was over idle on its new runner,
  waiting for its next message (hand-off goes with it); the test waited for a self-started hand-off turn. Test fixed
  (sends SECOND after the move; expected files by whether the turn heard kiln_handoff).
- A-734 (main) makes runners join only once the server lists every node: test now starts all runners together.
- kiln commit 1e2ba19d6 on agents/w-fed-move-vendor (worktree ~/slates-work/w-fed-move-kv): vendor patch 0003 (A-756),
  test changes, W7 paragraph; merged kiln main 715063153; vendor check 0 differences. NOT LANDED.
- Running: checks + local release live move (~/slates-work/w-fed-move-local24.log). Then remote (adas-mac-studio-2 +
  adas-mac-studio-3), then land kiln via bundle + Studio-4 (~/.cache/w-fed-move-push may need re-cloning).

## 2026-11-20 ~08:15 PST: REOPENED by main ("fix em up"): the live move's two Slates blockers
- A-756 LANDED slates main 0f681456 (fix 0fc610f9): a materialized volume (moved/taken over) claims its name's alias
  (verbs::claim_name_alias from fleet::make_taken). Red/green: fleet test
  a_captured_clone_moved_to_another_node_is_found_there_by_its_name (three_members_sharded(4)). Slates clone
  ~/slates-work/w-fed-move-slates; push via bundle + adas-mac-studio-3.local clone ~/.cache/w-fed-move-slates-push.
- kiln worktree ~/slates-work/w-fed-move-kv (branch agents/w-fed-move-vendor): vendor/patches/slates/0003 recorded
  (python3.12 scripts/vendor.py record/check: 0 differences) + test's remote_logs now include Slates anchor logs.
  Uncommitted; local release run ~/slates-work/w-fed-move-local20.log.
- Next: blocker 2 (LAN adoption never completes on the target): remote run with anchor logs, diagnose, fix red-first.

## STATUS 2026-11-20 ~06:30 PST: ENDED (superseded above). Four landings on kiln main (founder 3d8e479cf, share 5a294413c, test 4aabdba18,
## test follow-up 9129995e8). Everything cleaned (worktrees, clone, target, labs, remote folders, push clone).
## The live move's last leg is blocked in Slates: a moved volume's MOUNT NOENT (4/5 local runs) and, across the LAN, the
## target waiting on adoption with no end. Next owner: rerun
##   SLATES_FLEET_LIVE_MOVE=adas-mac-studio-2.local,adas-mac-studio-3.local SLATES_FLEET_LIVE_MOVE_HOST=<this IP>
##   cargo test --release -p kiln --test slates_fleet_live_move -- --nocapture
## once Slates fixes them.

## RESUME HERE (Studio instance)
- Private state clone ~/fanout-state-w-fed-move (other workers race on ~/fanout-state's rm -rf + clone).
- kiln clone ~/slates-work/w-fed-move-kiln (from ~/.cache/s32-kiln, origin = github hyper-light/kiln; HTTP/2 clone
  of kiln failed: use `git -c http.version=HTTP/1.1 fetch`). Worktree ~/slates-work/w-fed-move-wt, branch
  agents/w-fed-move off kiln origin/main f7ec15878. Target ~/.cache/w-fed-move-target, CARGO_BUILD_JOBS=8.
- Found: no kiln test exercises agents.move with a Slates volume at all (only slates_fleet_faults.rs, never compiled,
  cfg(linux)). Plan: crates/kiln/tests/slates_fleet_live_move.rs, mode `SLATES_FLEET_LIVE_MOVE=local` (three runners
  on this Mac, debug the move on macOS) then `=<runnerB>,<runnerC>` (remote via kiln run --on), design below.
- Test written + committed c60199af8 on agents/w-fed-move (NOT pushed: the Studio has no GitHub credential; HTTPS none,
  SSH key denied). Push route: runner adas-mac-studio.local (Studio-3) has GitHub SSH auth -> ship a git bundle there
  (served from this Mac over HTTP) and push from a clone there.
- Builds: ~/slates-work/w-fed-move-build{1,2}.log. Next: compile, unit checks (`--test slates_fleet_live_move checks`),
  then SLATES_FLEET_LIVE_MOVE=local run, then remote run (B=adas-mac-studio.local, C=adas-macbook-pro.local,
  HOST=192.168.1.82).
- FINDINGS (by use, local mode on the Studio, 2026-11-19 ~22:20 PST), for main:
  1. A runner started with `--no-terminal` has no harness side: no workspaces and no Slates fleet node
     (agent.rs ~2495 `harness = (!o.ephemeral && o.terminal)`). common::Fleet::start_runner_with passes it, so
     slates_fleet_faults.rs's lab can never form a fleet (tell w-fed-faults). My test starts runners as vm_e2e::runner.
  2. kiln bug, reproduced: three listed runners started together each join hearing only themselves (`nodes=1` in each
     manifest) and form three fleets of one that never merge (members stayed 1 for 10 min). Not my piece: reported; the
     test starts runners one at a time, each waiting for its join with nodes=i+1.
  3. Lab::start in slates_fleet_faults.rs calls start_server twice (Fleet::with already starts it).
  4. BLOCKER found by use and FIXED as its own piece: kiln never bootstraps a fleet node's groups, Slates' fleet needs
     one node to `bootstrap root`; every agent start on a fleet runner ended "this node has no consensus group yet"
     (88 starts / 8 min). Fix: commit 30cc13d04 on branch agents/w-fed-move-founder (worktree
     ~/slates-work/w-fed-move-founder): Place::founds (optional field), founder = first listed node; that runner
     ensure_group()s after its node starts. `cargo test -p kiln --lib k3_` 5 passed. NOT LANDED yet.
  5. kiln doesn't wait for a fleet node's group import before giving a workspace (the test retries the start).
- Test branch agents/w-fed-move merged origin/main + founder (b8cb04b68); test edits uncommitted on top.
- Landing plan: land founder piece first, then the test piece (merge origin/main; amend nothing; push via Studio-3).
  6. With founding fixed (local run): fleet forms, agent starts first try and edits in its volume on live-a, but every
     agents.move is refused `[kind: unplaced]` (records not placed at f+1 within the fleet's measured failover delay),
     even 8+ min after the volume was made, each refusal ~1 s after the ask. Debug build: trying a RELEASE build
     (~/slates-work/w-fed-move-release-build.log) to see if it's speed. kiln dedups identical notices (one notice for 2+
     refusals) -> the test retries every 30 s, up to 10.
- Founder piece: amended to 4937d9c6e (+docs/agentic.md W7 line), merged origin/main; k3_ rerun queued
  (~/slates-work/w-fed-move-founder-unit2.log). Push via runner adas-mac-studio-3.local (host Studio-4, GitHub SSH ok),
  clone ~/.cache/w-fed-move-push there; bundle served from this Mac. (adas-mac-studio.local went offline 07:40Z.)
- Test branch will need rebuilding after the founder lands (it merged the pre-amend founder commit): new branch from
  origin/main + the test file.
  7. LANDED founder fix: kiln main 3d8e479cf (fix 4937d9c6e). Worktree removed.
  8. Release build: moves still `unplaced`; Slates anchor log on A every period: "forward lane to HostId(..): the path's
     measured floor 3102 ms is past the failover SLO 3100 ms (silence window 1998 ms)" -> placement can't meet the
     failover bound (Slates finding, for main). On the 5th ask Slates' move DID complete ("a volume .. this node moved
     away is released to member ..") but kiln's session move then failed "Claude Code's record of session .. is not on
     this machine" (stub writes no transcript) -> kiln finding: kiln moves the volume BEFORE checking it can carry the
     harness record, leaving the agent on A with its volume on B. Test now uses kind `opencode` (ACP: carried in words).
  9. Release + kind opencode: MOVED at ask 4 (after 3 unplaced). Then B's mount refused: "Slates' mount: denied:
     Forbidden: the caller's rights on the volume do not include status": the volume's owner is Uid{host: A's anchor,
     uid}; B's kiln is Uid{B's anchor, uid}: nothing shares it. FIX (piece 2, worktree ~/slates-work/w-fed-move-share,
     branch agents/w-fed-move-share off origin/main fce36c347, uncommitted): NodeUser{anchor,uid} rides the runner's
     member report -> Member -> VolumeMove.user -> move_out shares (slates client.share, all rights) before moving.
     slates-ipc added as a direct dep (Rights isn't re-exported by slates-api); Cargo.lock +1 line. Unit: k4_ 6 passed,
     k3_ 5 passed; k4 member test extended (k4_a_nodes_member_id_and_user_ride_its_runners_word). Proof run:
     ~/slates-work/w-fed-move-local12.log (test file copied in, untracked).
  10. MAIN IS BROKEN (not mine): kiln origin/main's binary doesn't compile since 4eda3a697 (W9 container engine):
      sandbox/mod.rs `#[cfg(test)] pub mod container;` but harness/session.rs:5371 calls sandbox::container::launch.
      Lib tests (cfg(test)) hide it. Removing the cfg(test) line builds clean (release). Local uncommitted hack in the
      share worktree only for proof runs: NEVER commit crates/kiln/src/sandbox/mod.rs. Report to main.
  11. With share: B's mount refused "MOUNT service refused the export (mountstat3 2)" 0.3 s after the switch (export
      not yet listing the adopted volume). Then the session ended and kiln on B DESTROYED the moved volume ("its clone's
      destroy") -> data-loss finding for main. Fix attempt (piece 3, same worktree): mounted_patiently waits on
      MountFault::RootHandle{status:Some(2)} up to 4x failover (>=10 s), unit k4_a_moved_volumes_unlisted_export_...
      Proof: ~/slates-work/w-fed-move-local14.log.
  12. Arrival wait NOT proven: run 14 mounted (carried on, B's CWD in its mount; hand-off turn replayed the first turn's
      words: REMOVE_FAIL gone.txt = removal moved), run 15 stayed NOENT for 10.5 s of waiting. Slates' MOUNT-by-name
      lists only the current shard's volumes (nfs.rs ShardVolumeSet::entries via state::with_state) -> a volume adopted
      on another shard is unlisted depending on the serving shard: SLATES finding, intermittent. Wait reverted (kept in
      ~/slates-work/w-fed-move-arrival-wait.patch). Share piece = share only. Test now judges the hand-off turn (turn 2)
      then its own turn (3); README expected with two APPENDED lines. Runs 16, 17 in flight.
  13. LANDED share fix: kiln main 5a294413c (fix 04d0dc570). LANDED test: kiln main 4aabdba18 (test 7ac22adf4) +
      docs/agentic.md W7 paragraph. main's sandbox::container cfg(test) breakage was fixed on main by its owner.
      Local release runs 13-17 after share: moved every run (asks 4-5), carried on 1/5 (run 14); rest Slates NOENT.
  14. Remote run 1 in flight (~/slates-work/w-fed-move-remote1.log): B=adas-mac-studio-2.local (Studio-2),
      C=adas-mac-studio-3.local (Studio-4), HOST=192.168.1.82. adas-macbook-pro.local went offline;
      adas-mac-studio.local-2 is THIS Studio. Bundle HTTP server on :47811 (task b10uxf735), stop at END.
  15. Remote: release kiln is 258 MB, LAN to Studio-2 ~280 KB/s -> test now ships kiln.gz (~104 MB), prep bound 1800 s
      (uncommitted follow-up in share worktree). Remote run 2: fleet FORMED across 3 Macs (A .82, B .81 Studio-2,
      C .83 Studio-4); agent start refused "Slates' mount: refused for now (LeaseUnconfirmed{version:7})" -> kiln
      finding: first mount doesn't wait on Slates Retry (the moved-volume mount does); test now retries the start.
      Run 3: agent WORKED on A; move refused "Slates' unmount: .. is busy: holders not read [kind: slates]" -> test
      asks again on that too. Run 4 in flight (~/slates-work/w-fed-move-remote4.log). Remotes cleaned each time.
- Kiln AGENTS.md: no tree-wide cargo fmt (rustfmt only the new file), cite IDs in test names.

## What exists (checked)
- K1-K5 and A-503 (kiln side) landed; the K4/K5 live move across Macs was only ever filed as a MANUAL run request
  (run-requests.md lines ~113, ~478, ~1118): never written as a test, never run.
- A-580 (kiln 9ff9eafb0, tests/slates_fleet_faults.rs): one-machine loopback lab, cfg(linux), never compiled; its
  W7 doc says cross-machine runners come "through `kiln run --on` once this one holds". w-fed-faults owns it.
- Finding by reading (for main, not chased): slates_fleet_faults.rs reads agents.get at top-level pointers
  (/workspace/made/volume, /on), but agents.get answers {"agent","info","state",...} (harness/ops.rs:114): the session's
  workspace/target are under /info (or use GET /api/harness/sessions/<id>, whose `target` is "agent:<machine>" and
  `workspace.made.{volume,mount}`, as slates_e2e.rs workspace_of does). Its runners also set no KILN_MACHINE_ID while
  the lab keys them by common::machine(name). (Tell w-fed-faults.)

## Machines (probed 2026-11-19 ~21:10 PST via `kiln run --on <r> --no-sync`; nested kiln run from the Studio works)
- A = d1323898b255a3ed435eb83fa0f7b0cf (Adas-Mac-Studio, 192.168.1.82, 1.2 TB free): server + test + runner A.
- B = runner `adas-mac-studio.local` (host Adas-Mac-Studio-3, 192.168.1.84, 53 GB free): agent's target.
- C = runner `adas-macbook-pro.local` (host MacBook-Pro-3, 192.168.1.86, 77 GB free): third node (f=1 needs 3), no mount.
- All: firewall disabled, /usr/bin/python3 3.9.6 (stub parses as 3.9), curl. No SSH between machines.
- Installed ~/.local/bin/kiln differs per machine (hashes differ) -> ship ONE test-built binary.
- Avoid this MacBook (192.168.1.71: supervisor lives here). Local runner's link to the kiln server flaps; retry.

## Design (gated test, run on A)
- `#![cfg(unix)]`, `mod common; mod vm_e2e;`, gate `SLATES_FLEET_LIVE_MOVE=<runnerB>,<runnerC>` +
  `SLATES_FLEET_LIVE_MOVE_HOST=<A's LAN IPv4>`; skips saying so otherwise. Pure checks in a `mod checks` (unit tests).
- File server thread on 0.0.0.0:0 serving only /<random>/kiln (env!("CARGO_BIN_EXE_kiln")), /<random>/stub
  (tests/fixtures/vm-stub/harness/stub), /<random>/join (join token: keep it out of kiln run command lines/logs).
- Prepare B, C: `kiln run -q --on R --no-sync -- <sh>`: d=$HOME/.cache/kiln-live-move/<run>-<name>; mkdir bin home state
  work; curl kiln, stub, join; ln -s stub bin/claude; login-shell (quoted heredoc: PATH=$d/bin:/usr/bin:/bin:/usr/sbin:/sbin,
  drop all args but the last, exec /bin/sh -c "$1", as vm_e2e::stub_host_env); python3 prints two consecutive free UDP
  ports (node base) + a free TCP port (KILN_OTLP_ADDR) + the IP that reaches A (UDP connect + getsockname).
- Server on A: own config like common::Fleet::with_tables but http/quic on 0.0.0.0, public_url/quic_advertise at A's IP,
  `[slates_fleet] f = 1` with nodes machine(name) = "<ip>:<base>" for A, B, C (common::machine = sha256("kiln machine
  id\n"+name)[..32]; set KILN_MACHINE_ID=name on every runner). Build a common::Fleet from its pub fields + start_server.
- Runner A: fleet.start_runner_with(name, stub_host_env + KILN_MACHINE_ID, KILN_SLATES_INSTANCE, KILN_SLATES_DIRECT=1,
  KILN_SLATES=""). Runners B, C: spawned `kiln run -q --on R --no-sync --detach --timeout 7200 -- 'echo $$ > $d/runner.pid;
  exec env -i HOME=$d/home PATH=... SHELL=$d/login-shell KILN_MACHINE_ID=.. KILN_SLATES_INSTANCE=.. KILN_SLATES_DIRECT=1
  KILN_SLATES= KILN_OTLP_ADDR=127.0.0.1:<tcp> KILN_OTLP_GRPC_ADDR=off $d/kiln agent --server http://<A>:<http> --token
  "$(cat $d/join)" --name <name> --slots 1 --labels baremetal --no-terminal --no-reach --state-dir $d/state --work-dir
  $d/work >> $d/runner.log 2>&1'` (env -i: a run's KILN_* would point it at production).
- Formed: common::agents::connected for all three, then A's slates_api::Instance::attach(instA).daemon_status()
  .fleet.members.len() >= 3.
- Flow: git project under fleet root (README.md, gone.txt, src/lib.rs); POST /api/ops/agents.start {"kind":"claude",
  "on":machine(A),"cwd":project,"mode":"bypassPermissions","prompt":"WRITE=made.txt APPEND=README.md REMOVE=gone.txt",
  "workspace":{"kind":"volume"}}; wait events (GET /api/harness/sessions/<id>/events?limit=1000, vm_e2e::said) for
  STUB_DONE; check WROTE/APPENDED/REMOVED and CWD under A's mount; read session -> workspace.made.{volume,mount}.
  POST /api/ops/agents.move {"agent":id,"to":machine(B)} (202); wait session target == "agent:<machine B>" (a
  "[kind: x]" refusal in the timeline fails with it). agents.send {"agent":id,"text":"WRITE=on-b.txt APPEND=made.txt"};
  wait 2nd STUB_DONE; APPENDED made.txt proves the file moved; CWD under B's $d/state/workspaces. Remote cat (base64) of
  B's mount: README "the project\nAPPENDED README.md\n", made.txt "made.txt\nAPPENDED made.txt\n", on-b.txt, lib.rs
  unchanged, gone.txt absent. Old runner A: its old mount point out of /sbin/mount's table; reads through instance A
  never stale (current bytes = forwarded, or a typed refusal: slates' own never_stale rule, fleet.rs ~6418);
  attach_volume(Write) on A refused by kind (Elsewhere/... ; report what it is). agents.stop; B's mount gone.
- Teardown (NFS: a daemon killed under a live macOS NFS mount panics the Mac): stop the agent first; on each machine
  refuse to signal anything while `mount` lists a path under the lab's dir (print LEFT RUNNING); else TERM the runner by
  its recorded pid and the instance's processes by exact `--instance <unique name>` (ps + awk; never pkill -f), then
  rm the remote dir.
