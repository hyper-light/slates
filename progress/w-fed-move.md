# w-fed-move — progress (implementer; 2026-11-19 ~21:05-22:00 PST; ENDED on main's order to move agents into kiln)

## RESUME HERE (exact state at END)
- No code written, nothing committed, nothing in flight (no kiln run, no build). Nothing landed.
- Kiln worktree: ~/slates-work/w-fed-move-kiln, branch agents/w-fed-move at kiln origin/main 44cfdf353, clean,
  NOT pushed (nothing to push). Re-create anywhere with `git worktree add -b agents/w-fed-move <dir> origin/main`
  from a kiln clone (e.g. ~/slates-work/g264-kiln-bare-metal-kiln).
- Scratch: ~/slates-work/w-fed-move-fleet.rs.txt (a copy of slates main crates/server/tests/fleet.rs, for reading; delete).
- Next step: write crates/kiln/tests/slates_fleet_live_move.rs per the design below; push branch; build + run on the
  Mac Studio (d1323898) in ~/.cache/w-fed-move-kiln-clone (shallow GitHub clone of the pushed branch), target
  ~/.cache/w-fed-move-target, CARGO_BUILD_JOBS ~6; land in kiln (merge origin/main, push HEAD:main); one docs line in
  kiln docs/agentic.md W7 (replace "runners on separate machines ... once this one holds"); report to slates-7a.

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
